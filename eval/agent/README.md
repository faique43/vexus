# Agent-in-the-loop eval

Every other measurement in `eval/` tests the index. This one tests the
*steering*: given a real question and both options available, does an agent
reach for a vexus tool or fall back to grepping?

That question can't be unit-tested. It depends on a model's judgment, which
depends on the tool descriptions and the server instructions — so the only
honest way to measure it is to run real sessions and count what the agent
actually did.

## Running it

```sh
cargo build --release -p vexus-cli
VEXUS_AGENT_MODEL=claude-sonnet-4-5 bash eval/agent/run.sh
```

Requirements: the `claude` CLI on `PATH`, a built binary, and working auth —
**either** an `ANTHROPIC_API_KEY` **or** a `claude` CLI that is already
logged in (subscription auth works; no separate API key purchase needed).
The script says which one it is using, and aborts on the first failed
session rather than reporting a summary of zeros that would read as a
steering result.

It copies a fixture corpus to a temp directory, indexes it, writes an
`.mcp.json` pointing at `vexus serve`, and runs each task as a separate
`claude -p` session with a JSON transcript.

| variable | effect |
| --- | --- |
| `VEXUS_AGENT_MODEL` | Pins the model. **Set it** — an unpinned run uses whatever the CLI defaults to that day, so its numbers aren't comparable to a previous one. Recorded in `summary.txt` either way. |
| `VEXUS_AGENT_CORPUS` | Points at a different repo. The default fixture is 30 files; a few thousand is where tool choice actually gets interesting, so pointing this at a real checkout is the more realistic run. |
| `VEXUS_BINARY` | Uses a different `vexus` build. |

Results land in `eval/agent/results/` (gitignored): one transcript per task
plus a `summary.txt` carrying the model, date, per-task counts, the vexus
share, and the total session cost. Cost comes from each transcript's own
`total_cost_usd`; a CLI that doesn't emit it reports `?` rather than a
fabricated zero.

Expect a handful of cents per run on the default corpus.

## Reading the result

The number that matters is the **vexus share** — vexus tool calls as a
fraction of all code-navigation calls (vexus + Grep/Glob/Read):

- **High share**: the descriptions are doing their job.
- **Low share with good answers**: the agent found the code by grepping
  anyway. The index isn't wrong, it's unpersuasive — that's a tool-description
  problem, and the fix is in `crates/vexus-mcp/src/server.rs`, not the indexer.
- **Low share with bad answers**: check `status` and the retrieval metrics
  first; the agent may have tried vexus, got a poor result, and correctly
  stopped trusting it.

A share below 100% is not automatically a failure. The tool descriptions
deliberately tell agents to keep using grep for exact strings, comments, and
config values — an agent grepping for a literal string is following the
instructions, not ignoring them.

## Why it isn't a gate

It costs real API tokens and its output varies between runs, so it is not in
PR CI. The nightly workflow runs it only when an `ANTHROPIC_API_KEY` secret
is configured; without one the job skips, which is the expected state for
forks. Treat it as a trend to watch after changing tool descriptions or the
server instructions, not a pass/fail check.
