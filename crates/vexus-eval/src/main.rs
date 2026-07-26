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
use serde::{Deserialize, Serialize};
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
    /// Compare a fresh run against the committed baseline
    /// (eval/baseline-mock.json, or eval/baseline-real.json with --real);
    /// exit 1 if any metric dropped more than 0.02 absolute against it.
    Check {
        /// Compare a --real run against eval/baseline-real.json instead of
        /// the mock run against eval/baseline-mock.json. Never used by CI
        /// (real requires a downloaded model) — for local/nightly
        /// pre-release checks only.
        #[arg(long)]
        real: bool,
    },
    /// Overwrite the committed baseline with a fresh run's metrics.
    Bless {
        /// Bless eval/baseline-real.json from a --real run instead of
        /// eval/baseline-mock.json from a mock run.
        #[arg(long)]
        real: bool,
    },
}

/// The full `eval/last-run.json` shape: metrics per corpus, plus "overall"
/// pooled across every corpus evaluated this run (see `metrics.rs`'s
/// aggregation note — pooled, not a mean of the per-corpus figures).
/// `per_corpus` is a `BTreeMap` so both the printed table and the JSON's key
/// order are stable (alphabetical), not insertion-order-dependent.
#[derive(Debug, Serialize, Deserialize)]
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

/// Plan 5 Global Constraints' ratchet rule: "ANY metric dropping > 0.02
/// absolute" fails the gate. Strict `>` (not `>=`) — a drop of exactly 0.02
/// passes.
const REGRESSION_THRESHOLD: f64 = 0.02;

/// Floating-point tolerance around [`REGRESSION_THRESHOLD`] so a drop that's
/// meant to land exactly on the boundary (e.g. two values that are each
/// already rounded to 4 decimal places by [`metrics::round4`]) isn't
/// mis-classified by binary-float representation noise a few ULPs either
/// side of the mathematical `0.02`.
const REGRESSION_EPSILON: f64 = 1e-9;

/// One named metric's baseline vs. current value — `label` is
/// `"{scope}.{metric}"`, e.g. `"pyapp.recall@5"` or `"overall.mrr"`, which
/// is what `check`'s printed regression/improvement list names.
#[derive(Debug, Clone, PartialEq)]
struct MetricDelta {
    label: String,
    baseline: f64,
    current: f64,
}

impl MetricDelta {
    /// `current - baseline`: negative is a drop, positive is an improvement.
    fn delta(&self) -> f64 {
        self.current - self.baseline
    }
}

/// The 7 named metrics (see the Plan 5 Global Constraints) paired up
/// between `base` and `cur` for one scope (a corpus name, or "overall").
fn metric_pairs(
    base: &metrics::MetricSet,
    cur: &metrics::MetricSet,
) -> [(&'static str, f64, f64); 7] {
    [
        ("recall@5", base.recall_at_5, cur.recall_at_5),
        ("recall@10", base.recall_at_10, cur.recall_at_10),
        ("mrr", base.mrr, cur.mrr),
        ("ndcg@10", base.ndcg_at_10, cur.ndcg_at_10),
        (
            "answer_in_bundle",
            base.answer_in_bundle,
            cur.answer_in_bundle,
        ),
        ("edge_precision", base.edge_precision, cur.edge_precision),
        ("edge_recall", base.edge_recall, cur.edge_recall),
    ]
}

/// Compares every named metric (per matching corpus, plus "overall")
/// between `baseline` and `current`, returning `(regressions,
/// improvements)`:
/// - a regression is any metric whose value dropped by strictly more than
///   [`REGRESSION_THRESHOLD`] absolute (the binding ratchet rule).
/// - an improvement is any metric that went up at all (no threshold — the
///   binding only says "improvements print but don't fail").
///
/// A corpus present in only one of the two reports is skipped entirely for
/// that scope (comparing a corpus against nothing has no well-defined
/// "drop") — this never happens for CI's own `check` invocation (no
/// `--corpus` filter, so both sides always cover every corpus
/// `discover_corpora` finds), but guards a hand-built `Report` in a test, or
/// a future corpus addition mid-development, from panicking or silently
/// mis-scoring.
fn diff_reports(baseline: &Report, current: &Report) -> (Vec<MetricDelta>, Vec<MetricDelta>) {
    let mut regressions = Vec::new();
    let mut improvements = Vec::new();

    let mut scopes: Vec<(&str, &metrics::MetricSet, &metrics::MetricSet)> = baseline
        .per_corpus
        .iter()
        .filter_map(|(name, base)| {
            current
                .per_corpus
                .get(name)
                .map(|cur| (name.as_str(), base, cur))
        })
        .collect();
    scopes.push(("overall", &baseline.overall, &current.overall));

    for (scope, base, cur) in scopes {
        for (metric, base_val, cur_val) in metric_pairs(base, cur) {
            let delta = MetricDelta {
                label: format!("{scope}.{metric}"),
                baseline: base_val,
                current: cur_val,
            };
            let d = delta.delta();
            if d < -(REGRESSION_THRESHOLD + REGRESSION_EPSILON) {
                regressions.push(delta);
            } else if d > REGRESSION_EPSILON {
                improvements.push(delta);
            }
        }
    }
    (regressions, improvements)
}

/// `eval/baseline-mock.json`, or `eval/baseline-real.json` when `real`.
fn baseline_path(eval_root: &Path, real: bool) -> PathBuf {
    eval_root.join(if real {
        "baseline-real.json"
    } else {
        "baseline-mock.json"
    })
}

/// Loads a previously-`bless`ed baseline [`Report`]. A missing file gets its
/// own clear message (the binding "missing baseline → exit 1 telling user
/// to bless" rule) instead of a generic I/O error; any other failure
/// (unreadable permissions, corrupt JSON) keeps its own `Context`-wrapped
/// message instead, since re-`bless`ing over a permissions error or a
/// genuinely corrupt file wouldn't actually fix that problem.
fn load_baseline(path: &Path, real: bool) -> Result<Report> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let flag = if real { " --real" } else { "" };
            anyhow::bail!(
                "no baseline at {} — run `cargo run -p vexus-eval -- bless{flag}` first, then commit it",
                path.display()
            );
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn check(real: bool) -> Result<()> {
    let root = eval_root()?;
    let embedder: Arc<dyn Embedder> = if real {
        corpus::build_real_embedder()?
    } else {
        Arc::new(vexus_embed::MockEmbedder)
    };
    let names = corpus::discover_corpora(&root)?;
    let current = compute_report(embedder, &root, &names)?;

    let baseline_file = baseline_path(&root, real);
    let baseline = load_baseline(&baseline_file, real)?;

    let (regressions, improvements) = diff_reports(&baseline, &current);

    if !improvements.is_empty() {
        println!("improvements:");
        for m in &improvements {
            println!(
                "  {:<28} {:.4} -> {:.4}  (+{:.4})",
                m.label,
                m.baseline,
                m.current,
                m.delta()
            );
        }
        println!();
    }

    if regressions.is_empty() {
        println!(
            "eval-gate: PASS — no metric dropped more than {REGRESSION_THRESHOLD:.2} absolute against {}",
            baseline_file.display()
        );
        Ok(())
    } else {
        println!("regressions (> {REGRESSION_THRESHOLD:.2} absolute drop):");
        for m in &regressions {
            println!(
                "  {:<28} {:.4} -> {:.4}  ({:.4})",
                m.label,
                m.baseline,
                m.current,
                m.delta()
            );
        }
        anyhow::bail!(
            "eval-gate: FAIL — {} metric(s) regressed against {}",
            regressions.len(),
            baseline_file.display()
        );
    }
}

fn bless(real: bool) -> Result<()> {
    let root = eval_root()?;
    let embedder: Arc<dyn Embedder> = if real {
        corpus::build_real_embedder()?
    } else {
        Arc::new(vexus_embed::MockEmbedder)
    };
    let names = corpus::discover_corpora(&root)?;
    let report = compute_report(embedder, &root, &names)?;
    print_table(&report);

    let path = baseline_path(&root, real);
    let mut json = serde_json::to_string_pretty(&report).context("serialize baseline")?;
    json.push('\n');
    std::fs::write(&path, &json).with_context(|| format!("write {}", path.display()))?;
    eprintln!("vexus-eval: wrote baseline {}", path.display());
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { real, corpus } => run(real, corpus),
        Cmd::Check { real } => check(real),
        Cmd::Bless { real } => bless(real),
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

    // ---- ratchet gate (Plan 5 Task 3): diff_reports / load_baseline --------

    fn metric_set(v: f64) -> metrics::MetricSet {
        metrics::MetricSet {
            recall_at_5: v,
            recall_at_10: v,
            mrr: v,
            ndcg_at_10: v,
            answer_in_bundle: v,
            edge_precision: v,
            edge_recall: v,
        }
    }

    fn report_with(
        per_corpus: &[(&str, metrics::MetricSet)],
        overall: metrics::MetricSet,
    ) -> Report {
        Report {
            mode: "mock".to_string(),
            per_corpus: per_corpus
                .iter()
                .map(|(n, m)| (n.to_string(), *m))
                .collect(),
            overall,
        }
    }

    #[test]
    fn diff_reports_flags_a_drop_over_the_threshold_as_a_regression() {
        // Every one of the 7 metrics drops by 0.03 (> 0.02) in both the
        // corpus scope and "overall" -> 14 regressions total, 0 improvements.
        let baseline = report_with(&[("pyapp", metric_set(0.50))], metric_set(0.50));
        let current = report_with(&[("pyapp", metric_set(0.47))], metric_set(0.47));

        let (regressions, improvements) = diff_reports(&baseline, &current);

        assert!(improvements.is_empty(), "{improvements:?}");
        assert_eq!(regressions.len(), 14, "{regressions:?}");
        assert!(regressions.iter().any(|m| m.label == "pyapp.recall@5"));
        assert!(regressions.iter().any(|m| m.label == "overall.mrr"));
        for m in &regressions {
            assert!((m.delta() + 0.03).abs() < 1e-9, "{m:?}");
        }
    }

    #[test]
    fn diff_reports_a_drop_of_exactly_the_threshold_is_not_a_regression() {
        // Binding is "dropping > 0.02 absolute" — strictly greater, so an
        // exact 0.02 drop must pass.
        let base_ms = metrics::MetricSet {
            recall_at_5: 0.50,
            ..metric_set(0.20)
        };
        let cur_ms = metrics::MetricSet {
            recall_at_5: 0.48,
            ..metric_set(0.20)
        };
        let baseline = report_with(&[("pyapp", base_ms)], base_ms);
        let current = report_with(&[("pyapp", cur_ms)], cur_ms);

        let (regressions, _) = diff_reports(&baseline, &current);
        assert!(
            regressions.is_empty(),
            "{regressions:?} — an exact 0.02 drop must not fail, only strictly > 0.02 does"
        );
    }

    #[test]
    fn diff_reports_a_drop_just_over_the_threshold_is_a_regression() {
        // overall's own MetricSet is passed separately from pyapp's here
        // (unlike the other tests above), specifically so this test can
        // isolate the regression to ONE scope ("pyapp" only, not "overall"
        // too) and assert exactly one named regression comes out.
        let base_ms = metrics::MetricSet {
            recall_at_5: 0.50,
            ..metric_set(0.20)
        };
        let cur_ms = metrics::MetricSet {
            recall_at_5: 0.4799,
            ..metric_set(0.20)
        }; // -0.0201
        let overall = metric_set(0.20); // held identical on both sides
        let baseline = report_with(&[("pyapp", base_ms)], overall);
        let current = report_with(&[("pyapp", cur_ms)], overall);

        let (regressions, _) = diff_reports(&baseline, &current);
        assert_eq!(regressions.len(), 1, "{regressions:?}");
        assert_eq!(regressions[0].label, "pyapp.recall@5");
    }

    #[test]
    fn diff_reports_an_increase_is_an_improvement_not_a_regression() {
        let base_ms = metrics::MetricSet {
            mrr: 0.30,
            ..metric_set(0.20)
        };
        let cur_ms = metrics::MetricSet {
            mrr: 0.40,
            ..metric_set(0.20)
        };
        let baseline = report_with(&[("pyapp", base_ms)], base_ms);
        let current = report_with(&[("pyapp", cur_ms)], cur_ms);

        let (regressions, improvements) = diff_reports(&baseline, &current);
        assert!(regressions.is_empty(), "{regressions:?}");
        assert!(
            improvements
                .iter()
                .any(|m| m.label == "pyapp.mrr" && (m.delta() - 0.10).abs() < 1e-9),
            "{improvements:?}"
        );
    }

    #[test]
    fn diff_reports_skips_a_corpus_present_in_only_one_side() {
        // A corpus dropped from (or added to) the corpus set between bless
        // and check has no well-defined "before/after" for itself and must
        // not be treated as a regression or improvement.
        let baseline = report_with(
            &[
                ("pyapp", metric_set(0.5)),
                ("stale_corpus", metric_set(0.9)),
            ],
            metric_set(0.5),
        );
        let current = report_with(
            &[("pyapp", metric_set(0.5)), ("new_corpus", metric_set(0.1))],
            metric_set(0.5),
        );

        let (regressions, improvements) = diff_reports(&baseline, &current);
        assert!(regressions.is_empty(), "{regressions:?}");
        assert!(improvements.is_empty(), "{improvements:?}");
    }

    #[test]
    fn load_baseline_missing_file_names_bless_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("baseline-mock.json");
        let err = load_baseline(&missing, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("bless"), "{msg}");
        assert!(!msg.contains("--real"), "{msg}");
    }

    #[test]
    fn load_baseline_missing_real_file_names_bless_with_the_real_flag() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("baseline-real.json");
        let err = load_baseline(&missing, true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("bless --real"), "{msg}");
    }

    #[test]
    fn load_baseline_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline-mock.json");
        let report = report_with(&[("pyapp", metric_set(0.3))], metric_set(0.3));
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();

        let loaded = load_baseline(&path, false).unwrap();
        assert_eq!(loaded.overall.mrr, 0.3);
        assert_eq!(loaded.per_corpus["pyapp"].mrr, 0.3);
    }

    #[test]
    fn baseline_path_selects_mock_or_real_filename() {
        let root = Path::new("/tmp/eval");
        assert_eq!(
            baseline_path(root, false),
            Path::new("/tmp/eval/baseline-mock.json")
        );
        assert_eq!(
            baseline_path(root, true),
            Path::new("/tmp/eval/baseline-real.json")
        );
    }
}
