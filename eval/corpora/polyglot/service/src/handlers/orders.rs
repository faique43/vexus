//! HTTP-adjacent handlers for order endpoints.

use crate::services::order_service::{cancel_order, place_order};
use crate::services::pricing::quote_total;
use crate::utils::validation::validate;

/// Handle a "create order" request: validate the cart, quote the total,
/// then place the order.
///
/// Note: this calls `utils::validation::validate` (sku format) —
/// `services::auth::validate` (session tokens) is a different function
/// with the same name, unrelated to this check.
pub fn create_order_endpoint(customer_id: &str, items: Vec<(String, u32)>) -> String {
    for (sku, _qty) in &items {
        if !validate(sku) {
            panic!("invalid sku: {sku}");
        }
    }
    let total_cents = quote_total(&items);
    place_order(customer_id, items, total_cents)
}

/// Handle a "cancel order" request.
pub fn cancel_order_endpoint(customer_id: &str, order_id: &str) -> bool {
    cancel_order(customer_id, order_id)
}
