use std::path::Path;

fn parse_fixture(rel: &str) -> vexus_core::model::FileIndex {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(rel);
    let source = std::fs::read_to_string(&path).unwrap();
    let lang = vexus_index::lang::for_path(Path::new(rel)).expect("language for fixture");
    vexus_index::parse::parse_file(lang, rel, &source)
}

fn symbol_lines(idx: &vexus_core::model::FileIndex) -> Vec<String> {
    idx.symbols.iter().map(|s| format!(
        "{} {} [{}-{}] arity={:?} parent={:?} sig={:?}",
        s.kind.as_str(), s.qualname, s.start_line, s.end_line, s.arity, s.parent, s.sig
    )).collect()
}

fn edge_lines(idx: &vexus_core::model::FileIndex) -> Vec<String> {
    idx.edges.iter().map(|e| format!(
        "{} {} -> {} arity={:?}",
        e.kind.as_str(), idx.symbols[e.src].qualname, e.dst_name, e.dst_arity
    )).collect()
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
