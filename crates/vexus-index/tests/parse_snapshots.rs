use std::path::Path;

fn parse_fixture(rel: &str) -> vexus_core::model::FileIndex {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel);
    let source = std::fs::read_to_string(&path).unwrap();
    let lang = vexus_index::lang::for_path(Path::new(rel)).expect("language for fixture");
    vexus_index::parse::parse_file(lang, rel, &source)
}

fn symbol_lines(idx: &vexus_core::model::FileIndex) -> Vec<String> {
    idx.symbols
        .iter()
        .map(|s| {
            format!(
                "{} {} [{}-{}] arity={:?} parent={:?} sig={:?}",
                s.kind.as_str(),
                s.qualname,
                s.start_line,
                s.end_line,
                s.arity,
                s.parent,
                s.sig
            )
        })
        .collect()
}

fn edge_lines(idx: &vexus_core::model::FileIndex) -> Vec<String> {
    idx.edges
        .iter()
        .map(|e| {
            format!(
                "{} {} -> {} arity={:?}",
                e.kind.as_str(),
                idx.symbols[e.src].qualname,
                e.dst_name,
                e.dst_arity
            )
        })
        .collect()
}

fn chunk_lines(idx: &vexus_core::model::FileIndex) -> Vec<String> {
    idx.chunks
        .iter()
        .map(|c| {
            format!(
                "sym={:?} [{}-{}] tokens~{} :: {}",
                c.symbol,
                c.start_line,
                c.end_line,
                vexus_core::model::estimate_tokens(&c.content),
                c.content
                    .replace('\n', "⏎")
                    .chars()
                    .take(80)
                    .collect::<String>()
            )
        })
        .collect()
}

#[test]
fn python_symbols() {
    let idx = parse_fixture("python/sample.py");
    insta::assert_yaml_snapshot!(symbol_lines(&idx));
}

#[test]
fn python_edges() {
    let idx = parse_fixture("python/sample.py");
    insta::assert_yaml_snapshot!(edge_lines(&idx));
}

#[test]
fn typescript_symbols() {
    let idx = parse_fixture("typescript/sample.ts");
    insta::assert_yaml_snapshot!(symbol_lines(&idx));
}

#[test]
fn typescript_edges() {
    let idx = parse_fixture("typescript/sample.ts");
    insta::assert_yaml_snapshot!(edge_lines(&idx));
}

#[test]
fn rust_symbols() {
    let idx = parse_fixture("rust/sample.rs");
    insta::assert_yaml_snapshot!(symbol_lines(&idx));
}

#[test]
fn rust_edges() {
    let idx = parse_fixture("rust/sample.rs");
    insta::assert_yaml_snapshot!(edge_lines(&idx));
}

#[test]
fn python_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("python/sample.py")));
}

#[test]
fn typescript_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("typescript/sample.ts")));
}

#[test]
fn rust_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("rust/sample.rs")));
}

#[test]
fn javascript_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("javascript/sample.js")));
}

#[test]
fn javascript_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("javascript/sample.js")));
}

#[test]
fn javascript_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("javascript/sample.js")));
}

#[test]
fn go_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("go/sample.go")));
}

#[test]
fn go_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("go/sample.go")));
}

#[test]
fn go_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("go/sample.go")));
}

#[test]
fn java_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("java/Sample.java")));
}

#[test]
fn java_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("java/Sample.java")));
}

#[test]
fn java_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("java/Sample.java")));
}

#[test]
fn c_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("c/sample.c")));
}

#[test]
fn c_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("c/sample.c")));
}

#[test]
fn c_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("c/sample.c")));
}

#[test]
fn cpp_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("cpp/sample.cpp")));
}

#[test]
fn cpp_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("cpp/sample.cpp")));
}

#[test]
fn cpp_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("cpp/sample.cpp")));
}

#[test]
fn c_sharp_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("c_sharp/Sample.cs")));
}

#[test]
fn c_sharp_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("c_sharp/Sample.cs")));
}

#[test]
fn c_sharp_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("c_sharp/Sample.cs")));
}

#[test]
fn kotlin_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("kotlin/sample.kt")));
}

#[test]
fn kotlin_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("kotlin/sample.kt")));
}

#[test]
fn kotlin_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("kotlin/sample.kt")));
}

#[test]
fn swift_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("swift/sample.swift")));
}

#[test]
fn swift_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("swift/sample.swift")));
}

#[test]
fn swift_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("swift/sample.swift")));
}

#[test]
fn ruby_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("ruby/sample.rb")));
}

#[test]
fn ruby_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("ruby/sample.rb")));
}

#[test]
fn ruby_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("ruby/sample.rb")));
}

#[test]
fn php_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("php/sample.php")));
}

#[test]
fn php_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("php/sample.php")));
}

#[test]
fn php_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("php/sample.php")));
}

#[test]
fn scala_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("scala/sample.scala")));
}

#[test]
fn scala_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("scala/sample.scala")));
}

#[test]
fn scala_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("scala/sample.scala")));
}

#[test]
fn elixir_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("elixir/sample.ex")));
}

#[test]
fn elixir_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("elixir/sample.ex")));
}

#[test]
fn elixir_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("elixir/sample.ex")));
}

#[test]
fn dart_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("dart/sample.dart")));
}

#[test]
fn dart_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("dart/sample.dart")));
}

#[test]
fn dart_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("dart/sample.dart")));
}

#[test]
fn lua_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("lua/sample.lua")));
}

#[test]
fn lua_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("lua/sample.lua")));
}

#[test]
fn lua_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("lua/sample.lua")));
}

#[test]
fn bash_symbols() {
    insta::assert_yaml_snapshot!(symbol_lines(&parse_fixture("bash/sample.sh")));
}

#[test]
fn bash_edges() {
    insta::assert_yaml_snapshot!(edge_lines(&parse_fixture("bash/sample.sh")));
}

#[test]
fn bash_chunks() {
    insta::assert_yaml_snapshot!(chunk_lines(&parse_fixture("bash/sample.sh")));
}

/// Field report: `export const readDataStream = async function* <T>(...)`
/// produced no symbol at all — `callers`/`callees`/`impact` answered "no
/// symbol found" for it while arrow consts worked. Every const-assigned
/// function form (and generator declarations) must land in the symbol
/// table.
#[test]
fn typescript_const_assigned_function_forms_are_symbols() {
    let idx = parse_fixture("typescript/sample.ts");
    let quals: Vec<&str> = idx.symbols.iter().map(|s| s.qualname.as_str()).collect();
    for expected in [
        "typescript.sample.readDataStream", // async generator function expression
        "typescript.sample.compact",        // plain function expression
        "typescript.sample.idGen",          // generator function declaration
        "typescript.sample.fetchUserArrow", // arrow (already worked; regression guard)
    ] {
        assert!(
            quals.contains(&expected),
            "missing symbol {expected}; got {quals:?}"
        );
    }
}
