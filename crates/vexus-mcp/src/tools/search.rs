//! `search` tool: hybrid semantic+keyword search over the code index,
//! rendered as a ranked list of locations + short excerpts. Deliberately
//! carries no full symbol bodies — that's what `open`/`explore` are for.

use vexus_core::model::estimate_tokens;
#[cfg(test)]
use vexus_watch::pipeline;

use crate::state::AppState;
use crate::tools::{clamp_budget, embed_query};

const DEFAULT_LIMIT: u32 = 10;
const MAX_LIMIT: u32 = 100;
const DEFAULT_BUDGET_TOKENS: u32 = 4000;

/// Pure inner implementation of the `search` tool. See `tools::embed_query`
/// for how the query vector is (or isn't) produced.
pub fn search_text(
    state: &AppState,
    query: &str,
    limit: Option<u32>,
    budget_tokens: Option<u32>,
) -> String {
    // `0` would silently return nothing at all, and an absurd caller-
    // supplied value would ask `search_hybrid` to rank far more rows than
    // any real caller reads — clamp to a sane [1, 100] range either way.
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let budget_tokens = clamp_budget(budget_tokens, DEFAULT_BUDGET_TOKENS);

    // Embed before locking: a real embedder's inference call must not hold
    // the store mutex, or it stalls every other tool call for its duration.
    let query_vec = embed_query(state, query);

    let store = state.lock_store_fresh();
    let hits = match store.search_hybrid(query, query_vec.as_deref(), limit) {
        Ok(hits) => hits,
        Err(e) => return format!("search error: {e:#}"),
    };
    drop(store);

    if hits.is_empty() {
        return format!(
            "no matches for \"{query}\" — try broader terms, or 'explore' with a question."
        );
    }

    let mut out = String::new();
    let mut used_tokens = 0u32;
    for (i, hit) in hits.iter().enumerate() {
        let qual = hit.qualname.as_deref().unwrap_or("(preamble)");
        let line = format!(
            "{}. {}  {}:{}-{}  score {:.3}\n   {}\n",
            i + 1,
            qual,
            hit.path,
            hit.start_line,
            hit.end_line,
            hit.score,
            hit.excerpt
        );
        let cost = estimate_tokens(&line);
        // Always keep at least the top hit even if it alone exceeds budget —
        // an empty result for a real match set would be a worse answer than
        // one slightly-over-budget line.
        if !out.is_empty() && used_tokens + cost > budget_tokens {
            break;
        }
        out.push_str(&line);
        used_tokens += cost;
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use vexus_embed::Embedder as _;

    use super::*;

    fn write(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Builds an `AppState` over a freshly indexed + embedded temp repo,
    /// using `MockEmbedder` directly (never `VEXUS_EMBEDDER`) so the test is
    /// hermetic regardless of process environment.
    fn indexed_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        pipeline::index_repo(root, &mut store).unwrap();
        let embedder = vexus_embed::MockEmbedder;
        store.set_model(embedder.id(), embedder.dim()).unwrap();
        pipeline::embed_pending(&mut store, &embedder).unwrap();

        let embedder_slot = OnceLock::new();
        let _ = embedder_slot.set(Some(std::sync::Arc::new(vexus_embed::MockEmbedder)
            as std::sync::Arc<dyn vexus_embed::Embedder>));
        AppState {
            store: Mutex::new(store),
            embedder: embedder_slot,
            root: root.to_path_buf(),
            last_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn keyword_only_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        pipeline::index_repo(root, &mut store).unwrap();

        let embedder_slot: OnceLock<Option<std::sync::Arc<dyn vexus_embed::Embedder>>> =
            OnceLock::new();
        let _ = embedder_slot.set(None);
        AppState {
            store: Mutex::new(store),
            embedder: embedder_slot,
            root: root.to_path_buf(),
            last_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[test]
    fn search_returns_ranked_lines_with_excerpts_no_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.py",
            "def compute_total(items):\n    return sum(items)\n",
        );
        write(root, "b.py", "def unrelated():\n    return None\n");

        let state = indexed_state(root);
        let out = search_text(&state, "compute_total", None, None);

        assert!(out.starts_with("1. "), "got: {out:?}");
        assert!(out.contains("a.py:"), "got: {out:?}");
        assert!(out.contains("score "), "got: {out:?}");
        assert!(
            !out.contains("return sum(items)") || out.contains("compute_total"),
            "excerpt line present: {out:?}"
        );
        // No full-body rendering (no fenced code block markers from bundle rendering).
        assert!(
            !out.contains("```"),
            "search must not render full bodies: {out:?}"
        );
    }

    #[test]
    fn search_empty_hits_returns_exact_no_match_text() {
        // An empty repo (nothing indexed) has no chunks in either the FTS or
        // vector table, so `search_hybrid` returns no hits regardless of
        // embedder availability — unlike a nonempty repo, where the mock
        // embedder's KNN branch would always surface *some* "nearest"
        // (if meaningless) chunk.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let state = indexed_state(root);
        let out = search_text(&state, "zzz_nonexistent_term_zzz", None, None);
        assert_eq!(
            out,
            "no matches for \"zzz_nonexistent_term_zzz\" — try broader terms, or 'explore' with a question."
        );
    }

    #[test]
    fn search_degrades_to_keyword_only_without_an_embedder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.py",
            "def compute_total(items):\n    return sum(items)\n",
        );

        let state = keyword_only_state(root);
        let out = search_text(&state, "compute_total", None, None);
        assert!(out.contains("compute_total"), "got: {out:?}");
    }

    #[test]
    fn search_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..5 {
            write(
                root,
                &format!("f{i}.py"),
                &format!("def shared_term_fn_{i}():\n    return {i}\n"),
            );
        }

        let state = indexed_state(root);
        let out = search_text(&state, "shared_term_fn", Some(2), None);
        let numbered_lines = out
            .lines()
            .filter(|l| l.contains(". ") && l.starts_with(char::is_numeric))
            .count();
        assert!(
            numbered_lines <= 2,
            "expected at most 2 results, got: {out:?}"
        );
    }

    #[test]
    fn search_limit_clamped_to_100_and_never_zero() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..5 {
            write(
                root,
                &format!("f{i}.py"),
                &format!("def shared_term_fn_{i}():\n    return {i}\n"),
            );
        }

        let state = indexed_state(root);

        // An absurd requested limit must not reach `search_hybrid` verbatim
        // — clamped to 100 is still plenty for this 5-hit fixture, so this
        // just proves the call doesn't error or misbehave under a huge
        // input; the real guard is `MAX_LIMIT` itself.
        let out_huge = search_text(&state, "shared_term_fn", Some(u32::MAX), None);
        assert!(
            !out_huge.is_empty(),
            "an absurd limit must still return results, not blow up: {out_huge:?}"
        );

        // `0` must not silently return nothing — clamped up to at least 1.
        let out_zero = search_text(&state, "shared_term_fn", Some(0), None);
        assert!(
            out_zero.starts_with("1. "),
            "limit 0 must clamp to at least 1 result, got: {out_zero:?}"
        );
    }
}
