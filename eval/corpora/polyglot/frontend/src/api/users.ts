/** Typed API calls for the user endpoints. */

import { request } from "./client";

/** Fetch the current user's profile. */
export function fetchUser(userId: string): Promise<unknown> {
    return request(`/api/users/${userId}`, { method: "GET" });
}

/** Register a new user account. */
export function registerUser(email: string, displayName: string): Promise<unknown> {
    return request("/api/users", { method: "POST", body: { email, displayName } });
}
