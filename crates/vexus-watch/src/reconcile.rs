//! Full-repo reconciliation pass: brings the index in line with disk state
//! for every file the repo considers "in scope", without doing a
//! from-scratch rebuild the way `pipeline::index_repo` does.
//!
//! Used both at watcher startup (catch up on whatever changed on disk while
//! nothing was watching — the process wasn't running, or the DB predates
//! this feature) and whenever the watcher itself flags
//! `meta('needs_reconcile')` (e.g. after a `notify` error it couldn't
//! recover from incrementally — see `watcher::mark_degraded`).
//!
//! Unlike `index_repo`'s single `ignore` walk, reconcile prefers `git
//! ls-files -z` (when `root/.git` exists) to enumerate tracked files — no
//! filesystem walk needed, and it can't drift from what the repo's own
//! `.gitignore` considers in scope. A missing `.git`, or `git` itself
//! failing for any reason (not on `PATH`, a corrupt or non-repository
//! `.git` entry, ...), falls back to the same `ignore`-crate walk
//! `index_repo` uses, so reconcile always makes forward progress even
//! outside a git checkout.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use vexus_core::Store;
use vexus_embed::Embedder;

use crate::freshness::{set_freshness, Freshness};
use crate::pipeline::walk_repo_relative_files;
use crate::update::{update_file, UpdateOutcome};

/// How many files between each `meta('reconcile_progress')` write — cheap
/// enough that reconciling a huge repo doesn't pay for a meta write per
/// file, but frequent enough that a reader polling `status` mid-reconcile
/// sees real movement.
const PROGRESS_EVERY: usize = 25;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Files that were (re)indexed: changed content, or newly appeared on
    /// disk since the index last saw them.
    pub updated: usize,
    /// Files that were in the index but are no longer on disk (deleted,
    /// renamed away, or newly gitignored since they were last indexed).
    pub removed: usize,
    /// Files inspected whose content hash matched what was already
    /// stored — nothing to do.
    pub unchanged: usize,
}

/// Repo-relative paths (forward-slash-normalized) `git` considers tracked
/// in `root`, via `git ls-files -z` (NUL-separated, so a filename
/// containing a newline can't corrupt the split the way plain `ls-files`
/// output could). `None` on any subprocess failure — `git` missing from
/// `PATH`, `root` not actually a valid repository despite having a `.git`
/// entry, a non-zero exit, anything — so the caller can fall back to the
/// `ignore` walk uniformly rather than needing to distinguish failure
/// modes.
fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .arg("-z")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).replace('\\', "/"))
            .collect(),
    )
}

/// The file list reconcile treats as "in scope" for `root`: `git ls-files`
/// when `root/.git` exists and the subprocess succeeds, else the same
/// `ignore`-crate walk `index_repo` uses.
fn list_in_scope_files(root: &Path) -> Vec<String> {
    if root.join(".git").exists() {
        if let Some(files) = git_ls_files(root) {
            return files;
        }
    }
    walk_repo_relative_files(root)
}

/// Bring `store` in line with `root`'s current disk state. Every file
/// `list_in_scope_files` reports, plus every DB path it *doesn't* report
/// (deleted, renamed away, or gitignored since it was last indexed), goes
/// through `update_file` — which already knows how to detect "gone from
/// disk" (`FileClass::Missing`) and remove stale rows accordingly, so no
/// separate removal pass is needed here the way `index_repo` has one.
///
/// Freshness is `Reconciling` for the duration, `Fresh` on a clean finish,
/// `Degraded` if this returns `Err` — a hard failure (a store I/O error, an
/// embedder erroring out, ...). An individual file's own read/parse
/// failure does *not* itself count as one: per `update_file`'s contract
/// those keep the file's prior rows exactly as they were and only bump
/// `meta('last_index_failed')`, matching `index_repo`'s
/// degrade-gracefully-on-one-bad-file philosophy rather than aborting the
/// whole pass over it.
pub fn reconcile(
    store: &mut Store,
    embedder: Option<&dyn Embedder>,
    root: &Path,
) -> Result<ReconcileReport> {
    set_freshness(store, Freshness::Reconciling)?;
    let result = reconcile_inner(store, embedder, root);
    finalize(store, result.is_ok());
    result
}

/// The one place both of `reconcile`'s exit paths funnel through, so a
/// failure in *this* step itself can never leave the index stuck at
/// `Reconciling` forever (the bug this fixes: the old success arm did
/// `set_freshness(store, Fresh)?`, so if that single write failed — a
/// transient `SQLITE_BUSY`, say — `reconcile` returned `Err` with the state
/// still `Reconciling`. `drain_and_apply`'s healing gate refuses to touch
/// `Reconciling`, and `effective_freshness` never ages a non-`Degraded`
/// state into `Stale` either, so nothing would ever move it off
/// `Reconciling` again short of another reconcile pass succeeding).
///
/// Every write here is best-effort: clears `reconcile_progress`
/// unconditionally (this also fixes the `reconcile_inner`-internal `?` that
/// used to skip that clear on any mid-loop error exit), then tries to
/// persist the *intended* terminal state (`Fresh` on success, `Degraded` on
/// failure) — and if that specific write itself errors, falls back to
/// trying `Degraded` too (a no-op if `Degraded` was already the intended
/// state). `Degraded` is the deliberate fallback target rather than leaving
/// the state untouched: it's a state `drain_and_apply` CAN heal back to
/// `Fresh` on the next successful drain, and one `effective_freshness`
/// escalates to `Stale` after five minutes — either beats a silent,
/// permanent `Reconciling`. If even the `Degraded` write fails, there's
/// nothing more productive left to do than let the caller's `Result` speak
/// for itself.
fn finalize(store: &mut Store, succeeded: bool) {
    let _ = store.delete_meta("reconcile_progress");
    let intended = if succeeded {
        Freshness::Fresh
    } else {
        Freshness::Degraded
    };
    if set_freshness(store, intended).is_err() && intended != Freshness::Degraded {
        let _ = set_freshness(store, Freshness::Degraded);
    }
}

fn reconcile_inner(
    store: &mut Store,
    embedder: Option<&dyn Embedder>,
    root: &Path,
) -> Result<ReconcileReport> {
    let mut all_paths = list_in_scope_files(root);

    // DB paths the listing doesn't report are gone from the "in scope" set
    // (deleted, renamed away, or newly gitignored) — update_file's Missing
    // branch removes them exactly the way a deleted file does.
    let listed_set: HashSet<&str> = all_paths.iter().map(String::as_str).collect();
    let db_paths = store.file_paths()?;
    let extra_removed: Vec<String> = db_paths
        .into_iter()
        .filter(|p| !listed_set.contains(p.as_str()))
        .collect();
    drop(listed_set);
    all_paths.extend(extra_removed);

    let total = all_paths.len();
    let mut report = ReconcileReport::default();

    for (i, rel) in all_paths.iter().enumerate() {
        match update_file(store, embedder, root, rel)? {
            UpdateOutcome::Reindexed { .. } => report.updated += 1,
            UpdateOutcome::Removed => report.removed += 1,
            UpdateOutcome::SkippedUnchanged => report.unchanged += 1,
            // Not indexable (and either never was, or just became so) —
            // not a change reconcile needs to report either way.
            UpdateOutcome::SkippedUnsupported => {}
            // See this fn's doc comment: a single file's failure degrades
            // gracefully rather than aborting the whole pass.
            UpdateOutcome::Failed(_) => {}
        }

        let done = i + 1;
        if done.is_multiple_of(PROGRESS_EVERY) || done == total {
            store.set_meta("reconcile_progress", &format!("{done}/{total}"))?;
        }
    }

    // `reconcile_progress` is cleared by `finalize`, uniformly across every
    // exit path (including an `Err` from the `?`s above, which used to skip
    // a `delete_meta` call that lived here directly) — not here.
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::freshness::effective_freshness;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn reconcile_over_offline_edits_reports_modify_delete_add() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");
        write(root, "b.py", "def gone():\n    return 2\n");

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();
        assert_eq!(store.counts().unwrap().files, 2);

        // Offline edits: none of this goes through update_file or the
        // watcher, simulating changes made while nothing was watching.
        write(root, "a.py", "def helper():\n    return 42\n"); // modify
        std::fs::remove_file(root.join("b.py")).unwrap(); // delete
        write(root, "c.py", "def brand_new():\n    return 3\n"); // add

        let report = reconcile(&mut store, None, root).unwrap();
        assert_eq!(
            report,
            ReconcileReport {
                updated: 2,
                removed: 1,
                unchanged: 0,
            }
        );

        assert_eq!(
            store.file_hash("b.py").unwrap(),
            None,
            "deleted file's rows must be gone"
        );
        assert!(
            store.file_hash("c.py").unwrap().is_some(),
            "newly added file must now be indexed"
        );
        assert!(
            store.file_hash("a.py").unwrap().is_some(),
            "modified file must still be indexed"
        );

        assert_eq!(
            effective_freshness(&store).unwrap(),
            Freshness::Fresh,
            "a clean reconcile must leave the index Fresh"
        );
        assert_eq!(
            store.meta("reconcile_progress").unwrap(),
            None,
            "progress key must be cleared once reconcile finishes"
        );
    }

    #[test]
    fn reconcile_reports_unchanged_files_that_needed_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();

        // Nothing changed on disk at all.
        let report = reconcile(&mut store, None, root).unwrap();
        assert_eq!(
            report,
            ReconcileReport {
                updated: 0,
                removed: 0,
                unchanged: 1,
            }
        );
    }

    #[test]
    fn reconcile_falls_back_to_walk_when_no_git_dir_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");
        assert!(!root.join(".git").exists());

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        let report = reconcile(&mut store, None, root).unwrap();
        assert_eq!(
            report.updated, 1,
            "the ignore-walk fallback must still find a.py"
        );
        assert!(store.file_hash("a.py").unwrap().is_some());
    }

    #[test]
    fn reconcile_falls_back_to_walk_when_git_subprocess_fails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");
        // A `.git` entry that exists but isn't a real repository — `git -C
        // root ls-files -z` will fail against it, exercising the
        // subprocess-failure fallback path specifically (as opposed to the
        // no-`.git`-at-all path above).
        std::fs::write(root.join(".git"), b"not a real git repo").unwrap();

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        let report = reconcile(&mut store, None, root).unwrap();
        assert_eq!(
            report.updated, 1,
            "a broken .git must fall back to the ignore walk, not silently index nothing"
        );
    }

    #[test]
    fn reconcile_prefers_git_ls_files_when_a_real_repo_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "tracked.py", "def tracked():\n    return 1\n");
        write(root, "untracked.py", "def untracked():\n    return 2\n");

        let git = |args: &[&str]| Command::new("git").arg("-C").arg(root).args(args).output();
        let Ok(init) = git(&["init", "-q"]) else {
            eprintln!("git not available on PATH; skipping");
            return;
        };
        if !init.status.success() {
            eprintln!("git init failed; skipping");
            return;
        }
        for args in [
            vec![
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "add",
                "tracked.py",
            ],
            vec![
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        ] {
            let out = git(&args).unwrap();
            assert!(
                out.status.success(),
                "{:?}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        let report = reconcile(&mut store, None, root).unwrap();

        // Only the git-tracked file should have been indexed — proves the
        // git-ls-files path (not a plain directory walk, which would have
        // also picked up untracked.py) actually ran.
        assert_eq!(report.updated, 1);
        assert!(store.file_hash("tracked.py").unwrap().is_some());
        assert_eq!(
            store.file_hash("untracked.py").unwrap(),
            None,
            "untracked.py must not be indexed when a git repo is present"
        );
    }

    struct FailingEmbedder;
    impl Embedder for FailingEmbedder {
        fn id(&self) -> &str {
            "failing"
        }
        fn dim(&self) -> usize {
            4
        }
        fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            anyhow::bail!("embedder exploded")
        }
    }

    #[test]
    fn reconcile_marks_degraded_and_returns_err_on_a_hard_failure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");

        let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
        crate::pipeline::index_repo(root, &mut store).unwrap();
        store
            .set_model(FailingEmbedder.id(), FailingEmbedder.dim())
            .unwrap();
        // Force a real reindex (not SkippedUnchanged) so update_file's
        // embed_pending call actually runs and hits the failing embedder.
        write(root, "a.py", "def helper():\n    return 2\n");

        let err = reconcile(&mut store, Some(&FailingEmbedder), root).unwrap_err();
        assert!(
            err.to_string().contains("embedder exploded"),
            "got: {err:#}"
        );
        assert_eq!(
            effective_freshness(&store).unwrap(),
            Freshness::Degraded,
            "a hard failure mid-reconcile must leave the index Degraded, not silently Fresh"
        );
        assert_eq!(
            store.meta("reconcile_progress").unwrap(),
            None,
            "reconcile_progress must still be cleared on an error exit (finding 2)"
        );
    }

    /// Finding 1 (review): `finalize` is the single funnel both of
    /// `reconcile`'s exit paths go through, specifically so a failure
    /// clearing `reconcile_progress` or writing the intended terminal state
    /// can never leave the index stuck at `Reconciling` — checked directly
    /// against `finalize` (rather than only through the full `reconcile`,
    /// where injecting a meta-write failure at exactly the right instant
    /// isn't cheaply reachable) for both the success and failure shapes.
    #[test]
    fn finalize_clears_progress_key_and_sets_the_intended_state_on_both_outcomes() {
        for (succeeded, expected) in [(true, Freshness::Fresh), (false, Freshness::Degraded)] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::open(&dir.path().join(".vexus/index.db")).unwrap();
            store.set_meta("reconcile_progress", "5/10").unwrap();
            set_freshness(&mut store, Freshness::Reconciling).unwrap();

            finalize(&mut store, succeeded);

            assert_eq!(
                store.meta("reconcile_progress").unwrap(),
                None,
                "finalize must clear reconcile_progress regardless of outcome"
            );
            assert_eq!(
                effective_freshness(&store).unwrap(),
                expected,
                "succeeded={succeeded}"
            );
        }
    }

    /// Finding 1 (review), fallback branch: if the intended-state write
    /// itself fails, `finalize` must still attempt (rather than skip) a
    /// `Degraded` fallback write, and — the property this test actually
    /// pins down — never panic or otherwise diverge even in the worst case
    /// where *every* write it attempts fails (here, by handing it a
    /// read-only connection, so both the intended write and the `Degraded`
    /// fallback are guaranteed to error).
    ///
    /// This does not exercise the "first write fails, fallback succeeds"
    /// interior branch in isolation — doing that would need a `Store`
    /// whose `set_meta` can be made to fail exactly once (by call count or
    /// argument), which `vexus-core`'s concrete `Store` has no seam for
    /// without disproportionate mocking for this fix. The restructure
    /// itself (one `finalize` funnel, `Degraded` as the always-attempted
    /// fallback) is otherwise covered by code review and by the "both
    /// outcomes" test above exercising the non-failing write path for
    /// each branch.
    #[test]
    fn finalize_does_not_panic_when_every_write_it_attempts_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".vexus/index.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            set_freshness(&mut store, Freshness::Reconciling).unwrap();
        }

        let mut reader = Store::open_read_only(&db_path).unwrap();
        finalize(&mut reader, true);
        finalize(&mut reader, false);
    }
}
