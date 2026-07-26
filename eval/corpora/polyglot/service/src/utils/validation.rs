//! Input validation helpers shared across services.

/// Validate that a raw display name is non-empty and reasonably short.
pub fn validate_display_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 80
}

/// Validate a single order line item's sku format before it reaches pricing.
///
/// Note: `services::auth` also has a `validate`, for session tokens — same
/// name, unrelated job. Don't reach for that one when you mean this.
pub fn validate(input: &str) -> bool {
    !input.is_empty() && input.starts_with("sku-")
}
