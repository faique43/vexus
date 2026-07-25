//! `open` tool: fetch the verbatim source of a symbol (by qualname or bare
//! name) or an exact file `path:start-end` line range — replaces reading
//! whole files.

use std::path::{Path, PathBuf};

use vexus_core::model::estimate_tokens;
use vexus_core::query::Resolution;

use crate::bundle::{pack, BundleItem};
use crate::format::{render_bundle, render_candidates};
use crate::state::AppState;
use crate::tools::clamp_budget;

const DEFAULT_BUDGET_TOKENS: u32 = 6000;

/// Pure inner implementation of the `open` tool.
pub fn open_text(state: &AppState, target: &str, budget_tokens: Option<u32>) -> String {
    let budget_tokens = clamp_budget(budget_tokens, DEFAULT_BUDGET_TOKENS);

    if let Some((rel_path, start, end)) = parse_path_slice(target) {
        return open_path_slice(&state.root, rel_path, start, end, budget_tokens);
    }

    let store = state.store.lock().expect("store mutex poisoned");
    let resolution = match store.resolve_symbol(target) {
        Ok(r) => r,
        Err(e) => return format!("open error: {e:#}"),
    };

    match resolution {
        Resolution::Exact(info) => {
            let chunks = match store.symbol_source(info.id) {
                Ok(c) => c,
                Err(e) => return format!("open error: {e:#}"),
            };
            drop(store);
            if chunks.is_empty() {
                return format!(
                    "{} ({}:{}-{}) has no source chunks (likely a module or an empty body).",
                    info.qualname, info.path, info.start_line, info.end_line
                );
            }
            let items: Vec<BundleItem> = chunks
                .into_iter()
                .map(|(start_line, end_line, content)| BundleItem {
                    path: info.path.clone(),
                    qualname: Some(info.qualname.clone()),
                    start_line,
                    end_line,
                    content,
                    score: 1.0,
                    chunk_id: -1,
                })
                .collect();
            let (selected, omitted) = pack(items, budget_tokens);
            render_bundle(&selected, &omitted)
        }
        Resolution::Candidates(candidates) => {
            drop(store);
            let mut out = render_candidates(&candidates);
            out.push_str("\nAmbiguous — narrow with the full qualname.\n");
            out
        }
        Resolution::NotFound { suggestions } => {
            drop(store);
            if suggestions.is_empty() {
                format!("no symbol found for \"{target}\".")
            } else {
                format!(
                    "no symbol found for \"{target}\" — did you mean: {}?",
                    suggestions.join(", ")
                )
            }
        }
    }
}

/// Recognizes the `path:start-end` form (equivalent to the regex
/// `^(.+):(\d+)-(\d+)$`), gated on the path portion containing `/` or `.` so
/// a bare symbol name is never misread as a path. Splits on the *last*
/// colon, matching the greedy `(.+)` in the equivalent regex.
fn parse_path_slice(target: &str) -> Option<(&str, u32, u32)> {
    let (path, range) = target.rsplit_once(':')?;
    if path.is_empty() || !(path.contains('/') || path.contains('.')) {
        return None;
    }
    let (start_s, end_s) = range.split_once('-')?;
    if start_s.is_empty()
        || end_s.is_empty()
        || !start_s.bytes().all(|b| b.is_ascii_digit())
        || !end_s.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let start: u32 = start_s.parse().ok()?;
    let end: u32 = end_s.parse().ok()?;
    Some((path, start, end))
}

/// Rejects any relative path that would climb out of `root`: a lexical `..`
/// (or ancestor-escaping symlink, caught by the canonicalize + `starts_with`
/// check) never gets read, even if the path happens not to exist.
fn escapes_root(root: &Path, rel: &str) -> bool {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return true;
    }
    let candidate = root.join(rel_path);
    if let (Ok(canon_root), Ok(canon_candidate)) = (root.canonicalize(), candidate.canonicalize()) {
        if !canon_candidate.starts_with(&canon_root) {
            return true;
        }
    }
    false
}

fn open_path_slice(
    root: &Path,
    rel_path: &str,
    start: u32,
    end: u32,
    budget_tokens: u32,
) -> String {
    if escapes_root(root, rel_path) {
        return format!("\"{rel_path}\" escapes the repository root — refusing to read it.");
    }

    let candidate: PathBuf = root.join(rel_path);
    let content = match std::fs::read_to_string(&candidate) {
        Ok(c) => c,
        Err(_) => {
            return format!("file not found: {rel_path} — try `search` to locate the right path.")
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as u32;
    if total == 0 {
        return format!("{rel_path} is empty.");
    }
    if start < 1 || start > total {
        return format!(
            "line range {start}-{end} is out of bounds for {rel_path} ({total} lines)."
        );
    }
    let end = end.clamp(start, total);
    let requested = &lines[(start - 1) as usize..end as usize];
    let requested_count = requested.len();
    let (slice, kept) = truncate_to_budget(requested, budget_tokens);
    let truncated = kept < requested_count;
    let rendered_end = start + kept as u32 - 1;

    let item = BundleItem {
        path: rel_path.to_string(),
        qualname: None,
        start_line: start,
        end_line: rendered_end,
        content: slice,
        score: 1.0,
        chunk_id: -1,
    };
    let mut out = render_bundle(&[item], &[]);
    if truncated {
        out.push_str(&format!(
            "range truncated to {kept} of {requested_count} lines to fit budget_tokens; \
             request a narrower range or raise budget_tokens\n"
        ));
    }
    out
}

/// Fits `lines` under `budget_tokens` by keeping the longest whole-line
/// prefix whose `estimate_tokens` cost is within budget (binary search over
/// the prefix length, so a huge over-budget range doesn't cost O(n^2) to
/// shrink one line at a time). Always keeps at least one line — a
/// single line that alone exceeds budget is still returned as-is, since
/// there's no narrower full-line unit to fall back to. Returns the joined
/// (possibly truncated) content and how many lines it kept; the caller
/// compares that against `lines.len()` to decide whether to note the cut.
fn truncate_to_budget(lines: &[&str], budget_tokens: u32) -> (String, usize) {
    let full = lines.join("\n");
    if lines.len() <= 1 || estimate_tokens(&full) <= budget_tokens {
        return (full, lines.len());
    }

    let fits = |n: usize| estimate_tokens(&lines[0..n].join("\n")) <= budget_tokens;
    if !fits(1) {
        return (lines[0].to_string(), 1);
    }

    let mut lo = 1usize;
    let mut hi = lines.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    (lines[0..lo].join("\n"), lo)
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

    fn indexed_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        vexus_embed::pipeline::index_repo(root, &mut store).unwrap();
        AppState {
            store: Mutex::new(store),
            embedder: OnceLock::new(),
            root: root.to_path_buf(),
        }
    }

    #[test]
    fn open_exact_symbol_returns_source_block() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def foo():\n    return 1\n");

        let state = indexed_state(root);
        let out = open_text(&state, "a.foo", None);

        assert!(out.contains("a.py:1-2"), "got: {out:?}");
        assert!(out.contains("def foo():"), "got: {out:?}");
        assert!(
            out.contains("```"),
            "expected a fenced source block: {out:?}"
        );
    }

    #[test]
    fn open_ambiguous_name_returns_candidates_and_narrow_hint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def foo():\n    return 1\n");
        write(root, "b.py", "def foo():\n    return 2\n");

        let state = indexed_state(root);
        let out = open_text(&state, "foo", None);

        assert!(out.contains("a.foo"), "got: {out:?}");
        assert!(out.contains("b.foo"), "got: {out:?}");
        assert!(
            out.to_lowercase().contains("qualname"),
            "expected a hint to narrow with the full qualname: {out:?}"
        );
        // Candidates must not include full bodies.
        assert!(
            !out.contains("```"),
            "candidates must not render bodies: {out:?}"
        );
    }

    #[test]
    fn open_notfound_returns_suggestions_text() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def slugify():\n    return None\n");

        let state = indexed_state(root);
        let out = open_text(&state, "totally_unknown_symbol_xyz", None);

        assert!(out.contains("no symbol found"), "got: {out:?}");
    }

    #[test]
    fn open_path_slice_reads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "line1\nline2\nline3\nline4\n");

        let state = indexed_state(root);
        let out = open_text(&state, "a.py:2-3", None);

        assert!(out.contains("line2"), "got: {out:?}");
        assert!(out.contains("line3"), "got: {out:?}");
        assert!(!out.contains("line1"), "got: {out:?}");
        assert!(!out.contains("line4"), "got: {out:?}");
        assert!(out.contains("a.py:2-3"), "got: {out:?}");
    }

    #[test]
    fn open_path_slice_truncates_to_fit_a_small_budget_but_leaves_normal_ranges_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content: String = (1..=500).map(|n| format!("line{n}\n")).collect();
        write(root, "big.py", &content);

        let state = indexed_state(root);

        // A tiny budget can only fit a handful of "lineN" entries — the full
        // 1-500 range would blow well past it, so the tail must be cut and a
        // truncation note appended.
        let out = open_text(&state, "big.py:1-500", Some(20));
        assert!(out.contains("line1"), "got: {out:?}");
        assert!(
            !out.contains("line500"),
            "expected truncation well before line500: {out:?}"
        );
        assert!(
            out.contains("range truncated to") && out.contains("to fit budget_tokens"),
            "expected a truncation note: {out:?}"
        );

        // A normal, budget-friendly range is unaffected — no truncation note.
        let out2 = open_text(&state, "big.py:1-3", None);
        assert!(out2.contains("line1"), "got: {out2:?}");
        assert!(out2.contains("line3"), "got: {out2:?}");
        assert!(
            !out2.contains("range truncated to"),
            "a range comfortably within budget must not be truncated: {out2:?}"
        );
    }

    #[test]
    fn open_path_slice_missing_file_suggests_search() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "line1\n");

        let state = indexed_state(root);
        let out = open_text(&state, "does_not_exist.py:1-2", None);

        assert!(out.contains("file not found"), "got: {out:?}");
        assert!(out.contains("search"), "got: {out:?}");
    }

    #[test]
    fn open_path_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "top secret contents\n").unwrap();

        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "a.py", "line1\n");

        let state = indexed_state(&root);
        let out = open_text(&state, "../outside/secret.txt:1-1", None);

        assert!(out.contains("escapes the repository root"), "got: {out:?}");
        assert!(
            !out.contains("top secret"),
            "traversal must never leak file contents: {out:?}"
        );
    }
}
