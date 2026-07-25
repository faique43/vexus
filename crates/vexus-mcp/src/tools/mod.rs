//! Pure, MCP-transport-independent implementations backing the `search` and
//! `open` tools. Each submodule exposes a plain function that takes `&AppState`
//! plus the tool's params and returns a `String` — directly unit-testable
//! without going through rmcp. `server.rs` wires these up behind `#[tool]`
//! wrappers (schemars param struct, `spawn_blocking`), following the same
//! shape as the existing `status` tool.

pub mod open;
pub mod search;

/// Hard ceiling on `budget_tokens` regardless of what a caller requests —
/// a client-supplied budget of e.g. `u32::MAX` must not translate into an
/// attempt to stuff the entire repo into one response.
pub(crate) const MAX_BUDGET_TOKENS: u32 = 20_000;

/// Resolve the effective token budget: the caller's request (or `default`
/// if none given), clamped to `MAX_BUDGET_TOKENS`.
pub(crate) fn clamp_budget(requested: Option<u32>, default: u32) -> u32 {
    requested.unwrap_or(default).min(MAX_BUDGET_TOKENS)
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
