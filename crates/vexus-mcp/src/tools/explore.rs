//! `explore` tool (the flagship): answer a question about the codebase in
//! one call. Pipeline (binding):
//!
//! 1. `search_hybrid_scored(question, embed(question), floor, entry_limit)`
//!    → entry chunks, rendered as `BundleItem`s carrying their RRF score.
//!    `entry_limit` (and every other limit below) scales with the corpus
//!    tier — see `params_for`; the Medium tier is 12/8/24/8000, identical
//!    to the historical constants. A `WeakVectorOnly` outcome (no keyword
//!    hit, nothing under the KNN floor) renders the entries under a "weak
//!    match" note and skips steps 2–3 entirely.
//! 2. For each entry chunk's `symbol_id` (deduped, first-seen score wins,
//!    max `max_entry_symbols` distinct — `search_hybrid`'s results are
//!    already score-descending, so first-N distinct is top-N by score):
//!    walk `callers_of(id, 1, 10)`, `callees_of(id, 1, 10)`, and
//!    `imports_of(id)` for neighbor symbol ids. Only resolved neighbors
//!    (id != -1) count; collection stops at `max_neighbor_ids` distinct.
//! 3. Each neighbor's `symbol_source` chunks become `BundleItem`s too, with
//!    score = the parent entry's score × 0.5 (a neighbor reached from
//!    multiple entries keeps the max of those).
//! 4. `pack(entries ++ neighbors, budget)` → `render_bundle`. Entry and
//!    neighbor items can name the very same chunk (an entry symbol can be
//!    another entry's neighbor) — `pack`'s chunk_id dedupe (keep highest
//!    score) resolves that, which is why both entry and neighbor items
//!    carry their real `chunk_id` rather than the `-1` non-chunk sentinel.
//! 5. Prefix the rendered bundle with an `explore: "{question}"` header.

use std::collections::HashMap;

#[cfg(test)]
use vexus_watch::pipeline;

use crate::bundle::{pack, BundleItem};
use crate::format::render_bundle;
use crate::state::{freshness_header, AppState};
use crate::tools::{apply_header, clamp_budget, embed_query};

const NEIGHBOR_DEPTH: u32 = 1;
const NEIGHBOR_LIMIT: u32 = 10;
const NEIGHBOR_SCORE_FACTOR: f64 = 0.5;

/// Per-tier limits: entry hits, distinct entry symbols to expand, distinct
/// neighbor ids, and the default token budget (still overridable per call).
/// `Medium` is byte-identical to the historical constants (12/8/24/8000);
/// smaller tiers shrink everything so a question against a 30-file repo
/// returns hundreds of tokens, not the whole repo — the token benchmark
/// measured the old defaults at 0.2×–0.4× grep's cost on such corpora.
struct ExploreParams {
    entry_limit: u32,
    max_entry_symbols: usize,
    max_neighbor_ids: usize,
    default_budget: u32,
}

fn params_for(tier: vexus_core::model::CorpusTier) -> ExploreParams {
    use vexus_core::model::CorpusTier;
    match tier {
        CorpusTier::Tiny => ExploreParams {
            entry_limit: 8,
            max_entry_symbols: 6,
            max_neighbor_ids: 12,
            default_budget: 4000,
        },
        CorpusTier::Small => ExploreParams {
            entry_limit: 8,
            max_entry_symbols: 6,
            max_neighbor_ids: 16,
            default_budget: 4000,
        },
        CorpusTier::Medium => ExploreParams {
            entry_limit: 12,
            max_entry_symbols: 8,
            max_neighbor_ids: 24,
            default_budget: 8000,
        },
    }
}

const NO_MATCH_TEXT: &str = "nothing indexed matches that question — try 'search' with distinctive words from the code, or 'status' to check index coverage.";

const WEAK_MATCH_TEXT: &str = "weak match — nothing indexed clearly matches this question; the snippets below are only the nearest neighbors. For exact strings or comments, grep is the better tool here.";

/// Pure inner implementation of the `explore` tool.
pub fn explore_text(state: &AppState, question: &str, budget_tokens: Option<u32>) -> String {
    let question_header = format!("explore: \"{question}\"\n\n");

    // Embed before locking: a real embedder's inference call must not hold
    // the store mutex, or it stalls every other tool call for its duration.
    let query_vec = embed_query(state, question);
    let knn_floor = super::knn_floor(state);

    let store = match state.lock_store_fresh() {
        Ok(s) => s,
        Err(msg) => return msg,
    };
    let fresh_header = freshness_header(&store);
    let params = params_for(
        store
            .corpus_tier()
            .unwrap_or(vexus_core::model::CorpusTier::Medium),
    );
    let budget_tokens = clamp_budget(budget_tokens, params.default_budget);
    let (hits, outcome) = match store.search_hybrid_scored(
        question,
        query_vec.as_deref(),
        knn_floor,
        params.entry_limit,
    ) {
        Ok(h) => h,
        Err(e) => return apply_header(fresh_header, format!("explore error: {e:#}")),
    };

    if hits.is_empty() {
        return apply_header(fresh_header, format!("{question_header}{NO_MATCH_TEXT}"));
    }
    let weak = outcome == vexus_core::search::SearchOutcome::WeakVectorOnly;

    // Step 1: entry chunks as BundleItems, plus the deduped (symbol_id,
    // score) list step 2 expands from. `hits` is already score-descending
    // (search_hybrid's RRF ranking), so the first occurrence of a symbol_id
    // is that symbol's best score among the entries, and capping at the
    // first 8 distinct ids keeps the top 8 by score.
    let mut items: Vec<BundleItem> = Vec::with_capacity(hits.len());
    let mut entry_symbols: Vec<(i64, f64)> = Vec::with_capacity(params.max_entry_symbols);
    for hit in &hits {
        items.push(BundleItem {
            path: hit.path.clone(),
            qualname: hit.qualname.clone(),
            start_line: hit.start_line,
            end_line: hit.end_line,
            content: hit.content.clone(),
            score: hit.score,
            chunk_id: hit.chunk_id,
        });
        // A weak match skips graph expansion entirely: fanning out through
        // callers/callees of a nearest-neighbor guess is where the
        // small-repo token bloat came from, and the neighbors of a wrong
        // entry are just more wrongness.
        if weak {
            continue;
        }
        if let Some(sid) = hit.symbol_id {
            if entry_symbols.len() < params.max_entry_symbols
                && !entry_symbols.iter().any(|(id, _)| *id == sid)
            {
                entry_symbols.push((sid, hit.score));
            }
        }
    }

    // Step 2: one hop of call/import graph expansion from each entry
    // symbol, collecting resolved-only neighbor ids (max 24 total) with
    // their best (max) neighbor score.
    let mut neighbor_scores: HashMap<i64, f64> = HashMap::new();
    let mut neighbor_order: Vec<i64> = Vec::new();
    'entries: for (sid, entry_score) in &entry_symbols {
        let neighbor_score = entry_score * NEIGHBOR_SCORE_FACTOR;
        let mut candidate_ids: Vec<i64> = Vec::new();
        if let Ok(callers) = store.callers_of(*sid, NEIGHBOR_DEPTH, NEIGHBOR_LIMIT) {
            candidate_ids.extend(callers.iter().map(|e| e.symbol.id));
        }
        if let Ok(callees) = store.callees_of(*sid, NEIGHBOR_DEPTH, NEIGHBOR_LIMIT) {
            candidate_ids.extend(callees.iter().map(|e| e.symbol.id));
        }
        if let Ok((outgoing, incoming)) = store.imports_of(*sid) {
            candidate_ids.extend(outgoing.iter().map(|e| e.symbol.id));
            candidate_ids.extend(incoming.iter().map(|e| e.symbol.id));
        }
        for nid in candidate_ids {
            if nid == -1 {
                continue; // unresolved — no real symbol to expand.
            }
            neighbor_scores
                .entry(nid)
                .and_modify(|s| {
                    if neighbor_score > *s {
                        *s = neighbor_score;
                    }
                })
                .or_insert(neighbor_score);
            if !neighbor_order.contains(&nid) {
                neighbor_order.push(nid);
                if neighbor_order.len() >= params.max_neighbor_ids {
                    break 'entries;
                }
            }
        }
    }

    // Step 3: each neighbor's full symbol source becomes BundleItems too.
    for nid in &neighbor_order {
        let Ok(Some(info)) = store.symbol_info(*nid) else {
            continue;
        };
        let Ok(chunks) = store.symbol_source(*nid) else {
            continue;
        };
        let score = neighbor_scores[nid];
        for (chunk_id, start_line, end_line, content) in chunks {
            items.push(BundleItem {
                path: info.path.clone(),
                qualname: Some(info.qualname.clone()),
                start_line,
                end_line,
                content,
                score,
                chunk_id,
            });
        }
    }
    drop(store);

    // Step 4: pack + render.
    let (selected, omitted) = pack(items, budget_tokens);
    let weak_note = if weak {
        format!("{WEAK_MATCH_TEXT}\n\n")
    } else {
        String::new()
    };
    apply_header(
        fresh_header,
        format!(
            "{question_header}{weak_note}{}",
            render_bundle(&selected, &omitted)
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;

    fn write(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// A keyword-only `AppState` (embedder explicitly `None`, never the
    /// `VEXUS_EMBEDDER`-driven default) so `search_hybrid` here is driven
    /// purely by FTS — deterministic and immune to a mock embedder's
    /// content-blind hash vectors coincidentally pulling in chunks that
    /// have nothing to do with the query text. That determinism is what
    /// makes it possible to assert a neighbor chunk is present in the
    /// output *only* via graph expansion, not because it happened to also
    /// rank as a direct hit.
    fn keyword_only_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        pipeline::index_repo(root, &mut store).unwrap();
        let embedder_slot: OnceLock<Option<std::sync::Arc<dyn vexus_embed::Embedder>>> =
            OnceLock::new();
        let _ = embedder_slot.set(None);
        AppState {
            store: Mutex::new(Some(store)),
            embedder: embedder_slot,
            root: root.to_path_buf(),
            last_generation: std::sync::atomic::AtomicU64::new(0),
            is_writer: true,
        }
    }

    /// `alpha_process` calls `unique_marker_beta`. The query text
    /// ("alpha_process") appears in alpha's own chunk (both its `def` line
    /// and the call site) but nowhere in beta's chunk, whose only content
    /// is its own `def`/`pass` lines — so a keyword-only `search_hybrid`
    /// surfaces alpha alone as the entry, and beta can only appear in the
    /// output via explore's one-hop callee expansion.
    fn chain_repo(root: &std::path::Path) {
        write(
            root,
            "chain.py",
            "def alpha_process():\n    unique_marker_beta()\n\n\ndef unique_marker_beta():\n    pass\n",
        );
    }

    #[test]
    fn explore_expands_one_hop_to_include_the_callee_body() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        let out = explore_text(&state, "alpha_process", None);

        assert!(
            out.starts_with("explore: \"alpha_process\"\n\n"),
            "got: {out:?}"
        );
        assert!(
            out.contains("def alpha_process():"),
            "entry symbol's own body must be present: {out:?}"
        );
        assert!(
            out.contains("def unique_marker_beta():") && out.contains("pass"),
            "callee's body must be present via one-hop expansion even though \
             only alpha matched the question text: {out:?}"
        );
        assert!(
            !out.contains("Related (not included"),
            "both bodies comfortably fit the default budget, nothing should be omitted: {out:?}"
        );
    }

    #[test]
    fn explore_small_budget_drops_the_expanded_neighbor_into_related() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        // Big enough for alpha's own (higher-scored) entry chunk alone
        // (~31 tokens incl. per-item overhead), too small to also fit
        // beta's (half-scored) neighbor chunk (~28 more).
        let out = explore_text(&state, "alpha_process", Some(40));

        assert!(
            out.contains("def alpha_process():"),
            "higher-scored entry must still be kept: {out:?}"
        );
        assert!(
            !out.contains("def unique_marker_beta():"),
            "neighbor's body must be dropped, not rendered, under a tight budget: {out:?}"
        );
        assert!(
            out.contains("Related (not included, raise budget_tokens or use `open`):")
                && out.contains("unique_marker_beta"),
            "dropped neighbor must be named in the related/omitted footer: {out:?}"
        );
    }

    #[test]
    fn explore_no_match_returns_exact_text_with_header() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        let out = explore_text(&state, "", None);

        assert_eq!(
            out,
            format!(
                "explore: \"\"\n\nnothing indexed matches that question — try 'search' with \
                 distinctive words from the code, or 'status' to check index coverage."
            )
        );
    }

    /// `MockEmbedder` that declares a distance floor no hash vector will
    /// ever clear — every KNN candidate lands above it, so a query with no
    /// keyword overlap must come back `WeakVectorOnly`. A wrapper type
    /// (rather than the `VEXUS_KNN_FLOOR` env override) keeps the test free
    /// of env races with concurrently running tests.
    struct FlooredMock;
    impl vexus_embed::Embedder for FlooredMock {
        fn id(&self) -> &str {
            "mock"
        }
        fn dim(&self) -> usize {
            vexus_embed::MockEmbedder.dim()
        }
        fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            vexus_embed::MockEmbedder.embed(texts)
        }
        fn distance_floor(&self) -> Option<f64> {
            Some(1e-4)
        }
    }

    /// Like `keyword_only_state`, but embedded with the mock model and a
    /// floor-declaring embedder installed, so the weak-match path is
    /// reachable (it needs a query vector AND a floor).
    fn floored_mock_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        pipeline::index_repo(root, &mut store).unwrap();
        let embedder = FlooredMock;
        store
            .set_model(
                vexus_embed::Embedder::id(&embedder),
                vexus_embed::Embedder::dim(&embedder),
            )
            .unwrap();
        pipeline::embed_pending(&mut store, &embedder).unwrap();
        let embedder_slot: OnceLock<Option<std::sync::Arc<dyn vexus_embed::Embedder>>> =
            OnceLock::new();
        let _ = embedder_slot.set(Some(std::sync::Arc::new(FlooredMock)));
        AppState {
            store: Mutex::new(Some(store)),
            embedder: embedder_slot,
            root: root.to_path_buf(),
            last_generation: std::sync::atomic::AtomicU64::new(0),
            is_writer: true,
        }
    }

    #[test]
    fn explore_weak_match_prepends_note_and_skips_graph_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = floored_mock_state(root);

        // No keyword overlap with the fixture, and the floor guarantees no
        // KNN candidate counts as near: weak match. (On a corpus this tiny
        // every chunk comes back as an entry hit via the nearest-neighbor
        // fallback, so skipping expansion isn't observable in the output —
        // the note is the contract this test pins.)
        let out = explore_text(&state, "zzqqxx unrelated nonsense", None);
        assert!(
            out.contains("weak match —"),
            "weak note must be present: {out:?}"
        );

        // A query with keyword overlap stays strong: no weak note.
        let out = explore_text(&state, "alpha_process", None);
        assert!(
            !out.contains("weak match —"),
            "keyword hit must suppress the weak note: {out:?}"
        );
    }

    #[test]
    fn explore_prepends_freshness_header_when_reconciling_absent_when_fresh() {
        use vexus_watch::{set_freshness, Freshness};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        let out_fresh = explore_text(&state, "alpha_process", None);
        assert!(
            !out_fresh.starts_with('\u{26a0}'),
            "Fresh index must not carry the warning header: {out_fresh:?}"
        );
        assert!(out_fresh.starts_with("explore: \"alpha_process\"\n\n"));

        {
            let mut guard = state.store.lock().unwrap();
            set_freshness(guard.as_mut().unwrap(), Freshness::Reconciling).unwrap();
        }
        let out_reconciling = explore_text(&state, "alpha_process", None);
        assert!(
            out_reconciling.starts_with(
                "⚠ index reconciling — results may miss recent changes\n\nexplore: \"alpha_process\"\n\n"
            ),
            "got: {out_reconciling:?}"
        );
    }
}
