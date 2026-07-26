//! `vexus-eval`: retrieval-metric runner over the hand-authored fixture
//! corpora under `eval/` (Plan 5 Task 2). Indexes each corpus into a fresh
//! temp-directory index, runs every applicable query through the same code
//! paths the MCP tools use, computes `recall@5`/`recall@10`/`mrr`/`ndcg@10`/
//! `answer_in_bundle`/`edge_precision`/`edge_recall` (see `metrics.rs` for
//! the exact formulas and `corpus.rs` for how each is fed), prints a table,
//! and writes `eval/last-run.json`.

mod corpus;
mod metrics;
mod queries;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use vexus_embed::Embedder;

#[derive(Parser)]
#[command(
    name = "vexus-eval",
    about = "Retrieval-metric runner over eval/ fixture corpora"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index each corpus, run every applicable query, compute metrics, print
    /// a table, and write eval/last-run.json.
    Run {
        /// Use the real ONNX embedder (requires a model already downloaded
        /// under ~/.vexus/models — run `vexus index` once against any repo
        /// first to fetch it) instead of the deterministic mock embedder.
        #[arg(long)]
        real: bool,
        /// Only evaluate this one corpus (a directory name under
        /// eval/corpora/), instead of every corpus found there.
        #[arg(long)]
        corpus: Option<String>,
    },
}

/// The full `eval/last-run.json` shape: metrics per corpus, plus "overall"
/// pooled across every corpus evaluated this run (see `metrics.rs`'s
/// aggregation note — pooled, not a mean of the per-corpus figures).
/// `per_corpus` is a `BTreeMap` so both the printed table and the JSON's key
/// order are stable (alphabetical), not insertion-order-dependent.
#[derive(Debug, Serialize)]
struct Report {
    mode: String,
    per_corpus: BTreeMap<String, metrics::MetricSet>,
    overall: metrics::MetricSet,
}

/// `crates/vexus-eval` -> repo root's `eval/` — resolved from the crate's
/// build-time manifest dir (not the process's current working directory),
/// same as Task 1's `eval_corpora_validation.rs::eval_root()`, so this works
/// regardless of where `cargo run -p vexus-eval` is invoked from within the
/// workspace.
fn eval_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../eval")
        .canonicalize()
        .context("eval/ must exist at the repo root")
}

/// The pure computation behind `run`: build the embedder, evaluate every
/// named corpus, and pool into a [`Report`] — no printing, no JSON file
/// write. Split out from `run` so the determinism test below can call this
/// twice and diff the result without ever touching `eval/last-run.json`.
fn compute_report(embedder: Arc<dyn Embedder>, root: &Path, names: &[String]) -> Result<Report> {
    ensure!(!names.is_empty(), "no corpora to evaluate");
    let mode = if embedder.id() == vexus_embed::MockEmbedder.id() {
        "mock"
    } else {
        "real"
    };

    let mut per_corpus_accum: BTreeMap<String, corpus::CorpusAccum> = BTreeMap::new();
    for name in names {
        eprintln!("vexus-eval: indexing + scoring corpus {name}...");
        let accum = corpus::eval_corpus(root, name, &embedder)?;
        per_corpus_accum.insert(name.clone(), accum);
    }

    let overall_accum = per_corpus_accum
        .values()
        .copied()
        .fold(corpus::CorpusAccum::default(), corpus::CorpusAccum::combine);

    let per_corpus = per_corpus_accum
        .iter()
        .map(|(name, accum)| (name.clone(), accum.metrics()))
        .collect();

    Ok(Report {
        mode: mode.to_string(),
        per_corpus,
        overall: overall_accum.metrics(),
    })
}

fn print_table(report: &Report) {
    println!("mode: {}\n", report.mode);
    for (name, m) in &report.per_corpus {
        print_metric_set(name, m);
        println!();
    }
    print_metric_set("overall", &report.overall);
}

fn print_metric_set(label: &str, m: &metrics::MetricSet) {
    println!("{label}:");
    println!("  recall@5          {:.4}", m.recall_at_5);
    println!("  recall@10         {:.4}", m.recall_at_10);
    println!("  mrr               {:.4}", m.mrr);
    println!("  ndcg@10           {:.4}", m.ndcg_at_10);
    println!("  answer_in_bundle  {:.4}", m.answer_in_bundle);
    println!("  edge_precision    {:.4}", m.edge_precision);
    println!("  edge_recall       {:.4}", m.edge_recall);
}

fn run(real: bool, only_corpus: Option<String>) -> Result<()> {
    let embedder: Arc<dyn Embedder> = if real {
        corpus::build_real_embedder()?
    } else {
        Arc::new(vexus_embed::MockEmbedder)
    };

    let root = eval_root()?;
    let names = match only_corpus {
        Some(name) => vec![name],
        None => corpus::discover_corpora(&root)?,
    };

    let report = compute_report(embedder, &root, &names)?;
    print_table(&report);

    let out_path = root.join("last-run.json");
    let mut json = serde_json::to_string_pretty(&report).context("serialize report")?;
    json.push('\n');
    std::fs::write(&out_path, json).with_context(|| format!("write {}", out_path.display()))?;
    eprintln!("vexus-eval: wrote {}", out_path.display());

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { real, corpus } => run(real, corpus),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_embedder() -> Arc<dyn Embedder> {
        Arc::new(vexus_embed::MockEmbedder)
    }

    /// The binding determinism requirement: running the whole mock-mode
    /// pipeline twice, in-process, over every real corpus must produce
    /// byte-identical JSON — no reliance on wall-clock time, random seeds,
    /// or HashMap iteration order leaking into the result.
    #[test]
    fn mock_mode_report_is_identical_across_two_independent_runs() {
        let root = eval_root().unwrap();
        let names = corpus::discover_corpora(&root).unwrap();
        assert!(
            names.len() >= 2,
            "expected >= 2 real corpora, got {names:?}"
        );

        let first = compute_report(mock_embedder(), &root, &names).unwrap();
        let second = compute_report(mock_embedder(), &root, &names).unwrap();

        let first_json = serde_json::to_string_pretty(&first).unwrap();
        let second_json = serde_json::to_string_pretty(&second).unwrap();
        assert_eq!(
            first_json, second_json,
            "mock-mode metrics must be identical across independent runs"
        );

        // A quick sanity check on the shape itself, so a future regression
        // that made every metric trivially 0 (e.g. an empty query set) would
        // still fail this test even though "0 == 0" is technically stable.
        assert_eq!(first.mode, "mock");
        assert!(first.overall.recall_at_10 > 0.0, "{:?}", first.overall);
    }

    #[test]
    fn compute_report_errors_on_an_empty_corpus_list() {
        let root = eval_root().unwrap();
        let err = compute_report(mock_embedder(), &root, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("no corpora"));
    }
}
