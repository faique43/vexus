/** Client-side request throttling to avoid hammering the API on rapid input
 * (e.g. an autocomplete box). Internally this is always called rate
 * limiting, matching the naming used on the service side — a query for
 * "throttle API calls from the client" has to rely on semantic matching
 * rather than a literal keyword hit against this module.
 */

/** Wrap `fn` so it can run at most once per `waitMs` window, dropping calls in between. */
export function rateLimit<T extends (...args: unknown[]) => void>(fn: T, waitMs: number): T {
    let lastCall = 0;
    return ((...args: unknown[]) => {
        const now = Date.now();
        if (now - lastCall >= waitMs) {
            lastCall = now;
            fn(...args);
        }
    }) as T;
}
