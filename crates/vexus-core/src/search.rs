use anyhow::Result;

use crate::Store;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub path: String,
    pub qualname: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f64,
    pub excerpt: String,
}

/// Turn arbitrary user text into a safe FTS5 query: each alphanumeric term
/// double-quoted, OR-joined. Empty input → None.
fn sanitize_fts(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

impl Store {
    pub fn search_keyword(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let Some(fts) = sanitize_fts(query) else {
            return Ok(vec![]);
        };
        let mut stmt = self.conn.prepare(
            "SELECT c.id, f.path, s.qualname, c.start_line, c.end_line,
                    -bm25(fts_chunks) AS score, c.content
             FROM fts_chunks
             JOIN chunks c ON c.id = fts_chunks.rowid
             JOIN files f ON f.id = c.file_id
             LEFT JOIN symbols s ON s.id = c.symbol_id
             WHERE fts_chunks MATCH ?1
             ORDER BY score DESC LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(rusqlite::params![fts, limit], |r| {
                let content: String = r.get(6)?;
                let excerpt: String = content.replace('\n', " ").chars().take(120).collect();
                Ok(SearchHit {
                    chunk_id: r.get(0)?,
                    path: r.get(1)?,
                    qualname: r.get(2)?,
                    start_line: r.get(3)?,
                    end_line: r.get(4)?,
                    score: r.get(5)?,
                    excerpt,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use crate::model::*;
    use crate::Store;

    #[test]
    fn keyword_search_ranks_and_survives_weird_queries() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let idx = FileIndex {
            symbols: vec![NewSymbol {
                name: "retry".into(),
                qualname: "up.retry".into(),
                kind: SymbolKind::Function,
                sig: None,
                start_line: 1,
                end_line: 3,
                parent: None,
                arity: Some(1),
            }],
            edges: vec![],
            chunks: vec![
                NewChunk {
                    symbol: Some(0),
                    start_line: 1,
                    end_line: 3,
                    content: "def retry(delay):\n    backoff = delay * 2\n    return backoff\n"
                        .into(),
                },
                NewChunk {
                    symbol: None,
                    start_line: 5,
                    end_line: 6,
                    content: "unrelated logging helper\n".into(),
                },
            ],
        };
        store
            .replace_file("up.py", "python", &[1u8; 32], &idx)
            .unwrap();

        let hits = store.search_keyword("retry backoff", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "up.py");
        assert_eq!(hits[0].qualname.as_deref(), Some("up.retry"));
        assert!(hits[0].score > 0.0);

        // FTS operator characters must not error
        for q in ["retry AND", "\"unbalanced", "a*b(c)", "-", ""] {
            let _ = store.search_keyword(q, 10).unwrap();
        }
    }
}
