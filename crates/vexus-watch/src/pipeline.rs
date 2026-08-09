//! File-walking indexer + embedding backlog drainer.
//!
//! Both live here (rather than split across crates) because they were
//! originally one file in vexus-cli and share test fixtures; `index_repo`
//! moved alongside `embed_pending` so `vexus-mcp`'s startup path (which needs
//! both, without depending on the `vexus-cli` binary crate — that dependency
//! would be circular, since `vexus-cli` depends on `vexus-mcp` for the
//! `serve` subcommand) can call a single shared implementation instead of a
//! duplicated copy.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};
use vexus_core::Store;

use vexus_embed::Embedder;

#[derive(Debug, Default)]
pub struct IndexReport {
    pub indexed: usize,
    pub skipped_unchanged: usize,
    pub skipped_unsupported: usize,
    pub removed: usize,
    pub failed: Vec<String>,
}

const MAX_FILE_BYTES: u64 = 1_048_576;

/// The result of inspecting a single repo-relative path on disk, shared by
/// `index_repo`'s full-walk loop and `update::update_file`'s single-file
/// path so the hash/skip/binary-sniff rules can never drift between the two
/// (both used to reimplement this per-file logic separately, which is
/// exactly the kind of duplication that grows a silent inconsistency).
pub(crate) enum FileClass {
    /// Indexable: known language, under the size cap, not binary-sniffed.
    /// `bytes` are the file's raw contents (read once, reused for hashing
    /// and parsing).
    Supported {
        lang: &'static vexus_index::lang::Lang,
        bytes: Vec<u8>,
    },
    /// Not present on disk at all (stat/read came back `NotFound`).
    Missing,
    /// Present but not indexable: unknown extension, over `MAX_FILE_BYTES`,
    /// or binary-sniffed (a NUL byte in the first 8KiB).
    Unsupported,
    /// Present but couldn't be read (permissions, race, etc.) — kept
    /// distinct from `Missing` so callers don't treat a transient failure
    /// as "the file is gone" and delete its existing index rows.
    ReadError(String),
}

/// Classify `root.join(rel)` per the rules above. Never panics; every
/// failure mode is a `FileClass` variant, not an `Err`, since "this file
/// can't be indexed right now" is an ordinary outcome for both callers, not
/// an exceptional one.
pub(crate) fn classify_file(root: &Path, rel: &str) -> FileClass {
    let path = root.join(rel);

    let Some(lang) = vexus_index::lang::for_path(&path) else {
        return FileClass::Unsupported;
    };
    // A stat failure (including "gone") degrades to size 0 here rather than
    // an early return — the subsequent `std::fs::read` is the authoritative
    // existence check (its `NotFound` maps to `Missing` below), so a racy or
    // permission-denied stat doesn't preempt that with a wrong verdict.
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if len > MAX_FILE_BYTES {
        return FileClass::Unsupported;
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileClass::Missing,
        Err(e) => return FileClass::ReadError(e.to_string()),
    };
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return FileClass::Unsupported;
    }
    FileClass::Supported { lang, bytes }
}

/// Walks `root` yielding repo-relative, forward-slash-normalized paths for
/// every regular file, skipping `.git` and `.vexus` entirely — the walker
/// rules `index_repo`'s full sweep uses, extracted so `reconcile`'s
/// non-git fallback (`vexus_watch::reconcile`) can reuse exactly the same
/// rules rather than risk the two ever drifting on which files are "in
/// scope" for the index.
///
/// `require_git(false)` — `ignore::WalkBuilder`
/// otherwise only honors `.gitignore`/`.git/info/exclude`/the global
/// excludesfile when `root` is actually inside a git repository (a `.git`
/// entry present), silently ignoring every `.gitignore` file otherwise. A
/// plain (non-git) directory with its own `.gitignore` — root-level or
/// nested — must still have it honored here, or a full `vexus index` run
/// would index files a git checkout of the same tree never would.
pub(crate) fn walk_repo_relative_files(root: &Path) -> Vec<String> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".git" && name != ".vexus"
        })
        .build();

    let mut out = Vec::new();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(rel);
    }
    // readdir order differs between filesystems, and it determines rowid
    // assignment, which `search_hybrid` uses as its RRF tie-break — so an
    // unsorted walk makes the committed eval baseline platform-dependent.
    out.sort();
    out
}

pub(crate) fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .arg("-z")
        .arg("--cached")
        .arg("--others")
        .arg("--exclude-standard")
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
pub(crate) fn list_in_scope_files(root: &Path) -> Vec<String> {
    if root.join(".git").exists() {
        if let Some(files) = git_ls_files(root) {
            return files;
        }
    }
    walk_repo_relative_files(root)
}

pub fn index_repo(root: &Path, store: &mut Store) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let mut seen: HashSet<String> = HashSet::new();

    for rel in list_in_scope_files(root) {
        match classify_file(root, &rel) {
            FileClass::Missing => {
                // Disappeared between the walk and the stat/read (race); the
                // removal pass below will clean up its DB rows, if any.
            }
            FileClass::Unsupported => {
                seen.insert(rel.clone());
                report.skipped_unsupported += 1;
            }
            FileClass::ReadError(e) => {
                // The file still exists on disk (transient read failure, e.g.
                // permissions) — mark it seen so the removal pass below doesn't
                // delete its existing DB rows out from under it.
                seen.insert(rel.clone());
                report.failed.push(format!("{rel}: {e}"));
            }
            FileClass::Supported { lang, bytes } => {
                seen.insert(rel.clone());
                let hash: [u8; 32] = *blake3::hash(&bytes).as_bytes();
                if store.file_hash(&rel)? == Some(hash) {
                    report.skipped_unchanged += 1;
                    continue;
                }

                let source = String::from_utf8_lossy(&bytes).into_owned();
                let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    vexus_index::parse::parse_file(lang, &rel, &source)
                }));
                match parsed {
                    Ok(idx) => {
                        store.replace_file(&rel, lang.name, &hash, &idx)?;
                        report.indexed += 1;
                    }
                    Err(_) => report.failed.push(format!("{rel}: parser panic")),
                }
            }
        }
    }

    // Remove DB entries for files gone from disk.
    let db_paths = store.file_paths()?;
    for p in db_paths {
        if !seen.contains(&p) {
            store.remove_file(&p)?;
            report.removed += 1;
        }
    }

    store.resolve_all_edges()?;

    // Persisted so a later `status` call (possibly in a different process,
    // e.g. the MCP server) can report "skipped files" from the *last* run
    // without re-walking the repo.
    store.set_meta("last_index_failed", &report.failed.len().to_string())?;
    // Finding I6: a reader's `lock_store_fresh` only re-probes cached
    // derived state (e.g. "does vec_chunks exist") when `generation` moves —
    // a full `index_repo` run that actually changed anything must bump it,
    // or a reader that cached "empty index" before this run would keep
    // reporting that stale answer indefinitely.
    if report.indexed > 0 || report.removed > 0 {
        store.bump_generation()?;
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct EmbedReport {
    pub embedded: usize,
}

/// Backlogs at or below this stay silent. The watcher calls `embed_pending`
/// on every save (a handful of chunks); only a first index / big reindex —
/// where minutes of silence read as a hang — is worth narrating.
const EMBED_PROGRESS_MIN_BACKLOG: i64 = 256;

/// Embed every chunk missing a vector, in batches. A no-op (vec
/// unavailable, or nothing missing) returns an empty report immediately.
///
/// Large backlogs report progress to stderr: the first index of a real repo
/// embeds for minutes on CPU, and with no output that is indistinguishable
/// from a hang (users ^C it — the exact first-run failure this line exists
/// to prevent).
pub fn embed_pending(store: &mut Store, embedder: &dyn Embedder) -> Result<EmbedReport> {
    let mut report = EmbedReport { embedded: 0 };
    let total = store.embed_backlog()?;
    // Big backlogs always narrate. A first-ever embed pass (nothing embedded
    // yet) narrates regardless of size: on a small repo the cold run's only
    // slow step is the one-time model download, and a silent first index
    // right after a silent download reads as a hang — the exact failure the
    // progress lines exist to prevent. The watcher's steady-state calls
    // (small backlog, embeddings already present) stay silent.
    let announce =
        total > EMBED_PROGRESS_MIN_BACKLOG || (total > 0 && store.embedded_count()? == 0);
    if announce {
        if total > EMBED_PROGRESS_MIN_BACKLOG {
            eprintln!(
                "vexus: embedding {total} chunks — this can take a few minutes on a large repo \
                 (safe to interrupt; it resumes where it left off)"
            );
        } else {
            eprintln!("vexus: embedding {total} chunks …");
        }
    }
    loop {
        let backlog_before = store.embed_backlog()?;
        let missing = store.chunks_missing_embedding(256)?;
        if missing.is_empty() {
            break;
        }
        let mut ready: Vec<(i64, Vec<f32>)> = Vec::new();
        let to_embed: Vec<(i64, String, Vec<u8>)> = missing;
        for batch in to_embed.chunks(32) {
            let texts: Vec<&str> = batch.iter().map(|(_, c, _)| c.as_str()).collect();
            let vecs = embedder.embed(&texts)?;
            // `Embedder::embed` promises one vector per input text; a
            // misbehaving implementation returning fewer would otherwise have
            // `zip` silently drop the untranslated tail of the batch instead
            // of failing loudly.
            if vecs.len() != batch.len() {
                bail!(
                    "embedder {} batch length mismatch: returned {} vectors for {} texts",
                    embedder.id(),
                    vecs.len(),
                    batch.len()
                );
            }
            for ((id, _, _hash), v) in batch.iter().zip(vecs) {
                ready.push((*id, v));
                report.embedded += 1;
            }
        }
        store.put_embeddings(&ready)?;
        if announce {
            eprintln!("vexus: embedded {}/{total} chunks", report.embedded);
        }

        // Defensive: we just processed a non-empty batch of missing rows, so
        // the total backlog must have shrunk. If it somehow didn't (e.g.
        // put_embeddings silently no-oped), looping again would spin forever
        // re-fetching the same rows.
        let backlog_after = store.embed_backlog()?;
        if backlog_after >= backlog_before {
            bail!(
                "embed_pending made no progress: backlog was {backlog_before}, \
                 now {backlog_after}, after processing a batch"
            );
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn embed_pending_embeds_every_chunk_missing_a_vector() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f1():\n    pass\n");
        write(root, "b.py", "def f2():\n    pass\n");
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        index_repo(root, &mut store).unwrap();

        let embedder = vexus_embed::MockEmbedder;
        store.set_model(embedder.id(), embedder.dim()).unwrap();
        let r = embed_pending(&mut store, &embedder).unwrap();
        assert!(r.embedded >= 2);
        assert_eq!(store.embed_backlog().unwrap(), 0);

        // Editing a file re-creates its chunks; whatever is missing a vector
        // afterwards gets embedded, and the backlog returns to zero.
        write(root, "a.py", "def f1():\n    pass\n# comment\n");
        index_repo(root, &mut store).unwrap();
        embed_pending(&mut store, &embedder).unwrap();
        assert_eq!(store.embed_backlog().unwrap(), 0);
    }

    #[test]
    fn index_hash_gate_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/app.py", "def run():\n    return 1\n");
        write(root, "src/util.py", "def helper():\n    return 2\n");
        write(root, "README.md", "# not code\n");
        write(root, "big.py", &"x = 1\n".repeat(200_000)); // >1MB

        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();

        let r = index_repo(root, &mut store).unwrap();
        assert_eq!(r.indexed, 2);
        assert_eq!(r.skipped_unsupported, 2); // README (no language) + big.py (>1MB)
        assert_eq!(store.counts().unwrap().files, 2);

        // second run: nothing changed
        let r = index_repo(root, &mut store).unwrap();
        assert_eq!((r.indexed, r.skipped_unchanged), (0, 2));

        // edit one, delete one
        write(root, "src/app.py", "def run():\n    return 42\n");
        std::fs::remove_file(root.join("src/util.py")).unwrap();
        let r = index_repo(root, &mut store).unwrap();
        assert_eq!((r.indexed, r.removed), (1, 1));
        assert_eq!(store.counts().unwrap().files, 1);
    }

    /// Finding I6: a full `index_repo` run that actually changes anything
    /// (indexes or removes at least one file) must bump `generation`, so a
    /// reader's `lock_store_fresh` notices and re-probes derived state
    /// rather than trusting whatever it cached before this run — a bare
    /// `vexus index` invocation never calls `bump_generation` on its own,
    /// so without this the only such signal would come from the
    /// (unrelated) embedding path.
    #[test]
    fn index_repo_bumps_generation_only_when_something_actually_changed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def helper():\n    return 1\n");
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        assert_eq!(store.generation().unwrap(), 0);

        let r = index_repo(root, &mut store).unwrap();
        assert_eq!(r.indexed, 1);
        assert_eq!(
            store.generation().unwrap(),
            1,
            "indexing a new file is a real change and must bump"
        );

        // Nothing changed on disk: a no-op run must not bump again.
        let r = index_repo(root, &mut store).unwrap();
        assert_eq!(r.indexed, 0);
        assert_eq!(r.removed, 0);
        assert_eq!(
            store.generation().unwrap(),
            1,
            "a run with nothing indexed or removed must not bump"
        );

        std::fs::remove_file(root.join("a.py")).unwrap();
        let r = index_repo(root, &mut store).unwrap();
        assert_eq!(r.removed, 1);
        assert_eq!(
            store.generation().unwrap(),
            2,
            "a run that removes a stale file is a real change and must bump"
        );
    }

    /// The regression class that kept recurring here: growing the *same*
    /// final index two structurally different
    /// ways must converge on an identical final structural state
    /// (files/symbols/edges/chunks counts), regardless of which path built
    /// it.
    ///
    /// The "watcher-grown" side drives `update_file` directly, once per
    /// in-scope file, instead of emitting real filesystem events through a
    /// live `notify` watch — deliberately: the property this test pins down
    /// is scope/state *parity* between the two index-building paths, not
    /// event *delivery* (already covered elsewhere, e.g.
    /// `watcher::tests::watcher_indexes_a_new_file_and_marks_the_index_fresh`).
    /// Driving `update_file` directly is deterministic and immune to
    /// filesystem-event timing/flakiness, and — since `update_file`'s own
    /// per-file logic is exactly what a real watcher event invokes once
    /// debounced — proves the same property without needing a real OS-level
    /// watch at all.
    ///
    /// The fixture is a plain, non-git directory with a nested `.gitignore`
    /// (`sub/.gitignore` ignoring `*.gen.py`) in addition to a root one
    /// (`build/`) — exercising `require_git(false)` (item 1's fix to
    /// `walk_repo_relative_files`, which both sides below share to decide
    /// "what's in scope"). The ignored file `sub/gadget.gen.py` defines
    /// `gadget`, the very symbol `sub/c.py` calls: if the nested `.gitignore`
    /// were ever silently dropped, that file would leak into scope and the
    /// edge would resolve — a visible symbols/edges mismatch, not a silent
    /// no-op, which is what makes this fixture actually prove the exclusion
    /// rather than passing vacuously.
    #[test]
    fn index_repo_and_per_file_update_file_converge_on_the_same_final_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write(root, "a.py", "def helper():\n    return 1\n");
        write(root, "sub/b.py", "def use_helper():\n    return helper()\n");
        write(root, "sub/c.py", "def use_gadget():\n    return gadget()\n");

        write(root, ".gitignore", "build/\n");
        write(
            root,
            "build/ignored_by_root.py",
            "def dead():\n    return 0\n",
        );

        write(root, "sub/.gitignore", "*.gen.py\n");
        write(root, "sub/gadget.gen.py", "def gadget():\n    return 2\n");

        assert!(
            !root.join(".git").exists(),
            "fixture must be a plain, non-git directory — this pins down the \
             require_git(false) non-git parity fix, not the git check-ignore path"
        );

        // Side A: one atomic full walk.
        let mut store_a = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        let report = index_repo(root, &mut store_a).unwrap();
        assert_eq!(report.indexed, 3, "must index exactly the 3 in-scope files");

        // Side B: start empty, then drive update_file once per in-scope
        // file — reusing the very same scope-enumeration
        // (`walk_repo_relative_files`) `index_repo` uses internally, so both
        // sides agree on *what's* in scope. What this test actually proves
        // is that applying updates one file at a time (as a real watcher
        // eventually would, one per debounced path) converges on the same
        // graph a single full-walk pass does.
        let store_b_dir = tempfile::tempdir().unwrap();
        let mut store_b = vexus_core::Store::open(&store_b_dir.path().join("index.db")).unwrap();
        for rel in walk_repo_relative_files(root) {
            crate::update::update_file(&mut store_b, None, root, &rel).unwrap();
        }

        let a = store_a.counts().unwrap();
        let b = store_b.counts().unwrap();
        assert_eq!(a.files, b.files, "files count must match: {a:?} vs {b:?}");
        assert_eq!(
            a.symbols, b.symbols,
            "symbols count must match: {a:?} vs {b:?}"
        );
        assert_eq!(a.edges, b.edges, "edges count must match: {a:?} vs {b:?}");
        assert_eq!(
            a.chunks, b.chunks,
            "chunks count must match: {a:?} vs {b:?}"
        );

        for ignored in ["build/ignored_by_root.py", "sub/gadget.gen.py"] {
            assert_eq!(
                store_a.file_hash(ignored).unwrap(),
                None,
                "{ignored} must be excluded from the full-index side"
            );
            assert_eq!(
                store_b.file_hash(ignored).unwrap(),
                None,
                "{ignored} must be excluded from the watcher-grown side"
            );
        }
    }

    #[test]
    fn index_repo_persists_last_index_failed_to_meta() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/app.py", "def run():\n    return 1\n");
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();

        let r = index_repo(root, &mut store).unwrap();
        assert_eq!(r.failed.len(), 0);
        assert_eq!(
            store.meta("last_index_failed").unwrap().as_deref(),
            Some("0")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_goes_to_failed_and_others_still_index() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/app.py", "def run():\n    return 1\n");
        write(root, "src/secret.py", "def hidden():\n    return 2\n");

        let secret_path = root.join("src/secret.py");
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        let result = index_repo(root, &mut store);

        // Restore permissions before asserting/unwrapping so tempdir cleanup never fails.
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let r = result.unwrap();
        assert_eq!(r.failed.len(), 1, "failed: {:?}", r.failed);
        assert!(r.failed[0].contains("secret.py"), "failed: {:?}", r.failed);
        assert_eq!(r.indexed, 1);
        assert_eq!(store.counts().unwrap().files, 1);
        assert_eq!(
            store.meta("last_index_failed").unwrap().as_deref(),
            Some("1"),
            "failed count must be persisted for a later status read"
        );
    }

    #[cfg(unix)]
    #[test]
    fn transient_read_failure_preserves_existing_index_rows() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/app.py", "def run():\n    return 1\n");
        write(root, "src/secret.py", "def hidden():\n    return 2\n");

        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();

        // First run: file is readable, gets indexed successfully.
        let r = index_repo(root, &mut store).unwrap();
        assert_eq!(r.indexed, 2);
        assert_eq!(r.removed, 0);
        assert_eq!(store.counts().unwrap().files, 2);
        assert!(store
            .file_paths()
            .unwrap()
            .iter()
            .any(|p| p == "src/secret.py"));

        // Now make the file transiently unreadable (still present on disk) and re-index.
        let secret_path = root.join("src/secret.py");
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = index_repo(root, &mut store);

        // Restore permissions before asserting/unwrapping so tempdir cleanup never fails.
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let r = result.unwrap();
        assert_eq!(r.failed.len(), 1, "failed: {:?}", r.failed);
        assert!(r.failed[0].contains("secret.py"), "failed: {:?}", r.failed);

        // The read failure must NOT be treated as "file gone from disk": its
        // existing DB rows must survive, and removed must be 0.
        assert_eq!(r.removed, 0, "transient read failure must not remove rows");
        assert_eq!(store.counts().unwrap().files, 2);
        assert!(
            store
                .file_paths()
                .unwrap()
                .iter()
                .any(|p| p == "src/secret.py"),
            "secret.py rows should survive a transient read failure"
        );
    }

    struct ShortBatchEmbedder;
    impl vexus_embed::Embedder for ShortBatchEmbedder {
        fn id(&self) -> &str {
            "short"
        }
        fn dim(&self) -> usize {
            4
        }
        fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .skip(1)
                .map(|_| vec![1.0, 0.0, 0.0, 0.0])
                .collect()) // one short
        }
    }

    #[test]
    fn embed_pending_errors_on_wrong_batch_length() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f1():\n    pass\ndef f2():\n    pass\n");
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        index_repo(root, &mut store).unwrap();
        store.set_model("short", 4).unwrap();
        let err = embed_pending(&mut store, &ShortBatchEmbedder).unwrap_err();
        assert!(err.to_string().contains("batch"), "got: {err}");
    }
}
