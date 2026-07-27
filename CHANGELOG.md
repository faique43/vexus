# Changelog

## v0.1.1 — document the real Linux floor, unpin a deprecated runner

The Linux binaries require **glibc 2.39**, so they do not start on Ubuntu
22.04, Debian 12, RHEL 9 or Amazon Linux 2023. v0.1.0 shipped without saying
so; the docs now state it up front.

This floor is not a packaging choice. The prebuilt ONNX Runtime that `ort`
downloads is compiled against glibc 2.38 and references `__isoc23_strtol`, so
building on an older image fails at link time rather than producing a more
portable binary. Ubuntu 24.04 is the oldest image that links. Lowering the
floor means building ONNX Runtime from source or switching backends, and
`cargo install` hits the same wall.

- README, CONTRIBUTING and the limitations section state the glibc floor and
  why building from source does not work around it.
- The release workflow fails if the required glibc drifts above 2.39, so a
  newer runner image cannot raise it unnoticed.
- The Apple Silicon build moved off the deprecated `macos-14` image. A retired
  runner does not fail, it stays queued until the run times out.

No code changes. macOS users are unaffected.

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

Apple Silicon macOS and Linux (x64, arm64, glibc 2.39+) in this release.
Windows needs a non-`flock` writer lock; Intel macOS needs an ONNX Runtime
build that the embedding backend does not ship. See the README's limitations.
