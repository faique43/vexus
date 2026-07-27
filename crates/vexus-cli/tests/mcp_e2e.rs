//! End-to-end coverage over the *real* MCP stdio transport: spawns the
//! `vexus` binary's `serve` subcommand as a genuine child process and drives
//! it with rmcp's own client (not the pure `vexus_mcp::tools::*` functions
//! unit-tested elsewhere) — this is the only test in the workspace that
//! proves the server actually speaks MCP end-to-end: real JSON-RPC framing
//! over real pipes, a real `initialize` handshake, real `tools/list` and
//! `tools/call` round-trips.
//!
//! Lives in `vexus-cli` (not `vexus-mcp`) because `assert_cmd::cargo_bin`
//! only resolves binaries owned by the crate under test, and the `vexus`
//! binary lives in this crate.
//!
//! rmcp 3.0.0-beta.2 client API notes (the crates.io listing is stale at
//! "0.3"; the resolved version is
//! `3.0.0-beta.2`, whose client shape follows its own
//! `tests/test_with_python.rs`):
//! - `ServiceExt::serve` performs the `initialize` handshake as part of
//!   connecting, so `().serve(transport).await?` alone is already a live,
//!   initialized session — there's no separate `.initialize()` call.
//! - `CallToolRequestParam` is a deprecated alias for `CallToolRequestParams`,
//!   which is `#[non_exhaustive]`; build it via `::new(name).with_arguments(map)`
//!   rather than a struct literal.
//! - `RunningService<RoleClient, ()>` (what `().serve(..)` returns) derefs to
//!   `Peer<RoleClient>`, which is where `list_all_tools`/`call_tool`/
//!   `peer_info` live.

use std::path::Path;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command as AssertCommand;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// A small call chain (`alpha_process` -> `unique_marker_beta`) with
/// distinctive keywords, so `search`/`explore` deterministically surface it
/// under `VEXUS_EMBEDDER=mock` (a content-blind hash embedder — the FTS side
/// of hybrid search is what actually has to find these names).
fn write_chain_repo(root: &Path) {
    write(
        root,
        "app.py",
        "def alpha_process():\n    \"\"\"Kicks off unique_marker_beta.\"\"\"\n    unique_marker_beta()\n\n\ndef unique_marker_beta():\n    \"\"\"Does the real unique_marker_beta work.\"\"\"\n    return 42\n",
    );
}

/// Builds the index the same way a real user would: `vexus index <root>`
/// via `assert_cmd`, under the deterministic mock embedder.
fn index_repo(root: &Path) {
    AssertCommand::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .success();
}

/// Spawns `vexus serve <root>` as a genuine child process and completes a
/// real MCP `initialize` handshake against it, following the exact
/// transport + client-construction pattern rmcp uses for its own
/// `tests/test_with_python.rs`.
async fn connect(root: &Path) -> RunningService<RoleClient, ()> {
    let bin = cargo_bin("vexus");
    let root_arg = root.to_str().unwrap().to_string();
    let transport = TokioChildProcess::new(tokio::process::Command::new(bin).configure(|cmd| {
        cmd.arg("serve")
            .arg(&root_arg)
            .env("VEXUS_EMBEDDER", "mock");
    }))
    .expect("failed to spawn `vexus serve` as a child process");

    ().serve(transport)
        .await
        .expect("MCP initialize handshake with `vexus serve` failed")
}

/// Same as `connect`, but with a caller-chosen `VEXUS_EMBEDDER` for the
/// spawned `vexus serve` process specifically (independent of whatever
/// embedder the repo was originally indexed with) — used by the watcher e2e
/// below, which needs the *serve* process to skip embedding query text
/// entirely (`"none"`) so `search` degrades to pure keyword/FTS matching:
/// with any embedder present, `search_hybrid`'s KNN branch always surfaces
/// *some* (meaningless) nearest chunk once the vector table is nonempty, so
/// a real "no matches" response for a not-yet-existing symbol is only
/// reachable keyword-only (see `vexus-mcp`'s own
/// `search_empty_hits_returns_exact_no_match_text` test, which notes the
/// same thing).
async fn connect_with_embedder(root: &Path, embedder: &str) -> RunningService<RoleClient, ()> {
    let bin = cargo_bin("vexus");
    let root_arg = root.to_str().unwrap().to_string();
    let embedder = embedder.to_string();
    let transport = TokioChildProcess::new(tokio::process::Command::new(bin).configure(|cmd| {
        cmd.arg("serve")
            .arg(&root_arg)
            .env("VEXUS_EMBEDDER", embedder);
    }))
    .expect("failed to spawn `vexus serve` as a child process");

    ().serve(transport)
        .await
        .expect("MCP initialize handshake with `vexus serve` failed")
}

/// Flattens a `CallToolResult`'s text content blocks into one string — every
/// vexus tool replies with a single `String` (rmcp wraps it as one text
/// content block), but joining defensively covers any future multi-block
/// reply too.
fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call_tool(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> String {
    let mut params = CallToolRequestParams::new(name);
    if let Some(arguments) = arguments {
        params = params.with_arguments(arguments);
    }
    let result = client
        .call_tool(params)
        .await
        .unwrap_or_else(|e| panic!("call_tool({name}) failed: {e}"));
    assert_ne!(
        result.is_error,
        Some(true),
        "call_tool({name}) returned an error result: {:?}",
        text_of(&result)
    );
    text_of(&result)
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_e2e_lists_and_drives_all_seven_tools() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_chain_repo(root);
    index_repo(root);

    let client = connect(root).await;

    // `initialize` carried the steering-layer instructions verbatim.
    let peer_info = client
        .peer_info()
        .expect("no server info recorded after initialize");
    let instructions = peer_info.instructions.as_deref().unwrap_or("");
    assert!(
        !instructions.is_empty(),
        "expected non-empty MCP `instructions`"
    );
    assert!(
        instructions.contains("explore"),
        "expected the steering-layer instructions to mention `explore`: {instructions:?}"
    );
    assert_eq!(peer_info.server_info.name, "vexus");

    // Exactly the 7 spec'd tools, each with a non-empty description.
    let tools = client
        .list_all_tools()
        .await
        .expect("tools/list over stdio failed");
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["callees", "callers", "explore", "impact", "open", "search", "status",],
        "expected exactly the 7 documented tools, got: {names:?}"
    );
    for tool in &tools {
        assert!(
            tool.description.as_deref().is_some_and(|d| !d.is_empty()),
            "tool `{}` has an empty description",
            tool.name
        );
    }

    // status: index counts line.
    let status = call_tool(&client, "status", None).await;
    assert!(
        status.contains("index:") && status.contains("files"),
        "expected an index-counts line: {status:?}"
    );

    // search: ranked result line. `contains` (not `starts_with`) tolerates
    // the `⚠ index reconciling ...` freshness header `apply_header` prepends
    // if this call happens to land while the startup reconcile pass (which
    // runs on every `serve`, writer or not) is still in flight — a real,
    // valid state (see the watcher e2e test below, which documents the same
    // tolerance), not a race worth papering over by making `serve` block
    // tool calls until reconcile finishes.
    let mut search_args = serde_json::Map::new();
    search_args.insert("query".to_string(), json!("alpha_process"));
    let search = call_tool(&client, "search", Some(search_args)).await;
    assert!(
        search.contains("1. app.alpha_process"),
        "expected a ranked line: {search:?}"
    );
    assert!(search.contains("app.py"), "got: {search:?}");

    // open: fenced verbatim source.
    let mut open_args = serde_json::Map::new();
    open_args.insert("target".to_string(), json!("app.alpha_process"));
    let open = call_tool(&client, "open", Some(open_args)).await;
    assert!(open.contains("```"), "expected fenced source: {open:?}");
    assert!(open.contains("def alpha_process"), "got: {open:?}");

    // explore: `## <path>` file group header plus fenced source, and the
    // one-hop callee expansion pulling in unique_marker_beta's own body.
    let mut explore_args = serde_json::Map::new();
    explore_args.insert("question".to_string(), json!("alpha_process"));
    let explore = call_tool(&client, "explore", Some(explore_args)).await;
    assert!(
        explore.contains("## app.py"),
        "expected a `## <path>` file group header: {explore:?}"
    );
    assert!(
        explore.contains("```"),
        "expected fenced source: {explore:?}"
    );
    assert!(
        explore.contains("unique_marker_beta"),
        "expected the one-hop callee to be pulled in: {explore:?}"
    );

    client
        .cancel()
        .await
        .expect("clean MCP shutdown of `vexus serve` failed");
}

/// The spec §7 watcher e2e, over the *real* MCP stdio transport (not
/// `vexus-watch`'s own in-process unit test, which drives `update_file` and
/// a bare `Store` directly and never touches a live `vexus serve` process,
/// its reader `AppState`, or the MCP `search`/`status` tools): search for a
/// symbol that doesn't exist yet -> miss; write a new `.py` file defining
/// it; poll `status` until the watcher heals the index to `freshness:
/// fresh` *and* records a non-`none` `last event` (proving the watcher's
/// own drain ran, not just that the file happens to be on disk); search
/// again -> hit. This is the full watch -> update -> query loop.
///
/// This test caught a real bug on first write (see `vexus_mcp::serve_async`
/// in `lib.rs`): the writer thread's shutdown-channel sender was declared
/// *inside* the `if is_writer { ... }` block, so it dropped at that block's
/// closing brace — long before `serve` itself was done. The watcher loop's
/// very first iteration checks the shutdown receiver before it ever touches
/// the `notify` channel, and a disconnected sender reads exactly like an
/// explicit shutdown — so the writer thread exited on its first tick, having
/// never watched anything. Reconcile (which runs before the watch loop)
/// still completed fine, so `status` showed `freshness: fresh` with
/// `last event: none` forever, no matter how long a caller waited. Fixed by
/// hoisting the sender out to `serve_async`'s top level and dropping it
/// explicitly only after `.waiting()` returns.
#[tokio::test(flavor = "multi_thread")]
async fn watcher_e2e_search_miss_write_file_status_heals_then_search_hit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_chain_repo(root);
    index_repo(root);

    // `VEXUS_EMBEDDER=none` for the serve process specifically — see
    // `connect_with_embedder`'s doc comment for why: it's what makes a real
    // "no matches" response reachable at all for a not-yet-existing symbol.
    let client = connect_with_embedder(root, "none").await;

    let mut search_args = serde_json::Map::new();
    search_args.insert("query".to_string(), json!("brand_new_watcher_symbol_zzz"));

    // `contains` (not `starts_with`) tolerates the `⚠ index reconciling ...`
    // freshness header that `apply_header` prepends when this first call
    // happens to land while the startup reconcile pass is still running —
    // a real, valid state (not a race to paper over), and irrelevant to what
    // this assertion actually checks: that the not-yet-existing symbol isn't
    // findable yet.
    let miss = call_tool(&client, "search", Some(search_args.clone())).await;
    assert!(
        miss.contains("no matches"),
        "expected no match before the file defining it exists: {miss:?}"
    );

    // Give the watcher's OS-level watch a moment to register before writing
    // — same generous margin `vexus-watch`'s own watcher unit test uses.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let content = "def brand_new_watcher_symbol_zzz():\n    return 7\n";
    write(root, "watcher_new_file.py", content);

    // Poll `status` until both conditions hold, max 15s (generous: this
    // path crosses a child process's OS-level file watch, a debounce
    // window, an incremental reindex, and a fresh MCP round-trip — more
    // hops than the in-process watcher unit test's 5s budget covers).
    // macOS FSEvents can be slow to deliver a brand-new file's first event;
    // nudge with a rewrite after 3s rather than tightening the budget and
    // risking flakiness.
    let start = Instant::now();
    let deadline = start + Duration::from_secs(15);
    let mut nudged = false;
    let mut last_status = String::new();
    let mut healed = false;
    while Instant::now() < deadline {
        last_status = call_tool(&client, "status", None).await;
        let fresh = last_status.contains("freshness: fresh");
        let has_last_event = !last_status.contains("last event: none");
        if fresh && has_last_event {
            healed = true;
            break;
        }
        if !nudged && start.elapsed() >= Duration::from_secs(3) {
            write(
                root,
                "watcher_new_file.py",
                &format!("{content}    # nudge\n"),
            );
            nudged = true;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(
        healed,
        "status never healed to `freshness: fresh` with a recorded `last event` within 15s: \
         {last_status:?}"
    );

    let hit = call_tool(&client, "search", Some(search_args)).await;
    assert!(
        hit.starts_with("1. "),
        "expected a ranked hit once the watcher caught up: {hit:?}"
    );
    assert!(hit.contains("watcher_new_file.py"), "got: {hit:?}");

    client
        .cancel()
        .await
        .expect("clean MCP shutdown of `vexus serve` failed");
}

/// A repo of `n` trivially small files — used only to give a `vexus serve`
/// winning the advisory lock a non-instant first index build, so the
/// process spawned right after it has a real (not just theoretical) chance
/// of starting before `index.db` exists at all.
fn write_many_file_repo(root: &Path, n: usize) {
    for i in 0..n {
        write(
            root,
            &format!("f{i}.py"),
            &format!("def fn_{i}():\n    return {i}\n"),
        );
    }
}

/// Finding C3 regression: a `vexus serve` that loses the advisory writer
/// lock race to another `vexus serve` process — one that is *still
/// building its very first index*, with no `index.db` on disk at all yet —
/// used to fail its own startup `open_reader_with_probe` call with a bare
/// `?` and die before ever completing the MCP `initialize` handshake (see
/// `vexus_mcp::lib::serve_async`'s doc comments). No `vexus index` run
/// happens first here — nothing on disk at all — and the winner gets a
/// deliberately non-trivial fixture so the second process has a real shot
/// at starting while `index.db` still doesn't exist. Whether or not that
/// exact race actually lands on a given run, this proves the loser's
/// `serve` always comes up (real MCP handshake, real `status` call) and
/// always eventually reports the winner's completed index — never dies,
/// never gets stuck.
#[tokio::test(flavor = "multi_thread")]
async fn loser_serve_survives_racing_the_winners_first_index_build() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_many_file_repo(root, 200);
    assert!(!root.join(".vexus/index.db").exists());

    let winner = connect_with_embedder(root, "mock").await;
    // Spawned immediately after, with no delay: the loser races the
    // winner's still-in-progress first index build.
    let loser = connect_with_embedder(root, "mock").await;

    // Both processes having already completed a real MCP `initialize`
    // handshake (`connect_with_embedder` panics on failure) is itself most
    // of what this test proves — the pre-fix bug meant the loser's child
    // process could exit before that handshake ever finished. A `status`
    // call must also succeed either way: a real report, or (finding C3's
    // fallback) the "not ready yet" text — never a dead connection.
    let status = call_tool(&loser, "status", None).await;
    assert!(
        status.contains("index:") || status.contains("index not ready"),
        "loser must answer status — a real report or 'not ready' — not die: {status:?}"
    );

    // Whichever branch it started in, the loser must catch up once the
    // winner's first index finishes — proving `status: None` (if it ever
    // was) self-heals rather than serving a permanently empty/stuck state.
    let start = Instant::now();
    let deadline = start + Duration::from_secs(10);
    let mut caught_up = false;
    let mut last = String::new();
    while Instant::now() < deadline {
        last = call_tool(&loser, "status", None).await;
        if last.contains("index: ") && !last.contains("index: 0 files") {
            caught_up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        caught_up,
        "loser never saw the winner's completed first index within 10s: {last:?}"
    );

    winner
        .cancel()
        .await
        .expect("winner clean MCP shutdown failed");
    loser
        .cancel()
        .await
        .expect("loser clean MCP shutdown failed");
}

/// Dogfood: point the same real-stdio harness at the vexus repo itself and
/// ask a real question ("how are edges resolved to symbols") that should
/// pull in `crates/vexus-core/src/resolve.rs`. Requires `vexus index .` to
/// have already been run against this repo with `VEXUS_EMBEDDER=mock`
/// (`cargo run -p vexus-cli -- index .`) — `#[ignore]`d so `cargo test
/// --workspace` stays hermetic (temp-dir-only) and deterministic; run
/// explicitly with `cargo test -p vexus-cli --test mcp_e2e -- --ignored
/// dogfood --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "dogfood: run manually against the real repo root after `VEXUS_EMBEDDER=mock cargo run -p vexus-cli -- index .`"]
async fn dogfood_explore_how_edges_resolve_to_symbols() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    assert!(
        root.join(".vexus/index.db").exists(),
        "expected an existing index at {root:?}/.vexus/index.db — run \
         `VEXUS_EMBEDDER=mock cargo run -p vexus-cli -- index .` from the repo root first"
    );

    let client = connect(root).await;

    let mut args = serde_json::Map::new();
    args.insert(
        "question".to_string(),
        json!("how are edges resolved to symbols"),
    );
    let explore = call_tool(&client, "explore", Some(args)).await;

    println!("--- dogfood explore output ---\n{explore}");

    assert!(
        explore.contains("resolve.rs"),
        "expected resolve.rs to be pulled into the bundle: {explore:?}"
    );
    assert!(
        explore.contains("## "),
        "expected file group headers: {explore:?}"
    );
    assert!(
        explore.contains("```"),
        "expected fenced source: {explore:?}"
    );

    client
        .cancel()
        .await
        .expect("clean MCP shutdown of `vexus serve` failed");
}
