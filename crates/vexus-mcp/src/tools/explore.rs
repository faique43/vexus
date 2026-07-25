//! `explore` tool (the flagship): answer a question about the codebase in
//! one call. Pipeline (binding, per the Task 7 brief):
//!
//! 1. `search_hybrid(question, embed(question), 12)` → entry chunks,
//!    rendered as `BundleItem`s carrying their RRF score.
//! 2. For each entry chunk's `symbol_id` (deduped, first-seen score wins,
//!    max 8 distinct symbols — `search_hybrid`'s results are already score-
//!    descending, so "first 8 distinct" is "top 8 by score"): walk
//!    `callers_of(id, 1, 10)`, `callees_of(id, 1, 10)`, and `imports_of(id)`
//!    for neighbor symbol ids. Only resolved neighbors (id != -1) count;
//!    collection stops once 24 distinct neighbor ids are found.
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

use crate::bundle::{pack, BundleItem};
use crate::format::render_bundle;
use crate::state::AppState;
use crate::tools::{clamp_budget, embed_query};

const DEFAULT_BUDGET_TOKENS: u32 = 8000;
const ENTRY_LIMIT: u32 = 12;
const MAX_ENTRY_SYMBOLS: usize = 8;
const MAX_NEIGHBOR_IDS: usize = 24;
const NEIGHBOR_DEPTH: u32 = 1;
const NEIGHBOR_LIMIT: u32 = 10;
const NEIGHBOR_SCORE_FACTOR: f64 = 0.5;

const NO_MATCH_TEXT: &str = "nothing indexed matches that question — try 'search' with distinctive words from the code, or 'status' to check index coverage.";

/// Pure inner implementation of the `explore` tool.
pub fn explore_text(state: &AppState, question: &str, budget_tokens: Option<u32>) -> String {
    let budget_tokens = clamp_budget(budget_tokens, DEFAULT_BUDGET_TOKENS);
    let header = format!("explore: \"{question}\"\n\n");

    let store = state.store.lock().expect("store mutex poisoned");
    let query_vec = embed_query(state, &store, question);
    let hits = match store.search_hybrid(question, query_vec.as_deref(), ENTRY_LIMIT) {
        Ok(h) => h,
        Err(e) => return format!("explore error: {e:#}"),
    };

    if hits.is_empty() {
        return format!("{header}{NO_MATCH_TEXT}");
    }

    // Step 1: entry chunks as BundleItems, plus the deduped (symbol_id,
    // score) list step 2 expands from. `hits` is already score-descending
    // (search_hybrid's RRF ranking), so the first occurrence of a symbol_id
    // is that symbol's best score among the entries, and capping at the
    // first 8 distinct ids keeps the top 8 by score.
    let mut items: Vec<BundleItem> = Vec::with_capacity(hits.len());
    let mut entry_symbols: Vec<(i64, f64)> = Vec::with_capacity(MAX_ENTRY_SYMBOLS);
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
        if let Some(sid) = hit.symbol_id {
            if entry_symbols.len() < MAX_ENTRY_SYMBOLS
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
                if neighbor_order.len() >= MAX_NEIGHBOR_IDS {
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
    format!("{header}{}", render_bundle(&selected, &omitted))
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
        vexus_embed::pipeline::index_repo(root, &mut store).unwrap();
        let embedder_slot: OnceLock<Option<std::sync::Arc<dyn vexus_embed::Embedder>>> =
            OnceLock::new();
        let _ = embedder_slot.set(None);
        AppState {
            store: Mutex::new(store),
            embedder: embedder_slot,
            root: root.to_path_buf(),
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
}
