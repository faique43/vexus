use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

pub const SCHEMA_VERSION: &str = "1";
const SCHEMA: &str = include_str!("schema.sql");

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        if !Self::version_ok(&conn)? {
            drop(conn);
            if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(e.into());
                }
            }
            conn = Connection::open(path)?;
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Self::init_if_empty(&conn)?;
        Ok(Self { conn })
    }

    fn version_ok(conn: &Connection) -> Result<bool> {
        let has_meta: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'meta'",
            [],
            |r| r.get(0),
        )?;
        if has_meta == 0 {
            return Ok(true); // fresh file, nothing to mismatch
        }
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .ok();
        Ok(v.as_deref() == Some(SCHEMA_VERSION))
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

    pub fn file_hash(&self, path: &str) -> Result<Option<[u8; 32]>> {
        let v: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT hash FROM files WHERE path = ?1", [path], |r| r.get(0))
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
        let tx = self.conn.transaction()?;
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
            let parent_id = s.parent.map(|i| ids[i]);
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
            tx.execute(
                "INSERT INTO edges (src_id, kind, dst_name, dst_arity, dst_id, confidence)
                 VALUES (?1, ?2, ?3, ?4, NULL, 'name_only')",
                rusqlite::params![ids[e.src], e.kind.as_str(), e.dst_name, e.dst_arity],
            )?;
        }
        for c in &idx.chunks {
            let content_hash = blake3::hash(c.content.as_bytes());
            tx.execute(
                "INSERT INTO chunks (file_id, symbol_id, start_line, end_line, content, content_hash, token_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    file_id, c.symbol.map(|i| ids[i]), c.start_line, c.end_line,
                    c.content, content_hash.as_bytes().as_slice(), estimate_tokens(&c.content)
                ],
            )?;
        }
        tx.commit()?;
        Ok(file_id)
    }

    pub fn remove_file(&mut self, path: &str) -> Result<bool> {
        let n = self.conn.execute("DELETE FROM files WHERE path = ?1", [path])?;
        Ok(n > 0)
    }

    pub fn counts(&self) -> Result<crate::model::Counts> {
        let one = |sql: &str| -> Result<i64> {
            Ok(self.conn.query_row(sql, [], |r| r.get(0))?)
        };
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
        assert_eq!(store.meta("schema_version").unwrap().unwrap(), SCHEMA_VERSION);
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
    fn version_mismatch_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        {
            let store = Store::open(&db).unwrap();
            store
                .conn
                .execute("UPDATE meta SET value = '0' WHERE key = 'schema_version'", [])
                .unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(store.meta("schema_version").unwrap().unwrap(), SCHEMA_VERSION);
    }

    fn sample_index() -> crate::model::FileIndex {
        use crate::model::*;
        FileIndex {
            symbols: vec![
                NewSymbol { name: "mod".into(), qualname: "app".into(), kind: SymbolKind::Module,
                    sig: None, start_line: 1, end_line: 10, parent: None, arity: None },
                NewSymbol { name: "greet".into(), qualname: "app.greet".into(), kind: SymbolKind::Function,
                    sig: Some("def greet(name)".into()), start_line: 3, end_line: 5, parent: Some(0), arity: Some(1) },
            ],
            edges: vec![NewEdge { src: 1, kind: EdgeKind::Calls, dst_name: "format".into(), dst_arity: Some(1) }],
            chunks: vec![NewChunk { symbol: Some(1), start_line: 3, end_line: 5,
                content: "def greet(name):\n    return f\"hi {name}\"\n".into() }],
        }
    }

    #[test]
    fn replace_file_round_trip_and_hash_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let hash = [7u8; 32];

        assert!(store.file_hash("app.py").unwrap().is_none());
        store.replace_file("app.py", "python", &hash, &sample_index()).unwrap();
        assert_eq!(store.file_hash("app.py").unwrap(), Some(hash));

        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (1, 2, 1, 1));

        // parent_id got remapped from batch index to rowid
        let parent_ok: i64 = store.conn.query_row(
            "SELECT count(*) FROM symbols s JOIN symbols p ON s.parent_id = p.id
             WHERE s.qualname = 'app.greet' AND p.qualname = 'app'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(parent_ok, 1);

        // replacing again does not duplicate
        store.replace_file("app.py", "python", &[8u8; 32], &sample_index()).unwrap();
        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (1, 2, 1, 1));

        // remove cleans everything through cascades
        assert!(store.remove_file("app.py").unwrap());
        let c = store.counts().unwrap();
        assert_eq!((c.files, c.symbols, c.edges, c.chunks), (0, 0, 0, 0));
    }
}
