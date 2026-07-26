# `eval/` — retrieval-metric fixtures and ratchet baselines

This directory holds the hand-authored fixture corpora, query/edge ground
truth, and baseline metrics that `crates/vexus-eval` (the `vexus-eval`
binary) scores against. It exists so retrieval quality (search ranking,
explore's bundle assembly, call-graph resolution) has a **repeatable,
gated** measurement instead of only ad-hoc manual spot-checks.

## Layout

```text
eval/
  corpora/{pyapp,polyglot}/   hand-authored fixture repos (never real vendored code)
  queries/{pyapp,polyglot}.yaml   >=25 graded/ungraded queries per corpus
  edges/{pyapp,polyglot}.yaml     >=40 labeled caller/callee pairs per corpus
  baseline-mock.json          committed ratchet baseline (mock embedder)
  baseline-real.json          local/nightly baseline (real ONNX embedder) — not committed by CI
  last-run.json                the most recent `run`'s output (gitignored — regeneratable, not ground truth)
```

## Why the corpora are hand-authored, not vendored real repos

Plan 5 originally considered vendoring real open-source repos as fixtures.
We chose hand-authored fixtures instead:

- **No licensing/redistribution risk** — every file in `eval/corpora/` is
  original content written for this repo, so there's nothing to attribute,
  relicense, or accidentally violate by shipping it in this repository's
  history.
- **No size creep** — a vendored real repo pulls in its full history and
  file count; a ~25-30 file hand-authored corpus stays small and fast to
  index on every `check` run (this runs on every PR).
- **Full determinism and control over ground truth** — because we wrote
  every symbol, docstring, and call site, every `expect` qualname in
  `queries/*.yaml` and every labeled pair in `edges/*.yaml` is something we
  can verify by hand (see `crates/vexus-cli/tests/eval_corpora_validation.rs`,
  which asserts every expected qualname actually resolves). A vendored repo's
  "ground truth" would require guessing at what a real user actually wanted,
  which is a much weaker basis for a gate that blocks PRs.

The tradeoff: hand-authored fixtures are smaller and less stylistically
diverse than a large real-world codebase, so they're a proxy for retrieval
quality, not a guarantee real-world quality never regresses. The **real**-
embedder run (`--real`, see below) partially offsets this by testing actual
semantic search quality rather than just structural/keyword behavior, but
still only ever exercises these same two fixture repos.

## The `vexus-eval` CLI

All commands run from the repo root (or anywhere in the workspace — every
path is resolved from `CARGO_MANIFEST_DIR`, not the process's current
directory):

```bash
# Score every corpus under eval/corpora/, print a table, write eval/last-run.json.
cargo run -p vexus-eval -- run [--real] [--corpus NAME]

# Compare the current run against the committed baseline; exit 1 (naming
# every regressed metric) if anything dropped more than 0.02 absolute.
cargo run -p vexus-eval -- check [--real]

# Overwrite the baseline with the current run's metrics. Only ever do this
# deliberately (a real quality improvement, or a corpus/metric-definition
# change) — never just to "make check pass" after an unreviewed regression.
cargo run -p vexus-eval -- bless [--real]
```

### The ratchet gate

`check` (no flags) always uses the deterministic **mock** embedder and
compares against the committed `eval/baseline-mock.json`. This is what CI's
`eval-gate` job runs on every PR (`VEXUS_EMBEDDER=mock`) — mock mode has
zero semantic signal, so it's a pure regression detector for the
*structural* pipeline (chunking, keyword/FTS ranking, RRF fusion, call-graph
resolution), not a measure of real search quality.

Rule (see the Plan 5 Global Constraints): **any** of the 7 metrics
(`recall@5`, `recall@10`, `mrr`, `ndcg@10`, `answer_in_bundle`,
`edge_precision`, `edge_recall`), in any corpus or in the pooled "overall"
row, dropping by **more than 0.02 absolute** against the baseline fails the
gate (`exit 1`, every regressed metric named with its before/after values).
Improvements are printed too, but never fail the gate. A missing baseline
file fails with a message telling you to run `bless` first (and to commit
the result) rather than a generic file-not-found error.

### Pre-release: `--real --check`

The mock gate never touches a downloaded model and never runs against real
semantic search — by design, so PR CI stays fast and hermetic. Before
cutting a release (or any time you want to check real embedding quality
hasn't regressed), run the real-embedder equivalent locally:

```bash
# First time only: requires a model already downloaded under
# ~/.vexus/models (run `vexus index` once against any repo to fetch it).
cargo run -p vexus-eval -- bless --real   # creates + commits eval/baseline-real.json

# Every pre-release after that:
cargo run -p vexus-eval -- check --real
```

This is never run in PR CI (no model downloads in PR CI) — it's a manual
(or nightly-scheduled, in a later task) pre-release gate only.
