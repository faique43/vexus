---
name: vexus
description: Use when searching, reading, or tracing code in a repo served by the vexus MCP server — picks the right vexus tool instead of grep/read scanning
---

# Navigating code with vexus

This repo's code is indexed (semantic vectors + call graph). Pick by question shape:

| Question | Tool |
|---|---|
| How does X work / where is X handled / what happens when Y | `explore`, then `open` what you need |
| Show me this exact symbol / file range | `open` |
| Who calls this / what does it call / what breaks if I change it | `trace` with `direction: callers`, `callees` or `impact` |
| Results look wrong or stale | `status` |

`explore` returns a ranked map of locations and call links, not source. Open
narrow ranges from it rather than whole files. Use grep only for exact
string/regex hunts, comments, config values, or generated files.
