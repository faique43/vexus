# Changelog

## v0.1.1 — Linux binaries that actually run

The v0.1.0 Linux artifacts were built on Ubuntu 24.04 and so required
`GLIBC_2.39`. A glibc-linked binary runs on its build machine's glibc and
newer, never older, which left them failing at exec on Ubuntu 22.04, Debian 12,
RHEL 9 and Amazon Linux 2023 with `version 'GLIBC_2.39' not found`.

- Linux targets now build on Ubuntu 22.04, dropping the floor to **glibc 2.35**.
- The release workflow fails if the required glibc rises above 2.35, so a
  future runner bump cannot reintroduce this quietly.
- The Apple Silicon build moved off the deprecated `macos-14` image. A retired
  runner does not fail, it stays queued until the run times out.

No code changes. macOS users are unaffected; if you are on Linux and v0.1.0
would not start, this is the fix.

## v0.1.0 — first release

Local code intelligence for coding agents: a semantic + structural index of a
repository, served over MCP.

- **Index.** tree-sitter parsing for Python, TypeScript/TSX and Rust into
  symbols, call and import edges, and doc-comment-aware chunks; local ONNX
  embeddings (jina-code-v2, downloaded and checksum-verified on first run);
  SQLite storage with sqlite-vec and FTS5.
- **Serve.** `vexus serve` speaks MCP over stdio with seven tools —
  `explore`, `search`, `open`, `callers`, `callees`, `impact`, `status` —
  returning token-budgeted verbatim source. Hybrid retrieval fuses vector and
  keyword results; `explore` expands one hop through the graph.
- **Freshness.** A debounced file watcher and a startup reconcile keep the
  index current; every tool response reports when the index is not fresh.
  Concurrent servers coordinate through an advisory lock.
- **Steering.** `vexus init` installs packs for Claude Code, Cursor, or any
  agent reading `AGENTS.md`.
- **Measurement.** Retrieval metrics gated in CI against hand-labelled
  corpora, performance budgets, and a token-cost benchmark versus grep.

Apple Silicon macOS and Linux (x64, arm64, glibc 2.35+) in this release.
Windows needs a non-`flock` writer lock; Intel macOS needs an ONNX Runtime
build that the embedding backend does not ship. See the README's limitations.
