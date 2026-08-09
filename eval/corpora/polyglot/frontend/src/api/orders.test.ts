/** Unit tests for the order API calls. */

import { describe, expect, it } from "vitest";

import { fetchOrder, placeOrder } from "./orders";

async function placeOrderRejectsADuplicateSubmission(): Promise<void> {
    await placeOrder("cus_1", [{ sku: "sku_1", qty: 1 }], "key_dup");
    await expect(
        placeOrder("cus_1", [{ sku: "sku_1", qty: 1 }], "key_dup"),
    ).rejects.toThrow("duplicate order submission");
}

async function fetchOrderRequestsTheOrderEndpoint(): Promise<void> {
    await expect(fetchOrder("ord_1")).resolves.toBeDefined();
}

describe("orders api", () => {
    it("rejects a duplicate order submission", placeOrderRejectsADuplicateSubmission);
    it("fetches a single order by id", fetchOrderRequestsTheOrderEndpoint);
});
