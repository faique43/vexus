//! Order-lifecycle notification delivery.

/// Notify the customer that their order was placed successfully.
pub fn notify_order_placed(customer_id: &str, order_id: &str) {
    send(customer_id, &format!("Order {order_id} placed"));
}

/// Notify the customer that their order was cancelled.
pub fn notify_order_cancelled(customer_id: &str, order_id: &str) {
    send(customer_id, &format!("Order {order_id} cancelled"));
}

fn send(customer_id: &str, message: &str) {
    println!("to {customer_id}: {message}");
}
