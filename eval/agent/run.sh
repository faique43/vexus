#!/usr/bin/env bash
# Agent-in-the-loop eval: does an agent with vexus available actually use it?
#
# Everything else in eval/ measures the index. This measures the steering —
# the one thing no unit test can, because it depends on a model's judgment
# about which tool to reach for. It answers two questions per task:
#
#   1. Did the agent use vexus at all, or fall back to Grep/Read?
#   2. How many tokens did the session cost?
#
# It is deliberately a shell script over `claude -p` rather than a Rust
# harness: the thing under test is a real agent session, so the closer this
# is to how someone actually runs one, the more the result means.
#
# Usage:  ANTHROPIC_API_KEY=... bash eval/agent/run.sh
# Output: eval/agent/results/{task-N.json,summary.txt}
#
# Costs real API tokens. Nothing in CI runs it without an explicit key.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
corpus="${VEXUS_AGENT_CORPUS:-$repo_root/eval/corpora/pyapp}"
results="$repo_root/eval/agent/results"
binary="${VEXUS_BINARY:-$repo_root/target/release/vexus}"

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ANTHROPIC_API_KEY is not set — this harness makes real API calls." >&2
  exit 1
fi
if ! command -v claude >/dev/null 2>&1; then
  echo "the 'claude' CLI is not on PATH — install Claude Code to run this." >&2
  exit 1
fi
if [ ! -x "$binary" ]; then
  echo "no vexus binary at $binary — run: cargo build --release -p vexus-cli" >&2
  exit 1
fi

# The tasks are phrased the way a person asks, not the way a tool expects —
# picking the right tool is precisely what's being measured.
tasks=(
  "How does an invoice get created, end to end?"
  "What would break if I changed charge_card?"
  "Where are repeated login attempts throttled?"
  "Explain what the retry helper does and who calls it."
)

mkdir -p "$results"
: > "$results/summary.txt"

# Work on a copy: the agent may write, and the corpus is committed fixture
# data that the retrieval metrics depend on being byte-stable.
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
cp -R "$corpus" "$workdir/repo"

"$binary" index "$workdir/repo" >/dev/null

cat > "$workdir/repo/.mcp.json" <<EOF
{ "mcpServers": { "vexus": { "command": "$binary", "args": ["serve", "$workdir/repo"] } } }
EOF

vexus_total=0
grep_total=0

for i in "${!tasks[@]}"; do
  task="${tasks[$i]}"
  echo "task $((i + 1)): $task"
  out="$results/task-$((i + 1)).json"

  # --output-format json gives a machine-readable transcript including the
  # tool calls, which is the whole measurement.
  # --allowedTools is not optional: `claude -p` won't auto-approve a project
  # .mcp.json server, and Grep/Read are permitted more readily than MCP tools.
  # Without it the harness would report "the agent didn't use vexus" for a
  # permissions reason and read as a steering failure — a biased measurement
  # is worse than none.
  (cd "$workdir/repo" && claude -p "$task" \
      --output-format json \
      --mcp-config .mcp.json \
      --allowedTools "mcp__vexus__explore,mcp__vexus__search,mcp__vexus__open,mcp__vexus__callers,mcp__vexus__callees,mcp__vexus__impact,mcp__vexus__status,Grep,Glob,Read" \
      > "$out") || {
    echo "  session failed; see $out" >&2
    continue
  }

  # Count tool invocations by name. Counting from the transcript rather than
  # asking the model what it did keeps the measurement honest.
  # `|| true` on each: under `set -o pipefail` a grep with no matches exits 1
  # and would abort the harness — and "no vexus calls" is exactly the result
  # worth recording, not crashing on.
  vexus_calls=$( { grep -o '"name":"mcp__vexus__[a-z]*"' "$out" || true; } | wc -l | tr -d ' ')
  grep_calls=$( { grep -oE '"name":"(Grep|Glob|Read)"' "$out" || true; } | wc -l | tr -d ' ')
  vexus_total=$((vexus_total + vexus_calls))
  grep_total=$((grep_total + grep_calls))

  printf 'task %d: vexus=%s grep/read=%s :: %s\n' \
    "$((i + 1))" "$vexus_calls" "$grep_calls" "$task" >> "$results/summary.txt"
done

{
  echo
  echo "totals: vexus=$vexus_total grep/read=$grep_total"
  if [ "$((vexus_total + grep_total))" -gt 0 ]; then
    printf 'vexus share: %d%%\n' \
      $(( vexus_total * 100 / (vexus_total + grep_total) ))
  fi
} >> "$results/summary.txt"

cat "$results/summary.txt"
