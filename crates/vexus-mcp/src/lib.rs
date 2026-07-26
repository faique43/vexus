//! Vexus MCP server: exposes the Plan 1+2 index (structural + semantic) as
//! MCP tools over stdio for `vexus serve`.

pub mod bundle;
pub mod format;
pub mod server;
pub mod state;
pub mod tools;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use vexus_watch::{pipeline, WriterLock};

use crate::state::AppState;
use crate::writer::{start_writer, WriterHandle};

/// Entry point for `vexus serve [PATH]`. Blocks for the lifetime of the
/// stdio MCP session (until the client disconnects); builds its own tokio
/// runtime internally so callers don't need to be async.
pub fn serve(root: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve_async(root))
}

async fn serve_async(root: PathBuf) -> Result<()> {
    let db_path = root.join(".vexus/index.db");

    // Try to acquire the advisory writer lock. If another process holds it,
    // we'll run as a reader with no writer thread (Task 6).
    let writer_lock = WriterLock::try_acquire(&root)?;
    let is_writer = writer_lock.is_some();

    // Only the writer does startup indexing.
    if is_writer {
        let mut store = vexus_core::Store::open(&db_path)
            .with_context(|| format!("failed to open index at {}", db_path.display()))?;
        // Self-ignoring dir, like target/ — same convention as the CLI's
        // `open_store`. Best-effort: a stray `.gitignore` we can't write (e.g.
        // read-only checkout) is a nuisance, not a reason to refuse to serve.
        let _ = std::fs::write(root.join(".vexus/.gitignore"), "*\n");

        if store.counts()?.files == 0 {
            eprintln!("vexus: no index found for {root:?} — building one now...");
            // A failed initial index degrades to serving an empty/partial index
            // rather than refusing to start at all — `status` still reports the
            // truth, and tools simply have less to work with until the caller
            // re-runs `vexus index`.
            match pipeline::index_repo(&root, &mut store)
                .with_context(|| format!("failed to index repo at {}", root.display()))
            {
                Ok(report) => {
                    eprintln!(
                        "vexus: indexed {} files ({} failed)",
                        report.indexed,
                        report.failed.len()
                    );
                    if store.vec_available() {
                        match vexus_embed::select::make_embedder() {
                            Some(embedder) => {
                                store.set_model(embedder.id(), embedder.dim())?;
                                match pipeline::embed_pending(&mut store, embedder.as_ref()) {
                                    Ok(er) => eprintln!(
                                        "vexus: embedded {} chunks (cache hits: {})",
                                        er.embedded, er.from_cache
                                    ),
                                    Err(e) => eprintln!(
                                        "vexus: embedding failed ({e:#}); serving structural-only"
                                    ),
                                }
                            }
                            None => {
                                eprintln!("vexus: embeddings unavailable; serving structural-only")
                            }
                        }
                    } else {
                        eprintln!(
                            "vexus: embeddings skipped (sqlite-vec unavailable); serving structural-only"
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "vexus: initial indexing failed ({e:#}); serving with an empty index"
                    );
                }
            }
        }
    } else {
        // Reader path: check if DB is empty and warn.
        let store = open_reader_with_probe(&db_path, &root)?;
        if store.counts().ok().map(|c| c.files) == Some(0) {
            eprintln!("vexus: no index found for {root:?} — run 'vexus index' to build one (reader mode: another vexus serve owns the index)");
        }
    }

    // Tool handlers only ever read; opening read-only keeps concurrent
    // access (this process's watcher, spawned below, and any other reader)
    // from contending with tools over a write lock, and makes accidental
    // writes from a tool handler fail loudly instead of corrupting state.
    let store = open_reader_with_probe(&db_path, &root)?;

    let state = Arc::new(AppState {
        store: Mutex::new(store),
        embedder: OnceLock::new(),
        root: root.clone(),
        last_generation: AtomicU64::new(0),
        is_writer,
    });

    // Only start the writer thread if we won the lock. `writer_handle` is
    // held across *both* awaits below (the `serve` handshake and the
    // `waiting` loop) and only dropped once they're done — see `writer.rs`'s
    // module doc for the real bug this structure exists to make impossible
    // to reintroduce: an earlier version's shutdown channel was scoped to
    // end inside this `if` block, disconnecting (and so signalling
    // shutdown) long before `serve` itself was actually done.
    let mut writer_handle: Option<WriterHandle> = None;
    if let Some(lock) = writer_lock {
        let embedder = state.embedder();
        writer_handle = Some(start_writer(root.clone(), &db_path, embedder, lock)?);
    }

    let server = server::VexusServer::new(state);
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    // Explicit, even though it would happen anyway when `writer_handle`
    // goes out of scope at the end of the function: this is the actual
    // "tell the writer thread to shut down, then wait for it" step, and it
    // belongs right here — once serving is genuinely done — not any
    // earlier.
    drop(writer_handle);
    Ok(())
}

/// Opens `db_path` read-only, then immediately runs a cheap probe query
/// against it so a specific class of failure surfaces here, with a clear
/// hint, instead of confusing the first real tool call.
///
/// Carried finding (Task 1 review): `Store::open_read_only` can succeed
/// even when `root`'s containing directory can't be written to, because
/// opening a connection doesn't by itself need to touch anything beyond the
/// `.db` file. The first *query* is a different story — this index was
/// written by a writer `Store` in WAL mode (see `Store::open`), and WAL
/// requires SQLite to create/maintain `-wal`/`-shm` companion files
/// alongside `index.db`, even for a read-only connection (readers still
/// need the shared-memory wal-index to establish a consistent read
/// snapshot). If those companion files don't already exist and the
/// directory can't be written to, creating them fails right here, on the
/// very first real query, with an opaque low-level SQLite error ("unable to
/// open database file") that gives no hint about *why*. Kept as its own
/// function (rather than inlined into `serve_async`) so it's unit-testable
/// on its own — `serve_async`'s mandatory startup writer-open tends to
/// surface most on-disk permission problems earlier anyway, but this
/// becomes the *first* thing to open the DB at all once Task 6's advisory
/// lock lets a losing process skip that writer-open step entirely.
fn open_reader_with_probe(db_path: &Path, root: &Path) -> Result<vexus_core::Store> {
    let store = vexus_core::Store::open_read_only(db_path)
        .with_context(|| format!("failed to open read-only index at {}", db_path.display()))?;
    store.counts().with_context(|| {
        format!(
            "failed to query index at {} right after opening it read-only; if {} (or its \
             .vexus subdirectory) is on a read-only filesystem, this is almost certainly why — \
             SQLite needs write access to the directory containing index.db to create its WAL \
             companion files, even for read-only queries",
            db_path.display(),
            root.display()
        )
    })?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store-open failure is the one startup error that must still be
    /// fatal (a `.vexus/index.db` we can't even open isn't something
    /// `serve` can degrade around) — but it must fail with a message naming
    /// *what* failed to open, not a bare io error. Forces the failure by
    /// pre-creating `.vexus` as a plain file, so `Store::open`'s
    /// `create_dir_all(root/.vexus)` errors out before touching stdio (this
    /// runs synchronously to completion — it never reaches the point where
    /// `serve` would block on real stdin).
    #[test]
    fn serve_wraps_store_open_failure_with_the_index_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join(".vexus"), b"not a directory").unwrap();

        let err = serve(root.clone()).expect_err("must fail when .vexus is a file");
        let msg = format!("{err:#}");
        // The lock acquisition now runs first, so check for the lock/index error
        let has_clear_error = msg.contains("failed to open index")
            || msg.contains("lock")
            || msg.contains("directory");
        assert!(
            has_clear_error,
            "expected the wrapped context to name the failing step: {msg}"
        );
        let has_path_context = msg.contains("index.db") || msg.contains(".vexus");
        assert!(
            has_path_context,
            "expected the error to name the relevant path: {msg}"
        );
    }

    /// Carried finding (Task 1 review): a directory that can't be written
    /// to makes `open_read_only` succeed but the very next query fail
    /// confusingly (SQLite needing to create fresh `-wal`/`-shm` companion
    /// files it can't). Reproduced directly against
    /// `open_reader_with_probe` (rather than through the whole `serve`
    /// flow) because `serve_async`'s own mandatory startup writer-open
    /// would otherwise hit the very same permission problem first — this
    /// isolates the read-only-open-then-probe code path this task adds.
    #[cfg(unix)]
    #[test]
    fn open_reader_with_probe_names_directory_writability_as_the_likely_cause() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.py"), "def f():\n    return 1\n").unwrap();

        let db_path = root.join(".vexus/index.db");
        {
            // Directory is still writable here: build a real index and let
            // the writer close normally (which leaves the DB in WAL mode
            // with no guarantee its `-wal`/`-shm` companion files survive
            // the close).
            let mut store = vexus_core::Store::open(&db_path).unwrap();
            vexus_watch::pipeline::index_repo(&root, &mut store).unwrap();
        }
        // Make sure they're gone regardless, so the read-only reopen below
        // has to (try to) recreate them on its first query.
        std::fs::remove_file(root.join(".vexus/index.db-wal")).ok();
        std::fs::remove_file(root.join(".vexus/index.db-shm")).ok();

        let vexus_dir = root.join(".vexus");
        std::fs::set_permissions(&vexus_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = open_reader_with_probe(&db_path, &root);

        // Restore permissions before asserting/unwrapping so tempdir
        // cleanup never fails.
        std::fs::set_permissions(&vexus_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = match result {
            Ok(_) => panic!(
                "a read-only directory missing its WAL companion files must fail the probe query"
            ),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("read-only filesystem") && msg.contains("write access"),
            "expected the error to hint at directory writability: {msg}"
        );
    }
}
