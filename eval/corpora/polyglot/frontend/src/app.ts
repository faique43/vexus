/** Frontend entrypoint: wires the checkout flow together. */

import { placeOrder } from "./api/orders";
import { AppConfig, loadConfig } from "./config";
import { addItem, cartItemCount } from "./state/cart";
import { renderOrderSummary } from "./views/order_summary";

/** Load the frontend's runtime configuration at startup. */
export function bootstrap(): AppConfig {
    return loadConfig();
}

/** Run the checkout flow for the current cart. */
export async function checkout(customerId: string): Promise<string> {
    addItem("sku-1", 2);
    const itemCount = cartItemCount();
    await placeOrder(customerId, [{ sku: "sku-1", qty: 2 }], `${customerId}-cart`);
    return renderOrderSummary({
        orderId: "pending",
        totalCents: 0,
        itemCount,
        placedAt: new Date().toISOString(),
    });
}
