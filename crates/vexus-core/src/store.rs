use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

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
    pub fn set_model(&mut self, model_id: &str, dim: usize) -> Result<bool> {
        let current_id = self.meta("model_id")?;
        let current_dim = self.meta("model_dim")?;
        let unchanged = current_id.as_deref() == Some(model_id)
            && current_dim.as_deref() == Some(&*dim.to_string());
        if unchanged {
            self.ensure_vec_table(dim)?;
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

    pub fn replace_file(
        &mut self,
        path: &str,
        lang: &str,
        hash: &[u8; 32],
        idx: &crate::model::FileIndex,
    ) -> Result<i64> {
        use crate::model::estimate_tokens;
        use anyhow::anyhow;
        let vec_available = self.vec_available;
        let tx = self.conn.transaction()?;

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
        Ok(file_id)
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
}
