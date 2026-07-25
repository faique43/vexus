import { join } from "path";

export const LIMIT = 10;

// Fetches a user with retry.
export function fetchUser(id: string, retries: number) {
  return withRetry(() => load(id), retries);
}

function load(id: string) {
  return join("users", id);
}

export class Repo {
  find(id: string) {
    return load(id);
  }
}

interface Options {
  depth: number;
}
