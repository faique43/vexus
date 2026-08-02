//! Deterministic synthetic 500-file corpus generator + timing harness for
//! the perf budgets: index-500, a single-file incremental
//! update, reconcile-100-changed, and search/explore p50/p99 latency (mock
//! embedder only — embed throughput is excluded from gating; see the
//! Constraints, since a real ONNX model's throughput is hardware-dependent,
//! not something vexus's own code can regress or fix).
//!
//! Every byte of the synthetic corpus is a pure function of a file index
//! `i` — no `rand`, no OS entropy, no wall-clock seeding anywhere in
//! [`generate_synthetic_corpus`] or the functions it calls — so two
//! generator runs, in the same process or different ones, always produce
//! byte-identical output (see
//! `tests::generating_the_corpus_twice_is_byte_identical_across_two_temp_dirs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use vexus_core::Store;
use vexus_embed::{Embedder, MockEmbedder};
use vexus_mcp::state::AppState;
use vexus_mcp::tools::explore::explore_text;
use vexus_watch::{reconcile, update_file, UpdateOutcome};

/// Total files in the synthetic perf corpus (binding: "500 files").
pub const FILE_COUNT: usize = 500;
/// How many of those files get a formulaic edit for the reconcile timing
/// (binding: "reconcile-100-changed").
const RECONCILE_CHANGED_COUNT: usize = 100;
/// Query repetitions for the search/explore p50/p99 windows (binding:
/// "≥200 reps").
const QUERY_REPS: usize = 200;
/// Safely below the smallest per-language file count the 500-file,
/// round-robin-by-3 corpus produces (167/167/166 — see `lang_for_index`'s
/// doc comment), so every `helper_{k:04}` name [`query_text_for_rep`]
/// produces was really indexed, for all 3 languages.
const QUERY_SYMBOL_MOD: usize = 150;

// Deliberately still 3 of the 18 supported languages: this harness
// measures pipeline scaling (walk/parse/chunk/store throughput), not
// language coverage — parse_snapshots and the polyglot eval corpus own
// per-language correctness. Extending the generators ×18 would multiply
// advisory-job CI time for no additional signal.
const LANGS: [&str; 3] = ["python", "typescript", "rust"];

/// Which of the 3 languages file index `i` gets, round-robin. For a
/// [`FILE_COUNT`] of 500 this yields 167 python files (i = 0, 3, 6, ...,
/// 498), 167 typescript files (i = 1, 4, ..., 499), and 166 rust files (i =
/// 2, 5, ..., 497).
fn lang_for_index(i: usize) -> &'static str {
    LANGS[i % LANGS.len()]
}

/// This file's position among files of its own language (0-based) — since
/// languages are assigned round-robin by `i % 3`, that's `i / 3`.
fn local_index(i: usize) -> usize {
    i / LANGS.len()
}

/// Deterministic repo-relative path for file index `i`. Grouped into
/// `pkgNN/` directories of 30 files each purely for directory-walk realism
/// (real repos nest files); the grouping has no bearing on language
/// assignment or the call-chain below.
fn file_rel_path(i: usize) -> String {
    let pkg = i / 30;
    let ext = match lang_for_index(i) {
        "python" => "py",
        "typescript" => "ts",
        _ => "rs",
    };
    format!("pkg{pkg:02}/module_{i:04}.{ext}")
}

/// Module `k`'s python content: a `helper_{k:04}` function that calls the
/// previous same-language file's `helper_{k-1:04}` (a real cross-file call
/// for the resolver to walk) plus a small class for chunk-count realism.
/// `k == 0` (the first python file) has no predecessor, so it's a base case
/// returning a constant instead of delegating. The call-site text alone is
/// enough for vexus's name-based call resolution — no `import` statement is
/// needed for either tree-sitter's structural parse (which never resolves
/// imports) or vexus's resolver (which matches bare call-site names against
/// symbol names project-wide, not import paths).
fn python_file(k: usize) -> String {
    let body = if k == 0 {
        format!(
            "def helper_{k:04}(x):\n    \"\"\"Base case: the first module of this language has no predecessor.\"\"\"\n    return x + {k}\n"
        )
    } else {
        let prev = k - 1;
        format!(
            "def helper_{k:04}(x):\n    \"\"\"Delegate to the previous module's helper — a real cross-file call chain.\"\"\"\n    return helper_{prev:04}(x) + {k}\n"
        )
    };
    format!(
        "\"\"\"Module {k} of the synthetic perf corpus (formulaic content — see perf.rs).\"\"\"\n\n\n{body}\n\nclass Widget{k:04}:\n    \"\"\"Deterministic widget number {k}.\"\"\"\n\n    def value(self):\n        \"\"\"Return this widget's fixed value.\"\"\"\n        return {k}\n"
    )
}

/// Module `k`'s typescript content — same shape as [`python_file`].
fn typescript_file(k: usize) -> String {
    let body = if k == 0 {
        format!("export function helper_{k:04}(x: number): number {{\n  return x + {k};\n}}\n")
    } else {
        let prev = k - 1;
        format!(
            "export function helper_{k:04}(x: number): number {{\n  return helper_{prev:04}(x) + {k};\n}}\n"
        )
    };
    format!(
        "/** Module {k} of the synthetic perf corpus (formulaic content — see perf.rs). */\n\n{body}\n/** Deterministic widget number {k}. */\nexport class Widget{k:04} {{\n  value(): number {{\n    return {k};\n  }}\n}}\n"
    )
}

/// Module `k`'s rust content — same shape as [`python_file`].
fn rust_file(k: usize) -> String {
    let body = if k == 0 {
        format!("pub fn helper_{k:04}(x: i64) -> i64 {{\n    x + {k}\n}}\n")
    } else {
        let prev = k - 1;
        format!("pub fn helper_{k:04}(x: i64) -> i64 {{\n    helper_{prev:04}(x) + {k}\n}}\n")
    };
    format!(
        "/// Module {k} of the synthetic perf corpus (formulaic content — see perf.rs).\n{body}\n/// Deterministic widget number {k}.\npub struct Widget{k:04};\n\nimpl Widget{k:04} {{\n    pub fn value(&self) -> i64 {{\n        {k}\n    }}\n}}\n"
    )
}

/// Deterministic (repo-relative path, content) pair for file index `i` of
/// the fixed [`FILE_COUNT`]-file synthetic corpus — a pure function of `i`
/// alone (see the module doc's determinism guarantee).
fn synth_file(i: usize) -> (String, String) {
    let rel = file_rel_path(i);
    let k = local_index(i);
    let content = match lang_for_index(i) {
        "python" => python_file(k),
        "typescript" => typescript_file(k),
        _ => rust_file(k),
    };
    (rel, content)
}

/// Writes [`FILE_COUNT`] deterministic, formulaic files under `root` —
/// python/typescript/rust round-robin by index (see `lang_for_index`), each
/// (after the first of its language) calling its same-language predecessor.
/// No randomness API of any kind is used anywhere in this path — every byte
/// is `i`-derived arithmetic and string formatting — so two calls (even
/// from different processes) always produce byte-identical trees.
pub fn generate_synthetic_corpus(root: &Path) -> Result<()> {
    generate_synthetic_corpus_sized(root, FILE_COUNT)
}

/// Same generator, with an explicit file count — used by the token-efficiency
/// benchmark's scaling section, which needs the *same* corpus shape at
/// several sizes to show how each approach's cost responds to repository
/// size. Still fully deterministic and randomness-free.
pub fn generate_synthetic_corpus_sized(root: &Path, file_count: usize) -> Result<()> {
    for i in 0..file_count {
        let (rel, content) = synth_file(i);
        let path = root.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// Rewrites file index `i` with its original formulaic content plus a
/// deterministic, language-correct trailing comment line naming
/// `edit_round` — different content than the original (so a real
/// reindex/reconcile is forced, never a `SkippedUnchanged` no-op), but still
/// fully deterministic. `edit_round` only needs to differ between the
/// incremental-update edit and the reconcile edit so the two timings never
/// accidentally share a no-op path; its exact value carries no other
/// meaning.
fn apply_deterministic_edit(root: &Path, i: usize, edit_round: u32) -> Result<String> {
    let (rel, mut content) = synth_file(i);
    let comment_prefix = if lang_for_index(i) == "python" {
        "#"
    } else {
        "//"
    };
    content.push_str(&format!(
        "\n{comment_prefix} perf-edit-round-{edit_round}\n"
    ));
    let path = root.join(&rel);
    std::fs::write(&path, &content).with_context(|| format!("write {}", path.display()))?;
    Ok(rel)
}

/// Deterministic query text for search/explore rep `i` — cycles through
/// `helper_{k:04}` names guaranteed to exist for EVERY language (see
/// [`QUERY_SYMBOL_MOD`]'s doc comment), so every rep is a real,
/// hit-producing query rather than a miss that would time an
/// empty-result fast path instead of realistic ranking work.
fn query_text_for_rep(i: usize) -> String {
    format!("helper_{:04}", i % QUERY_SYMBOL_MOD)
}

/// The measured timings from one `perf` run, in milliseconds. `embed_500_ms`
/// and both `_p50_ms` fields are informational (printed, appended to
/// history) but never compared against [`Budgets`] — see [`check_budgets`].
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Timings {
    pub index_500_ms: f64,
    pub embed_500_ms: f64,
    pub incremental_ms: f64,
    pub reconcile_100_ms: f64,
    pub search_p50_ms: f64,
    pub search_p99_ms: f64,
    pub explore_p50_ms: f64,
    pub explore_p99_ms: f64,
}

/// `bench/budgets.json`'s shape — the committed thresholds `Timings` are
/// compared against (all in milliseconds: index-500 < 30s,
/// incremental < 1s, reconcile-100 < 10s,
/// search p99 < 200ms, explore p99 < 600ms).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Budgets {
    pub index_500_ms: f64,
    pub incremental_ms: f64,
    pub reconcile_100_ms: f64,
    pub search_p99_ms: f64,
    pub explore_p99_ms: f64,
}

pub fn load_budgets(path: &Path) -> Result<Budgets> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// One measured value exceeding its budget counterpart.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetViolation {
    pub metric: &'static str,
    pub actual_ms: f64,
    pub budget_ms: f64,
}

/// Which measured [`Timings`] exceed their [`Budgets`] counterpart —
/// strictly greater-than, so a measurement exactly equal to its budget
/// passes (mirrors the ratchet gate's own strict "> 0.02", not ">="). Only
/// only the 5 gated fields are compared here —
/// `embed_500_ms` and both p50s are informational/printed only, never
/// budgeted (embed throughput is explicitly excluded from gating; p50 has
/// no named budget, only p99 does).
pub fn check_budgets(timings: &Timings, budgets: &Budgets) -> Vec<BudgetViolation> {
    let pairs: [(&'static str, f64, f64); 5] = [
        ("index_500_ms", timings.index_500_ms, budgets.index_500_ms),
        (
            "incremental_ms",
            timings.incremental_ms,
            budgets.incremental_ms,
        ),
        (
            "reconcile_100_ms",
            timings.reconcile_100_ms,
            budgets.reconcile_100_ms,
        ),
        (
            "search_p99_ms",
            timings.search_p99_ms,
            budgets.search_p99_ms,
        ),
        (
            "explore_p99_ms",
            timings.explore_p99_ms,
            budgets.explore_p99_ms,
        ),
    ];
    pairs
        .into_iter()
        .filter(|(_, actual, budget)| actual > budget)
        .map(|(metric, actual_ms, budget_ms)| BudgetViolation {
            metric,
            actual_ms,
            budget_ms,
        })
        .collect()
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Nearest-rank percentile (`p` in `0.0..=100.0`) of an ALREADY-SORTED
/// (ascending) sample slice. `rank = round(p/100 * (n-1))`, clamped to the
/// last index — e.g. for a 101-sample `0..=100` set, p50 lands exactly on
/// index 50 (value `50.0`) and p99 exactly on index 99 (value `99.0`), which
/// is what `tests::percentile_hand_computed_on_a_101_sample_set` checks.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    assert!(!sorted.is_empty(), "percentile of an empty sample set");
    let rank = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Sorts `samples` ascending and returns `(p50, p99)`.
fn p50_p99(mut samples: Vec<f64>) -> (f64, f64) {
    samples.sort_by(|a, b| a.total_cmp(b));
    (percentile(&samples, 50.0), percentile(&samples, 99.0))
}

#[derive(Debug, Serialize)]
struct HistoryRow {
    unix_ts: u64,
    #[serde(flatten)]
    timings: Timings,
}

/// Appends one JSON-line row (`{"unix_ts": ..., ...timings fields}`) to
/// `path`, creating it if it doesn't exist yet — `bench/history.jsonl` is
/// gitignored (see `.gitignore`'s comment), a running local/CI log, not a
/// committed artifact.
fn append_history(path: &Path, timings: Timings) -> Result<()> {
    use std::io::Write;
    let unix_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let row = HistoryRow { unix_ts, timings };
    let mut line = serde_json::to_string(&row).context("serialize history row")?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {} for append", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("append to {}", path.display()))
}

fn print_perf_table(timings: &Timings, budgets: &Budgets) {
    println!("perf ({FILE_COUNT}-file synthetic corpus, mock embedder):\n");
    println!(
        "  index-500           {:>10.1} ms   (budget {:.0} ms)",
        timings.index_500_ms, budgets.index_500_ms
    );
    println!(
        "  embed-500 (mock)    {:>10.1} ms   (informational — excluded from gating)",
        timings.embed_500_ms
    );
    println!(
        "  incremental          {:>9.1} ms   (budget {:.0} ms)",
        timings.incremental_ms, budgets.incremental_ms
    );
    println!(
        "  reconcile-100        {:>9.1} ms   (budget {:.0} ms)",
        timings.reconcile_100_ms, budgets.reconcile_100_ms
    );
    println!(
        "  search   p50/p99     {:>6.1} / {:.1} ms   (p99 budget {:.0} ms)",
        timings.search_p50_ms, timings.search_p99_ms, budgets.search_p99_ms
    );
    println!(
        "  explore  p50/p99     {:>6.1} / {:.1} ms   (p99 budget {:.0} ms)",
        timings.explore_p50_ms, timings.explore_p99_ms, budgets.explore_p99_ms
    );
}

/// `crates/vexus-eval` -> repo root's `bench/` — resolved from the crate's
/// build-time manifest dir (not the process's current working directory),
/// same pattern as `main.rs`'s `eval_root()`.
fn bench_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../bench")
        .canonicalize()
        .context("bench/ must exist at the repo root")
}

/// `vexus-eval perf`: generates the synthetic corpus into a temp dir, times
/// index/incremental-update/reconcile/search/explore against it (mock
/// embedder), prints a table, appends `bench/history.jsonl`, and compares
/// against `bench/budgets.json`. Always exits `Ok(())` (prints violations,
/// if any) unless `enforce` is set, in which case any budget violation
/// becomes an `Err` (exit 1).
pub fn run(enforce: bool) -> Result<()> {
    let bench_root = bench_root()?;
    let budgets = load_budgets(&bench_root.join("budgets.json"))?;

    let corpus_dir = tempfile::tempdir().context("tempdir for synthetic perf corpus")?;
    generate_synthetic_corpus(corpus_dir.path())?;

    let db_dir = tempfile::tempdir().context("tempdir for perf index db")?;
    let mut store = Store::open(&db_dir.path().join("index.db")).context("open temp perf store")?;

    let t = Instant::now();
    let report = vexus_watch::pipeline::index_repo(corpus_dir.path(), &mut store)
        .context("index synthetic perf corpus")?;
    let index_500_ms = elapsed_ms(t);
    ensure!(
        report.indexed == FILE_COUNT,
        "expected to index all {FILE_COUNT} synthetic files, indexed {} (failed: {:?})",
        report.indexed,
        report.failed
    );

    let embedder = MockEmbedder;
    store.set_model(embedder.id(), embedder.dim())?;
    let t = Instant::now();
    vexus_watch::pipeline::embed_pending(&mut store, &embedder).context("embed perf corpus")?;
    let embed_500_ms = elapsed_ms(t);

    // Incremental: one deterministic single-file edit, timing update_file
    // alone (the preceding disk write is deliberately outside the timer —
    // vexus isn't responsible for how fast the OS/editor writes a file).
    let incr_rel = apply_deterministic_edit(corpus_dir.path(), 0, 1)?;
    let t = Instant::now();
    let outcome = update_file(&mut store, Some(&embedder), corpus_dir.path(), &incr_rel)
        .context("incremental update_file timing")?;
    let incremental_ms = elapsed_ms(t);
    ensure!(
        matches!(outcome, UpdateOutcome::Reindexed { .. }),
        "incremental edit did not reindex as expected: {outcome:?}"
    );

    // Reconcile-100-changed: a distinct edit round (2) across a disjoint
    // 100-file range (file 0 was already just re-edited above by the
    // incremental step) — again, only the reconcile CALL is timed.
    for i in 1..=RECONCILE_CHANGED_COUNT {
        apply_deterministic_edit(corpus_dir.path(), i, 2)?;
    }
    let t = Instant::now();
    let recon =
        reconcile(&mut store, Some(&embedder), corpus_dir.path()).context("reconcile timing")?;
    let reconcile_100_ms = elapsed_ms(t);
    ensure!(
        recon.updated == RECONCILE_CHANGED_COUNT,
        "expected exactly {RECONCILE_CHANGED_COUNT} updated files during reconcile timing, got {}",
        recon.updated
    );

    // From here on, read-only timing (search/explore) — wrap the same store
    // in an AppState (mirrors corpus.rs's index_into_temp_state) so both
    // paths go through the exact lock/freshness-probe route the MCP server
    // itself uses per call, not a bare Store method call.
    let embedder_arc: Arc<dyn Embedder> = Arc::new(embedder);
    let embedder_slot: OnceLock<Option<Arc<dyn Embedder>>> = OnceLock::new();
    let _ = embedder_slot.set(Some(embedder_arc.clone()));
    let state = AppState {
        store: Mutex::new(Some(store)),
        embedder: embedder_slot,
        root: corpus_dir.path().to_path_buf(),
        last_generation: AtomicU64::new(0),
        is_writer: true,
    };

    let mut search_samples = Vec::with_capacity(QUERY_REPS);
    for i in 0..QUERY_REPS {
        let q = query_text_for_rep(i);
        let t = Instant::now();
        let query_vec = embedder_arc
            .embed(&[q.as_str()])
            .with_context(|| format!("embed perf query {q:?}"))?
            .pop();
        {
            let store = state
                .lock_store_fresh()
                .map_err(|msg| anyhow::anyhow!("{msg}"))?;
            let _hits = store.search_hybrid(&q, query_vec.as_deref(), 20)?;
        }
        search_samples.push(elapsed_ms(t));
    }
    let (search_p50_ms, search_p99_ms) = p50_p99(search_samples);

    let mut explore_samples = Vec::with_capacity(QUERY_REPS);
    for i in 0..QUERY_REPS {
        let q = query_text_for_rep(i);
        let t = Instant::now();
        let _bundle = explore_text(&state, &q, None);
        explore_samples.push(elapsed_ms(t));
    }
    let (explore_p50_ms, explore_p99_ms) = p50_p99(explore_samples);

    let timings = Timings {
        index_500_ms,
        embed_500_ms,
        incremental_ms,
        reconcile_100_ms,
        search_p50_ms,
        search_p99_ms,
        explore_p50_ms,
        explore_p99_ms,
    };

    print_perf_table(&timings, &budgets);
    append_history(&bench_root.join("history.jsonl"), timings)?;

    let violations = check_budgets(&timings, &budgets);
    if violations.is_empty() {
        println!("\nperf: within budget");
    } else {
        println!("\nperf: budget exceeded:");
        for v in &violations {
            println!(
                "  {} = {:.1} ms > budget {:.1} ms",
                v.metric, v.actual_ms, v.budget_ms
            );
        }
        if enforce {
            anyhow::bail!("{} perf budget(s) exceeded (--enforce)", violations.len());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generating_the_corpus_twice_is_byte_identical_across_two_temp_dirs() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        generate_synthetic_corpus(dir_a.path()).unwrap();
        generate_synthetic_corpus(dir_b.path()).unwrap();

        for i in 0..FILE_COUNT {
            let (rel, _) = synth_file(i);
            let a = std::fs::read_to_string(dir_a.path().join(&rel))
                .unwrap_or_else(|e| panic!("{rel}: {e}"));
            let b = std::fs::read_to_string(dir_b.path().join(&rel)).unwrap();
            assert_eq!(a, b, "{rel} differs between two independent generations");
        }
    }

    #[test]
    fn generator_produces_exactly_file_count_files_across_three_languages() {
        let dir = tempfile::tempdir().unwrap();
        generate_synthetic_corpus(dir.path()).unwrap();

        let mut exts: HashSet<String> = HashSet::new();
        for i in 0..FILE_COUNT {
            let (rel, _) = synth_file(i);
            let path = dir.path().join(&rel);
            assert!(path.is_file(), "{rel} missing on disk");
            exts.insert(path.extension().unwrap().to_str().unwrap().to_string());
        }
        assert_eq!(
            exts,
            HashSet::from(["py".to_string(), "ts".to_string(), "rs".to_string()])
        );
    }

    #[test]
    fn the_first_file_of_each_language_is_a_base_case_with_no_predecessor_call() {
        // Global indices 0, 1, 2 are local_index 0 for python/typescript/rust
        // respectively (round-robin by 3) -- each must be a base case with
        // no helper_{prev} call, since there IS no previous same-language
        // file to delegate to.
        for i in 0..LANGS.len() {
            let (_, content) = synth_file(i);
            // "helper_" appears exactly once in a base case (its own `def
            // helper_0000`/`fn helper_0000` declaration) — a delegating file
            // has a second occurrence in its call to the predecessor. Also
            // assert "+ 0" is present as the base-case body's literal
            // expression (`x + 0`, with or without a `return` keyword —
            // rust's is an implicit return with no `return`/`;`).
            assert_eq!(
                content.matches("helper_").count(),
                1,
                "file {i} (k=0) must reference helper_ exactly once (its own def, no predecessor call): {content}"
            );
            assert!(
                content.contains("+ 0"),
                "file {i} (k=0) should be a base case (`x + 0`): {content}"
            );
        }
        // The next file of each language (local_index 1, global index i+3)
        // DOES call its predecessor, helper_0000 — two distinct
        // "helper_" occurrences (its own def + the delegated call).
        for i in 0..LANGS.len() {
            let (_, content) = synth_file(i + LANGS.len());
            assert_eq!(
                content.matches("helper_").count(),
                2,
                "file {} (k=1) should reference helper_ twice (its own def + calling helper_0000): {content}",
                i + LANGS.len()
            );
            assert!(
                content.contains("helper_0000"),
                "file {} (k=1) should call helper_0000: {content}",
                i + LANGS.len()
            );
        }
    }

    #[test]
    fn synth_file_content_is_a_pure_function_of_the_index() {
        // Calling it twice for the same i (even with unrelated calls to
        // other indices in between) must give byte-identical output.
        let (rel_a, content_a) = synth_file(42);
        let _ = synth_file(7);
        let _ = synth_file(499);
        let (rel_b, content_b) = synth_file(42);
        assert_eq!(rel_a, rel_b);
        assert_eq!(content_a, content_b);
    }

    #[test]
    fn apply_deterministic_edit_changes_the_content_hash_and_uses_a_line_comment() {
        let dir = tempfile::tempdir().unwrap();
        generate_synthetic_corpus(dir.path()).unwrap();
        let (rel, original) = synth_file(2); // rust
        let rel_returned = apply_deterministic_edit(dir.path(), 2, 9).unwrap();
        assert_eq!(rel, rel_returned);
        let edited = std::fs::read_to_string(dir.path().join(&rel)).unwrap();
        assert_ne!(original, edited, "the edit must change the file's content");
        assert!(edited.starts_with(&original));
        assert!(edited.contains("// perf-edit-round-9"));
    }

    // ---- percentile --------------------------------------------------------

    #[test]
    fn percentile_hand_computed_on_a_101_sample_set() {
        let samples: Vec<f64> = (0..=100).map(|x| x as f64).collect();
        assert_eq!(percentile(&samples, 50.0), 50.0);
        assert_eq!(percentile(&samples, 99.0), 99.0);
        assert_eq!(percentile(&samples, 0.0), 0.0);
        assert_eq!(percentile(&samples, 100.0), 100.0);
    }

    #[test]
    fn p50_p99_sorts_before_computing() {
        let samples = vec![5.0, 1.0, 3.0, 2.0, 4.0]; // unsorted 1..=5
        let (p50, p99) = p50_p99(samples);
        assert_eq!(p50, 3.0); // median of 1..=5
        assert_eq!(p99, 5.0); // round(0.99*4)=4 -> sorted[4] = 5.0
    }

    // ---- check_budgets -------------------------------------------------------

    fn sample_budgets() -> Budgets {
        Budgets {
            index_500_ms: 30_000.0,
            incremental_ms: 1_000.0,
            reconcile_100_ms: 10_000.0,
            search_p99_ms: 200.0,
            explore_p99_ms: 600.0,
        }
    }

    fn sample_timings_within(b: &Budgets) -> Timings {
        Timings {
            index_500_ms: b.index_500_ms - 1.0,
            embed_500_ms: 1.0,
            incremental_ms: b.incremental_ms - 1.0,
            reconcile_100_ms: b.reconcile_100_ms - 1.0,
            search_p50_ms: 1.0,
            search_p99_ms: b.search_p99_ms - 1.0,
            explore_p50_ms: 1.0,
            explore_p99_ms: b.explore_p99_ms - 1.0,
        }
    }

    #[test]
    fn check_budgets_all_within_budget_is_empty() {
        let budgets = sample_budgets();
        let timings = sample_timings_within(&budgets);
        assert!(check_budgets(&timings, &budgets).is_empty());
    }

    #[test]
    fn check_budgets_flags_exactly_the_metric_over_budget() {
        let budgets = sample_budgets();
        let mut timings = sample_timings_within(&budgets);
        timings.search_p99_ms = budgets.search_p99_ms + 1.0;
        let violations = check_budgets(&timings, &budgets);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert_eq!(violations[0].metric, "search_p99_ms");
        assert_eq!(violations[0].actual_ms, budgets.search_p99_ms + 1.0);
        assert_eq!(violations[0].budget_ms, budgets.search_p99_ms);
    }

    #[test]
    fn check_budgets_exactly_at_budget_is_not_a_violation() {
        let budgets = sample_budgets();
        let mut timings = sample_timings_within(&budgets);
        timings.index_500_ms = budgets.index_500_ms; // exactly equal, not over
        assert!(check_budgets(&timings, &budgets).is_empty());
    }

    #[test]
    fn check_budgets_flags_multiple_violations_independently() {
        let budgets = sample_budgets();
        let mut timings = sample_timings_within(&budgets);
        timings.incremental_ms = budgets.incremental_ms + 5.0;
        timings.explore_p99_ms = budgets.explore_p99_ms + 5.0;
        let violations = check_budgets(&timings, &budgets);
        let metrics: HashSet<&str> = violations.iter().map(|v| v.metric).collect();
        assert_eq!(metrics, HashSet::from(["incremental_ms", "explore_p99_ms"]));
    }

    #[test]
    fn check_budgets_never_flags_the_informational_only_fields() {
        // embed_500_ms and both p50s have no budget field at all to compare
        // against — pushing them to absurd values must never produce a
        // violation, since check_budgets only ever looks at the 5 gated
        // fields.
        let budgets = sample_budgets();
        let mut timings = sample_timings_within(&budgets);
        timings.embed_500_ms = 1_000_000.0;
        timings.search_p50_ms = 1_000_000.0;
        timings.explore_p50_ms = 1_000_000.0;
        assert!(check_budgets(&timings, &budgets).is_empty());
    }

    #[test]
    fn load_budgets_parses_the_real_committed_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/budgets.json");
        let budgets = load_budgets(&path).unwrap();
        assert_eq!(budgets.index_500_ms, 30_000.0);
        assert_eq!(budgets.incremental_ms, 1_000.0);
        assert_eq!(budgets.reconcile_100_ms, 10_000.0);
        assert_eq!(budgets.search_p99_ms, 200.0);
        assert_eq!(budgets.explore_p99_ms, 600.0);
    }
}
