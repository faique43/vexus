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
/// strategy (a flat 0-cent discount by default).
///
/// Note: `discount` is a `Box<dyn Discount>`, so `discount.apply(...)`
/// below is trait-object dispatch — whether it lands in
/// `PercentageDiscount::apply` or `FlatDiscount::apply` is decided at
/// runtime by whatever gets boxed above, not by anything you can read off
/// this call site.
pub fn quote_total(items: &[(String, u32)]) -> u64 {
    let subtotal = base_price(items);
    let discount: Box<dyn Discount> = Box::new(PercentageDiscount { percent_off: 0 });
    discount.apply(subtotal)
}

fn base_price(items: &[(String, u32)]) -> u64 {
    items.iter().map(|(_, qty)| (*qty as u64) * 500).sum()
}
