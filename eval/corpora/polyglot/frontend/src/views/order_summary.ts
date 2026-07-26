/** Renders a plain-text order summary (the real UI layer swaps this for JSX). */

import { formatDate, formatMoney } from "../utils/format";

export interface OrderSummaryData {
    orderId: string;
    totalCents: number;
    itemCount: number;
    placedAt: string;
}

/** Render a one-line summary of an order, for display or email. */
export function renderOrderSummary(data: OrderSummaryData): string {
    return `Order ${data.orderId} (${formatDate(data.placedAt)}): ${data.itemCount} item(s), ${formatMoney(data.totalCents)}`;
}
