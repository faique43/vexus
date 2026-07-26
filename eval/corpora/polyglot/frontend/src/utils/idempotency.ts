/** Prevents duplicate submissions when a network retry resends the same request. */

const seenKeys = new Set<string>();

/** Return true the first time `key` is seen; false on every subsequent call,
 * so a client-side retry that replays an already-sent request is silently
 * absorbed instead of double-submitting it.
 */
export function markIfNew(key: string): boolean {
    if (seenKeys.has(key)) {
        return false;
    }
    seenKeys.add(key);
    return true;
}
