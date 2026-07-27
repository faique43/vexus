# Changelog

## v0.1.2 — make the first run legible

The first real-world install surfaced every first-run rough edge at once: the
embedding phase after `vexus index`'s structural summary printed nothing for
minutes and read as a hang, `vexus serve` run by hand blocked silently, and
nothing actually registered the MCP server. All fixed:

- `vexus init --agent claude-code` now registers the MCP server in
  `.mcp.json`: creates the file, or merges into an existing one without
  touching other servers or reordering keys. An identical entry is skipped, a
  customized `vexus` entry is kept unless `--force`, and malformed JSON is
  left alone (the snippet is printed instead).
- `vexus index` reports embedding progress on large backlogs — an upfront
  count plus `embedded X/N chunks` lines — and says it is safe to interrupt
  (it resumes from where it stopped). The watcher's small incremental updates
  stay silent.
- `vexus serve` prints a startup banner on stderr explaining it is a stdio
  MCP server normally launched by an agent via `.mcp.json`, not by hand.
- The model download line now includes a size hint (~160 MB, one-time).
- `-v` is accepted alongside `-V`/`--version`.
- `install.sh`'s next steps no longer suggest running `vexus serve` manually
  and warn that the first index downloads the model and can take minutes.

Also picks up dependency bumps: notify 6→8, tokenizers 0.23, actions/cache 6.

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
