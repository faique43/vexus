/** Public entry point: re-exports the frontend package surface. */

export { request } from "./api/client";
export { fetchOrder, placeOrder } from "./api/orders";
export { fetchUser, registerUser } from "./api/users";

export { addItem, cartItemCount, cartStore, removeItem } from "./state/cart";
export { Store } from "./state/store";

export { formatDate, formatMoney } from "./utils/format";
export { markIfNew } from "./utils/idempotency";
export { rateLimit } from "./utils/rate_limit";

export { renderOrderSummary } from "./views/order_summary";
