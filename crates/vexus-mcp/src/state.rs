//! Shared server state: one `Store` behind a `Mutex` (rusqlite's `Connection`
//! is `Send` but not `Sync`, so tool handlers take the lock inside
//! `spawn_blocking` rather than holding a `&Store` across an `.await`), plus
//! a lazily-built embedder shared for the life of the process.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use anyhow::Result;
use vexus_embed::Embedder;
#[cfg(test)]
use vexus_watch::pipeline;

pub struct AppState {
    pub store: Mutex<vexus_core::Store>,
    pub embedder: OnceLock<Option<Arc<dyn Embedder>>>,
    pub root: PathBuf,
    /// Last `Store::generation()` this `AppState` observed. Compared against
    /// the store's current generation on every `lock_store_fresh` call so a
    /// writer elsewhere (the watcher, in a later task) bumping the generation
    /// is noticed and cached derived state is invalidated accordingly.
    pub last_generation: AtomicU64,
}

impl AppState {
    /// Lazily builds the embedder on first use (via the same
    /// `VEXUS_EMBEDDER`-driven selection the CLI uses), then reuses it for
    /// the rest of the process — never rebuilt per call. A build failure
    /// degrades to `None` (structural-only search); `make_embedder` already
    /// prints the reason to stderr.
    pub fn embedder(&self) -> Option<Arc<dyn Embedder>> {
        self.embedder
            .get_or_init(|| vexus_embed::select::make_embedder().map(Arc::from))
            .clone()
    }

    /// Locks the store, recovering the guard even if a previous holder
    /// panicked mid-transaction. Safe because rusqlite rolls a transaction
    /// back on unwind — a poisoned lock here means "some earlier call
    /// panicked", not "the store is corrupt" — so bricking every subsequent
    /// tool call over it would turn one bad request into a dead server.
    pub fn lock_store(&self) -> MutexGuard<'_, vexus_core::Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Locks the store (via `lock_store`) and ensures cached derived state is
    /// coherent with it: reads the store's generation counter and compares
    /// it against the last one this `AppState` observed. On a change (a
    /// writer elsewhere bumped it since our last look), clears the store's
    /// caches before handing back the guard, so a reader that cached e.g.
    /// "no vec table" before the writer created one doesn't keep reporting
    /// that stale answer. `Ordering::Relaxed` suffices — the mutex guard
    /// already provides the synchronization; the atomic here only needs to
    /// be consistent with itself across calls on the same `AppState`, not to
    /// order with unrelated memory operations.
    ///
    /// All tool call sites should use this instead of the raw `lock_store`.
    pub fn lock_store_fresh(&self) -> MutexGuard<'_, vexus_core::Store> {
        let guard = self.lock_store();
        let last = self.last_generation.load(Ordering::Relaxed);
        let current = guard.generation().unwrap_or(last);
        if current != last {
            guard.clear_caches();
            self.last_generation.store(current, Ordering::Relaxed);
        }
        guard
    }

    /// Renders the `status` tool's plain-text report. Kept as a plain method
    /// on `AppState` (rather than inline in the tool handler) so it's
    /// directly unit-testable without going through the MCP transport.
    pub fn status_text(&self) -> Result<String> {
        let store = self.lock_store_fresh();
        let c = store.counts()?;
        let model_id = store.meta("model_id")?.unwrap_or_else(|| "none".into());
        let backlog = store.embed_backlog()?;
        let vec_status = if store.vec_available() {
            "available"
        } else {
            "unavailable"
        };
        let failed: i64 = store
            .meta("last_index_failed")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut lines = vec![
            format!(
                "index: {} files, {} symbols, {} edges, {} chunks",
                c.files, c.symbols, c.edges, c.chunks
            ),
            format!(
                "model: {model_id}  embed backlog: {backlog}  vec: {vec_status}"
            ),
            "freshness: static (watcher lands in a future release — re-run 'vexus index' after big changes)".to_string(),
        ];
        if failed > 0 {
            lines.push(format!("skipped files: {failed}"));
        }
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn indexed_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        pipeline::index_repo(root, &mut store).unwrap();
        let embedder = vexus_embed::MockEmbedder;
        store.set_model(embedder.id(), embedder.dim()).unwrap();
        pipeline::embed_pending(&mut store, &embedder).unwrap();
        AppState {
            store: Mutex::new(store),
            embedder: OnceLock::new(),
            root: root.to_path_buf(),
            last_generation: AtomicU64::new(0),
        }
    }

    /// Exact-format regression test per the Task 3 brief: the four line
    /// shapes (index counts / model+backlog+vec / static freshness line /
    /// optional skipped-files line), built from the store's own counts so
    /// the assertion tracks real indexed content rather than a hardcoded
    /// guess at tree-sitter's symbol/chunk output for this fixture.
    #[test]
    fn status_text_matches_exact_line_format_with_no_failures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let state = indexed_state(root);
        let c = state.store.lock().unwrap().counts().unwrap();
        assert_eq!(
            c.files, 1,
            "sanity: exactly one file was indexed for this fixture"
        );

        let text = state.status_text().unwrap();
        let expected = format!(
            "index: {} files, {} symbols, {} edges, {} chunks\n\
             model: mock  embed backlog: 0  vec: available\n\
             freshness: static (watcher lands in a future release — re-run 'vexus index' after big changes)",
            c.files, c.symbols, c.edges, c.chunks
        );
        assert_eq!(text, expected);
        assert!(
            !text.contains("skipped files"),
            "no failures last run -> no skipped-files line"
        );
    }

    #[test]
    fn status_text_appends_skipped_files_line_when_last_index_had_failures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let state = indexed_state(root);
        // Simulate a prior run that failed on 2 files (index_repo persists
        // this via Store::set_meta; exercised end-to-end in
        // vexus-embed's pipeline tests — here we only check AppState reads it).
        state
            .store
            .lock()
            .unwrap()
            .set_meta("last_index_failed", "2")
            .unwrap();

        let text = state.status_text().unwrap();
        assert!(text.ends_with("\nskipped files: 2"), "got: {text:?}");
    }

    /// Exercises `lock_store_fresh` itself (as opposed to vexus-core's lower
    /// level test of the same mechanism): a reader `AppState` opened while
    /// `vec_chunks` doesn't exist yet caches "absent"; a separate writer
    /// connection then creates the table, embeds a chunk, and bumps the
    /// generation. The next `lock_store_fresh` call must notice the bump and
    /// clear the stale cache so the reader's own `Store` sees the change —
    /// this is the actual coherence path tools rely on, not a simulation.
    #[test]
    fn lock_store_fresh_clears_stale_vec_cache_after_a_writer_bumps_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let db_path = root.join(".vexus/index.db");
        let mut writer = vexus_core::Store::open(&db_path).unwrap();
        pipeline::index_repo(root, &mut writer).unwrap();

        // AppState's reader connection opens before any model is set, so
        // vec_chunks doesn't exist yet.
        let reader = vexus_core::Store::open_read_only(&db_path).unwrap();
        let state = AppState {
            store: Mutex::new(reader),
            embedder: OnceLock::new(),
            root: root.to_path_buf(),
            last_generation: AtomicU64::new(0),
        };
        {
            let store = state.lock_store_fresh();
            assert!(
                !store.vec_table_exists().unwrap(),
                "vec table shouldn't exist yet"
            );
        }

        // Writer creates vec_chunks, embeds, and bumps the generation.
        let embedder = vexus_embed::MockEmbedder;
        writer.set_model(embedder.id(), embedder.dim()).unwrap();
        pipeline::embed_pending(&mut writer, &embedder).unwrap();
        writer.bump_generation().unwrap();

        // lock_store_fresh must notice the generation change and clear the
        // reader's stale cache before returning the guard.
        let store = state.lock_store_fresh();
        assert!(
            store.vec_table_exists().unwrap(),
            "lock_store_fresh must clear the stale cache after the generation bump"
        );
        let query = embedder.embed(&["def f(): return 1"]).unwrap().remove(0);
        assert!(
            !store.knn_chunks(&query, 5).unwrap().is_empty(),
            "reader must be able to see the writer's embeddings via KNN"
        );
    }

    #[test]
    fn embedder_is_built_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::env::set_var("VEXUS_EMBEDDER", "mock");
        let state = indexed_state(root);
        let first = state.embedder().expect("mock embedder always builds");
        let second = state.embedder().expect("second call reuses the OnceLock");
        assert_eq!(first.id(), "mock");
        assert!(
            Arc::ptr_eq(&first, &second),
            "embedder() must return the same Arc after the first call, not rebuild"
        );
    }
}
