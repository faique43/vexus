//! Service entrypoint: boots the app and exercises the primary request flows.

mod handlers;
mod models;
mod services;
mod utils;

use handlers::orders::create_order_endpoint;
use handlers::users::register_user_endpoint;

/// Boot the service (in production this would bind an HTTP listener).
fn main() {
    let user_id = register_user_endpoint("new.customer@example.com", "New Customer");
    let order_id = create_order_endpoint(&user_id, vec![("sku-1".to_string(), 2)]);
    println!("booted service, placed order {order_id} for user {user_id}");
}
