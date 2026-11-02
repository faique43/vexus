//! `explore` tool (the flagship): a ranked map of the code behind a question.
//!
//! An agent that greps pays a little for pointers, judges them, then reads
//! narrow ranges. Returning source here would charge full price for every
//! guess before the model could reject one, so `explore` stays on the pointer
//! side of that split: one line per symbol, and the model calls `open` for
//! what it wants. A question naming exactly one symbol skips all of this and
//! gets that symbol's source and links. Pipeline:
//!
//! 1. `search_hybrid_scored(question, embed(question), floor, entry_limit)`
//!    → entry chunks, trimmed by `RELEVANCE_FLOOR`. `entry_limit` and the
//!    limits below scale with the corpus tier — see `params_for`. A
//!    `WeakVectorOnly` outcome (no keyword hit, nothing under the KNN floor)
//!    renders under a "weak match" note and skips step 2.
//! 2. One hop of callees and callers from the first `max_entry_symbols`
//!    distinct entry symbols adds neighbors, up to `max_neighbor_ids`, along
//!    with what the top hit's callers also call. Test code is skipped unless
//!    the question asks about tests.
//! 3. Rows render best first, grouped by file, until the budget runs out. The
//!    top `LINKED_ROWS` rows also show their call links, which is what the
//!    index knows that grep doesn't.

use std::collections::HashSet;

#[cfg(test)]
use vexus_watch::pipeline;

use vexus_core::model::estimate_tokens;
use vexus_core::priors::query_seeks_tests;
use vexus_core::query::{EdgeHit, Resolution, SymbolInfo};
use vexus_core::search::{SearchHit, SearchOutcome};
use vexus_core::Store;

use crate::state::{freshness_header, AppState};
use crate::tools::{apply_header, clamp_budget, embed_query};

const NEIGHBOR_DEPTH: u32 = 1;
const NEIGHBOR_LIMIT: usize = 10;

/// Rows that get a `calls:` / `called by:` line.
const LINKED_ROWS: usize = 3;
const MAX_LINKS: usize = 4;
/// Callers of the top hit whose other callees join the map.
const SIBLING_CALLERS: usize = 2;
const MAX_LABEL_CHARS: usize = 100;
/// The largest named symbol returned whole, in real tokens. Bigger ones fall
/// back to the map, where the model can open the part it wants.
const NAMED_SOURCE_TOKENS: u32 = 1200;

/// Search hits scoring under this fraction of the best hit are dropped, so a
/// map ends where relevance does. A hit in one search list scores about half
/// a hit in both, so a one-list hit survives and one carrying the 0.4 test
/// penalty, at 0.2, doesn't.
const RELEVANCE_FLOOR: f64 = 0.25;

fn keep_strong<T>(items: Vec<T>, score: impl Fn(&T) -> f64) -> Vec<T> {
    let best = items.iter().map(&score).fold(f64::MIN, f64::max);
    items
        .into_iter()
        .filter(|i| score(i) >= best * RELEVANCE_FLOOR)
        .collect()
}

/// Per-tier limits: entry hits, distinct entry symbols to expand, distinct
/// neighbor ids. Every map shares one budget, because a row costs about the
/// same however large the repo is.
struct ExploreParams {
    entry_limit: u32,
    max_entry_symbols: usize,
    max_neighbor_ids: usize,
}

const DEFAULT_BUDGET_TOKENS: u32 = 800;

fn params_for(tier: vexus_core::model::CorpusTier) -> ExploreParams {
    use vexus_core::model::CorpusTier;
    match tier {
        CorpusTier::Tiny => ExploreParams {
            entry_limit: 8,
            max_entry_symbols: 6,
            max_neighbor_ids: 12,
        },
        CorpusTier::Small => ExploreParams {
            entry_limit: 8,
            max_entry_symbols: 6,
            max_neighbor_ids: 16,
        },
        CorpusTier::Medium => ExploreParams {
            entry_limit: 10,
            max_entry_symbols: 5,
            max_neighbor_ids: 10,
        },
    }
}

const NO_MATCH_TEXT: &str = "nothing indexed matches that question — try distinctive words from the code, or 'status' to check index coverage.";

const WEAK_MATCH_TEXT: &str = "weak match — nothing indexed clearly matches this question; the rows below are only the nearest neighbors. For exact strings or comments, grep is the better tool here.";

const MAP_NOTE: &str = "Best match first. These are locations, not source: `open` \"path:start-end\" for what you need.";

/// One map line: a location, and a label to judge it by.
struct Row {
    path: String,
    start_line: u32,
    end_line: u32,
    label: String,
    symbol_id: Option<i64>,
}

fn truncated(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_LABEL_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_LABEL_CHARS).collect();
    format!("{cut}…")
}

fn symbol_row(info: &SymbolInfo) -> Row {
    Row {
        path: info.path.clone(),
        start_line: info.start_line,
        end_line: info.end_line,
        label: truncated(info.sig.as_deref().unwrap_or(&info.qualname)),
        symbol_id: Some(info.id),
    }
}

/// A row for a search hit. A symbol too long for one chunk gets the chunk
/// that matched, since `open` can stop at its budget before reaching it. A
/// chunk that only reaches further up, over a doc comment, is the whole symbol.
fn hit_row(hit: &SearchHit, info: &SymbolInfo) -> Row {
    let mut row = symbol_row(info);
    if hit.start_line > info.start_line || hit.end_line < info.end_line {
        row.start_line = hit.start_line;
        row.end_line = hit.end_line;
        let whole = format!("(part of {}-{})", info.start_line, info.end_line);
        row.label = format!("{} {whole}", row.label);
    }
    row
}

/// A label for a chunk outside any symbol: its first line of code, or failing
/// that of comment. Such a chunk can open on a `/**`, or on the `})` that ends
/// the symbol before it, and neither says what's there.
fn preamble_label(content: &str) -> String {
    let readable = |l: &&str| l.chars().any(char::is_alphanumeric);
    let comment = |l: &&str| ["/*", "*", "//", "#"].iter().any(|m| l.starts_with(m));
    let lines = || content.lines().map(str::trim).filter(readable);
    let label = lines()
        .find(|l| !comment(l))
        .or_else(|| lines().next())
        .or_else(|| content.lines().next());
    truncated(label.unwrap_or(""))
}

fn is_test_code(store: &Store, info: &SymbolInfo) -> bool {
    store.is_test_symbol(info.id, &info.path).unwrap_or(false)
}

/// The distinct symbols on the far end of `edges` that a map may show, up to
/// `NEIGHBOR_LIMIT`. The store's limit can't do this: rows arrive in id order,
/// so a cut before filtering lets repeat call sites and test code use up the
/// slots of everything indexed after them.
fn linked_symbols(store: &Store, edges: Vec<EdgeHit>, wants_tests: bool) -> Vec<SymbolInfo> {
    let mut seen = HashSet::new();
    edges
        .into_iter()
        .filter(|e| e.confidence.is_some() && seen.insert(e.symbol.id))
        .filter(|e| wants_tests || !is_test_code(store, &e.symbol))
        .map(|e| e.symbol)
        .take(NEIGHBOR_LIMIT)
        .collect()
}

fn callers_shown(store: &Store, symbol_id: i64, wants_tests: bool) -> Vec<SymbolInfo> {
    let edges = store.callers_of(symbol_id, NEIGHBOR_DEPTH, u32::MAX);
    linked_symbols(store, edges.unwrap_or_default(), wants_tests)
}

fn callees_shown(store: &Store, symbol_id: i64, wants_tests: bool) -> Vec<SymbolInfo> {
    let edges = store.callees_of(symbol_id, NEIGHBOR_DEPTH, u32::MAX);
    linked_symbols(store, edges.unwrap_or_default(), wants_tests)
}

/// `calls: a, b  called by: c` for a symbol, or `None` if it has no links.
fn link_line(store: &Store, symbol_id: i64, wants_tests: bool) -> Option<String> {
    let names = |symbols: Vec<SymbolInfo>| -> Vec<String> {
        let mut seen = HashSet::new();
        symbols
            .into_iter()
            .map(|s| s.name)
            .filter(|name| seen.insert(name.clone()))
            .take(MAX_LINKS)
            .collect()
    };
    let callees = names(callees_shown(store, symbol_id, wants_tests));
    let callers = names(callers_shown(store, symbol_id, wants_tests));
    let mut parts = Vec::new();
    if !callees.is_empty() {
        parts.push(format!("calls: {}", callees.join(", ")));
    }
    if !callers.is_empty() {
        parts.push(format!("called by: {}", callers.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("  "))
}

/// Groups rows by file, best file first, spending at most `budget_tokens`.
fn render_map(store: &Store, rows: &[Row], wants_tests: bool, budget_tokens: u32) -> String {
    let mut paths: Vec<&str> = Vec::new();
    for row in rows {
        if !paths.contains(&row.path.as_str()) {
            paths.push(&row.path);
        }
    }

    let linked: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.symbol_id)
        .take(LINKED_ROWS)
        .collect();
    let mut output = String::new();
    let mut used = 0;
    let mut omitted = 0;
    for path in paths {
        let mut group = format!("## {path}\n");
        let mut listed = false;
        for row in rows.iter().filter(|r| r.path == path) {
            let mut line = format!("{}-{} {}\n", row.start_line, row.end_line, row.label);
            if let Some(links) = row
                .symbol_id
                .filter(|id| linked.contains(id))
                .and_then(|id| link_line(store, id, wants_tests))
            {
                line.push_str(&format!("  {links}\n"));
            }
            let cost = estimate_tokens(&line) + if listed { 0 } else { estimate_tokens(&group) };
            if used + cost > budget_tokens {
                omitted += 1;
                continue;
            }
            used += cost;
            group.push_str(&line);
            listed = true;
        }
        if listed {
            output.push_str(&group);
        }
    }
    if omitted > 0 {
        output.push_str(&format!("{omitted} more not shown.\n"));
    }
    output
}

fn looks_like_code(word: &str) -> bool {
    word.contains('.')
        || word.contains("::")
        || word.contains('_')
        || word.chars().skip(1).any(char::is_uppercase)
}

/// The one symbol a question names, if it names exactly one.
fn named_symbol(store: &Store, question: &str) -> Option<SymbolInfo> {
    let mut named = question
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
        .filter(|w| looks_like_code(w))
        .filter_map(|w| match store.resolve_symbol(w) {
            Ok(Resolution::Exact(info)) if info.kind != "module" => Some(info),
            _ => None,
        });
    let first = named.next()?;
    named.all(|other| other.id == first.id).then_some(first)
}

/// A named symbol's location, links and source. Source comes along here
/// because this guess is nearly certain, which is the case a map exists to
/// avoid paying for.
fn render_named(store: &Store, info: &SymbolInfo) -> Option<String> {
    let chunks = store.symbol_source(info.id).ok()?;
    let source: String = chunks.into_iter().map(|c| c.3).collect();
    if estimate_tokens(&source) > clamp_budget(Some(NAMED_SOURCE_TOKENS), NAMED_SOURCE_TOKENS) {
        return None;
    }
    let mut out = format!(
        "## {}\n{}-{} {}\n",
        info.path,
        info.start_line,
        info.end_line,
        truncated(info.sig.as_deref().unwrap_or(&info.qualname))
    );
    if let Some(links) = link_line(store, info.id, false) {
        out.push_str(&format!("  {links}\n"));
    }
    out.push_str(&format!("```\n{source}```\n"));
    Some(out)
}

/// Pure inner implementation of the `explore` tool.
pub fn explore_text(state: &AppState, question: &str, budget_tokens: Option<u32>) -> String {
    let question_header = format!("explore: \"{question}\"\n\n");

    // Embed before locking: a real embedder's inference call must not hold
    // the store mutex, or it stalls every other tool call for its duration.
    let query_vec = embed_query(state, question);
    let knn_floor = super::knn_floor(state);

    let store = match state.lock_store_fresh() {
        Ok(s) => s,
        Err(msg) => return msg,
    };
    let fresh_header = freshness_header(&store);
    let params = params_for(
        store
            .corpus_tier()
            .unwrap_or(vexus_core::model::CorpusTier::Medium),
    );
    let budget_tokens = clamp_budget(budget_tokens, DEFAULT_BUDGET_TOKENS);
    if let Some(named) = named_symbol(&store, question).and_then(|i| render_named(&store, &i)) {
        return apply_header(fresh_header, format!("{question_header}{named}"));
    }
    let (hits, outcome) = match store.search_hybrid_scored(
        question,
        query_vec.as_deref(),
        knn_floor,
        params.entry_limit,
    ) {
        Ok(h) => h,
        Err(e) => return apply_header(fresh_header, format!("explore error: {e:#}")),
    };

    if hits.is_empty() {
        return apply_header(fresh_header, format!("{question_header}{NO_MATCH_TEXT}"));
    }
    let weak = outcome == SearchOutcome::WeakVectorOnly;
    let wants_tests = query_seeks_tests(question);

    let mut rows: Vec<Row> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut entry_symbols: Vec<i64> = Vec::new();
    for hit in keep_strong(hits, |h| h.score) {
        let info = hit
            .symbol_id
            .and_then(|id| store.symbol_info(id).ok().flatten())
            .filter(|info| info.kind != "module");
        match info {
            Some(info) => {
                if seen.insert(info.id) {
                    entry_symbols.push(info.id);
                    rows.push(hit_row(&hit, &info));
                }
            }
            None => rows.push(Row {
                path: hit.path.clone(),
                start_line: hit.start_line,
                end_line: hit.end_line,
                label: preamble_label(&hit.content),
                symbol_id: None,
            }),
        }
    }

    if !weak {
        let mut neighbors: Vec<SymbolInfo> = Vec::new();
        for (rank, id) in entry_symbols
            .iter()
            .take(params.max_entry_symbols)
            .enumerate()
        {
            let callers = callers_shown(&store, *id, wants_tests);
            let callees = callees_shown(&store, *id, wants_tests);
            // Siblings of the best match: the other steps its callers take
            // are usually the rest of the flow being asked about.
            let siblings: Vec<SymbolInfo> = if rank == 0 {
                callers
                    .iter()
                    .filter(|caller| caller.kind != "module")
                    .take(SIBLING_CALLERS)
                    .flat_map(|caller| callees_shown(&store, caller.id, wants_tests))
                    .collect()
            } else {
                Vec::new()
            };
            // Callees first: a question about how something works is
            // answered by what it does, more than by who reaches it.
            neighbors.extend(callees.into_iter().chain(callers).chain(siblings));
        }
        let unseen = neighbors
            .iter()
            .filter(|s| s.kind != "module" && seen.insert(s.id));
        rows.extend(unseen.take(params.max_neighbor_ids).map(symbol_row));
    }

    let note = if weak { WEAK_MATCH_TEXT } else { MAP_NOTE };
    let map = render_map(&store, &rows, wants_tests, budget_tokens);
    drop(store);
    apply_header(fresh_header, format!("{question_header}{note}\n\n{map}"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;

    fn write(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// A keyword-only `AppState` (embedder explicitly `None`, never the
    /// `VEXUS_EMBEDDER`-driven default) so `search_hybrid` here is driven
    /// purely by FTS — deterministic and immune to a mock embedder's
    /// content-blind hash vectors coincidentally pulling in chunks that
    /// have nothing to do with the query text. That determinism is what
    /// makes it possible to assert a neighbor chunk is present in the
    /// output *only* via graph expansion, not because it happened to also
    /// rank as a direct hit.
    fn keyword_only_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        pipeline::index_repo(root, &mut store).unwrap();
        let embedder_slot: OnceLock<Option<std::sync::Arc<dyn vexus_embed::Embedder>>> =
            OnceLock::new();
        let _ = embedder_slot.set(None);
        AppState {
            store: Mutex::new(Some(store)),
            embedder: embedder_slot,
            root: root.to_path_buf(),
            last_generation: std::sync::atomic::AtomicU64::new(0),
            is_writer: true,
        }
    }

    /// Every eval corpus is 30-40 files, i.e. Tiny or Small — the Medium
    /// tier's parameters, which is what any real repository actually hits,
    /// are never exercised by a graded
    /// query. This generates a deterministic corpus past the 2000-chunk
    /// boundary and pins the behaviour there: the tier resolves to Medium,
    /// explore answers within the larger default budget, and a production
    /// symbol still outranks a byte-identical copy of itself sitting on a
    /// test path.
    #[test]
    fn medium_tier_corpus_uses_medium_params_and_still_prefers_production_code() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // ~3 chunks per file (module preamble + two functions) — 800 files
        // clears the 2000-chunk Medium boundary with room to spare.
        for i in 0..800 {
            write(
                root,
                &format!("pkg/mod_{i:04}.py"),
                &format!(
                    "import os\n\n\ndef helper_{i:04}(x):\n    return x + {i}\n\n\ndef caller_{i:04}():\n    return helper_{i:04}({i})\n"
                ),
            );
        }
        // The distinctive symbol under test, plus a byte-identical copy of
        // it on a test path — same content, so only the structural prior
        // can separate them.
        let body = "def reticulate_splines(count):\n    \"\"\"Reticulate the splines.\"\"\"\n    return count * 2\n";
        write(root, "pkg/splines.py", body);
        write(root, "tests/test_splines.py", body);

        let state = keyword_only_state(root);
        {
            let store = state.lock_store_fresh().unwrap();
            assert_eq!(
                store.corpus_tier().unwrap(),
                vexus_core::model::CorpusTier::Medium,
                "fixture must clear the 2000-chunk boundary; got {} chunks",
                store.counts().unwrap().chunks
            );
        }

        let out = explore_text(&state, "reticulate splines", None);
        assert!(
            out.contains("pkg/splines.py"),
            "the production definition must be in the map: {out}"
        );
        let prod_at = out.find("pkg/splines.py");
        let test_at = out.find("tests/test_splines.py");
        if let (Some(p), Some(t)) = (prod_at, test_at) {
            assert!(
                p < t,
                "production must be ranked above its identical test-path copy: {out}"
            );
        }
        assert!(
            vexus_core::model::estimate_tokens(&out) > 0,
            "bundle must not be empty on a Medium corpus"
        );

        // A broad question matches everything, so this is the case that
        // fills the budget. Rendered text runs at ~2.5 chars per token.
        let broad = explore_text(&state, "helper caller", None);
        let real_tokens = broad.chars().count() * 2 / 5;
        assert!(
            real_tokens <= 1000,
            "a default map must stay near 800 real tokens, got ~{real_tokens}"
        );
    }

    #[test]
    fn items_far_below_the_best_score_are_dropped() {
        let kept = keep_strong(vec![1.0, 0.3, 0.2], |score| *score);

        assert_eq!(kept, vec![1.0, 0.3]);
    }

    /// Returning source for every guess makes the agent pay before it can
    /// judge: a NestJS answer cited 2 of the 9 files `explore` sent, at about
    /// 5,300 tokens. A map costs a few tokens per guess and lets the model
    /// open only what it wants.
    #[test]
    fn explore_returns_a_map_of_locations_rather_than_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pkg/core.py",
            "def settle(x):\n    \"\"\"Reticulates the splines.\"\"\"\n    secret_body_marker = helper(x)\n    return secret_body_marker\n\n\ndef helper(x):\n    return x\n",
        );
        write(
            root,
            "pkg/job.py",
            "from pkg.core import settle\n\n\ndef run_job():\n    return settle(1)\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(out.contains("pkg/core.py"), "{out}");
        assert!(out.contains("1-4"), "the symbol's line range: {out}");
        assert!(out.contains("settle"), "{out}");
        assert!(
            !out.contains("secret_body_marker"),
            "a map must not carry function bodies: {out}"
        );
        assert!(!out.contains("```"), "a map has no source fences: {out}");
    }

    #[test]
    fn explore_map_shows_call_links_for_the_top_hit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pkg/core.py",
            "def settle(x):\n    \"\"\"Reticulates the splines.\"\"\"\n    return helper(x)\n\n\ndef helper(x):\n    return x\n",
        );
        write(
            root,
            "pkg/job.py",
            "from pkg.core import settle\n\n\ndef run_job():\n    return settle(1)\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(out.contains("calls: helper"), "{out}");
        assert!(out.contains("called by: run_job"), "{out}");
    }

    /// Naming a symbol leaves nothing to rank. Grep answers that in a line,
    /// so a full map here cost more than the question was worth.
    #[test]
    fn a_question_naming_one_symbol_gets_that_symbol_and_not_a_map() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pkg/ledger.py",
            "class Ledger:\n    def settle(self, x):\n        body_marker = helper_step(x)\n        return body_marker\n\n    def audit(self, x):\n        return x\n\n\ndef helper_step(x):\n    return x\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "what does Ledger.settle do", None);

        assert!(out.contains("body_marker"), "the source: {out}");
        assert!(out.contains("calls: helper_step"), "the links: {out}");
        assert!(!out.contains("def audit"), "no other rows: {out}");
    }

    #[test]
    fn an_ordinary_question_still_gets_the_map() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        let out = explore_text(&state, "how does the process work", None);

        assert!(!out.contains("```"), "{out}");
    }

    /// A step in a flow is often a sibling of the match, not a neighbor:
    /// `place_order` doesn't call `quote_total`, the handler calling it does.
    #[test]
    fn explore_lists_what_the_top_hits_callers_also_call() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pkg/orders.py",
            "def place_order(x):\n    \"\"\"Reticulates the splines.\"\"\"\n    return x\n\n\ndef quote_total(x):\n    return x\n\n\ndef handler(x):\n    total = quote_total(x)\n    return place_order(total)\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(out.contains("def quote_total"), "{out}");
    }

    /// Rust keeps tests in the file they test, so the path says nothing.
    /// These used to fill neighbor slots and push real callees off the map.
    #[test]
    fn graph_expansion_skips_inline_rust_tests() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/lib.rs",
            "/// Reticulates the splines.\npub fn settle(x: u32) -> u32 {\n    x\n}\n\npub fn run_job() -> u32 {\n    settle(1)\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn settle_keeps_the_value() {\n        let got = settle(1);\n        assert!(got == 1);\n    }\n}\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(out.contains("run_job"), "{out}");
        assert!(!out.contains("settle_keeps_the_value"), "{out}");
    }

    /// A test that calls a function is a real caller, so one hop of graph
    /// expansion used to pull whole spec files into the bundle: on NestJS
    /// they took 58% of the result for a question about production code.
    #[test]
    fn graph_expansion_skips_test_callers_unless_the_question_asks_for_tests() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pkg/core.py",
            "def settle(x):\n    \"\"\"Reticulates the splines.\"\"\"\n    return x\n",
        );
        write(
            root,
            "pkg/job.py",
            "from pkg.core import settle\n\n\ndef run_job():\n    return settle(1)\n",
        );
        write(
            root,
            "tests/test_core.py",
            "from pkg.core import settle\n\n\ndef test_settle():\n    assert settle(1) == 1\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);
        assert!(
            out.contains("def run_job"),
            "the production caller must stay: {out}"
        );
        assert!(
            !out.contains("def test_settle"),
            "a test caller must not be expanded into a production question: {out}"
        );

        let out = explore_text(&state, "reticulates splines tests", None);
        assert!(
            out.contains("def test_settle"),
            "a question about tests must still reach them: {out}"
        );
    }

    /// `open` stops at its budget, so pointing at all of a long function can
    /// hand back everything but the part that matched.
    #[test]
    fn a_match_inside_a_long_function_points_at_that_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let padding: String = (0..600)
            .map(|i| format!("    value_{i} = compute_something({i}) + other_thing({i})\n"))
            .collect();
        write(
            root,
            "pkg/long.py",
            &format!(
                "def long_function(x):\n{padding}    reticulate_the_splines = x\n    return reticulate_the_splines\n"
            ),
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulate_the_splines", None);

        let row = out
            .lines()
            .find(|l| l.contains("def long_function"))
            .unwrap_or_else(|| panic!("{out}"));
        let range = row.split_once(' ').unwrap().0;
        let (start, end) = range.split_once('-').unwrap();
        let (start, end): (u32, u32) = (start.parse().unwrap(), end.parse().unwrap());
        assert!(start > 1 && end == 603, "{row}");
        assert!(row.contains("part of 1-603"), "{row}");
    }

    /// Doc comments stretch a chunk past its symbol. That isn't a partial
    /// match, and the row should keep naming the symbol.
    #[test]
    fn a_doc_comment_does_not_shift_a_whole_symbols_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/lib.rs",
            "/// Reticulates the splines.\npub fn settle(x: u32) -> u32 {\n    x\n}\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(out.contains("\n2-4 pub fn settle"), "{out}");
        assert!(!out.contains("part of"), "{out}");
    }

    /// A call at the top of a file has the file's module as its caller. A
    /// row for that is the whole file, and what the file calls is not a step
    /// in the flow being asked about.
    #[test]
    fn a_file_level_caller_is_not_a_neighbor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pkg/core.py",
            "def settle(x):\n    \"\"\"Reticulates the splines.\"\"\"\n    return x\n\n\ndef unrelated_step():\n    return 1\n",
        );
        write(
            root,
            "pkg/boot.py",
            "from pkg.core import settle, unrelated_step\n\nsettle(1)\nunrelated_step()\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(!out.contains("pkg/boot.py"), "{out}");
        assert!(!out.contains("unrelated_step"), "{out}");
    }

    #[test]
    fn a_preamble_row_is_labelled_with_what_it_does() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pkg/app.js",
            "/**\n * Reticulates the splines.\n */\n\nvar splines = require('splines');\n\nfunction other() {\n  return 1;\n}\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(out.contains("var splines = require('splines');"), "{out}");
        assert!(!out.contains(" /**"), "{out}");
    }

    #[test]
    fn preamble_labels_skip_punctuation_and_comment_markers() {
        assert_eq!(
            preamble_label("})\n\napp.listen(3000);\n"),
            "app.listen(3000);"
        );
        assert_eq!(
            preamble_label("/**\n * Docs.\n */\nconst a = 1;\n"),
            "const a = 1;"
        );
        assert_eq!(preamble_label("# Only a comment.\n"), "# Only a comment.");
        assert_eq!(preamble_label("/**\n * Only docs.\n */\n"), "* Only docs.");
        assert_eq!(preamble_label("})\n"), "})");
    }

    /// An unresolved `x.drain_queue()` names no target, so it can't be shown
    /// as a fact about any one of the methods sharing that name.
    #[test]
    fn an_unresolved_call_is_not_a_caller_of_every_method_with_that_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..7 {
            write(
                root,
                &format!("pkg/worker_{i}.py"),
                &format!(
                    "class Worker{i}:\n    def drain_queue(self, x):\n        \"\"\"Reticulates splines {i}.\"\"\"\n        return x\n\n\ndef local_caller_{i}(worker):\n    return worker.drain_queue(1)\n"
                ),
            );
        }
        write(
            root,
            "pkg/other.py",
            "def totally_unrelated(thing):\n    return thing.drain_queue(1)\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(out.contains("called by: local_caller_0"), "{out}");
        assert!(!out.contains("totally_unrelated"), "{out}");
    }

    /// Rows arrive in id order, so a file indexed late sits behind every
    /// test that calls the same function. Cutting before filtering emptied
    /// the slots and hid the one caller the question could use.
    #[test]
    fn test_callers_do_not_use_up_the_slots_of_a_production_caller() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pkg/core.py",
            "def settle(x):\n    \"\"\"Reticulates the splines.\"\"\"\n    return x\n",
        );
        let tests: String = (0..12)
            .map(|i| format!("def test_settle_{i}():\n    assert settle({i})\n\n\n"))
            .collect();
        write(
            root,
            "tests/test_core.py",
            &format!("from pkg.core import settle\n\n\n{tests}"),
        );
        write(
            root,
            "zz/job.py",
            "from pkg.core import settle\n\n\ndef run_job():\n    return settle(1)\n",
        );
        let state = keyword_only_state(root);

        let out = explore_text(&state, "reticulates splines", None);

        assert!(out.contains("called by: run_job"), "{out}");
        assert!(out.contains("def run_job"), "{out}");
    }

    /// `alpha_process` calls `unique_marker_beta`. The query text
    /// ("alpha_process") appears in alpha's own chunk (both its `def` line
    /// and the call site) but nowhere in beta's chunk, whose only content
    /// is its own `def`/`pass` lines — so a keyword-only `search_hybrid`
    /// surfaces alpha alone as the entry, and beta can only appear in the
    /// output via explore's one-hop callee expansion.
    fn chain_repo(root: &std::path::Path) {
        write(
            root,
            "chain.py",
            "def alpha_process():\n    unique_marker_beta()\n\n\ndef unique_marker_beta():\n    pass\n",
        );
    }

    #[test]
    fn explore_expands_one_hop_to_list_the_callee() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        let out = explore_text(&state, "alpha process", None);

        assert!(
            out.starts_with("explore: \"alpha process\"\n\n"),
            "got: {out:?}"
        );
        assert!(
            out.contains("1-2 def alpha_process():"),
            "the entry symbol must be listed: {out:?}"
        );
        assert!(
            out.contains("5-6 def unique_marker_beta():"),
            "the callee must be listed via one-hop expansion even though only \
             alpha matched the question text: {out:?}"
        );
        assert!(
            !out.contains("more not shown"),
            "both rows fit the default budget: {out:?}"
        );
    }

    #[test]
    fn explore_small_budget_keeps_the_best_row_and_counts_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        let out = explore_text(&state, "alpha process", Some(30));

        assert!(
            out.contains("def alpha_process():"),
            "the best row must be kept: {out:?}"
        );
        assert!(
            !out.contains("5-6 def unique_marker_beta():"),
            "a row past the budget must not render: {out:?}"
        );
        assert!(
            out.contains("1 more not shown."),
            "dropped rows must be counted: {out:?}"
        );
    }

    #[test]
    fn explore_no_match_returns_exact_text_with_header() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        let out = explore_text(&state, "", None);

        assert_eq!(
            out,
            "explore: \"\"\n\nnothing indexed matches that question — try distinctive words \
             from the code, or 'status' to check index coverage."
        );
    }

    /// `MockEmbedder` that declares a distance floor no hash vector will
    /// ever clear — every KNN candidate lands above it, so a query with no
    /// keyword overlap must come back `WeakVectorOnly`. A wrapper type
    /// (rather than the `VEXUS_KNN_FLOOR` env override) keeps the test free
    /// of env races with concurrently running tests.
    struct FlooredMock;
    impl vexus_embed::Embedder for FlooredMock {
        fn id(&self) -> &str {
            "mock"
        }
        fn dim(&self) -> usize {
            vexus_embed::MockEmbedder.dim()
        }
        fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            vexus_embed::MockEmbedder.embed(texts)
        }
        fn distance_floor(&self) -> Option<f64> {
            Some(1e-4)
        }
    }

    /// Like `keyword_only_state`, but embedded with the mock model and a
    /// floor-declaring embedder installed, so the weak-match path is
    /// reachable (it needs a query vector AND a floor).
    fn floored_mock_state(root: &std::path::Path) -> AppState {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        pipeline::index_repo(root, &mut store).unwrap();
        let embedder = FlooredMock;
        store
            .set_model(
                vexus_embed::Embedder::id(&embedder),
                vexus_embed::Embedder::dim(&embedder),
            )
            .unwrap();
        pipeline::embed_pending(&mut store, &embedder).unwrap();
        let embedder_slot: OnceLock<Option<std::sync::Arc<dyn vexus_embed::Embedder>>> =
            OnceLock::new();
        let _ = embedder_slot.set(Some(std::sync::Arc::new(FlooredMock)));
        AppState {
            store: Mutex::new(Some(store)),
            embedder: embedder_slot,
            root: root.to_path_buf(),
            last_generation: std::sync::atomic::AtomicU64::new(0),
            is_writer: true,
        }
    }

    #[test]
    fn explore_weak_match_prepends_note_and_skips_graph_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = floored_mock_state(root);

        // No keyword overlap with the fixture, and the floor guarantees no
        // KNN candidate counts as near: weak match. (On a corpus this tiny
        // every chunk comes back as an entry hit via the nearest-neighbor
        // fallback, so skipping expansion isn't observable in the output —
        // the note is the contract this test pins.)
        let out = explore_text(&state, "zzqqxx unrelated nonsense", None);
        assert!(
            out.contains("weak match —"),
            "weak note must be present: {out:?}"
        );

        // A query with keyword overlap stays strong: no weak note.
        let out = explore_text(&state, "alpha_process", None);
        assert!(
            !out.contains("weak match —"),
            "keyword hit must suppress the weak note: {out:?}"
        );
    }

    #[test]
    fn explore_prepends_freshness_header_when_reconciling_absent_when_fresh() {
        use vexus_watch::{set_freshness, Freshness};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        chain_repo(root);
        let state = keyword_only_state(root);

        let out_fresh = explore_text(&state, "alpha_process", None);
        assert!(
            !out_fresh.starts_with('\u{26a0}'),
            "Fresh index must not carry the warning header: {out_fresh:?}"
        );
        assert!(out_fresh.starts_with("explore: \"alpha_process\"\n\n"));

        {
            let mut guard = state.store.lock().unwrap();
            set_freshness(guard.as_mut().unwrap(), Freshness::Reconciling).unwrap();
        }
        let out_reconciling = explore_text(&state, "alpha_process", None);
        assert!(
            out_reconciling.starts_with(
                "⚠ index reconciling — results may miss recent changes\n\nexplore: \"alpha_process\"\n\n"
            ),
            "got: {out_reconciling:?}"
        );
    }
}
