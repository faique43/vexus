//! The writer thread wiring `serve_async` uses when this process wins the
//! advisory `.vexus/lock`: opens a dedicated writer `Store`, spawns a thread
//! that builds the index if there is none yet, loads the embedder and then
//! runs `vexus_watch::spawn_writer` (holding the `WriterLock` for that
//! thread's whole life), and hands back a [`WriterHandle`] whose `Drop`
//! signals shutdown and joins the thread.
//!
//! Pulled out of `serve_async` into its own testable unit after a real bug:
//! an earlier version's shutdown-channel sender was scoped to end *inside*
//! an `if is_writer { ... }` block, so it dropped — and so signalled
//! shutdown, since the watch loop's first `shutdown_rx.try_recv()` per tick
//! treats a disconnected sender exactly like an explicit shutdown — right at
//! that block's closing brace, long before `serve` itself was actually done.
//! The writer thread exited on its very first loop tick, having never
//! watched anything; reconcile (synchronous, runs before the loop) had
//! already finished by then, so `status` kept reporting `freshness: fresh`
//! while `last_event_at` never got stamped, no matter how long anything
//! waited. `WriterHandle` makes "hold it, the thread runs; drop it, the
//! thread stops" an invariant of the type itself, rather than something
//! every future caller has to get right by eye.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use vexus_core::Store;
use vexus_embed::Embedder;
use vexus_watch::{pipeline, set_freshness, Freshness, WriterLock};

/// Owns the writer thread's lifetime. While a `WriterHandle` is alive, the
/// writer thread (reconcile + filesystem watch, per
/// `vexus_watch::spawn_writer`) keeps running; dropping it signals shutdown
/// (by dropping the shutdown sender first) and joins the thread, so by the
/// time `drop` returns the thread — and the `WriterLock` it held — are
/// actually gone.
///
/// `#[must_use]`: binding the result of `start_writer` to `_` (or otherwise
/// dropping it immediately) would stop the writer right where it started,
/// which is never what a caller means to do.
#[must_use]
pub(crate) struct WriterHandle {
    // `Option` so `Drop::drop` (which only gets `&mut self`) can `.take()`
    // them out — `JoinHandle::join` takes `self` by value, so a plain field
    // can't be moved out of a `&mut self` receiver.
    shutdown_tx: Option<Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl WriterHandle {
    /// Whether the writer thread is still running. Test-only: production
    /// code has no legitimate reason to poll this — it should simply hold
    /// the handle for as long as the writer should run, and drop it when
    /// done.
    #[cfg(test)]
    fn is_running(&self) -> bool {
        self.join.as_ref().is_some_and(|j| !j.is_finished())
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        // Drop the sender FIRST — that's the real shutdown signal, per the
        // module doc above. Only then join, so a caller that drops a
        // `WriterHandle` can rely on the thread (and the advisory
        // `WriterLock` it was holding) being gone by the time this returns.
        self.shutdown_tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Opens a dedicated writer `Store` at `db_path`, spawns the writer thread
/// (holding `lock` for that thread's entire life), and returns a
/// [`WriterHandle`] for it. Callers must hold the returned handle for as
/// long as the writer should keep running — dropping it early stops the
/// writer immediately, however soon that is.
///
/// An empty index (a first run, or a schema bump that wiped the old one) is
/// built on that thread, not here: `serve` calls this before it answers the
/// MCP `initialize` handshake, and clients give up on a server that takes
/// too long to (Claude Code waits 30s). Only the `Indexing` mark happens
/// here, so a `status` call can never see a new, empty index as `fresh`.
///
/// The embedder is loaded on that thread too, by `load_embedder`, after any
/// structural build: on a machine that has never run vexus, loading it
/// downloads the ~160 MB model first. The index is queryable keyword-only
/// in the meantime, and the watcher embeds the backlog once it has loaded.
pub(crate) fn start_writer(
    root: PathBuf,
    db_path: &Path,
    load_embedder: impl FnOnce() -> Option<Arc<dyn Embedder>> + Send + 'static,
    lock: WriterLock,
) -> Result<WriterHandle> {
    let mut writer_store = Store::open(db_path)
        .with_context(|| format!("failed to open writer index at {}", db_path.display()))?;
    let first_build = writer_store.counts()?.files == 0;
    if first_build {
        let _ = set_freshness(&mut writer_store, Freshness::Indexing);
        eprintln!("vexus: no index found for {root:?} — building one in the background...");
    }
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let join = std::thread::spawn(move || {
        // Hold the WriterLock for the duration of this thread.
        let _lock = lock;
        if first_build {
            build_structural_index(&root, &mut writer_store);
        }
        let embedder = load_embedder();
        let inner_handle = vexus_watch::spawn_writer(root, writer_store, embedder, shutdown_rx);
        let _ = inner_handle.join();
    });
    Ok(WriterHandle {
        shutdown_tx: Some(shutdown_tx),
        join: Some(join),
    })
}

/// The first half of building an index from nothing: every file parsed and
/// its calls linked, without embeddings, so tools can answer as soon as this
/// returns rather than after minutes of embedding. What follows on the same
/// thread finishes the job: `spawn_writer`'s startup reconcile moves the
/// index from `Indexing` to `Fresh` (or `Degraded`), then its backlog drain
/// embeds every chunk, resumably.
///
/// A failure, or a panic, leaves whatever was indexed in place: the
/// reconcile that follows goes over every file anyway. Left to unwind, a
/// panic would end this thread before the watcher started, with the index
/// stuck at `Indexing` and every tool refusing to answer.
fn build_structural_index(root: &Path, store: &mut Store) {
    let built = std::panic::catch_unwind(AssertUnwindSafe(|| pipeline::index_repo(root, store)));
    match built {
        Ok(Ok(report)) => eprintln!(
            "vexus: indexed {} files ({} failed)",
            report.indexed,
            report.failed.len()
        ),
        Ok(Err(e)) => eprintln!("vexus: initial indexing failed ({e:#}); reconciling instead"),
        Err(_) => eprintln!("vexus: initial indexing panicked; reconciling instead"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Regression guard for the real bug this module exists to make
    /// impossible to reintroduce by accident (see the module doc comment):
    /// an earlier `serve_async` scoped its shutdown sender so it dropped —
    /// and so signalled shutdown — before `serve` was actually done,
    /// exiting the writer thread on its very first loop tick. This is
    /// FSEvents-independent and fast: it needs no real filesystem event, or
    /// even a registered OS-level watch, to prove the wiring itself (not
    /// just `vexus-watch`'s lower-level `spawn_watcher`/`spawn_writer`,
    /// which are exercised directly elsewhere) keeps the thread alive while
    /// the handle exists, and stops it once the handle is dropped.
    #[test]
    fn writer_handle_keeps_the_thread_alive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root, "a.py", "def helper():\n    return 1\n");

        let db_path = root.join(".vexus/index.db");
        {
            let mut store = vexus_core::Store::open(&db_path).unwrap();
            vexus_watch::pipeline::index_repo(&root, &mut store).unwrap();
        }

        let lock = WriterLock::try_acquire(&root)
            .unwrap()
            .expect("nothing else holds the lock in this fresh tempdir");
        let handle = start_writer(root.clone(), &db_path, || None, lock).unwrap();

        // No filesystem events at all during this window — the bug this
        // guards against would have exited the thread within its very
        // first ~100ms tick regardless, so 1.5s is a generous margin, not
        // a tight one.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            handle.is_running(),
            "writer thread must still be running while the WriterHandle is alive, \
             even with zero filesystem events — this is exactly what the pre-fix \
             shape (shutdown sender dropped inside `if is_writer {{ }}`) would fail"
        );

        drop(handle);
        // `Drop` itself joins the thread synchronously, so by the time
        // `drop(handle)` above returns, the thread is already gone — this
        // is really just double-checking `Drop`'s own join succeeded rather
        // than e.g. panicking or hanging.
    }

    /// `serve` calls `start_writer` before answering the MCP handshake, so an
    /// empty index must already be marked `Indexing` when this returns — a
    /// new database has no freshness yet, which reads as `fresh` — and then
    /// get built on the thread: structure, the startup reconcile, and every
    /// chunk embedded.
    #[test]
    fn start_writer_marks_an_empty_index_indexing_then_builds_it_on_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root, "a.py", "def helper():\n    return 1\n");

        let db_path = root.join(".vexus/index.db");
        drop(Store::open(&db_path).unwrap());
        let reader = Store::open_read_only(&db_path).unwrap();
        assert_eq!(reader.meta("freshness").unwrap(), None);

        let lock = WriterLock::try_acquire(&root)
            .unwrap()
            .expect("nothing else holds the lock in this fresh tempdir");
        let handle = start_writer(
            root.clone(),
            &db_path,
            || Some(Arc::new(vexus_embed::MockEmbedder)),
            lock,
        )
        .unwrap();

        assert!(
            reader.meta("freshness").unwrap().is_some(),
            "the empty index must be marked before start_writer returns"
        );
        let freshness = vexus_watch::get_freshness(&reader).unwrap();
        let files = reader.counts().unwrap().files;
        assert!(
            !(freshness == Freshness::Fresh && files == 0),
            "an empty index must never read as fresh"
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let freshness = vexus_watch::get_freshness(&reader).unwrap();
            let files = reader.counts().unwrap().files;
            let backlog = reader.embed_backlog().unwrap();
            if freshness == Freshness::Fresh && files == 1 && backlog == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never built: {freshness:?}, {files} files, {backlog} chunks unembedded"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(reader.meta("model_id").unwrap().as_deref(), Some("mock"));

        drop(handle);
    }
}
