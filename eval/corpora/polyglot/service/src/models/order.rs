//! The `Order` record: a customer's purchase in progress.

/// A customer's purchase, tracked from placement through fulfillment.
pub struct Order {
    pub id: String,
    pub total_cents: u64,
}

impl Order {
    /// Construct a new order for the given total.
    ///
    /// Note: `User::new` takes the same two-argument shape as this one —
    /// same name, different type. Easy to grab the wrong `new` if your
    /// editor's "go to definition" isn't scoped by type.
    pub fn new(id: String, total_cents: u64) -> Self {
        Self { id, total_cents }
    }
}
