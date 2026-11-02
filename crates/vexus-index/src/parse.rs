use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, QueryCursor};

use vexus_core::model::{EdgeKind, FileIndex, NewEdge, NewSymbol, SymbolKind};

use crate::lang::Lang;

/// A Rust `impl` block.
struct ImplScope {
    start_line: u32,
    end_line: u32,
    type_name: String,
}

pub fn parse_file(lang: &Lang, rel_path: &str, source: &str) -> FileIndex {
    let mut idx = FileIndex::default();
    let module_qual = rel_path
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(rel_path)
        .replace('/', ".");
    let stem = module_qual
        .rsplit('.')
        .next()
        .unwrap_or(&module_qual)
        .to_string();
    let total_lines = source.lines().count().max(1) as u32;
    idx.symbols.push(NewSymbol {
        name: stem,
        qualname: module_qual.clone(),
        kind: SymbolKind::Module,
        sig: None,
        start_line: 1,
        end_line: total_lines,
        parent: None,
        arity: None,
    });

    let mut parser = Parser::new();
    if parser.set_language(&lang.grammar()).is_err() {
        return idx; // structural-only degradation: module symbol still useful
    }
    let Some(tree) = parser.parse(source, None) else {
        return idx;
    };

    // Collect definition captures: (def node, name, kind hint, params node)
    let query = lang.symbols_query();
    let mut defs: Vec<(Node, String, &str, Option<Node>)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut def_node = None;
        let mut def_kind = "";
        let mut name = None;
        let mut params = None;
        for cap in m.captures {
            let cap_name = &query.capture_names()[cap.index as usize];
            match *cap_name {
                "def.function" => {
                    def_node = Some(cap.node);
                    def_kind = "function";
                }
                "def.class" => {
                    def_node = Some(cap.node);
                    def_kind = "class";
                }
                "def.struct" => {
                    def_node = Some(cap.node);
                    def_kind = "struct";
                }
                "def.enum" => {
                    def_node = Some(cap.node);
                    def_kind = "enum";
                }
                "def.trait" => {
                    def_node = Some(cap.node);
                    def_kind = "trait";
                }
                "def.interface" => {
                    def_node = Some(cap.node);
                    def_kind = "interface";
                }
                "def.const" => {
                    def_node = Some(cap.node);
                    def_kind = "const";
                }
                "def.type" => {
                    def_node = Some(cap.node);
                    def_kind = "type";
                }
                // Namespace-like containers: Ruby `module`, C++/C#/PHP
                // `namespace`, Elixir `defmodule`. Members nest under them
                // for qualnames, but functions inside stay `Function`, not
                // `Method` (the promotion list below excludes Module).
                "def.module" => {
                    def_node = Some(cap.node);
                    def_kind = "module";
                }
                // Methods whose methodness is syntactic rather than
                // parent-derived: Go's `method_declaration` (top-level, no
                // enclosing type node), Ruby's `def self.x`.
                "def.method" => {
                    def_node = Some(cap.node);
                    def_kind = "method";
                }
                "def.impl" => {
                    def_node = Some(cap.node);
                    def_kind = "impl";
                }
                "def.name" => {
                    name = Some(
                        cap.node
                            .utf8_text(source.as_bytes())
                            .unwrap_or("")
                            .to_string(),
                    )
                }
                "def.params" => params = Some(cap.node),
                _ => {}
            }
        }
        if let (Some(node), Some(name)) = (def_node, name) {
            defs.push((node, name, def_kind, params));
        }
    }
    // Document order so parents precede children (store relies on this).
    defs.sort_by_key(|(n, ..)| n.start_byte());

    let mut impls: Vec<ImplScope> = Vec::new();
    for (node, name, kind_hint, params) in defs {
        // Walk up to the nearest enclosing def already in idx.symbols.
        let parent = enclosing_symbol(&idx, node, true);
        let start_line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        if kind_hint == "impl" {
            impls.push(ImplScope {
                start_line,
                end_line,
                type_name: name,
            });
            continue;
        }
        let impl_scope = impls.iter().rev().find(|scope| {
            (scope.start_line..=scope.end_line).contains(&start_line)
                && (parent == 0 || scope.start_line > idx.symbols[parent].start_line)
        });
        let parent_kind = idx.symbols[parent].kind;
        let kind = match kind_hint {
            "function"
                if impl_scope.is_some()
                    || matches!(
                        parent_kind,
                        SymbolKind::Class
                            | SymbolKind::Struct
                            | SymbolKind::Enum
                            | SymbolKind::Trait
                            | SymbolKind::Interface
                    ) =>
            {
                SymbolKind::Method
            }
            "function" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "trait" => SymbolKind::Trait,
            "interface" => SymbolKind::Interface,
            "const" => SymbolKind::Const,
            "type" => SymbolKind::Type,
            "module" => SymbolKind::Module,
            _ => SymbolKind::Function,
        };
        let parent_name = &idx.symbols[parent].qualname;
        let qualname = match impl_scope {
            Some(scope) => format!("{parent_name}.{}.{name}", scope.type_name),
            None => format!("{parent_name}.{name}"),
        };
        let sig = source
            .lines()
            .nth(node.start_position().row)
            .map(|l| l.trim().to_string());
        let arity = params.map(|p| count_params(p, source));
        idx.symbols.push(NewSymbol {
            name,
            qualname,
            kind,
            sig,
            start_line,
            end_line,
            parent: Some(parent),
            arity,
        });
    }

    // Extract edges (calls and imports)
    let equery = lang.edges_query();
    let mut ecursor = QueryCursor::new();
    let mut ematches = ecursor.matches(&equery, tree.root_node(), source.as_bytes());
    while let Some(m) = ematches.next() {
        let mut call_name = None;
        let mut call_args = None;
        let mut call_node = None;
        let mut import_module = None;
        for cap in m.captures {
            let cap_name = &equery.capture_names()[cap.index as usize];
            match *cap_name {
                "call" => call_node = Some(cap.node),
                "call.name" => {
                    call_name = Some(
                        cap.node
                            .utf8_text(source.as_bytes())
                            .unwrap_or("")
                            .to_string(),
                    )
                }
                "call.args" => call_args = Some(cap.node),
                "import.module" => {
                    import_module = Some(
                        cap.node
                            .utf8_text(source.as_bytes())
                            .unwrap_or("")
                            .to_string(),
                    )
                }
                _ => {}
            }
        }
        if let (Some(node), Some(name)) = (call_node, call_name) {
            if is_definition_head_call(node, source) {
                continue;
            }
            let src = enclosing_symbol(&idx, node, false);
            let arity = call_args.map(|a| a.named_child_count() as u32);
            idx.edges.push(NewEdge {
                src,
                kind: EdgeKind::Calls,
                dst_name: name,
                dst_arity: arity,
            });
        } else if let Some(module) = import_module {
            idx.edges.push(NewEdge {
                src: 0,
                kind: EdgeKind::Imports,
                dst_name: module,
                dst_arity: None,
            });
        }
    }

    crate::chunk::build_chunks(&mut idx, source);
    if lang.name == "rust" {
        mark_rust_tests(&mut idx, &impls, source);
    }
    idx
}

/// Flags the chunks of Rust test code. It sits on production paths, and a
/// helper in a test module has nothing of its own that says so, so what counts
/// is the attribute on the outermost item around it.
fn mark_rust_tests(idx: &mut FileIndex, impls: &[ImplScope], source: &str) {
    let lines: Vec<&str> = source.lines().collect();
    let symbols = idx.symbols.iter().skip(1);
    let test_ranges: Vec<(u32, u32)> = symbols
        .map(|s| (s.start_line, s.end_line))
        .chain(impls.iter().map(|s| (s.start_line, s.end_line)))
        .filter(|(start, _)| has_test_attribute(&lines, *start))
        .collect();
    for chunk in &mut idx.chunks {
        let Some(symbol) = chunk.symbol.filter(|s| *s != 0) else {
            continue;
        };
        let line = idx.symbols[symbol].start_line;
        chunk.is_test = test_ranges.iter().any(|(s, e)| (*s..=*e).contains(&line));
    }
}

/// Whether the attributes directly above `start_line` mark a test item.
fn has_test_attribute(lines: &[&str], start_line: u32) -> bool {
    lines[..start_line as usize - 1]
        .iter()
        .rev()
        .map(|line| line.trim())
        .take_while(|line| line.starts_with("#[") || line.starts_with("//"))
        .any(is_test_attribute)
}

fn is_test_attribute(line: &str) -> bool {
    let Some(attribute) = line.strip_prefix("#[") else {
        return false;
    };
    if let Some(condition) = attribute.strip_prefix("cfg(") {
        // Odd segments are string literals, like `feature = "test-utils"`.
        let code: String = condition.split('"').step_by(2).collect();
        return !code.contains("not(")
            && code
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|word| word == "test");
    }
    let path = attribute.split(['(', ']']).next().unwrap_or("");
    let name = path.rsplit("::").next().unwrap_or(path);
    name == "test" || name == "rstest" || name.starts_with("test_") || name.ends_with("_test")
}

/// True when a captured `(call)` node is not a real call site but part of a
/// definition's own syntax, which happens in grammars (Elixir) where `def`
/// itself parses as a call:
///
/// - a def head — `def total(items, tax) do` nests `total(items, tax)` as a
///   `(call)` inside the `def` keyword-call's arguments. Emitting an edge for
///   it fabricates a self-recursion edge on every function definition,
///   indistinguishable from real single-line recursion.
/// - a module attribute — `@moduledoc "..."` is a `(call)` under the `@`
///   unary operator, not an invocation of anything.
///
/// The checks key on the Elixir grammar's `target` field and keyword set, so
/// call nodes from other grammars (whose calls use `function`/`method`
/// fields) never match. A real call under a different unary operator
/// (`!authorized?(user)`) keeps its edge — only `@` is an attribute marker.
fn is_definition_head_call(call_node: Node, source: &str) -> bool {
    const DEF_KEYWORDS: [&str; 11] = [
        "def",
        "defp",
        "defmodule",
        "defmacro",
        "defmacrop",
        "defprotocol",
        "defimpl",
        "defdelegate",
        "defguard",
        "defguardp",
        "defexception",
    ];
    let Some(parent) = call_node.parent() else {
        return false;
    };
    match parent.kind() {
        "arguments" => {
            let Some(grand) = parent.parent() else {
                return false;
            };
            if grand.kind() != "call" {
                return false;
            }
            let Some(target) = grand.child_by_field_name("target") else {
                return false;
            };
            let text = target.utf8_text(source.as_bytes()).unwrap_or("");
            DEF_KEYWORDS.contains(&text)
        }
        "unary_operator" => {
            parent
                .child(0)
                .map(|op| op.utf8_text(source.as_bytes()).unwrap_or(""))
                == Some("@")
        }
        _ => false,
    }
}

/// Index (into idx.symbols) of the innermost symbol whose line range strictly
/// contains `node`, defaulting to the module symbol (0).
/// `is_def`: if true, a def does not enclose itself; if false, a call site can be enclosed by its def.
fn enclosing_symbol(idx: &FileIndex, node: Node, is_def: bool) -> usize {
    let line = node.start_position().row as u32 + 1;
    let mut best = 0usize;
    for (i, s) in idx.symbols.iter().enumerate().skip(1) {
        let contains =
            s.start_line <= line && line <= s.end_line && !(is_def && s.start_line == line); // if is_def, a def does not enclose itself
        if contains && (s.start_line > idx.symbols[best].start_line || best == 0) {
            best = i;
        }
    }
    best
}

/// Count named parameter children, excluding self/cls receivers.
fn count_params(params: Node, source: &str) -> u32 {
    let mut n = 0;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() == "self_parameter" {
            continue;
        }
        let text = child.utf8_text(source.as_bytes()).unwrap_or("");
        if text == "self" || text == "cls" {
            continue;
        }
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use vexus_core::model::{FileIndex, SymbolKind};

    fn parsed(rel: &str, source: &str) -> FileIndex {
        let lang = crate::lang::for_path(Path::new(rel)).unwrap();
        super::parse_file(lang, rel, source)
    }

    fn symbol<'a>(idx: &'a FileIndex, qualname: &str) -> &'a vexus_core::model::NewSymbol {
        idx.symbols
            .iter()
            .find(|s| s.qualname == qualname)
            .unwrap_or_else(|| {
                let names: Vec<_> = idx.symbols.iter().map(|s| &s.qualname).collect();
                panic!("no {qualname} in {names:?}")
            })
    }

    #[test]
    fn impl_methods_are_named_after_their_type() {
        let idx = parsed(
            "store.rs",
            "pub struct Store {\n    conn: u8,\n}\n\nimpl Store {\n    const LIMIT: u8 = 1;\n\n    pub fn open(path: u8) -> Self {\n        todo!()\n    }\n}\n",
        );

        let open = symbol(&idx, "store.Store.open");
        assert_eq!(open.kind, SymbolKind::Method);
        assert_eq!(open.arity, Some(1));
        assert_eq!(symbol(&idx, "store.Store.LIMIT").kind, SymbolKind::Const);
    }

    #[test]
    fn a_type_with_many_impls_stays_one_symbol() {
        let idx = parsed(
            "a.rs",
            "struct A;\nimpl A {\n    fn x(&self) {}\n}\nimpl Clone for A {\n    fn clone(&self) -> A { A }\n}\n",
        );

        assert_eq!(idx.symbols.iter().filter(|s| s.name == "A").count(), 1);
        symbol(&idx, "a.A.x");
        symbol(&idx, "a.A.clone");
    }

    #[test]
    fn impl_type_forms_all_resolve_to_the_type_name() {
        for (header, qualname) in [
            ("impl<T> Stack<T>", "a.Stack.go"),
            ("impl fmt::Display for Stack", "a.Stack.go"),
            ("impl crate::x::Stack", "a.Stack.go"),
            ("impl<T> Trait for crate::x::Stack<T>", "a.Stack.go"),
            ("impl<'a> Trait for &'a Stack", "a.Stack.go"),
        ] {
            let idx = parsed("a.rs", &format!("{header} {{\n    fn go() {{}}\n}}\n"));
            assert_eq!(symbol(&idx, qualname).kind, SymbolKind::Method, "{header}");
        }
    }

    #[test]
    fn impl_of_a_type_from_elsewhere_still_names_it() {
        let idx = parsed("a.rs", "impl Foreign {\n    fn go(&self) {}\n}\n");

        assert_eq!(symbol(&idx, "a.Foreign.go").kind, SymbolKind::Method);
    }

    #[test]
    fn impl_before_its_type_still_names_it() {
        let idx = parsed(
            "a.rs",
            "impl Late {\n    fn go(&self) {}\n}\nstruct Late;\n",
        );

        symbol(&idx, "a.Late.go");
        symbol(&idx, "a.Late");
    }

    #[test]
    fn a_function_nested_in_a_method_stays_under_the_method() {
        let idx = parsed(
            "a.rs",
            "struct A;\nimpl A {\n    fn outer(&self) {\n        fn inner() {}\n    }\n}\n",
        );

        symbol(&idx, "a.A.outer.inner");
    }

    #[test]
    fn inline_modules_qualify_their_members() {
        let idx = parsed(
            "a.rs",
            "mod inner {\n    pub struct S;\n    impl S {\n        pub fn go() {}\n    }\n    pub fn free() {}\n}\nmod declared_elsewhere;\n",
        );

        assert_eq!(symbol(&idx, "a.inner").kind, SymbolKind::Module);
        symbol(&idx, "a.inner.S.go");
        assert_eq!(symbol(&idx, "a.inner.free").kind, SymbolKind::Function);
        assert!(!idx.symbols.iter().any(|s| s.name == "declared_elsewhere"));
    }

    fn test_chunks(idx: &FileIndex) -> Vec<String> {
        idx.chunks
            .iter()
            .filter(|c| c.is_test)
            .map(|c| idx.symbols[c.symbol.unwrap()].name.clone())
            .collect()
    }

    #[test]
    fn helpers_and_every_item_in_a_test_module_are_test_code() {
        let idx = parsed(
            "a.rs",
            "fn real() {}\n\n#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n    use super::*;\n\n    fn helper() {}\n\n    struct Fixture;\n    impl Fixture {\n        fn build() {}\n    }\n\n    #[test]\n    fn checks() {}\n}\n",
        );

        let mut names = test_chunks(&idx);
        names.sort();
        assert_eq!(names, ["Fixture", "build", "checks", "helper", "tests"]);
    }

    #[test]
    fn test_attributes_are_recognised_under_any_runner() {
        for attribute in [
            "#[test]",
            "#[tokio::test]",
            "#[tokio::test(flavor = \"multi_thread\")]",
            "#[async_std::test]",
            "#[rstest]",
            "#[test_case(1)]",
            "#[wasm_bindgen_test]",
            "#[cfg(test)]",
            "#[cfg(all(test, unix))]",
        ] {
            let idx = parsed("a.rs", &format!("{attribute}\nfn f() {{}}\n"));
            assert_eq!(test_chunks(&idx), ["f"], "{attribute}");
        }
    }

    #[test]
    fn a_test_attribute_above_a_doc_comment_counts() {
        let idx = parsed("a.rs", "#[test]\n// Explains.\nfn f() {}\n");
        assert_eq!(test_chunks(&idx), ["f"]);
    }

    #[test]
    fn production_attributes_are_not_test_code() {
        for attribute in [
            "#[cfg(not(test))]",
            "#[cfg(feature = \"test-utils\")]",
            "#[inline]",
            "#[derive(Debug)]",
            "#[attest]",
        ] {
            let idx = parsed("a.rs", &format!("{attribute}\nfn f() {{}}\n"));
            assert!(test_chunks(&idx).is_empty(), "{attribute}");
        }
    }

    #[test]
    fn a_cfg_test_impl_flags_its_methods() {
        let idx = parsed(
            "a.rs",
            "struct A;\n\n#[cfg(test)]\nimpl A {\n    fn only_in_tests() {}\n}\n",
        );
        assert_eq!(test_chunks(&idx), ["only_in_tests"]);
    }

    #[test]
    fn other_languages_are_never_flagged() {
        let idx = parsed("a.py", "@test\ndef f():\n    pass\n");
        assert!(test_chunks(&idx).is_empty());
    }
}
