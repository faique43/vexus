/** Low-level HTTP client shared by every typed API module. */

export interface RequestOptions {
    method: string;
    body?: unknown;
}

/** Issue an HTTP request against the backend, throwing on a non-2xx response. */
export function request(path: string, options: RequestOptions): Promise<unknown> {
    return fetch(path, {
        method: options.method,
        body: JSON.stringify(options.body),
    }).then((res) => {
        if (!res.ok) {
            throw new Error(`request to ${path} failed with ${res.status}`);
        }
        return res.json();
    });
}
