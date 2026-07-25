use anyhow::Result;

use crate::model::Confidence;
use crate::Store;

struct EdgeRow {
    id: i64,
    src_file: i64,
    dst_name: String,
    dst_arity: Option<u32>,
}

/// Extract the last path segment of a possibly-qualified name, recognizing
/// `.` (Python/JS-style), `::` (Rust-style), and `/` (relative-path-style)
/// separators. Picks whichever separator occurs latest in the string, so
/// mixed names (e.g. `std::collections::HashMap`, `./utils/helper`) resolve
/// to their true last segment rather than falling back to Python-only `.`
/// splitting.
pub(crate) fn last_segment(name: &str) -> &str {
    let cut = name
        .rfind("::")
        .map(|i| i + 2)
        .into_iter()
        .chain(name.rfind('.').map(|i| i + 1))
        .chain(name.rfind('/').map(|i| i + 1))
        .max()
        .unwrap_or(0);
    &name[cut..]
}

impl Store {
    pub fn resolve_all_edges(&mut self) -> Result<u64> {
        self.resolve_where("1=1", &[])
    }

    pub fn resolve_edges_for_names(&mut self, names: &[String]) -> Result<u64> {
        let mut total = 0;
        for name in names {
            // Match edges whose dst_name equals the name OR ends with a
            // qualified suffix using any of the supported separators:
            // `.name` (Python/JS), `::name` (Rust), or `/name` (paths).
            // Exact-length suffix comparisons are used instead of LIKE to
            // avoid `_`/`%` being interpreted as wildcards.
            total += self.resolve_where(
                "(e.dst_name = ?1
                  OR substr(e.dst_name, -length(?1) - 1) = '.' || ?1
                  OR substr(e.dst_name, -length(?1) - 2) = '::' || ?1
                  OR substr(e.dst_name, -length(?1) - 1) = '/' || ?1)",
                &[name],
            )?;
        }
        Ok(total)
    }

    fn resolve_where(&mut self, cond: &str, params: &[&String]) -> Result<u64> {
        let sql = format!(
            "SELECT e.id, s.file_id, e.dst_name, e.dst_arity
             FROM edges e JOIN symbols s ON e.src_id = s.id
             WHERE {cond}"
        );
        let edges: Vec<EdgeRow> = {
            let mut stmt = self.conn.prepare(&sql)?;
            let p: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(|s| *s as &dyn rusqlite::ToSql).collect();
            let rows: Vec<EdgeRow> = stmt
                .query_map(p.as_slice(), |r| {
                    Ok(EdgeRow {
                        id: r.get(0)?,
                        src_file: r.get(1)?,
                        dst_name: r.get(2)?,
                        dst_arity: r.get(3)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            rows
        };

        let mut updated = 0;
        let tx = self.conn.transaction()?;
        for e in &edges {
            let last = last_segment(&e.dst_name);
            // candidates: (id, file_id, qualname, arity), same-file first then lowest id
            let cands: Vec<(i64, i64, String, Option<u32>)> = {
                let mut stmt = tx.prepare_cached(
                    "SELECT id, file_id, qualname, arity FROM symbols WHERE name = ?1
                     ORDER BY (file_id = ?2) DESC, id ASC",
                )?;
                let rows: Vec<(i64, i64, String, Option<u32>)> = stmt
                    .query_map(rusqlite::params![last, e.src_file], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })?
                    .collect::<Result<_, _>>()?;
                rows
            };

            let hit = cands
                .iter()
                .find(|c| c.2 == e.dst_name)
                .map(|c| (c.0, Confidence::Exact))
                .or_else(|| {
                    e.dst_arity.and_then(|a| {
                        cands
                            .iter()
                            .find(|c| c.3 == Some(a))
                            .map(|c| (c.0, Confidence::NameArity))
                    })
                })
                .or_else(|| cands.first().map(|c| (c.0, Confidence::NameOnly)));

            if let Some((dst_id, conf)) = hit {
                tx.execute(
                    "UPDATE edges SET dst_id = ?1, confidence = ?2 WHERE id = ?3",
                    rusqlite::params![dst_id, conf.as_str(), e.id],
                )?;
                updated += 1;
            } else {
                tx.execute(
                    "UPDATE edges SET dst_id = NULL, confidence = 'name_only' WHERE id = ?1",
                    [e.id],
                )?;
            }
        }
        tx.commit()?;
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use crate::model::*;
    use crate::Store;

    fn store_with(files: &[(&str, FileIndex)]) -> Store {
        let dir = tempfile::tempdir().unwrap();
        // keep tempdir alive by leaking: fine for tests
        let dir = Box::leak(Box::new(dir));
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        for (i, (path, idx)) in files.iter().enumerate() {
            store
                .replace_file(path, "python", &[i as u8; 32], idx)
                .unwrap();
        }
        store
    }

    fn sym(name: &str, qual: &str, kind: SymbolKind, arity: Option<u32>) -> NewSymbol {
        NewSymbol {
            name: name.into(),
            qualname: qual.into(),
            kind,
            sig: None,
            start_line: 1,
            end_line: 2,
            parent: None,
            arity,
        }
    }

    #[test]
    fn precedence_exact_then_arity_then_name() {
        let callers = FileIndex {
            symbols: vec![sym("caller", "a.caller", SymbolKind::Function, Some(0))],
            edges: vec![
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "b.target".into(),
                    dst_arity: Some(2),
                },
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "target".into(),
                    dst_arity: Some(1),
                },
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "target".into(),
                    dst_arity: Some(9),
                },
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "missing".into(),
                    dst_arity: None,
                },
            ],
            chunks: vec![],
        };
        let callees = FileIndex {
            symbols: vec![
                sym("target", "b.target", SymbolKind::Function, Some(2)),
                sym("target", "c.target", SymbolKind::Function, Some(1)),
            ],
            edges: vec![],
            chunks: vec![],
        };
        let mut store = store_with(&[("a.py", callers), ("b.py", callees)]);
        let n = store.resolve_all_edges().unwrap();
        assert_eq!(n, 3); // 'missing' stays unresolved

        let rows: Vec<(String, Option<String>, String)> = store
            .conn
            .prepare(
                "SELECT e.dst_name, s.qualname, e.confidence
                      FROM edges e LEFT JOIN symbols s ON e.dst_id = s.id ORDER BY e.id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(
            rows[0],
            ("b.target".into(), Some("b.target".into()), "exact".into())
        );
        assert_eq!(
            rows[1],
            (
                "target".into(),
                Some("c.target".into()),
                "name_arity".into()
            )
        );
        // arity 9 matches nothing exactly → name_only, tie → lowest id (b.target)
        assert_eq!(
            rows[2],
            ("target".into(), Some("b.target".into()), "name_only".into())
        );
        assert_eq!(rows[3], ("missing".into(), None, "name_only".into()));
    }

    #[test]
    fn incremental_reresolve_by_name() {
        let callers = FileIndex {
            symbols: vec![sym("caller", "a.caller", SymbolKind::Function, Some(0))],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Calls,
                dst_name: "late".into(),
                dst_arity: Some(0),
            }],
            chunks: vec![],
        };
        let mut store = store_with(&[("a.py", callers)]);
        store.resolve_all_edges().unwrap();
        // target arrives later (new file indexed)
        let newfile = FileIndex {
            symbols: vec![sym("late", "z.late", SymbolKind::Function, Some(0))],
            edges: vec![],
            chunks: vec![],
        };
        store
            .replace_file("z.py", "python", &[9u8; 32], &newfile)
            .unwrap();
        let n = store.resolve_edges_for_names(&["late".into()]).unwrap();
        assert_eq!(n, 1);
        let q: Option<String> = store
            .conn
            .query_row(
                "SELECT s.qualname FROM edges e JOIN symbols s ON e.dst_id = s.id
             WHERE e.dst_name = 'late'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(q.as_deref(), Some("z.late"));
    }

    #[test]
    fn suffix_matching_literal_underscore_not_wildcard() {
        // Test that suffix matching doesn't treat '_' as a wildcard.
        // When looking for symbol '_data' via resolve_edges_for_names(&["_data"]),
        // the pattern '%.._data' should match 'mod._data' but NOT 'mod.adata'
        // (underscore in LIKE is a wildcard matching any single char)
        let caller = FileIndex {
            symbols: vec![sym("caller", "a.caller", SymbolKind::Function, Some(0))],
            edges: vec![
                // Edge that should match _data (exact unqualified name)
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "_data".into(),
                    dst_arity: None,
                },
                // Edge that should match _data (qualified suffix)
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "mod._data".into(),
                    dst_arity: None,
                },
                // This edge should NOT match _data, but vulnerable LIKE pattern would:
                // Pattern '%.._data' matches 'mod.adata' because _ is wildcard in LIKE
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Calls,
                    dst_name: "mod.aData".into(),
                    dst_arity: None,
                },
            ],
            chunks: vec![],
        };
        let mut store = store_with(&[("a.py", caller)]);

        // Add symbol with underscore prefix in name
        let defs = FileIndex {
            symbols: vec![sym("_data", "lib._data", SymbolKind::Function, None)],
            edges: vec![],
            chunks: vec![],
        };
        store
            .replace_file("lib.py", "python", &[2u8; 32], &defs)
            .unwrap();

        // Resolve by name - should only match edges ending with literal '._data',
        // NOT edges with 'mod.aData' even though the buggy LIKE pattern would select it
        let n = store.resolve_edges_for_names(&["_data".into()]).unwrap();
        assert_eq!(n, 2); // Only the two _data edges should resolve

        // Verify which edges got resolved
        let resolved: Vec<(String, Option<String>)> = store.conn
            .prepare("SELECT e.dst_name, s.qualname FROM edges e LEFT JOIN symbols s ON e.dst_id = s.id ORDER BY e.id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(resolved[0], ("_data".into(), Some("lib._data".into())));
        assert_eq!(resolved[1], ("mod._data".into(), Some("lib._data".into())));
        assert_eq!(resolved[2], ("mod.aData".into(), None)); // Should still be NULL
    }

    #[test]
    fn rust_style_double_colon_path_resolves_to_last_segment() {
        // dogfood evidence: Rust import edges like `std::collections::HashMap`
        // never resolved because last-segment extraction only split on '.'.
        let caller = FileIndex {
            symbols: vec![sym("caller", "a::caller", SymbolKind::Function, None)],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Imports,
                dst_name: "std::collections::HashMap".into(),
                dst_arity: None,
            }],
            chunks: vec![],
        };
        let defs = FileIndex {
            symbols: vec![sym(
                "HashMap",
                "std::collections::HashMap",
                SymbolKind::Class,
                None,
            )],
            edges: vec![],
            chunks: vec![],
        };
        let mut store = store_with(&[("a.rs", caller), ("collections.rs", defs)]);
        let n = store.resolve_all_edges().unwrap();
        assert_eq!(n, 1);

        let q: Option<String> = store
            .conn
            .query_row(
                "SELECT s.qualname FROM edges e JOIN symbols s ON e.dst_id = s.id
                 WHERE e.dst_name = 'std::collections::HashMap'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(q.as_deref(), Some("std::collections::HashMap"));
    }

    #[test]
    fn relative_path_style_import_resolves_to_last_segment() {
        // dogfood evidence: relative-path import edges like `./utils/helper`
        // never resolved because last-segment extraction only split on '.'.
        let caller = FileIndex {
            symbols: vec![sym("caller", "app.caller", SymbolKind::Function, None)],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Imports,
                dst_name: "./utils/helper".into(),
                dst_arity: None,
            }],
            chunks: vec![],
        };
        let defs = FileIndex {
            symbols: vec![sym("helper", "utils.helper", SymbolKind::Function, None)],
            edges: vec![],
            chunks: vec![],
        };
        let mut store = store_with(&[("app.py", caller), ("utils.py", defs)]);
        let n = store.resolve_all_edges().unwrap();
        assert_eq!(n, 1);

        let q: Option<String> = store
            .conn
            .query_row(
                "SELECT s.qualname FROM edges e JOIN symbols s ON e.dst_id = s.id
                 WHERE e.dst_name = './utils/helper'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(q.as_deref(), Some("utils.helper"));
    }

    #[test]
    fn resolve_edges_for_names_matches_double_colon_and_slash_suffixes() {
        // Extends the exact-suffix matching used by incremental re-resolution
        // (resolve_edges_for_names) to the '::' and '/' separators, not just '.'.
        let caller = FileIndex {
            symbols: vec![sym("caller", "a::caller", SymbolKind::Function, None)],
            edges: vec![
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Imports,
                    dst_name: "std::collections::HashMap".into(),
                    dst_arity: None,
                },
                NewEdge {
                    src: 0,
                    kind: EdgeKind::Imports,
                    dst_name: "./utils/helper".into(),
                    dst_arity: None,
                },
            ],
            chunks: vec![],
        };
        let mut store = store_with(&[("a.rs", caller)]);
        store.resolve_all_edges().unwrap(); // both edges start unresolved

        let late = FileIndex {
            symbols: vec![
                sym(
                    "HashMap",
                    "std::collections::HashMap",
                    SymbolKind::Class,
                    None,
                ),
                sym("helper", "utils.helper", SymbolKind::Function, None),
            ],
            edges: vec![],
            chunks: vec![],
        };
        store
            .replace_file("late.rs", "rust", &[3u8; 32], &late)
            .unwrap();

        let n = store
            .resolve_edges_for_names(&["HashMap".into(), "helper".into()])
            .unwrap();
        assert_eq!(n, 2);

        let resolved: Vec<(String, Option<String>)> = store.conn
            .prepare("SELECT e.dst_name, s.qualname FROM edges e LEFT JOIN symbols s ON e.dst_id = s.id ORDER BY e.id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            resolved[0],
            (
                "std::collections::HashMap".into(),
                Some("std::collections::HashMap".into())
            )
        );
        assert_eq!(
            resolved[1],
            ("./utils/helper".into(), Some("utils.helper".into()))
        );
    }
}
