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
}
