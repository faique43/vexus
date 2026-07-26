//! The watcher task: a `notify` filesystem watch feeding the pure
//! [`Debouncer`](crate::debounce::Debouncer), draining ready paths through
//! [`update_file`](crate::update::update_file) on a std thread that owns the
//! writer [`Store`] for its entire lifetime.
//!
//! This is the only place in the crate that touches threads, channels, or
//! the OS filesystem-event API — everything it calls into (`Debouncer`,
//! `update_file`, `set_freshness`) is itself plain, synchronous, and already
//! unit-tested on its own.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{RecursiveMode, Watcher};
use vexus_core::Store;
use vexus_embed::Embedder;

use crate::debounce::Debouncer;
use crate::freshness::{get_freshness, set_freshness, Freshness};
use crate::pipeline::embed_pending;
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

/// Bundles the gitignore-filtering state `normalize_event_paths` and
/// `drain_and_apply` share across the whole life of the writer thread's
/// event loop — grouped into one struct (rather than three loose
/// parameters threaded through both functions) mainly to keep their
/// argument lists small, but it also keeps the three pieces that only ever
/// make sense *together* from drifting apart.
///
/// - `is_git_repo`: whether `root` is a git repo, checked once at watcher
///   start (item 1, P4 residual) — a root becoming (or ceasing to be) one
///   mid-run is out of scope for this fix.
/// - `gitignore`: a matcher for `root`'s own top-level `.gitignore` only —
///   the sole gitignore signal available for a non-git root, and the
///   fallback for a git root when `git_check_ignore` itself fails. Rebuilt
///   by `run_writer_inner` whenever a drained path is `.gitignore` itself.
/// - `check_ignore_broken_logged`: set the first (and only the first) time
///   `git_check_ignore` fails during this thread's life, so a persistently
///   broken `git` doesn't spam stderr once per debounce cycle forever.
struct GitignoreState {
    is_git_repo: bool,
    gitignore: Gitignore,
    check_ignore_broken_logged: bool,
}

/// Turn a raw `notify::Event`'s (absolute) paths into repo-relative,
/// forward-slash-normalized paths, dropping anything under `.vexus/` or
/// `.git/`, and anything outside `root` entirely. `Access` events are
/// ignored up front (no paths returned) — they fire on reads, not writes,
/// and carry no information `update_file` needs to act on.
///
/// Gitignore filtering itself does NOT happen here (item 1, P4 residual):
/// in a git repo, it's done once per drain batch via [`git_check_ignore`],
/// which covers nested `.gitignore`s, `.git/info/exclude`, and the global
/// excludesfile — all of which this per-event, root-`.gitignore`-only
/// matcher misses. For a non-git directory (no authoritative `git
/// check-ignore` to call), `gi.gitignore` is still applied right here,
/// since that's the only gitignore signal available at all for a non-git
/// root; see `drain_and_apply`'s `is_git_repo` branch for where the
/// git-repo case gets its own filtering instead.
fn normalize_event_paths(root: &Path, event: &notify::Event, gi: &GitignoreState) -> Vec<PathBuf> {
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
            let rel = PathBuf::from(rel);
            if !gi.is_git_repo
                && gi
                    .gitignore
                    .matched_path_or_any_parents(&rel, false)
                    .is_ignore()
            {
                return None;
            }
            Some(rel)
        })
        .collect()
}

/// Filters `paths` (repo-relative, forward-slash-normalized) through `git
/// check-ignore --stdin -z`, spawned once for the whole batch (rather than
/// once per path) — cheap enough per drain, and the only way to get the
/// *authoritative* answer `pipeline::index_repo`'s full walk relies on
/// (nested `.gitignore`s, `.git/info/exclude`, the global excludesfile),
/// none of which the watcher's own lightweight root-`.gitignore` matcher
/// covers. Paths are written to stdin NUL-separated (`-z` on both ends) so a
/// filename containing a newline can't corrupt the split.
///
/// Returns the ignored subset on success, or `None` if the subprocess itself
/// failed — `git` missing from `PATH`, `root` not a valid repository despite
/// having a `.git` entry, or any other non-{0,1} exit — so the caller can
/// fall back to the root-matcher uniformly rather than needing to
/// distinguish failure modes. Per `git check-ignore`'s documented exit-code
/// convention: `0` means at least one given path is ignored, `1` means none
/// are (routine, not a failure), anything else (notably `128`, a fatal
/// error) is the hard-failure case.
///
/// Writing happens on a separate thread from reading `stdout` — a batch
/// large enough that `git`'s own stdout fills its pipe buffer before this
/// process has finished writing every path to stdin would otherwise
/// deadlock (`git` blocked writing ignored paths we're not yet reading;
/// this process blocked writing more stdin `git` isn't yet reading either).
fn git_check_ignore(root: &Path, paths: &[PathBuf]) -> Option<HashSet<PathBuf>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("check-ignore")
        .arg("--stdin")
        .arg("-z")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdin = child.stdin.take()?;
    let payload: Vec<u8> = paths
        .iter()
        .flat_map(|p| {
            let mut bytes = p.to_string_lossy().replace('\\', "/").into_bytes();
            bytes.push(0);
            bytes
        })
        .collect();
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&payload);
        // `stdin` drops here, closing the pipe so `git` sees EOF.
    });

    let output = child.wait_with_output().ok()?;
    let _ = writer.join();

    match output.status.code() {
        Some(0) | Some(1) => Some(
            output
                .stdout
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(String::from_utf8_lossy(s).replace('\\', "/")))
                .collect(),
        ),
        _ => None,
    }
}

/// Builds a `.gitignore`-aware matcher for `root`'s own top-level
/// `.gitignore` file (not nested `.gitignore`s, `.git/info/exclude`, or the
/// global gitignore — narrower than `pipeline::walk_repo_relative_files`'s
/// full `ignore::WalkBuilder`, but covers the common case of a project-root
/// `.gitignore` for build output, dependency directories, etc.). Rebuilt
/// live by `run_writer` whenever a drained path is `.gitignore` itself, so
/// edits to it take effect without a restart.
///
/// A missing or unreadable `.gitignore` degrades to "nothing ignored"
/// rather than an error — the watcher must never refuse to run over this.
fn build_gitignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let gi_path = root.join(".gitignore");
    if gi_path.exists() {
        if let Some(e) = builder.add(&gi_path) {
            eprintln!("vexus: failed to parse {}: {e}", gi_path.display());
        }
    }
    builder.build().unwrap_or_else(|e| {
        eprintln!(
            "vexus: failed to build gitignore matcher for {}: {e}",
            root.display()
        );
        Gitignore::empty()
    })
}

/// After a reconcile pass, drain whatever's left of the embedding backlog —
/// chunks `reconcile`'s own per-file `update_file` calls never touch,
/// because `update_file` only re-embeds files it actually reindexed (a
/// content-hash change), never a backlog that predates this run entirely
/// (e.g. an index built via `vexus index` with `VEXUS_EMBEDDER=none`, or a
/// model switch since the DB was last written to — see finding I5). Guarded
/// so the common case — nothing to embed, same model already recorded —
/// costs one `embed_backlog` count and two `meta` reads, not a wasted
/// `set_model` transaction on every single reconcile pass.
fn drain_embed_backlog(store: &mut Store, embedder: Option<&dyn Embedder>) {
    let Some(embedder) = embedder else { return };
    let model_differs = store.meta("model_id").ok().flatten().as_deref() != Some(embedder.id())
        || store.meta("model_dim").ok().flatten().as_deref()
            != Some(embedder.dim().to_string().as_str());
    let backlog = store.embed_backlog().unwrap_or(0);
    if !model_differs && backlog <= 0 {
        return;
    }
    if let Err(e) = store.set_model(embedder.id(), embedder.dim()) {
        eprintln!("vexus: failed to record embedding model ({e:#})");
        return;
    }
    match embed_pending(store, embedder) {
        Ok(_) => {
            let _ = store.bump_generation();
        }
        Err(e) => eprintln!("vexus: embedding backlog drain failed ({e:#})"),
    }
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
///
/// Returns whatever it drained (possibly empty) so callers can react to
/// *which* paths just settled — `run_writer` uses this to notice a
/// `.gitignore` edit and rebuild its gitignore matcher (finding C2).
///
/// Item 1 (P4 residual): in a git repo (`is_git_repo`), the drained batch is
/// filtered through [`git_check_ignore`] before any of it reaches
/// `update_file` — the authoritative check `pipeline::index_repo`'s full
/// walk is already held to, covering nested `.gitignore`s,
/// `.git/info/exclude`, and the global excludesfile alike (none of which
/// `normalize_event_paths`'s lightweight root-matcher can see). If that
/// subprocess itself fails, this falls back to `gitignore` (the root-only
/// matcher) instead — logging the fallback exactly once via
/// `check_ignore_broken_logged` rather than on every single drain, so a
/// persistently broken `git` (missing from `PATH`, say) doesn't spam
/// stderr once per debounce cycle for the rest of this thread's life. A
/// non-git root skips this filtering step entirely: `normalize_event_paths`
/// already applied the root-matcher when the event first arrived, so
/// `ready` is already in scope.
fn drain_and_apply(
    store: &mut Store,
    debouncer: &mut Debouncer,
    embedder: Option<&dyn Embedder>,
    root: &Path,
    now: Instant,
    gi: &mut GitignoreState,
) -> Vec<PathBuf> {
    let ready = debouncer.drain_ready(now);
    if ready.is_empty() {
        return ready;
    }

    let to_apply: Vec<&PathBuf> = if gi.is_git_repo {
        match git_check_ignore(root, &ready) {
            Some(ignored) => ready.iter().filter(|p| !ignored.contains(*p)).collect(),
            None => {
                if !gi.check_ignore_broken_logged {
                    eprintln!(
                        "vexus: git check-ignore failed; falling back to the root .gitignore \
                         matcher until the next successful drain"
                    );
                    gi.check_ignore_broken_logged = true;
                }
                ready
                    .iter()
                    .filter(|p| {
                        !gi.gitignore
                            .matched_path_or_any_parents(p, false)
                            .is_ignore()
                    })
                    .collect()
            }
        }
    } else {
        ready.iter().collect()
    };

    let mut any_failed = false;
    for rel_path in &to_apply {
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
    ready
}

/// Whether `meta('needs_reconcile')` is currently set — a read error is
/// treated the same as "not set" (there's nothing more productive to do
/// about a meta-read failure here than wait for the next check).
fn needs_reconcile(store: &Store) -> bool {
    store.meta("needs_reconcile").ok().flatten().as_deref() == Some("1")
}

/// Best-effort extraction of a human-readable message from a
/// `catch_unwind` payload — `panic!("...")` with a `&'static str` or a
/// `String` (from `format!`/`panic!("{}", ...)`) covers the overwhelming
/// majority of real panics; anything else (a custom payload type from some
/// dependency) degrades to a generic label rather than failing to report at
/// all.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
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
/// The `notify` watch is registered *before* any startup reconcile runs
/// (finding I1) — `notify`'s channel (`mpsc::channel`, unbounded) means any
/// filesystem change landing during a (possibly slow, on a big repo)
/// reconcile pass simply queues up and gets drained once the loop starts,
/// rather than falling in a gap where nothing is watching yet. Watching
/// first, then reconciling, is what actually delivers "fully caught up, no
/// missed window before live updates take over" — reconciling first would
/// leave exactly that window open.
///
/// When `do_reconcile` is true, a [`crate::reconcile::reconcile`] pass runs
/// right after the watch is registered — freshness goes `Reconciling` for
/// its duration, `Fresh` or `Degraded` per its own outcome. A reconcile
/// failure is logged to stderr and does **not** stop the watch loop from
/// running anyway: a degraded-but-watched index can still heal
/// incrementally (or via the `needs_reconcile` flag below), whereas
/// refusing to watch at all over one failed reconcile would leave it stuck.
/// Either way, [`drain_embed_backlog`] runs right after (finding I5) so a
/// backlog reconcile's own per-file updates never touch — e.g. one left
/// over from an index built with embeddings disabled — still gets drained.
///
/// Once watching, every `NEEDS_RECONCILE_CHECK_EVERY`th loop iteration
/// checks `meta('needs_reconcile')` (set by [`mark_degraded`] when the
/// watcher itself hits a `notify` error it can't recover from
/// incrementally); when set, it's cleared and a fresh reconcile pass (plus
/// backlog drain) runs inline, on this same thread, before the loop resumes
/// draining events.
///
/// The thread exits cleanly — dropping the `notify` watcher first, then the
/// store — as soon as `shutdown_rx` yields a message or its sender is
/// dropped (disconnected); either is treated as "shut down now". If the
/// watcher itself fails to start (can't register with the OS, or `root`
/// can't be watched), the store is marked `Degraded` + `needs_reconcile` and
/// the function returns immediately without entering the event loop.
fn run_writer_inner(
    root: PathBuf,
    store: &mut Store,
    embedder: Option<Arc<dyn Embedder>>,
    shutdown_rx: Receiver<()>,
    do_reconcile: bool,
) {
    // Test-only fault injection (finding I3's regression test): lets a test
    // trigger a real panic inside the very body `run_writer`'s catch_unwind
    // wraps, without needing to find a real bug to panic on. No production
    // code path ever sets this key.
    #[cfg(test)]
    if store.meta("__test_force_panic").ok().flatten().as_deref() == Some("1") {
        panic!("test-injected writer panic");
    }

    // Item 3 (P4 residual): a fresh writer-thread start clears any
    // `last_event_at` left over from a previous run of this process (or a
    // previous `vexus serve` against the same DB) — it describes "the last
    // time THIS run's watcher applied a successful drain," which this run
    // hasn't done yet, so a stale timestamp from before would otherwise look
    // like a live event just happened. `last_index_failed` resets to `0` the
    // same way: whatever a prior run last recorded there no longer describes
    // anything this run has actually observed.
    let _ = store.delete_meta("last_event_at");
    let _ = store.set_meta("last_index_failed", "0");

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

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();

    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(_) => {
            mark_degraded(store);
            return;
        }
    };
    if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
        mark_degraded(store);
        return;
    }

    if do_reconcile {
        if let Err(e) = crate::reconcile::reconcile(store, embedder.as_deref(), &root) {
            eprintln!("vexus: startup reconcile failed ({e:#}); watching anyway");
        }
        drain_embed_backlog(store, embedder.as_deref());
    }

    // Item 1 (P4 residual): `is_git_repo` is checked once, here, rather than
    // per-drain — the brief's own scoping ("check once at watcher start"),
    // since a root becoming (or ceasing to be) a git repository mid-run is
    // out of scope for this fix.
    let mut gi = GitignoreState {
        is_git_repo: root.join(".git").exists(),
        gitignore: build_gitignore(&root),
        check_ignore_broken_logged: false,
    };
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
                for rel in normalize_event_paths(&root, &event, &gi) {
                    debouncer.push(rel, now);
                }
            }
            Ok(Err(_notify_err)) => mark_degraded(store),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // The watcher's own sender is gone — nothing more will
                // ever arrive on `rx`. Flag it and stop the loop rather
                // than spin on an immediately-returning recv_timeout.
                mark_degraded(store);
                break;
            }
        }

        let ready = drain_and_apply(
            store,
            &mut debouncer,
            embedder.as_deref(),
            &root,
            Instant::now(),
            &mut gi,
        );
        if ready
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"))
        {
            gi.gitignore = build_gitignore(&root);
        }

        tick += 1;
        if tick.is_multiple_of(NEEDS_RECONCILE_CHECK_EVERY) && needs_reconcile(store) {
            let _ = store.delete_meta("needs_reconcile");
            if let Err(e) = crate::reconcile::reconcile(store, embedder.as_deref(), &root) {
                eprintln!("vexus: flagged reconcile failed ({e:#}); still watching");
            }
            drain_embed_backlog(store, embedder.as_deref());
        }
    }

    // Explicit per spec: drop the watcher (stop receiving OS events)
    // before the store — the store itself lives in the caller (`run_writer`,
    // which owns it), so there's nothing further to drop here.
    drop(watcher);
}

/// Thin panic-catching shell around [`run_writer_inner`] (finding I3): an
/// unhandled panic anywhere in the writer thread's body used to be
/// completely invisible short of the process itself dying — `JoinHandle`'s
/// `Result` is dropped by every caller (`spawn_watcher`/`spawn_writer`
/// return the handle for lifetime management, not panic inspection), and
/// `serve` never joins until shutdown. Catching it here means the store
/// (kept in this function's own scope, borrowed rather than moved into the
/// possibly-panicking closure, so a panic can't take it down too) still
/// gets a best-effort [`mark_degraded`] — a store `rusqlite` has already
/// rolled any in-flight transaction back on unwind for, per the same
/// reasoning as `AppState::lock_store`'s poisoned-lock recovery — before
/// this thread quietly exits, so a reader sees `degraded`/`needs_reconcile`
/// instead of a permanently stale `fresh`.
fn run_writer(
    root: PathBuf,
    mut store: Store,
    embedder: Option<Arc<dyn Embedder>>,
    shutdown_rx: Receiver<()>,
    do_reconcile: bool,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_writer_inner(root, &mut store, embedder, shutdown_rx, do_reconcile)
    }));
    if let Err(payload) = result {
        mark_degraded(&mut store);
        eprintln!(
            "vexus: writer thread panicked ({}); index marked degraded",
            panic_message(&payload)
        );
    }
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

    /// Regression guard for a real bug in `vexus_mcp::serve_async`: its
    /// shutdown-channel sender was scoped to end at the close of an `if
    /// is_writer { ... }` block — dropping (and so disconnecting the
    /// receiver) long before `serve` itself was actually done. The main
    /// loop's very first `shutdown_rx.try_recv()`, checked before it ever
    /// touches the `notify` channel, treats a disconnected sender exactly
    /// like an explicit shutdown signal, so the writer thread exited on its
    /// first tick, having never watched anything — reconcile still
    /// completed fine, so `status` read `freshness: fresh` with `last
    /// event: none` forever, no matter how long a caller waited.
    ///
    /// Catching that class of bug doesn't need a real filesystem event (or
    /// even a registered OS-level watch, which is exactly what makes this
    /// fast and platform-independent, unlike the test above): it only needs
    /// to show that holding the sender keeps the thread alive with no
    /// events at all, and that dropping it stops the thread promptly.
    #[test]
    fn writer_thread_stays_alive_while_shutdown_sender_is_held_and_exits_once_dropped() {
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
        let handle = spawn_watcher(root, writer_store, None, shutdown_rx);

        // No filesystem events at all during this window — the bug this
        // guards against would exit the thread within its very first
        // ~100ms tick regardless, so 1.5s is a generous, not a tight, margin.
        thread::sleep(Duration::from_millis(1500));
        assert!(
            !handle.is_finished(),
            "writer thread must still be running while the shutdown sender is alive, \
             even with zero filesystem events"
        );

        drop(shutdown_tx);
        // The main loop polls shutdown_rx once per RECV_TIMEOUT (100ms)
        // tick; a few ticks' worth of margin covers scheduling jitter.
        for _ in 0..20 {
            if handle.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            handle.is_finished(),
            "writer thread must exit promptly once the shutdown sender is dropped"
        );
        handle.join().unwrap();
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// `git init -q` plus enough `user.email`/`user.name` config that a
    /// later commit (if a test needs one) wouldn't fail — mirrors
    /// `reconcile.rs`'s own real-git-repo test fixtures. Returns early
    /// (rather than panicking) if `git` itself isn't on `PATH`, matching how
    /// the existing `reconcile` tests skip gracefully in that environment.
    fn init_git_repo(root: &Path) -> bool {
        let git = |args: &[&str]| Command::new("git").arg("-C").arg(root).args(args).output();
        let Ok(init) = git(&["init", "-q"]) else {
            eprintln!("git not available on PATH; skipping");
            return false;
        };
        if !init.status.success() {
            eprintln!("git init failed; skipping");
            return false;
        }
        for args in [
            vec!["config", "user.email", "t@t.dev"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(git(&args).unwrap().status.success());
        }
        true
    }

    /// Item 1 (P4 residual): `git check-ignore --stdin -z` must correctly
    /// distinguish paths ignored via a root `.gitignore`, a *nested*
    /// `sub/.gitignore`, and `.git/info/exclude` from paths that aren't
    /// ignored at all — none of the watcher's own lightweight root-only
    /// matcher can see the nested or info/exclude cases, which is exactly
    /// why this batch call exists.
    #[test]
    fn git_check_ignore_drops_nested_and_info_exclude_ignored_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !init_git_repo(root) {
            return;
        }

        write(root, ".gitignore", "build/\n");
        write(root, "sub/.gitignore", "*.gen.py\n");
        std::fs::write(root.join(".git/info/exclude"), "excluded_by_info.py\n").unwrap();

        let paths = vec![
            PathBuf::from("a.py"),
            PathBuf::from("build/x.py"),
            PathBuf::from("sub/y.gen.py"),
            PathBuf::from("sub/z.py"),
            PathBuf::from("excluded_by_info.py"),
        ];
        let ignored =
            git_check_ignore(root, &paths).expect("git check-ignore must succeed in a real repo");

        assert!(ignored.contains(&PathBuf::from("build/x.py")));
        assert!(ignored.contains(&PathBuf::from("sub/y.gen.py")));
        assert!(ignored.contains(&PathBuf::from("excluded_by_info.py")));
        assert!(!ignored.contains(&PathBuf::from("a.py")));
        assert!(!ignored.contains(&PathBuf::from("sub/z.py")));
    }

    /// The exit-code contract `git_check_ignore` relies on: `1` (nothing in
    /// the batch is ignored) is routine, not a failure — it must still
    /// return `Some` (an empty set), never fall back to `None`.
    #[test]
    fn git_check_ignore_returns_an_empty_set_when_nothing_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !init_git_repo(root) {
            return;
        }
        write(root, ".gitignore", "build/\n");

        let ignored = git_check_ignore(root, &[PathBuf::from("a.py"), PathBuf::from("b.py")])
            .expect("exit code 1 (nothing ignored) must not be treated as a failure");
        assert!(ignored.is_empty());
    }

    /// The hard-failure side of the same contract: outside a git repository
    /// entirely, `git check-ignore` exits `128` (fatal) — `git_check_ignore`
    /// must surface that as `None` so its caller falls back to the
    /// root-matcher, rather than silently treating a `git` failure as "ok,
    /// nothing's ignored."
    #[test]
    fn git_check_ignore_returns_none_outside_a_git_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path(); // deliberately never git-init'd
        assert!(!root.join(".git").exists());

        let result = git_check_ignore(root, &[PathBuf::from("a.py")]);
        assert!(
            result.is_none(),
            "a hard git-check-ignore failure must signal None, not an empty ignore set"
        );
    }

    /// Item 1 (P4 residual), end to end: a *real* git repo with a nested
    /// `sub/.gitignore` — something the watcher's old per-event root-only
    /// matcher could never see — must still keep a live-created file under
    /// it out of the index, now that `drain_and_apply` routes git repos
    /// through `git_check_ignore` instead. A non-ignored sentinel file is
    /// also written so the assertion on the ignored one actually proves the
    /// watcher ran at all.
    #[test]
    fn watcher_honors_a_nested_gitignore_in_a_real_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        if !init_git_repo(&root) {
            return;
        }
        write(&root, "sub/.gitignore", "*.gen.py\n");
        write(&root, "a.py", "def helper():\n    return 1\n");

        let db_path = root.join(".vexus/index.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            crate::pipeline::index_repo(&root, &mut store).unwrap();
        }

        let writer_store = Store::open(&db_path).unwrap();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let handle = spawn_watcher(root.clone(), writer_store, None, shutdown_rx);

        // Give the OS watch a moment to register before writing.
        thread::sleep(Duration::from_millis(200));
        std::fs::write(
            root.join("sub/gadget.gen.py"),
            "def ignored_symbol():\n    return 1\n",
        )
        .unwrap();
        let sentinel = root.join("live_mod.py");
        std::fs::write(&sentinel, "def live_symbol():\n    return 2\n").unwrap();

        let start = Instant::now();
        let deadline = start + Duration::from_secs(5);
        let mut nudged = false;
        let mut sentinel_seen = false;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(150));

            // Same macOS FSEvents flakiness workaround used elsewhere in
            // this file.
            if !nudged && start.elapsed() >= Duration::from_secs(1) {
                let _ =
                    std::fs::write(&sentinel, "def live_symbol():\n    return 2\n    # nudge\n");
                nudged = true;
            }

            let Ok(reader) = Store::open_read_only(&db_path) else {
                continue;
            };
            if let Ok(Resolution::Exact(_)) = reader.resolve_symbol("live_symbol") {
                sentinel_seen = true;
                break;
            }
        }

        drop(shutdown_tx);
        handle.join().unwrap();

        assert!(
            sentinel_seen,
            "watcher did not pick up the non-ignored sentinel file within 5s"
        );

        let reader = Store::open_read_only(&db_path).unwrap();
        assert_eq!(
            reader.file_hash("sub/gadget.gen.py").unwrap(),
            None,
            "a file under a real git repo's nested .gitignore must never be indexed by the \
             live watcher, even though the old root-only matcher couldn't see it"
        );
        assert!(
            matches!(
                reader.resolve_symbol("ignored_symbol").unwrap(),
                Resolution::NotFound { .. }
            ),
            "the nested-gitignored file's own symbol must never resolve"
        );
    }

    /// Item 3 (P4 residual): a fresh writer-thread start must clear
    /// `last_event_at` and reset `last_index_failed` to `0`, even with zero
    /// filesystem events — both keys otherwise carry forward stale meaning
    /// from whatever a *previous* run of this process (or a previous `vexus
    /// serve` against the same DB) last wrote.
    #[test]
    fn run_writer_start_clears_last_event_at_and_resets_last_index_failed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root, "a.py", "def helper():\n    return 1\n");

        let db_path = root.join(".vexus/index.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            crate::pipeline::index_repo(&root, &mut store).unwrap();
            // Simulate stale state left over from a previous run.
            store.set_meta("last_event_at", "1234567890").unwrap();
            store.set_meta("last_index_failed", "7").unwrap();
        }

        let writer_store = Store::open(&db_path).unwrap();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let handle = spawn_watcher(root, writer_store, None, shutdown_rx);

        // No filesystem event needed: the reset happens unconditionally at
        // thread start, before the watch loop even begins — this just gives
        // the thread a moment to actually run that far.
        thread::sleep(Duration::from_millis(300));

        let reader = Store::open_read_only(&db_path).unwrap();
        assert_eq!(
            reader.meta("last_event_at").unwrap(),
            None,
            "a fresh writer-thread start must clear any stale last_event_at"
        );
        assert_eq!(
            reader.meta("last_index_failed").unwrap().as_deref(),
            Some("0"),
            "a fresh writer-thread start must reset last_index_failed to 0"
        );

        drop(shutdown_tx);
        handle.join().unwrap();
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
            let mut gi = GitignoreState {
                is_git_repo: false,
                gitignore: Gitignore::empty(),
                check_ignore_broken_logged: false,
            };
            drain_and_apply(
                &mut store,
                &mut debouncer,
                None,
                root,
                now + crate::debounce::DEBOUNCE_WINDOW,
                &mut gi,
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
            let mut gi = GitignoreState {
                is_git_repo: false,
                gitignore: Gitignore::empty(),
                check_ignore_broken_logged: false,
            };
            drain_and_apply(
                &mut store,
                &mut debouncer,
                None,
                root,
                now + crate::debounce::DEBOUNCE_WINDOW,
                &mut gi,
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

    /// Finding I2 regression: `set_freshness` used to restamp
    /// `freshness_since` on every call, even when the persisted state
    /// didn't actually change — so a watcher stuck calling `mark_degraded`
    /// repeatedly (e.g. a run of unrecoverable `notify` errors, all while
    /// already `Degraded`) would keep resetting the "how long has this been
    /// Degraded" clock back to zero, and `effective_freshness`'s
    /// Degraded -> Stale escalation could never actually fire in practice.
    #[test]
    fn repeated_mark_degraded_does_not_reset_the_since_clock() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join(".vexus/index.db")).unwrap();

        mark_degraded(&mut store);
        // Backdate `since` well past the Stale threshold, as if this
        // Degraded state has genuinely been sitting for a long time.
        let since = now_unix_secs() - 3600;
        store
            .set_meta("freshness_since", &since.to_string())
            .unwrap();

        // A second `mark_degraded` call, "0s apart" per the finding — same
        // state (Degraded) — must NOT restamp `since` back to now.
        mark_degraded(&mut store);

        assert_eq!(
            crate::freshness::effective_freshness(&store).unwrap(),
            Freshness::Stale,
            "a repeated same-state mark_degraded must not reset freshness_since, \
             or Degraded could never age into Stale"
        );
    }

    /// Finding C2: a full `vexus index` run (`pipeline::index_repo`, via
    /// `ignore::WalkBuilder`) has always honored `.gitignore`; the live
    /// watcher didn't, so a gitignored file created while the watcher was
    /// running would get indexed even though a subsequent full reindex would
    /// immediately remove it again — the two views of "what's in the index"
    /// permanently disagreeing. A non-ignored sentinel file is also written
    /// so the assertion on the ignored one actually proves the watcher ran
    /// at all, rather than passing vacuously because nothing was ever
    /// applied.
    #[test]
    fn watcher_never_indexes_a_gitignored_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(root.join("a.py"), "def helper():\n    return 1\n").unwrap();

        let db_path = root.join(".vexus/index.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            crate::pipeline::index_repo(&root, &mut store).unwrap();
        }

        let writer_store = Store::open(&db_path).unwrap();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let handle = spawn_watcher(root.clone(), writer_store, None, shutdown_rx);

        // Give the OS watch a moment to register before writing.
        thread::sleep(Duration::from_millis(200));
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(
            root.join("build/x.py"),
            "def ignored_symbol():\n    return 1\n",
        )
        .unwrap();
        let sentinel = root.join("live_mod.py");
        std::fs::write(&sentinel, "def live_symbol():\n    return 2\n").unwrap();

        let start = Instant::now();
        let deadline = start + Duration::from_secs(5);
        let mut nudged = false;
        let mut sentinel_seen = false;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(150));

            // Same macOS FSEvents flakiness workaround as the test above.
            if !nudged && start.elapsed() >= Duration::from_secs(1) {
                let _ =
                    std::fs::write(&sentinel, "def live_symbol():\n    return 2\n    # nudge\n");
                nudged = true;
            }

            let Ok(reader) = Store::open_read_only(&db_path) else {
                continue;
            };
            if let Ok(Resolution::Exact(_)) = reader.resolve_symbol("live_symbol") {
                sentinel_seen = true;
                break;
            }
        }

        drop(shutdown_tx);
        handle.join().unwrap();

        assert!(
            sentinel_seen,
            "watcher did not pick up the non-ignored sentinel file within 5s"
        );

        let reader = Store::open_read_only(&db_path).unwrap();
        assert_eq!(
            reader.file_hash("build/x.py").unwrap(),
            None,
            "a gitignored file written under a live watcher must never be indexed"
        );
        assert!(
            matches!(
                reader.resolve_symbol("ignored_symbol").unwrap(),
                Resolution::NotFound { .. }
            ),
            "the gitignored file's own symbol must never resolve"
        );
    }

    /// Finding I3: an unhandled panic anywhere in the writer thread's body
    /// used to be completely invisible — the `JoinHandle` (and its `Result`)
    /// is never inspected by any caller, and `serve` doesn't join until
    /// shutdown — so a reader would keep seeing whatever freshness state was
    /// last written, forever, with no watcher actually alive behind it. Uses
    /// the `__test_force_panic` fault-injection hook (see `run_writer_inner`)
    /// to trigger a real panic deterministically, rather than needing to
    /// find/exploit an actual bug.
    #[test]
    fn writer_panic_marks_the_index_degraded_and_the_thread_exits_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.py"), "def helper():\n    return 1\n").unwrap();

        let db_path = root.join(".vexus/index.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            crate::pipeline::index_repo(&root, &mut store).unwrap();
            set_freshness(&mut store, Freshness::Fresh).unwrap();
            store.set_meta("__test_force_panic", "1").unwrap();
        }

        let writer_store = Store::open(&db_path).unwrap();
        let (_shutdown_tx, shutdown_rx) = mpsc::channel();
        let handle = spawn_watcher(root, writer_store, None, shutdown_rx);

        // `run_writer`'s `catch_unwind` absorbs the injected panic, so the
        // OS thread itself finishes normally — `join()` must return `Ok`,
        // not propagate the panic to this test.
        handle.join().expect(
            "run_writer's catch_unwind must absorb the injected panic; the \
             OS thread itself must not panic",
        );

        let reader = Store::open_read_only(&db_path).unwrap();
        assert_eq!(
            get_freshness(&reader).unwrap(),
            Freshness::Degraded,
            "a writer-thread panic must leave the index marked Degraded, not silently Fresh"
        );
        assert_eq!(
            reader.meta("needs_reconcile").unwrap().as_deref(),
            Some("1"),
            "a writer-thread panic must flag needs_reconcile so a later reconcile heals it"
        );
    }
}
