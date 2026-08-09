//! End-to-end order lifecycle exercised through the public service API.

use service::services::order_service::{cancel_order, mark_order_paid, place_order};

#[test]
fn order_lifecycle_place_pay_cancel() {
    let id = place_order("cus_it", vec![("sku_it".to_string(), 3)], 1500);
    assert!(mark_order_paid(&id));
    assert!(cancel_order("cus_it", &id));
}

#[test]
fn cancelling_an_unknown_order_still_notifies() {
    assert!(cancel_order("cus_it", "ord_missing"));
}
