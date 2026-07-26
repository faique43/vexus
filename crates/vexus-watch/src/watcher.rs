//! The watcher task: a `notify` filesystem watch feeding the pure
//! [`Debouncer`](crate::debounce::Debouncer), draining ready paths through
//! [`update_file`](crate::update::update_file) on a std thread that owns the
//! writer [`Store`] for its entire lifetime.
//!
//! This is the only place in the crate that touches threads, channels, or
//! the OS filesystem-event API — everything it calls into (`Debouncer`,
//! `update_file`, `set_freshness`) is itself plain, synchronous, and already
//! unit-tested on its own.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use vexus_core::Store;
use vexus_embed::Embedder;

use crate::debounce::Debouncer;
use crate::freshness::{get_freshness, set_freshness, Freshness};
use crate::update::{update_file, UpdateOutcome};

/// How long `rx.recv_timeout` waits per loop iteration before falling
/// through to a `drain_ready` check anyway — this is what makes debounced
/// paths eventually drain even during a lull with no new filesystem events.
const RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// How many main-loop iterations between `meta('needs_reconcile')` checks.
/// A cheap `meta` read every single iteration would be wasteful busywork at
/// the loop's ~100ms cadence; checking every 10th iteration (~1s, in the
/// common case where each iteration isn't itself blocked handling a large
/// event burst) is frequent enough that a flagged reconcile gets picked up
/// promptly without turning it into a per-tick cost.
const NEEDS_RECONCILE_CHECK_EVERY: u64 = 10;

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Best-effort: mark the index degraded and flag it for a reconcile pass.
/// Errors from these writes are swallowed deliberately — if the store itself
/// is unwritable there's nothing more this thread can do about it beyond
/// what its caller already observes independently (e.g. every tool call
/// failing), and panicking the watcher thread over a meta write would only
/// make things worse (no more updates at all, not even a degraded flag).
fn mark_degraded(store: &mut Store) {
    let _ = set_freshness(store, Freshness::Degraded);
    let _ = store.set_meta("needs_reconcile", "1");
}

/// Turn a raw `notify::Event`'s (absolute) paths into repo-relative,
/// forward-slash-normalized paths, dropping anything under `.vexus/` or
/// `.git/` and anything outside `root` entirely. `Access` events are ignored
/// up front (no paths returned) — they fire on reads, not writes, and carry
/// no information `update_file` needs to act on.
fn normalize_event_paths(root: &Path, event: &notify::Event) -> Vec<PathBuf> {
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return Vec::new();
    }
    event
        .paths
        .iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(root).ok()?;
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rel.is_empty()
                || rel == ".vexus"
                || rel.starts_with(".vexus/")
                || rel == ".git"
                || rel.starts_with(".git/")
            {
                return None;
            }
            Some(PathBuf::from(rel))
        })
        .collect()
}

/// Drain whatever's ready and, if anything was, apply each through
/// `update_file`. A drain that touched at least one path with zero
/// `Failed`/`Err` outcomes stamps `meta('last_event_at')` and, per the
/// freshness rule below, may mark the index `Fresh`; any failure marks it
/// `Degraded` and flags `needs_reconcile` instead. An empty drain (nothing
/// yet past its debounce window) does nothing.
///
/// The successful-drain case only sets `Fresh` when the index's *current*
/// freshness is `Fresh` or `Degraded` — a good drain healing either of
/// those is exactly what should happen. But if a reconcile pass (or the
/// initial index build) is in flight concurrently — `Reconciling` or
/// `Indexing` — this drain succeeding says nothing about whether *that*
/// pass has finished; stomping its state to `Fresh` here would let a
/// still-incomplete reconcile look done. Leaving the state alone in that
/// case means the reconcile/index pass itself owns when it transitions to
/// `Fresh` (or `Degraded`) once it actually completes.
fn drain_and_apply(
    store: &mut Store,
    debouncer: &mut Debouncer,
    embedder: Option<&dyn Embedder>,
    root: &Path,
    now: Instant,
) {
    let ready = debouncer.drain_ready(now);
    if ready.is_empty() {
        return;
    }

    let mut any_failed = false;
    for rel_path in &ready {
        let rel = rel_path.to_string_lossy();
        match update_file(store, embedder, root, &rel) {
            Ok(UpdateOutcome::Failed(_)) | Err(_) => any_failed = true,
            Ok(_) => {}
        }
    }

    if any_failed {
        mark_degraded(store);
    } else {
        let current = get_freshness(store).unwrap_or(Freshness::Degraded);
        if matches!(current, Freshness::Fresh | Freshness::Degraded) {
            let _ = set_freshness(store, Freshness::Fresh);
        }
        let _ = store.set_meta("last_event_at", &now_unix_secs().to_string());
    }
}

/// Whether `meta('needs_reconcile')` is currently set — a read error is
/// treated the same as "not set" (there's nothing more productive to do
/// about a meta-read failure here than wait for the next check).
fn needs_reconcile(store: &Store) -> bool {
    store.meta("needs_reconcile").ok().flatten().as_deref() == Some("1")
}

/// The writer thread's actual body: optionally reconcile, then watch —
/// both against the *same* writer `Store`, on the *same* thread, for the
/// whole of `store`'s life. Factored out of `spawn_watcher` so `vexus-mcp`'s
/// `serve` can run "reconcile once at startup, then watch" as a single
/// writer-owning thread (via [`spawn_writer`]) without duplicating the
/// watch-loop half of that; `spawn_watcher` itself becomes a thin wrapper
/// passing `do_reconcile: false`, keeping every existing watcher-only test
/// (and caller) working unchanged.
///
/// When `do_reconcile` is true, a [`crate::reconcile::reconcile`] pass runs
/// before the watch loop starts — freshness goes `Reconciling` for its
/// duration, `Fresh` or `Degraded` per its own outcome. A reconcile failure
/// is logged to stderr and does **not** stop the watch loop from starting
/// anyway: a degraded-but-watched index can still heal incrementally (or
/// via the `needs_reconcile` flag below), whereas refusing to watch at all
/// over one failed reconcile would leave it stuck.
///
/// Once watching, every `NEEDS_RECONCILE_CHECK_EVERY`th loop iteration
/// checks `meta('needs_reconcile')` (set by [`mark_degraded`] when the
/// watcher itself hits a `notify` error it can't recover from
/// incrementally); when set, it's cleared and a fresh reconcile pass runs
/// inline, on this same thread, before the loop resumes draining events.
///
/// The thread exits cleanly — dropping the `notify` watcher first, then the
/// store — as soon as `shutdown_rx` yields a message or its sender is
/// dropped (disconnected); either is treated as "shut down now". If the
/// watcher itself fails to start (can't register with the OS, or `root`
/// can't be watched), the store is marked `Degraded` + `needs_reconcile` and
/// the function returns immediately without entering the event loop.
fn run_writer(
    root: PathBuf,
    mut store: Store,
    embedder: Option<Arc<dyn Embedder>>,
    shutdown_rx: Receiver<()>,
    do_reconcile: bool,
) {
    // macOS's FSEvents backend (what `notify::recommended_watcher` picks on
    // this platform) reports paths through the OS's realpath, which
    // resolves symlinked ancestors — notably `/var` -> `/private/var`, the
    // very prefix `std::env::temp_dir()` (and so every tempdir-based test)
    // returns. Without this, `normalize_event_paths`'s `strip_prefix(root)`
    // would silently fail for every single event and nothing would ever
    // debounce. Canonicalizing once here keeps `root` consistent with what
    // events actually carry; if canonicalization fails (root vanished
    // before the watch even starts), fall back to the given path and let
    // `watcher.watch` below surface the real error.
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    if do_reconcile {
        if let Err(e) = crate::reconcile::reconcile(&mut store, embedder.as_deref(), &root) {
            eprintln!("vexus: startup reconcile failed ({e:#}); watching anyway");
        }
    }

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();

    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(_) => {
            mark_degraded(&mut store);
            return;
        }
    };
    if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
        mark_degraded(&mut store);
        return;
    }

    let mut debouncer = Debouncer::default();
    let mut tick: u64 = 0;

    loop {
        match shutdown_rx.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }

        match rx.recv_timeout(RECV_TIMEOUT) {
            Ok(Ok(event)) => {
                let now = Instant::now();
                for rel in normalize_event_paths(&root, &event) {
                    debouncer.push(rel, now);
                }
            }
            Ok(Err(_notify_err)) => mark_degraded(&mut store),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // The watcher's own sender is gone — nothing more will
                // ever arrive on `rx`. Flag it and stop the loop rather
                // than spin on an immediately-returning recv_timeout.
                mark_degraded(&mut store);
                break;
            }
        }

        drain_and_apply(
            &mut store,
            &mut debouncer,
            embedder.as_deref(),
            &root,
            Instant::now(),
        );

        tick += 1;
        if tick.is_multiple_of(NEEDS_RECONCILE_CHECK_EVERY) && needs_reconcile(&store) {
            let _ = store.delete_meta("needs_reconcile");
            if let Err(e) = crate::reconcile::reconcile(&mut store, embedder.as_deref(), &root) {
                eprintln!("vexus: flagged reconcile failed ({e:#}); still watching");
            }
        }
    }

    // Explicit per spec: drop the watcher (stop receiving OS events)
    // before the store, even though declaration order already implies
    // this drop order.
    drop(watcher);
    drop(store);
}

/// Spawn the watcher thread with no startup reconcile: it owns `store` (the
/// writer connection) for its entire life, watches `root` recursively via
/// `notify`, debounces raw events through [`Debouncer`], and applies ready
/// paths via `update_file`. See [`run_writer`] for the full behavior this
/// wraps (`do_reconcile: false`).
pub fn spawn_watcher(
    root: PathBuf,
    store: Store,
    embedder: Option<Arc<dyn Embedder>>,
    shutdown_rx: Receiver<()>,
) -> JoinHandle<()> {
    thread::spawn(move || run_writer(root, store, embedder, shutdown_rx, false))
}

/// Spawn the writer thread `vexus-mcp`'s `serve` uses: a startup
/// [`crate::reconcile::reconcile`] pass followed by the same watch loop
/// [`spawn_watcher`] runs, both against one writer `Store` on one thread.
/// See [`run_writer`] (`do_reconcile: true`) for the full behavior.
pub fn spawn_writer(
    root: PathBuf,
    store: Store,
    embedder: Option<Arc<dyn Embedder>>,
    shutdown_rx: Receiver<()>,
) -> JoinHandle<()> {
    thread::spawn(move || run_writer(root, store, embedder, shutdown_rx, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vexus_core::query::Resolution;

    #[test]
    fn watcher_indexes_a_new_file_and_marks_the_index_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.py"), "def helper():\n    return 1\n").unwrap();

        let db_path = root.join(".vexus/index.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            crate::pipeline::index_repo(&root, &mut store).unwrap();
        }

        let writer_store = Store::open(&db_path).unwrap();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let embedder: Arc<dyn Embedder> = Arc::new(vexus_embed::MockEmbedder);
        let handle = spawn_watcher(root.clone(), writer_store, Some(embedder), shutdown_rx);

        // Give the OS watch a moment to register before writing.
        thread::sleep(Duration::from_millis(200));
        let new_file = root.join("new_mod.py");
        std::fs::write(&new_file, "def brand_new_symbol():\n    return 42\n").unwrap();

        let start = Instant::now();
        let deadline = start + Duration::from_secs(5);
        let mut nudged = false;
        let mut found = false;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(150));

            // macOS FSEvents can be slow (or occasionally drop) the very
            // first event for a brand-new file; nudging by writing it again
            // partway through the poll window keeps this test from being
            // flaky without weakening what it actually asserts.
            if !nudged && start.elapsed() >= Duration::from_secs(1) {
                let _ = std::fs::write(
                    &new_file,
                    "def brand_new_symbol():\n    return 42\n    # nudge\n",
                );
                nudged = true;
            }

            let Ok(reader) = Store::open_read_only(&db_path) else {
                continue;
            };
            if let Ok(Resolution::Exact(_)) = reader.resolve_symbol("brand_new_symbol") {
                assert_eq!(
                    crate::freshness::effective_freshness(&reader).unwrap(),
                    Freshness::Fresh,
                    "index should be Fresh right after a successful drain"
                );
                found = true;
                break;
            }
        }

        drop(shutdown_tx);
        handle.join().unwrap();

        assert!(
            found,
            "watcher did not pick up new_mod.py's new symbol within 5s"
        );
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Carried finding (Task 4 review): a successful drain must not
    /// unconditionally stamp `Fresh` — only healing `Fresh`/`Degraded`. If a
    /// reconcile (or the initial index build) is in flight and has already
    /// set `Reconciling`/`Indexing`, a drain succeeding concurrently must
    /// leave that state alone so the reconcile pass still owns its own
    /// transition to `Fresh`/`Degraded` once *it* actually finishes.
    #[test]
    fn successful_drain_leaves_reconciling_and_indexing_state_alone() {
        for state in [Freshness::Reconciling, Freshness::Indexing] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, "a.py", "def helper():\n    return 1\n");

            let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
            crate::pipeline::index_repo(root, &mut store).unwrap();
            set_freshness(&mut store, state).unwrap();

            write(root, "a.py", "def helper():\n    return 2\n");
            let mut debouncer = Debouncer::default();
            let now = Instant::now();
            debouncer.push(PathBuf::from("a.py"), now);
            drain_and_apply(
                &mut store,
                &mut debouncer,
                None,
                root,
                now + crate::debounce::DEBOUNCE_WINDOW,
            );

            assert_eq!(
                get_freshness(&store).unwrap(),
                state,
                "a successful drain must not clobber {state:?} to Fresh"
            );
        }
    }

    /// The flip side: a successful drain over a `Fresh` or `Degraded` index
    /// does still (re)confirm/heal it to `Fresh` — this is the existing,
    /// intended healing behavior, unchanged.
    #[test]
    fn successful_drain_heals_fresh_and_degraded_to_fresh() {
        for state in [Freshness::Fresh, Freshness::Degraded] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, "a.py", "def helper():\n    return 1\n");

            let mut store = Store::open(&root.join(".vexus/index.db")).unwrap();
            crate::pipeline::index_repo(root, &mut store).unwrap();
            set_freshness(&mut store, state).unwrap();

            write(root, "a.py", "def helper():\n    return 2\n");
            let mut debouncer = Debouncer::default();
            let now = Instant::now();
            debouncer.push(PathBuf::from("a.py"), now);
            drain_and_apply(
                &mut store,
                &mut debouncer,
                None,
                root,
                now + crate::debounce::DEBOUNCE_WINDOW,
            );

            assert_eq!(
                get_freshness(&store).unwrap(),
                Freshness::Fresh,
                "a successful drain over {state:?} must heal it to Fresh"
            );
        }
    }

    #[test]
    fn needs_reconcile_reads_the_flag_and_treats_absence_as_false() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join(".vexus/index.db")).unwrap();
        assert!(!needs_reconcile(&store));

        store.set_meta("needs_reconcile", "1").unwrap();
        assert!(needs_reconcile(&store));

        store.delete_meta("needs_reconcile").unwrap();
        assert!(!needs_reconcile(&store));
    }
}
