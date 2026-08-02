# Changelog

## v0.2.0 — Windows, 18 languages, and small repos that pay their way

The three things that kept vexus off real machines: it did not run on
Windows, it understood three languages, and on a repo of a few dozen
files it cost more tokens than grep. All three are fixed.

### Platforms

- **Windows is supported** (x64 and arm64). The advisory writer lock moved
  from raw `libc::flock` to `std::fs::File::try_lock` — flock(2) on Unix,
  LockFileEx on Windows — replacing a non-unix stub that handed *every*
  process a writer lock, so concurrent `serve` instances raced. Mutual
  exclusion now holds everywhere and its test runs unconditionally. `libc`
  is gone as a dependency. Releases ship `.zip` archives with `vexus.exe`;
  `irm https://raw.githubusercontent.com/faique43/vexus/main/install.ps1 | iex`
  installs to `%LOCALAPPDATA%\vexus\bin` with mandatory SHA256
  verification. CI runs the full suite on `windows-latest`, and a
  `.gitattributes` forces LF checkouts so snapshots and blake3 file hashes
  match across platforms.
- **Intel macOS and glibc < 2.39 are no longer stranded.** A default
  `onnx` cargo feature compiles the embedding runtime out under
  `--no-default-features`, which is what frees those targets from ort's
  prebuilt-binary matrix. Releases ship `-structural` artifacts for
  `x86_64-apple-darwin` and `x86_64-unknown-linux-gnu` (glibc 2.35):
  keyword and call-graph search work fully, semantic search is off, and
  both the startup line and `status` say so. `install.sh` detects pre-2.39
  glibc — it used to install a binary that failed at exec — and Intel
  macOS is no longer a hard error. A CI job keeps the no-ONNX build
  compiling. **musl/Alpine remains unsupported**: compiling ort out is not
  enough there, because `sqlite-vec`'s C source needs the BSD-only
  `u_int8_t` family of typedefs that musl doesn't provide. `install.sh`
  now says so instead of downloading an artifact that doesn't exist.
- **Every release artifact is smoke-tested before packaging**: the built
  binary runs `--version`, indexes a three-file fixture, and must report
  its symbols. Nothing previously executed a release binary, so one that
  linked but crashed at ONNX load would have shipped.
- The watcher canonicalizes its root through `dunce`, so Windows
  `ReadDirectoryChangesW` events (plain `C:\...`) match the watched root
  instead of failing `strip_prefix` against a `\\?\`-prefixed path.

### Languages: 3 → 18

**JavaScript/JSX, Go, Java, C, C++, C#, Kotlin, Swift, Ruby, PHP, Scala,
Elixir, Dart, Lua and Bash** join Python, TypeScript/TSX and Rust. Each is
a grammar dependency, two `.scm` query files and a registry entry — still
no parser code. Notable per-language behavior:

- Go receiver methods index as methods with receiver-free arity; Java
  captures constructors and records; C captures prototypes and typedefs
  (`#define` is a known gap). Plain JS gets its own grammar rather than
  riding TypeScript's, which diverges on legacy and Flow-annotated files.
- C++ namespaces index as modules and out-of-class `Type::method`
  definitions keep their qualified name. C# covers both namespace forms;
  properties are deliberately not symbols. Kotlin objects and enum classes
  index as classes. Swift extensions nest members under the extended
  type's name, and Swift symbols carry no arity (its grammar has no
  parameter-list node), so those edges resolve by name only.
- Ruby captures modules, `def self.x` singletons, and methods with
  optional parameters. PHP exercises the new `\` namespace separator end
  to end. Scala objects index as classes. Elixir's `def`/`defp`/`defmodule`
  are ordinary call nodes discriminated by query predicates; a def's own
  head parses as a call, so every Elixir function currently carries a
  self-edge.
- Dart, Lua (`M.f`/`M:m` names resolve by suffix) and Bash (functions
  only; commands become potential callees) round out the set.

Supporting work: the symbol extractor learned `@def.module` and
`@def.method` captures, the resolver learned PHP's `\` separator, the
chunker learned `--` comments, and tree-sitter moved to 0.25 (C# and Swift
ship ABI-15 parsers that 0.24 refuses to load). Binary size grew ~27 MB
unstripped for all fifteen grammars; release binaries are stripped.

### Small repos

Three retrieval changes, all no-ops at 2,000+ chunks — the scale the
original constants were tuned for:

- **A KNN distance floor.** sqlite-vec's `k = 50` returns the 50 nearest
  chunks regardless of distance, so on a corpus smaller than that, every
  query ranked the entire repo. Candidates above the embedder's L2 floor
  (jina-code-v2: 1.1; `VEXUS_KNN_FLOOR` overrides, `0` disables) no longer
  fuse into results — unless nothing else matched, in which case they come
  back explicitly labeled.
- **Honest weak matches.** With no keyword hit and nothing under the floor,
  `explore` and `search` now say so and point at grep, and `explore` skips
  graph expansion rather than fanning out from a wrong guess. Previously
  `explore` could never answer "no" on a non-empty repo.
- **Corpus-tier limits.** Below 200 chunks `explore` uses 8 entries / 6
  expanded symbols / 12 neighbors / 4,000-token default budget; 200–1,999
  chunks gets 8/6/16/4,000. Real-model token cost on the small benchmark
  corpora fell 35–40% on the worst questions while every real-model
  retrieval metric improved (answer-in-bundle +0.06, recall@10 +0.05).

### First run

- **Cold `vexus serve` no longer loads the embedding model twice.** The
  startup index build and the writer thread each used to construct their
  own ORT session from the ~160 MB file.
- The model download reports progress every ~10% instead of going silent
  for minutes, and a first-ever embedding pass narrates regardless of size
  — previously a small repo's first index, the one that also pays for the
  download, was the quietest.
- The Claude Code pack's grep nudge is now `vexus hook nudge-grep` instead
  of a bash script, so the pack works identically on Windows. Re-run
  `vexus init --agent claude-code --force` to migrate; `nudge-grep.sh`
  ships one more release as a shim.

## v0.1.4 — index every const function form, budget and de-noise the graph tools

Dogfooding on a large TypeScript repo surfaced three defects, all fixed:

- **Symbols**: `const f = function ...` and `const g = async function* ...`
  now index like arrow consts do (they are distinct tree-sitter node types
  the query didn't match), and `function*` declarations are captured too.
  Previously these symbols didn't exist, so `callers`/`callees`/`impact`
  answered "no symbol found" for them.
- **`impact`** takes `budget_tokens` (default 4000, `budget` alias) like
  every other tool. Its 500-row cap bounds rows, not bytes — one hot symbol
  returned ~100k chars and blew the MCP client's token limit. The
  `affected: N symbols across M files` summary is always rendered and is
  computed over the full result set, so truncation costs detail, never the
  headline numbers.
- **`callers`/`callees`** collapse duplicate unresolved names at the same
  depth into one row with a ×count (a React hook's callees were 18 rows of
  duplicated builtins), and synthetic unresolved rows no longer render a
  meaningless `(:0)` location.

## v0.1.3 — tolerate the param names agents actually send

The first real Claude Code session called `explore` with `{"query": ...}` —
the reflex trained by `search`'s own param name — got a raw
`missing field 'question'` error, and fell back to grep for the rest of the
session. One guessable mistake should not kill the tool.

- Every MCP tool now accepts the predictable wrong param spellings as
  deserialize-only aliases: `explore` takes `query`/`q` for `question`,
  `search` takes `question`/`q` for `query`, `open` takes `symbol`/`path`
  for `target`, `callers`/`callees`/`impact` take `target`/`name` for
  `symbol`, and every `budget_tokens` accepts `budget`. The published JSON
  schema is unchanged — one canonical field per param.
- The Claude Code steering pack's SKILL.md now states the param names per
  tool. Re-run `vexus init --agent claude-code --force` in already-set-up
  repos to pick that up.

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
