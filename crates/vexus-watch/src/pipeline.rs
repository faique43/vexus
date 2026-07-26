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

pub fn index_repo(root: &Path, store: &mut Store) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let mut seen: HashSet<String> = HashSet::new();

    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".git" && name != ".vexus"
        })
        .build();

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
    Ok(report)
}

#[derive(Debug, Default)]
pub struct EmbedReport {
    pub embedded: usize,
    pub from_cache: usize,
}

/// Embed every chunk missing a vector, in batches, reusing `embed_cache` hits
/// (keyed by content hash) so unchanged content across re-indexes never pays
/// for a fresh model call. A no-op (vec unavailable, or nothing missing)
/// returns an empty report immediately.
pub fn embed_pending(store: &mut Store, embedder: &dyn Embedder) -> Result<EmbedReport> {
    let mut report = EmbedReport {
        embedded: 0,
        from_cache: 0,
    };
    loop {
        let backlog_before = store.embed_backlog()?;
        let missing = store.chunks_missing_embedding(256)?;
        if missing.is_empty() {
            break;
        }
        let mut ready: Vec<(i64, Vec<f32>)> = Vec::new();
        let mut to_embed: Vec<(i64, String, Vec<u8>)> = Vec::new();
        for (id, content, hash) in missing {
            match store.embed_cache_get(&hash)? {
                Some(v) if v.len() == embedder.dim() => {
                    ready.push((id, v));
                    report.from_cache += 1;
                }
                _ => to_embed.push((id, content, hash)),
            }
        }
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
            for ((id, _, hash), v) in batch.iter().zip(vecs) {
                store.embed_cache_put(hash, &v)?;
                ready.push((*id, v));
                report.embedded += 1;
            }
        }
        store.put_embeddings(&ready)?;

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
    fn embed_pending_embeds_all_chunks_and_uses_cache() {
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
        assert_eq!(r.from_cache, 0);
        assert_eq!(store.embed_backlog().unwrap(), 0);

        // touch a file -> chunks re-created with same content -> cache hits, no re-embeds
        write(root, "a.py", "def f1():\n    pass\n# comment\n");
        index_repo(root, &mut store).unwrap();
        let r = embed_pending(&mut store, &embedder).unwrap();
        assert!(
            r.from_cache >= 1,
            "unchanged chunk content must hit embed_cache"
        );
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
