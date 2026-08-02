<?php
namespace Shop\Legacy;

// Legacy coupon validation kept for the old storefront.
function coupon_discount_cents(string $code, int $subtotal_cents): int {
    if (!coupon_is_active($code)) {
        return 0;
    }
    return intdiv($subtotal_cents, 10);
}

function coupon_is_active(string $code): bool {
    return str_starts_with($code, "SAVE");
}
