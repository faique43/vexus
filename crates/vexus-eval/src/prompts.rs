//! Advisory score for `eval/queries/prompts.yaml`: raw change requests and bug
//! reports through `Store::locate`, the code behind the prompt hook, graded on
//! whether the right file is near the top. The labelled queries read like
//! search queries and these don't, so this is the closer look at what agents
//! get. It has no baseline and can't fail anything: a set this small moves too
//! much on one prompt to gate on.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use vexus_core::locate::Hint;
use vexus_embed::Embedder;
use vexus_mcp::state::AppState;

use crate::corpus::index_into_temp_state;

/// What the hook asks `locate` for, in `vexus-cli`'s `prompt_hook`.
const LOCATED_HINTS: usize = 8;
/// How much of a prompt the hook embeds.
const EMBEDDED_PROMPT_CHARS: usize = 1200;
/// Long enough to tell one prompt from another in the list of misses.
const MISS_PROMPT_CHARS: usize = 70;

#[derive(Debug, Deserialize)]
pub struct PromptCase {
    pub corpus: String,
    pub kind: String,
    pub prompt: String,
    pub files: Vec<String>,
}

pub fn load_prompts(path: &Path) -> Result<Vec<PromptCase>> {
    let yaml = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_yaml::from_str(&yaml).with_context(|| format!("parse {}", path.display()))
}

/// The distinct files among `hints`, best first.
fn ranked_files(hints: &[Hint]) -> Vec<&str> {
    let mut seen = HashSet::new();
    hints
        .iter()
        .map(|hint| hint.path.as_str())
        .filter(|path| seen.insert(*path))
        .collect()
}

/// 0-based position of the first file `wanted` names.
fn rank_of(ranked: &[&str], wanted: &[String]) -> Option<usize> {
    ranked
        .iter()
        .position(|path| wanted.iter().any(|w| w == path))
}

/// How many prompts landed the right file within each cutoff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub cases: u32,
    pub top_1: u32,
    pub top_3: u32,
    pub top_5: u32,
}

impl Tally {
    fn record(&mut self, rank: Option<usize>) {
        self.cases += 1;
        let within = |k: usize| rank.is_some_and(|r| r < k) as u32;
        self.top_1 += within(1);
        self.top_3 += within(3);
        self.top_5 += within(5);
    }

    fn rate(&self, hits: u32) -> f64 {
        if self.cases == 0 {
            0.0
        } else {
            f64::from(hits) / f64::from(self.cases)
        }
    }
}

#[derive(Debug)]
pub struct Miss {
    pub corpus: String,
    pub prompt: String,
    pub wanted: String,
    pub got: Vec<String>,
}

/// Tallies by corpus, by kind and "overall", plus every prompt whose right
/// file fell outside the top 5.
#[derive(Debug, Default)]
pub struct PromptReport {
    pub tallies: BTreeMap<String, Tally>,
    pub misses: Vec<Miss>,
}

fn locate_prompt(
    state: &AppState,
    embedder: Option<&Arc<dyn Embedder>>,
    prompt: &str,
) -> Result<Vec<Hint>> {
    let opening: String = prompt.chars().take(EMBEDDED_PROMPT_CHARS).collect();
    let query_vec = match embedder {
        Some(embedder) => embedder.embed(&[opening.as_str()])?.pop(),
        None => None,
    };
    let floor = embedder.and_then(|e| vexus_embed::effective_distance_floor(e.as_ref()));
    let store = state
        .lock_store_fresh()
        .map_err(|msg| anyhow::anyhow!("{msg}"))?;
    store.locate(prompt, query_vec.as_deref(), floor, LOCATED_HINTS)
}

pub fn score_prompts(
    eval_root: &Path,
    embedder: Option<&Arc<dyn Embedder>>,
) -> Result<PromptReport> {
    let cases = load_prompts(&eval_root.join("queries").join("prompts.yaml"))?;
    let mut corpora: Vec<&str> = cases.iter().map(|c| c.corpus.as_str()).collect();
    corpora.sort_unstable();
    corpora.dedup();

    let mut report = PromptReport::default();
    for corpus in corpora {
        eprintln!("vexus-eval: indexing + locating prompts for {corpus}...");
        let root = eval_root.join("corpora").join(corpus);
        let (state, _db_dir) = index_into_temp_state(&root, embedder)?;
        for case in cases.iter().filter(|c| c.corpus == corpus) {
            let hints = locate_prompt(&state, embedder, &case.prompt)?;
            let ranked = ranked_files(&hints);
            let rank = rank_of(&ranked, &case.files);
            for scope in [corpus, case.kind.as_str(), "overall"] {
                report.tallies.entry(scope.into()).or_default().record(rank);
            }
            if !rank.is_some_and(|r| r < 5) {
                report.misses.push(Miss {
                    corpus: corpus.to_string(),
                    prompt: case.prompt.chars().take(MISS_PROMPT_CHARS).collect(),
                    wanted: case.files.join(" or "),
                    got: ranked.iter().take(3).map(|p| p.to_string()).collect(),
                });
            }
        }
    }
    Ok(report)
}

pub fn print_report(mode: &str, report: &PromptReport) {
    println!("real prompts through locate ({mode}), advisory, never gated:");
    println!(
        "  {:<10} {:>5}  {:>6}  {:>6}  {:>6}",
        "", "cases", "top-1", "top-3", "top-5"
    );
    for (scope, t) in &report.tallies {
        println!(
            "  {scope:<10} {:>5}  {:.4}  {:.4}  {:.4}",
            t.cases,
            t.rate(t.top_1),
            t.rate(t.top_3),
            t.rate(t.top_5)
        );
    }
    if !report.misses.is_empty() {
        println!("\nright file outside the top 5:");
    }
    for miss in &report.misses {
        let prompt = miss.prompt.replace('\n', " ");
        println!("  [{}] {prompt}", miss.corpus);
        println!("    wanted {}", miss.wanted);
        println!("    got    {}", miss.got.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(path: &str) -> Hint {
        Hint {
            path: path.to_string(),
            qualname: format!("{path}.symbol"),
            sig: None,
            start_line: 1,
            end_line: 2,
            named: false,
        }
    }

    fn eval_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval")
    }

    #[test]
    fn files_rank_once_each_in_hint_order() {
        let hints = [hint("a.py"), hint("a.py"), hint("b.py"), hint("a.py")];
        assert_eq!(ranked_files(&hints), ["a.py", "b.py"]);
    }

    #[test]
    fn any_named_file_counts_and_the_best_one_sets_the_rank() {
        let wanted = vec!["c.py".to_string(), "b.py".to_string()];
        assert_eq!(rank_of(&["a.py", "b.py", "c.py"], &wanted), Some(1));
        assert_eq!(rank_of(&["a.py"], &wanted), None);
    }

    #[test]
    fn a_tally_counts_a_hit_at_every_cutoff_it_clears() {
        let mut tally = Tally::default();
        tally.record(Some(0));
        tally.record(Some(2));
        tally.record(Some(4));
        tally.record(Some(5));
        tally.record(None);
        assert_eq!(
            tally,
            Tally {
                cases: 5,
                top_1: 1,
                top_3: 2,
                top_5: 3
            }
        );
        assert_eq!(tally.rate(tally.top_5), 0.6);
    }

    #[test]
    fn an_empty_tally_rates_zero() {
        assert_eq!(Tally::default().rate(0), 0.0);
    }

    /// Labels are the only ground truth here, so one pointing at a file that
    /// doesn't exist would read as a retrieval miss forever.
    #[test]
    fn every_shipped_prompt_names_real_files_and_a_known_kind() {
        let root = eval_root();
        let cases = load_prompts(&root.join("queries/prompts.yaml")).unwrap();
        for corpus in ["pyapp", "polyglot"] {
            for kind in ["change", "bug"] {
                let count = cases
                    .iter()
                    .filter(|c| c.corpus == corpus && c.kind == kind)
                    .count();
                assert!(count >= 5, "only {count} {kind} prompts for {corpus}");
            }
        }
        for case in &cases {
            assert!(!case.prompt.trim().is_empty());
            assert!(!case.files.is_empty(), "{:?}", case.prompt);
            assert!(matches!(case.kind.as_str(), "change" | "bug"));
            for file in &case.files {
                let path = root.join("corpora").join(&case.corpus).join(file);
                assert!(path.is_file(), "{:?} names missing {file}", case.prompt);
            }
        }
    }

    #[test]
    fn the_shipped_set_scores_end_to_end_without_an_embedder() {
        let report = score_prompts(&eval_root(), None).unwrap();
        let overall = report.tallies["overall"];
        let by_corpus = report.tallies["pyapp"].cases + report.tallies["polyglot"].cases;
        let by_kind = report.tallies["bug"].cases + report.tallies["change"].cases;
        assert_eq!(overall.cases, by_corpus);
        assert_eq!(overall.cases, by_kind);
        assert!(overall.top_1 <= overall.top_3 && overall.top_3 <= overall.top_5);
        assert!(overall.top_5 > 0, "{overall:?}");
        assert_eq!(report.misses.len() as u32, overall.cases - overall.top_5);
    }
}
