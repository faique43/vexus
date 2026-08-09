//! Indexes one `eval/corpora/{name}` fixture into a fresh temp-directory
//! index, builds an `AppState` over it (the same struct `vexus serve` uses —
//! see `vexus_mcp::state::AppState`), and scores every applicable query/edge
//! row for that corpus through the SAME functions the MCP tools call
//! (`Store::search_hybrid`, `vexus_mcp::tools::explore::explore_text`,
//! `Store::callees_of`) against the pure math in `metrics.rs`.
//!
//! Only `queries.yaml` rows with `tool: search` and `tool: explore` feed a
//! named metric. The metrics are exactly: recall@5, recall@10, mrr,
//! ndcg@10 (search); answer_in_bundle (explore); edge_precision,
//! edge_recall (callers/callees vs **labeled ground truth**). The
//! `tool: callers`/`tool: callees` rows in `queries.yaml` exist for
//! query-corpus diversity and are validated for resolvability by the
//! corpora gate test, but edge_precision/edge_recall are computed
//! exclusively from `eval/edges/{repo}.yaml`'s labeled pairs, not from those
//! rows — so they're loaded (to keep `serde_yaml` happy parsing the whole
//! file) but deliberately don't feed any metric here.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{ensure, Context, Result};
use vexus_core::query::Resolution;
use vexus_core::Store;
use vexus_embed::Embedder;
use vexus_mcp::state::AppState;
use vexus_mcp::tools::explore::explore_text;

use crate::metrics::{self, Accum, EdgeCounts, LabeledEdge};
use crate::queries;

/// Raw `SearchHit`s are fetched well past the largest cutoff (`@10`) so that
/// filtering out `qualname: None` preamble/module-level chunks (the binding
/// "None -> skip row" rule) doesn't shrink the effective top-10 window before
/// recall@10/MRR/nDCG@10 ever see it.
const SEARCH_FETCH_LIMIT: u32 = 20;

/// `callees_of` depth-1 row limit for edge scoring. These are hand-authored
/// fixture corpora (~25-30 files each) where no single symbol has anywhere
/// near this many real callees — generous headroom against truncation, which
/// would otherwise silently undercount `edge_precision`'s denominator.
const EDGE_CALLEES_LIMIT: u32 = 100;

/// Lists the corpus names under `eval_root/corpora/` (sorted, so output
/// order is stable) — whatever's actually there, rather than a hardcoded
/// `["pyapp", "polyglot"]`, so a future corpus is picked up with no change
/// to this runner.
pub fn discover_corpora(eval_root: &Path) -> Result<Vec<String>> {
    let corpora_dir = eval_root.join("corpora");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&corpora_dir)
        .with_context(|| format!("read {}", corpora_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Per-corpus accumulated raw results — the sums/counts `main.rs` pools
/// across corpora for "overall" (see `metrics.rs`'s aggregation note: pooling
/// these, never averaging each corpus's own [`MetricSet`], is what makes
/// "overall" correct).
#[derive(Debug, Clone, Copy, Default)]
pub struct CorpusAccum {
    pub recall_at_5: Accum,
    pub recall_at_10: Accum,
    pub mrr: Accum,
    pub ndcg_at_10: Accum,
    pub clean_at_5: Accum,
    pub answer_in_bundle: Accum,
    pub bundle_clean: Accum,
    pub edges: EdgeCounts,
}

impl CorpusAccum {
    pub fn combine(self, other: Self) -> Self {
        Self {
            recall_at_5: self.recall_at_5.combine(other.recall_at_5),
            recall_at_10: self.recall_at_10.combine(other.recall_at_10),
            mrr: self.mrr.combine(other.mrr),
            ndcg_at_10: self.ndcg_at_10.combine(other.ndcg_at_10),
            clean_at_5: self.clean_at_5.combine(other.clean_at_5),
            answer_in_bundle: self.answer_in_bundle.combine(other.answer_in_bundle),
            bundle_clean: self.bundle_clean.combine(other.bundle_clean),
            edges: self.edges.combine(other.edges),
        }
    }

    /// The final, rounded [`metrics::MetricSet`] — the JSON/table shape.
    pub fn metrics(&self) -> metrics::MetricSet {
        metrics::MetricSet {
            recall_at_5: metrics::round4(self.recall_at_5.mean()),
            recall_at_10: metrics::round4(self.recall_at_10.mean()),
            mrr: metrics::round4(self.mrr.mean()),
            ndcg_at_10: metrics::round4(self.ndcg_at_10.mean()),
            clean_at_5: metrics::round4(self.clean_at_5.mean()),
            answer_in_bundle: metrics::round4(self.answer_in_bundle.mean()),
            bundle_clean: metrics::round4(self.bundle_clean.mean()),
            edge_precision: metrics::round4(self.edges.precision()),
            edge_recall: metrics::round4(self.edges.recall()),
        }
    }
}

/// Indexes `eval_root/corpora/{name}` into a fresh `tempfile::tempdir()`
/// database (never in-place under `eval/` — no `.vexus` artifact is ever
/// created there), embeds it with `embedder`, then scores every applicable
/// `queries.yaml`/`edges.yaml` row for that corpus.
pub fn eval_corpus(
    eval_root: &Path,
    name: &str,
    embedder: &Arc<dyn Embedder>,
) -> Result<CorpusAccum> {
    let corpus_root = eval_root.join("corpora").join(name);
    ensure!(
        corpus_root.is_dir(),
        "no such corpus directory: {}",
        corpus_root.display()
    );

    // The tempdir guard must outlive every query below: dropping it would
    // remove the directory `index.db` lives in while the store's WAL-mode
    // connection may still need to (re)create `-wal`/`-shm` companion files
    // there (see `vexus_mcp`'s `open_reader_with_probe` doc comment for the
    // same hazard) — so it stays bound as `_db_dir` for this whole function,
    // not dropped the moment `index_into_temp_state` returns.
    let (state, _db_dir) = index_into_temp_state(&corpus_root, embedder)?;

    let queries_path = eval_root.join("queries").join(format!("{name}.yaml"));
    let queries = queries::load_queries(&queries_path)?;
    let edges_path = eval_root.join("edges").join(format!("{name}.yaml"));
    let edges = queries::load_edges(&edges_path)?;

    let mut accum = CorpusAccum::default();

    for query in queries.iter().filter(|q| q.tool == "search") {
        score_search_query(&state, embedder, query, &mut accum)?;
    }
    for query in queries.iter().filter(|q| q.tool == "explore") {
        score_explore_query(&state, query, &mut accum);
    }

    let labeled: Vec<LabeledEdge> = edges
        .into_iter()
        .map(|e| LabeledEdge {
            caller: e.caller,
            callee: e.callee,
        })
        .collect();
    accum.edges = metrics::edge_counts(&labeled, |caller| depth1_resolved_callees(&state, caller));

    Ok(accum)
}

/// Builds a fresh, fully indexed + embedded `AppState` over `corpus_root`,
/// backed by a brand-new temp-directory database — the exact same struct
/// `vexus serve`'s tool handlers run against (see `vexus_mcp::state`), so
/// `explore_text`/`Store::search_hybrid`/`Store::callees_of` below are the
/// real production code paths, not a reimplementation of them.
///
/// Returns the `tempfile::TempDir` guard alongside the `AppState`: the
/// caller must keep it alive for as long as `AppState` is in use (see
/// `eval_corpus`'s `_db_dir` binding) since dropping it deletes the
/// directory `index.db` lives in, which a live WAL-mode connection may still
/// need to (re)create `-wal`/`-shm` companion files in.
pub fn index_into_temp_state(
    corpus_root: &Path,
    embedder: &Arc<dyn Embedder>,
) -> Result<(AppState, tempfile::TempDir)> {
    let db_dir = tempfile::tempdir().context("tempdir for eval index")?;
    let mut store = Store::open(&db_dir.path().join("index.db"))
        .with_context(|| format!("open temp eval store for {}", corpus_root.display()))?;
    let report = vexus_watch::pipeline::index_repo(corpus_root, &mut store)
        .with_context(|| format!("index corpus at {}", corpus_root.display()))?;
    ensure!(
        report.failed.is_empty(),
        "corpus at {} failed to index cleanly: {:?}",
        corpus_root.display(),
        report.failed
    );
    ensure!(
        report.indexed > 0,
        "corpus at {} indexed 0 files",
        corpus_root.display()
    );

    store.set_model(embedder.id(), embedder.dim())?;
    vexus_watch::pipeline::embed_pending(&mut store, embedder.as_ref())
        .with_context(|| format!("embed corpus at {}", corpus_root.display()))?;

    let embedder_slot: OnceLock<Option<Arc<dyn Embedder>>> = OnceLock::new();
    let _ = embedder_slot.set(Some(embedder.clone()));
    let state = AppState {
        store: Mutex::new(Some(store)),
        embedder: embedder_slot,
        root: corpus_root.to_path_buf(),
        last_generation: AtomicU64::new(0),
        is_writer: true,
    };
    Ok((state, db_dir))
}

/// Scores one `tool: search` query: embeds `query.q` directly with
/// `embedder` (skipping `vexus_mcp::tools::embed_query`'s model-mismatch
/// guard — that guard exists for a long-lived server process where the
/// active embedder might not match what's indexed; here we just called
/// `store.set_model` with this exact embedder a moment ago, so there's
/// nothing for it to catch), then calls the same `Store::search_hybrid`
/// `tools::search::search_text` calls internally, and folds the ranked
/// qualname list into `accum` via `recall_at_k`/`reciprocal_rank`/
/// `ndcg_at_10`.
fn score_search_query(
    state: &AppState,
    embedder: &Arc<dyn Embedder>,
    query: &queries::Query,
    accum: &mut CorpusAccum,
) -> Result<()> {
    let query_vec = embedder
        .embed(&[query.q.as_str()])
        .with_context(|| format!("embed query {:?}", query.q))?
        .pop();

    let hits = {
        let store = state
            .lock_store_fresh()
            .map_err(|msg| anyhow::anyhow!("{msg}"))?;
        // The embedder's own floor so the real-model eval exercises exactly
        // what serving does; the mock embedder declares no floor, keeping
        // the CI baseline untouched.
        let floor = vexus_embed::effective_distance_floor(embedder.as_ref());
        store
            .search_hybrid_scored(&query.q, query_vec.as_deref(), floor, SEARCH_FETCH_LIMIT)?
            .0
    };
    // Binding rule: "ranked qualnames from SearchHit.qualname (None -> skip
    // row)" — a preamble/module-level chunk carries no qualname at all, so it
    // isn't a candidate the ranking can credit or blame; it's removed from
    // the sequence entirely rather than counted as an empty-qualname rank.
    let ranked: Vec<String> = hits.into_iter().filter_map(|h| h.qualname).collect();

    accum
        .recall_at_5
        .push(metrics::recall_at_k(&ranked, &query.expect, 5));
    accum
        .recall_at_10
        .push(metrics::recall_at_k(&ranked, &query.expect, 10));
    accum
        .mrr
        .push(metrics::reciprocal_rank(&ranked, &query.expect, 10));
    if !query.graded.is_empty() {
        accum
            .ndcg_at_10
            .push(metrics::ndcg_at_10(&ranked, &query.graded));
    }
    // Same "not applicable, don't push" contract as `graded`/ndcg@10: an
    // empty forbidden set contributes nothing rather than a fake 0.0.
    if !query.expect_not.is_empty() {
        accum
            .clean_at_5
            .push(metrics::clean_at_k(&ranked, &query.expect_not, 5));
    }
    Ok(())
}

/// Scores one `tool: explore` query at the DEFAULT budget (`None` —
/// `explore_text`'s own default), pushing `1.0`/`0.0` into
/// `accum.answer_in_bundle`.
fn score_explore_query(state: &AppState, query: &queries::Query, accum: &mut CorpusAccum) {
    let bundle = explore_text(state, &query.q, None);
    let passed = metrics::answer_in_bundle(&bundle, &query.expect, |qualname| {
        first_chunk_content(state, qualname)
    });
    accum.answer_in_bundle.push(if passed { 1.0 } else { 0.0 });
    if !query.expect_not.is_empty() {
        accum.bundle_clean.push(metrics::bundle_clean(
            &bundle,
            &query.expect_not,
            |qualname| first_chunk_content(state, qualname),
        ));
    }
}

/// The first (lowest `start_line`) source chunk's content for a resolved
/// qualname — `None` if it doesn't resolve to exactly one symbol, or
/// resolves but owns zero chunks (see `metrics::answer_in_bundle`'s doc
/// comment for why that's "not found", not a panic or a vacuous pass).
fn first_chunk_content(state: &AppState, qualname: &str) -> Option<String> {
    let store = state.lock_store_fresh().ok()?;
    let Resolution::Exact(info) = store.resolve_symbol(qualname).ok()? else {
        return None;
    };
    let chunks = store.symbol_source(info.id).ok()?;
    chunks.into_iter().next().map(|(_, _, _, content)| content)
}

/// A resolved caller's full depth-1 callee qualname set — the closure
/// `metrics::edge_counts` expects. Only resolved rows count (`symbol.id !=
/// -1`): an unresolved `EdgeHit`'s "qualname" is really just the raw
/// (unqualified) call-site text, which is never itself a real fully
/// qualified ground-truth qualname, so including it could only ever produce
/// a false match by pure string coincidence. Returns an empty `Vec` (rather
/// than propagating an error) when `caller` doesn't resolve to exactly one
/// symbol — the corpora validation test already guarantees every `edges.yaml`
/// caller resolves `Exact` against this same corpus, so this is a defensive
/// fallback, not an expected path.
fn depth1_resolved_callees(state: &AppState, caller: &str) -> Vec<String> {
    let Ok(store) = state.lock_store_fresh() else {
        return Vec::new();
    };
    let Ok(Resolution::Exact(info)) = store.resolve_symbol(caller) else {
        return Vec::new();
    };
    let Ok(edges) = store.callees_of(info.id, 1, EDGE_CALLEES_LIMIT) else {
        return Vec::new();
    };
    edges
        .into_iter()
        .filter(|e| e.symbol.id != -1)
        .map(|e| e.symbol.qualname)
        .collect()
}

/// Only used by `main.rs`'s `--real` sanity check, to build a real
/// `AppState`-free embedder for a plain smoke test without indexing a whole
/// corpus. Kept here (rather than duplicated in `main.rs`) since it's the
/// same "home dir -> ~/.vexus/models -> ensure_model -> OnnxEmbedder::load"
/// sequence `--real` runs use to build the real embedder in the first place.
pub fn build_real_embedder() -> Result<Arc<dyn Embedder>> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context(
            "no HOME/USERPROFILE set — --real needs ~/.vexus/models/<model> on disk to load from",
        )?;
    let models_root = home.join(".vexus/models");
    let model_dir = vexus_embed::ensure_model(&vexus_embed::JINA_CODE_V2, &models_root).context(
        "ensure_model for --real (expected an already-downloaded model under ~/.vexus/models)",
    )?;
    let embedder = vexus_embed::OnnxEmbedder::load(&model_dir, &vexus_embed::JINA_CODE_V2)
        .context("load the real ONNX embedder for --real")?;
    Ok(Arc::new(embedder))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../eval")
            .canonicalize()
            .expect("eval/ must exist at the repo root")
    }

    fn mock_embedder() -> Arc<dyn Embedder> {
        Arc::new(vexus_embed::MockEmbedder)
    }

    #[test]
    fn discover_corpora_finds_pyapp_and_polyglot() {
        let names = discover_corpora(&eval_root()).unwrap();
        assert!(names.contains(&"pyapp".to_string()), "{names:?}");
        assert!(names.contains(&"polyglot".to_string()), "{names:?}");
        // Sorted, so the output/JSON key order is stable.
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn eval_corpus_pyapp_produces_sane_nonzero_metrics_under_mock() {
        let accum = eval_corpus(&eval_root(), "pyapp", &mock_embedder()).unwrap();
        let m = accum.metrics();

        for (name, value) in [
            ("recall@5", m.recall_at_5),
            ("recall@10", m.recall_at_10),
            ("mrr", m.mrr),
            ("ndcg@10", m.ndcg_at_10),
            ("answer_in_bundle", m.answer_in_bundle),
            ("edge_precision", m.edge_precision),
            ("edge_recall", m.edge_recall),
        ] {
            assert!(
                (0.0..=1.0).contains(&value),
                "{name} out of [0,1] range: {value}"
            );
        }
        // A hand-authored corpus this small, with an exact-substring search
        // query set, should not score a flat zero on the primary retrieval
        // metrics under the mock embedder (keyword/FTS half of the hybrid
        // fusion alone should find plenty) — a hard zero here would mean
        // something upstream (indexing, query loading, or the scoring glue
        // itself) is broken, not that retrieval quality is merely mediocre.
        assert!(m.recall_at_10 > 0.0, "{m:?}");
        assert!(m.mrr > 0.0, "{m:?}");
        assert!(m.answer_in_bundle > 0.0, "{m:?}");
        assert_eq!(accum.edges.labeled, 57, "eval/edges/pyapp.yaml row count");
    }

    #[test]
    fn eval_corpus_unknown_name_errors() {
        let err = eval_corpus(&eval_root(), "does-not-exist", &mock_embedder()).unwrap_err();
        assert!(format!("{err:#}").contains("no such corpus"));
    }

    #[test]
    fn eval_corpus_never_leaves_a_vexus_dir_under_eval() {
        let _ = eval_corpus(&eval_root(), "pyapp", &mock_embedder()).unwrap();
        let vexus_dir = eval_root().join("corpora/pyapp/.vexus");
        assert!(
            !vexus_dir.exists(),
            "found a stray {vexus_dir:?} — the eval index must live in a tempdir, never in-place"
        );
    }
}
