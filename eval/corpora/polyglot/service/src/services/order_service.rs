//! Order lifecycle: placing, paying, and cancelling orders.

use crate::models::order::Order;
use crate::services::notifications::{notify_order_cancelled, notify_order_placed};
use crate::utils::ids::generate_id;
use crate::utils::retry::retry_with_backoff;

/// Place a new order for `customer_id` at the already-quoted `total_cents`.
pub fn place_order(customer_id: &str, items: Vec<(String, u32)>, total_cents: u64) -> String {
    let order_id = generate_id("ord");
    let order = Order::new(order_id.clone(), total_cents);
    retry_with_backoff(|| persist_order(&order));
    notify_order_placed(customer_id, &order_id);
    let _ = items;
    order_id
}

/// Mark an order as paid once its webhook confirmation arrives.
pub fn mark_order_paid(order_id: &str) -> bool {
    persist_status(order_id, "paid")
}

/// Cancel an order that hasn't shipped yet.
pub fn cancel_order(customer_id: &str, order_id: &str) -> bool {
    let cancelled = persist_status(order_id, "cancelled");
    notify_order_cancelled(customer_id, order_id);
    cancelled
}

fn persist_order(order: &Order) -> bool {
    order.total_cents > 0
}

fn persist_status(order_id: &str, status: &str) -> bool {
    !order_id.is_empty() && !status.is_empty()
}
