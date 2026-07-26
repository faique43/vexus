//! `callers`/`callees`/`impact` tools: transitive call-graph traversal,
//! rendered as an edge tree (`callers`/`callees`) or a depth-grouped summary
//! (`impact`).
//!
//! Per the Task 2 review note: `EdgeHit`s carry a fully hydrated
//! `SymbolInfo` already (including the synthetic `id: -1` placeholder for
//! unresolved callees) — nothing here re-fetches `symbol_info` for them.
//! Rendering always goes straight off the `EdgeHit` itself.

use std::collections::HashSet;

use vexus_core::model::estimate_tokens;
use vexus_core::query::{EdgeHit, IMPACT_ROW_CAP};
#[cfg(test)]
use vexus_watch::pipeline;

use crate::format::render_edge_tree;
use crate::state::{freshness_header, AppState};
use crate::tools::{apply_header, clamp_budget, resolve_or_text};

const DEFAULT_BUDGET_TOKENS: u32 = 4000;
/// `callers_of`/`callees_of` row limit — generous enough for any real
/// depth-1..3 fan-out, independent of `impact`'s much larger hard cap.
const CALLERS_CALLEES_LIMIT: u32 = 50;

/// Stable-sort so unresolved rows (confidence `None`) land after resolved
/// rows within the same depth, without disturbing the query's own
/// `(depth, id)` ordering among rows that tie on `(depth, is_unresolved)`.
fn sort_unresolved_last(edges: &mut [EdgeHit]) {
    edges.sort_by_key(|e| (e.depth, e.confidence.is_none()));
}

/// Render `header`, then as many `render_edge_tree` lines as fit under
/// `budget_tokens`, appending a `… (truncated)` marker line if not all of
/// `edges` made it in.
fn render_capped(header: String, edges: &[EdgeHit], budget_tokens: u32) -> String {
    let mut out = header;
    out.push('\n');
    let mut used = estimate_tokens(&out);
    let mut truncated = false;
    for edge in edges {
        let line = render_edge_tree(std::slice::from_ref(edge));
        let cost = estimate_tokens(&line);
        if used + cost > budget_tokens {
            truncated = true;
            break;
        }
        out.push_str(&line);
        used += cost;
    }
    if truncated {
        out.push_str("… (truncated)\n");
    }
    out
}

/// Pure inner implementation of the `callers` tool.
pub fn callers_text(
    state: &AppState,
    symbol: &str,
    depth: Option<u32>,
    budget_tokens: Option<u32>,
) -> String {
    let depth = depth.unwrap_or(1).clamp(1, 3);
    let budget_tokens = clamp_budget(budget_tokens, DEFAULT_BUDGET_TOKENS);

    let store = match state.lock_store_fresh() {
        Ok(s) => s,
        Err(msg) => return msg,
    };
    let fresh_header = freshness_header(&store);
    let info = match resolve_or_text(&store, symbol) {
        Ok(info) => info,
        Err(text) => return apply_header(fresh_header, text),
    };
    let mut edges = match store.callers_of(info.id, depth, CALLERS_CALLEES_LIMIT) {
        Ok(e) => e,
        Err(e) => return apply_header(fresh_header, format!("callers error: {e:#}")),
    };
    drop(store);

    sort_unresolved_last(&mut edges);
    let header = format!(
        "{} caller(s) of {} (depth {}):",
        edges.len(),
        info.qualname,
        depth
    );
    apply_header(fresh_header, render_capped(header, &edges, budget_tokens))
}

/// Pure inner implementation of the `callees` tool.
pub fn callees_text(
    state: &AppState,
    symbol: &str,
    depth: Option<u32>,
    budget_tokens: Option<u32>,
) -> String {
    let depth = depth.unwrap_or(1).clamp(1, 3);
    let budget_tokens = clamp_budget(budget_tokens, DEFAULT_BUDGET_TOKENS);

    let store = match state.lock_store_fresh() {
        Ok(s) => s,
        Err(msg) => return msg,
    };
    let fresh_header = freshness_header(&store);
    let info = match resolve_or_text(&store, symbol) {
        Ok(info) => info,
        Err(text) => return apply_header(fresh_header, text),
    };
    let mut edges = match store.callees_of(info.id, depth, CALLERS_CALLEES_LIMIT) {
        Ok(e) => e,
        Err(e) => return apply_header(fresh_header, format!("callees error: {e:#}")),
    };
    drop(store);

    sort_unresolved_last(&mut edges);
    let header = format!(
        "{} callee(s) of {} (depth {}):",
        edges.len(),
        info.qualname,
        depth
    );
    apply_header(fresh_header, render_capped(header, &edges, budget_tokens))
}

/// Pure inner implementation of the `impact` tool: the transitive caller
/// graph (per `impact_of`) plus module-level import dependents (the
/// incoming side of `imports_of`) — "blast radius" per the tool's shipped
/// description covers both, even though the plan's interface line only
/// spelled out the call-graph half.
pub fn impact_text(state: &AppState, symbol: &str, max_depth: Option<u32>) -> String {
    let max_depth = max_depth.unwrap_or(5).clamp(1, 5);

    let store = match state.lock_store_fresh() {
        Ok(s) => s,
        Err(msg) => return msg,
    };
    let fresh_header = freshness_header(&store);
    let info = match resolve_or_text(&store, symbol) {
        Ok(info) => info,
        Err(text) => return apply_header(fresh_header, text),
    };
    let edges = match store.impact_of(info.id, max_depth) {
        Ok(e) => e,
        Err(e) => return apply_header(fresh_header, format!("impact error: {e:#}")),
    };
    // Only the incoming side matters for "blast radius" — files that import
    // this symbol's module (i.e. would be affected by changing it), not
    // modules this file itself imports.
    let (_outgoing, importers) = match store.imports_of(info.id) {
        Ok(v) => v,
        Err(e) => return apply_header(fresh_header, format!("impact error: {e:#}")),
    };
    drop(store);

    let row_cap_hit = edges.len() >= IMPACT_ROW_CAP as usize;
    let deepest = edges.iter().map(|e| e.depth).max().unwrap_or(0);

    let mut out = String::new();
    for d in 1..=deepest {
        let group: Vec<EdgeHit> = edges.iter().filter(|e| e.depth == d).cloned().collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("depth {d}:\n"));
        out.push_str(&render_edge_tree(&group));
    }

    if !importers.is_empty() {
        out.push_str("import dependents:\n");
        out.push_str(&render_edge_tree(&importers));
    }

    let mut symbol_ids = HashSet::new();
    let mut files = HashSet::new();
    for e in edges.iter().chain(importers.iter()) {
        symbol_ids.insert(e.symbol.id);
        files.insert(e.symbol.path.clone());
    }
    out.push_str(&format!(
        "affected: {} symbols across {} files\n",
        symbol_ids.len(),
        files.len()
    ));
    if row_cap_hit {
        out.push_str("(row cap reached — results truncated)\n");
    }
    apply_header(fresh_header, out)
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

    /// `main` calls `helper`, which calls `leaf` — a straight depth-2 call
    /// chain, all within one file so the fixture stays simple while still
    /// exercising depth-1 vs depth-2 traversal.
    fn chain_repo(root: &std::path::Path) {
        write(
            root,
            "chain.py",
            "def leaf():\n    pass\n\n\ndef helper():\n    leaf()\n\n\ndef main():\n    helper()\n",
        );
    }

    fn indexed_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        pipeline::index_repo(root, &mut store).unwrap();
        AppState {
            store: Mutex::new(Some(store)),
            embedder: OnceLock::new(),
            root: root.to_path_buf(),
            last_generation: std::sync::atomic::AtomicU64::new(0),
            is_writer: true,
        }
    }

    #[test]
    fn callers_depth_1_and_2_output_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = indexed_state(root);

        let out1 = callers_text(&state, "chain.leaf", Some(1), None);
        assert!(
            out1.starts_with("1 caller(s) of chain.leaf (depth 1):"),
            "got: {out1:?}"
        );
        assert!(out1.contains("chain.helper"), "got: {out1:?}");
        assert!(!out1.contains("chain.main"), "got: {out1:?}");

        let out2 = callers_text(&state, "chain.leaf", Some(2), None);
        assert!(
            out2.starts_with("2 caller(s) of chain.leaf (depth 2):"),
            "got: {out2:?}"
        );
        assert!(out2.contains("chain.helper"), "got: {out2:?}");
        assert!(out2.contains("chain.main"), "got: {out2:?}");
        // depth-2 row (main) must be indented past depth-1 (helper).
        let helper_line = out2.lines().find(|l| l.contains("chain.helper")).unwrap();
        let main_line = out2.lines().find(|l| l.contains("chain.main")).unwrap();
        let helper_indent = helper_line.len() - helper_line.trim_start().len();
        let main_indent = main_line.len() - main_line.trim_start().len();
        assert!(
            main_indent > helper_indent,
            "expected main (depth 2) indented deeper than helper (depth 1): {out2:?}"
        );
    }

    #[test]
    fn callers_depth_clamped_to_3() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = indexed_state(root);

        let out = callers_text(&state, "chain.leaf", Some(99), None);
        assert!(
            out.starts_with("2 caller(s) of chain.leaf (depth 3):"),
            "depth must clamp to 3, got: {out:?}"
        );
    }

    #[test]
    fn callees_output_shape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = indexed_state(root);

        let out = callees_text(&state, "chain.main", Some(1), None);
        assert!(
            out.starts_with("1 callee(s) of chain.main (depth 1):"),
            "got: {out:?}"
        );
        assert!(out.contains("chain.helper"), "got: {out:?}");

        let out2 = callees_text(&state, "chain.main", Some(2), None);
        assert!(
            out2.starts_with("2 callee(s) of chain.main (depth 2):"),
            "got: {out2:?}"
        );
        assert!(out2.contains("chain.leaf"), "got: {out2:?}");
    }

    #[test]
    fn unresolved_rows_sorted_last_within_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // `main` calls both `helper` (resolves) and a nonexistent `ghost`
        // (name-only, never resolves) — both land at depth 1 under `ghost`'s
        // callers... actually simplest: query callees_of(main, 1) where one
        // callee resolves and one doesn't, and check ordering.
        write(
            root,
            "u.py",
            "def helper():\n    pass\n\n\ndef main():\n    helper()\n    ghost()\n",
        );
        let state = indexed_state(root);

        let out = callees_text(&state, "u.main", Some(1), None);
        let helper_pos = out.find("u.helper").expect("helper present");
        let ghost_pos = out.find("ghost").expect("ghost present");
        assert!(
            helper_pos < ghost_pos,
            "resolved helper must render before unresolved ghost: {out:?}"
        );
        assert!(out.contains("[unresolved]"), "got: {out:?}");
    }

    #[test]
    fn callers_candidates_and_notfound_text_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def dup():\n    pass\n");
        write(root, "b.py", "def dup():\n    pass\n");
        let state = indexed_state(root);

        let ambiguous = callers_text(&state, "dup", None, None);
        assert!(ambiguous.contains("a.dup"), "got: {ambiguous:?}");
        assert!(ambiguous.contains("b.dup"), "got: {ambiguous:?}");
        assert!(
            ambiguous.to_lowercase().contains("qualname"),
            "got: {ambiguous:?}"
        );
        assert!(!ambiguous.contains("caller(s)"), "got: {ambiguous:?}");

        let notfound = callers_text(&state, "totally_unknown_xyz", None, None);
        assert!(notfound.contains("no symbol found"), "got: {notfound:?}");
    }

    #[test]
    fn callers_budget_truncation_appends_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Many distinct direct callers of `target` so the rendered body
        // comfortably exceeds a tiny budget.
        let mut content = String::from("def target():\n    pass\n\n\n");
        for i in 0..40 {
            content.push_str(&format!("def caller{i}():\n    target()\n\n\n"));
        }
        write(root, "big.py", &content);
        let state = indexed_state(root);

        let out = callers_text(&state, "big.target", Some(1), Some(20));
        assert!(
            out.contains("… (truncated)"),
            "expected a truncation marker: {out:?}"
        );
        assert!(
            out.starts_with("40 caller(s) of big.target (depth 1):"),
            "header reports the full count regardless of truncation: {out:?}"
        );
    }

    #[test]
    fn impact_groups_by_depth_with_footer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = indexed_state(root);

        let out = impact_text(&state, "chain.leaf", Some(5));
        assert!(out.contains("depth 1:"), "got: {out:?}");
        assert!(out.contains("depth 2:"), "got: {out:?}");
        let depth1_pos = out.find("depth 1:").unwrap();
        let depth2_pos = out.find("depth 2:").unwrap();
        assert!(depth1_pos < depth2_pos, "got: {out:?}");
        assert!(out.contains("chain.helper"), "got: {out:?}");
        assert!(out.contains("chain.main"), "got: {out:?}");
        assert!(
            out.contains("affected: 2 symbols across 1 files"),
            "got: {out:?}"
        );
        assert!(!out.contains("row cap"), "got: {out:?}");
        assert!(
            !out.contains("import dependents:"),
            "no other file imports chain.py, so the section must be omitted entirely: {out:?}"
        );
    }

    #[test]
    fn impact_includes_import_dependents_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def target():\n    pass\n");
        write(root, "b.py", "import a\n");
        let state = indexed_state(root);

        let out = impact_text(&state, "a.target", None);
        assert!(out.contains("import dependents:"), "got: {out:?}");
        assert!(
            out.contains("b  (b.py:1)"),
            "expected b's module symbol line: {out:?}"
        );
        // target has no callers, so the only affected entity is b's module —
        // the footer's file count must still pick up the import dependent.
        assert!(
            out.contains("affected: 1 symbols across 1 files"),
            "got: {out:?}"
        );
    }

    #[test]
    fn impact_max_depth_clamped_to_5() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = indexed_state(root);

        // Depth 2 is the deepest real chain here regardless of the
        // requested max_depth, but an absurd request must still clamp
        // rather than pass straight through to impact_of.
        let out = impact_text(&state, "chain.leaf", Some(99));
        assert!(out.contains("depth 2:"), "got: {out:?}");
        assert!(!out.contains("depth 3:"), "got: {out:?}");
    }

    #[test]
    fn impact_row_cap_note_when_500_hit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut content = String::from("def target():\n    pass\n\n\n");
        for i in 0..510 {
            content.push_str(&format!("def caller{i}():\n    target()\n\n\n"));
        }
        write(root, "huge.py", &content);
        let state = indexed_state(root);

        let out = impact_text(&state, "huge.target", Some(1));
        assert!(
            out.contains("(row cap reached — results truncated)"),
            "got: {out:?}"
        );
    }

    #[test]
    fn impact_candidates_and_notfound_text_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def dup():\n    pass\n");
        write(root, "b.py", "def dup():\n    pass\n");
        let state = indexed_state(root);

        let ambiguous = impact_text(&state, "dup", None);
        assert!(ambiguous.contains("a.dup"), "got: {ambiguous:?}");
        assert!(
            ambiguous.to_lowercase().contains("qualname"),
            "got: {ambiguous:?}"
        );

        let notfound = impact_text(&state, "totally_unknown_xyz", None);
        assert!(notfound.contains("no symbol found"), "got: {notfound:?}");
    }
}
