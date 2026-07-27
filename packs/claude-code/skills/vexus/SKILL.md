---
name: vexus
description: Use when searching, reading, or tracing code in a repo served by the vexus MCP server — picks the right vexus tool instead of grep/read scanning
---

# Navigating code with vexus

This repo's code is indexed (semantic vectors + call graph). Pick by question shape:

| Question | Tool |
|---|---|
| How does X work / where is X handled / what happens when Y | `explore` (one call, returns budgeted verbatim source) |
| Find a symbol/snippet by meaning or words | `search` |
| Show me this exact symbol / file range | `open` |
| Who calls this / what does it call | `callers` / `callees` |
| What breaks if I change this | `impact` |
| Results look wrong or stale | `status` (index freshness + coverage) |

Param names: `explore(question: "…")`, `search(query: "…")`,
`open(target: "qualname or path:start-end")`, `callers`/`callees`/`impact(symbol: "…")`.
All accept an optional `budget_tokens`.

Raise `budget_tokens` when a bundle gets truncated. Use grep only for exact
string/regex hunts, comments, config values, or generated files.
