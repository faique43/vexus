//! Handlers for inbound webhook events from the payment processor.

use crate::services::auth::validate;
use crate::services::order_service::mark_order_paid;

/// Handle an inbound "payment succeeded" webhook event, gated on a valid
/// signing token.
///
/// Note: this means `services::auth::validate` (session token) — don't
/// confuse it with `utils::validation::validate` (sku format), a different
/// check with the same name.
pub fn handle_payment_succeeded(order_id: &str, token: &str) -> bool {
    if !validate(token) {
        return false;
    }
    mark_order_paid(order_id)
}
