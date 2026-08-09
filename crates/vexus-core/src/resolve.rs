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
/// `.` (Python/JS-style), `::` (Rust-style), `/` (relative-path-style), and
/// `\` (PHP-namespace-style) separators. Picks whichever separator occurs
/// latest in the string, so mixed names (e.g. `std::collections::HashMap`,
/// `./utils/helper`, `App\Service\Mailer::send`) resolve to their true last
/// segment rather than falling back to Python-only `.` splitting.
pub(crate) fn last_segment(name: &str) -> &str {
    let cut = name
        .rfind("::")
        .map(|i| i + 2)
        .into_iter()
        .chain(name.rfind('.').map(|i| i + 1))
        .chain(name.rfind('/').map(|i| i + 1))
        .chain(name.rfind('\\').map(|i| i + 1))
        .max()
        .unwrap_or(0);
    &name[cut..]
}

/// SQL boolean expression matching `dst_expr` (a `dst_name`-shaped column
/// reference, e.g. `e.dst_name`) against the already-bound parameter
/// `param` (e.g. `?1`): either exactly equal, or a qualified suffix using
/// any of the supported separators (`.` Python/JS, `::` Rust, `/` paths,
/// `\` PHP namespaces). Exact-length `substr` comparisons are used instead
/// of `LIKE` so that `_`/`%` in the target name are never interpreted as
/// wildcards. (SQLite single-quoted literals treat `\` literally — no
/// escaping needed.)
pub(crate) fn suffix_match_sql(dst_expr: &str, param: &str) -> String {
    format!(
        "({dst_expr} = {param}
          OR substr({dst_expr}, -length({param}) - 1) = '.' || {param}
          OR substr({dst_expr}, -length({param}) - 2) = '::' || {param}
          OR substr({dst_expr}, -length({param}) - 1) = '/' || {param}
          OR substr({dst_expr}, -length({param}) - 1) = '\\' || {param})"
    )
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
            total += self.resolve_where(&suffix_match_sql("e.dst_name", "?1"), &[name])?;
        }
        Ok(total)
    }

    /// How many same-name, same-arity candidates a cross-file match may
    /// compete with before the edge is left unresolved instead. A `push`
    /// with dozens of same-arity definitions carries no information; a
    /// handful is a plausible guess worth labelling `[name_arity]`.
    const AMBIGUOUS_CANDIDATE_LIMIT: usize = 5;

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

            // A receiver-qualified call site (`metrics.push(x)`) extracts as
            // the bare method name, so `push` proposes every `push` in the
            // repo as a candidate. The lower two tiers therefore refuse to
            // guess out of a crowd: an unresolved row is honest and renders
            // compactly, while a confidently wrong `[name_arity]` edge is
            // indistinguishable from a real one downstream (callers,
            // callees, impact, and explore's neighbour expansion all treat
            // any non-null dst_id as fact). Same-file candidates are exempt
            // — `cands` is ordered same-file-first, so a local definition
            // stays the answer no matter how common the name is elsewhere.
            let src_file = e.src_file;
            let hit = cands
                .iter()
                .find(|c| c.2 == e.dst_name)
                .map(|c| (c.0, Confidence::Exact))
                .or_else(|| {
                    e.dst_arity.and_then(|a| {
                        let matching: Vec<&(i64, i64, String, Option<u32>)> =
                            cands.iter().filter(|c| c.3 == Some(a)).collect();
                        matching
                            .iter()
                            .find(|c| c.1 == src_file)
                            .copied()
                            .or_else(|| {
                                if matching.len() <= Self::AMBIGUOUS_CANDIDATE_LIMIT {
                                    matching.first().copied()
                                } else {
                                    None
                                }
                            })
                            .map(|c| (c.0, Confidence::NameArity))
                    })
                })
                .or_else(|| {
                    // Name-only is the weakest signal there is; take it only
                    // when the name is unambiguous repo-wide or the match is
                    // in the caller's own file.
                    cands
                        .iter()
                        .find(|c| c.1 == src_file)
                        .or_else(|| {
                            if cands.len() == 1 {
                                cands.first()
                            } else {
                                None
                            }
                        })
                        .map(|c| (c.0, Confidence::NameOnly))
                });

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
        // 'missing' has no candidate at all; the arity-9 `target` has two
        // cross-file candidates and no arity evidence to choose between
        // them, so it stays unresolved rather than guessing (see below).
        assert_eq!(n, 2);

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
        // Arity 9 matches no candidate's arity, and both `target`s live in
        // another file — with nothing to separate them, picking the
        // lowest-id one would be a coin flip rendered as a fact.
        assert_eq!(rows[2], ("target".into(), None, "name_only".into()));
        assert_eq!(rows[3], ("missing".into(), None, "name_only".into()));
    }

    /// A receiver-qualified call (`metrics.push(x)`) extracts as the bare
    /// method name, so the candidate set is every same-named symbol in the
    /// repo. Matching one of a crowd by arity alone produced a confidently
    /// wrong edge indistinguishable from a real one; past the ambiguity
    /// limit the edge must stay unresolved instead.
    #[test]
    fn a_crowded_cross_file_name_stays_unresolved() {
        let caller = FileIndex {
            symbols: vec![sym("caller", "a.caller", SymbolKind::Function, Some(0))],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Calls,
                dst_name: "push".into(),
                dst_arity: Some(1),
            }],
            chunks: vec![],
        };
        // Seven unrelated one-argument `push` definitions, none in a.py.
        let crowd = FileIndex {
            symbols: (0..7)
                .map(|i| sym("push", &format!("b.T{i}.push"), SymbolKind::Method, Some(1)))
                .collect(),
            edges: vec![],
            chunks: vec![],
        };
        let mut store = store_with(&[("a.py", caller), ("b.py", crowd)]);
        store.resolve_all_edges().unwrap();

        let (dst, conf): (Option<i64>, String) = store
            .conn
            .query_row("SELECT dst_id, confidence FROM edges LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            dst, None,
            "a name shared by 7 cross-file symbols must not resolve"
        );
        assert_eq!(conf, "name_only");
    }

    /// The strictness must never cost a local definition: a same-file
    /// candidate wins however common the name is elsewhere.
    #[test]
    fn a_same_file_definition_still_wins_over_a_crowd() {
        let caller = FileIndex {
            symbols: vec![
                sym("caller", "a.caller", SymbolKind::Function, Some(0)),
                sym("push", "a.push", SymbolKind::Function, Some(1)),
            ],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Calls,
                dst_name: "push".into(),
                dst_arity: Some(1),
            }],
            chunks: vec![],
        };
        let crowd = FileIndex {
            symbols: (0..7)
                .map(|i| sym("push", &format!("b.T{i}.push"), SymbolKind::Method, Some(1)))
                .collect(),
            edges: vec![],
            chunks: vec![],
        };
        let mut store = store_with(&[("a.py", caller), ("b.py", crowd)]);
        store.resolve_all_edges().unwrap();

        let qual: Option<String> = store
            .conn
            .query_row(
                "SELECT s.qualname FROM edges e LEFT JOIN symbols s ON s.id = e.dst_id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(qual.as_deref(), Some("a.push"));
    }

    /// An unambiguous cross-file name still resolves — the limit only
    /// refuses to pick out of a crowd, it doesn't require locality.
    #[test]
    fn a_unique_cross_file_name_still_resolves() {
        let caller = FileIndex {
            symbols: vec![sym("caller", "a.caller", SymbolKind::Function, Some(0))],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Calls,
                dst_name: "singleton".into(),
                dst_arity: None,
            }],
            chunks: vec![],
        };
        let other = FileIndex {
            symbols: vec![sym(
                "singleton",
                "b.singleton",
                SymbolKind::Function,
                Some(3),
            )],
            edges: vec![],
            chunks: vec![],
        };
        let mut store = store_with(&[("a.py", caller), ("b.py", other)]);
        store.resolve_all_edges().unwrap();

        let qual: Option<String> = store
            .conn
            .query_row(
                "SELECT s.qualname FROM edges e LEFT JOIN symbols s ON s.id = e.dst_id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(qual.as_deref(), Some("b.singleton"));
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
    fn php_backslash_namespace_resolves_to_last_segment() {
        // `App\Service\Mailer::send`-style names: `\` separates namespace
        // segments; a trailing `::method` still wins as the latest separator.
        assert_eq!(super::last_segment(r"App\Service\Mailer"), "Mailer");
        assert_eq!(super::last_segment(r"App\Service\Mailer::send"), "send");
        assert_eq!(super::last_segment(r"\Globals"), "Globals");

        let caller = FileIndex {
            symbols: vec![sym("caller", "app.caller", SymbolKind::Function, None)],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Calls,
                dst_name: r"App\Service\mailer_send".into(),
                dst_arity: None,
            }],
            chunks: vec![],
        };
        let defs = FileIndex {
            symbols: vec![sym(
                "mailer_send",
                "mailer.mailer_send",
                SymbolKind::Function,
                None,
            )],
            edges: vec![],
            chunks: vec![],
        };
        let mut store = store_with(&[("app.php", caller), ("mailer.php", defs)]);
        let n = store.resolve_all_edges().unwrap();
        assert_eq!(n, 1);

        let q: Option<String> = store
            .conn
            .query_row(
                r"SELECT s.qualname FROM edges e JOIN symbols s ON e.dst_id = s.id
                 WHERE e.dst_name = 'App\Service\mailer_send'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(q.as_deref(), Some("mailer.mailer_send"));
    }

    #[test]
    fn resolve_edges_for_names_matches_backslash_suffix() {
        // Incremental re-resolution must recognize `\name` suffixes too.
        let caller = FileIndex {
            symbols: vec![sym("caller", "app.caller", SymbolKind::Function, None)],
            edges: vec![NewEdge {
                src: 0,
                kind: EdgeKind::Imports,
                dst_name: r"App\Util\Slug".into(),
                dst_arity: None,
            }],
            chunks: vec![],
        };
        let mut store = store_with(&[("app.php", caller)]);
        store.resolve_all_edges().unwrap(); // starts unresolved

        let late = FileIndex {
            symbols: vec![sym("Slug", "util.Slug", SymbolKind::Class, None)],
            edges: vec![],
            chunks: vec![],
        };
        store
            .replace_file("util.php", "python", &[7u8; 32], &late)
            .unwrap();
        let n = store.resolve_edges_for_names(&["Slug".into()]).unwrap();
        assert_eq!(n, 1);
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
