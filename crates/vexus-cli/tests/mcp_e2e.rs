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
//! binary lives in this crate (see the Task 8 brief).
//!
//! rmcp 3.0.0-beta.2 client API notes (this crate's `rmcp` client feature is
//! stale on crates.io at "0.3" per earlier plan docs; the resolved version is
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
        "expected exactly the 7 tools from the spec, got: {names:?}"
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

    // search: ranked result line.
    let mut search_args = serde_json::Map::new();
    search_args.insert("query".to_string(), json!("alpha_process"));
    let search = call_tool(&client, "search", Some(search_args)).await;
    assert!(
        search.starts_with("1. "),
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
