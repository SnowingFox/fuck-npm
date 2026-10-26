/**
 * bun writes the binary `bun.lockb` when `install.saveTextLockfile = false`; fnpm only has its text lockfile and
 * writes `fnpm.lock` in that case too. The tests check and read `bun.lockb` after such an install, so the compat
 * readers (`access`, `exists`, `rm`, `stat`, `readFile` and `Bun.file`) look at `fnpm.lock` instead when there is
 * no real `bun.lockb` and the bunfig.toml next to it asked for the binary lockfile. A `bun.lockb` a test writes
 * itself (a fixture to migrate) is still the real file.
 */
import fs from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { parse as parseToml } from "smol-toml";

export function lockfilePath<P>(path: P): P {
  const name = typeof path === "string" ? path : path instanceof URL ? fileURLToPath(path) : undefined;
  if (name === undefined || basename(name) !== "bun.lockb" || fs.existsSync(name)) return path;
  const dir = dirname(name);
  let config: any;
  try {
    config = parseToml(fs.readFileSync(join(dir, "bunfig.toml"), "utf8"));
  } catch {
    return path;
  }
  return (config?.install?.saveTextLockfile === false ? join(dir, "fnpm.lock") : path) as P;
}
