<!--
Thanks for sending this. Keep the PR focused on one thing; a small diff with a
clear reason gets merged faster than a large one that needs unpicking.
-->

## What this changes

<!-- One or two sentences. The diff says what; say why. -->

## Why

<!-- What was broken, missing, or wrong. Link an issue if there is one. -->

Closes #

## How you tested it

<!--
Beyond the CI gates. If it's a retrieval or graph change, say which queries or
fixtures you checked by hand.
-->

## Checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo run --release -p vexus-eval -- check` passes

## If a metric moved

<!--
Delete this section if the eval gate passed untouched.

The gate fails on any of the seven retrieval metrics dropping more than 0.02
absolute. If you re-blessed eval/baseline-mock.json, explain why the new
numbers are correct rather than a regression. This is a reviewable decision.
-->

## If you touched performance

<!--
Delete if not applicable.

Note that `vexus-eval perf` runs against the mock embedder, so its numbers are
useful for catching algorithmic regressions and not for user-facing claims.
Any figure that ends up in the README or docs/BENCHMARKS.md must be measured
with the real model.
-->
