#!/usr/bin/env bash
# Live cost comparison: the same questions, the same model, with and without
# vexus. run.sh asks whether an agent reaches for vexus; this asks whether
# doing so saved anything.
#
# `vexus-eval token-bench` can't answer that. Its grep side is a fixed
# transcript, and a real agent is far cheaper than a transcript: it greps a
# name, then reads a narrow line range. The first version of this comparison
# showed vexus costing twice what plain tools did on real repos, on the same
# day the synthetic table said 37x cheaper.
#
# Both arms run isolated from the user's own plugins, memory and MCP servers,
# which would otherwise skew the numbers. Skills stay off on both sides because
# the built-in skill list alone adds about 3,000 tokens per session.
#
# Usage:  VEXUS_AGENT_MODEL=claude-sonnet-5-5 bash eval/agent/compare.sh
#   VEXUS_AGENT_CORPUS   repo to test (default: eval/corpora/pyapp)
#   VEXUS_AGENT_TASKS    file with one question per line
#   VEXUS_BINARY         vexus build to test
#   VEXUS_COMPARE_STRICT set to 1 to exit non-zero if vexus used more context
# Output: eval/agent/results/compare/ and a table on stdout. Needs claude + jq.
#
# Costs real API tokens or subscription usage.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
corpus="${VEXUS_AGENT_CORPUS:-$repo_root/eval/corpora/pyapp}"
tasks_file="${VEXUS_AGENT_TASKS:-$repo_root/eval/agent/tasks-pyapp.txt}"
binary="${VEXUS_BINARY:-$repo_root/target/release/vexus}"
model="${VEXUS_AGENT_MODEL:?set VEXUS_AGENT_MODEL: an unpinned run is not comparable}"
results="$repo_root/eval/agent/results/compare"

for tool in claude jq; do
  command -v "$tool" >/dev/null 2>&1 || { echo "$tool is not on PATH" >&2; exit 1; }
done
[ -x "$binary" ] || { echo "no vexus binary at $binary" >&2; exit 1; }

mkdir -p "$results"
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
cp -R "$corpus" "$workdir/repo"
rm -rf "$workdir/repo/.vexus" "$workdir/repo/.mcp.json" "$workdir/repo/.claude"
"$binary" index "$workdir/repo" >/dev/null 2>&1
printf '{"mcpServers":{"vexus":{"command":"%s","args":["serve","."]}}}\n' "$binary" > "$workdir/vexus.json"

session() { # question, mcp config, transcript, extra args...
  local question="$1" config="$2" out="$3"
  shift 3
  (cd "$workdir/repo" && claude -p "$question" \
      --setting-sources project --strict-mcp-config --mcp-config "$config" \
      --disable-slash-commands --tools Read,Grep,Glob "$@" \
      --model "$model" --no-session-persistence --max-budget-usd 1 \
      --output-format stream-json --verbose < /dev/null > "$out" 2>/dev/null)
}

# Context growth, not total usage: every turn re-reads the prompt, so totals
# punish extra turns twice, and the two arms start from different tool lists.
measure() { # transcript -> "calls grew cost seconds"
  jq -rs '
    [.[] | select(.type == "assistant") | .message] as $turns
    | ($turns | map(.usage | .input_tokens + .cache_read_input_tokens + .cache_creation_input_tokens)) as $context
    | ($turns | map(.content[] | select(.type == "tool_use")) | length) as $calls
    | (.[] | select(.type == "result")) as $result
    | "\($calls) \($context[-1] - $context[0]) \($result.total_cost_usd) \($result.duration_ms / 1000 | floor)"
  ' "$1"
}

printf '%-52s %17s %17s %15s\n' question 'calls plain/vexus' 'context added' 'cost'
plain_total=0
vexus_total=0
n=0
while IFS= read -r question; do
  [ -n "$question" ] || continue
  n=$((n + 1))
  session "$question" '{"mcpServers":{}}' "$results/q$n-plain.jsonl"
  session "$question" "$workdir/vexus.json" "$results/q$n-vexus.jsonl" --allowedTools 'mcp__vexus__*'
  read -r plain_calls plain_grew plain_cost _ <<< "$(measure "$results/q$n-plain.jsonl")"
  read -r vexus_calls vexus_grew vexus_cost _ <<< "$(measure "$results/q$n-vexus.jsonl")"
  plain_total=$((plain_total + plain_grew))
  vexus_total=$((vexus_total + vexus_grew))
  printf '%-52.52s %8s/%-8s %8s/%-8s %.3f/%.3f\n' "$question" \
    "$plain_calls" "$vexus_calls" "$plain_grew" "$vexus_grew" "$plain_cost" "$vexus_cost"
done < "$tasks_file"

echo
echo "context added, all questions: plain=$plain_total vexus=$vexus_total"
if [ "${VEXUS_COMPARE_STRICT:-0}" = 1 ] && [ "$vexus_total" -gt "$plain_total" ]; then
  echo "vexus used more context than plain tools" >&2
  exit 1
fi
