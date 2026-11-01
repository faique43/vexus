//! The rmcp `ServerHandler` implementation: tool registration + the
//! steering-layer `instructions` text served on `initialize`.
//!
//! rmcp 3.0.0-beta.2 API notes (the `rmcp = "0.3"` line on crates.io is
//! stale — the crate is at `3.0.0-beta.2`): tool methods live in an
//! `impl` block annotated
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
use crate::tools::{explore, graph, open};

// Serde `alias` attributes below accept the wrong-but-predictable param
// names agents actually send (field report: a first session called `explore`
// with `query`, got "missing field `question`", and fell back to grep for
// good). Aliases are deserialize-only — the published JSON schema still
// names exactly one canonical field per param.

/// Params for the `open` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct OpenParams {
    /// A symbol name or `path:start-end`.
    #[serde(alias = "symbol", alias = "path")]
    target: String,
    /// Default 6000.
    #[serde(alias = "budget")]
    budget_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum TraceDirection {
    Callers,
    Callees,
    Impact,
}

/// Params for the `trace` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct TraceParams {
    /// A symbol name.
    #[serde(alias = "target", alias = "name")]
    symbol: String,
    direction: TraceDirection,
    /// Default 1 for callers and callees, 5 for impact.
    #[serde(alias = "max_depth")]
    depth: Option<u32>,
    #[schemars(skip)]
    #[serde(alias = "budget")]
    budget_tokens: Option<u32>,
}

/// Params for the `explore` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ExploreParams {
    /// A question, or symbol names.
    #[serde(alias = "query", alias = "q")]
    question: String,
    #[schemars(skip)]
    #[serde(alias = "budget")]
    budget_tokens: Option<u32>,
}

/// Steering layer 1: the `instructions` text served on `initialize`.
const INSTRUCTIONS: &str = "This repo is indexed. For questions about the code, `explore` for a ranked map, then `open` only the ranges you need. `trace` follows calls. Use grep for exact strings, comments and config.";

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
    #[tool(description = "Index freshness and counts.")]
    async fn status(&self) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || state.status_text()).await {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => format!("status error: {e:#}"),
            Err(e) => format!("status error: tool task panicked ({e})"),
        }
    }

    #[tool(
        description = "Map the code behind a question: ranked locations, signatures and call links, not source. Start here, then `open` what you need."
    )]
    async fn explore(&self, Parameters(params): Parameters<ExploreParams>) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || {
            explore::explore_text(&state, &params.question, params.budget_tokens)
        })
        .await
        {
            Ok(text) => text,
            Err(e) => format!("explore error: tool task panicked ({e})"),
        }
    }

    #[tool(description = "Exact source of a symbol or a `path:start-end` range.")]
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

    #[tool(description = "Follow a symbol's callers, callees, or what a change would impact.")]
    async fn trace(&self, Parameters(params): Parameters<TraceParams>) -> String {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || match params.direction {
            TraceDirection::Callers => {
                graph::callers_text(&state, &params.symbol, params.depth, params.budget_tokens)
            }
            TraceDirection::Callees => {
                graph::callees_text(&state, &params.symbol, params.depth, params.budget_tokens)
            }
            TraceDirection::Impact => {
                graph::impact_text(&state, &params.symbol, params.depth, params.budget_tokens)
            }
        })
        .await
        {
            Ok(text) => text,
            Err(e) => format!("trace error: tool task panicked ({e})"),
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
            store: Mutex::new(Some(store)),
            embedder: OnceLock::new(),
            root: root.to_path_buf(),
            last_generation: std::sync::atomic::AtomicU64::new(0),
            is_writer: true,
        });
        let server = VexusServer::new(state);
        let info = server.get_info();
        assert_eq!(info.instructions.as_deref(), Some(INSTRUCTIONS));
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "vexus");
    }
}
