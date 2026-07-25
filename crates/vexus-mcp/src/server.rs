//! The rmcp `ServerHandler` implementation: tool registration + the
//! steering-layer `instructions` text served on `initialize`.
//!
//! rmcp 3.0.0-beta.2 API notes (the version resolved from the brief's
//! `rmcp = "0.3"`, which is stale on crates.io — the crate is at
//! `3.0.0-beta.2`): tool methods live in an `impl` block annotated
//! `#[tool_router]`; `ServerHandler` is wired up with `#[tool_handler]` on
//! its `impl` block, which auto-generates `call_tool`/`list_tools` from the
//! router but leaves a hand-written `get_info()` alone if one is present
//! (verified against `rmcp`'s own `tests/test_tool_macros.rs::ManualInfoServer`
//! pattern) — used here to set `instructions` exactly as specified.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::state::AppState;
use crate::tools::{graph, open, search};

/// Params for the `search` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct SearchParams {
    /// Free-text query — words, a symbol name, or a short question.
    query: String,
    /// Max results to return (default 10).
    #[schemars(description = "Max results to return (default 10).")]
    limit: Option<u32>,
    /// Token budget for the rendered result list (default 4000, capped at 20000).
    #[schemars(
        description = "Token budget for the rendered result list (default 4000, capped at 20000)."
    )]
    budget_tokens: Option<u32>,
}

/// Params for the `open` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct OpenParams {
    /// A symbol qualname/name (e.g. `app.util.slug`) or a `path:start-end` line range (e.g. `src/app.py:10-40`).
    target: String,
    /// Token budget for the rendered source (default 6000, capped at 20000).
    #[schemars(
        description = "Token budget for the rendered source (default 6000, capped at 20000)."
    )]
    budget_tokens: Option<u32>,
}

/// Params for the `callers`/`callees` tools.
#[derive(Debug, Deserialize, JsonSchema)]
struct CallGraphParams {
    /// A symbol qualname/name (e.g. `app.util.slug`).
    symbol: String,
    /// Traversal depth (default 1, max 3).
    #[schemars(description = "Traversal depth (default 1, max 3).")]
    depth: Option<u32>,
    /// Token budget for the rendered edge tree (default 4000, capped at 20000).
    #[schemars(
        description = "Token budget for the rendered edge tree (default 4000, capped at 20000)."
    )]
    budget_tokens: Option<u32>,
}

/// Params for the `impact` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ImpactParams {
    /// A symbol qualname/name (e.g. `app.util.slug`).
    symbol: String,
    /// Traversal depth (default 5, max 10).
    #[schemars(description = "Traversal depth (default 5, max 10).")]
    max_depth: Option<u32>,
}

/// Steering layer 1 (see plan doc §5): shipped verbatim per the Task 3 brief.
const INSTRUCTIONS: &str =
    "Vexus is a pre-built semantic + structural index of this repository. Reads are
milliseconds. Consult it BEFORE grep/find/read when looking for code.

- Any question about the code (\"how does X work\", \"where is X\", \"what is the flow
  from A to B\") → `explore` — ONE call returns the relevant verbatim source, token-
  budgeted and grouped by file. It is usually the only call you need.
- Find a symbol/snippet by words → `search`. Fetch a known symbol/file range → `open`.
- Trace relationships → `callers` / `callees` / `impact`.
- Results wrong or missing? → `status` shows index freshness and coverage.

Use grep only for what an index can't answer: exact string/regex hunts, comments,
config values, generated files.";

#[derive(Clone)]
pub struct VexusServer {
    state: Arc<AppState>,
    // Read only from the code the `#[tool_handler]` macro generates (the
    // `ServerHandler::call_tool`/`list_tools` it wires up), which rustc's
    // dead-code analysis doesn't trace through macro expansion for — same
    // false positive upstream silences with `#![allow(dead_code)]` in
    // rmcp's own `tests/test_tool_macros.rs`.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl VexusServer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl VexusServer {
    #[tool(
        description = "Shows index freshness and coverage: file/symbol/edge/chunk counts, the embedding model and its backlog, and vector-search availability. Use this when results seem wrong or missing."
    )]
    async fn status(&self) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || state.status_text()).await {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => format!("status error: {e:#}"),
            Err(e) => format!("status error: tool task panicked ({e})"),
        }
    }

    #[tool(
        description = "Hybrid semantic+keyword search over the code index. Returns ranked symbol locations with excerpts. Cheaper than explore; use explore when you want the actual source."
    )]
    async fn search(&self, Parameters(params): Parameters<SearchParams>) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || {
            search::search_text(&state, &params.query, params.limit, params.budget_tokens)
        })
        .await
        {
            Ok(text) => text,
            Err(e) => format!("search error: tool task panicked ({e})"),
        }
    }

    #[tool(
        description = "Fetch the verbatim source of a symbol (by qualname or name) or an exact file line range. Replaces reading whole files."
    )]
    async fn open(&self, Parameters(params): Parameters<OpenParams>) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || {
            open::open_text(&state, &params.target, params.budget_tokens)
        })
        .await
        {
            Ok(text) => text,
            Err(e) => format!("open error: tool task panicked ({e})"),
        }
    }

    #[tool(
        description = "Who calls this symbol (transitive with depth). Confidence-annotated; 'unresolved' rows are name-only heuristic matches."
    )]
    async fn callers(&self, Parameters(params): Parameters<CallGraphParams>) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || {
            graph::callers_text(&state, &params.symbol, params.depth, params.budget_tokens)
        })
        .await
        {
            Ok(text) => text,
            Err(e) => format!("callers error: tool task panicked ({e})"),
        }
    }

    #[tool(
        description = "Who this symbol calls (transitive with depth). Confidence-annotated; 'unresolved' rows are name-only heuristic matches."
    )]
    async fn callees(&self, Parameters(params): Parameters<CallGraphParams>) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || {
            graph::callees_text(&state, &params.symbol, params.depth, params.budget_tokens)
        })
        .await
        {
            Ok(text) => text,
            Err(e) => format!("callees error: tool task panicked ({e})"),
        }
    }

    #[tool(
        description = "Transitive blast radius of changing a symbol — every caller chain that reaches it, plus import dependents."
    )]
    async fn impact(&self, Parameters(params): Parameters<ImpactParams>) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || {
            graph::impact_text(&state, &params.symbol, params.max_depth)
        })
        .await
        {
            Ok(text) => text,
            Err(e) => format!("impact error: tool task panicked ({e})"),
        }
    }
}

#[tool_handler]
impl ServerHandler for VexusServer {
    fn get_info(&self) -> ServerInfo {
        // Without this, `ServerInfo::new`'s default `server_info` bakes in
        // rmcp's own crate name/version (`Implementation::from_build_env()`
        // expands `env!(...)` at *rmcp's* compile time, not ours) — clients
        // would show "rmcp 3.0.0-beta.2" as the connected server's identity
        // instead of vexus.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
            .with_server_info(rmcp::model::Implementation::new(
                "vexus",
                env!("CARGO_PKG_VERSION"),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn write(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn get_info_carries_the_verbatim_instructions_and_tools_capability() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def f():\n    return 1\n");
        let store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        let state = Arc::new(AppState {
            store: Mutex::new(store),
            embedder: OnceLock::new(),
            root: root.to_path_buf(),
        });
        let server = VexusServer::new(state);
        let info = server.get_info();
        assert_eq!(info.instructions.as_deref(), Some(INSTRUCTIONS));
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "vexus");
    }
}
