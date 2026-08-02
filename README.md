<div align="center">

# vexus

**Stop letting your coding agent grep its way around your codebase.**

Vexus indexes your repo into a semantic + call-graph index and serves it over MCP.
Your agent asks one question and gets the relevant source back — instead of
burning ten tool calls narrowing in on it.

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
explore "how does an invoice get created"   (budget_tokens: 500)
```

One call. Vexus finds the entry points semantically, walks one hop through the
call graph to pull in what they depend on, and returns verbatim source grouped
by file. Below is the complete, unedited response — reproduce it by indexing
[`eval/corpora/pyapp`](eval/corpora/pyapp) and asking that question with
`budget_tokens: 500`:

````text
explore: "how does an invoice get created"

## handlers/invoices.py
handlers/invoices.py:3-8
```
from services.invoice_service import (
    create_invoice,
    get_invoice,
    list_invoices,
    refund_invoice,
)
```
handlers/invoices.py:14-18
```
    def create(self, req):
        """Handle POST /invoices: create a new invoice for the customer."""
        return create_invoice(
            req["customer_id"], req["amount_cents"], req.get("currency", "usd")
        )
```
## services/invoice_service.py
services/invoice_service.py:3-8
```
from models.invoice import Invoice
from services.billing_service import charge_card
from services.notification_service import EmailNotifier, notify_invoice_created
from utils.ids import generate_id
from utils.pagination import paginate
from utils.validation import validate_amount
```
services/invoice_service.py:13-25
```
def create_invoice(customer_id, amount_cents, currency="usd"):
    """Create and persist a new invoice, then notify the customer it exists."""
    validate_amount(amount_cents)
    invoice = Invoice(
        id=generate_id("inv"),
        customer_id=customer_id,
        amount_cents=amount_cents,
        currency=currency,
        status="open",
    )
    _INVOICES[invoice.id] = invoice
    notify_invoice_created(invoice, _default_notifier())
    return invoice
```
## services/notification_service.py
services/notification_service.py:20-28
```
def notify_invoice_created(invoice, notifier):
    """Notify the customer that `invoice` was created, via whichever `notifier` was configured.

    Note: `notifier` is duck-typed — callers pass an `EmailNotifier`, an
    `SmsNotifier`, or any future channel exposing `send(self, message)` —
    so which one actually runs is decided entirely by the caller, not by
    this function.
    """
    return notifier.send(f"Invoice {invoice.id} for {invoice.amount_cents} created")
```
Related (not included, raise budget_tokens or use `open`): services.invoice_service.get_invoice:28-30, handlers.invoices.InvoiceHandler.get:20-22, models.invoice.Invoice:6-14, models.invoice:1-1, services.invoice_service.refund_invoice:39-47, handlers.invoices:1-1, handlers.webhooks:3-4, services.notification_service.EmailNotifier.send:7-9, handlers.webhooks:1-1, jobs.invoice_reminders:1-1, jobs.invoice_reminders:3-4, services.invoice_service:1-1, services.invoice_service:10-10, services.subscription_service:1-1, services.subscription_service:3-5
````

Note the third file. Nothing in the question mentions notifications — vexus
pulled it in because `create_invoice` calls it. That hop is the difference
between "here are some matches" and "here is the flow."

The budget is what keeps that tight. `budget_tokens` defaults to 8000, and at
that setting this same query returns considerably more of the repo — the
ranking is the same, the cutoff is just further down. Agents should pass a
budget that matches how much context the answer is worth.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/faique43/vexus/main/install.sh | sh
```

Downloads the release binary for your platform, verifies it against the
release checksums, and installs to `~/.local/bin`.

<details>
<summary>Other ways to install</summary>

**From source** (Rust stable):

```sh
cargo install --git https://github.com/faique43/vexus vexus-cli
```

**Pin a version / change the location:**

```sh
VEXUS_VERSION=0.1.4 VEXUS_INSTALL_DIR=/usr/local/bin \
  curl -fsSL https://raw.githubusercontent.com/faique43/vexus/main/install.sh | sh
```

Prebuilt binaries: macOS (Apple Silicon) and Linux (x64, arm64, **glibc 2.39 or
newer** — Ubuntu 24.04+, Debian 13+, Fedora 39+). ONNX Runtime is linked
statically, so there is nothing to install alongside the binary.

On an older distro the binary will not start. Ubuntu 22.04, Debian 12 and
RHEL 9 are all below the floor, and building from source does not help there;
see [Limitations](#limitations). Intel macOS and Windows aren't supported
either.

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
| **`explore`** | "How does X work?", "what happens when Y?" — one call, verbatim source, budgeted |
| **`search`** | Find a symbol by meaning, not just by name |
| **`open`** | Fetch a known symbol or an exact file range |
| **`callers`** / **`callees`** | Trace who calls what |
| **`impact`** | Everything a change here could reach, plus import dependents |
| **`status`** | Index freshness, coverage, health |

Every response is real source with `path:line` headers — the agent can act on
it without a follow-up read.

## Benchmarks

Full methodology and caveats: [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

### Context cost scales with your repo — vexus doesn't

Same question, same repository, three sizes. grep+read searches and reads;
vexus makes one `explore` call.

| Repo size | grep + read | vexus | |
| ---: | ---: | ---: | --- |
| 50 files | 1,782 tokens | 863 | 2.1× less |
| 200 files | 7,032 tokens | 609 | 11.5× less |
| 500 files | 17,598 tokens | **469** | **37.5× less** |

Grep grew 9.9×. Vexus went *down*. That's the whole pitch: a tool call returns
a top-ranked handful bounded by its budget, while searching costs more the more
code you have.

> **Straight answer on the small-repo case:** on a few dozen files, grep is
> cheaper and vexus is not worth installing. The benchmark reports those rows
> too, including the ones vexus loses. The synthetic corpus above is also
> grep's worst case (every file matches the term), and the two sides answer at
> different breadth — grep returns every match, vexus returns the best ones.
> Both caveats are spelled out in the benchmark doc rather than buried.

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
correspondingly lower (1.2 s index, 5 ms incremental). Timing is advisory in
CI, not a merge gate: runner variance makes it a poor blocker. Budgets live in
[`bench/budgets.json`](bench/budgets.json).

## How it works

```text
vexus index    tree-sitter → symbols + call/import edges → doc-aware chunks
               → local ONNX embeddings → SQLite (sqlite-vec + FTS5)

vexus serve    MCP over stdio + debounced file watcher + startup reconcile
```

- **Hybrid retrieval.** Vector KNN and BM25 keyword results fused with
  reciprocal rank fusion — semantic recall without losing exact-name precision.
- **Graph expansion.** `explore` doesn't just return matches; it walks one hop
  through callers, callees, and imports, then packs the result to a budget.
- **Honest freshness.** When the index is reconciling or degraded, every tool
  response says so on its first line. Nothing silently serves stale code.
- **Concurrency.** Multiple `vexus serve` processes coordinate with an advisory
  lock: one maintains the index, the rest read it.
- **Local, always.** The embedding model runs on CPU on your machine. No code
  leaves it. Zero agent tokens spent on indexing.

Languages: **Python, TypeScript/TSX, JavaScript/JSX, Rust, Go, Java, C, C++,
C#, Kotlin, Swift**. Adding one is a grammar, a query file, and a registry
entry — no parser code.

## Limitations

The things worth knowing before you rely on it:

- **Call edges are heuristic.** They resolve by name and arity, not by type, so
  same-named methods, duck typing, and dynamic dispatch resolve to a best guess
  or not at all. Every edge carries a confidence label, and unresolved ones say
  `unresolved` rather than guessing quietly.
- **Apple Silicon macOS and recent Linux only.** Windows is out because the
  writer lock uses `flock`. The other two limits both come from the embedded
  ONNX Runtime rather than from vexus, which means building from source does
  not work around either:
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
  migration path, on purpose.
- **Agent adoption is unproven.** The tool descriptions are written to steer
  agents away from grep, but whether they actually do is measured by a harness
  (`eval/agent/`) that hasn't been run against a live model yet. Treat the
  steering as a design intent, not a demonstrated result.

## Contributing

Bug reports that say "this returned garbage for my repo" are as useful here as
patches, and there's an [issue template](.github/ISSUE_TEMPLATE/retrieval_quality.yml)
built for exactly that. Adding a language needs no parser code: a grammar, a
`.scm` query file, and a registry entry.

Start with [CONTRIBUTING.md](CONTRIBUTING.md).

```sh
export VEXUS_EMBEDDER=mock                 # no model download needed to develop

cargo test --workspace                     # ~281 tests
cargo run -p vexus-eval -- check           # retrieval-metric gate
cargo run -p vexus-eval -- perf            # timings (mock embedder — see below)
cargo run -p vexus-eval -- token-bench     # regenerate docs/BENCHMARKS.md
```

Tests run against a deterministic mock embedder, so nothing in CI downloads the
model. The same is true of `perf`, which is why its numbers catch algorithmic
regressions but must never be quoted as user-facing performance.

## License

MIT — see [LICENSE](LICENSE).
