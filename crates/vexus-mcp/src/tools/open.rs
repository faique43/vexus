//! `open` tool: fetch the verbatim source of a symbol (by qualname or bare
//! name) or an exact file `path:start-end` line range — replaces reading
//! whole files.

use std::path::{Path, PathBuf};

use vexus_core::model::estimate_tokens;
use vexus_core::query::SymbolInfo;
#[cfg(test)]
use vexus_watch::pipeline;

use crate::bundle::BundleItem;
use crate::format::render_bundle;
use crate::state::{freshness_header, AppState};
use crate::tools::{apply_header, clamp_budget, resolve_or_text};

const DEFAULT_BUDGET_TOKENS: u32 = 6000;

/// Pure inner implementation of the `open` tool.
pub fn open_text(state: &AppState, target: &str, budget_tokens: Option<u32>) -> String {
    let budget_tokens = clamp_budget(budget_tokens, DEFAULT_BUDGET_TOKENS);

    // Locked up front — even the path-slice branch below (which never
    // touches symbol/chunk tables) needs the freshness header, and this is
    // the one lock scope the whole function shares.
    let store = match state.lock_store_fresh() {
        Ok(s) => s,
        Err(msg) => return msg,
    };
    let fresh_header = freshness_header(&store);

    if let Some((rel_path, start, end)) = parse_path_slice(target) {
        drop(store);
        return apply_header(
            fresh_header,
            open_path_slice(&state.root, rel_path, start, end, budget_tokens),
        );
    }

    let info = match resolve_or_text(&store, target) {
        Ok(info) => info,
        Err(text) => return apply_header(fresh_header, text),
    };

    // A symbol's chunks start at its doc comment, which `info` doesn't.
    let start_line = match store.symbol_source(info.id) {
        Ok(chunks) => chunks
            .first()
            .map_or(info.start_line, |chunk| chunk.1.min(info.start_line)),
        Err(e) => return apply_header(fresh_header, format!("open error: {e:#}")),
    };
    let indexed_hash = store.file_hash(&info.path).ok().flatten();
    drop(store);
    apply_header(
        fresh_header,
        open_symbol(&state.root, &info, start_line, indexed_hash, budget_tokens),
    )
}

/// Chunks are for retrieval, and a container's is a skeleton, so a symbol's
/// source comes from the file. The index only knows where to look, which
/// goes wrong once the file has moved on, so that's noted.
fn open_symbol(
    root: &Path,
    info: &SymbolInfo,
    start_line: u32,
    indexed_hash: Option<[u8; 32]>,
    budget_tokens: u32,
) -> String {
    let bytes = match read_source(root, &info.path) {
        Ok(bytes) => bytes,
        Err(msg) => return msg,
    };
    let content = String::from_utf8_lossy(&bytes);
    let mut out = render_slice(
        &info.path,
        &content,
        start_line,
        info.end_line,
        budget_tokens,
    );
    if indexed_hash != Some(*blake3::hash(&bytes).as_bytes()) {
        out.push_str(&format!(
            "{} changed since it was indexed, so these lines may no longer be {}\n",
            info.path, info.qualname
        ));
    }
    out
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
    match read_source(root, rel_path) {
        Ok(bytes) => render_slice(
            rel_path,
            &String::from_utf8_lossy(&bytes),
            start,
            end,
            budget_tokens,
        ),
        Err(msg) => msg,
    }
}

fn read_source(root: &Path, rel_path: &str) -> Result<Vec<u8>, String> {
    if escapes_root(root, rel_path) {
        return Err(format!(
            "\"{rel_path}\" escapes the repository root — refusing to read it."
        ));
    }
    let candidate: PathBuf = root.join(rel_path);
    std::fs::read(&candidate).map_err(|_| {
        format!("file not found: {rel_path} — try `explore` to locate the right path.")
    })
}

fn render_slice(rel_path: &str, content: &str, start: u32, end: u32, budget_tokens: u32) -> String {
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
    let line_cut = estimate_tokens(requested[0]) > budget_tokens;
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
    if kept < requested_count {
        out.push_str(&format!(
            "range truncated to {kept} of {requested_count} lines to fit budget_tokens; \
             request a narrower range or raise budget_tokens\n"
        ));
    }
    if line_cut {
        out.push_str(&format!(
            "line {start} cut to fit budget_tokens; raise budget_tokens to see all of it\n"
        ));
    }
    out
}

/// Fits `lines` under `budget_tokens` by keeping the longest whole-line
/// prefix whose `estimate_tokens` cost is within budget (binary search over
/// the prefix length, so a huge over-budget range doesn't cost O(n^2) to
/// shrink one line at a time). A first line that alone exceeds budget is cut
/// mid-line, since there's no narrower full-line unit to fall back to.
/// Returns the joined (possibly truncated) content and how many lines it
/// touched; the caller compares that against `lines.len()` to decide whether
/// to note the cut.
fn truncate_to_budget(lines: &[&str], budget_tokens: u32) -> (String, usize) {
    let full = lines.join("\n");
    if estimate_tokens(&full) <= budget_tokens {
        return (full, lines.len());
    }

    let fits = |n: usize| estimate_tokens(&lines[0..n].join("\n")) <= budget_tokens;
    if !fits(1) {
        let head = lines[0].chars().take(budget_tokens as usize * 4);
        return (head.collect(), 1);
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

    /// `open` is described as exact source, so a container must come back as
    /// the file has it, not as the skeleton the index embeds.
    #[test]
    fn open_container_returns_the_file_not_the_skeleton() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = "export interface Options {\n  /** Largest payload, in bytes. */\n  limit: number;\n\n  name?: string;\n}\n";
        write(root, "opts.ts", source);

        let state = indexed_state(root);
        let out = open_text(&state, "Options", None);

        assert!(out.contains("opts.ts:1-6"), "got: {out:?}");
        assert!(out.contains(source), "got: {out:?}");
    }

    #[test]
    fn open_class_keeps_method_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "c.py",
            "class C:\n    def a(self):\n        return 'body of a'\n",
        );

        let state = indexed_state(root);
        let out = open_text(&state, "C", None);

        assert!(out.contains("return 'body of a'"), "got: {out:?}");
    }

    #[test]
    fn open_split_function_has_no_injected_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let body: String = (0..300).map(|i| format!("    line_{i:04} = 0\n")).collect();
        write(root, "big.py", &format!("def big():\n{body}"));

        let state = indexed_state(root);
        let out = open_text(&state, "big.big", Some(20_000));

        assert_eq!(out.matches("def big():").count(), 1, "got: {out:?}");
        assert!(out.contains("big.py:1-301"), "got: {out:?}");
    }

    #[test]
    fn open_symbol_includes_its_doc_comment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "# Explains foo.\ndef foo():\n    return 1\n");

        let state = indexed_state(root);
        let out = open_text(&state, "a.foo", None);

        assert!(out.contains("a.py:1-3"), "got: {out:?}");
        assert!(out.contains("# Explains foo."), "got: {out:?}");
    }

    #[test]
    fn open_notes_when_the_symbol_changed_on_disk_after_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def foo():\n    return 1\n");

        let state = indexed_state(root);
        write(root, "a.py", "# new first line\ndef foo():\n    return 2\n");
        let out = open_text(&state, "a.foo", None);

        assert!(out.contains("changed since it was indexed"), "got: {out:?}");
    }

    #[test]
    fn open_symbol_whose_file_is_gone_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def foo():\n    return 1\n");

        let state = indexed_state(root);
        std::fs::remove_file(root.join("a.py")).unwrap();
        let out = open_text(&state, "a.foo", None);

        assert!(out.contains("file not found"), "got: {out:?}");
    }

    #[test]
    fn open_cuts_a_lone_line_that_exceeds_the_budget_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "min.js", &format!("{}\n", "x".repeat(20_000)));

        let state = indexed_state(root);
        let out = open_text(&state, "min.js:1-1", Some(100));

        assert!(out.len() < 2_000, "got {} bytes", out.len());
        assert!(out.contains("cut to fit budget_tokens"), "got: {out:?}");
    }

    /// Silently showing a truncated body reads as the whole symbol.
    #[test]
    fn open_symbol_over_budget_says_how_much_it_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let body: String = (0..300).map(|i| format!("    line_{i:04} = 0\n")).collect();
        write(root, "big.py", &format!("def big():\n{body}"));

        let state = indexed_state(root);

        let out = open_text(&state, "big.big", Some(700));
        assert!(out.contains("range truncated to"), "got: {out:?}");
        assert!(out.contains("of 301 lines"), "got: {out:?}");
        assert!(out.contains("raise budget_tokens"), "got: {out:?}");
        assert!(!out.contains("line_0299"), "got: {out:?}");

        let out_full = open_text(&state, "big.big", Some(20_000));
        assert!(
            !out_full.contains("range truncated to"),
            "got: {out_full:?}"
        );
        assert!(out_full.contains("line_0299"), "got: {out_full:?}");
    }

    #[test]
    fn open_type_dot_method_picks_the_rust_impl_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "lib.rs",
            "struct Store;\nimpl Store {\n    fn open() -> u8 { 1 }\n}\nstruct Index;\nimpl Index {\n    fn open() -> u8 { 2 }\n}\n",
        );

        let state = indexed_state(root);
        let out = open_text(&state, "Store.open", None);

        assert!(out.contains("lib.rs:3-3"), "got: {out:?}");
        assert!(out.contains("{ 1 }"), "got: {out:?}");
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
    fn open_path_slice_missing_file_points_at_explore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "line1\n");

        let state = indexed_state(root);
        let out = open_text(&state, "does_not_exist.py:1-2", None);

        assert!(out.contains("file not found"), "got: {out:?}");
        assert!(out.contains("`explore`"), "got: {out:?}");
        assert!(!out.contains("`search`"), "got: {out:?}");
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
