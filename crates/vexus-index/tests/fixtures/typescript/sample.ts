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

// Const-assigned function forms: all must index like the arrow form does.
export const readDataStream = async function* <T>(chunks: T[]) {
  for (const c of chunks) yield load(String(c));
};

const compact = function (xs: number[]) {
  return xs.filter(Boolean);
};

export function* idGen(prefix: string) {
  yield prefix;
}

export const fetchUserArrow = (id: string) => load(id);
