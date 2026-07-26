/** Shopping-cart state built on top of the generic `Store`. */

import { Store } from "./store";
import { validate } from "../utils/validation";

export interface CartItem {
    sku: string;
    qty: number;
}

export const cartStore = new Store<CartItem[]>([]);

/** Add an item to the cart, merging quantities if the sku is already present.
 *
 * Note: `validate` here is this module's own local sku format check — the
 * backend has its own, separate `validate` for session tokens and another
 * for sku format server-side; don't assume this one covers those.
 */
export function addItem(sku: string, qty: number): void {
    if (!validate(sku)) {
        throw new Error(`invalid sku: ${sku}`);
    }
    const items = cartStore.getState();
    const existing = items.find((item) => item.sku === sku);
    if (existing) {
        existing.qty += qty;
    } else {
        items.push({ sku, qty });
    }
    cartStore.setState(items);
}

/** Remove an item from the cart entirely. */
export function removeItem(sku: string): void {
    const items = cartStore.getState().filter((item) => item.sku !== sku);
    cartStore.setState(items);
}

/** Compute the cart's total quantity across all line items. */
export function cartItemCount(): number {
    return cartStore.getState().reduce((sum, item) => sum + item.qty, 0);
}
