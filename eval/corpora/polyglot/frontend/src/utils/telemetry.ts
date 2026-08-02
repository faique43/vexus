/** Client-side analytics labels.
 *
 * The handlers here are function *values* — an object-literal table and a
 * member assignment — rather than top-level declarations, which is how most
 * JS/TS event plumbing is actually written.
 */

import { formatMoney } from "./format";

export interface AnalyticsEvent {
    name: string;
    cents: number;
}

export const labelers = {
    /** Turn a completed checkout into a one-line human-readable label. */
    describeCheckout: function (event: AnalyticsEvent): string {
        return `checkout ${formatMoney(event.cents)}`;
    },

    /** Same shape of label for a refund. */
    describeRefund: (event: AnalyticsEvent): string => `refund ${formatMoney(event.cents)}`,
};

/** Handlers keyed by event name, populated by assignment below. */
export const registry: Record<string, (event: AnalyticsEvent) => string> = {};

registry.purchase = function trackPurchase(event: AnalyticsEvent): string {
    return labelers.describeCheckout(event);
};
