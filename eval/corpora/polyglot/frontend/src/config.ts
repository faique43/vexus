/** Frontend runtime configuration. */

export interface AppConfig {
    apiBaseUrl: string;
    requestTimeoutMs: number;
}

/** Load configuration baked in at build time (or defaulted for local dev). */
export function loadConfig(): AppConfig {
    return {
        apiBaseUrl: (globalThis as { API_BASE_URL?: string }).API_BASE_URL ?? "http://localhost:8080",
        requestTimeoutMs: 5000,
    };
}
