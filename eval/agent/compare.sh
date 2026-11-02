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
# Output is split as plain/vexus. Both arms run isolated from the user's own plugins, memory and MCP servers,
# which would otherwise skew the numbers. Skills stay off on both sides because
# the built-in skill list alone adds about 3,000 tokens per session.
#
# Usage:  VEXUS_AGENT_MODEL=claude-sonnet-5-5 bash eval/agent/compare.sh
#   VEXUS_AGENT_CORPUS   repo to test (default: eval/corpora/pyapp)
#   VEXUS_AGENT_TASKS    file of `question | expected, names`, one per line
#   VEXUS_COMPARE_KEEP_INDEX  set to 1 to reuse the corpus's existing .vexus
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
rm -rf "$workdir/repo/.mcp.json" "$workdir/repo/.claude"
[ "${VEXUS_COMPARE_KEEP_INDEX:-0}" = 1 ] || rm -rf "$workdir/repo/.vexus"
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

# Final context, not growth: tool definitions are paid for on every turn of every
# session, so a measure that starts after them flatters whichever arm has more.
# Coverage counts the expected names the answer mentions, so a cheap wrong
# answer can't pass as a saving.
measure() { # transcript, comma-separated names -> "calls context cost covered"
  jq -rs --arg names "$2" '
    [.[] | select(.type == "assistant") | .message] as $turns
    | ($turns | map(.usage | .input_tokens + .cache_read_input_tokens + .cache_creation_input_tokens)) as $context
    | ($turns | map(.content[] | select(.type == "tool_use")) | length) as $calls
    | (.[] | select(.type == "result")) as $result
    | ($names | split(",") | map(gsub("^ +| +$"; ""))) as $expected
    | ($expected | map(select(. as $name | $result.result | ascii_downcase | contains($name | ascii_downcase))) | length) as $hit
    | "\($calls) \($context[-1]) \($result.total_cost_usd) \($hit)/\($expected | length)"
  ' "$1"
}

# Warm both arms' prompt caches first, or whichever arm runs cold pays to write
# its whole prompt on the first question and looks worse for it.
session 'reply ok' '{"mcpServers":{}}' "$results/warm-plain.jsonl"
session 'reply ok' "$workdir/vexus.json" "$results/warm-vexus.jsonl" --allowedTools 'mcp__vexus__*'

printf '%-44s %9s %15s %13s %11s\n' question calls 'final context' cost covered
plain_total=0
vexus_total=0
n=0
while IFS='|' read -r question names; do
  question="${question%"${question##*[![:space:]]}"}"
  [ -n "$question" ] || continue
  n=$((n + 1))
  session "$question" '{"mcpServers":{}}' "$results/q$n-plain.jsonl"
  session "$question" "$workdir/vexus.json" "$results/q$n-vexus.jsonl" --allowedTools 'mcp__vexus__*'
  read -r plain_calls plain_context plain_cost plain_hit <<< "$(measure "$results/q$n-plain.jsonl" "$names")"
  read -r vexus_calls vexus_context vexus_cost vexus_hit <<< "$(measure "$results/q$n-vexus.jsonl" "$names")"
  plain_total=$((plain_total + plain_context))
  vexus_total=$((vexus_total + vexus_context))
  printf '%-44.44s %4s/%-4s %7s/%-7s %.3f/%.3f %5s %5s\n' "$question" \
    "$plain_calls" "$vexus_calls" "$plain_context" "$vexus_context" \
    "$plain_cost" "$vexus_cost" "$plain_hit" "$vexus_hit"
done < "$tasks_file"

echo
echo "final context, all questions: plain=$plain_total vexus=$vexus_total"
if [ "${VEXUS_COMPARE_STRICT:-0}" = 1 ] && [ "$vexus_total" -gt "$plain_total" ]; then
  echo "vexus used more context than plain tools" >&2
  exit 1
fi
