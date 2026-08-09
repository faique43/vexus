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
# Usage:  bash eval/agent/run.sh
#   Auth: an ANTHROPIC_API_KEY env var, or a `claude` CLI already logged in
#   (subscription auth) — either works; without both, sessions fail fast.
#   Model: set VEXUS_AGENT_MODEL to pin one (recorded in the summary either
#   way — unpinned runs aren't comparable across time).
# Output: eval/agent/results/{task-N.jsonl,summary.txt}
#
# Costs real API tokens or subscription usage. Nothing in CI runs it
# without an explicit key.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
corpus="${VEXUS_AGENT_CORPUS:-$repo_root/eval/corpora/pyapp}"
results="$repo_root/eval/agent/results"
binary="${VEXUS_BINARY:-$repo_root/target/release/vexus}"
model="${VEXUS_AGENT_MODEL:-}"

if ! command -v claude >/dev/null 2>&1; then
  echo "the 'claude' CLI is not on PATH — install Claude Code to run this." >&2
  exit 1
fi
if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  # A logged-in CLI (subscription auth) works too — announce which auth is
  # in play so a failed run reads correctly, and fail fast on the first
  # session rather than pretending "0 vexus calls" was a steering result.
  echo "note: ANTHROPIC_API_KEY not set — relying on the claude CLI's own login." >&2
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
{
  echo "model: ${model:-cli default (unpinned — set VEXUS_AGENT_MODEL for comparable runs)}"
  echo "date:  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
} > "$results/summary.txt"

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
cost_total="0"
cost_known=1

for i in "${!tasks[@]}"; do
  task="${tasks[$i]}"
  echo "task $((i + 1)): $task"
  out="$results/task-$((i + 1)).jsonl"
  model_args=()
  if [ -n "$model" ]; then
    model_args=(--model "$model")
  fi

  # --output-format stream-json (with --verbose) emits every message as it
  # happens, including the assistant's tool_use blocks — that stream IS the
  # measurement. Plain `--output-format json` returns only the final result
  # object, with no record of which tools ran, so it cannot answer the
  # question this harness exists to ask.
  # --allowedTools is not optional: `claude -p` won't auto-approve a project
  # .mcp.json server, and Grep/Read are permitted more readily than MCP tools.
  # Without it the harness would report "the agent didn't use vexus" for a
  # permissions reason and read as a steering failure — a biased measurement
  # is worse than none.
  (cd "$workdir/repo" && claude -p "$task" \
      --output-format stream-json --verbose \
      --mcp-config .mcp.json \
      "${model_args[@]}" \
      --allowedTools "mcp__vexus__explore,mcp__vexus__search,mcp__vexus__open,mcp__vexus__callers,mcp__vexus__callees,mcp__vexus__impact,mcp__vexus__status,Grep,Glob,Read" \
      > "$out") || {
    echo "  session failed; see $out" >&2
    if [ "$i" -eq 0 ]; then
      # First session failing is almost always auth/setup, not steering —
      # aborting beats a summary full of zeros that reads as a result.
      echo "aborting: the first session failed (check auth: API key or 'claude' login)." >&2
      exit 1
    fi
    continue
  }

  # Count tool invocations by name. Counting from the transcript rather than
  # asking the model what it did keeps the measurement honest.
  # `|| true` on each: under `set -o pipefail` a grep with no matches exits 1
  # and would abort the harness — and "no vexus calls" is exactly the result
  # worth recording, not crashing on.
  # Count only names attached to a tool_use block, so a tool merely
  # *mentioned* in prose can't inflate the count.
  vexus_calls=$( { grep -o '"type":"tool_use"[^}]*"name":"mcp__vexus__[a-z]*"' "$out" || true; } | wc -l | tr -d ' ')
  grep_calls=$( { grep -oE '"type":"tool_use"[^}]*"name":"(Grep|Glob|Read)"' "$out" || true; } | wc -l | tr -d ' ')
  vexus_total=$((vexus_total + vexus_calls))
  grep_total=$((grep_total + grep_calls))

  # Session cost, from the transcript's own result object. Absent (older
  # CLI, different schema) reports as "?" rather than a fabricated 0 — the
  # header promises this number, so it must be real or visibly missing.
  cost=$( { grep -o '"total_cost_usd":[0-9.]*' "$out" | tail -1 | cut -d: -f2 || true; } )
  if [ -n "$cost" ]; then
    cost_total=$(awk -v a="$cost_total" -v b="$cost" 'BEGIN { printf "%.4f", a + b }')
    cost_label="\$$cost"
  else
    cost_known=0
    cost_label="?"
  fi

  printf 'task %d: vexus=%s grep/read=%s cost=%s :: %s\n' \
    "$((i + 1))" "$vexus_calls" "$grep_calls" "$cost_label" "$task" >> "$results/summary.txt"
done

{
  echo
  echo "totals: vexus=$vexus_total grep/read=$grep_total"
  if [ "$((vexus_total + grep_total))" -gt 0 ]; then
    printf 'vexus share: %d%%\n' \
      $(( vexus_total * 100 / (vexus_total + grep_total) ))
  fi
  if [ "$cost_known" -eq 1 ]; then
    echo "session cost: \$$cost_total"
  else
    echo "session cost: unavailable (no total_cost_usd in this CLI's transcript)"
  fi
} >> "$results/summary.txt"

cat "$results/summary.txt"
