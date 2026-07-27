//! `eval/queries/{repo}.yaml` and `eval/edges/{repo}.yaml` schemas.
//!
//! This is the same shape
//! `crates/vexus-cli/tests/eval_corpora_validation.rs` already validates
//! every qualname against — kept as an independent copy here (not a shared
//! dependency) since that file lives in a different crate's `tests/`
//! directory and only asserts resolvability; this module's job is to
//! actually load the ground truth for scoring.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

fn default_tool() -> String {
    "search".to_string()
}

/// One `eval/queries/{repo}.yaml` row.
#[derive(Debug, Clone, Deserialize)]
pub struct Query {
    pub q: String,
    #[serde(default = "default_tool")]
    pub tool: String,
    #[serde(default)]
    pub expect: Vec<String>,
    #[serde(default)]
    pub graded: HashMap<String, u8>,
}

/// One `eval/edges/{repo}.yaml` row — just the two qualnames the runner
/// actually scores. The yaml also carries `expected` (`resolved`|
/// `heuristic`) and a `note`, read by the corpora validation test
/// (`crates/vexus-cli/tests/eval_corpora_validation.rs`, which checks every
/// `heuristic` row carries a non-empty `note`); this runner scores every row
/// identically regardless of `expected` (see `metrics::edge_counts`'s doc
/// comment for why a `heuristic` row scoring as "not found" is an honest
/// outcome, not a bug to filter around), so those two fields are simply left
/// out of this struct — `serde_yaml` ignores unrecognized keys by default,
/// so parsing the same files here doesn't require `deny_unknown_fields` or a
/// dead field just to keep the derive happy.
#[derive(Debug, Clone, Deserialize)]
pub struct Edge {
    pub caller: String,
    pub callee: String,
}

pub fn load_queries(path: &Path) -> Result<Vec<Query>> {
    let yaml = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_yaml::from_str(&yaml).with_context(|| format!("parse {}", path.display()))
}

pub fn load_edges(path: &Path) -> Result<Vec<Edge>> {
    let yaml = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_yaml::from_str(&yaml).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_queries_parses_the_real_pyapp_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/queries/pyapp.yaml");
        let queries = load_queries(&path).unwrap();
        assert!(queries.len() >= 25, "got {}", queries.len());
        assert!(queries.iter().any(|q| q.tool == "explore"));
        assert!(queries.iter().any(|q| q.tool == "search"));
        assert!(queries.iter().any(|q| !q.graded.is_empty()));
    }

    #[test]
    fn load_edges_parses_the_real_pyapp_fixture_ignoring_the_expected_and_note_columns() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/edges/pyapp.yaml");
        let edges = load_edges(&path).unwrap();
        assert!(edges.len() >= 40, "got {}", edges.len());
        assert!(edges
            .iter()
            .all(|e| !e.caller.is_empty() && !e.callee.is_empty()));
    }

    #[test]
    fn query_tool_defaults_to_search_when_omitted() {
        let yaml = "- q: \"where is X\"\n  expect: [a.b]\n";
        let queries: Vec<Query> = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(queries[0].tool, "search");
    }
}
