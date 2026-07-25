//! Symbol resolution and source lookup: turning a user-supplied name into a
//! concrete symbol (or a useful "did you mean" answer), and fetching the
//! source chunks that belong to a resolved symbol.

use anyhow::Result;
use rusqlite::OptionalExtension;

use crate::resolve::{last_segment, suffix_match_sql};
use crate::Store;

const NAME_MATCH_LIMIT: i64 = 11;
const SUGGESTION_LIMIT: i64 = 5;
/// Hard row cap for `impact_of`, independent of `max_depth`: a highly
/// connected graph must never return an unbounded result set.
const IMPACT_ROW_CAP: u32 = 500;

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

/// One endpoint of a graph edge, hydrated for display: the caller/callee/
/// import-partner symbol, the raw name the edge was recorded under, and
/// whether that specific edge resolved to a real symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeHit {
    pub symbol: SymbolInfo,
    pub via_name: String,
    pub confidence: Option<String>,
    pub depth: u32,
}

/// Build the placeholder `SymbolInfo` for an edge endpoint that never
/// resolved to a real symbol row (`dst_id IS NULL`): `id: -1` flags it as
/// synthetic, `qualname` carries the raw `dst_name` so callers still have
/// something to show, and `name` is best-effort via `last_segment`.
fn synthetic_symbol(dst_name: &str) -> SymbolInfo {
    SymbolInfo {
        id: -1,
        name: last_segment(dst_name).to_string(),
        qualname: dst_name.to_string(),
        kind: "unknown".to_string(),
        sig: None,
        path: String::new(),
        start_line: 0,
        end_line: 0,
    }
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

    /// Direct callers of `symbol_id` (edges kind='calls' with dst_id =
    /// symbol_id), plus name-only matches (dst_id NULL AND dst_name
    /// suffix-matches the symbol's name) reported with confidence None.
    /// `depth` walks callers-of-callers over resolved edges only.
    pub fn callers_of(&self, symbol_id: i64, depth: u32, limit: u32) -> Result<Vec<EdgeHit>> {
        let Some(info) = self.symbol_info(symbol_id)? else {
            return Ok(vec![]);
        };
        self.walk_callers(symbol_id, &info.name, depth, limit)
    }

    /// Outgoing calls from `symbol_id`: resolved edges hydrate the callee
    /// via `symbols`; unresolved edges (dst_id NULL) produce an `EdgeHit`
    /// with a synthetic `SymbolInfo` (id -1, qualname = dst_name) and
    /// confidence None. `depth` walks callees-of-callees over resolved
    /// edges only (an unresolved callee has no real id to keep walking from).
    pub fn callees_of(&self, symbol_id: i64, depth: u32, limit: u32) -> Result<Vec<EdgeHit>> {
        let max_depth = depth.max(1);
        let sql = "WITH RECURSIVE callees(dst_id, dst_name, confidence, depth) AS (
                SELECT e.dst_id, e.dst_name, e.confidence, 1
                FROM edges e
                WHERE e.kind = 'calls' AND e.src_id = ?1

                UNION ALL

                SELECT e.dst_id, e.dst_name, e.confidence, c.depth + 1
                FROM edges e
                JOIN callees c ON e.src_id = c.dst_id
                WHERE e.kind = 'calls' AND c.depth < ?2
            )
            SELECT c.dst_id, s.id, s.name, s.qualname, s.kind, s.sig, f.path,
                   s.start_line, s.end_line, c.dst_name, c.confidence, c.depth
            FROM callees c
            LEFT JOIN symbols s ON s.id = c.dst_id
            LEFT JOIN files f ON f.id = s.file_id
            ORDER BY c.depth ASC, c.dst_id ASC
            LIMIT ?3";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params![symbol_id, max_depth, limit], |r| {
                let dst_id: Option<i64> = r.get(0)?;
                let via_name: String = r.get(9)?;
                let confidence: String = r.get(10)?;
                let depth: i64 = r.get(11)?;
                let symbol = match dst_id {
                    Some(id) => SymbolInfo {
                        id,
                        name: r.get(2)?,
                        qualname: r.get(3)?,
                        kind: r.get(4)?,
                        sig: r.get(5)?,
                        path: r.get(6)?,
                        start_line: r.get(7)?,
                        end_line: r.get(8)?,
                    },
                    None => synthetic_symbol(&via_name),
                };
                Ok(EdgeHit {
                    symbol,
                    via_name,
                    confidence: dst_id.is_some().then_some(confidence),
                    depth: depth as u32,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Imports touching the file of `symbol_id`: modules this file imports
    /// (outgoing from its module symbol; unresolved targets get a synthetic
    /// `SymbolInfo` like `callees_of`) and files importing this module
    /// (incoming, resolved matches only — dst_id must already point at this
    /// file's module symbol to know which import statement is "this one").
    /// Empty on both sides when `symbol_id`'s file has no module symbol.
    pub fn imports_of(&self, symbol_id: i64) -> Result<(Vec<EdgeHit>, Vec<EdgeHit>)> {
        let file_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT file_id FROM symbols WHERE id = ?1",
                [symbol_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(file_id) = file_id else {
            return Ok((vec![], vec![]));
        };
        let module_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM symbols WHERE file_id = ?1 AND kind = 'module'",
                [file_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(module_id) = module_id else {
            return Ok((vec![], vec![]));
        };

        let mut out_stmt = self.conn.prepare(
            "SELECT e.dst_id, s.id, s.name, s.qualname, s.kind, s.sig, f.path,
                    s.start_line, s.end_line, e.dst_name, e.confidence
             FROM edges e
             LEFT JOIN symbols s ON s.id = e.dst_id
             LEFT JOIN files f ON f.id = s.file_id
             WHERE e.kind = 'imports' AND e.src_id = ?1
             ORDER BY e.id",
        )?;
        let outgoing = out_stmt
            .query_map([module_id], |r| {
                let dst_id: Option<i64> = r.get(0)?;
                let via_name: String = r.get(9)?;
                let confidence: String = r.get(10)?;
                let symbol = match dst_id {
                    Some(id) => SymbolInfo {
                        id,
                        name: r.get(2)?,
                        qualname: r.get(3)?,
                        kind: r.get(4)?,
                        sig: r.get(5)?,
                        path: r.get(6)?,
                        start_line: r.get(7)?,
                        end_line: r.get(8)?,
                    },
                    None => synthetic_symbol(&via_name),
                };
                Ok(EdgeHit {
                    symbol,
                    via_name,
                    confidence: dst_id.is_some().then_some(confidence),
                    depth: 1,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;

        let mut in_stmt = self.conn.prepare(
            "SELECT s.id, s.name, s.qualname, s.kind, s.sig, f.path, s.start_line, s.end_line,
                    e.dst_name, e.confidence
             FROM edges e
             JOIN symbols s ON s.id = e.src_id
             JOIN files f ON f.id = s.file_id
             WHERE e.kind = 'imports' AND e.dst_id = ?1
             ORDER BY e.id",
        )?;
        let incoming = in_stmt
            .query_map([module_id], |r| {
                let symbol = symbol_info_from_row(r)?;
                let via_name: String = r.get(8)?;
                let confidence: String = r.get(9)?;
                Ok(EdgeHit {
                    symbol,
                    via_name,
                    confidence: Some(confidence),
                    depth: 1,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;

        Ok((outgoing, incoming))
    }

    /// Transitive callers via recursive CTE over resolved call edges,
    /// grouped by depth (results are ordered depth-ascending), max
    /// `max_depth`, hard row cap 500 regardless of `max_depth`. Depth-1
    /// rows include unresolved name-matches (see `callers_of`).
    pub fn impact_of(&self, symbol_id: i64, max_depth: u32) -> Result<Vec<EdgeHit>> {
        let Some(info) = self.symbol_info(symbol_id)? else {
            return Ok(vec![]);
        };
        self.walk_callers(symbol_id, &info.name, max_depth, IMPACT_ROW_CAP)
    }

    /// Shared recursive-caller walk backing `callers_of` and `impact_of`.
    /// Depth 1 seeds from edges resolved to `symbol_id` plus unresolved
    /// (dst_id NULL) edges whose `dst_name` suffix-matches `name`; deeper
    /// levels only follow resolved edges (`e.dst_id = <known caller id>`
    /// can never match a NULL dst_id), since an unresolved edge's caller is
    /// still a real symbol id (src_id) — it's just that specific edge into
    /// the previous frontier that didn't resolve.
    fn walk_callers(
        &self,
        symbol_id: i64,
        name: &str,
        max_depth: u32,
        row_cap: u32,
    ) -> Result<Vec<EdgeHit>> {
        let max_depth = max_depth.max(1);
        let suffix = suffix_match_sql("e.dst_name", "?2");
        let sql = format!(
            "WITH RECURSIVE callers(caller_id, via_name, confidence, resolved, depth) AS (
                SELECT e.src_id, e.dst_name, e.confidence, (e.dst_id IS NOT NULL), 1
                FROM edges e
                WHERE e.kind = 'calls'
                  AND (e.dst_id = ?1 OR (e.dst_id IS NULL AND {suffix}))

                UNION ALL

                SELECT e.src_id, e.dst_name, e.confidence, 1, c.depth + 1
                FROM edges e
                JOIN callers c ON e.dst_id = c.caller_id
                WHERE e.kind = 'calls' AND c.depth < ?3
            )
            SELECT s.id, s.name, s.qualname, s.kind, s.sig, f.path, s.start_line, s.end_line,
                   callers.via_name, callers.confidence, callers.resolved, callers.depth
            FROM callers
            JOIN symbols s ON s.id = callers.caller_id
            JOIN files f ON f.id = s.file_id
            ORDER BY callers.depth ASC, s.id ASC
            LIMIT ?4"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params![symbol_id, name, max_depth, row_cap],
                |r| {
                    let symbol = symbol_info_from_row(r)?;
                    let via_name: String = r.get(8)?;
                    let confidence: String = r.get(9)?;
                    let resolved: bool = r.get(10)?;
                    let depth: i64 = r.get(11)?;
                    Ok(EdgeHit {
                        symbol,
                        via_name,
                        confidence: resolved.then_some(confidence),
                        depth: depth as u32,
                    })
                },
            )?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
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

    fn fn_sym(name: &str, qual: &str, arity: Option<u32>) -> NewSymbol {
        NewSymbol {
            name: name.into(),
            qualname: qual.into(),
            kind: SymbolKind::Function,
            sig: None,
            start_line: 1,
            end_line: 2,
            parent: None,
            arity,
        }
    }

    fn module_sym(name: &str, qual: &str) -> NewSymbol {
        NewSymbol {
            name: name.into(),
            qualname: qual.into(),
            kind: SymbolKind::Module,
            sig: None,
            start_line: 1,
            end_line: 1,
            parent: None,
            arity: None,
        }
    }

    fn id_of(store: &Store, qualname: &str) -> i64 {
        store
            .conn_ref_for_tests()
            .query_row(
                "SELECT id FROM symbols WHERE qualname = ?1",
                [qualname],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// `a.main` calls `helper` (resolved) and `leaf`; `b.helper` calls
    /// `leaf`; `c.leaf` is a leaf function with no outgoing calls.
    ///
    /// `main`'s call to `leaf` is deliberately forced back to unresolved
    /// (dst_id NULL) after `resolve_all_edges` runs. It can't be built
    /// unresolved directly: a symbol named `leaf` genuinely exists, so the
    /// resolver's name-only fallback would always assign it a dst_id. The
    /// raw-SQL override below simulates the real-world case this query
    /// layer has to handle regardless — an edge added (or a symbol
    /// removed) after the last resolution pass, leaving dst_id NULL on an
    /// edge whose dst_name still suffix-matches a live symbol.
    fn graph_fixture() -> (Store, i64, i64, i64) {
        let dir = tempfile::tempdir().unwrap();
        let dir = Box::leak(Box::new(dir));
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();

        let a = FileIndex {
            symbols: vec![fn_sym("main", "a.main", Some(0))],
            edges: vec![
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "helper".into(),
                    dst_arity: Some(0),
                },
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "leaf".into(),
                    dst_arity: Some(0),
                },
            ],
            chunks: vec![],
        };
        store
            .replace_file("a.py", "python", &[1u8; 32], &a)
            .unwrap();

        let b = FileIndex {
            symbols: vec![fn_sym("helper", "b.helper", Some(0))],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Calls,
                dst_name: "leaf".into(),
                dst_arity: Some(0),
            }],
            chunks: vec![],
        };
        store
            .replace_file("b.py", "python", &[2u8; 32], &b)
            .unwrap();

        let c = FileIndex {
            symbols: vec![fn_sym("leaf", "c.leaf", Some(0))],
            edges: vec![],
            chunks: vec![],
        };
        store
            .replace_file("c.py", "python", &[3u8; 32], &c)
            .unwrap();

        store.resolve_all_edges().unwrap();

        store
            .conn_ref_for_tests()
            .execute(
                "UPDATE edges SET dst_id = NULL, confidence = 'name_only'
                 WHERE dst_name = 'leaf'
                   AND src_id = (SELECT id FROM symbols WHERE qualname = 'a.main')",
                [],
            )
            .unwrap();

        let main_id = id_of(&store, "a.main");
        let helper_id = id_of(&store, "b.helper");
        let leaf_id = id_of(&store, "c.leaf");
        (store, main_id, helper_id, leaf_id)
    }

    #[test]
    fn callers_of_depth_1_includes_resolved_and_unresolved_name_match() {
        let (store, _main_id, helper_id, leaf_id) = graph_fixture();
        let hits = store.callers_of(leaf_id, 1, 50).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");

        let helper_hit = hits
            .iter()
            .find(|h| h.symbol.id == helper_id)
            .expect("helper is a resolved caller of leaf");
        assert_eq!(helper_hit.depth, 1);
        assert!(helper_hit.confidence.is_some());
        assert_eq!(helper_hit.via_name, "leaf");

        let main_hit = hits
            .iter()
            .find(|h| h.symbol.qualname == "a.main")
            .expect("main is an unresolved-name-match caller of leaf");
        assert_eq!(main_hit.depth, 1);
        assert_eq!(
            main_hit.confidence, None,
            "dst_id IS NULL must render as confidence None"
        );
        assert_eq!(main_hit.via_name, "leaf");
    }

    #[test]
    fn callers_of_depth_2_walks_callers_of_callers() {
        let (store, main_id, _helper_id, leaf_id) = graph_fixture();
        let hits = store.callers_of(leaf_id, 2, 50).unwrap();

        let depth2_main = hits
            .iter()
            .find(|h| h.depth == 2 && h.symbol.id == main_id)
            .expect("main should appear at depth 2, reached via helper");
        assert!(depth2_main.confidence.is_some());
        assert_eq!(depth2_main.via_name, "helper");

        // main also still appears at depth 1 (its own unresolved call into
        // leaf) — duplicate entries at different depths are expected, not
        // deduped, since they represent distinct edges in the call graph.
        assert!(
            hits.iter()
                .any(|h| h.depth == 1 && h.symbol.id == main_id && h.confidence.is_none()),
            "{hits:?}"
        );
    }

    #[test]
    fn callees_of_resolved_and_unresolved_synthetic() {
        let (store, main_id, helper_id, _leaf_id) = graph_fixture();
        let hits = store.callees_of(main_id, 1, 50).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");

        let helper_hit = hits
            .iter()
            .find(|h| h.symbol.id == helper_id)
            .expect("resolved callee helper");
        assert_eq!(helper_hit.depth, 1);
        assert!(helper_hit.confidence.is_some());

        let leaf_hit = hits
            .iter()
            .find(|h| h.via_name == "leaf")
            .expect("unresolved callee leaf");
        assert_eq!(
            leaf_hit.symbol.id, -1,
            "unresolved callee must be a synthetic SymbolInfo"
        );
        assert_eq!(leaf_hit.symbol.qualname, "leaf");
        assert_eq!(leaf_hit.confidence, None);
    }

    #[test]
    fn callees_of_depth_2_walks_callees_of_callees() {
        let (store, main_id, _helper_id, leaf_id) = graph_fixture();
        let hits = store.callees_of(main_id, 2, 50).unwrap();

        // helper->leaf is a genuinely resolved edge (untouched by the
        // fixture's forced-unresolved override, which only nulls main's own
        // edge into leaf), so walking callees-of-callees from main should
        // reach leaf a second time at depth 2, this time resolved.
        let depth2_leaf = hits
            .iter()
            .find(|h| h.depth == 2 && h.symbol.id == leaf_id)
            .expect("leaf should appear at depth 2 via helper");
        assert!(depth2_leaf.confidence.is_some());
        assert_eq!(depth2_leaf.via_name, "leaf");

        // the depth-1 unresolved synthetic leaf (main's own forced-null
        // edge) is still present alongside it.
        assert!(
            hits.iter()
                .any(|h| h.depth == 1 && h.symbol.id == -1 && h.via_name == "leaf"),
            "{hits:?}"
        );
    }

    #[test]
    fn imports_of_outgoing_resolved_and_unresolved_incoming_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let dir = Box::leak(Box::new(dir));
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();

        let app = FileIndex {
            symbols: vec![module_sym("app", "app")],
            edges: vec![
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Imports,
                    dst_name: "utils".into(),
                    dst_arity: None,
                },
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Imports,
                    dst_name: "totally_unknown_target".into(),
                    dst_arity: None,
                },
            ],
            chunks: vec![],
        };
        store
            .replace_file("app.py", "python", &[1u8; 32], &app)
            .unwrap();

        let utils = FileIndex {
            symbols: vec![module_sym("utils", "utils")],
            edges: vec![],
            chunks: vec![],
        };
        store
            .replace_file("utils.py", "python", &[2u8; 32], &utils)
            .unwrap();

        store.resolve_all_edges().unwrap();

        let app_id = id_of(&store, "app");
        let utils_id = id_of(&store, "utils");

        let (outgoing, _incoming) = store.imports_of(app_id).unwrap();
        assert_eq!(outgoing.len(), 2, "{outgoing:?}");
        let utils_hit = outgoing
            .iter()
            .find(|h| h.symbol.id == utils_id)
            .expect("resolved import of utils");
        assert!(utils_hit.confidence.is_some());
        let unknown_hit = outgoing
            .iter()
            .find(|h| h.via_name == "totally_unknown_target")
            .expect("genuinely unresolved import (no symbol has this name at all)");
        assert_eq!(unknown_hit.symbol.id, -1);
        assert_eq!(unknown_hit.symbol.qualname, "totally_unknown_target");
        assert_eq!(unknown_hit.confidence, None);

        let (_outgoing2, incoming) = store.imports_of(utils_id).unwrap();
        assert_eq!(incoming.len(), 1, "{incoming:?}");
        assert_eq!(incoming[0].symbol.id, app_id);
        assert!(incoming[0].confidence.is_some());
    }

    #[test]
    fn impact_of_groups_by_depth() {
        let (store, main_id, helper_id, leaf_id) = graph_fixture();
        let hits = store.impact_of(leaf_id, 5).unwrap();

        let depth1: Vec<_> = hits.iter().filter(|h| h.depth == 1).collect();
        assert_eq!(depth1.len(), 2, "{hits:?}");
        assert!(depth1
            .iter()
            .any(|h| h.symbol.id == helper_id && h.confidence.is_some()));
        assert!(depth1
            .iter()
            .any(|h| h.symbol.id == main_id && h.confidence.is_none()));

        let depth2: Vec<_> = hits.iter().filter(|h| h.depth == 2).collect();
        assert_eq!(depth2.len(), 1, "{hits:?}");
        assert_eq!(depth2[0].symbol.id, main_id);
        assert!(depth2[0].confidence.is_some());
    }

    #[test]
    fn impact_of_hard_row_cap_500_and_callers_of_limit_respected() {
        let dir = tempfile::tempdir().unwrap();
        let dir = Box::leak(Box::new(dir));
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();

        let target = FileIndex {
            symbols: vec![fn_sym("target", "t.target", Some(0))],
            edges: vec![],
            chunks: vec![],
        };
        store
            .replace_file("t.py", "python", &[0u8; 32], &target)
            .unwrap();

        // 510 distinct direct callers of `target`, more than the 500 hard cap.
        for i in 0..510u32 {
            let idx = FileIndex {
                symbols: vec![fn_sym("caller", &format!("callers.c{i}"), Some(0))],
                edges: vec![NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "target".into(),
                    dst_arity: Some(0),
                }],
                chunks: vec![],
            };
            store
                .replace_file(&format!("c{i}.py"), "python", &[1u8; 32], &idx)
                .unwrap();
        }
        store.resolve_all_edges().unwrap();

        let target_id = id_of(&store, "t.target");

        let hits = store.impact_of(target_id, 1).unwrap();
        assert_eq!(
            hits.len(),
            500,
            "impact_of must cap at 500 rows regardless of max_depth or caller count"
        );

        // callers_of's own explicit `limit` param is honored independent of
        // the impact_of-specific hard cap.
        let limited = store.callers_of(target_id, 1, 10).unwrap();
        assert_eq!(limited.len(), 10);
    }
}
