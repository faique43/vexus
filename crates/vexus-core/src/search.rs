use anyhow::Result;
use rusqlite::OptionalExtension;

use crate::Store;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk_id: i64,
    /// The chunk's owning symbol, when it belongs to one (`None` for
    /// preamble/module-level chunks with no enclosing symbol) — lets
    /// `explore` walk the call/import graph from a search hit without a
    /// separate symbol lookup.
    pub symbol_id: Option<i64>,
    pub path: String,
    pub qualname: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f64,
    pub excerpt: String,
    /// Full chunk content, verbatim (unlike `excerpt`, which is truncated
    /// for display in `search`'s result list) — what `explore` renders as
    /// the entry chunk's source.
    pub content: String,
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

const RRF_K: f64 = 60.0;
const CANDIDATES: u32 = 50;

impl Store {
    pub fn search_keyword(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let Some(fts) = sanitize_fts(query) else {
            return Ok(vec![]);
        };
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.symbol_id, f.path, s.qualname, c.start_line, c.end_line,
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
                let content: String = r.get(7)?;
                let excerpt: String = content.replace('\n', " ").chars().take(120).collect();
                Ok(SearchHit {
                    chunk_id: r.get(0)?,
                    symbol_id: r.get(1)?,
                    path: r.get(2)?,
                    qualname: r.get(3)?,
                    start_line: r.get(4)?,
                    end_line: r.get(5)?,
                    score: r.get(6)?,
                    excerpt,
                    content,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(hits)
    }

    pub fn search_hybrid(
        &self,
        query_text: &str,
        query_vec: Option<&[f32]>,
        limit: u32,
    ) -> Result<Vec<SearchHit>> {
        use std::collections::HashMap;
        let mut scores: HashMap<i64, f64> = HashMap::new();

        for (rank, hit) in self
            .search_keyword(query_text, CANDIDATES)?
            .iter()
            .enumerate()
        {
            *scores.entry(hit.chunk_id).or_default() += 1.0 / (RRF_K + rank as f64);
        }
        if let Some(qv) = query_vec {
            for (rank, (chunk_id, _dist)) in self.knn_chunks(qv, CANDIDATES)?.iter().enumerate() {
                *scores.entry(*chunk_id).or_default() += 1.0 / (RRF_K + rank as f64);
            }
        }

        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(limit as usize);
        self.hydrate_hits(&ranked)
    }

    fn hydrate_hits(&self, ranked: &[(i64, f64)]) -> Result<Vec<SearchHit>> {
        let mut out = Vec::with_capacity(ranked.len());
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.id, c.symbol_id, f.path, s.qualname, c.start_line, c.end_line, c.content
             FROM chunks c JOIN files f ON f.id = c.file_id
             LEFT JOIN symbols s ON s.id = c.symbol_id WHERE c.id = ?1",
        )?;
        for (chunk_id, score) in ranked {
            let hit = stmt
                .query_row([chunk_id], |r| {
                    let content: String = r.get(6)?;
                    let excerpt: String = content.replace('\n', " ").chars().take(120).collect();
                    Ok(SearchHit {
                        chunk_id: r.get(0)?,
                        symbol_id: r.get(1)?,
                        path: r.get(2)?,
                        qualname: r.get(3)?,
                        start_line: r.get(4)?,
                        end_line: r.get(5)?,
                        score: *score,
                        excerpt,
                        content,
                    })
                })
                .optional()?;
            if let Some(hit) = hit {
                out.push(hit);
            }
        }
        Ok(out)
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

    #[test]
    fn hybrid_fuses_vector_and_keyword_ranks() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store.set_model("mock", 4).unwrap();

        // three chunks: A matches keyword only, B matches vector only, C matches both
        let idx = FileIndex {
            symbols: vec![NewSymbol {
                name: "m".into(),
                qualname: "m".into(),
                kind: SymbolKind::Module,
                sig: None,
                start_line: 1,
                end_line: 9,
                parent: None,
                arity: None,
            }],
            edges: vec![],
            chunks: vec![
                NewChunk {
                    symbol: Some(0),
                    start_line: 1,
                    end_line: 1,
                    content: "keyword banana split".into(),
                },
                NewChunk {
                    symbol: Some(0),
                    start_line: 3,
                    end_line: 3,
                    content: "vector only content".into(),
                },
                NewChunk {
                    symbol: Some(0),
                    start_line: 5,
                    end_line: 5,
                    content: "banana vector hybrid".into(),
                },
            ],
        };
        store
            .replace_file("x.py", "python", &[1u8; 32], &idx)
            .unwrap();

        let missing = store.chunks_missing_embedding(10).unwrap();
        // embed: B and C near the query vector, A orthogonal
        for (id, content, _hash) in &missing {
            let v = if content.contains("vector") {
                vec![1.0, 0.0, 0.0, 0.0]
            } else {
                vec![0.0, 1.0, 0.0, 0.0]
            };
            store.put_embeddings(&[(*id, v)]).unwrap();
        }

        let hits = store
            .search_hybrid("banana", Some(&[1.0, 0.0, 0.0, 0.0]), 10)
            .unwrap();
        // C (both lists) must outrank A (keyword only) and B (vector only)
        assert!(
            hits[0].excerpt.contains("hybrid"),
            "both-list chunk wins RRF"
        );
        assert_eq!(hits.len(), 3);

        // None query_vec degrades to keyword-only: B disappears
        let kw = store.search_hybrid("banana", None, 10).unwrap();
        assert_eq!(kw.len(), 2);
    }

    #[test]
    fn hydrate_skips_chunks_deleted_after_ranking() {
        // Rank against a store, then delete the file (cascades chunks), then hydrate.
        // search_hybrid re-ranks internally, so simulate by deleting between two calls:
        // simplest deterministic version — delete a ranked chunk id directly via SQL,
        // then assert search_hybrid returns the remaining hits instead of Err.
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store.set_model("mock", 4).unwrap();
        let idx = FileIndex {
            symbols: vec![NewSymbol {
                name: "m".into(),
                qualname: "m".into(),
                kind: SymbolKind::Module,
                sig: None,
                start_line: 1,
                end_line: 3,
                parent: None,
                arity: None,
            }],
            edges: vec![],
            chunks: vec![
                NewChunk {
                    symbol: Some(0),
                    start_line: 1,
                    end_line: 1,
                    content: "banana one".into(),
                },
                NewChunk {
                    symbol: Some(0),
                    start_line: 2,
                    end_line: 2,
                    content: "banana two".into(),
                },
            ],
        };
        store
            .replace_file("x.py", "python", &[1u8; 32], &idx)
            .unwrap();
        // delete one chunk row out from under FTS (external-content table keeps the
        // fts row only via triggers; direct delete fires chunks_ad, so instead we
        // delete AFTER capturing that both rank; emulate the race by removing the
        // chunk row with triggers disabled):
        store.conn_ref_for_tests().execute_batch(
            "DROP TRIGGER chunks_ad; DELETE FROM chunks WHERE id = (SELECT min(id) FROM chunks);"
        ).unwrap();
        let hits = store.search_hybrid("banana", None, 10).unwrap();
        assert_eq!(hits.len(), 1, "surviving chunk still returned, no Err");
    }
}
