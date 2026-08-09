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

/// How confident retrieval is in what it returned — computed, not guessed,
/// from which candidate lists actually contributed. RRF scores themselves
/// are rank-based and corpus-size-dependent, so they can't be thresholded
/// for this; membership can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    /// At least one keyword hit, or at least one KNN candidate under the
    /// distance floor (or no floor to judge by) — the hits mean something.
    Strong,
    /// Keyword search found nothing and every KNN candidate sat above the
    /// distance floor: the hits are merely the corpus's nearest neighbors
    /// to a query nothing indexed actually matches. Callers should say so
    /// (and suggest grep) instead of presenting them as an answer.
    WeakVectorOnly,
}

// Candidate-pool sizing note: an earlier draft scaled the pools down with
// corpus size (KNN k = chunks/4 on tiny corpora). Measured against the eval
// corpora it only reshuffled luck — the keyword list's members are real bm25
// matches at any size, and the KNN list's small-corpus noise is exactly what
// `knn_floor` sheds by *distance* rather than by count. So both lists keep
// the full `CANDIDATES` everywhere and the floor does the de-noising.

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
        Ok(self
            .search_hybrid_scored(query_text, query_vec, None, limit)?
            .0)
    }

    /// `search_hybrid` plus a KNN distance floor and an honesty signal.
    ///
    /// KNN candidates at distance ≤ `knn_floor` fuse with keyword hits as
    /// usual. When keyword search finds nothing AND no KNN candidate clears
    /// the floor, the above-floor candidates are ranked anyway — a non-empty
    /// corpus should still show its nearest neighbors — but the outcome says
    /// `WeakVectorOnly` so the caller can present them as guesses, not
    /// answers. `knn_floor: None` (no embedder-specific floor, or the mock
    /// embedder) keeps every candidate and always reports `Strong`,
    /// byte-identical to the historical behavior.
    pub fn search_hybrid_scored(
        &self,
        query_text: &str,
        query_vec: Option<&[f32]>,
        knn_floor: Option<f64>,
        limit: u32,
    ) -> Result<(Vec<SearchHit>, SearchOutcome)> {
        use std::collections::HashMap;
        let mut scores: HashMap<i64, f64> = HashMap::new();

        let mut keyword_hit = false;
        for (rank, hit) in self
            .search_keyword(query_text, CANDIDATES)?
            .iter()
            .enumerate()
        {
            keyword_hit = true;
            *scores.entry(hit.chunk_id).or_default() += 1.0 / (RRF_K + rank as f64);
        }

        let mut near_vector_hit = false;
        if let Some(qv) = query_vec {
            let knn = self.knn_chunks(qv, CANDIDATES)?;
            let near: Vec<&(i64, f64)> = match knn_floor {
                Some(floor) => knn.iter().filter(|(_, d)| *d <= floor).collect(),
                None => knn.iter().collect(),
            };
            // Nothing near and nothing from keywords: fall back to the far
            // candidates rather than returning empty on a non-empty corpus.
            let ranked_list: Vec<&(i64, f64)> = if near.is_empty() && !keyword_hit {
                knn.iter().collect()
            } else {
                near_vector_hit = !near.is_empty();
                near
            };
            for (rank, (chunk_id, _dist)) in ranked_list.into_iter().enumerate() {
                *scores.entry(*chunk_id).or_default() += 1.0 / (RRF_K + rank as f64);
            }
        }

        // Structural priors run over the full candidate map BEFORE the sort
        // and truncation below, so they decide membership of the top-N (and
        // therefore explore's graph-expansion seeds), not just display order.
        self.apply_ranking_priors(query_text, &mut scores, &crate::priors::Priors::from_env())?;

        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(limit as usize);
        let outcome = if keyword_hit || near_vector_hit || ranked.is_empty() {
            SearchOutcome::Strong
        } else {
            SearchOutcome::WeakVectorOnly
        };
        Ok((self.hydrate_hits(&ranked)?, outcome))
    }

    /// Multiply each candidate chunk's fused score by the structural
    /// penalties it earns (see `priors.rs` for the rationale and
    /// magnitudes): test code — by path convention, or by a Rust
    /// `#[cfg(test)]`/`#[test]` content sniff since inline test modules
    /// live on production paths — module-level preamble (the chunk's owning
    /// symbol is the file's module), and sub-floor tiny fragments. The test
    /// penalty is suspended entirely when the query itself asks about tests.
    ///
    /// One point lookup per candidate (≤ 2×CANDIDATES rows, same
    /// prepared-statement pattern as `hydrate_hits`) — sub-millisecond
    /// against the ranking work already done.
    fn apply_ranking_priors(
        &self,
        query_text: &str,
        scores: &mut std::collections::HashMap<i64, f64>,
        priors: &crate::priors::Priors,
    ) -> Result<()> {
        let skip_test_penalty = priors.test == 1.0 || crate::priors::query_seeks_tests(query_text);
        let all_disabled = skip_test_penalty && priors.preamble == 1.0 && priors.tiny == 1.0;
        if all_disabled {
            return Ok(());
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.token_count, f.path, s.kind,
                    (f.lang = 'rust' AND (instr(c.content, '#[cfg(test)]') > 0
                                          OR instr(c.content, '#[test]') > 0))
             FROM chunks c JOIN files f ON f.id = c.file_id
             LEFT JOIN symbols s ON s.id = c.symbol_id WHERE c.id = ?1",
        )?;
        for (chunk_id, score) in scores.iter_mut() {
            let row: Option<(i64, String, Option<String>, bool)> = stmt
                .query_row([chunk_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?;
            let Some((token_count, path, kind, rust_test_sniff)) = row else {
                continue; // chunk deleted between rank and lookup
            };
            if !skip_test_penalty && (crate::priors::is_test_path(&path) || rust_test_sniff) {
                *score *= priors.test;
            }
            if kind.as_deref() == Some("module") {
                *score *= priors.preamble;
            }
            if token_count < priors.tiny_floor as i64 {
                *score *= priors.tiny;
            }
        }
        Ok(())
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

    /// A one-symbol, one-chunk file whose chunk is comfortably above the
    /// tiny-penalty floor and mentions `keyword` — the building block for
    /// the prior tests below.
    fn one_fn_file(qualname: &str, keyword: &str) -> FileIndex {
        let filler = "the quick brown fox jumps over the lazy dog and keeps on running ";
        FileIndex {
            symbols: vec![NewSymbol {
                name: qualname.rsplit('.').next().unwrap().into(),
                qualname: qualname.into(),
                kind: SymbolKind::Function,
                sig: None,
                start_line: 1,
                end_line: 4,
                parent: None,
                arity: Some(0),
            }],
            edges: vec![],
            chunks: vec![NewChunk {
                symbol: Some(0),
                start_line: 1,
                end_line: 4,
                content: format!(
                    "def f():\n    # {keyword} {keyword}\n    {filler}{filler}{filler}\n"
                ),
            }],
        }
    }

    #[test]
    fn test_path_chunk_ranks_below_identical_production_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store
            .replace_file(
                "billing.py",
                "python",
                &[1u8; 32],
                &one_fn_file("billing.charge", "frobnicate"),
            )
            .unwrap();
        store
            .replace_file(
                "tests/test_billing.py",
                "python",
                &[2u8; 32],
                &one_fn_file("tests.test_billing.test_charge", "frobnicate"),
            )
            .unwrap();

        let (hits, _) = store
            .search_hybrid_scored("frobnicate", None, None, 10)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].path, "billing.py",
            "identical keyword match: production chunk must outrank the test chunk"
        );
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn rust_cfg_test_sniff_demotes_inline_tests_on_a_production_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store
            .replace_file(
                "src/billing.rs",
                "rust",
                &[1u8; 32],
                &one_fn_file("src.billing.charge", "frobnicate"),
            )
            .unwrap();
        let mut sniffed = one_fn_file("src.orders.charge_twice", "frobnicate");
        sniffed.chunks[0].content = format!("#[cfg(test)]\n{}", sniffed.chunks[0].content);
        store
            .replace_file("src/orders.rs", "rust", &[2u8; 32], &sniffed)
            .unwrap();

        let (hits, _) = store
            .search_hybrid_scored("frobnicate", None, None, 10)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].path, "src/billing.rs",
            "the #[cfg(test)]-sniffed chunk must rank below the clean one \
             despite both living on production paths"
        );
    }

    #[test]
    fn test_seeking_query_suspends_the_test_penalty() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store
            .replace_file(
                "billing.py",
                "python",
                &[1u8; 32],
                &one_fn_file("billing.charge", "frobnicate"),
            )
            .unwrap();
        // The test file mentions the keyword more, so pure relevance puts it
        // first — and a query asking for tests must let it stay there.
        let mut test_idx = one_fn_file("tests.test_billing.test_charge", "frobnicate");
        test_idx.chunks[0]
            .content
            .push_str("frobnicate frobnicate tests\n");
        store
            .replace_file("tests/test_billing.py", "python", &[2u8; 32], &test_idx)
            .unwrap();

        let (hits, _) = store
            .search_hybrid_scored("tests for frobnicate", None, None, 10)
            .unwrap();
        assert_eq!(
            hits[0].path, "tests/test_billing.py",
            "a query asking about tests must not have test chunks penalized"
        );
    }

    #[test]
    fn module_preamble_and_tiny_chunks_are_demoted() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let filler = "the quick brown fox jumps over the lazy dog and keeps on running ";
        let idx = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "m".into(),
                    qualname: "m".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: 20,
                    parent: None,
                    arity: None,
                },
                NewSymbol {
                    name: "work".into(),
                    qualname: "m.work".into(),
                    kind: SymbolKind::Function,
                    sig: None,
                    start_line: 10,
                    end_line: 14,
                    parent: Some(0),
                    arity: Some(0),
                },
            ],
            edges: vec![],
            chunks: vec![
                // Module-preamble chunk: owned by the module symbol, and
                // tiny — an import line. Earns both penalties.
                NewChunk {
                    symbol: Some(0),
                    start_line: 1,
                    end_line: 1,
                    content: "from frobnicate import frobnicate\n".into(),
                },
                NewChunk {
                    symbol: Some(1),
                    start_line: 10,
                    end_line: 14,
                    content: format!("def work():\n    # frobnicate\n    {filler}{filler}\n"),
                },
            ],
        };
        store
            .replace_file("m.py", "python", &[1u8; 32], &idx)
            .unwrap();

        let (hits, _) = store
            .search_hybrid_scored("frobnicate", None, None, 10)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].qualname.as_deref(),
            Some("m.work"),
            "the function body must outrank the import-preamble fragment: {hits:?}"
        );
    }

    #[test]
    fn disabled_priors_leave_scores_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        store
            .replace_file(
                "tests/test_billing.py",
                "python",
                &[1u8; 32],
                &one_fn_file("tests.test_billing.test_charge", "frobnicate"),
            )
            .unwrap();
        let mut scores = std::collections::HashMap::from([(1i64, 0.5f64)]);
        let expected = scores.clone();
        let off = crate::priors::Priors {
            test: 1.0,
            preamble: 1.0,
            tiny: 1.0,
            tiny_floor: 32,
        };
        store
            .apply_ranking_priors("frobnicate", &mut scores, &off)
            .unwrap();
        assert_eq!(scores, expected, "all-1.0 coefficients must be a no-op");
    }

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
    fn corpus_tier_boundaries() {
        use crate::model::CorpusTier;
        assert_eq!(CorpusTier::from_chunks(0), CorpusTier::Tiny);
        assert_eq!(CorpusTier::from_chunks(199), CorpusTier::Tiny);
        assert_eq!(CorpusTier::from_chunks(200), CorpusTier::Small);
        assert_eq!(CorpusTier::from_chunks(1999), CorpusTier::Small);
        assert_eq!(CorpusTier::from_chunks(2000), CorpusTier::Medium);
    }

    /// Three chunks embedded on known axes so vec0's L2 distances are exact:
    /// near ones at distance 0, far ones at sqrt(2). A floor between the two
    /// must keep the near set, drop the far — and when nothing survives the
    /// floor AND keywords found nothing, the far set comes back as hits but
    /// the outcome degrades to `WeakVectorOnly`.
    #[test]
    fn knn_floor_drops_far_candidates_and_flags_weak_vector_only() {
        use super::SearchOutcome;
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
                    content: "alpha near".into(),
                },
                NewChunk {
                    symbol: Some(0),
                    start_line: 3,
                    end_line: 3,
                    content: "omega far".into(),
                },
            ],
        };
        store
            .replace_file("x.py", "python", &[1u8; 32], &idx)
            .unwrap();
        let missing = store.chunks_missing_embedding(10).unwrap();
        for (id, content, _hash) in &missing {
            let v = if content.contains("near") {
                vec![1.0, 0.0, 0.0, 0.0]
            } else {
                vec![0.0, 1.0, 0.0, 0.0]
            };
            store.put_embeddings(&[(*id, v)]).unwrap();
        }
        let q = [1.0f32, 0.0, 0.0, 0.0];

        // No keyword hits; floor keeps only the distance-0 chunk. Strong.
        let (hits, outcome) = store
            .search_hybrid_scored("zzz", Some(&q), Some(0.5), 10)
            .unwrap();
        assert_eq!(outcome, SearchOutcome::Strong);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].excerpt.contains("near"),
            "far chunk must be dropped"
        );

        // No keyword hits and NOTHING under the floor: nearest neighbors
        // still come back (never empty on a non-empty corpus) but flagged.
        let far_q = [0.0f32, 0.0, 1.0, 0.0];
        let (hits, outcome) = store
            .search_hybrid_scored("zzz", Some(&far_q), Some(0.5), 10)
            .unwrap();
        assert_eq!(outcome, SearchOutcome::WeakVectorOnly);
        assert_eq!(hits.len(), 2, "far candidates returned as a fallback");

        // A keyword hit rescues the outcome even when the floor drops
        // every KNN candidate.
        let (_hits, outcome) = store
            .search_hybrid_scored("alpha", Some(&far_q), Some(0.5), 10)
            .unwrap();
        assert_eq!(outcome, SearchOutcome::Strong);

        // No floor: everything fuses, outcome always Strong — the exact
        // historical behavior (and what mock-mode eval runs).
        let (hits, outcome) = store
            .search_hybrid_scored("zzz", Some(&far_q), None, 10)
            .unwrap();
        assert_eq!(outcome, SearchOutcome::Strong);
        assert_eq!(hits.len(), 2);
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
