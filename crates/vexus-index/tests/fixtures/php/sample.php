<?php
namespace Shop\Billing;

use Shop\Util\Slug;

const TAX_RATE = 8;

// Formats a cent amount for display.
function format_cents(int $cents): string {
    return "$" . strval($cents / 100);
}

class Invoice {
    private array $lines;

    public function __construct(array $lines) {
        $this->lines = $lines;
    }

    // Total with tax.
    public function total(int $tax): int {
        return $this->subtotal() + $tax;
    }

    private function subtotal(): int {
        return array_sum($this->lines);
    }
}

interface Renderable {
    public function render(): string;
}

trait Loggable {}

enum Status {
    case Open;
    case Paid;
}
