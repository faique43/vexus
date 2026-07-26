use std::path::Path;

use anyhow::{bail, Result};
use rusqlite::{Connection, OpenFlags};

pub const SCHEMA_VERSION: &str = "1";
const SCHEMA: &str = include_str!("schema.sql");

static VEC_INIT: std::sync::Once = std::sync::Once::new();

/// Signature of a SQLite extension entry point (what `sqlite3_auto_extension`
/// actually expects). `sqlite_vec::sqlite3_vec_init` is declared with no
/// arguments in the `sqlite-vec` crate (it's only ever called indirectly by
/// SQLite through this pointer), so we transmute through this explicit type
/// rather than relying on an inferred one.
type VecExtensionEntryPoint = unsafe extern "C" fn(
    db: *mut rusqlite::ffi::sqlite3,
    pz_err_msg: *mut *mut std::os::raw::c_char,
    p_api: *const rusqlite::ffi::sqlite3_api_routines,
) -> std::os::raw::c_int;

/// Register the sqlite-vec extension process-wide via `sqlite3_auto_extension`
/// so every connection opened anywhere in this binary (including other tests
/// in the same test binary) picks up `vec0`/`vec_version()`. Must run before
/// any `Connection::open`; idempotent via `Once`.
fn register_sqlite_vec() {
    VEC_INIT.call_once(|| unsafe {
        let init_fn: unsafe extern "C" fn() = sqlite_vec::sqlite3_vec_init;
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            unsafe extern "C" fn(),
            VecExtensionEntryPoint,
        >(init_fn)));
    });
}

fn f32s_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn blob_to_f32s(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// `files.id` for `path`, or `None` if it isn't indexed. Takes a bare
/// `&Connection` (rather than `&Store`) so it can run either against
/// `self.conn` directly or against an in-progress `Transaction` (which
/// derefs to `Connection`) — `replace_file` needs the latter, to read the
/// old file's id before its own transaction deletes it.
fn file_id_for_path(conn: &Connection, path: &str) -> Result<Option<i64>> {
    conn.query_row("SELECT id FROM files WHERE path = ?1", [path], |r| r.get(0))
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })
        .map_err(Into::into)
}

/// Distinct `symbols.name` values for a given `file_id`.
fn symbol_names_for_file_id(conn: &Connection, file_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT name FROM symbols WHERE file_id = ?1")?;
    let rows = stmt.query_map([file_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub struct Store {
    pub(crate) conn: Connection,
    vec_available: bool,
    vec_table_cached: std::cell::Cell<Option<bool>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        register_sqlite_vec();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        if !Self::version_ok(&conn) {
            drop(conn);
            Self::remove_db_files(path)?;
            conn = Connection::open(path)?;
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Finding I8: without this, two writers racing for the same SQLite
        // lock (e.g. a `vexus serve` writer thread and a concurrently
        // launched `vexus index`) get an immediate `SQLITE_BUSY` instead of
        // a bounded wait — 5s is generous enough to ride out any single
        // write transaction this codebase does, without hanging indefinitely
        // if something is actually stuck.
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Self::init_if_empty(&conn)?;
        let vec_available = conn
            .query_row("SELECT vec_version()", [], |_| Ok(()))
            .is_ok();
        Ok(Self {
            conn,
            vec_available,
            vec_table_cached: std::cell::Cell::new(None),
        })
    }

    /// Opens an existing index read-only: no schema init, no corrupt-rebuild
    /// — a missing or corrupt DB is an error the caller decides how to
    /// handle, rather than something this constructor silently repairs (that
    /// asymmetry is the point: only a writer is allowed to create/heal the
    /// file). sqlite-vec is still registered so KNN reads work; write
    /// methods called on the resulting `Store` return the underlying
    /// "attempt to write a readonly database" sqlite error — tools never
    /// call them, so no read-only guard is needed on every method.
    ///
    /// Item 5 (P4 residual): unlike `open`, this constructor can never
    /// rebuild a version-mismatched DB (only a writer is allowed to touch
    /// the file at all) — so instead, right after opening, it compares
    /// `meta('schema_version')` against this build's `SCHEMA_VERSION` and
    /// errs if they differ, rather than letting a reader silently run
    /// queries against a schema shape it doesn't actually understand (wrong
    /// column set, missing table, ...) and surface some unrelated, confusing
    /// SQL error on the first real query instead.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        register_sqlite_vec();
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Finding I8: a read-only connection can still hit SQLITE_BUSY
        // establishing its WAL read snapshot while a writer holds the lock
        // mid-transaction — worth a bounded wait here too, same as `open`.
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let vec_available = conn
            .query_row("SELECT vec_version()", [], |_| Ok(()))
            .is_ok();
        let store = Self {
            conn,
            vec_available,
            vec_table_cached: std::cell::Cell::new(None),
        };
        if store.meta("schema_version")?.as_deref() != Some(SCHEMA_VERSION) {
            bail!("index built by an incompatible vexus version — re-run 'vexus index'");
        }
        Ok(store)
    }

    /// Whether the sqlite-vec extension is loaded in this process. When
    /// false, all vec-related methods become no-ops / empty results rather
    /// than erroring, so callers can run without vector search support.
    pub fn vec_available(&self) -> bool {
        self.vec_available
    }

    /// Test-only hook to exercise the `!vec_available` degrade branches.
    /// sqlite-vec is statically linked in this workspace, so `vec_available`
    /// is always true in practice; this lets tests simulate the extension
    /// being missing without needing a differently-built binary.
    #[cfg(test)]
    pub(crate) fn force_vec_unavailable(&mut self) {
        self.vec_available = false;
    }

    /// Test-only escape hatch to access the connection directly for
    /// simulating edge cases (e.g., deleting rows to trigger race conditions).
    #[cfg(test)]
    pub(crate) fn conn_ref_for_tests(&self) -> &rusqlite::Connection {
        &self.conn
    }

    pub fn vec_table_exists(&self) -> Result<bool> {
        if let Some(cached) = self.vec_table_cached.get() {
            return Ok(cached);
        }
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'vec_chunks'",
            [],
            |r| r.get(0),
        )?;
        let exists = n > 0;
        self.vec_table_cached.set(Some(exists));
        Ok(exists)
    }

    fn vec_table_exists_uncached(conn: &Connection) -> Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'vec_chunks'",
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Create the `vec_chunks` virtual table if it doesn't exist yet.
    /// No-op when sqlite-vec isn't available or the table already exists.
    pub fn ensure_vec_table(&mut self, dim: usize) -> Result<()> {
        if !self.vec_available {
            return Ok(());
        }
        if self.vec_table_exists()? {
            return Ok(());
        }
        self.conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE vec_chunks USING vec0(chunk_id INTEGER PRIMARY KEY, embedding FLOAT[{dim}])"
        ))?;
        self.vec_table_cached.set(Some(true));
        Ok(())
    }

    /// Record the active embedding model. On a change from the previously
    /// recorded `model_id`/`model_dim` (or if none was recorded yet), wipes
    /// the embed cache and the vec table (dimensions may differ between
    /// models) and recreates `vec_chunks` for the new dimension. Returns
    /// whether a wipe happened.
    ///
    /// Bumps `generation` (finding I6) whenever `vec_chunks` is actually
    /// created or recreated — either branch below can do that (the
    /// changed-model path always does; the unchanged path only if
    /// `ensure_vec_table` finds the table missing, e.g. sqlite-vec just
    /// became available) — so a reader's `lock_store_fresh` notices and
    /// clears its cached "no vec table" answer instead of continuing to
    /// report it after the writer just created one.
    pub fn set_model(&mut self, model_id: &str, dim: usize) -> Result<bool> {
        let current_id = self.meta("model_id")?;
        let current_dim = self.meta("model_dim")?;
        let unchanged = current_id.as_deref() == Some(model_id)
            && current_dim.as_deref() == Some(&*dim.to_string());
        if unchanged {
            let existed = self.vec_table_exists()?;
            self.ensure_vec_table(dim)?;
            if !existed && self.vec_table_exists()? {
                self.bump_generation()?;
            }
            return Ok(false);
        }
        let vec_available = self.vec_available;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM embed_cache", [])?;
        tx.execute("DROP TABLE IF EXISTS vec_chunks", [])?;
        if vec_available {
            tx.execute_batch(&format!(
                "CREATE VIRTUAL TABLE vec_chunks USING vec0(chunk_id INTEGER PRIMARY KEY, embedding FLOAT[{dim}])"
            ))?;
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('model_id', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [model_id],
        )?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('model_dim', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [dim.to_string()],
        )?;
        tx.commit()?;
        self.vec_table_cached.set(Some(vec_available));
        self.bump_generation()?;
        Ok(true)
    }

    /// `(chunk_id, content, content_hash)` for chunks with no `vec_chunks`
    /// row yet. Empty when vec is unavailable.
    pub fn chunks_missing_embedding(&self, limit: u32) -> Result<Vec<(i64, String, Vec<u8>)>> {
        if !self.vec_available {
            return Ok(vec![]);
        }
        let sql = if self.vec_table_exists()? {
            "SELECT c.id, c.content, c.content_hash FROM chunks c
             LEFT JOIN vec_chunks v ON v.chunk_id = c.id
             WHERE v.chunk_id IS NULL LIMIT ?1"
        } else {
            "SELECT c.id, c.content, c.content_hash FROM chunks c LIMIT ?1"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn embed_cache_get(&self, hash: &[u8]) -> Result<Option<Vec<f32>>> {
        let v: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT embedding FROM embed_cache WHERE content_hash = ?1",
                [hash],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(v.map(|b| blob_to_f32s(&b)))
    }

    pub fn embed_cache_put(&mut self, hash: &[u8], v: &[f32]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO embed_cache (content_hash, embedding) VALUES (?1, ?2)",
            rusqlite::params![hash, f32s_to_blob(v)],
        )?;
        Ok(())
    }

    /// Total rows currently in `embed_cache` — a small utility for callers
    /// (tests, `status`-style diagnostics) that want a cheap sense of the
    /// cache's size without reaching for a bespoke query.
    pub fn embed_cache_len(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM embed_cache", [], |r| r.get(0))?)
    }

    /// Deletes every `embed_cache` row whose `content_hash` no longer
    /// belongs to any current `chunks` row. `embed_cache` is keyed purely by
    /// content hash — no foreign key back to a chunk or file — so an edit or
    /// deletion that changes a chunk's content (and so its hash) leaves the
    /// old cached embedding behind with nothing else ever cleaning it up; a
    /// full `index_repo` run (see `pipeline::index_repo`) is the natural
    /// point to sweep those, since by the time it finishes every chunk the
    /// repo currently has is already known. Returns the number of rows
    /// removed, so the caller can log a non-zero prune without needing a
    /// separate count query.
    pub fn prune_orphaned_embed_cache(&mut self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM embed_cache WHERE content_hash NOT IN \
             (SELECT DISTINCT content_hash FROM chunks)",
            [],
        )?)
    }

    /// Upsert embeddings for chunks in one transaction. No-op when vec is
    /// unavailable. If `vec_chunks` doesn't exist yet (e.g. `set_model` was
    /// never called), it's created using the dimension of the first row,
    /// matching how `chunks_missing_embedding`/`embed_backlog` already treat
    /// a missing table as "nothing embedded yet" rather than an error.
    pub fn put_embeddings(&mut self, rows: &[(i64, Vec<f32>)]) -> Result<()> {
        if !self.vec_available || rows.is_empty() {
            return Ok(());
        }
        if !self.vec_table_exists()? {
            self.ensure_vec_table(rows[0].1.len())?;
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
            )?;
            for (chunk_id, v) in rows {
                stmt.execute(rusqlite::params![chunk_id, f32s_to_blob(v)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// `(chunk_id, distance)` nearest neighbors, ordered closest-first. Empty
    /// when vec is unavailable or `vec_chunks` doesn't exist yet.
    pub fn knn_chunks(&self, query: &[f32], k: u32) -> Result<Vec<(i64, f64)>> {
        if !self.vec_available || !self.vec_table_exists()? {
            return Ok(vec![]);
        }
        let mut stmt = self.conn.prepare(
            "SELECT chunk_id, distance FROM vec_chunks WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance",
        )?;
        let rows = stmt.query_map(rusqlite::params![f32s_to_blob(query), k], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Count of chunks lacking a `vec_chunks` row. When vec is unavailable,
    /// no chunk can ever have an embedding, so the true backlog is every
    /// chunk in the index — NOT 0 (0 would misreport a structural-only
    /// index as fully embedded).
    pub fn embed_backlog(&self) -> Result<i64> {
        if !self.vec_available {
            return Ok(self
                .conn
                .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?);
        }
        let n: i64 = if self.vec_table_exists()? {
            self.conn.query_row(
                "SELECT count(*) FROM chunks c
                 LEFT JOIN vec_chunks v ON v.chunk_id = c.id
                 WHERE v.chunk_id IS NULL",
                [],
                |r| r.get(0),
            )?
        } else {
            self.conn
                .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?
        };
        Ok(n)
    }

    /// Delete the DB file plus its WAL/SHM sidecars, ignoring "not found" errors.
    fn remove_db_files(path: &Path) -> Result<()> {
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let mut shm = path.as_os_str().to_owned();
        shm.push("-shm");
        for p in [path.as_os_str().to_owned(), wal, shm] {
            if let Err(e) = std::fs::remove_file(Path::new(&p)) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    /// The index is a disposable cache: any error probing the schema (corrupt
    /// or truncated DB file, unreadable `meta` table, etc.) is treated as
    /// "not ok" so `open` falls through to delete-and-rebuild rather than
    /// propagating the error and leaving the store permanently unusable.
    fn version_ok(conn: &Connection) -> bool {
        let has_meta: Result<i64, rusqlite::Error> = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'meta'",
            [],
            |r| r.get(0),
        );
        let has_meta = match has_meta {
            Ok(n) => n,
            Err(_) => return false,
        };
        if has_meta == 0 {
            return true; // fresh file, nothing to mismatch
        }
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .ok();
        v.as_deref() == Some(SCHEMA_VERSION)
    }

    fn init_if_empty(conn: &Connection) -> Result<()> {
        let has_meta: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'meta'",
            [],
            |r| r.get(0),
        )?;
        if has_meta == 0 {
            conn.execute_batch(SCHEMA)?;
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                [SCHEMA_VERSION],
            )?;
        }
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(v)
    }

    /// Generic single-key upsert into `meta`, for callers that need to persist
    /// one fact without a bespoke method (e.g. `index_repo` recording the
    /// last run's failed-file count under `last_index_failed`).
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Removes `key` from `meta` entirely (as opposed to overwriting it with
    /// some sentinel value) — for transient, presence-means-something-is-
    /// happening keys like `reconcile_progress`, where a stale leftover
    /// value read after the fact would otherwise need every reader to also
    /// know to ignore it based on some other state. A no-op (not an error)
    /// when `key` was already absent.
    pub fn delete_meta(&mut self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Monotonically increasing counter bumped by a writer on any change
    /// that could invalidate a reader's cached state (currently:
    /// `vec_table_cached`). Absent (fresh DB, or written by a build predating
    /// this field) reads as 0, so a never-bumped store never looks "changed"
    /// to a reader whose baseline also starts at 0.
    pub fn generation(&self) -> Result<u64> {
        Ok(self
            .meta("index_generation")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    /// Increments and persists the generation counter. Callers (e.g. the
    /// watcher, or startup indexing) invoke this after any write a reader's
    /// cache needs to know about; readers compare it against their own
    /// last-seen value (see `vexus-mcp`'s `lock_store_fresh`) and call
    /// `clear_caches` on a mismatch.
    pub fn bump_generation(&mut self) -> Result<()> {
        let next = self.generation()? + 1;
        self.set_meta("index_generation", &next.to_string())
    }

    /// Resets cached derived state (currently `vec_table_cached`) so the next
    /// read re-probes the schema instead of trusting a possibly-stale cache.
    /// Called by a reader after it observes `generation()` has moved past
    /// its last-seen value.
    pub fn clear_caches(&self) {
        self.vec_table_cached.set(None);
    }

    pub fn file_hash(&self, path: &str) -> Result<Option<[u8; 32]>> {
        let v: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT hash FROM files WHERE path = ?1", [path], |r| {
                r.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(v.map(|b| {
            let mut h = [0u8; 32];
            h.copy_from_slice(&b);
            h
        }))
    }

    /// Distinct symbol `name`s currently indexed for `path`, or empty if the
    /// file isn't indexed at all. For callers that need to know what a file
    /// used to define *before* removing it (its rows are about to disappear
    /// and won't be queryable afterward) — in particular
    /// `vexus_watch::update::update_file`'s "gone from disk" and "no longer
    /// supported" branches, which must target `resolve_edges_for_names` at
    /// the old names since `remove_file` itself doesn't report them.
    pub fn symbol_names_for_file(&self, path: &str) -> Result<Vec<String>> {
        match file_id_for_path(&self.conn, path)? {
            Some(file_id) => symbol_names_for_file_id(&self.conn, file_id),
            None => Ok(Vec::new()),
        }
    }

    pub fn replace_file(
        &mut self,
        path: &str,
        lang: &str,
        hash: &[u8; 32],
        idx: &crate::model::FileIndex,
    ) -> Result<(i64, Vec<String>)> {
        use crate::model::estimate_tokens;
        use anyhow::anyhow;
        let vec_available = self.vec_available;
        let tx = self.conn.transaction()?;

        // Capture the old symbol names before the delete cascade below wipes
        // them, so the caller can re-resolve exactly the names that changed
        // (removed + added) via `resolve_edges_for_names` instead of a full
        // `resolve_all_edges` sweep (spec §2 decision 2: no global recompute
        // on a single-file update).
        let old_names: Vec<String> = match file_id_for_path(&tx, path)? {
            Some(old_file_id) => symbol_names_for_file_id(&tx, old_file_id)?,
            None => Vec::new(),
        };

        // vec_chunks is a virtual table with no FK cascade, so its rows for
        // this file's chunks must be cleaned up explicitly before the old
        // file row (and its chunks, via ON DELETE CASCADE) disappears.
        if vec_available && Self::vec_table_exists_uncached(&tx)? {
            tx.execute(
                "DELETE FROM vec_chunks WHERE chunk_id IN (
                     SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id WHERE f.path = ?1
                 )",
                [path],
            )?;
        }

        tx.execute("DELETE FROM files WHERE path = ?1", [path])?;
        tx.execute(
            "INSERT INTO files (path, lang, hash, indexed_at) VALUES (?1, ?2, ?3, unixepoch())",
            rusqlite::params![path, lang, hash.as_slice()],
        )?;
        let file_id = tx.last_insert_rowid();

        // Insert symbols in batch order; parents always precede children in the batch
        // (parser guarantee), so the id map is complete when we need it.
        let mut ids: Vec<i64> = Vec::with_capacity(idx.symbols.len());
        for s in &idx.symbols {
            let parent_id = match s.parent {
                Some(i) => Some(*ids.get(i).ok_or_else(|| anyhow!("symbol parent index {} out of range (batch has {} symbols inserted so far)", i, ids.len()))?),
                None => None,
            };
            tx.execute(
                "INSERT INTO symbols (file_id, name, qualname, kind, sig, start_line, end_line, parent_id, arity)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    file_id, s.name, s.qualname, s.kind.as_str(), s.sig,
                    s.start_line, s.end_line, parent_id, s.arity
                ],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        for e in &idx.edges {
            let src_id = *ids.get(e.src).ok_or_else(|| {
                anyhow!(
                    "edge src index {} out of range (batch has {} symbols)",
                    e.src,
                    ids.len()
                )
            })?;
            tx.execute(
                "INSERT INTO edges (src_id, kind, dst_name, dst_arity, dst_id, confidence)
                 VALUES (?1, ?2, ?3, ?4, NULL, 'name_only')",
                rusqlite::params![src_id, e.kind.as_str(), e.dst_name, e.dst_arity],
            )?;
        }
        for c in &idx.chunks {
            let symbol_id = match c.symbol {
                Some(i) => Some(*ids.get(i).ok_or_else(|| {
                    anyhow!(
                        "chunk symbol index {} out of range (batch has {} symbols)",
                        i,
                        ids.len()
                    )
                })?),
                None => None,
            };
            let content_hash = blake3::hash(c.content.as_bytes());
            tx.execute(
                "INSERT INTO chunks (file_id, symbol_id, start_line, end_line, content, content_hash, token_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    file_id, symbol_id, c.start_line, c.end_line,
                    c.content, content_hash.as_bytes().as_slice(), estimate_tokens(&c.content)
                ],
            )?;
        }
        tx.commit()?;

        // Touched names = union of the old (removed) and new (just-inserted
        // batch) symbol names, deduped — this is exactly the target set for
        // `resolve_edges_for_names` after a single-file update.
        let mut touched = old_names.clone();
        let mut seen: std::collections::HashSet<String> = old_names.into_iter().collect();
        for s in &idx.symbols {
            if seen.insert(s.name.clone()) {
                touched.push(s.name.clone());
            }
        }

        Ok((file_id, touched))
    }

    pub fn remove_file(&mut self, path: &str) -> Result<bool> {
        let vec_available = self.vec_available;
        let tx = self.conn.transaction()?;

        // Same reasoning as replace_file: vec_chunks has no FK cascade, so
        // clean it up before the chunks disappear, otherwise a stale
        // embedding row can later collide with a reused chunk rowid.
        if vec_available && Self::vec_table_exists_uncached(&tx)? {
            tx.execute(
                "DELETE FROM vec_chunks WHERE chunk_id IN (
                     SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id WHERE f.path = ?1
                 )",
                [path],
            )?;
        }

        let n = tx.execute("DELETE FROM files WHERE path = ?1", [path])?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn counts(&self) -> Result<crate::model::Counts> {
        let one = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        Ok(crate::model::Counts {
            files: one("SELECT count(*) FROM files")?,
            symbols: one("SELECT count(*) FROM symbols")?,
            edges: one("SELECT count(*) FROM edges")?,
            chunks: one("SELECT count(*) FROM chunks")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_schema_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION
        );
        // core tables exist
        for t in ["files", "symbols", "edges", "chunks", "embed_cache"] {
            let n: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {t}");
        }
    }

    /// Finding I8: both `open` and `open_read_only` must set a real
    /// `busy_timeout` — otherwise a writer and reader (or two writers)
    /// racing for the same SQLite lock get an immediate `SQLITE_BUSY`
    /// instead of a bounded wait.
    #[test]
    fn open_and_open_read_only_both_set_a_nonzero_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let writer = Store::open(&db).unwrap();
        let timeout: i64 = writer
            .conn
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, 5000, "writer busy_timeout must be set");

        let reader = Store::open_read_only(&db).unwrap();
        let timeout: i64 = reader
            .conn
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, 5000, "reader busy_timeout must be set");
    }

    #[test]
    fn set_meta_inserts_then_upserts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        assert_eq!(store.meta("last_index_failed").unwrap(), None);
        store.set_meta("last_index_failed", "3").unwrap();
        assert_eq!(
            store.meta("last_index_failed").unwrap().as_deref(),
            Some("3")
        );
        store.set_meta("last_index_failed", "0").unwrap();
        assert_eq!(
            store.meta("last_index_failed").unwrap().as_deref(),
            Some("0")
        );
    }

    #[test]
    fn delete_meta_removes_the_key_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store.set_meta("reconcile_progress", "5/10").unwrap();
        assert_eq!(
            store.meta("reconcile_progress").unwrap().as_deref(),
            Some("5/10")
        );

        store.delete_meta("reconcile_progress").unwrap();
        assert_eq!(
            store.meta("reconcile_progress").unwrap(),
            None,
            "deleted key must read back as absent, not an empty string"
        );

        // Deleting an already-absent key is a no-op, not an error.
        store.delete_meta("reconcile_progress").unwrap();
    }

    #[test]
    fn corrupt_db_is_deleted_and_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        std::fs::write(&db, b"not a database at all").unwrap();

        // Also drop stale WAL/SHM sidecars to verify they get cleaned up too.
        std::fs::write(dir.path().join("index.db-wal"), b"stale wal").unwrap();
        std::fs::write(dir.path().join("index.db-shm"), b"stale shm").unwrap();

        let store = Store::open(&db).expect("corrupt DB must self-heal, not fail forever");
        assert_eq!(
            store.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION
        );
        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (0, 0, 0, 0));

        // The stale sidecar content must be gone — WAL mode may recreate its
        // own valid file, but never with the old garbage bytes.
        if let Ok(wal) = std::fs::read(dir.path().join("index.db-wal")) {
            assert_ne!(wal, b"stale wal");
        }
    }

    #[test]
    fn version_mismatch_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        {
            let store = Store::open(&db).unwrap();
            store
                .conn
                .execute(
                    "UPDATE meta SET value = '0' WHERE key = 'schema_version'",
                    [],
                )
                .unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION
        );
    }

    fn sample_index() -> crate::model::FileIndex {
        use crate::model::*;
        FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "mod".into(),
                    qualname: "app".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: 10,
                    parent: None,
                    arity: None,
                },
                NewSymbol {
                    name: "greet".into(),
                    qualname: "app.greet".into(),
                    kind: SymbolKind::Function,
                    sig: Some("def greet(name)".into()),
                    start_line: 3,
                    end_line: 5,
                    parent: Some(0),
                    arity: Some(1),
                },
            ],
            edges: vec![NewEdge {
                src: 1,
                kind: EdgeKind::Calls,
                dst_name: "format".into(),
                dst_arity: Some(1),
            }],
            chunks: vec![NewChunk {
                symbol: Some(1),
                start_line: 3,
                end_line: 5,
                content: "def greet(name):\n    return f\"hi {name}\"\n".into(),
            }],
        }
    }

    #[test]
    fn replace_file_round_trip_and_hash_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let hash = [7u8; 32];

        assert!(store.file_hash("app.py").unwrap().is_none());
        store
            .replace_file("app.py", "python", &hash, &sample_index())
            .unwrap();
        assert_eq!(store.file_hash("app.py").unwrap(), Some(hash));

        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (1, 2, 1, 1));

        // parent_id got remapped from batch index to rowid
        let parent_ok: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM symbols s JOIN symbols p ON s.parent_id = p.id
             WHERE s.qualname = 'app.greet' AND p.qualname = 'app'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parent_ok, 1);

        // replacing again does not duplicate
        store
            .replace_file("app.py", "python", &[8u8; 32], &sample_index())
            .unwrap();
        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (1, 2, 1, 1));

        // remove cleans everything through cascades
        assert!(store.remove_file("app.py").unwrap());
        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (0, 0, 0, 0));
    }

    #[test]
    fn replace_file_returns_touched_names_union_of_old_and_new_deduped() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();

        // First insert: no old rows yet, touched == every new symbol name.
        let (id1, touched1) = store
            .replace_file("app.py", "python", &[1u8; 32], &sample_index())
            .unwrap();
        assert!(id1 > 0);
        let mut sorted1 = touched1;
        sorted1.sort();
        assert_eq!(sorted1, vec!["greet".to_string(), "mod".to_string()]);

        // Second replace renames "greet" to "hello" but keeps "mod": touched
        // must be the deduped union of the old names (mod, greet) and the new
        // batch's names (mod, hello) — "mod" (unchanged) counted once.
        use crate::model::*;
        let renamed = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "mod".into(),
                    qualname: "app".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: 10,
                    parent: None,
                    arity: None,
                },
                NewSymbol {
                    name: "hello".into(),
                    qualname: "app.hello".into(),
                    kind: SymbolKind::Function,
                    sig: None,
                    start_line: 3,
                    end_line: 5,
                    parent: Some(0),
                    arity: Some(1),
                },
            ],
            edges: vec![],
            chunks: vec![],
        };
        let (_, touched2) = store
            .replace_file("app.py", "python", &[2u8; 32], &renamed)
            .unwrap();
        let mut sorted2 = touched2;
        sorted2.sort();
        assert_eq!(
            sorted2,
            vec!["greet".to_string(), "hello".to_string(), "mod".to_string()],
            "touched names must be the deduped union of removed (greet) and added (hello) names"
        );
    }

    #[test]
    fn malformed_index_out_of_range_src_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let hash = [9u8; 32];

        // Create a malformed index with out-of-range edge src
        use crate::model::*;
        let bad_index = FileIndex {
            symbols: vec![NewSymbol {
                name: "func".into(),
                qualname: "app.func".into(),
                kind: SymbolKind::Function,
                sig: None,
                start_line: 1,
                end_line: 5,
                parent: None,
                arity: None,
            }],
            edges: vec![NewEdge {
                src: 99,
                kind: EdgeKind::Calls,
                dst_name: "other".into(),
                dst_arity: None,
            }],
            chunks: vec![],
        };

        // Should return Err, not panic
        let result = store.replace_file("bad.py", "python", &hash, &bad_index);
        assert!(result.is_err(), "malformed index should return Err");
        assert!(
            result.unwrap_err().to_string().contains("edge src index"),
            "error should mention src index"
        );

        // Store should be unchanged (counts all 0)
        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (0, 0, 0, 0));
    }

    #[test]
    fn malformed_index_out_of_range_parent_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let hash = [10u8; 32];

        // Create a malformed index with out-of-range parent
        use crate::model::*;
        let bad_index = FileIndex {
            symbols: vec![NewSymbol {
                name: "child".into(),
                qualname: "app.child".into(),
                kind: SymbolKind::Function,
                sig: None,
                start_line: 1,
                end_line: 5,
                parent: Some(99),
                arity: None,
            }],
            edges: vec![],
            chunks: vec![],
        };

        // Should return Err, not panic
        let result = store.replace_file("bad2.py", "python", &hash, &bad_index);
        assert!(result.is_err(), "malformed index should return Err");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("symbol parent index"),
            "error should mention parent index"
        );

        // Store should be unchanged (counts all 0)
        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (0, 0, 0, 0));
    }

    #[test]
    fn malformed_index_out_of_range_symbol_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let hash = [11u8; 32];

        // Create a malformed index with out-of-range chunk symbol
        use crate::model::*;
        let bad_index = FileIndex {
            symbols: vec![NewSymbol {
                name: "func".into(),
                qualname: "app.func".into(),
                kind: SymbolKind::Function,
                sig: None,
                start_line: 1,
                end_line: 5,
                parent: None,
                arity: None,
            }],
            edges: vec![],
            chunks: vec![NewChunk {
                symbol: Some(99),
                start_line: 1,
                end_line: 5,
                content: "code".into(),
            }],
        };

        // Should return Err, not panic
        let result = store.replace_file("bad3.py", "python", &hash, &bad_index);
        assert!(result.is_err(), "malformed index should return Err");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("chunk symbol index"),
            "error should mention symbol index"
        );

        // Store should be unchanged (counts all 0)
        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (0, 0, 0, 0));
    }

    #[test]
    fn vec_roundtrip_knn_and_model_wipe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        assert!(
            store.vec_available(),
            "sqlite-vec should be statically linked"
        );

        assert!(store.set_model("mock", 4).unwrap()); // first set counts as change
        store
            .replace_file("a.py", "python", &[1u8; 32], &sample_index())
            .unwrap();

        let missing = store.chunks_missing_embedding(100).unwrap();
        assert_eq!(missing.len(), 1);
        let (chunk_id, _content, hash) = missing[0].clone();

        store.embed_cache_put(&hash, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        assert_eq!(
            store.embed_cache_get(&hash).unwrap().unwrap(),
            vec![1.0, 0.0, 0.0, 0.0]
        );

        store
            .put_embeddings(&[(chunk_id, vec![1.0, 0.0, 0.0, 0.0])])
            .unwrap();
        assert_eq!(store.embed_backlog().unwrap(), 0);

        let hits = store.knn_chunks(&[1.0, 0.0, 0.0, 0.0], 5).unwrap();
        assert_eq!(hits[0].0, chunk_id);

        // replace_file cleans vec rows for the file's chunks
        store
            .replace_file("a.py", "python", &[2u8; 32], &sample_index())
            .unwrap();
        assert_eq!(store.embed_backlog().unwrap(), 1); // new chunk id, no vec row

        // model change wipes cache + vec table
        assert!(store.set_model("other-model", 4).unwrap());
        assert!(store.embed_cache_get(&hash).unwrap().is_none());
        assert_eq!(store.knn_chunks(&[1.0, 0.0, 0.0, 0.0], 5).unwrap().len(), 0);

        assert!(!store.set_model("other-model", 4).unwrap()); // same model: no wipe
    }

    /// Item 6 (P4 residual): `embed_cache` rows are keyed purely by content
    /// hash, with no foreign key back to any chunk — a hash that no current
    /// chunk references anymore (its chunk was deleted, or edited into
    /// different content) is an orphan `prune_orphaned_embed_cache` must
    /// remove, while a hash a live chunk still references must survive.
    #[test]
    fn prune_orphaned_embed_cache_removes_only_hashes_with_no_matching_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store
            .replace_file("a.py", "python", &[1u8; 32], &sample_index())
            .unwrap();
        let (_chunk_id, _content, live_hash) =
            store.chunks_missing_embedding(100).unwrap()[0].clone();

        store.embed_cache_put(&live_hash, &[1.0, 0.0]).unwrap();
        // A hash no chunk references at all.
        store.embed_cache_put(&[9u8; 32], &[2.0, 0.0]).unwrap();
        assert_eq!(store.embed_cache_len().unwrap(), 2);

        let pruned = store.prune_orphaned_embed_cache().unwrap();
        assert_eq!(pruned, 1, "exactly the orphaned row must be pruned");
        assert_eq!(store.embed_cache_len().unwrap(), 1);
        assert!(
            store.embed_cache_get(&live_hash).unwrap().is_some(),
            "a hash still referenced by a live chunk must survive the prune"
        );
    }

    #[test]
    fn remove_file_cleans_vec_rows_so_reused_chunk_ids_start_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store.set_model("mock", 4).unwrap();

        store
            .replace_file("a.py", "python", &[1u8; 32], &sample_index())
            .unwrap();
        let (chunk_id, _, _) = store.chunks_missing_embedding(100).unwrap()[0].clone();
        store
            .put_embeddings(&[(chunk_id, vec![1.0, 0.0, 0.0, 0.0])])
            .unwrap();
        assert_eq!(store.embed_backlog().unwrap(), 0);

        // Remove the file entirely: its vec row must go with it, otherwise a
        // future chunk that reuses this rowid would inherit a stale embedding.
        assert!(store.remove_file("a.py").unwrap());
        assert_eq!(
            store.knn_chunks(&[1.0, 0.0, 0.0, 0.0], 5).unwrap().len(),
            0,
            "orphaned vec row must not survive remove_file"
        );
    }

    #[test]
    fn put_embeddings_self_heals_missing_vec_table() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        // No set_model call: vec_chunks doesn't exist yet.
        store
            .replace_file("a.py", "python", &[1u8; 32], &sample_index())
            .unwrap();
        let (chunk_id, _, _) = store.chunks_missing_embedding(100).unwrap()[0].clone();

        // Must not silently drop the embedding just because the table wasn't
        // created yet — chunks_missing_embedding already reported it as
        // outstanding work, so put_embeddings has to honor it.
        store
            .put_embeddings(&[(chunk_id, vec![1.0, 0.0, 0.0, 0.0])])
            .unwrap();
        assert_eq!(store.embed_backlog().unwrap(), 0);
        assert_eq!(
            store.knn_chunks(&[1.0, 0.0, 0.0, 0.0], 5).unwrap()[0].0,
            chunk_id
        );
    }

    #[test]
    fn set_model_wipes_on_dimension_change_even_with_same_model_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        assert!(store.set_model("mock", 4).unwrap());
        store
            .replace_file("a.py", "python", &[1u8; 32], &sample_index())
            .unwrap();
        let (chunk_id, _, hash) = store.chunks_missing_embedding(100).unwrap()[0].clone();
        store.embed_cache_put(&hash, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        store
            .put_embeddings(&[(chunk_id, vec![1.0, 0.0, 0.0, 0.0])])
            .unwrap();

        // Same model_id, different dim: must be treated as a real change.
        assert!(store.set_model("mock", 8).unwrap());
        assert!(store.embed_cache_get(&hash).unwrap().is_none());
        assert_eq!(
            store
                .knn_chunks(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 5)
                .unwrap()
                .len(),
            0
        );
    }

    /// Finding I6: `set_model` must bump `generation` whenever it actually
    /// creates or recreates `vec_chunks` — a reader's `lock_store_fresh`
    /// only re-probes "does vec_chunks exist" on a generation change, so a
    /// model-changed write that never bumped would leave a reader that
    /// cached "no vec table" stuck reporting that forever.
    #[test]
    fn set_model_bumps_generation_on_a_real_model_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        assert_eq!(store.generation().unwrap(), 0);

        assert!(store.set_model("mock", 4).unwrap());
        assert_eq!(
            store.generation().unwrap(),
            1,
            "first-ever set_model creates vec_chunks and must bump"
        );

        assert!(store.set_model("mock", 8).unwrap());
        assert_eq!(
            store.generation().unwrap(),
            2,
            "a dimension change recreates vec_chunks and must bump again"
        );
    }

    /// The flip side: calling `set_model` again with the exact same
    /// model/dim, once the table already exists, is a true no-op — it must
    /// not bump `generation` on every repeated call (e.g. every `vexus
    /// index` run against an unchanged model).
    #[test]
    fn set_model_does_not_bump_generation_when_unchanged_and_table_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store.set_model("mock", 4).unwrap();
        let gen_after_first_set = store.generation().unwrap();

        assert!(!store.set_model("mock", 4).unwrap());
        assert_eq!(
            store.generation().unwrap(),
            gen_after_first_set,
            "an unchanged set_model call over an already-existing table must not bump"
        );
    }

    /// All the `!vec_available` degrade branches are unreachable via a real
    /// `Store::open` (sqlite-vec is statically linked, so it's always
    /// available in this workspace's binaries). `force_vec_unavailable`
    /// exists purely so this test can exercise them.
    #[test]
    fn vec_unavailable_degrades_every_vec_path_instead_of_lying() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store.set_model("mock", 4).unwrap();
        store
            .replace_file("a.py", "python", &[1u8; 32], &sample_index())
            .unwrap();
        let chunk_count = store.counts().unwrap().chunks;
        assert!(chunk_count > 0, "fixture must produce at least one chunk");

        store.force_vec_unavailable();

        // The true backlog is every chunk, never 0 — 0 would lie that a
        // structural-only index is fully embedded.
        assert_eq!(store.embed_backlog().unwrap(), chunk_count);

        // Nothing is ever reported as embeddable or embedded.
        assert!(store.chunks_missing_embedding(100).unwrap().is_empty());
        assert!(store
            .knn_chunks(&[1.0, 0.0, 0.0, 0.0], 5)
            .unwrap()
            .is_empty());

        // put_embeddings silently no-ops rather than erroring.
        store
            .put_embeddings(&[(1, vec![1.0, 0.0, 0.0, 0.0])])
            .unwrap();
        assert_eq!(store.embed_backlog().unwrap(), chunk_count);

        // search_hybrid falls back to keyword-only: passing a query vector
        // changes nothing, since knn_chunks always returns empty.
        let with_vec = store
            .search_hybrid("greet", Some(&[1.0, 0.0, 0.0, 0.0]), 10)
            .unwrap();
        let keyword_only = store.search_hybrid("greet", None, 10).unwrap();
        assert_eq!(with_vec.len(), keyword_only.len());
        assert_eq!(
            with_vec.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            keyword_only.iter().map(|h| h.chunk_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn vec_table_exists_is_cached() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        assert!(!store.vec_table_exists().unwrap());
        store.ensure_vec_table(4).unwrap();
        assert!(store.vec_table_exists().unwrap());
        store.set_model("mock", 4).unwrap();
        assert!(store.vec_table_exists().unwrap());
        // behavioral check that cache is used: drop the table behind the cache's back;
        // the cached value is now (intentionally) stale — documents the invalidation contract
        store
            .conn_ref_for_tests()
            .execute("DROP TABLE vec_chunks", [])
            .unwrap();
        assert!(
            store.vec_table_exists().unwrap(),
            "cache intentionally not invalidated by raw SQL"
        );
    }

    #[test]
    fn dim_mismatched_query_vec_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store.set_model("mock", 4).unwrap();
        store
            .replace_file("a.py", "python", &[1u8; 32], &sample_index())
            .unwrap();

        let missing = store.chunks_missing_embedding(100).unwrap();
        assert_eq!(missing.len(), 1);
        let (chunk_id, _content, _hash) = missing[0].clone();

        // Embed with 4-dim vector
        store
            .put_embeddings(&[(chunk_id, vec![1.0, 0.0, 0.0, 0.0])])
            .unwrap();
        assert_eq!(store.embed_backlog().unwrap(), 0);

        // Matching dimension works
        let hits = store
            .search_hybrid("greet", Some(&[1.0, 0.0, 0.0, 0.0]), 10)
            .unwrap();
        assert!(
            !hits.is_empty(),
            "4-dim query against 4-dim table should return hits"
        );

        // Mismatched dimension must not panic—acceptable outcomes are Err or keyword-only
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.search_hybrid("greet", Some(&[0.5; 8]), 10)
        }));
        assert!(
            result.is_ok(),
            "8-dim query against 4-dim table must not panic"
        );
        match result.unwrap() {
            Ok(hits) => {
                // Keyword-only results are acceptable (knn_chunks fails gracefully)
                let kw_only = store.search_hybrid("greet", None, 10).unwrap();
                assert_eq!(
                    hits.len(),
                    kw_only.len(),
                    "mismatched dim query should degrade to keyword-only, not error"
                );
            }
            Err(_) => {
                // Err is also acceptable if the implementation catches the mismatch
            }
        }
    }

    #[test]
    fn open_read_only_reads_fine_but_write_attempts_err() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        {
            let mut store = Store::open(&db).unwrap();
            store
                .replace_file("a.py", "python", &[1u8; 32], &sample_index())
                .unwrap();
        }

        let mut reader = Store::open_read_only(&db).unwrap();
        // Reads work fine.
        assert_eq!(reader.file_hash("a.py").unwrap(), Some([1u8; 32]));
        assert_eq!(reader.counts().unwrap().files, 1);
        assert_eq!(
            reader.meta("schema_version").unwrap().as_deref(),
            Some(SCHEMA_VERSION)
        );

        // Writes must error rather than silently succeed or panic.
        assert!(
            reader.set_meta("foo", "bar").is_err(),
            "write on a read-only store must return an error"
        );
    }

    /// Item 5 (P4 residual): a DB whose persisted `schema_version` doesn't
    /// match this build's `SCHEMA_VERSION` must fail `open_read_only` with a
    /// message telling the caller how to recover, rather than silently
    /// reading (and possibly misinterpreting) a schema this build doesn't
    /// actually understand. Simulated by overwriting `schema_version` via
    /// `set_meta` on an otherwise-normal DB — `Store::open` itself would
    /// self-heal a real mismatch by rebuilding the file (see `version_ok`),
    /// so this deliberately writes the mismatch *after* that check has
    /// already passed, isolating `open_read_only`'s own check from `open`'s.
    #[test]
    fn open_read_only_errs_on_schema_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        {
            let mut store = Store::open(&db).unwrap();
            store.set_meta("schema_version", "999").unwrap();
        }

        let err = match Store::open_read_only(&db) {
            Ok(_) => panic!("open_read_only must err on a schema_version mismatch"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("re-run 'vexus index'"),
            "got: {err}"
        );
    }

    #[test]
    fn open_read_only_errs_when_db_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("does-not-exist.db");
        let result = Store::open_read_only(&db);
        assert!(
            result.is_err(),
            "read-only open of a missing DB must error, never create one"
        );
        assert!(!db.exists(), "read-only open must never create the DB file");
    }

    #[test]
    fn generation_bump_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        assert_eq!(
            store.generation().unwrap(),
            0,
            "absent generation reads as 0"
        );
        store.bump_generation().unwrap();
        assert_eq!(store.generation().unwrap(), 1);
        store.bump_generation().unwrap();
        store.bump_generation().unwrap();
        assert_eq!(store.generation().unwrap(), 3);
    }

    /// The scenario `lock_store_fresh` (vexus-mcp) exists to fix: a reader
    /// connection opened before `vec_chunks` existed caches "table absent".
    /// A writer then creates the table, embeds, and bumps the generation.
    /// Without an explicit `clear_caches`, the reader's stale cached `false`
    /// would keep reporting no vec table / no KNN hits forever. Clearing the
    /// cache after observing the generation change must make the reader see
    /// the writer's data.
    #[test]
    fn stale_vec_table_cache_is_cleared_by_generation_bump_coherence() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");

        // Writer creates the schema and indexes a file, but hasn't set a
        // model yet, so vec_chunks doesn't exist.
        let mut writer = Store::open(&db).unwrap();
        writer
            .replace_file("a.py", "python", &[1u8; 32], &sample_index())
            .unwrap();

        // Reader opens while vec_chunks is still absent and caches that fact.
        let reader = Store::open_read_only(&db).unwrap();
        assert!(
            !reader.vec_table_exists().unwrap(),
            "vec table shouldn't exist yet"
        );
        assert_eq!(reader.generation().unwrap(), 0);

        // Writer creates vec_chunks via set_model — which itself bumps the
        // generation now that it just created the table (finding I6) —
        // embeds a chunk, then bumps again to signal that separate change
        // (set_model's own bump only covers the table's existence, not
        // whatever rows a caller puts into it afterward).
        assert!(writer.set_model("mock", 4).unwrap());
        let (chunk_id, _content, _hash) = writer.chunks_missing_embedding(100).unwrap()[0].clone();
        writer
            .put_embeddings(&[(chunk_id, vec![1.0, 0.0, 0.0, 0.0])])
            .unwrap();
        writer.bump_generation().unwrap();

        // The reader's cached "false" is now stale.
        assert!(
            !reader.vec_table_exists().unwrap(),
            "cache not yet invalidated — still reporting the old (wrong) answer"
        );

        // Simulate what `lock_store_fresh` does: notice the generation
        // changed, then clear caches. Two bumps: one from `set_model`
        // creating the table, one from the explicit `bump_generation` above.
        assert_eq!(reader.generation().unwrap(), 2);
        reader.clear_caches();

        assert!(
            reader.vec_table_exists().unwrap(),
            "cache cleared: table is now visible"
        );
        let hits = reader.knn_chunks(&[1.0, 0.0, 0.0, 0.0], 5).unwrap();
        assert_eq!(
            hits.first().map(|h| h.0),
            Some(chunk_id),
            "reader must see the writer's embeddings after the cache clear"
        );
    }
}
