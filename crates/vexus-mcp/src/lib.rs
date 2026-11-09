//! Vexus MCP server: exposes the index (structural + semantic) as
//! MCP tools over stdio for `vexus serve`.

pub mod bundle;
pub mod format;
pub mod server;
pub mod state;
pub mod tools;
pub mod update;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use vexus_watch::WriterLock;

use crate::state::AppState;
use crate::writer::{start_writer, WriterHandle};

/// How long the reader (lock-loser) path waits, in total, at startup for
/// the winner's very first index build to produce `index.db` before
/// falling back to serving anyway with a not-yet-populated store (finding
/// C3) — kept short, just a couple of seconds (finding D2, round 2): this
/// wait runs *before* the MCP `initialize` handshake completes, and a
/// client with its own (sometimes 30s or shorter) handshake timeout would
/// see `serve` as unresponsive/dead if this blocked anywhere near that
/// long — reintroducing C3's exact symptom through a different route. The
/// background thread (`fill_store_when_ready`) owns the actual long tail;
/// this bounded wait only exists to skip the "index not ready" text
/// entirely for the common case where the winner's first index finishes
/// near-instantly.
const READER_STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const READER_STARTUP_RETRY_ATTEMPTS: u32 = 4; // 4 * 500ms = 2s

/// How often the background thread (spawned only once the bounded startup
/// wait above is exhausted) tries again — `serve` is already up and
/// answering tool calls with `state::INDEX_NOT_READY` in the meantime, so
/// there's no urgency, just persistence.
const READER_BACKGROUND_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Entry point for `vexus serve [PATH]`. Blocks for the lifetime of the
/// stdio MCP session (until the client disconnects); builds its own tokio
/// runtime internally so callers don't need to be async.
pub fn serve(root: PathBuf) -> Result<()> {
    serve_on(root, rmcp::transport::stdio(), || {
        vexus_embed::select::make_embedder().map(Arc::from)
    })
}

/// [`serve`] over any transport, with the writer's embedder supplied by the
/// caller, so a test can run a real handshake in-process with an embedder of
/// its choosing.
fn serve_on<T, E, A>(
    root: PathBuf,
    transport: T,
    make_embedder: impl FnOnce() -> Option<Arc<dyn vexus_embed::Embedder>> + Send + 'static,
) -> Result<()>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve_async(root, transport, make_embedder))
}

async fn serve_async<T, E, A>(
    root: PathBuf,
    transport: T,
    make_embedder: impl FnOnce() -> Option<Arc<dyn vexus_embed::Embedder>> + Send + 'static,
) -> Result<()>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let db_path = root.join(".vexus/index.db");

    // Try to acquire the advisory writer lock. If another process holds it,
    // we'll run as a reader with no writer thread.
    let writer_lock = WriterLock::try_acquire(&root)?;
    let is_writer = writer_lock.is_some();

    // Straight away, before any (possibly long) startup indexing: run by
    // hand in a terminal, a stdio server that prints nothing and waits on
    // stdin is indistinguishable from a hang. One stderr line (never stdout
    // — that's the MCP transport) says what the silence is.
    eprintln!(
        "vexus: MCP server for {} over stdio ({} mode) — normally launched by an agent via \
         .mcp.json, not by hand; it waits silently for a client. Ctrl-C stops it.",
        root.display(),
        if is_writer { "writer" } else { "reader" }
    );
    if let Some(notice) = update::notice() {
        eprintln!("vexus: {notice}");
    }
    // Refresh off-thread and detached: the handshake must not wait on a
    // network round-trip, and a slow or hanging request must not delay
    // shutdown. Whatever it learns is picked up by the next invocation.
    std::thread::spawn(update::refresh_if_stale);

    // Opening the store here creates `index.db` (or recreates it, if a schema
    // bump wiped it) for the reader connection below. Neither building a
    // missing index nor loading the embedder happens here: both belong to
    // the writer thread, after the handshake (see `start_writer`).
    if is_writer {
        vexus_core::Store::open(&db_path)
            .with_context(|| format!("failed to open index at {}", db_path.display()))?;
        // Self-ignoring dir, like target/ — same convention as the CLI's
        // `open_store`. Best-effort: a stray `.gitignore` we can't write (e.g.
        // read-only checkout) is a nuisance, not a reason to refuse to serve.
        let _ = std::fs::write(root.join(".vexus/.gitignore"), "*\n");
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
            Ok(store) => {
                if store.counts().ok().map(|c| c.files) == Some(0) {
                    eprintln!(
                        "vexus: no index found for {root:?} — run 'vexus index' to build one \
                         (reader mode: another vexus serve owns the index)"
                    );
                }
                Some(store)
            }
            // Finding D1 (round 2): report the real, last-seen failure
            // rather than asserting facts we don't actually know. The old
            // message here ("no index found ... another vexus serve
            // finishes building it") was wrong on every clause whenever the
            // real cause was something else entirely — e.g. a read-only
            // `.vexus` directory, where the index *was* found but its WAL
            // companion files couldn't be (re)created, and nothing is
            // "building" anything.
            Err(e) => {
                eprintln!(
                    "vexus: index at {} still not queryable after {:?} ({e:#}) — serving \
                     anyway; tool calls will report the index isn't ready until it becomes \
                     queryable (if another 'vexus serve' is building the very first index, \
                     this resolves on its own once it finishes; otherwise see the error above)",
                    db_path.display(),
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
    // branch always populates `initial_store`: it created `index.db` itself).
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
        // Loaded into `AppState`'s `OnceLock`, so tools use the same
        // embedder the watcher does rather than loading the ~160 MB model a
        // second time. Readers keep the lazy path — they only pay for an
        // embedder if a tool actually needs a query vector.
        let shared = Arc::clone(&state);
        let load_embedder = move || shared.embedder.get_or_init(make_embedder).clone();
        writer_handle = Some(start_writer(root.clone(), &db_path, load_embedder, lock)?);
    }

    let server = server::VexusServer::new(state);
    server.serve(transport).await?.waiting().await?;
    // Explicit, even though it would happen anyway when `writer_handle`
    // goes out of scope at the end of the function: this is the actual
    // "tell the writer thread to shut down, then wait for it" step, and it
    // belongs right here — once serving is genuinely done — not any
    // earlier.
    drop(writer_handle);
    Ok(())
}

/// Repeatedly attempts `open_reader_with_probe`, `interval` apart, up to
/// `attempts` times, returning the first success — or, once `attempts` is
/// exhausted, `Err` of the *last* attempt's actual failure (finding D1,
/// round 2: previously this discarded the error entirely via `if let
/// Ok(...)`, so a caller had no way to tell "nothing built yet" apart from
/// e.g. a permissions problem that will never resolve on its own). The
/// bounded startup wait a reader (lock-loser) process gives the writer's
/// very first index build before falling back to serving with a
/// not-yet-populated store (finding C3). An async sleep (rather than
/// `std::thread::sleep`) since this runs directly inside `serve_async` on
/// the tokio runtime, before `serve` itself is up.
async fn wait_for_reader_store(
    db_path: &Path,
    root: &Path,
    attempts: u32,
    interval: Duration,
) -> Result<vexus_core::Store> {
    let mut last_err = None;
    for attempt in 0..attempts {
        match open_reader_with_probe(db_path, root) {
            Ok(store) => return Ok(store),
            Err(e) => last_err = Some(e),
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(interval).await;
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no attempt was made (attempts == 0)")))
}

/// Blocks the calling thread, retrying `open_reader_with_probe` every
/// `interval` forever until it succeeds, then fills `state`'s store — the
/// background fallback (finding C3) for when `wait_for_reader_store`'s
/// bounded startup wait was exhausted: `serve` is already up and answering
/// tool calls with `state::INDEX_NOT_READY` by the time this thread runs,
/// so there's no reason for it to ever give up. Logs the *first* failure
/// it sees, once (finding D1, round 2) — not the bare `if let Ok(...)` this
/// used to be, which silently swallowed every retry's error forever, and
/// not a log line per retry either, which would spam stderr indefinitely
/// for a condition that (by design) might never resolve. A plain function
/// (rather than inlined into a `thread::spawn` closure) so a test can
/// drive it directly with a tiny `interval` against a real filesystem
/// race, instead of waiting out the real (multi-second) production
/// interval.
fn fill_store_when_ready(state: &Arc<AppState>, db_path: &Path, root: &Path, interval: Duration) {
    let mut logged = false;
    loop {
        match open_reader_with_probe(db_path, root) {
            Ok(store) => {
                *state.lock_store() = Some(store);
                return;
            }
            Err(e) => {
                if !logged {
                    eprintln!(
                        "vexus: still waiting for a queryable index at {} ({e:#}); will keep \
                         retrying in the background every {interval:?}",
                        db_path.display()
                    );
                    logged = true;
                }
            }
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
/// `Store::open_read_only` can succeed
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
/// becomes the *first* thing to open the DB at all once the advisory
/// lock lets a losing process skip that writer-open step entirely.
///
/// `Store::open_read_only` itself runs a real
/// query right after opening (comparing `meta('schema_version')`) — so the
/// same WAL-creation failure this whole function exists to explain can now
/// surface from *that* call instead of from the `counts()` probe below. The
/// same read-only-filesystem hint is attached to both, so the explanation a
/// caller sees doesn't depend on exactly which of the two first touches the
/// directory for real.
fn open_reader_with_probe(db_path: &Path, root: &Path) -> Result<vexus_core::Store> {
    let hint = || {
        format!(
            "if {} (or its .vexus subdirectory) is on a read-only filesystem, this is almost \
             certainly why — SQLite needs write access to the directory containing index.db to \
             create its WAL companion files, even for read-only queries",
            root.display()
        )
    };
    let store = vexus_core::Store::open_read_only(db_path).with_context(|| {
        format!(
            "failed to open read-only index at {}; {}",
            db_path.display(),
            hint()
        )
    })?;
    store.counts().with_context(|| {
        format!(
            "failed to query index at {} right after opening it read-only; {}",
            db_path.display(),
            hint()
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
            store.is_ok(),
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
            store.is_ok(),
            "must succeed once index.db appears within the retry window, not just on attempt 1"
        );
    }

    /// The bounded side of "bounded wait": if `index.db` never appears, this
    /// must give up once `attempts` is exhausted rather than hang forever —
    /// what makes it safe to await directly inside `serve_async`. Finding
    /// D1 (round 2): the `Err` it gives up with must be the real, last-seen
    /// failure (here: `Store::open_read_only` erroring on a genuinely
    /// missing file) — not silently discarded the way `if let Ok(...)`
    /// used to — so a caller can actually tell "nothing built yet" apart
    /// from a permissions problem that will never resolve on its own.
    #[tokio::test]
    async fn wait_for_reader_store_gives_up_after_exhausting_its_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(".vexus/index.db"); // never created

        let result =
            wait_for_reader_store(&db_path, &root, 3, std::time::Duration::from_millis(5)).await;
        let err = match result {
            Ok(_) => panic!("must give up once attempts are exhausted, not hang forever"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("index.db") || msg.contains("open"),
            "the give-up error must be the real underlying failure, not a generic \
             placeholder: {msg:?}"
        );
    }

    /// Finding D2 (round 2): the pre-`initialize`-handshake bounded wait
    /// must stay short — a client with its own handshake timeout (Claude
    /// Code's default is 30s) would see `serve` as dead if this blocked
    /// anywhere near that long, reintroducing C3's exact symptom via a new
    /// route. Guards against the budget quietly creeping back up.
    #[test]
    fn reader_startup_retry_budget_stays_well_under_a_typical_handshake_timeout() {
        let budget = READER_STARTUP_RETRY_INTERVAL * READER_STARTUP_RETRY_ATTEMPTS;
        assert!(
            budget <= Duration::from_secs(5),
            "pre-handshake retry budget must stay a few seconds, not tens of seconds: {budget:?}"
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

    /// A directory that can't be written
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

    /// Embeds like `MockEmbedder`, but every batch takes `self.0`: a real
    /// model's first pass over a repo takes minutes.
    struct SlowEmbedder(Duration);

    impl vexus_embed::Embedder for SlowEmbedder {
        fn id(&self) -> &str {
            "slow-mock"
        }
        fn dim(&self) -> usize {
            vexus_embed::Embedder::dim(&vexus_embed::MockEmbedder)
        }
        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            std::thread::sleep(self.0);
            vexus_embed::Embedder::embed(&vexus_embed::MockEmbedder, texts)
        }
    }

    async fn call_tool(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        name: &'static str,
        arguments: Option<rmcp::serde_json::Value>,
    ) -> String {
        let mut params = rmcp::model::CallToolRequestParams::new(name);
        if let Some(arguments) = arguments {
            params = params.with_arguments(rmcp::model::object(arguments));
        }
        let result = client
            .call_tool(params)
            .await
            .unwrap_or_else(|e| panic!("{name} failed: {e}"));
        result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Calls `status` until `done` holds, returning that status text, or
    /// panics with the last one seen once `within` runs out.
    async fn wait_for_status(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        within: Duration,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = std::time::Instant::now() + within;
        loop {
            let status = call_tool(client, "status", None).await;
            if done(&status) {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "status never got there within {within:?}: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn embed_backlog(status: &str) -> i64 {
        status
            .split("embed backlog: ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no embed backlog in status: {status:?}"))
    }

    /// Runs `serve` in-process against `root`, whose index is missing or
    /// unusable, with an embedder that takes seconds per batch. Claude Code
    /// gives up on a server that hasn't answered `initialize` within 30s, and
    /// `serve` used to build the whole index, embeddings included, before
    /// answering it. The handshake must not wait for that build, and the
    /// build must still finish: structure first, so tools work while the
    /// embeddings are still running.
    async fn serve_answers_at_once_and_builds_in_the_background(root: &Path) {
        std::fs::write(
            root.join("app.py"),
            "def alpha_process():\n    return unique_marker_beta()\n\n\n\
             def unique_marker_beta():\n    return 42\n",
        )
        .unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server_root = root.to_path_buf();
        let server = std::thread::spawn(move || {
            serve_on(server_root, server_io, || {
                Some(Arc::new(SlowEmbedder(Duration::from_secs(4))))
            })
        });

        let client = tokio::time::timeout(Duration::from_secs(2), ().serve(client_io))
            .await
            .expect("initialize must be answered within 2s, without waiting for the index")
            .expect("initialize handshake failed");
        let tools = tokio::time::timeout(Duration::from_secs(2), client.list_all_tools())
            .await
            .expect("tools/list must be answered within 2s")
            .expect("tools/list failed");
        assert!(tools.iter().any(|t| t.name == "explore"), "{tools:?}");

        // Structure lands first: the index is queryable while the slow
        // embedder is still on its first batch.
        let status = wait_for_status(&client, Duration::from_secs(20), |s| {
            s.contains("freshness: fresh") && !s.contains("index: 0 files")
        })
        .await;
        assert!(
            embed_backlog(&status) > 0,
            "the structural index must not wait for embeddings: {status:?}"
        );
        let callees = call_tool(
            &client,
            "trace",
            Some(rmcp::serde_json::json!({
                "symbol": "app.alpha_process",
                "direction": "callees",
            })),
        )
        .await;
        assert!(callees.contains("unique_marker_beta"), "{callees:?}");

        // Then the embeddings finish on their own.
        wait_for_status(&client, Duration::from_secs(30), |s| embed_backlog(s) == 0).await;

        client.cancel().await.expect("clean MCP shutdown failed");
        server
            .join()
            .expect("serve panicked")
            .expect("serve returned an error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serve_answers_initialize_before_building_a_missing_index() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!dir.path().join(".vexus").exists());
        serve_answers_at_once_and_builds_in_the_background(dir.path()).await;
    }

    /// A schema bump wipes the old index on open, which lands on the same
    /// cold path as a repo that was never indexed.
    #[tokio::test(flavor = "multi_thread")]
    async fn serve_answers_initialize_before_rebuilding_an_outdated_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("old.py"), "def old():\n    return 1\n").unwrap();
        {
            let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
            vexus_watch::pipeline::index_repo(root, &mut store).unwrap();
            store.set_meta("schema_version", "0").unwrap();
        }
        serve_answers_at_once_and_builds_in_the_background(root).await;
    }

    /// On a machine that has never run vexus, building the embedder first
    /// downloads the ~160 MB model, which can take longer than a client waits
    /// for `initialize`. The handshake must not wait for it, tools must answer
    /// (keyword-only) while it loads, and the embeddings must still get done
    /// once it has.
    #[tokio::test(flavor = "multi_thread")]
    async fn serve_answers_while_the_embedder_is_still_loading() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("app.py"),
            "def alpha_process():\n    return unique_marker_beta()\n\n\n\
             def unique_marker_beta():\n    return 42\n",
        )
        .unwrap();
        {
            let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
            vexus_watch::pipeline::index_repo(root, &mut store).unwrap();
        }

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server_root = root.to_path_buf();
        let server = std::thread::spawn(move || {
            serve_on(server_root, server_io, || {
                std::thread::sleep(Duration::from_secs(6));
                Some(Arc::new(vexus_embed::MockEmbedder))
            })
        });

        let client = tokio::time::timeout(Duration::from_secs(2), ().serve(client_io))
            .await
            .expect("initialize must be answered within 2s, without waiting for the embedder")
            .expect("initialize handshake failed");
        let explore = tokio::time::timeout(
            Duration::from_secs(2),
            call_tool(
                &client,
                "explore",
                Some(rmcp::serde_json::json!({ "question": "unique_marker_beta" })),
            ),
        )
        .await
        .expect("explore must answer within 2s, without waiting for the embedder");
        assert!(explore.contains("app.py"), "{explore:?}");

        wait_for_status(&client, Duration::from_secs(30), |s| embed_backlog(s) == 0).await;

        client.cancel().await.expect("clean MCP shutdown failed");
        server
            .join()
            .expect("serve panicked")
            .expect("serve returned an error");
    }
}
