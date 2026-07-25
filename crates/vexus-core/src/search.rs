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

const RRF_K: f64 = 60.0;
const CANDIDATES: u32 = 50;

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
            "SELECT c.id, f.path, s.qualname, c.start_line, c.end_line, c.content
             FROM chunks c JOIN files f ON f.id = c.file_id
             LEFT JOIN symbols s ON s.id = c.symbol_id WHERE c.id = ?1",
        )?;
        for (chunk_id, score) in ranked {
            let hit = stmt.query_row([chunk_id], |r| {
                let content: String = r.get(5)?;
                let excerpt: String = content.replace('\n', " ").chars().take(120).collect();
                Ok(SearchHit {
                    chunk_id: r.get(0)?,
                    path: r.get(1)?,
                    qualname: r.get(2)?,
                    start_line: r.get(3)?,
                    end_line: r.get(4)?,
                    score: *score,
                    excerpt,
                })
            })?;
            out.push(hit);
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
        assert_eq!(
            hits[0].excerpt.contains("hybrid"),
            true,
            "both-list chunk wins RRF"
        );
        assert_eq!(hits.len(), 3);

        // None query_vec degrades to keyword-only: B disappears
        let kw = store.search_hybrid("banana", None, 10).unwrap();
        assert_eq!(kw.len(), 2);
    }
}
