//! Price quoting: applies a configured discount strategy to a cart's items.

/// A pluggable discount strategy applied to an order's subtotal.
pub trait Discount {
    /// Apply this discount to `amount_cents`, returning the discounted total.
    fn apply(&self, amount_cents: u64) -> u64;
}

/// A discount expressed as a percentage off the subtotal.
pub struct PercentageDiscount {
    pub percent_off: u64,
}

impl Discount for PercentageDiscount {
    fn apply(&self, amount_cents: u64) -> u64 {
        amount_cents - (amount_cents * self.percent_off / 100)
    }
}

/// A discount expressed as a flat amount off the subtotal.
pub struct FlatDiscount {
    pub amount_off_cents: u64,
}

impl Discount for FlatDiscount {
    fn apply(&self, amount_cents: u64) -> u64 {
        amount_cents.saturating_sub(self.amount_off_cents)
    }
}

/// Quote the total price for `items`, applying the configured discount
/// strategy (a flat 0-cent discount by default, in this fixture).
///
/// Retrieval-challenge note: `discount` is a `Box<dyn Discount>` — the
/// `discount.apply(...)` call below is trait-object dispatch, so a static
/// call-graph can't know at this call site whether it lands in
/// `PercentageDiscount::apply` or `FlatDiscount::apply`. See
/// `eval/edges/polyglot.yaml` for the heuristic-limit case this produces.
pub fn quote_total(items: &[(String, u32)]) -> u64 {
    let subtotal = base_price(items);
    let discount: Box<dyn Discount> = Box::new(PercentageDiscount { percent_off: 0 });
    discount.apply(subtotal)
}

fn base_price(items: &[(String, u32)]) -> u64 {
    items.iter().map(|(_, qty)| (*qty as u64) * 500).sum()
}
