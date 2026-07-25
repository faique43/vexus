//! Symbol resolution and source lookup: turning a user-supplied name into a
//! concrete symbol (or a useful "did you mean" answer), and fetching the
//! source chunks that belong to a resolved symbol.

use anyhow::Result;
use rusqlite::OptionalExtension;

use crate::resolve::last_segment;
use crate::Store;

const NAME_MATCH_LIMIT: i64 = 11;
const SUGGESTION_LIMIT: i64 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolInfo {
    pub id: i64,
    pub name: String,
    pub qualname: String,
    pub kind: String,
    pub sig: Option<String>,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Exact(SymbolInfo),
    /// Ambiguous name — max 10, no bodies.
    Candidates(Vec<SymbolInfo>),
    /// No match — nearest qualnames, max 5.
    NotFound {
        suggestions: Vec<String>,
    },
}

fn symbol_info_from_row(r: &rusqlite::Row) -> rusqlite::Result<SymbolInfo> {
    Ok(SymbolInfo {
        id: r.get(0)?,
        name: r.get(1)?,
        qualname: r.get(2)?,
        kind: r.get(3)?,
        sig: r.get(4)?,
        path: r.get(5)?,
        start_line: r.get(6)?,
        end_line: r.get(7)?,
    })
}

const SYMBOL_INFO_SELECT: &str = "SELECT s.id, s.name, s.qualname, s.kind, s.sig, f.path,
            s.start_line, s.end_line
     FROM symbols s JOIN files f ON f.id = s.file_id";

impl Store {
    /// `target`: qualname ("mod.Class.method") tried first, then bare name.
    /// Multiple name matches → Candidates. No match → NotFound with
    /// FTS-derived suggestions.
    pub fn resolve_symbol(&self, target: &str) -> Result<Resolution> {
        if let Some(info) = self.symbol_by_qualname(target)? {
            return Ok(Resolution::Exact(info));
        }

        let by_name = self.symbols_by_name(target, NAME_MATCH_LIMIT)?;
        match by_name.len() {
            0 => {}
            1 => return Ok(Resolution::Exact(by_name.into_iter().next().unwrap())),
            _ => {
                let mut candidates = by_name;
                candidates.truncate(10);
                return Ok(Resolution::Candidates(candidates));
            }
        }

        let seg = last_segment(target);
        let mut suggestions = self.suggest_by_name_like(seg)?;
        if suggestions.is_empty() {
            let prefix: String = seg.chars().take(3).collect();
            suggestions = self.suggest_by_name_like(&prefix)?;
        }
        Ok(Resolution::NotFound { suggestions })
    }

    /// All chunks belonging to the symbol (by symbol_id), ordered by
    /// start_line: (start_line, end_line, content). Empty for symbols with
    /// no chunks (e.g. module).
    pub fn symbol_source(&self, symbol_id: i64) -> Result<Vec<(u32, u32, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT start_line, end_line, content FROM chunks
             WHERE symbol_id = ?1 ORDER BY start_line",
        )?;
        let rows = stmt
            .query_map([symbol_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// SymbolInfo by id (for hydrating graph results).
    pub fn symbol_info(&self, id: i64) -> Result<Option<SymbolInfo>> {
        let sql = format!("{SYMBOL_INFO_SELECT} WHERE s.id = ?1");
        let info = self
            .conn
            .query_row(&sql, [id], symbol_info_from_row)
            .optional()?;
        Ok(info)
    }

    fn symbol_by_qualname(&self, qualname: &str) -> Result<Option<SymbolInfo>> {
        let sql = format!("{SYMBOL_INFO_SELECT} WHERE s.qualname = ?1");
        let info = self
            .conn
            .query_row(&sql, [qualname], symbol_info_from_row)
            .optional()?;
        Ok(info)
    }

    fn symbols_by_name(&self, name: &str, limit: i64) -> Result<Vec<SymbolInfo>> {
        let sql = format!("{SYMBOL_INFO_SELECT} WHERE s.name = ?1 ORDER BY s.qualname LIMIT ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params![name, limit], symbol_info_from_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn suggest_by_name_like(&self, pattern: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT qualname FROM symbols WHERE name LIKE '%' || ?1 || '%'
             ORDER BY length(name) LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![pattern, SUGGESTION_LIMIT], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use crate::model::*;
    use crate::query::Resolution;
    use crate::Store;

    fn sym(name: &str, qual: &str, kind: SymbolKind) -> NewSymbol {
        NewSymbol {
            name: name.into(),
            qualname: qual.into(),
            kind,
            sig: None,
            start_line: 1,
            end_line: 2,
            parent: None,
            arity: None,
        }
    }

    fn fixture_store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        let dir = Box::leak(Box::new(dir));
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();

        let a = FileIndex {
            symbols: vec![sym("slug", "app.util.slug", SymbolKind::Function)],
            edges: vec![],
            chunks: vec![NewChunk {
                symbol: Some(0),
                start_line: 1,
                end_line: 2,
                content: "def slug(text):\n    return text.lower()\n".into(),
            }],
        };
        store
            .replace_file("a.py", "python", &[1u8; 32], &a)
            .unwrap();

        let b = FileIndex {
            symbols: vec![
                sym("slug", "web.util.slug", SymbolKind::Function),
                sym("Handler", "web.Handler", SymbolKind::Class),
            ],
            edges: vec![],
            chunks: vec![],
        };
        store
            .replace_file("b.py", "python", &[2u8; 32], &b)
            .unwrap();

        store
    }

    #[test]
    fn exact_qualname_match() {
        let store = fixture_store();
        match store.resolve_symbol("app.util.slug").unwrap() {
            Resolution::Exact(info) => {
                assert_eq!(info.qualname, "app.util.slug");
                assert_eq!(info.path, "a.py");
                assert_eq!(info.kind, "function");
            }
            other => panic!("expected Exact, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_name_returns_ordered_candidates() {
        let store = fixture_store();
        match store.resolve_symbol("slug").unwrap() {
            Resolution::Candidates(cands) => {
                assert_eq!(cands.len(), 2);
                assert_eq!(cands[0].qualname, "app.util.slug");
                assert_eq!(cands[1].qualname, "web.util.slug");
            }
            other => panic!("expected Candidates, got {other:?}"),
        }
    }

    #[test]
    fn unique_bare_name_resolves_exact() {
        let store = fixture_store();
        match store.resolve_symbol("Handler").unwrap() {
            Resolution::Exact(info) => {
                assert_eq!(info.qualname, "web.Handler");
                assert_eq!(info.kind, "class");
                assert_eq!(info.path, "b.py");
            }
            other => panic!("expected Exact, got {other:?}"),
        }
    }

    #[test]
    fn unknown_name_returns_suggestions() {
        let store = fixture_store();
        match store.resolve_symbol("slugg").unwrap() {
            Resolution::NotFound { suggestions } => {
                assert!(
                    suggestions.iter().any(|s| s.contains("slug")),
                    "expected a suggestion containing 'slug', got {suggestions:?}"
                );
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn symbol_source_returns_chunk_text() {
        let store = fixture_store();
        let info = match store.resolve_symbol("app.util.slug").unwrap() {
            Resolution::Exact(info) => info,
            other => panic!("expected Exact, got {other:?}"),
        };
        let chunks = store.symbol_source(info.id).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, 1);
        assert_eq!(chunks[0].1, 2);
        assert!(chunks[0].2.contains("def slug(text):"));
    }

    #[test]
    fn symbol_source_empty_for_symbol_with_no_chunks() {
        let store = fixture_store();
        let info = match store.resolve_symbol("web.Handler").unwrap() {
            Resolution::Exact(info) => info,
            other => panic!("expected Exact, got {other:?}"),
        };
        assert!(store.symbol_source(info.id).unwrap().is_empty());
    }

    #[test]
    fn symbol_info_hydrates_by_id() {
        let store = fixture_store();
        let info = match store.resolve_symbol("web.Handler").unwrap() {
            Resolution::Exact(info) => info,
            other => panic!("expected Exact, got {other:?}"),
        };
        let hydrated = store.symbol_info(info.id).unwrap().unwrap();
        assert_eq!(hydrated, info);
        assert!(store.symbol_info(-1).unwrap().is_none());
    }
}
