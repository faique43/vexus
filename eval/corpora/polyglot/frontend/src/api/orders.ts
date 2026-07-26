/** Typed API calls for the order endpoints. */

import { request } from "./client";
import { markIfNew } from "../utils/idempotency";

export interface CartLine {
    sku: string;
    qty: number;
}

/** Fetch a single order by id. */
export function fetchOrder(orderId: string): Promise<unknown> {
    return request(`/api/orders/${orderId}`, { method: "GET" });
}

/** Place a new order for the given cart items, guarding against an
 * accidental double-submit (e.g. a double-click or a network retry)
 * resending the same idempotency key.
 */
export function placeOrder(
    customerId: string,
    items: CartLine[],
    idempotencyKey: string,
): Promise<unknown> {
    if (!markIfNew(idempotencyKey)) {
        return Promise.reject(new Error("duplicate order submission"));
    }
    return request("/api/orders", { method: "POST", body: { customerId, items } });
}
