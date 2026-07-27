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
