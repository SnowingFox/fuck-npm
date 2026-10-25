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
export function readTarball(path: string) {
  let data = fs.readFileSync(path);
  if (data[0] === 0x1f && data[1] === 0x8b) data = zlib.gunzipSync(data);
  const entries = readTar(data).map(entry => ({
    pathname: entry.path,
    kind: "file" as const,
    perm: entry.mode,
    size: entry.data.byteLength,
    contents: entry.data.toString("utf8"),
  }));
  return { entries, size: data.byteLength };
}

export function semverIntersects(a: string, b: string): boolean {
  return semver.intersects(a, b, { loose: true });
}

function matchList(list: string[] | string | undefined, current: string): boolean {
  if (list === undefined) return true;
  const values = Array.isArray(list) ? list : [list];
  if (values.length === 0) return true;
  const allowed = values.filter(value => !value.startsWith("!"));
  const denied = values.filter(value => value.startsWith("!")).map(value => value.slice(1));
  if (denied.includes(current)) return false;
  return allowed.length === 0 || allowed.includes(current) || allowed.includes("any");
}

export function isArchitectureMatch(cpu: string[] | string | undefined, arch: string = process.arch): boolean {
  return matchList(cpu, arch);
}

export function isOperatingSystemMatch(os: string[] | string | undefined, platform: string = process.platform): boolean {
  return matchList(os, platform);
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
