## Code search: use the vexus MCP server

This repo has a vexus index (semantic + call-graph). Prefer its MCP tools over
grep/find/read-file scanning:

- "how does X work / where is X handled / what happens when Y" → `explore` for
  a ranked map of locations, then `open` only the ranges you need
- who calls / what it calls / what breaks → `trace`
- results look stale or wrong → `status`

Use grep only for exact string/regex hunts, comments, config values, or
generated files — things an index can't answer better.
