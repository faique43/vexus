<div align="center">

# vexus

**Stop letting your coding agent grep its way around your codebase.**

Vexus indexes your repo into a semantic + call-graph index and serves it over MCP.
Your agent asks one question and gets a ranked map of the code behind it, then
reads only what it needs.

Runs entirely on your machine. Indexing and embedding cost zero agent tokens.

[![CI](https://github.com/faique43/vexus/actions/workflows/ci.yml/badge.svg)](https://github.com/faique43/vexus/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)

[Install](#install) · [Quickstart](#quickstart) · [Benchmarks](#benchmarks) · [How it works](#how-it-works) · [Limitations](#limitations)

</div>

---

## The problem

Ask an agent "how does an invoice get created?" in a repo it doesn't know, and
watch what happens:

```text
Grep "invoice"              → matches across a dozen files
Read handlers/invoices.py   → the wrong layer; it just forwards
Grep "create_invoice"       → more matches
Read services/invoice_service.py  → closer
Read models/invoice.py      → needed this too
```

Several round trips, most of the pulled-in text irrelevant, and the
notification hook hanging off the end of that flow still got missed. The agent
is doing full-text search because that is the only tool it has.

## The fix

```text
explore "how does an invoice get created"
```

One call. Vexus finds the entry points semantically, walks the call graph, and
returns a ranked map: locations, signatures and call links, with no source.
Below is the complete, unedited response. Reproduce it by indexing
[`eval/corpora/pyapp`](eval/corpora/pyapp) and asking that question:

```text
explore: "how does an invoice get created"

Best match first. These are locations, not source: `open` "path:start-end" for what you need.

## services/notification_service.py
20-28 def notify_invoice_created(invoice, notifier):
  calls: send  called by: create_invoice
7-9 def send(self, message):
## services/invoice_service.py
13-25 def create_invoice(customer_id, amount_cents, currency="usd"):
  calls: Invoice, _default_notifier, notify_invoice_created, generate_id  called by: create
28-30 def get_invoice(invoice_id):
39-47 def refund_invoice(invoice_id, amount_cents=None):
50-52 def _default_notifier():
## handlers/invoices.py
14-18 def create(self, req):
  calls: get, create_invoice  called by: dispatch_create
20-22 def get(self, req):
28-30 def refund(self, req):
24-26 def list(self, req):
## jobs/invoice_reminders.py
7-17 def send_invoice_reminders(customer_ids):
## models/invoice.py
7-14 class Invoice:
## utils/ids.py
6-8 def generate_id(prefix):
## utils/validation.py
20-25 def validate_amount(amount_cents):
## app.py
37-46 def dispatch_create(handler, req):
## config.py
16-24 def load_config():
## handlers/reports.py
10-13 def generate_usage_report(self, req):
## services/rate_limiter.py
19-29 def allow(self, key):
## services/subscription_service.py
10-22 def renew_subscription(subscription_id):
25-31 def cancel_subscription(subscription_id):
```

The model reads the map, decides what matters, and calls `open` on exactly those
ranges. That's the design: an agent pays a few tokens per guess and judges it
before paying for source. Returning source up front looks cheaper and isn't,
because every wrong guess costs a whole function.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/faique43/vexus/main/install.sh | sh
```

Downloads the release binary for your platform, verifies it against the
release checksums, and installs to `~/.local/bin`.

On Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/faique43/vexus/main/install.ps1 | iex
```

<details>
<summary>Other ways to install</summary>

**From source** (Rust stable):

```sh
cargo install --git https://github.com/faique43/vexus vexus-cli
```

**Pin a version / change the location:**

```sh
VEXUS_VERSION=0.3.0 VEXUS_INSTALL_DIR=/usr/local/bin \
  curl -fsSL https://raw.githubusercontent.com/faique43/vexus/main/install.sh | sh
```

Prebuilt binaries: macOS (Apple Silicon), Linux (x64, arm64, **glibc 2.39 or
newer** — Ubuntu 24.04+, Debian 13+, Fedora 39+) and Windows (x64, arm64).
ONNX Runtime is linked statically, so there is nothing to install alongside
the binary.

Hosts the full build can't run on get a **structural-only** build instead —
the embedding runtime is compiled out, so semantic search is off but keyword
and call-graph search work fully. `install.sh` detects these cases itself
and says so: Intel macOS, and Linux with glibc older than 2.39 (Ubuntu
22.04, Debian 12, RHEL 9). `vexus status` reports which build is running.
musl systems (Alpine) are not supported at all — the vector-search
dependency does not build against musl. See [Limitations](#limitations).

</details>

## Quickstart

```sh
cd your-repo
vexus index .     # first run also fetches the embedding model (~160 MB, once)
```

Point your MCP client at it. **Claude Code**:

```sh
vexus init --agent claude-code   # registers the MCP server in .mcp.json + installs the steering pack
```

Or add it by hand — `.mcp.json` in your repo:

```json
{
  "mcpServers": {
    "vexus": { "command": "vexus", "args": ["serve", "."] }
  }
}
```

<details>
<summary>Cursor, Windsurf, Cline, and other MCP clients</summary>

Any MCP client that can launch a stdio server works. The command is
`vexus serve <path-to-repo>`. For Cursor, add the same block to
`.cursor/mcp.json`.

Optional: `vexus init --agent cursor` writes a rules file telling the agent
when to prefer vexus over grep. `--agent claude-code` installs a skill and a
one-time nudge hook; `--agent generic` prints a snippet for your `AGENTS.md`.

</details>

That's it. `vexus serve` runs a file watcher, so the index stays current as you
edit — no re-indexing by hand.

## What your agent gets

| Tool | Use it for |
| --- | --- |
| **`explore`** | "How does X work?", "what happens when Y?" — a ranked map of locations, signatures and call links |
| **`open`** | Fetch a symbol or an exact file range |
| **`trace`** | Follow a symbol's callers, callees, or everything a change could reach |
| **`status`** | Index freshness, coverage, health |

Four tools on purpose: definitions are billed on every session, whether a tool
gets used or not.

## Benchmarks

Full methodology and caveats: [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

### Live agent runs

The numbers that count: the same questions in real Claude Code sessions, one
with vexus and one with only Read, Grep and Glob. Same model, three rounds of
five questions per repo, 60 sessions in all. Final context includes vexus's
tool definitions. Reproduce it with
[`eval/agent/compare.sh`](eval/agent/compare.sh).

| Repo | Cost | Tool calls | Final context | Expected names in the answer |
| --- | ---: | ---: | ---: | ---: |
| NestJS (2,205 files) | **29% less** ($0.72 → $0.51) | 26% fewer | 12% less | 41 → 39 of 51 |
| Express (142 files) | **17% less** ($0.56 → $0.46) | 24% fewer | same | 42 → 42 of 42 |

Vexus was cheaper in all six rounds. The savings grow with the repo, and it
still loses on a question that names exactly what to find: "how does app.use
register middleware" cost plain tools half as much every time. On a few dozen
files grep is cheaper, and vexus is not worth installing.

[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) has a synthetic table too, but it
counts only the `explore` call and not the reads that follow, so use it for
regressions, not for comparing against grep.

### Retrieval quality

Measured against hand-labelled fixture corpora — 69 graded queries, 99 labelled
call edges — and **gated in CI**: any metric dropping more than 0.02 absolute
fails the build.

| | |
| --- | ---: |
| recall@5 | 0.80 |
| recall@10 | 0.92 |
| answer-in-bundle (`explore` returned the source you needed) | 0.83 |
| call-edge precision / recall | 0.88 / 0.93 |

### Speed

500 Python files, release build, **real embedding model**, M-series laptop —
what you would actually feel using it:

| | |
| --- | ---: |
| First index (cold, includes embedding all 2,000 chunks) | 9.6 s |
| Save → searchable again (includes the 500 ms debounce) | ~565 ms |
| `search` p99 | 8 ms |
| `explore` p99 | 9 ms |

Queries stay in single-digit milliseconds because embedding one short question
is cheap; the model cost is paid at index time, in a background thread, off
your agent's clock.

`vexus-eval perf` reports the same shape against a **mock** embedder — that is
what CI tracks, so the harness never downloads a model — and its numbers are
correspondingly lower (0.26 s to "embed" 2000 chunks, versus 13.6 s for the
real model). Pass `--real` to time the shipped model instead. Timing is
advisory in CI, not a merge gate: runner variance makes it a poor blocker.
Budgets live in [`bench/budgets.json`](bench/budgets.json).

## How it works

```text
vexus index    tree-sitter → symbols + call/import edges → doc-aware chunks
               → local ONNX embeddings → SQLite (sqlite-vec + FTS5)

vexus serve    MCP over stdio + debounced file watcher + startup reconcile
```

- **Hybrid retrieval.** Vector KNN and BM25 keyword results fused with
  reciprocal rank fusion — semantic recall without losing exact-name precision.
- **Graph expansion.** `explore` doesn't just return matches; it walks one hop
  through callees and callers, plus what the top match's callers also call.
- **Honest freshness.** When the index is reconciling or degraded, every tool
  response says so on its first line. Nothing silently serves stale code.
- **Concurrency.** Multiple `vexus serve` processes coordinate with an advisory
  lock: one maintains the index, the rest read it.
- **Local, always.** The embedding model runs on CPU on your machine. No code
  leaves it. Zero agent tokens spent on indexing.

Languages (18): **Python, TypeScript/TSX, JavaScript/JSX, Rust, Go, Java,
C, C++, C#, Kotlin, Swift, Ruby, PHP, Scala, Elixir, Dart, Lua, Bash**.
Adding one is a grammar, a query file, and a registry entry — no parser
code.

## Limitations

The things worth knowing before you rely on it:

- **Call edges are heuristic.** They resolve by name and arity, not by type, so
  same-named methods, duck typing, and dynamic dispatch resolve to a best guess
  or not at all. Every edge carries a confidence label, and unresolved ones say
  `unresolved` rather than guessing quietly.
- **Apple Silicon macOS, recent Linux, and Windows.** The two remaining
  platform limits both come from the embedded ONNX Runtime rather than from
  vexus, which means building from source does not work around either:
  - **Intel macOS** has no prebuilt runtime at all, so the build fails.
  - **Linux needs glibc 2.39+.** The prebuilt runtime is compiled against
    glibc 2.38 and references `__isoc23_strtol`, so it will not even link
    against anything older. Ubuntu 22.04, Debian 12 and RHEL 9 are out.

  Lifting either one means vendoring an ONNX Runtime build or switching
  backends. Both are open for contribution.
- **First run needs network.** The model is fetched from Hugging Face (pinned
  revision, checksum-verified). Without it vexus still runs keyword-and-graph
  only, and `status` tells you so.
- **The index is a cache.** Any schema or model change rebuilds it. There is no
  migration path, on purpose — v0.4.0 bumps the schema, so upgrading to it
  re-indexes once.
- **Agent adoption is measured, but on a small sample.** In the first
  agent-in-the-loop run (`eval/agent/`, 2026-08-09, `claude-sonnet-4-5`), an
  agent with both vexus and Grep/Glob/Read available chose vexus for **5 of 5**
  code-navigation calls across the four tasks — a 100% vexus share, answering
  each in a single `explore` or `trace` call. That is four questions on a
  30-file fixture, not a broad result: read it as "the steering works on the
  cases it was designed for", and re-run it (pointing `VEXUS_AGENT_CORPUS` at
  a real checkout) after any change to the tool descriptions.

## Contributing

Bug reports that say "this returned garbage for my repo" are as useful here as
patches, and there's an [issue template](.github/ISSUE_TEMPLATE/retrieval_quality.yml)
built for exactly that. Adding a language needs no parser code: a grammar, a
`.scm` query file, and a registry entry.

Start with [CONTRIBUTING.md](CONTRIBUTING.md).

```sh
export VEXUS_EMBEDDER=mock                 # no model download needed to develop

cargo test --workspace                     # ~380 tests
cargo run -p vexus-eval -- check           # retrieval-metric gate
cargo run -p vexus-eval -- perf            # timings (mock embedder — see below)
cargo run -p vexus-eval -- perf --real     # same, timing the real model
cargo run -p vexus-eval -- token-bench     # regenerate docs/BENCHMARKS.md
```

Tests run against a deterministic mock embedder, so nothing in CI downloads the
model. `perf` defaults to the same mock, which is why its default numbers catch
algorithmic regressions but must never be quoted as user-facing performance —
quote a `perf --real` run instead.

## License

MIT — see [LICENSE](LICENSE).
