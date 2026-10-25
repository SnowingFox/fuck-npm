import { createHash } from "node:crypto";
/**
 * `bun:internal-for-testing` for the conformance suite. Helpers with an obvious meaning outside bun (reading a
 * tarball, semver intersection, os/cpu matching) are implemented; bun's binary-lockfile and manifest parsers
 * have no fnpm equivalent, so those members throw when they are used.
 */
import fs from "node:fs";
import zlib from "node:zlib";
import semver from "semver";
import { readTar } from "../../registry/src/tgz.ts";

const unavailable = (name: string) => () => {
  throw new Error(`bun:internal-for-testing.${name} is bun-internal; not available in the fnpm conformance suite`);
};

function stub(name: string): any {
  return new Proxy(unavailable(name), {
    get: (_fn, inner) => (typeof inner === "string" ? unavailable(`${name}.${inner}`) : undefined),
  });
}

/** Entries of a .tgz in archive order, in the shape bun's pack tests read. */
/** bun's `readTarball`: the entries, and the size, sha1 hex and sha512 base64 of the file as it is. */
export function readTarball(path: string) {
  const file = fs.readFileSync(path);
  const shasum = createHash("sha1").update(file).digest("hex");
  const integrity = createHash("sha512").update(file).digest("base64");
  let data = file;
  if (data[0] === 0x1f && data[1] === 0x8b) data = zlib.gunzipSync(data);
  const entries = readTar(data).map(entry => ({
    pathname: entry.path,
    kind: "file" as const,
    perm: entry.mode,
    size: entry.data.byteLength,
    contents: entry.data.toString("utf8"),
  }));
  return { entries, size: file.byteLength, shasum, integrity };
}

export function semverIntersects(a: string, b: string): boolean {
  return semver.intersects(a, b, { loose: true });
}

// Bun's Negatable resets wildcard/unknown state when it sees a recognized platform.
function matchList(list: string[] | string | undefined, current: string, known: readonly string[]): boolean {
  const added = new Set<string>();
  const removed = new Set<string>();
  let wildcard = false;
  let unknown = false;
  for (const value of list === undefined ? [] : Array.isArray(list) ? list : [list]) {
    if (value === "") continue;
    if (value === "any") { wildcard = true; continue; }
    if (value === "none") { unknown = true; continue; }
    const denied = value.startsWith("!");
    const name = denied ? value.slice(1) : value;
    if (!known.includes(name)) {
      if (!denied) unknown = true;
      continue;
    }
    (denied ? removed : added).add(name);
    wildcard = unknown = false;
  }
  if (removed.has(current)) return false;
  if (wildcard) return true;
  if (added.size === 0 && removed.size === 0) return !unknown;
  return added.size === 0 || added.has(current);
}

export function isArchitectureMatch(cpu: string[] | string | undefined, arch: string = process.arch): boolean {
  return matchList(cpu, arch, ["arm", "arm64", "ia32", "mips", "mipsel", "ppc", "ppc64", "s390", "s390x", "x32", "x64"]);
}

export function isOperatingSystemMatch(os: string[] | string | undefined, platform: string = process.platform): boolean {
  return matchList(os, platform, ["aix", "darwin", "freebsd", "linux", "openbsd", "sunos", "win32", "android"]);
}

export const install_test_helpers = stub("install_test_helpers");
export const npm_manifest_test_helpers = stub("npm_manifest_test_helpers");
export const upgrade_test_helpers = stub("upgrade_test_helpers");
export const iniInternals = stub("iniInternals");
export const heapStats = stub("heapStats");
export const parseLockfile = stub("parseLockfile");

export default {
  readTarball,
  semverIntersects,
  isArchitectureMatch,
  isOperatingSystemMatch,
  install_test_helpers,
  npm_manifest_test_helpers,
  upgrade_test_helpers,
  iniInternals,
  heapStats,
  parseLockfile,
};
