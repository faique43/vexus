//! Handlers for inbound webhook events from the payment processor.

use crate::services::auth::validate;
use crate::services::order_service::mark_order_paid;

/// Handle an inbound "payment succeeded" webhook event, gated on a valid
/// signing token.
///
/// Retrieval-challenge note: `validate` is a third bare call to that name in
/// this corpus — `utils::validation::validate` (sku format) and this
/// module's intended target, `services::auth::validate` (session token), are
/// both same-arity candidates. See `eval/edges/polyglot.yaml`.
pub fn handle_payment_succeeded(order_id: &str, token: &str) -> bool {
    if !validate(token) {
        return false;
    }
    mark_order_paid(order_id)
}
