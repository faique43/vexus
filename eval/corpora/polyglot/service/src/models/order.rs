//! The `Order` record: a customer's purchase in progress.

/// A customer's purchase, tracked from placement through fulfillment.
pub struct Order {
    pub id: String,
    pub total_cents: u64,
}

impl Order {
    /// Construct a new order for the given total.
    ///
    /// Retrieval-challenge note: `models::user::User` also defines an
    /// associated function named `new` with the same arity (two params) —
    /// vexus's indexer doesn't nest associated functions under their `impl`
    /// block (only free-standing definitions are captured), so both `new`s
    /// are indistinguishable flat symbols by name+arity alone. See
    /// `eval/edges/polyglot.yaml` for the heuristic-limit case this produces.
    pub fn new(id: String, total_cents: u64) -> Self {
        Self { id, total_cents }
    }
}
