import Foundation

// Formats the price badge shown on iOS product cards.
struct PriceBadge {
    let cents: Int

    // "$12.99"-style display text.
    func displayText() -> String {
        let dollars = cents / 100
        let rem = cents % 100
        return "$\(dollars).\(String(format: "%02d", rem))"
    }
}

// Badge text with the sale marker when a discount applies.
func saleBadgeText(badge: PriceBadge, onSale: Bool) -> String {
    let base = badge.displayText()
    return onSale ? "SALE " + base : base
}
