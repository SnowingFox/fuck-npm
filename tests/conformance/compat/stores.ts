/**
 * The stores the compat spawn gave tests with a cache of their own (see `storeOfCache` in ./bun.ts), by cache
 * directory. bun keeps packages in its cache, so a test that deletes its cache expects them gone: `rm` of the
 * cache (./fs-promises.ts) removes the store that goes with it too.
 */
import fs from "node:fs";
import { resolve } from "node:path";

const stores = new Map<string, string>();

export function rememberCacheStore(cache: string, store: string) {
  stores.set(resolve(cache), store);
}

/** Removes the store of the cache `path`, if the compat spawn gave it one. */
export function forgetCacheStore(path: string) {
  const store = stores.get(resolve(path));
  if (store) fs.rmSync(store, { recursive: true, force: true });
}
