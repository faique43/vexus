#!/usr/bin/env bash
# Nudge once per session toward vexus tools; never block.
marker="${TMPDIR:-/tmp}/vexus-nudge-${CLAUDE_SESSION_ID:-$PPID}"
[ -f "$marker" ] && exit 0
touch "$marker" 2>/dev/null
cat <<'EOF'
{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"This repo has a vexus code index. For finding/understanding code, the vexus MCP tools are faster and cheaper than grep: `explore` answers how/where questions in one call with verbatim source; `search` finds symbols by meaning. Grep remains right for exact strings, comments, and config values."}}
EOF
exit 0
