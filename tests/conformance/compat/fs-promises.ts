/** `fs/promises` as bun has it: node's plus `exists(path)`, with `bun.lockb` read as fnpm.lock (see ./lockfile.ts). */
import fsp from "node:fs/promises";
import { lockfilePath } from "./lockfile.ts";
import { forgetCacheStore } from "./stores.ts";

export * from "node:fs/promises";
export { default } from "node:fs/promises";

export const access: typeof fsp.access = (path, mode) => fsp.access(lockfilePath(path), mode);
export const stat = ((path: any, options?: any) => fsp.stat(lockfilePath(path), options)) as typeof fsp.stat;
export const readFile = ((path: any, options?: any) => fsp.readFile(lockfilePath(path), options)) as typeof fsp.readFile;
// Removing a test's cache removes the store that the compat spawn keeps for it (./stores.ts).
export const rm: typeof fsp.rm = async (path, options) => {
  await fsp.rm(lockfilePath(path), options);
  forgetCacheStore(String(path));
};

export async function exists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}
