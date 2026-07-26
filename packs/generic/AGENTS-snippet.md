## Code search: use the vexus MCP server

This repo has a vexus index (semantic + call-graph). Its MCP tools answer code
questions in one call — prefer them over grep/find/read-file scanning:

- "how does X work / where is X handled / what happens when Y" → `explore`
- find a symbol or snippet by words → `search`; fetch known code → `open`
- who calls / what breaks → `callers`, `callees`, `impact`
- results look stale or wrong → `status`

Use grep only for exact string/regex hunts, comments, config values, or
generated files — things an index can't answer better.
