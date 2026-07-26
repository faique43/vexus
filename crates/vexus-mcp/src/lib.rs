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
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use vexus_watch::{pipeline, WriterLock};

use crate::state::AppState;
use crate::writer::{start_writer, WriterHandle};

/// How long the reader (lock-loser) path waits, in total, at startup for
/// the winner's very first index build to produce `index.db` before
/// falling back to serving anyway with a not-yet-populated store (finding
/// C3) — long enough to ride out any first-index build this tool is meant
/// for; a repo whose very first index takes longer than this is rare
/// enough that "serve now, keep retrying in the background" beats blocking
/// `serve` from ever coming up at all.
const READER_STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const READER_STARTUP_RETRY_ATTEMPTS: u32 = 60; // 60 * 500ms = 30s

/// How often the background thread (spawned only once the bounded startup
/// wait above is exhausted) tries again — `serve` is already up and
/// answering tool calls with `state::INDEX_NOT_READY` in the meantime, so
/// there's no urgency, just persistence.
const READER_BACKGROUND_RETRY_INTERVAL: Duration = Duration::from_secs(2);

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
    }

    // Tool handlers only ever read; opening read-only keeps concurrent
    // access (this process's watcher, spawned below, and any other reader)
    // from contending with tools over a write lock, and makes accidental
    // writes from a tool handler fail loudly instead of corrupting state.
    //
    // The writer branch above already guarantees `db_path` exists by this
    // point (it just opened/created it), so a probe failure there stays a
    // hard `serve`-ending error. The reader (lock-loser) path may instead be
    // racing the winner's very first index build — `index.db` might not
    // exist yet at all — so it gets a bounded wait first (finding C3):
    // failing `serve` outright just because this process lost that race is
    // worse than a client seeing "index not ready" on its first few tool
    // calls. If even the bounded wait comes up empty, `serve` still comes
    // up — with `store: None` — and a background thread keeps retrying so
    // it self-heals the moment the winner's index appears, with no restart
    // needed.
    let initial_store = if is_writer {
        Some(open_reader_with_probe(&db_path, &root)?)
    } else {
        match wait_for_reader_store(
            &db_path,
            &root,
            READER_STARTUP_RETRY_ATTEMPTS,
            READER_STARTUP_RETRY_INTERVAL,
        )
        .await
        {
            Some(store) => {
                if store.counts().ok().map(|c| c.files) == Some(0) {
                    eprintln!(
                        "vexus: no index found for {root:?} — run 'vexus index' to build one \
                         (reader mode: another vexus serve owns the index)"
                    );
                }
                Some(store)
            }
            None => {
                eprintln!(
                    "vexus: no index found for {root:?} after {:?} — serving anyway; tool \
                     calls will report the index isn't ready until another 'vexus serve' \
                     finishes building it (reader mode: another vexus serve owns the index)",
                    READER_STARTUP_RETRY_INTERVAL * READER_STARTUP_RETRY_ATTEMPTS
                );
                None
            }
        }
    };

    let state = Arc::new(AppState {
        store: Mutex::new(initial_store),
        embedder: OnceLock::new(),
        root: root.clone(),
        last_generation: AtomicU64::new(0),
        is_writer,
    });

    // Only reachable via the reader path's None arm above (the writer
    // branch always populates `initial_store`, and the writer's own eager
    // startup index build means it never races anything to begin with).
    if state.lock_store().is_none() {
        spawn_background_reader_retry(Arc::clone(&state), db_path.clone(), root.clone());
    }

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

/// Repeatedly attempts `open_reader_with_probe`, `interval` apart, up to
/// `attempts` times, returning the first success (or `None` once `attempts`
/// is exhausted) — the bounded startup wait a reader (lock-loser) process
/// gives the writer's very first index build before falling back to
/// serving with a not-yet-populated store (finding C3). An async sleep
/// (rather than `std::thread::sleep`) since this runs directly inside
/// `serve_async` on the tokio runtime, before `serve` itself is up.
async fn wait_for_reader_store(
    db_path: &Path,
    root: &Path,
    attempts: u32,
    interval: Duration,
) -> Option<vexus_core::Store> {
    for attempt in 0..attempts {
        if let Ok(store) = open_reader_with_probe(db_path, root) {
            return Some(store);
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(interval).await;
        }
    }
    None
}

/// Blocks the calling thread, retrying `open_reader_with_probe` every
/// `interval` forever until it succeeds, then fills `state`'s store — the
/// background fallback (finding C3) for when `wait_for_reader_store`'s
/// bounded startup wait was exhausted: `serve` is already up and answering
/// tool calls with `state::INDEX_NOT_READY` by the time this thread runs,
/// so there's no reason for it to ever give up. A plain function (rather
/// than inlined into a `thread::spawn` closure) so a test can drive it
/// directly with a tiny `interval` against a real filesystem race, instead
/// of waiting out the real (multi-second) production interval.
fn fill_store_when_ready(state: &Arc<AppState>, db_path: &Path, root: &Path, interval: Duration) {
    loop {
        if let Ok(store) = open_reader_with_probe(db_path, root) {
            *state.lock_store() = Some(store);
            return;
        }
        std::thread::sleep(interval);
    }
}

/// Thin `thread::spawn` wrapper around `fill_store_when_ready`, using the
/// real production interval. Detached, deliberately: it either fills the
/// store and exits on its own, or the whole process exits with it when
/// `serve` itself does (this thread holds nothing — no lock, no file
/// handle beyond a probe's — worth tearing down explicitly on shutdown).
fn spawn_background_reader_retry(state: Arc<AppState>, db_path: PathBuf, root: PathBuf) {
    std::thread::spawn(move || {
        fill_store_when_ready(&state, &db_path, &root, READER_BACKGROUND_RETRY_INTERVAL)
    });
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

    /// Finding C3, stage 1 (the bounded startup wait): succeeds on the very
    /// first attempt when `index.db` already exists — the common case,
    /// where this reader isn't actually racing anything.
    #[tokio::test]
    async fn wait_for_reader_store_succeeds_immediately_when_the_index_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(".vexus/index.db");
        {
            let mut store = vexus_core::Store::open(&db_path).unwrap();
            vexus_watch::pipeline::index_repo(&root, &mut store).unwrap();
        }

        let store =
            wait_for_reader_store(&db_path, &root, 5, std::time::Duration::from_millis(10)).await;
        assert!(
            store.is_some(),
            "must succeed on the first attempt when index.db already exists"
        );
    }

    /// The actual race finding C3 is about: `index.db` doesn't exist *yet*
    /// when the wait starts (the winner is still building its first index),
    /// but appears partway through — a background thread here simulates the
    /// winner finishing shortly after this reader starts waiting.
    #[tokio::test]
    async fn wait_for_reader_store_succeeds_once_the_index_appears_mid_wait() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(".vexus/index.db");
        assert!(!db_path.exists());

        let root_for_winner = root.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            let mut store =
                vexus_core::Store::open(&root_for_winner.join(".vexus/index.db")).unwrap();
            vexus_watch::pipeline::index_repo(&root_for_winner, &mut store).unwrap();
        });

        let store =
            wait_for_reader_store(&db_path, &root, 20, std::time::Duration::from_millis(20)).await;
        assert!(
            store.is_some(),
            "must succeed once index.db appears within the retry window, not just on attempt 1"
        );
    }

    /// The bounded side of "bounded wait": if `index.db` never appears, this
    /// must give up once `attempts` is exhausted rather than hang forever —
    /// what makes it safe to await directly inside `serve_async`.
    #[tokio::test]
    async fn wait_for_reader_store_gives_up_after_exhausting_its_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(".vexus/index.db"); // never created

        let store =
            wait_for_reader_store(&db_path, &root, 3, std::time::Duration::from_millis(5)).await;
        assert!(
            store.is_none(),
            "must give up once attempts are exhausted, not hang forever"
        );
    }

    /// Finding C3, stage 2 (the background fallback): once the bounded
    /// startup wait has already given up and `serve` is running with a
    /// `None` store, `fill_store_when_ready` must keep retrying and
    /// populate `state.store` the moment `index.db` actually appears —
    /// exercised directly (with a tiny interval) rather than through the
    /// real 2s production interval.
    #[test]
    fn fill_store_when_ready_populates_the_state_once_the_index_appears() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(".vexus/index.db");

        let state = Arc::new(AppState {
            store: Mutex::new(None),
            embedder: OnceLock::new(),
            root: root.clone(),
            last_generation: AtomicU64::new(0),
            is_writer: false,
        });

        let state_for_thread = Arc::clone(&state);
        let (db_path_for_thread, root_for_thread) = (db_path.clone(), root.clone());
        let handle = std::thread::spawn(move || {
            fill_store_when_ready(
                &state_for_thread,
                &db_path_for_thread,
                &root_for_thread,
                Duration::from_millis(10),
            );
        });

        // Confirm it's genuinely still waiting before index.db exists —
        // otherwise the assertion below would pass vacuously.
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            state.lock_store().is_none(),
            "must still be waiting with nothing on disk yet"
        );

        // Simulate the winner finishing its first index build.
        let mut writer = vexus_core::Store::open(&db_path).unwrap();
        vexus_watch::pipeline::index_repo(&root, &mut writer).unwrap();
        drop(writer);

        handle.join().unwrap();
        assert!(
            state.lock_store().is_some(),
            "fill_store_when_ready must populate the store once index.db exists"
        );
    }

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
