//! Input validation helpers shared across services.

/// Validate that a raw display name is non-empty and reasonably short.
pub fn validate_display_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 80
}

/// Validate a single order line item's sku format before it reaches pricing.
///
/// Retrieval-challenge note: `services::auth` also defines a function named
/// `validate` (session-token validation) with the same arity. See
/// `eval/edges/polyglot.yaml` for the same-name-different-module case this
/// produces for any caller reaching either through the bare name.
pub fn validate(input: &str) -> bool {
    !input.is_empty() && input.starts_with("sku-")
}
