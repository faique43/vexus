//! The `ShipmentStatus` record: where a parcel is on its way to the customer.

/// Where a parcel is between the warehouse and the customer's door.
pub enum ShipmentStatus {
    AwaitingPickup,
    InTransit { carrier: String },
    OutForDelivery,
    Delivered,
    ReturnedToSender,
}
