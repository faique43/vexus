# Changelog

## Unreleased

- **The Claude Code pack's grep nudge no longer needs bash.** hooks.json
  now runs `vexus hook nudge-grep` (a hidden subcommand reading the hook
  payload's `session_id` from stdin), so the pack behaves identically on
  Windows/cmd/PowerShell. `nudge-grep.sh` is deprecated and now just execs
  the subcommand; it ships for one more release for hooks.json files
  installed by older versions — re-run
  `vexus init --agent claude-code --force` to migrate.
Small repos stop paying big-repo prices. Three retrieval changes, all
no-ops at Medium scale (≥2,000 chunks — the historical constants):

- **KNN distance floor.** vec0's `k = 50` returns the 50 nearest chunks
  regardless of distance, so on a corpus smaller than 50 chunks every
  query ranked the whole repo. Candidates above the embedder's L2 floor
  (jina-code-v2: 1.1; `VEXUS_KNN_FLOOR` overrides, `0` disables) no longer
  fuse into results — unless nothing else matched, in which case they come
  back explicitly labeled.
- **Honest weak matches.** When keyword search finds nothing and no vector
  candidate clears the floor, `explore` and `search` now say "weak match —
  nearest neighbors only, grep is the better tool here" and `explore`
  skips graph expansion instead of fanning out from a wrong guess.
  Previously `explore` could never say "no" on a non-empty repo.
- **Corpus-tier explore limits.** Under 200 chunks, `explore` uses 8
  entries / 6 expanded symbols / 12 neighbors / 4,000-token default budget
  (was 12/8/24/8,000); 200–1,999 chunks gets 8/6/16/4,000. Real-model token
  cost on the small benchmark corpora dropped ~35–40% on the worst
  questions while every real-model retrieval metric improved
  (answer-in-bundle +0.06 overall, recall@10 +0.05). Both baselines
  re-blessed; the mock baseline's answer_in_bundle drop is an artifact of
  random-ranking coverage, not retrieval quality — see the PR for the
  full analysis.
- **Cold `vexus serve` no longer loads the embedding model twice.** The
  writer path builds its embedder once and seeds the shared state with it;
  previously the startup index build and the writer thread each constructed
  their own ORT session from the ~160 MB model file.
- **The model download reports progress** — a stderr line every ~10% with
  MB downloaded/total — instead of going silent for minutes after the
  initial "downloading …" announcement.
- **A first-ever embed pass narrates regardless of size.** The progress
  gate only announced backlogs above 256 chunks, so a small repo's first
  index — the run that also pays the one-time model download — was the
  most silent one. First passes (nothing embedded yet) now always print;
  the watcher's steady-state updates stay silent.
- **Real cross-platform writer locking.** The advisory writer lock on
  `.vexus/lock` now uses `std::fs::File::try_lock` — flock(2) on Unix,
  LockFileEx on Windows — replacing the raw `libc::flock` call and the
  non-unix stub that handed every process a writer lock. Mutual exclusion
  now holds on every platform, and the mutual-exclusion test runs
  unconditionally. `libc` is no longer a dependency.
- CI runs the full test suite on `windows-latest` alongside Ubuntu and
  macOS. A `.gitattributes` forces LF checkouts so snapshot tests and
  blake3 file hashes are byte-identical across platforms.
- **Windows releases.** The release matrix builds `x86_64-pc-windows-msvc`
  and `aarch64-pc-windows-msvc` (both have prebuilt ONNX runtimes),
  packaged as `.zip` with `vexus.exe`. `install.ps1` installs to
  `%LOCALAPPDATA%\vexus\bin` with mandatory SHA256 verification and adds
  it to the user PATH: `irm .../install.ps1 | iex`.
- **Every release artifact is now smoke-tested** before packaging: the
  built binary runs `--version`, indexes a 3-file fixture and must report
  its symbols — catching binaries that link but crash at load, on every
  target including the ONNX-static ones.
- **Structural-only builds un-strand Intel macOS, old-glibc Linux, and
  musl/Alpine.** A new default `onnx` cargo feature compiles the ONNX
  runtime out under `--no-default-features`; the release ships
  `-structural` artifacts for `x86_64-apple-darwin`,
  `x86_64-unknown-linux-gnu` (glibc 2.35 floor) and
  `x86_64-unknown-linux-musl` (fully static). Keyword and call-graph
  search work fully; semantic search is off, and both the startup stderr
  line and `status` say so. `install.sh` now detects musl and pre-2.39
  glibc — previously it silently installed a binary that failed at exec —
  and installs the structural artifact with an honest note; Intel macOS
  stops being a hard `die`. A CI job keeps the no-ONNX shape compiling.
- The watcher canonicalizes the root via `dunce`, so Windows
  ReadDirectoryChangesW events (plain `C:\...`) match the watched root
  instead of failing `strip_prefix` against a `\\?\`-prefixed path.
- The watcher canonicalizes the root via `dunce`, so Windows
  ReadDirectoryChangesW events (plain `C:\...`) match the watched root
  instead of failing `strip_prefix` against a `\\?\`-prefixed path.
  blake3 file hashes are byte-identical across platforms. (Windows release
  artifacts and an installer land separately.)

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
