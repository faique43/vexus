//! Pure, MCP-transport-independent implementations backing the `search` and
//! `open` tools. Each submodule exposes a plain function that takes `&AppState`
//! plus the tool's params and returns a `String` — directly unit-testable
//! without going through rmcp. `server.rs` wires these up behind `#[tool]`
//! wrappers (schemars param struct, `spawn_blocking`), following the same
//! shape as the existing `status` tool.

pub mod explore;
pub mod graph;
pub mod open;
pub mod search;

use vexus_core::query::{Resolution, SymbolInfo};
use vexus_core::Store;

use crate::format::render_candidates;
use crate::state::AppState;

/// Hard ceiling on `budget_tokens` regardless of what a caller requests —
/// a client-supplied budget of e.g. `u32::MAX` must not translate into an
/// attempt to stuff the entire repo into one response.
pub(crate) const MAX_BUDGET_TOKENS: u32 = 20_000;

/// Resolve the effective token budget: the caller's request (or `default`
/// if none given), clamped to `MAX_BUDGET_TOKENS`.
pub(crate) fn clamp_budget(requested: Option<u32>, default: u32) -> u32 {
    requested.unwrap_or(default).min(MAX_BUDGET_TOKENS)
}

/// Shared resolve step for every tool that needs an unambiguous symbol:
/// `Exact` passes through as `Ok`; `Candidates`/`NotFound` render as the same
/// guidance text `open` has always shown (candidates: locations + "narrow
/// with the full qualname"; not found: nearest-name suggestions, or a plain
/// "no symbol found" when there are none) and come back as `Err(text)` — a
/// tool can just return that string directly.
pub(crate) fn resolve_or_text(store: &Store, target: &str) -> Result<SymbolInfo, String> {
    let resolution = store
        .resolve_symbol(target)
        .map_err(|e| format!("resolve error: {e:#}"))?;
    match resolution {
        Resolution::Exact(info) => Ok(info),
        Resolution::Candidates(candidates) => {
            let mut out = render_candidates(&candidates);
            out.push_str("\nAmbiguous — narrow with the full qualname.\n");
            Err(out)
        }
        Resolution::NotFound { suggestions } => Err(if suggestions.is_empty() {
            format!("no symbol found for \"{target}\".")
        } else {
            format!(
                "no symbol found for \"{target}\" — did you mean: {}?",
                suggestions.join(", ")
            )
        }),
    }
}

/// Shared embed-query step for every tool that feeds `search_hybrid`:
/// embeds `query` via `state.embedder()` when one is available AND its
/// model matches what the index was actually built with (checked against
/// `meta` while briefly locking the store, the same model-mismatch guard
/// `vexus-cli`'s `Cmd::Search` uses — a mismatched vector would otherwise
/// blow up `search_hybrid`'s KNN lookup against `vec_chunks`' declared
/// width). No embedder, or a mismatch, returns `None` so the caller
/// degrades to keyword-only search rather than failing.
///
/// Callers MUST call this *before* taking their own store lock for
/// `search_hybrid`: the model-guard check only needs the store held for
/// two `meta` reads, which this function does and releases internally —
/// but the embed itself can be a real (slow, possibly ONNX) inference call,
/// and must run unlocked so it doesn't stall every other tool call for the
/// duration.
pub(crate) fn embed_query(state: &AppState, query: &str) -> Option<Vec<f32>> {
    let embedder = state.embedder()?;
    let same_model = {
        let store = state.store.lock().expect("store mutex poisoned");
        let indexed_id = store.meta("model_id").ok().flatten();
        let indexed_dim = store.meta("model_dim").ok().flatten();
        indexed_id.as_deref() == Some(embedder.id())
            && indexed_dim.as_deref() == Some(embedder.dim().to_string().as_str())
    };
    if !same_model {
        return None;
    }
    embedder.embed(&[query]).ok().and_then(|mut v| v.pop())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_budget_uses_default_when_none() {
        assert_eq!(clamp_budget(None, 4000), 4000);
    }

    #[test]
    fn clamp_budget_passes_through_requests_under_the_cap() {
        assert_eq!(clamp_budget(Some(1000), 4000), 1000);
    }

    #[test]
    fn clamp_budget_caps_absurd_requests() {
        assert_eq!(clamp_budget(Some(u32::MAX), 4000), MAX_BUDGET_TOKENS);
        assert_eq!(clamp_budget(Some(50_000), 4000), MAX_BUDGET_TOKENS);
    }
}
