//! Pure metric computation — no `Store`, no I/O. Every public function here
//! takes already-fetched data (a ranked list of qualnames, a graded map, a
//! rendered bundle string, or a small closure standing in for a graph query)
//! so each one is directly hand-computable and unit-testable without a real
//! index. `crates/vexus-eval/src/corpus.rs` is the only caller that touches
//! `vexus_core::Store`/`vexus_mcp` — it fetches the raw data and feeds it
//! through the functions below.
//!
//! ## Aggregation scheme (binding)
//!
//! Every metric is "aggregated per corpus + overall". Two shapes of pooling
//! are used, and both share one rule: **overall is never a mean of per-corpus
//! means** — it recomputes the same formula over the pooled underlying units
//! (queries, or labeled edge pairs) from every corpus, so a corpus with more
//! queries contributes proportionally more to the overall figure, exactly as
//! it would if all corpora's queries had been one big corpus.
//!
//! - `recall@5`, `recall@10`, `mrr`, `ndcg@10`, `answer_in_bundle` are each a
//!   mean of one score per applicable query. [`Accum`] tracks `(sum, count)`;
//!   its `combine` is plain addition of both fields, so
//!   `overall = corpus_a.combine(corpus_b).mean()` is exactly "pool every
//!   query from every corpus, then average" — never `mean(mean_a, mean_b)`.
//! - `edge_precision`/`edge_recall` are ratios over counts that aren't
//!   per-query means to begin with (precision's denominator is a deduped
//!   edge set, not a query count) — [`EdgeCounts`] carries the three raw
//!   counts (`found`/`labeled`/`returned`) and combines by addition the same
//!   way, so `overall = corpus_a.combine(corpus_b)` pools the raw counts
//!   before dividing.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Running `(sum, count)` for a mean-of-per-query metric. `combine` pools two
/// accumulators by adding both fields — see the module doc's aggregation
/// note for why this (not averaging two means) is what "overall" must do.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Accum {
    pub sum: f64,
    pub count: u64,
}

impl Accum {
    /// Record one query's score.
    pub fn push(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
    }

    /// Mean of every pushed value; `0.0` when nothing was ever pushed (an
    /// empty applicable-query set — e.g. a corpus with no graded queries —
    /// reads as `0.0` rather than `NaN`).
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }

    /// Pool two accumulators (see the module doc: this is the ONLY correct
    /// way to combine per-corpus figures into an overall one).
    pub fn combine(self, other: Self) -> Self {
        Self {
            sum: self.sum + other.sum,
            count: self.count + other.count,
        }
    }
}

/// recall@k for one `search` query: the fraction of `expected` qualnames
/// present anywhere in the first `k` entries of `ranked`. `ranked` must
/// already have `SearchHit`s with `qualname: None` filtered out (see
/// `corpus.rs`) — this function has no notion of an unranked row, only a
/// list of qualnames in rank order.
///
/// `0.0` when `expected` is empty (never happens for real ground truth —
/// `eval/queries/*.yaml` rows always carry a non-empty `expect`, enforced by
/// the corpora validation test — but a division by zero here would be
/// worse than a defensive `0.0`).
pub fn recall_at_k(ranked: &[String], expected: &[String], k: usize) -> f64 {
    if expected.is_empty() {
        return 0.0;
    }
    let top_k: HashSet<&str> = ranked.iter().take(k).map(String::as_str).collect();
    let hits = expected
        .iter()
        .filter(|e| top_k.contains(e.as_str()))
        .count();
    hits as f64 / expected.len() as f64
}

/// Reciprocal rank of the first `expected` qualname found within the first
/// `top_n` entries of `ranked` (1-indexed: the first position scores `1.0`,
/// the second `0.5`, ...); `0.0` if none of `expected` appears in that
/// window. Callers use `top_n = 10` (the binding "MRR ... in top-10" rule).
pub fn reciprocal_rank(ranked: &[String], expected: &[String], top_n: usize) -> f64 {
    let expected: HashSet<&str> = expected.iter().map(String::as_str).collect();
    ranked
        .iter()
        .take(top_n)
        .position(|q| expected.contains(q.as_str()))
        .map(|idx| 1.0 / (idx as f64 + 1.0))
        .unwrap_or(0.0)
}

/// DCG over the first 10 gains, 1-indexed rank discount: `gain / log2(rank +
/// 1)`, i.e. `gain / log2(i + 2)` for a 0-indexed position `i`.
fn dcg_at_10(gains: impl Iterator<Item = f64>) -> f64 {
    gains
        .take(10)
        .enumerate()
        .map(|(i, gain)| gain / (i as f64 + 2.0).log2())
        .sum()
}

/// nDCG@10 for one graded `search` query: DCG of `ranked`'s first 10 entries
/// (gain = `graded[qualname]`, `0` for any ranked qualname absent from
/// `graded`) divided by the ideal DCG (the same `graded` gains sorted
/// descending, i.e. the best achievable ordering). `0.0` when `graded` is
/// empty or every gain in it is `0` (nothing to rank against — avoids a `0/0`
/// division).
///
/// Callers must only invoke this for queries that actually carry a non-empty
/// `graded` map (the "ONLY queries with `graded`" rule) — that filter lives
/// at the call site in `corpus.rs`, not here, since this function has no way
/// to distinguish "ungraded query" from "graded query where nothing ranked".
pub fn ndcg_at_10(ranked: &[String], graded: &HashMap<String, u8>) -> f64 {
    let dcg = dcg_at_10(
        ranked
            .iter()
            .map(|q| graded.get(q.as_str()).copied().unwrap_or(0) as f64),
    );
    let mut ideal: Vec<f64> = graded.values().map(|&g| g as f64).collect();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let idcg = dcg_at_10(ideal.into_iter());
    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

/// One `explore` query's pass/fail: `true` only if EVERY `expect` qualname's
/// first source chunk appears verbatim (substring) in `bundle_text`.
/// `first_chunk_content(qualname)` must return `None` when the qualname
/// doesn't resolve or resolves to a symbol with zero owned chunks — either
/// way that counts as "not found" for that symbol (failing the whole query),
/// never a vacuous pass. `false` when `expect` is empty (shouldn't happen —
/// same corpora-validation guarantee as `recall_at_k`).
pub fn answer_in_bundle(
    bundle_text: &str,
    expect: &[String],
    mut first_chunk_content: impl FnMut(&str) -> Option<String>,
) -> bool {
    if expect.is_empty() {
        return false;
    }
    expect
        .iter()
        .all(|qualname| first_chunk_content(qualname).is_some_and(|c| bundle_text.contains(&c)))
}

/// One labeled ground-truth call edge (`eval/edges/{repo}.yaml` row) — just
/// the two qualnames; `resolved` vs `heuristic` (the yaml's `expected` field)
/// doesn't change how a pair is scored (see `edge_counts`'s doc comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledEdge {
    pub caller: String,
    pub callee: String,
}

/// Raw counts backing `edge_recall`/`edge_precision` — see the module doc's
/// aggregation note for why these (not the two ratios) are what pools across
/// corpora into "overall".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EdgeCounts {
    /// Labeled pairs whose callee was found in its caller's depth-1 resolved
    /// callee set.
    pub found: usize,
    /// Total labeled pairs considered.
    pub labeled: usize,
    /// Size of the deduped set of (caller, callee) edges actually returned
    /// by the depth-1 callee traversal, across every distinct caller in the
    /// labeled set.
    pub returned: usize,
}

impl EdgeCounts {
    pub fn combine(self, other: Self) -> Self {
        Self {
            found: self.found + other.found,
            labeled: self.labeled + other.labeled,
            returned: self.returned + other.returned,
        }
    }

    /// `found / labeled`; `0.0` on an empty labeled set (never happens for
    /// real ground truth — `eval/edges/*.yaml` requires >= 40 rows per
    /// corpus, enforced by validation — but avoids a `0/0` division).
    pub fn recall(&self) -> f64 {
        if self.labeled == 0 {
            0.0
        } else {
            self.found as f64 / self.labeled as f64
        }
    }

    /// `found / returned`; `0.0` when nothing was ever returned (every
    /// labeled caller resolved to zero depth-1 callees at all).
    pub fn precision(&self) -> f64 {
        if self.returned == 0 {
            0.0
        } else {
            self.found as f64 / self.returned as f64
        }
    }
}

/// Computes [`EdgeCounts`] for one corpus's labeled ground truth.
///
/// For each labeled pair, `depth1_callees(caller)` must return that caller's
/// full depth-1 RESOLVED callee qualname set — literally the same list
/// `store.callees_of(caller_id, 1, ..)` produces once filtered to resolved
/// rows (`symbol.id != -1`), i.e. exactly what the `callees` MCP tool's
/// depth-1 output names for that symbol. It's a closure (not a `Store`
/// reference) so this function stays storage-agnostic and directly
/// unit-testable with a fake graph.
///
/// A pair is "found" when its `callee` is a member of its `caller`'s
/// depth-1 set — this holds regardless of whether the yaml labeled it
/// `resolved` or `heuristic` (see `eval/edges/*.yaml`'s header comment: a
/// `heuristic` pair is real, honest ground truth that vexus's name+arity
/// resolver may or may not land on; a corpus with several such pairs is
/// EXPECTED to score `recall`/`precision` below `1.0`, not a bug in this
/// function). `caller` is queried at most once regardless of how many
/// labeled pairs share it — repeat callers reuse the first call's result
/// (`depth1_callees` is expected to be side-effect-free, e.g. a bare `Store`
/// read; this dedupe exists to keep the underlying `Store` traversal down
/// to one query per distinct caller, not literal correctness).
///
/// `returned` is a deduped set of `(caller, callee)` pairs pooled across
/// every distinct caller's depth-1 result: the total edges returned across
/// those depth-1 queries, deduped.
pub fn edge_counts(
    labeled: &[LabeledEdge],
    mut depth1_callees: impl FnMut(&str) -> Vec<String>,
) -> EdgeCounts {
    let mut cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut returned: HashSet<(String, String)> = HashSet::new();
    let mut found = 0usize;

    for edge in labeled {
        let callees = cache
            .entry(edge.caller.clone())
            .or_insert_with(|| depth1_callees(&edge.caller));
        for callee in callees.iter() {
            returned.insert((edge.caller.clone(), callee.clone()));
        }
        if callees.iter().any(|c| c == &edge.callee) {
            found += 1;
        }
    }

    EdgeCounts {
        found,
        labeled: labeled.len(),
        returned: returned.len(),
    }
}

/// The exact seven metrics reported: `recall@5`, `recall@10`, `mrr`,
/// `ndcg@10` (search); `answer_in_bundle` (explore); `edge_precision`,
/// `edge_recall` (callers/callees vs labeled ground truth). Each is
/// already rounded to 4 decimal
/// places via [`round4`] — the shape written to `eval/last-run.json`, both
/// per corpus and for "overall". Field names are renamed on serialization to
/// match the literal metric names (`@` is a valid JSON string-key
/// character).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetricSet {
    #[serde(rename = "recall@5")]
    pub recall_at_5: f64,
    #[serde(rename = "recall@10")]
    pub recall_at_10: f64,
    pub mrr: f64,
    #[serde(rename = "ndcg@10")]
    pub ndcg_at_10: f64,
    pub answer_in_bundle: f64,
    pub edge_precision: f64,
    pub edge_recall: f64,
}

/// Rounds to 4 decimal places (half away from zero) — the "all floats
/// 0..1, 4 decimal places in JSON" rule. Applied to
/// every metric value right before it's placed in the JSON-serializable
/// report struct; `serde_json` then renders it via its normal (shortest
/// round-trip) float formatting, so e.g. `1.0` prints as `1.0`, not
/// `1.0000` — JSON numbers don't carry fixed-width formatting, only the
/// rounding itself is the guarantee this function makes.
pub fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ---- Accum ----------------------------------------------------------

    #[test]
    fn accum_mean_is_zero_when_nothing_pushed() {
        assert_eq!(Accum::default().mean(), 0.0);
    }

    #[test]
    fn accum_mean_and_combine() {
        let mut a = Accum::default();
        a.push(1.0);
        a.push(0.0);
        a.push(0.5);
        assert_eq!(a.mean(), 0.5); // (1 + 0 + 0.5) / 3

        let mut b = Accum::default();
        b.push(1.0);
        assert_eq!(b.mean(), 1.0);

        // Pooled mean over all 4 pushes (1, 0, 0.5, 1) = 2.5 / 4 = 0.625 —
        // NOT mean(0.5, 1.0) = 0.75. This is the property that makes
        // "overall" a pooled figure, not a mean of per-corpus means.
        let combined = a.combine(b);
        assert_eq!(combined.count, 4);
        assert_eq!(combined.mean(), 0.625);
    }

    // ---- recall_at_k -----------------------------------------------------

    #[test]
    fn recall_at_k_hand_computed() {
        // expected has 4 qualnames; top-5 window contains 2 of them (b, d).
        let ranked = strings(&["a", "b", "z", "d", "y", "c"]);
        let expected = strings(&["b", "c", "d", "missing"]);
        assert_eq!(recall_at_k(&ranked, &expected, 5), 0.5); // 2/4
                                                             // top-6 (whole list) also picks up c -> 3/4
        assert_eq!(recall_at_k(&ranked, &expected, 6), 0.75);
    }

    #[test]
    fn recall_at_k_perfect_and_zero() {
        let ranked = strings(&["a", "b"]);
        assert_eq!(recall_at_k(&ranked, &strings(&["a", "b"]), 5), 1.0);
        assert_eq!(recall_at_k(&ranked, &strings(&["x", "y"]), 5), 0.0);
    }

    #[test]
    fn recall_at_k_empty_expected_is_zero_not_a_panic() {
        assert_eq!(recall_at_k(&strings(&["a"]), &[], 5), 0.0);
    }

    #[test]
    fn recall_at_k_duplicate_ranked_entries_do_not_inflate_the_denominator() {
        // Same qualname surfacing via two chunks must not change the result
        // — the top-k set dedupes naturally.
        let ranked = strings(&["a", "a", "a"]);
        assert_eq!(recall_at_k(&ranked, &strings(&["a"]), 5), 1.0);
    }

    // ---- reciprocal_rank ---------------------------------------------------

    #[test]
    fn reciprocal_rank_hand_computed() {
        let ranked = strings(&["x", "y", "target", "z"]);
        // target at 0-indexed position 2 -> 1-indexed rank 3 -> 1/3.
        assert_eq!(
            reciprocal_rank(&ranked, &strings(&["target"]), 10),
            1.0 / 3.0
        );
    }

    #[test]
    fn reciprocal_rank_first_position_is_one() {
        let ranked = strings(&["target", "y"]);
        assert_eq!(reciprocal_rank(&ranked, &strings(&["target"]), 10), 1.0);
    }

    #[test]
    fn reciprocal_rank_uses_the_first_relevant_hit_not_the_best_one() {
        // Two expected qualnames both present; MRR uses whichever ranks
        // first, not the "best" (there's no notion of best — first is first).
        let ranked = strings(&["a", "expected_two", "expected_one"]);
        let expected = strings(&["expected_one", "expected_two"]);
        assert_eq!(reciprocal_rank(&ranked, &expected, 10), 1.0 / 2.0);
    }

    #[test]
    fn reciprocal_rank_zero_when_absent_or_outside_top_n() {
        let ranked = strings(&["a", "b", "target"]);
        assert_eq!(reciprocal_rank(&ranked, &strings(&["missing"]), 10), 0.0);
        // target is at rank 3, outside a top_n of 2.
        assert_eq!(reciprocal_rank(&ranked, &strings(&["target"]), 2), 0.0);
    }

    // ---- ndcg_at_10 --------------------------------------------------------
    //
    // Reference values below were computed independently in Python
    // (`python3 -c "import math; ..."`) at full precision, then hardcoded
    // here — the same cross-check discipline `vexus-mcp`'s
    // `epoch_to_rfc3339` test uses (comparing against `date -u`), since
    // log2-based DCG doesn't reduce to clean decimals by hand.

    #[test]
    fn ndcg_at_10_perfect_ranking_is_one() {
        // Ideal ordering (gains descending) is itself the ranking -> nDCG = 1.
        let ranked = strings(&["a", "b", "c"]);
        let graded: HashMap<String, u8> =
            [("a".into(), 3), ("b".into(), 2), ("c".into(), 1)].into();
        assert!((ndcg_at_10(&ranked, &graded) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn ndcg_at_10_hand_computed_with_irrelevant_and_reordered_items() {
        // graded: a=3, b=2, c=1. Ranked puts an ungraded item first, then
        // b, then a, then c — reordered relative to ideal.
        //   DCG  = 0/log2(2) + 2/log2(3) + 3/log2(4) + 1/log2(5)
        //   IDCG = 3/log2(2) + 2/log2(3) + 1/log2(4)      (ideal order a,b,c)
        // Cross-checked independently via
        // `python3 -c "import math; dcg = 0/math.log2(2) + 2/math.log2(3) +
        // 3/math.log2(4) + 1/math.log2(5); idcg = 3/math.log2(2) +
        // 2/math.log2(3) + 1/math.log2(4); print(dcg, idcg, dcg/idcg)"`
        // -> dcg=3.192536065216308, idcg=4.7618595071429155,
        //    ndcg=0.6704389452119323 — the same cross-check discipline
        // `vexus-mcp`'s `epoch_to_rfc3339` test uses against `date -u`,
        // since log2-based DCG doesn't reduce to clean decimals by hand.
        let ranked = strings(&["ungraded", "b", "a", "c"]);
        let graded: HashMap<String, u8> =
            [("a".into(), 3), ("b".into(), 2), ("c".into(), 1)].into();
        let got = ndcg_at_10(&ranked, &graded);
        assert!((got - 0.670_438_945_211_932_3).abs() < 1e-9, "got {got}");
    }

    #[test]
    fn ndcg_at_10_only_considers_the_first_10_ranked_entries() {
        // The one graded, relevant item sits at rank 11 — entirely outside
        // the @10 window — so DCG must be 0 regardless of IDCG.
        let mut ranked = vec!["pad".to_string(); 10];
        ranked.push("a".to_string());
        let graded: HashMap<String, u8> = [("a".into(), 3)].into();
        assert_eq!(ndcg_at_10(&ranked, &graded), 0.0);
    }

    #[test]
    fn ndcg_at_10_zero_when_graded_empty() {
        assert_eq!(ndcg_at_10(&strings(&["a"]), &HashMap::new()), 0.0);
    }

    #[test]
    fn ndcg_at_10_zero_when_nothing_ranked_matches_any_grade() {
        let ranked = strings(&["x", "y"]);
        let graded: HashMap<String, u8> = [("a".into(), 3)].into();
        assert_eq!(ndcg_at_10(&ranked, &graded), 0.0);
    }

    // ---- answer_in_bundle ---------------------------------------------------

    #[test]
    fn answer_in_bundle_true_when_every_expected_symbols_content_is_present() {
        let bundle = "explore: \"q\"\n\ndef foo():\n    pass\n\n\ndef bar():\n    return 1\n";
        let expect = strings(&["m.foo", "m.bar"]);
        let contents: HashMap<&str, &str> = [
            ("m.foo", "def foo():\n    pass\n"),
            ("m.bar", "def bar():\n    return 1\n"),
        ]
        .into();
        assert!(answer_in_bundle(bundle, &expect, |q| contents
            .get(q)
            .map(|s| s.to_string())));
    }

    #[test]
    fn answer_in_bundle_false_when_one_expected_symbol_is_missing_from_the_bundle() {
        let bundle = "def foo():\n    pass\n";
        let expect = strings(&["m.foo", "m.bar"]);
        let contents: HashMap<&str, &str> = [
            ("m.foo", "def foo():\n    pass\n"),
            ("m.bar", "def bar():\n    pass\n"),
        ]
        .into();
        assert!(!answer_in_bundle(bundle, &expect, |q| contents
            .get(q)
            .map(|s| s.to_string())));
    }

    #[test]
    fn answer_in_bundle_false_when_a_symbol_does_not_resolve() {
        let bundle = "def foo():\n    pass\n";
        let expect = strings(&["m.foo", "m.unresolvable"]);
        assert!(!answer_in_bundle(bundle, &expect, |q| {
            (q == "m.foo").then(|| "def foo():\n    pass\n".to_string())
        }));
    }

    #[test]
    fn answer_in_bundle_false_when_expect_is_empty() {
        assert!(!answer_in_bundle("anything", &[], |_| None));
    }

    // ---- edge_counts / EdgeCounts -------------------------------------------

    #[test]
    fn edge_counts_hand_computed_distinguishes_recall_from_precision() {
        // caller "a" really calls x, w, v (3 distinct depth-1 callees); the
        // labeled set claims a->x (true) and a->q (false, q isn't one of
        // a's callees at all).
        let labeled = vec![
            LabeledEdge {
                caller: "a".into(),
                callee: "x".into(),
            },
            LabeledEdge {
                caller: "a".into(),
                callee: "q".into(),
            },
        ];
        let counts = edge_counts(&labeled, |caller| {
            assert_eq!(caller, "a");
            strings(&["x", "w", "v"])
        });
        assert_eq!(counts.found, 1); // only a->x
        assert_eq!(counts.labeled, 2);
        assert_eq!(counts.returned, 3); // {(a,x), (a,w), (a,v)}
        assert_eq!(counts.recall(), 0.5); // 1/2
        assert_eq!(counts.precision(), 1.0 / 3.0); // 1/3 — distinct from recall
    }

    #[test]
    fn edge_counts_queries_each_distinct_caller_at_most_once() {
        use std::cell::Cell;
        let labeled = vec![
            LabeledEdge {
                caller: "a".into(),
                callee: "x".into(),
            },
            LabeledEdge {
                caller: "a".into(),
                callee: "z".into(),
            },
            LabeledEdge {
                caller: "b".into(),
                callee: "y".into(),
            },
        ];
        let calls = Cell::new(0u32);
        let counts = edge_counts(&labeled, |caller| {
            calls.set(calls.get() + 1);
            match caller {
                "a" => strings(&["x"]),
                "b" => strings(&["y"]),
                other => panic!("unexpected caller {other}"),
            }
        });
        assert_eq!(calls.get(), 2, "one call per distinct caller, not per row");
        assert_eq!(counts.found, 2); // a->x, b->y both found
        assert_eq!(counts.labeled, 3);
        assert_eq!(counts.returned, 2); // {(a,x), (b,y)}
    }

    #[test]
    fn edge_counts_combine_pools_raw_counts_not_ratios() {
        let pyapp = EdgeCounts {
            found: 8,
            labeled: 10,
            returned: 12,
        };
        let polyglot = EdgeCounts {
            found: 1,
            labeled: 10,
            returned: 1,
        };
        // Per-corpus recalls are 0.8 and 0.1 — a mean-of-means would give
        // 0.45. Pooled (the required behavior): (8+1)/(10+10) = 0.45 too by
        // coincidence of these numbers being symmetric; use precision
        // (12 vs 1 returned) to actually distinguish pooling from averaging.
        let overall = pyapp.combine(polyglot);
        assert_eq!(overall.found, 9);
        assert_eq!(overall.labeled, 20);
        assert_eq!(overall.returned, 13);
        assert_eq!(overall.recall(), 0.45); // 9/20
        assert_eq!(overall.precision(), 9.0 / 13.0); // pooled, not mean(8/12, 1/1)
    }

    #[test]
    fn edge_counts_zero_labeled_and_zero_returned_do_not_panic() {
        let counts = EdgeCounts::default();
        assert_eq!(counts.recall(), 0.0);
        assert_eq!(counts.precision(), 0.0);
    }

    // ---- MetricSet ------------------------------------------------------------

    #[test]
    fn metric_set_serializes_with_the_plans_literal_metric_names() {
        let set = MetricSet {
            recall_at_5: 0.8,
            recall_at_10: 0.9,
            mrr: 0.75,
            ndcg_at_10: 0.6667,
            answer_in_bundle: 0.5,
            edge_precision: 0.95,
            edge_recall: 0.85,
        };
        let json = serde_json::to_value(set).unwrap();
        assert_eq!(json["recall@5"], 0.8);
        assert_eq!(json["recall@10"], 0.9);
        assert_eq!(json["mrr"], 0.75);
        assert_eq!(json["ndcg@10"], 0.6667);
        assert_eq!(json["answer_in_bundle"], 0.5);
        assert_eq!(json["edge_precision"], 0.95);
        assert_eq!(json["edge_recall"], 0.85);
    }

    // ---- round4 --------------------------------------------------------------

    #[test]
    fn round4_rounds_to_four_decimal_places() {
        assert_eq!(round4(0.123_449), 0.1234);
        assert_eq!(round4(0.123_45), 0.1235); // half-away-from-zero at the boundary
        assert_eq!(round4(1.0), 1.0);
        assert_eq!(round4(0.0), 0.0);
        assert_eq!(round4(2.0 / 3.0), 0.6667);
    }
}
