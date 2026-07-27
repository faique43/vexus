//! Token-efficiency benchmark: how many tokens does answering a real
//! question cost with vexus versus with grep-and-read?
//!
//! This is the claim the whole project rests on, so the measurement is
//! deliberately unflattering to vexus where it can be:
//!
//! - The grep side is *executed*, not estimated. Every `grep` step is a real
//!   regex pass over the corpus and every matched line is counted; every
//!   `read` step really reads that file (or slice) and counts all of it.
//! - The grep transcripts in `tasks.yaml` are written as a competent agent
//!   would actually search, including the broad first grep and the wrong
//!   file — not as a strawman that greps for something no one would type.
//! - Both sides are counted with vexus's own `chars/4` estimator, so the
//!   ratio doesn't depend on a tokenizer choice.
//!
//! What it is *not*: a live agent study. It measures the cost of the context
//! each approach pulls in, not whether a model then uses it well. The
//! agent-in-the-loop harness under `eval/agent/` is where that gets measured.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use vexus_core::model::estimate_tokens;
use vexus_embed::Embedder;
use vexus_mcp::tools::{
    explore::explore_text, graph::callers_text, open::open_text, search::search_text,
};

use crate::corpus::index_into_temp_state;

#[derive(Debug, Deserialize)]
pub struct Task {
    pub task: String,
    pub corpus: String,
    pub grep_sim: Vec<GrepStep>,
    pub vexus: Vec<VexusStep>,
}

/// One step of the no-index transcript. Untagged so the YAML reads as the
/// shell verb it stands for (`- grep: "pattern"`), not a serde tag; the
/// variants' key names are distinct, so there's no ambiguity to resolve.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum GrepStep {
    /// A literal regex search over every file in the corpus. Cost = every
    /// matching line the agent would see, prefixed `path:lineno:` exactly as
    /// a real grep prints it.
    Grep { grep: String },
    /// Opening a file (optionally a line range). Cost = the whole slice.
    Read { read: ReadStep },
}

#[derive(Debug, Deserialize)]
pub struct ReadStep {
    pub path: String,
    pub start: Option<u32>,
    pub end: Option<u32>,
}

/// One step of the with-index transcript, named for the MCP tool it calls.
/// Untagged for the same reason as [`GrepStep`].
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum VexusStep {
    Explore { explore: String },
    Search { search: String },
    Open { open: String },
    Callers { callers: String },
}

pub fn load_tasks(path: &Path) -> Result<Vec<Task>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read token-bench tasks at {}", path.display()))?;
    serde_yaml::from_str(&text)
        .with_context(|| format!("parse token-bench tasks at {}", path.display()))
}

/// Walk every file under `root` that vexus would index, cheaply — the bench
/// only needs text files, and mirroring the indexer's exact scope rules here
/// would overstate grep's cost anyway (a real `grep -r` searches everything).
fn corpus_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .build()
    {
        let entry = entry?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            out.push(entry.into_path());
        }
    }
    out.sort();
    Ok(out)
}

/// Cost of one `grep` step: every matching line, formatted the way the agent
/// would receive it. A pattern that matches nothing still costs nothing —
/// that's the honest outcome of a search that missed, and the transcripts
/// include such steps on purpose.
fn grep_tokens(root: &Path, files: &[std::path::PathBuf], pattern: &str) -> Result<u32> {
    let re = regex::Regex::new(pattern)
        .with_context(|| format!("invalid grep pattern in tasks.yaml: {pattern}"))?;
    let mut total = 0u32;
    for path in files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                total += estimate_tokens(&format!("{rel}:{}:{line}\n", i + 1));
            }
        }
    }
    Ok(total)
}

fn read_tokens(root: &Path, step: &ReadStep) -> Result<u32> {
    let path = root.join(&step.path);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("token-bench read step: {}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let start = step.start.unwrap_or(1).max(1) as usize;
    let end = step.end.map(|e| e as usize).unwrap_or(lines.len());
    let slice = lines
        .get(start.saturating_sub(1)..end.min(lines.len()))
        .unwrap_or(&[])
        .join("\n");
    Ok(estimate_tokens(&slice))
}

#[derive(Debug)]
pub struct TaskResult {
    pub task: String,
    pub corpus: String,
    pub grep_tokens: u32,
    pub vexus_tokens: u32,
}

impl TaskResult {
    /// How many times more context the no-index route pulls in. `None` when
    /// vexus somehow returned nothing at all (guards against reporting an
    /// infinite, meaningless win).
    pub fn ratio(&self) -> Option<f64> {
        (self.vexus_tokens > 0).then(|| self.grep_tokens as f64 / self.vexus_tokens as f64)
    }
}

pub fn run(eval_root: &Path, embedder: Arc<dyn Embedder>, real: bool) -> Result<Vec<TaskResult>> {
    let tasks = load_tasks(&eval_root.join("token-bench").join("tasks.yaml"))?;
    let mut results = Vec::new();

    // Group by corpus so each one is indexed once, not once per task.
    let mut corpora: Vec<&str> = tasks.iter().map(|t| t.corpus.as_str()).collect();
    corpora.sort_unstable();
    corpora.dedup();

    for corpus in corpora {
        let corpus_root = eval_root.join("corpora").join(corpus);
        eprintln!("vexus-eval: token-bench indexing corpus {corpus}...");
        let (state, _db_dir) = index_into_temp_state(&corpus_root, &embedder)?;
        let files = corpus_files(&corpus_root)?;

        for task in tasks.iter().filter(|t| t.corpus == corpus) {
            let mut grep_total = 0u32;
            for step in &task.grep_sim {
                grep_total += match step {
                    GrepStep::Grep { grep } => grep_tokens(&corpus_root, &files, grep)?,
                    GrepStep::Read { read } => read_tokens(&corpus_root, read)?,
                };
            }

            let mut vexus_total = 0u32;
            for step in &task.vexus {
                let response = match step {
                    VexusStep::Explore { explore } => explore_text(&state, explore, None),
                    VexusStep::Search { search } => search_text(&state, search, None, None),
                    VexusStep::Open { open } => open_text(&state, open, None),
                    VexusStep::Callers { callers } => callers_text(&state, callers, None, None),
                };
                vexus_total += estimate_tokens(&response);
            }

            results.push(TaskResult {
                task: task.task.clone(),
                corpus: task.corpus.clone(),
                grep_tokens: grep_total,
                vexus_tokens: vexus_total,
            });
        }
    }

    let _ = real; // recorded by the caller in the report header
    Ok(results)
}

/// Corpus sizes for the scaling section. The hand-authored corpora above are
/// deliberately small (they exist to be hand-labelled for retrieval quality),
/// and at that size grep is *cheap* — so the per-task table alone would say
/// more about fixture size than about either approach. This sweep asks the
/// same question of the same synthetic repository at growing sizes, which is
/// the thing that actually distinguishes them.
const SCALING_SIZES: [usize; 3] = [50, 200, 500];

#[derive(Debug)]
pub struct ScalingPoint {
    pub files: usize,
    pub grep_tokens: u32,
    pub vexus_tokens: u32,
}

/// One representative *concept* search asked of the synthetic corpus at each
/// size in [`SCALING_SIZES`].
///
/// The pattern is deliberately a concept word (`helper`) rather than an
/// exact symbol name, because that is the search an agent actually runs: it
/// knows what it is looking for, not what it is called. That is also the
/// search whose cost grows with the repository — an exact-symbol grep matches
/// one definition whether the repo has 50 files or 50,000, and would make
/// grep look size-independent when the thing that hurts in practice is
/// sifting a concept's many matches.
///
/// The vexus side asks the equivalent question through one `explore` call,
/// whose cost is bounded by its token budget however large the repo gets.
pub fn run_scaling(embedder: Arc<dyn Embedder>) -> Result<Vec<ScalingPoint>> {
    let mut points = Vec::new();
    for files in SCALING_SIZES {
        eprintln!("vexus-eval: token-bench scaling at {files} files...");
        let dir = tempfile::tempdir().context("tempdir for scaling corpus")?;
        crate::perf::generate_synthetic_corpus_sized(dir.path(), files)?;

        let corpus = corpus_files(dir.path())?;
        let grep_tokens = grep_tokens(dir.path(), &corpus, "helper")?
            + read_tokens(
                dir.path(),
                &ReadStep {
                    path: "pkg00/module_0000.py".to_string(),
                    start: None,
                    end: None,
                },
            )?;

        let (state, _db_dir) = index_into_temp_state(dir.path(), &embedder)?;
        let vexus_tokens = estimate_tokens(&explore_text(
            &state,
            "what do the helper functions in this codebase do",
            None,
        ));

        points.push(ScalingPoint {
            files,
            grep_tokens,
            vexus_tokens,
        });
    }
    Ok(points)
}

/// Render `docs/BENCHMARKS.md`. Methodology and caveats are part of the
/// output on purpose: a ratio with no stated method is marketing, not
/// measurement.
pub fn render_markdown(results: &[TaskResult], scaling: &[ScalingPoint], real: bool) -> String {
    let mut out = String::new();
    out.push_str("# Token efficiency\n\n");
    out.push_str(
        "Answering the same question with the vexus index versus with grep and\n\
         file reads. Generated by `cargo run -p vexus-eval -- token-bench`.\n\n",
    );

    out.push_str("## The short version\n\n");
    out.push_str(
        "vexus trades a *bounded* cost for an *unbounded* one. A tool call returns a \
         top-ranked handful of results — capped by its token budget, and in practice \
         well under it — however large the repository is; grepping and reading costs \
         more the more code there is to search. So the honest answer to \"how many \
         tokens does vexus save?\" is: none at all on a small repository, and \
         increasingly many as the repository grows — with the caveat, spelled out under \
         Method, that the two sides answer at different breadth (grep returns every \
         match; vexus returns the top ones).\n\n\
         The scaling table below is the load-bearing measurement. The per-task table \
         after it is run against the small hand-authored corpora used for retrieval \
         scoring, and — precisely because they are small — grep often wins there. Both \
         are reported.\n\n",
    );

    if !scaling.is_empty() {
        out.push_str("## Cost versus repository size\n\n");
        out.push_str(
            "One question — \"what do the helper functions here do?\" — asked of the same \
             synthetic repository at three sizes. grep+read searches for the *concept* \
             (`helper`, the word an agent would actually reach for, not an exact symbol \
             name it doesn't know yet) and reads the most promising file; vexus is a \
             single `explore` call.\n\n",
        );
        out.push_str("| Files | grep+read | vexus | Ratio |\n| ---: | ---: | ---: | ---: |\n");
        for p in scaling {
            let ratio = if p.vexus_tokens > 0 {
                format!("{:.1}×", p.grep_tokens as f64 / p.vexus_tokens as f64)
            } else {
                "—".to_string()
            };
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                p.files, p.grep_tokens, p.vexus_tokens, ratio
            );
        }
        if let (Some(first), Some(last)) = (scaling.first(), scaling.last()) {
            let grep_growth = if first.grep_tokens > 0 {
                last.grep_tokens as f64 / first.grep_tokens as f64
            } else {
                0.0
            };
            let vexus_growth = if first.vexus_tokens > 0 {
                last.vexus_tokens as f64 / first.vexus_tokens as f64
            } else {
                0.0
            };
            let _ = write!(
                out,
                "\nFrom {} to {} files, grep+read grew {:.1}× while vexus grew {:.1}×.\n\n",
                first.files, last.files, grep_growth, vexus_growth
            );
        }
    }

    out.push_str("## Per task, on the retrieval-scoring corpora\n\n");
    out.push_str(
        "These two corpora are hand-authored and small (a few dozen short files) so \
         every symbol can be hand-labelled for the retrieval metrics. At that size a \
         grep is nearly free, so a ratio below 1 here means \"the repository is too \
         small for an index to pay for itself\" — not that the index failed to find the \
         answer. Retrieval quality is measured separately; see `eval/README.md`.\n\n",
    );
    out.push_str("| Task | Corpus | grep+read | vexus | Ratio |\n");
    out.push_str("| --- | --- | ---: | ---: | ---: |\n");
    for r in results {
        let ratio = match r.ratio() {
            Some(x) => format!("{x:.1}×"),
            None => "—".to_string(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            r.task, r.corpus, r.grep_tokens, r.vexus_tokens, ratio
        );
    }

    let embedder = if real {
        "the real ONNX model"
    } else {
        "the deterministic mock embedder"
    };
    let _ = write!(
        out,
        "\n## Method\n\n\
         Both corpora (`eval/corpora/`) are hand-authored fixture repositories, indexed \
         with {embedder}.\n\n\
         The **grep+read** column is executed, not estimated. Each task in \
         `eval/token-bench/tasks.yaml` carries an ordered transcript of the steps an \
         agent would run without an index: every `grep` is a real regex pass over the \
         corpus whose every matching line is counted (formatted `path:line:text`, as a \
         real grep prints it), and every `read` really reads that file or slice and \
         counts all of it. The transcripts include the wrong turns a real session has — \
         a first pattern that is too broad, a file opened at the wrong layer — because a \
         transcript that went straight to the answer would not be an honest baseline.\n\n\
         The **vexus** column runs the same functions `vexus serve` exposes over MCP \
         (`explore`, `search`, `open`, `callers`) at their default token budgets, and \
         counts the full response.\n\n\
         Both sides are measured with vexus's own `chars / 4` estimate, so the ratio \
         does not depend on a particular tokenizer.\n\n\
         The scaling table uses the deterministic synthetic corpus from the performance \
         harness (`vexus-eval perf`), generated at 50, 200 and 500 files. Its files are \
         formulaic, so it measures how each approach's cost *responds to size*, not how \
         well retrieval works on realistic code — that is what the hand-authored corpora \
         and the retrieval metrics are for.\n\n\
         Two properties of that table bound what it proves, and both favour the \
         conclusion, so they are worth stating plainly:\n\n\
         1. **Every file in the synthetic corpus contains the search term.** Each one \
         defines a `helper_*` function and calls its predecessor, so a `helper` grep \
         matches in all of them — a 100% hit rate. That is grep's *worst* case, not a \
         typical one; a term concentrated in a handful of files would scale far more \
         gently, and the ratio would shrink accordingly.\n\n\
         2. **The two sides return answers of different breadth.** grep returns every \
         match — an exhaustive answer. `explore` returns its top-ranked handful, using \
         well under a tenth of its 8000-token budget and *shrinking* slightly as the \
         corpus grows. So the flat line is top-k retrieval returning a fixed-size \
         answer, not a budget cap being enforced, and a ratio measured against an \
         exhaustive baseline flatters the top-k side by construction.\n\n\
         ## What this does not measure\n\n\
         This compares the cost of the context each approach pulls in. It is not a live \
         agent study: it does not measure whether a model then answers correctly, and it \
         cannot capture an agent that greps more (or fewer) times than the transcript. \
         `eval/agent/` holds the harness for measuring real sessions. Retrieval quality \
         itself is measured separately and gated in CI — see `eval/README.md`.\n\n\
         It also flatters grep in one specific way worth naming: the transcripts stop as \
         soon as the answer is in context. A real session often greps, reads the wrong \
         file, greps again with a better guess, and re-reads — and on a large repository \
         each of those rounds costs what the table's single round costs.\n\n\
         Reproduce: `cargo run -p vexus-eval -- token-bench` (add `--real` to use the \
         downloaded ONNX model).\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_is_none_when_vexus_returned_nothing() {
        let r = TaskResult {
            task: "t".into(),
            corpus: "c".into(),
            grep_tokens: 100,
            vexus_tokens: 0,
        };
        assert!(r.ratio().is_none());
    }

    #[test]
    fn ratio_divides_grep_by_vexus() {
        let r = TaskResult {
            task: "t".into(),
            corpus: "c".into(),
            grep_tokens: 900,
            vexus_tokens: 300,
        };
        assert_eq!(r.ratio(), Some(3.0));
    }

    #[test]
    fn shipped_tasks_file_parses_and_covers_both_corpora() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval");
        let tasks = load_tasks(&root.join("token-bench").join("tasks.yaml")).unwrap();
        assert!(tasks.len() >= 8, "want >= 8 tasks, got {}", tasks.len());
        for corpus in ["pyapp", "polyglot"] {
            assert!(
                tasks.iter().any(|t| t.corpus == corpus),
                "no task covers corpus {corpus}"
            );
        }
        // Every task must actually compare something on both sides.
        for t in &tasks {
            assert!(
                !t.grep_sim.is_empty(),
                "task {:?} has no grep steps",
                t.task
            );
            assert!(!t.vexus.is_empty(), "task {:?} has no vexus steps", t.task);
        }
    }

    #[test]
    fn markdown_states_its_method_and_limits() {
        let md = render_markdown(
            &[TaskResult {
                task: "t".into(),
                corpus: "pyapp".into(),
                grep_tokens: 900,
                vexus_tokens: 300,
            }],
            &[
                ScalingPoint {
                    files: 50,
                    grep_tokens: 100,
                    vexus_tokens: 500,
                },
                ScalingPoint {
                    files: 500,
                    grep_tokens: 1000,
                    vexus_tokens: 500,
                },
            ],
            false,
        );
        assert!(md.contains("3.0×"));
        assert!(md.contains("## Method"));
        assert!(md.contains("What this does not measure"));
        assert!(md.contains("mock embedder"));
        // The scaling section must report the growth of each side, since
        // that — not the per-task table — is the claim being made.
        assert!(md.contains("Cost versus repository size"));
        assert!(md.contains("grep+read grew 10.0× while vexus grew 1.0×"));
    }
}
