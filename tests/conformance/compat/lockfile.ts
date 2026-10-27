/**
 * Files fnpm names differently from bun, read under bun's names by the compat readers (`access`, `exists`, `rm`,
 * `stat`, `readFile`, `Bun.file` and `readdirSorted`):
 *
 * - bun writes the binary `bun.lockb` when `install.saveTextLockfile = false`; fnpm only has its text lockfile and
 *   writes `fnpm.lock` in that case too. A missing `bun.lockb` is read as the `fnpm.lock` next to it when the
 *   bunfig.toml there asked for the binary lockfile.
 * - bun tags git, GitHub and tarball installs with `.bun-tag`; fnpm writes `.fnpm-tag`. A missing `.bun-tag` is
 *   read as the `.fnpm-tag` next to it, and a listing shows `.fnpm-tag` as `.bun-tag`.
 *
 * A file a test writes under bun's name itself (a lockfile to migrate, a checked-in tag) is still the real file.
 */
import fs from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { parse as parseToml } from "smol-toml";

export function lockfilePath<P>(path: P): P {
  const name = typeof path === "string" ? path : path instanceof URL ? fileURLToPath(path) : undefined;
  if (name === undefined) return path;
  const base = basename(name);
  if ((base !== "bun.lockb" && base !== ".bun-tag") || fs.existsSync(name)) return path;
  const dir = dirname(name);
  if (base === ".bun-tag") {
    const tag = join(dir, ".fnpm-tag");
    return (fs.existsSync(tag) ? tag : path) as P;
  }
  let config: any;
  try {
    config = parseToml(fs.readFileSync(join(dir, "bunfig.toml"), "utf8"));
  } catch {
    return path;
  }
  return (config?.install?.saveTextLockfile === false ? join(dir, "fnpm.lock") : path) as P;
}

/** A directory listing with fnpm's `.fnpm-tag` under bun's name (see above), in the order it was given. */
export function bunNames(entries: string[]): string[] {
  if (!entries.includes(".fnpm-tag") || entries.includes(".bun-tag")) return entries;
  return entries.map(entry => (entry === ".fnpm-tag" ? ".bun-tag" : entry));
}
