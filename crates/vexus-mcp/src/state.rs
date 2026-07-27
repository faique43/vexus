//! Shared server state: one `Store` behind a `Mutex` (rusqlite's `Connection`
//! is `Send` but not `Sync`, so tool handlers take the lock inside
//! `spawn_blocking` rather than holding a `&Store` across an `.await`), plus
//! a lazily-built embedder shared for the life of the process.
//!
//! `store` is `Mutex<Option<Store>>`, not `Mutex<Store>` (finding C3): a
//! reader process that lost the advisory writer-lock race may start up
//! before the winner has finished building the index for the very first
//! time, so `index.db` might not exist yet when this process's own reader
//! connection would otherwise open. Rather than fail `serve` outright over
//! that race, `lib.rs`'s `serve_async` hands this `AppState` a `None` in
//! that case and spawns a background thread that keeps retrying until the
//! winner's index appears, filling the `Option` in once it does. Every tool
//! already funnels through `lock_store_fresh`, so `None` becomes one
//! `Err(INDEX_NOT_READY)` text response at a single call site rather than a
//! crash.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use anyhow::Result;
use vexus_embed::Embedder;
use vexus_watch::{effective_freshness, role_line, Freshness};
#[cfg(test)]
use vexus_watch::{pipeline, set_freshness};

/// What every tool returns verbatim (see `AppState::lock_store_fresh`)
/// while this process's store isn't populated yet — a transient condition
/// (the winner of the advisory writer lock is still building the index for
/// the first time), not a real failure, so the message tells a caller to
/// just retry rather than to give up.
pub const INDEX_NOT_READY: &str =
    "index not ready — another vexus serve is building it; retry shortly";

pub struct AppState {
    pub store: Mutex<Option<vexus_core::Store>>,
    pub embedder: OnceLock<Option<Arc<dyn Embedder>>>,
    pub root: PathBuf,
    /// Last `Store::generation()` this `AppState` observed. Compared against
    /// the store's current generation on every `lock_store_fresh` call so a
    /// writer elsewhere (the watcher, in a later task) bumping the generation
    /// is noticed and cached derived state is invalidated accordingly.
    pub last_generation: AtomicU64,
    /// Whether this process is the writer (owns the advisory lock) or a reader.
    pub is_writer: bool,
}

/// A `MutexGuard<Option<Store>>` known to be holding `Some` — the invariant
/// `lock_store_fresh` establishes before ever constructing one. Derefs
/// straight to `&Store`, so every existing call site (`let store =
/// state.lock_store_fresh(); ...; store.some_method(...)`) keeps working
/// unchanged past the one added `?`/`match` for the new `Result`, rather
/// than every tool needing its own `.as_ref().unwrap()` on the `Option`.
pub struct StoreGuard<'a>(MutexGuard<'a, Option<vexus_core::Store>>);

impl std::ops::Deref for StoreGuard<'_> {
    type Target = vexus_core::Store;
    fn deref(&self) -> &vexus_core::Store {
        self.0
            .as_ref()
            .expect("StoreGuard is only ever constructed over a Some")
    }
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
    pub fn lock_store(&self) -> MutexGuard<'_, Option<vexus_core::Store>> {
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
    /// `Err(INDEX_NOT_READY)` (finding C3) when the store isn't populated
    /// yet — see this module's doc comment — rather than panicking or
    /// blocking; every tool call site already matches on this `Result` and
    /// returns the message straight through.
    ///
    /// All tool call sites should use this instead of the raw `lock_store`.
    pub fn lock_store_fresh(&self) -> Result<StoreGuard<'_>, String> {
        let guard = self.lock_store();
        if guard.is_none() {
            return Err(INDEX_NOT_READY.to_string());
        }
        let last = self.last_generation.load(Ordering::Relaxed);
        let current = guard.as_ref().unwrap().generation().unwrap_or(last);
        if current != last {
            guard.as_ref().unwrap().clear_caches();
            self.last_generation.store(current, Ordering::Relaxed);
        }
        Ok(StoreGuard(guard))
    }

    /// Renders the `status` tool's plain-text report. Kept as a plain method
    /// on `AppState` (rather than inline in the tool handler) so it's
    /// directly unit-testable without going through the MCP transport.
    /// Delegates to the free `status_text` function below — the single
    /// source both the MCP `status` tool and the CLI's `vexus status`
    /// command render through, so CLI/MCP parity is structural rather than
    /// two hand-copied format strings that can drift. Unlike the other 6
    /// tools, `status` treats "not ready yet" (finding C3) as its own
    /// complete (successful) answer rather than an error — a caller asking
    /// "what's the state of the index" gets exactly that, even when the
    /// answer is "not built yet".
    pub fn status_text(&self) -> Result<String> {
        let store = match self.lock_store_fresh() {
            Ok(store) => store,
            Err(msg) => return Ok(msg),
        };
        status_text(&store, Some(self.is_writer))
    }
}

/// Renders the exact `status` shape shared by the MCP `status` tool and the
/// CLI's `vexus status` command:
///
/// ```text
/// index: {n} files, {n} symbols, {n} edges, {n} chunks
/// model: {id|none}  embed backlog: {n}  vec: {available|unavailable}
/// freshness: {state} (since {rfc3339}){ hint when not fresh }
/// role: {writer|reader (another vexus serve owns the index)}
/// last event: {rfc3339|none}
/// skipped files: {n}                  # only when >0
/// ```
///
/// `role` is `Some(is_writer)` — the caller's own outcome from probing the
/// advisory `.vexus/lock` — when the `role:` line applies, `None` to omit
/// it entirely. This function only renders the result, it never touches the
/// lock itself.
///
/// The `role:` line belongs to an actual in-`serve` process, which is why
/// the MCP `status` tool always passes `Some` (it holds the lock, or knows
/// it lost the race to whatever does, for as long as the process runs) while
/// the CLI's one-shot `vexus status` passes `None`: acquiring the lock just
/// to print a `status` line and releasing it a moment later isn't "being the
/// writer" in any sense a caller should rely on — a real `vexus serve`
/// started microseconds later would legitimately win the race instead. The
/// CLI renders its own `serve: running|not running` line from that same
/// probe instead (see `vexus-cli`'s `Cmd::Status` handler) — a claim about
/// whether a server is running, not about this one-shot command's own role.
pub fn status_text(store: &vexus_core::Store, role: Option<bool>) -> Result<String> {
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

    // Unlike the `⚠ index ...` header (which prepends to the other 6
    // tools' responses), `status` is the one place that always shows the
    // real state plainly — even when Fresh — since checking freshness is
    // exactly what a caller reaches for this tool to do.
    let freshness = effective_freshness(store)?;
    let since = store
        .meta("freshness_since")?
        .and_then(|v| v.parse::<u64>().ok());
    let mut freshness_line = format!("freshness: {}", freshness.as_str());
    if let Some(since) = since {
        freshness_line.push_str(&format!(" (since {})", epoch_to_rfc3339(since)));
    }
    if freshness != Freshness::Fresh {
        freshness_line
            .push_str(" — results may miss recent changes; re-run 'vexus index' after big changes if this persists");
    }

    // `last_event_at` is stamped by the watcher (`vexus-watch`'s
    // `drain_and_apply`) on every drain that applied at least one change
    // cleanly — absent means the watcher hasn't completed a successful
    // drain yet this run (or isn't running at all, e.g. reader mode).
    let last_event = store
        .meta("last_event_at")?
        .and_then(|v| v.parse::<u64>().ok())
        .map(epoch_to_rfc3339)
        .unwrap_or_else(|| "none".into());

    let mut lines = vec![
        format!(
            "index: {} files, {} symbols, {} edges, {} chunks",
            c.files, c.symbols, c.edges, c.chunks
        ),
        format!("model: {model_id}  embed backlog: {backlog}  vec: {vec_status}"),
        freshness_line,
    ];
    if let Some(is_writer) = role {
        if let Some(role) = role_line(is_writer) {
            lines.push(role);
        }
    }
    lines.push(format!("last event: {last_event}"));
    if failed > 0 {
        lines.push(format!("skipped files: {failed}"));
    }
    Ok(lines.join("\n"))
}

/// Formats a unix-epoch second count as an RFC 3339 UTC timestamp
/// (`YYYY-MM-DDTHH:MM:SSZ`) by hand — `status_text` is the only call site in
/// the workspace, so pulling in `chrono` for one conversion isn't worth the
/// dependency. Turns the day count into a proleptic-Gregorian year/month/day
/// triple via Howard Hinnant's civil-from-days algorithm
/// (<http://howardhinnant.github.io/date_algorithms.html>); the unit tests
/// below cross-check every vector against `date -u -r <secs>`.
fn epoch_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// The `⚠ index {state}{detail} — results may miss recent changes` warning
/// line prepended (as `header + "\n\n"`) to every non-`status` tool's
/// response when the index isn't `Fresh`. `None` on `Fresh` — the common case — so call sites skip
/// the prepend entirely rather than concatenating an empty string.
///
/// `detail` is ` ({done}/{total} files)` when the state is `Reconciling` and
/// `meta('reconcile_progress')` (written by the reconcile pass as
/// `"done/total"`) is present — or, when that reconcile pass has also
/// flagged `meta('reconcile_bulk')` (finding I7: crossed
/// `reconcile::BULK_REINDEX_THRESHOLD` changed files), ` (bulk reindex
/// {done}/{total} files)` instead, so a caller can tell "catching up on a
/// couple of edits" apart from "this is a large structural change, expect
/// the warning to stick around for a while". Empty otherwise.
pub fn freshness_header(store: &vexus_core::Store) -> Option<String> {
    let state = effective_freshness(store).unwrap_or(Freshness::Degraded);
    if state == Freshness::Fresh {
        return None;
    }
    let detail = if state == Freshness::Reconciling {
        store
            .meta("reconcile_progress")
            .ok()
            .flatten()
            .map(|progress| {
                let is_bulk = store.meta("reconcile_bulk").ok().flatten().as_deref() == Some("1");
                if is_bulk {
                    format!(" (bulk reindex {progress} files)")
                } else {
                    format!(" ({progress} files)")
                }
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let state_str = state.as_str();
    Some(format!(
        "⚠ index {state_str}{detail} — results may miss recent changes"
    ))
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
            store: Mutex::new(Some(store)),
            embedder: OnceLock::new(),
            root: root.to_path_buf(),
            last_generation: AtomicU64::new(0),
            is_writer: true,
        }
    }

    /// Exact-format regression test for the final `status` shape (
    /// shape: index counts / model+backlog+vec / real freshness line / role
    /// / last event / optional skipped-files line), built from the store's
    /// own counts so the assertion tracks real indexed content rather than a
    /// hardcoded guess at tree-sitter's symbol/chunk output for this
    /// fixture. `indexed_state` never calls `set_freshness`, so this store
    /// is a "nothing has ever touched freshness" DB — `effective_freshness`
    /// reads that as `Fresh`, with no `since` suffix and no re-run hint. It
    /// also never runs the watcher, so `last_event_at` is absent -> `none`.
    #[test]
    fn status_text_matches_exact_line_format_with_no_failures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let state = indexed_state(root);
        let c = state
            .store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .counts()
            .unwrap();
        assert_eq!(
            c.files, 1,
            "sanity: exactly one file was indexed for this fixture"
        );

        let text = state.status_text().unwrap();
        let expected = format!(
            "index: {} files, {} symbols, {} edges, {} chunks\n\
             model: mock  embed backlog: 0  vec: available\n\
             freshness: fresh\n\
             role: writer\n\
             last event: none",
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
            .as_mut()
            .unwrap()
            .set_meta("last_index_failed", "2")
            .unwrap();

        let text = state.status_text().unwrap();
        assert!(text.contains("\nrole: writer\n"), "got: {text:?}");
        assert!(text.contains("\nlast event: none\n"), "got: {text:?}");
        assert!(
            text.ends_with("\nskipped files: 2"),
            "skipped-files line should be last: {text:?}"
        );
    }

    /// The `status` shape's writer/reader role split, at the `status_text`
    /// level (the underlying `role_line` string itself is already exercised
    /// directly in `vexus-watch`'s `lock.rs` tests).
    #[test]
    fn status_text_shows_reader_role_line_when_not_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let mut state = indexed_state(root);
        state.is_writer = false;

        let text = state.status_text().unwrap();
        assert!(
            text.contains("\nrole: reader (another vexus serve owns the index)\n"),
            "got: {text:?}"
        );
    }

    /// `role: None` (what the CLI's one-shot `vexus status` passes — see
    /// the free `status_text` fn's doc comment for why a bare command isn't
    /// entitled to claim a `role:` of its own) omits the line entirely,
    /// rather than rendering a bogus default.
    #[test]
    fn status_text_omits_role_line_when_role_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let state = indexed_state(root);
        let guard = state.store.lock().unwrap();
        let store = guard.as_ref().unwrap();
        let text = status_text(store, None).unwrap();

        assert!(!text.contains("role:"), "got: {text:?}");
        let expected = format!(
            "index: {} files, {} symbols, {} edges, {} chunks\n\
             model: mock  embed backlog: 0  vec: available\n\
             freshness: fresh\n\
             last event: none",
            store.counts().unwrap().files,
            store.counts().unwrap().symbols,
            store.counts().unwrap().edges,
            store.counts().unwrap().chunks,
        );
        assert_eq!(text, expected);
    }

    /// `last event:` reads `meta('last_event_at')` (stamped by the
    /// watcher's `drain_and_apply` per successful drain — see
    /// `vexus-watch`'s `watcher.rs`) and renders it as RFC 3339, not the raw
    /// unix-epoch string.
    #[test]
    fn status_text_renders_last_event_at_as_rfc3339_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let state = indexed_state(root);
        state
            .store
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .set_meta("last_event_at", "1753488000")
            .unwrap();

        let text = state.status_text().unwrap();
        assert!(
            text.contains("\nlast event: 2025-07-26T00:00:00Z"),
            "got: {text:?}"
        );
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
            store: Mutex::new(Some(reader)),
            embedder: OnceLock::new(),
            root: root.to_path_buf(),
            last_generation: AtomicU64::new(0),
            is_writer: true,
        };
        {
            let store = state.lock_store_fresh().unwrap();
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
        let store = state.lock_store_fresh().unwrap();
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

    /// Finding C3: an `AppState` whose store isn't populated yet (the
    /// reader/lock-loser path, before the winner's first index build has
    /// produced `index.db`) must degrade every tool call to a plain text
    /// error rather than panicking or blocking — `lock_store_fresh` is the
    /// single funnel every tool goes through, so this is the one place that
    /// guarantee needs to hold.
    #[test]
    fn lock_store_fresh_errs_with_index_not_ready_when_store_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            store: Mutex::new(None),
            embedder: OnceLock::new(),
            root: dir.path().to_path_buf(),
            last_generation: AtomicU64::new(0),
            is_writer: false,
        };

        let err = match state.lock_store_fresh() {
            Ok(_) => panic!("a None store must not yield a StoreGuard"),
            Err(msg) => msg,
        };
        assert_eq!(err, INDEX_NOT_READY);
    }

    /// The `status` tool specifically must still answer (successfully) with
    /// that same text, rather than surfacing it as a tool error the way the
    /// other 6 tools do — `status_text` is exactly the tool a caller reaches
    /// for to ask "what's going on with the index".
    #[test]
    fn status_text_reports_index_not_ready_as_a_successful_answer_when_store_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            store: Mutex::new(None),
            embedder: OnceLock::new(),
            root: dir.path().to_path_buf(),
            last_generation: AtomicU64::new(0),
            is_writer: false,
        };

        assert_eq!(state.status_text().unwrap(), INDEX_NOT_READY);
    }

    #[test]
    fn status_text_shows_real_non_fresh_state_with_since_and_hint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let state = indexed_state(root);
        {
            let mut guard = state.store.lock().unwrap();
            set_freshness(guard.as_mut().unwrap(), Freshness::Degraded).unwrap();
        }

        let text = state.status_text().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // freshness is always the third line: index, model, freshness, ...
        let freshness_line = lines[2];
        assert!(
            freshness_line.starts_with("freshness: degraded (since "),
            "got: {freshness_line:?}"
        );
        assert!(
            freshness_line.contains("Z)"),
            "since must be rendered as an RFC 3339 UTC timestamp, not a raw \
             unix-epoch number: {freshness_line:?}"
        );
        assert!(
            freshness_line.contains("re-run 'vexus index'"),
            "non-fresh state must keep the re-run hint: {freshness_line:?}"
        );
    }

    #[test]
    fn status_text_omits_the_rerun_hint_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");

        let state = indexed_state(root);
        {
            let mut guard = state.store.lock().unwrap();
            set_freshness(guard.as_mut().unwrap(), Freshness::Fresh).unwrap();
        }

        let text = state.status_text().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // freshness is always the third line: index, model, freshness, ...
        let freshness_line = lines[2];
        assert!(
            freshness_line.starts_with("freshness: fresh (since "),
            "got: {freshness_line:?}"
        );
        assert!(
            !freshness_line.contains("re-run 'vexus index'"),
            "Fresh must not carry the re-run hint: {freshness_line:?}"
        );
    }

    #[test]
    fn freshness_header_is_none_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        assert_eq!(freshness_header(&store), None);
    }

    #[test]
    fn freshness_header_exact_text_for_each_non_fresh_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();

        for (state, expected) in [
            (
                Freshness::Indexing,
                "⚠ index indexing — results may miss recent changes",
            ),
            (
                Freshness::Reconciling,
                "⚠ index reconciling — results may miss recent changes",
            ),
            (
                Freshness::Degraded,
                "⚠ index degraded — results may miss recent changes",
            ),
            (
                Freshness::Stale,
                "⚠ index stale — results may miss recent changes",
            ),
        ] {
            set_freshness(&mut store, state).unwrap();
            assert_eq!(
                freshness_header(&store).as_deref(),
                Some(expected),
                "state {state:?}"
            );
        }
    }

    #[test]
    fn freshness_header_includes_reconcile_progress_detail_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        set_freshness(&mut store, Freshness::Reconciling).unwrap();
        store.set_meta("reconcile_progress", "50/200").unwrap();

        assert_eq!(
            freshness_header(&store).as_deref(),
            Some("⚠ index reconciling (50/200 files) — results may miss recent changes")
        );
    }

    /// Finding I7: once a reconcile pass has also flagged
    /// `meta('reconcile_bulk')`, the progress detail must say "bulk reindex"
    /// rather than the plain `(50/200 files)` form.
    #[test]
    fn freshness_header_labels_a_bulk_reconcile_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        set_freshness(&mut store, Freshness::Reconciling).unwrap();
        store.set_meta("reconcile_progress", "50/500").unwrap();
        store.set_meta("reconcile_bulk", "1").unwrap();

        assert_eq!(
            freshness_header(&store).as_deref(),
            Some(
                "⚠ index reconciling (bulk reindex 50/500 files) — results may miss recent changes"
            )
        );
    }

    #[test]
    fn freshness_header_omits_progress_detail_for_non_reconciling_states() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        set_freshness(&mut store, Freshness::Degraded).unwrap();
        // Stale `reconcile_progress` left over from an earlier reconcile
        // pass must not leak into an unrelated state's header.
        store.set_meta("reconcile_progress", "50/200").unwrap();

        assert_eq!(
            freshness_header(&store).as_deref(),
            Some("⚠ index degraded — results may miss recent changes")
        );
    }

    #[test]
    fn freshness_header_reflects_effective_freshness_not_raw_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        set_freshness(&mut store, Freshness::Degraded).unwrap();
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
        store
            .set_meta("freshness_since", &since.to_string())
            .unwrap();

        assert_eq!(
            freshness_header(&store).as_deref(),
            Some("⚠ index stale — results may miss recent changes"),
            "a long-Degraded store must render the header as Stale, not Degraded"
        );
    }

    /// Every vector here was cross-checked against `date -u -r <secs>
    /// +%Y-%m-%dT%H:%M:%SZ` on macOS before being hardcoded — not just
    /// hand-derived, to verify rather than assume. Covers the epoch, a
    /// recent date, a leap day, a non-midnight time crossing a
    /// year/month/day boundary (1999-12-31 -> 2000-01-01 in UTC, one second
    /// before that), and the mp>=10 branch of the civil-from-days algorithm
    /// (November/December, and January/February needing the `y+1`
    /// adjustment).
    #[test]
    fn epoch_to_rfc3339_matches_verified_date_u_output() {
        for (secs, expected) in [
            (0u64, "1970-01-01T00:00:00Z"),
            (1_753_488_000, "2025-07-26T00:00:00Z"),
            (1_709_208_000, "2024-02-29T12:00:00Z"),
            (978_307_199, "2000-12-31T23:59:59Z"),
            (2_147_483_647, "2038-01-19T03:14:07Z"),
        ] {
            assert_eq!(epoch_to_rfc3339(secs), expected, "secs = {secs}");
        }
    }
}
