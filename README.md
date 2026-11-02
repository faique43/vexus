<div align="center">

# vexus

**Stop letting your coding agent grep its way around your codebase.**

Vexus indexes your repo into a semantic + call-graph index, then tells Claude Code
where to look before it starts searching: a few locations with each prompt, no
tools to learn and no tool definitions to pay for.

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

Vexus searches before the agent does. A hook runs on each prompt, finds the
likely code locally, and adds a few locations to what the agent sees:

```text
Likely relevant code, from the repo index. Check it fits before relying on it.
services/invoice_service.py:13-25 create_invoice
services/notification_service.py:20-28 notify_invoice_created
handlers/invoices.py:14-18 InvoiceHandler.create
services/invoice_service.py:28-30 get_invoice
```

The agent opens the right lines first, and it found them without spending a
token. Nothing to learn, no tool to remember, and no tool definitions in every
session.

The optional MCP server lets the agent ask for more itself:

```text
explore "how does an invoice get created"
```

One call. Vexus finds the entry points semantically, walks the call graph, and
returns a ranked map: locations, signatures and call links, with no source.
Below is the complete, unedited response. Reproduce it by indexing
[`eval/corpora/pyapp`](eval/corpora/pyapp) and asking that question:

```text
explore: "how does an invoice get created"

Grouped by file, best first. These are locations, not source: `open` "path:start-end" for what you need.

## services/invoice_service.py
13-25 def create_invoice(customer_id, amount_cents, currency="usd"):
  calls: Invoice, _default_notifier, notify_invoice_created, generate_id  called by: create
28-30 def get_invoice(invoice_id):
50-52 def _default_notifier():
39-47 def refund_invoice(invoice_id, amount_cents=None):
## services/notification_service.py
20-28 def notify_invoice_created(invoice, notifier):
  calls: send  called by: create_invoice
7-9 def send(self, message):
## handlers/invoices.py
14-18 def create(self, req):
  calls: get, create_invoice  called by: dispatch_create
20-22 def get(self, req):
28-30 def refund(self, req):
24-26 def list(self, req):
## jobs/invoice_reminders.py
7-17 def send_invoice_reminders(customer_ids):
## handlers/webhooks.py
17-22 def _handle_charge_failed(self, req):
## models/invoice.py
7-14 class Invoice:
## utils/ids.py
6-8 def generate_id(prefix):
## utils/validation.py
20-25 def validate_amount(amount_cents):
## app.py
37-46 def dispatch_create(handler, req):
## services/subscription_service.py
10-22 def renew_subscription(subscription_id):
25-31 def cancel_subscription(subscription_id):
## services/user_service.py
23-25 def get_user(user_id):
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
VEXUS_VERSION=0.4.1 VEXUS_INSTALL_DIR=/usr/local/bin \
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

Then hook it into **Claude Code**:

```sh
vexus init --agent claude-code   # adds a prompt hook to .claude/settings.json
```

Each prompt now arrives with a few likely locations, about 100 tokens:

```text
Likely relevant code, from the repo index. Check it fits before relying on it.
lib/utils.js:130-152 compileETag
lib/request.js:449-511 lib.request
lib/response.js:126-222 send
lib/utils.js:249-257 createETagGenerator
```

Nothing else changes. The hook stays silent on replies and on long reports that
already name the code, because hints there cost more than they save. It runs
locally and spends no agent tokens. Line numbers follow a symbol through edits,
and the index refreshes itself in the background.

For more than Claude Code, or to give the agent search tools as well, use the
MCP server. Add `--mcp` to the command above, or put this in `.mcp.json`:

```json
{
  "mcpServers": {
    "vexus": { "command": "vexus", "args": ["serve", "."] }
  }
}
```

Its tool definitions cost every session whether they're used or not, and agents
mostly skip them on change requests, so it's opt-in. See
[live agent runs](#live-agent-runs) for what it does and doesn't save.

<details>
<summary>Cursor, Windsurf, Cline, and other MCP clients</summary>

Any MCP client that can launch a stdio server works. The command is
`vexus serve <path-to-repo>`. For Cursor, add the same block to
`.cursor/mcp.json`.

Optional: `vexus init --agent cursor` writes a rules file telling the agent
when to prefer vexus over grep. `--agent generic` prints a snippet for your
`AGENTS.md`.

</details>

That's it. `vexus serve` runs a file watcher, so the index stays current as you
edit — no re-indexing by hand.

## MCP tools

With `--mcp`, or on any other MCP client, your agent gets:

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

The numbers that count: the same tasks in real Claude Code sessions, with the
hook and without it. Sonnet 5.5, on Express (142 files) and NestJS (about 2,000
files), with sessions shuffled together and caches warmed first.

| Tasks | Sessions | Cost (95% interval) | Tool calls | Without vs with the hook |
| --- | ---: | ---: | ---: | ---: |
| Short change requests | 84 | **21% less** (9 to 33) | 22% fewer | 42 vs 42 of 42 pass |
| Real NestJS bugs, code names removed | 102 | 9% less (−2 to 20) | 19% fewer | 26 vs 27 of 51 pass |
| Same bugs, as filed | 102 | about the same (−4 to 6) | 4% fewer | 28 vs 27 of 51 pass |
| **All change requests** | 288 | **10% less** (4 to 17) | 16% fewer | 96 vs 96 of 144 pass |
| Read-only questions | 120 | **13% less** (3 to 22) | 25% fewer | 144 vs 145 of 150 names |
| Seven prompts in one session | 24 | 9% less | 11% fewer | 84 vs 84 of 84 pass |

Real bugs were scored by the maintainers' own specs, which the agent never saw.
Agents could edit but not run commands, so pass rates sit below what a full
session reaches. Savings are smallest where the agent already knows where to
look, which is why the hook stays silent on detailed reports.

The MCP server is a different trade. On the same read-only questions it cost
31% less (22 to 39), but its answers named fewer of the expected symbols, 136
against 144 of 150. On change requests it saved nothing measurable: 4% less
(−17 to 21), with agents ignoring it in about half of sessions while still
paying for its definitions.

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
| First index (cold, includes embedding all 2,000 chunks) | 17 s |
| Save → searchable again (includes the 500 ms debounce) | ~565 ms |
| `search` p99 | 8 ms |
| `explore` p99 | 9 ms |

Real repos have longer chunks, so they cost more per chunk: a first `vexus index`
of NestJS (2,205 files, 14,806 chunks) takes 171 s at 1.4 GB of memory on an
18-core machine, once the model is downloaded.

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
- **The hook is Claude Code only.** Other agents get the MCP server, which
  saves less on change requests.
- **Savings depend on the work.** They were measured on Express and NestJS, on
  tasks of a few minutes each. Larger repos and long sessions are less tested.
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
