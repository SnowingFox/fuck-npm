/** `fs/promises` as bun has it: node's plus `exists(path)`, with `bun.lockb` read as fnpm.lock (see ./lockfile.ts). */
import fsp from "node:fs/promises";
import { lockfilePath } from "./lockfile.ts";

export * from "node:fs/promises";
export { default } from "node:fs/promises";

export const access: typeof fsp.access = (path, mode) => fsp.access(lockfilePath(path), mode);
export const stat = ((path: any, options?: any) => fsp.stat(lockfilePath(path), options)) as typeof fsp.stat;
export const readFile = ((path: any, options?: any) => fsp.readFile(lockfilePath(path), options)) as typeof fsp.readFile;
export const rm: typeof fsp.rm = (path, options) => fsp.rm(lockfilePath(path), options);

export async function exists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}
