# vexus

Local code intelligence for coding agents. Vexus indexes a repository into a
semantic + structural graph and serves it over MCP, so an agent asks one
question and gets back the relevant source — instead of grepping its way
there a file at a time.

Everything runs on your machine. Indexing and embedding cost zero agent
tokens, and the index keeps itself current while the server is running.

Existing tools tend to be one half or the other: a structural graph with no
semantics, or vector search with no call graph. Vexus is both, incrementally
maintained by a file watcher, in one zero-config binary.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/faique43/vexus/main/install.sh | sh
```

Or from source (Rust stable):

```sh
cargo install --git https://github.com/faique43/vexus vexus-cli
```

## Quickstart

```sh
cd your-repo
vexus index .                    # first run downloads the embedding model (~160 MB)
vexus init --agent claude-code   # optional: install the steering pack
```

Then point your MCP client at it. For Claude Code, `.mcp.json`:

```json
{ "mcpServers": { "vexus": { "command": "vexus", "args": ["serve", "."] } } }
```

`vexus serve` hosts the tools and a file watcher, so edits are reflected
without re-indexing by hand.

## What the agent gets

| Tool | For |
| --- | --- |
| `explore` | "How does X work?", "where is Y handled?" — one call returns the relevant verbatim source, budgeted and grouped by file |
| `search` | Find a symbol by meaning or by words |
| `open` | Fetch a known symbol or an exact file range |
| `callers` / `callees` | Who calls this, what this calls |
| `impact` | Everything a change here could reach |
| `status` | Index freshness, coverage, health |

Every response is verbatim source with `path:line` headers, capped by a token
budget you can raise per call.

## How it works

```
vexus index    tree-sitter parse → symbols + call/import edges → chunks → local ONNX embeddings → SQLite
vexus serve    MCP (stdio) + file watcher + startup reconcile
.vexus/index.db   per-repo, gitignored, WAL
```

Search is hybrid: vector KNN and FTS5 keyword results fused with reciprocal
rank fusion. `explore` adds a one-hop expansion through the call and import
graph, then packs the result to a token budget.

Freshness is reported rather than assumed: when the index is reconciling or
degraded, every tool response says so on its first line, and `status` explains
why. Concurrent `vexus serve` processes coordinate with an advisory lock —
one maintains the index, the others read it.

The design document is in
[`docs/superpowers/specs/`](docs/superpowers/specs/); the implementation
plans that built it are alongside in `docs/superpowers/plans/`.

## Measurement

Retrieval quality is gated in CI against hand-labelled fixture corpora: a
regression greater than 0.02 absolute in recall@5/10, MRR, nDCG@10,
answer-in-bundle, or edge precision/recall fails the build. See
[`eval/README.md`](eval/README.md).

Token cost versus grepping is measured in
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md). The summary: a vexus call is
bounded by its token budget, while grep-and-read grows with the repository —
on a 500-file corpus that is roughly 36× less context for the same question,
and the gap widens with size. On a very small repository an index does not
pay for itself, and the benchmark says so.

Performance budgets (500-file corpus, checked by `vexus-eval perf`):
indexing under 30s, a single-file update under 1s, `search` p99 under 200ms,
`explore` p99 under 600ms.

## Languages

Python, TypeScript/TSX, and Rust are indexed today. Symbols and import edges
are extracted per language from tree-sitter queries; adding a language is a
grammar, a query file, and a registry entry — no parser code.

## Limitations

Worth knowing before you rely on it:

- **Call edges are heuristic.** They resolve by name and arity, not by type.
  Same-named methods on different types, duck-typed dispatch, and dynamic
  calls resolve to a best guess or not at all. Every edge is labelled with
  its confidence, and unresolved ones say `unresolved` rather than guessing
  silently.
- **Unix only for now.** The advisory writer lock uses `flock`; on other
  platforms every `vexus serve` would consider itself the writer.
- **The first run needs the network.** The embedding model is fetched from
  Hugging Face (pinned revision, checksum-verified) into `~/.vexus/models/`,
  and `ort` fetches an ONNX Runtime build at compile time. Without the model
  vexus still runs, keyword-and-graph only, and says so in `status`.
- **The index is a cache.** Any schema or model change rebuilds it from
  scratch — there is no migration path, by design.

## Development

```sh
cargo test --workspace                     # ~280 tests
cargo run -p vexus-eval -- check           # retrieval-metric gate
cargo run -p vexus-eval -- perf            # performance budgets
cargo run -p vexus-eval -- token-bench     # regenerate docs/BENCHMARKS.md
```

Tests use a deterministic mock embedder (`VEXUS_EMBEDDER=mock`), so nothing
in CI downloads the model.

## License

MIT — see [LICENSE](LICENSE).
