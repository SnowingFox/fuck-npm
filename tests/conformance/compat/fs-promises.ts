/** `fs/promises` as bun has it: node's plus `exists(path)`. */
import { access } from "node:fs/promises";

export * from "node:fs/promises";
export { default } from "node:fs/promises";

export async function exists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}
