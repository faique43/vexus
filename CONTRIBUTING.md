# Contributing to vexus

Thanks for looking. vexus is young and the gaps are real, so bug reports that
say "this returned garbage for my repo" are as useful as patches.

## Before you start

- **Bugs and small fixes:** open a PR directly.
- **New languages, new tools, schema changes:** open an issue first. These
  touch the index format or the MCP surface, and it's cheaper to agree on the
  shape before you write it.

## Setup

Rust stable is the only requirement.

```sh
git clone https://github.com/faique43/vexus
cd vexus
cargo build
```

You do **not** need the embedding model to develop. Every test runs against a
deterministic mock embedder, so nothing downloads a model and results are
reproducible:

```sh
export VEXUS_EMBEDDER=mock
```

`VEXUS_EMBEDDER` accepts `mock`, `none`, or `onnx` (the default, real model).

## The checks CI runs

Run these before pushing. They are the same three jobs that gate a PR:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace                          # ~281 tests
cargo run --release -p vexus-eval -- check      # retrieval-metric ratchet
```

Clippy runs with `-D warnings`, so a warning fails the build.

## The eval gate

`vexus-eval check` scores retrieval against hand-authored fixture corpora in
[`eval/`](eval/) and compares the result to a committed baseline. **Any of the
seven metrics dropping more than 0.02 absolute fails the build.** This is
deliberate: retrieval quality is easy to regress silently, and a passing test
suite says nothing about whether search still finds the right code.

If your change moves a metric legitimately, say so in the PR and explain why.
Re-blessing the baseline is a reviewable decision, not a formality:

```sh
cargo run --release -p vexus-eval -- run        # writes eval/last-run.json
# then copy it over eval/baseline-mock.json, and justify it in the PR
```

If you add fixture code or queries, `eval_corpora_validation.rs` enforces that
every qualname in the YAML resolves to a real, unambiguous symbol.

## A warning about the perf harness

`cargo run --release -p vexus-eval -- perf` hardcodes the **mock** embedder. Its
numbers describe a run that never embeds anything, so they are useful for
catching algorithmic regressions and useless as user-facing performance. Do not
quote them in docs. Real-model figures belong in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)
and the README, measured with the real model.

Timing is advisory in CI, never a merge blocker: runner variance makes it a
poor gate.

## Adding a language

This is the most contributor-friendly change in the repo, and it needs no
parser code:

1. Add the tree-sitter grammar to `crates/vexus-index/Cargo.toml`.
2. Write a `.scm` query file next to the existing ones in
   `crates/vexus-index/queries/`. The parser is driven entirely by capture
   names, so match the ones the existing languages use.
3. Register the language in the registry alongside Python, TypeScript and Rust.
4. Add a fixture under `eval/corpora/` and a few graded queries so the new
   language is actually measured, not just parsed.

## Platform reality

- **Supported:** Linux (x64, arm64, glibc 2.39+), macOS on Apple Silicon,
  Windows (x64, arm64).
  The glibc floor is set by the prebuilt ONNX Runtime `ort` downloads, not by
  our runner choice: it is compiled against glibc 2.38 and references
  `__isoc23_strtol`, so building on ubuntu-22.04 fails at link time with
  `undefined symbol: __isoc23_strtol` instead of producing a more portable
  binary. ubuntu-24.04 is the oldest image that links, which is where 2.39
  comes from. A step in the release workflow fails if the floor drifts above
  2.39, so a newer runner image cannot raise it unnoticed.
- **Windows (x64, arm64):** supported. The writer lock is
  `std::fs::File::try_lock` (LockFileEx there), CI runs the full suite on
  `windows-latest`, and releases ship `.zip` archives installed via
  `install.ps1`.
- **Intel macOS, glibc < 2.39, musl:** served by the *structural-only*
  build (`cargo build -p vexus-cli --no-default-features`) — the ONNX
  runtime is compiled out, so no semantic search, but keyword+graph search
  work fully. `install.sh` detects these hosts and installs the
  `-structural` artifact; CI keeps the shape compiling (`structural` job).
  Full semantic support there still means vendoring an ONNX Runtime build
  or switching backends — open for contribution.

## Commits and PRs

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org),
with a crate scope where it helps:

```
fix(core): dedupe callees_of's CTE by node instead of path
feat(mcp): callers, callees, impact tools
docs: report real-model speed, not the mock harness's numbers
```

Explain *why* in the body, not just what. The diff already says what.

`main` is protected: every change lands through a pull request with the three
gating checks green. Keep PRs focused on one thing.

## Reporting bugs

Include the output of `vexus status .` and your OS and architecture. If it's a
retrieval complaint ("it didn't find X"), the question you asked and the file
you expected back are the two things that make it reproducible.
