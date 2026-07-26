/** Display-formatting helpers for money and dates. */

/** Format a whole-cent amount as a localized currency string. */
export function formatMoney(cents: number): string {
    return `$${(cents / 100).toFixed(2)}`;
}

/** Format an ISO-8601 timestamp as a short human-readable date. */
export function formatDate(iso: string): string {
    const d = new Date(iso);
    return d.toLocaleDateString();
}
