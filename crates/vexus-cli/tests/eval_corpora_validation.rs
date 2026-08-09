//! Gate: every qualname named in `eval/queries/*.yaml`
//! (as an `expect` or a `graded` key) and `eval/edges/*.yaml` (as a `caller`
//! or `callee`) must resolve to a real, unambiguous symbol in its corpus.
//!
//! This is the deliverable's actual test: hand-authored ground truth is
//! only useful if every qualname in it actually exists in the fixture
//! corpus it claims to describe. A typo'd qualname, a symbol renamed after
//! the yaml was written, or a qualname format that drifts from how the
//! indexer actually derives them — all of those would otherwise sit
//! silently wrong forever. The `vexus-eval` runner consumes the same
//! files, but this gate is what keeps them honest.
//!
//! Each corpus is indexed once (mock embedder — no model download, fully
//! deterministic) into a `tempfile::tempdir()` database. Pointing the
//! `Store` at a path outside `eval/` while still walking the real
//! `eval/corpora/<repo>` tree as the read-only source means no `.vexus`
//! artifact is ever created under `eval/` by this test — nothing to clean
//! up, nothing that could accidentally get committed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use vexus_core::query::Resolution;
use vexus_core::Store;

fn default_tool() -> String {
    "search".to_string()
}

/// `eval/queries/{repo}.yaml` row. `tool` selects which tool the query is
/// graded against and defaults to `search`; `expect` lists the qualnames a
/// correct result must surface.
#[derive(Debug, Deserialize)]
struct Query {
    q: String,
    #[serde(default = "default_tool")]
    tool: String,
    #[serde(default)]
    expect: Vec<String>,
    /// Qualnames a correct result must NOT surface (feeds `clean@5` /
    /// `bundle_clean`). Validated here exactly like `expect`: every entry
    /// must resolve to a real, unambiguous symbol — a forbidden qualname
    /// that doesn't exist would make the metric pass vacuously forever.
    #[serde(default)]
    expect_not: Vec<String>,
    #[serde(default)]
    graded: HashMap<String, u8>,
}

/// `eval/edges/{repo}.yaml` row.
#[derive(Debug, Deserialize)]
struct Edge {
    caller: String,
    callee: String,
    expected: String,
    #[serde(default)]
    note: String,
}

fn eval_root() -> PathBuf {
    // crates/vexus-cli -> ../../eval
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../eval")
        .canonicalize()
        .expect("eval/ must exist at the repo root")
}

/// Index `corpus_root` (a real, read-only fixture directory under
/// `eval/corpora/`) into a fresh temp-directory database, structurally only
/// (no embedding needed — `resolve_symbol` never touches vectors).
fn index_corpus(corpus_root: &Path) -> (Store, tempfile::TempDir) {
    let db_dir = tempfile::tempdir().expect("temp dir for index db");
    let mut store = Store::open(&db_dir.path().join("index.db")).expect("open temp store");
    let report = vexus_watch::pipeline::index_repo(corpus_root, &mut store).expect("index corpus");
    assert_eq!(
        report.failed.len(),
        0,
        "fixture corpus at {corpus_root:?} must index cleanly, got failures: {:?}",
        report.failed
    );
    assert!(
        report.indexed > 0,
        "fixture corpus at {corpus_root:?} indexed 0 files — wrong path?"
    );
    (store, db_dir)
}

/// Assert `qualname` resolves to exactly one real symbol (`Resolution::Exact`)
/// against `store`. `context` names the yaml row/field this came from, for a
/// useful failure message.
fn assert_resolves_exact(store: &Store, qualname: &str, context: &str) {
    match store
        .resolve_symbol(qualname)
        .expect("resolve_symbol query")
    {
        Resolution::Exact(_) => {}
        Resolution::Candidates(cands) => panic!(
            "{context}: qualname {qualname:?} resolved to {} ambiguous candidates \
             instead of Exact: {:?} — ground truth must name a fully-qualified, \
             unambiguous symbol",
            cands.len(),
            cands.iter().map(|c| &c.qualname).collect::<Vec<_>>()
        ),
        Resolution::NotFound { suggestions } => panic!(
            "{context}: qualname {qualname:?} did not resolve at all (NotFound). \
             Nearest suggestions: {suggestions:?} — this qualname does not exist \
             in the indexed corpus; fix the yaml or the corpus."
        ),
    }
}

fn validate_corpus(repo: &str) {
    let root = eval_root();
    let corpus_root = root.join("corpora").join(repo);
    assert!(
        corpus_root.is_dir(),
        "expected a fixture corpus directory at {corpus_root:?}"
    );

    let (store, _db_dir) = index_corpus(&corpus_root);

    let queries_path = root.join("queries").join(format!("{repo}.yaml"));
    let queries_yaml = std::fs::read_to_string(&queries_path)
        .unwrap_or_else(|e| panic!("read {queries_path:?}: {e}"));
    let queries: Vec<Query> = serde_yaml::from_str(&queries_yaml)
        .unwrap_or_else(|e| panic!("parse {queries_path:?}: {e}"));
    assert!(
        queries.len() >= 25,
        "{repo}: expected >= 25 queries in {queries_path:?}, got {}",
        queries.len()
    );

    let mut graded_total = 0usize;
    for (i, query) in queries.iter().enumerate() {
        assert!(
            matches!(
                query.tool.as_str(),
                "search" | "explore" | "callers" | "callees"
            ),
            "{repo} queries.yaml row {i} (q={:?}): `tool` must be one of \
             search|explore|callers|callees, got {:?}",
            query.q,
            query.tool
        );
        assert!(
            !query.expect.is_empty(),
            "{repo} queries.yaml row {i} (q={:?}): `expect` must not be empty",
            query.q
        );
        for qualname in &query.expect {
            assert_resolves_exact(
                &store,
                qualname,
                &format!("{repo} queries.yaml row {i} (q={:?}) expect", query.q),
            );
        }
        for qualname in &query.expect_not {
            assert_resolves_exact(
                &store,
                qualname,
                &format!("{repo} queries.yaml row {i} (q={:?}) expect_not", query.q),
            );
            assert!(
                !query.expect.contains(qualname),
                "{repo} queries.yaml row {i} (q={:?}): {qualname:?} appears in both \
                 `expect` and `expect_not` — a symbol cannot be simultaneously \
                 required and forbidden",
                query.q
            );
        }
        for qualname in query.graded.keys() {
            assert_resolves_exact(
                &store,
                qualname,
                &format!("{repo} queries.yaml row {i} (q={:?}) graded", query.q),
            );
        }
        if !query.graded.is_empty() {
            graded_total += 1;
        }
    }
    assert!(
        graded_total >= 10,
        "{repo}: expected >= 10 queries with a `graded` map, got {graded_total}"
    );

    let expect_not_total = queries.iter().filter(|q| !q.expect_not.is_empty()).count();
    assert!(
        expect_not_total >= 5,
        "{repo}: expected >= 5 queries with an `expect_not` list (the clean@5 / \
         bundle_clean ground truth), got {expect_not_total}"
    );

    for tool in ["callers", "callees"] {
        let count = queries.iter().filter(|q| q.tool == tool).count();
        assert!(
            count >= 5,
            "{repo}: expected >= 5 `{tool}` queries in {queries_path:?}, got {count}"
        );
    }

    let edges_path = root.join("edges").join(format!("{repo}.yaml"));
    let edges_yaml =
        std::fs::read_to_string(&edges_path).unwrap_or_else(|e| panic!("read {edges_path:?}: {e}"));
    let edges: Vec<Edge> =
        serde_yaml::from_str(&edges_yaml).unwrap_or_else(|e| panic!("parse {edges_path:?}: {e}"));
    assert!(
        edges.len() >= 40,
        "{repo}: expected >= 40 edges in {edges_path:?}, got {}",
        edges.len()
    );

    let mut heuristic_count = 0usize;
    for (i, edge) in edges.iter().enumerate() {
        assert!(
            edge.expected == "resolved" || edge.expected == "heuristic",
            "{repo} edges.yaml row {i}: `expected` must be resolved|heuristic, got {:?}",
            edge.expected
        );
        if edge.expected == "heuristic" {
            heuristic_count += 1;
            assert!(
                !edge.note.trim().is_empty(),
                "{repo} edges.yaml row {i} ({} -> {}): heuristic-limit cases must carry \
                 an explanatory `note`",
                edge.caller,
                edge.callee
            );
        }
        assert_resolves_exact(
            &store,
            &edge.caller,
            &format!("{repo} edges.yaml row {i} caller"),
        );
        assert_resolves_exact(
            &store,
            &edge.callee,
            &format!("{repo} edges.yaml row {i} callee"),
        );
    }
    assert!(
        heuristic_count >= 3,
        "{repo}: expected >= 3 annotated heuristic-limit edges, got {heuristic_count}"
    );
}

#[test]
fn pyapp_ground_truth_resolves() {
    validate_corpus("pyapp");
}

#[test]
fn polyglot_ground_truth_resolves() {
    validate_corpus("polyglot");
}

/// Belt-and-suspenders: this test suite must never leave a `.vexus`
/// directory behind under `eval/corpora/` — every `Store` above is opened
/// against a `tempfile::tempdir()` path, never `eval/corpora/<repo>/.vexus`,
/// so there is nothing for this test to clean up. Assert that invariant
/// directly, in case a future edit reintroduces an in-place index.
#[test]
fn no_vexus_artifacts_left_under_eval() {
    let root = eval_root();
    for repo in ["pyapp", "polyglot"] {
        let vexus_dir = root.join("corpora").join(repo).join(".vexus");
        assert!(
            !vexus_dir.exists(),
            "found a stray {vexus_dir:?} — index artifacts must never live under eval/"
        );
    }
}
