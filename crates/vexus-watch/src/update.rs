//! Incremental single-file update: the watcher's per-event path.
//!
//! Unlike `pipeline::index_repo` (full walk, then one `resolve_all_edges`
//! sweep over the *entire* graph), `update_file` handles exactly one
//! repo-relative path and — per spec §2 decision 2 — re-resolves only the
//! symbol names that path's own change touched, via
//! `Store::resolve_edges_for_names`. A global re-resolve on every watcher
//! event would make the watcher's cost scale with total repo size instead of
//! with the size of the change.

use std::path::Path;

use anyhow::Result;
use vexus_core::Store;
use vexus_embed::Embedder;

use crate::pipeline::{classify_file, embed_pending, FileClass};

/// What happened to `rel` when `update_file` looked at it.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateOutcome {
    /// Parsed and stored; `names_resolved` is however many edges (anywhere
    /// in the graph, not just this file's own) `resolve_edges_for_names`
    /// touched while re-resolving this file's changed names.
    Reindexed { names_resolved: usize },
    /// Gone from disk (or no longer indexable — see `SkippedUnsupported`'s
    /// doc) and its rows, if any, were removed.
    Removed,
    /// Present and indexable, but its content hash matches what's already
    /// stored — nothing to do.
    SkippedUnchanged,
    /// Present but not indexable (unknown extension, over the size cap, or
    /// binary-sniffed). If it was previously indexed, its rows are removed
    /// here (the P1 regression ruling: a file that becomes unsupported —
    /// e.g. edited past the size cap — must not leave stale rows behind
    /// forever with nothing left to ever clean them up).
    SkippedUnsupported,
    /// Present and indexable but failed — a read error or a parser panic.
    /// Per spec §6, existing rows for this path are left exactly as they
    /// were (better a stale-but-present entry than none), and
    /// `meta('last_index_failed')` is incremented so `status` can surface
    /// that something needs attention.
    Failed(String),
}

/// Read current `meta('last_index_failed')` (absent/unparseable → 0),
/// increment, and persist. `index_repo`'s full runs instead *set* this key
/// wholesale to that run's total failure count at the end (see
/// `pipeline::index_repo`) — the two writers never race in practice (one
/// process runs either a full index or the watcher's incremental path at a
/// given moment), so incrementing here is safe and cheap relative to a full
/// recount.
fn increment_last_index_failed(store: &mut Store) -> Result<()> {
    let current: u64 = store
        .meta("last_index_failed")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    store.set_meta("last_index_failed", &(current + 1).to_string())
}

/// Remove `rel`'s existing rows (if any) and re-resolve whatever it used to
/// define, using the names captured *before* removal (`remove_file` itself
/// doesn't report them — they'd otherwise be unrecoverable once its rows are
/// gone).
fn remove_and_reresolve(store: &mut Store, rel: &str) -> Result<()> {
    let old_names = store.symbol_names_for_file(rel)?;
    store.remove_file(rel)?;
    store.resolve_edges_for_names(&old_names)?;
    Ok(())
}

/// Bring the index's row(s) for `rel` in line with disk. `embedder`, when
/// `Some`, is used to embed any chunks left missing a vector after a
/// `Reindexed` outcome (via `pipeline::embed_pending`, which only ever
/// processes rows still missing an embedding — cheap even when called after
/// every single-file update).
pub fn update_file(
    store: &mut Store,
    embedder: Option<&dyn Embedder>,
    root: &Path,
    rel: &str,
) -> Result<UpdateOutcome> {
    match classify_file(root, rel) {
        FileClass::Missing => {
            remove_and_reresolve(store, rel)?;
            Ok(UpdateOutcome::Removed)
        }
        FileClass::Unsupported => {
            // Only pay for the removal + re-resolve when it was actually
            // indexed before; a `.md` file that was never indexed touching
            // this path is a no-op.
            if store.file_hash(rel)?.is_some() {
                remove_and_reresolve(store, rel)?;
            }
            Ok(UpdateOutcome::SkippedUnsupported)
        }
        FileClass::ReadError(e) => {
            increment_last_index_failed(store)?;
            Ok(UpdateOutcome::Failed(format!("{rel}: {e}")))
        }
        FileClass::Supported { lang, bytes } => {
            let hash: [u8; 32] = *blake3::hash(&bytes).as_bytes();
            if store.file_hash(rel)? == Some(hash) {
                return Ok(UpdateOutcome::SkippedUnchanged);
            }

            let source = String::from_utf8_lossy(&bytes).into_owned();
            let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                vexus_index::parse::parse_file(lang, rel, &source)
            }));
            let idx = match parsed {
                Ok(idx) => idx,
                Err(_) => {
                    // Spec §6 ruling: a parse failure keeps the old rows
                    // exactly as they were — never delete or replace them —
                    // only the failure count moves.
                    increment_last_index_failed(store)?;
                    return Ok(UpdateOutcome::Failed(format!("{rel}: parser panic")));
                }
            };

            let (_file_id, touched) = store.replace_file(rel, lang.name, &hash, &idx)?;
            let names_resolved = store.resolve_edges_for_names(&touched)? as usize;
            if let Some(embedder) = embedder {
                embed_pending(store, embedder)?;
            }
            store.bump_generation()?;
            Ok(UpdateOutcome::Reindexed { names_resolved })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vexus_core::query::Resolution;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Confidence of the edge from `caller`'s (unique bare-name) symbol to
    /// `dst_name` — `None` means unresolved (`dst_id IS NULL`); panics if no
    /// such edge exists at all (a test-fixture bug, not a case under test).
    fn confidence_of(store: &Store, caller: &str, dst_name: &str) -> Option<String> {
        let Resolution::Exact(info) = store.resolve_symbol(caller).unwrap() else {
            panic!("expected a unique exact resolution for {caller}");
        };
        store
            .callees_of(info.id, 1, 100)
            .unwrap()
            .into_iter()
            .find(|h| h.via_name == dst_name)
            .unwrap_or_else(|| panic!("no edge {caller} -> {dst_name} in the graph"))
            .confidence
    }

    #[test]
    fn rename_reresolves_touched_names_only_no_global_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");
        write(root, "b.py", "def use_helper():\n    return helper()\n");
        write(
            root,
            "c.py",
            "def call_other():\n    return other_thing()\n",
        );

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();

        // b's call resolved against a.py's helper; c's call has no candidate
        // yet, so it's unresolved even right after this full index.
        assert!(
            confidence_of(&store, "use_helper", "helper").is_some(),
            "b.py's call to helper should resolve against a.py's definition"
        );
        assert!(
            confidence_of(&store, "call_other", "other_thing").is_none(),
            "c.py's call to a not-yet-defined name must be unresolved"
        );

        // Introduce a real candidate for "other_thing" via `replace_file`
        // directly (bypassing index_repo/update_file, both of which would
        // trigger a resolve pass) so nothing has re-resolved c.py's edge yet
        // — this is what lets the assertion below actually prove that the
        // later `update_file(a.py)` call doesn't do a global sweep.
        let f_source = "def other_thing():\n    return 2\n";
        write(root, "f.py", f_source);
        let lang = vexus_index::lang::for_path(Path::new("f.py")).unwrap();
        let f_idx = vexus_index::parse::parse_file(lang, "f.py", f_source);
        store
            .replace_file("f.py", lang.name, &[9u8; 32], &f_idx)
            .unwrap();
        assert!(
            confidence_of(&store, "call_other", "other_thing").is_none(),
            "inserting f.py via replace_file alone must not resolve anything"
        );

        // Rename helper -> assist in a.py and run the incremental update.
        write(root, "a.py", "def assist():\n    return 1\n");
        let outcome = update_file(&mut store, None, root, "a.py").unwrap();
        assert!(
            matches!(outcome, UpdateOutcome::Reindexed { .. }),
            "{outcome:?}"
        );

        // b's edge to the now-gone `helper` must go unresolved...
        assert!(
            confidence_of(&store, "use_helper", "helper").is_none(),
            "renamed-away helper must go unresolved"
        );
        // ...but c's edge, even though a valid `other_thing` candidate now
        // exists in the graph, must STILL be unresolved: update_file(a.py)
        // only re-resolves a.py's own touched names (helper, assist), never
        // a global resolve_all_edges sweep.
        assert!(
            confidence_of(&store, "call_other", "other_thing").is_none(),
            "an unrelated file's edge must not be touched by a.py's targeted resolve"
        );
    }

    #[test]
    fn delete_removes_file_and_breaks_dependent_edge() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");
        write(root, "b.py", "def use_helper():\n    return helper()\n");

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();
        assert!(confidence_of(&store, "use_helper", "helper").is_some());

        std::fs::remove_file(root.join("a.py")).unwrap();
        let outcome = update_file(&mut store, None, root, "a.py").unwrap();
        assert_eq!(outcome, UpdateOutcome::Removed);

        assert_eq!(
            store.file_hash("a.py").unwrap(),
            None,
            "removed file's row must be gone"
        );
        assert!(
            confidence_of(&store, "use_helper", "helper").is_none(),
            "b's edge must go unresolved once a.py (and its helper) is gone"
        );
    }

    #[test]
    fn touch_without_content_change_is_skipped_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = "def helper():\n    return 1\n";
        write(root, "a.py", content);

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();

        // Rewrite with identical content ("touch" — mtime changes, hash doesn't).
        write(root, "a.py", content);
        let outcome = update_file(&mut store, None, root, "a.py").unwrap();
        assert_eq!(outcome, UpdateOutcome::SkippedUnchanged);
    }

    #[test]
    fn oversized_overwrite_of_indexed_file_removes_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "big.py", "def helper():\n    return 1\n");

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();
        assert!(store.file_hash("big.py").unwrap().is_some());

        write(root, "big.py", &"x = 1\n".repeat(200_000)); // > 1MB
        let outcome = update_file(&mut store, None, root, "big.py").unwrap();
        assert_eq!(outcome, UpdateOutcome::SkippedUnsupported);
        assert_eq!(
            store.file_hash("big.py").unwrap(),
            None,
            "an oversized overwrite of a previously-indexed file must remove its stale rows \
             (P1 regression: otherwise nothing would ever clean them up)"
        );
    }

    #[test]
    fn generation_bumps_only_on_reindexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");
        write(root, "unsupported.md", "# hi\n");

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();
        let gen0 = store.generation().unwrap();

        let outcome = update_file(&mut store, None, root, "a.py").unwrap();
        assert_eq!(outcome, UpdateOutcome::SkippedUnchanged);
        assert_eq!(
            store.generation().unwrap(),
            gen0,
            "SkippedUnchanged must not bump"
        );

        let outcome = update_file(&mut store, None, root, "unsupported.md").unwrap();
        assert_eq!(outcome, UpdateOutcome::SkippedUnsupported);
        assert_eq!(
            store.generation().unwrap(),
            gen0,
            "SkippedUnsupported must not bump"
        );

        std::fs::remove_file(root.join("a.py")).unwrap();
        let outcome = update_file(&mut store, None, root, "a.py").unwrap();
        assert_eq!(outcome, UpdateOutcome::Removed);
        assert_eq!(store.generation().unwrap(), gen0, "Removed must not bump");

        write(root, "a.py", "def helper():\n    return 2\n");
        let outcome = update_file(&mut store, None, root, "a.py").unwrap();
        assert!(
            matches!(outcome, UpdateOutcome::Reindexed { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            store.generation().unwrap(),
            gen0 + 1,
            "Reindexed must bump exactly once"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_failure_keeps_old_rows_and_increments_last_index_failed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();
        assert_eq!(
            store.meta("last_index_failed").unwrap().as_deref(),
            Some("0")
        );

        let path = root.join("a.py");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = update_file(&mut store, None, root, "a.py");
        // Restore permissions before asserting/unwrapping so tempdir cleanup never fails.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let outcome = result.unwrap();
        assert!(matches!(outcome, UpdateOutcome::Failed(_)), "{outcome:?}");
        assert!(
            store.file_hash("a.py").unwrap().is_some(),
            "a read failure must keep the old rows exactly as they were"
        );
        assert_eq!(
            store.meta("last_index_failed").unwrap().as_deref(),
            Some("1")
        );
    }
}
