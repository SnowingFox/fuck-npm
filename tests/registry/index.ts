/**
 * A local npm registry for fnpm tests and benchmarks, on node:http.
 *
 * The core (packuments in both forms, tarballs, dist-tags, scoped names, ETag/304, ranges, users, tokens, publish,
 * read/write access rules) is bun's test registry (test/packages/registry) ported from Bun.serve to node. On top of
 * it: loose tarball fixtures, fault injection for retry tests, and an uplink that proxies and caches a real
 * registry on disk.
 *
 * ```ts
 * const registry = await start({ port: 0 });
 * // registry.url === "http://localhost:<port>/"
 * await registry.stop();
 * ```
 */
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Registry, type RecordedRequest, type RegistryOptions } from "./src/registry.ts";
import type { Fault } from "./src/server.ts";
import type { UplinkOptions } from "./src/uplink.ts";

export { Advisories, type Advisory } from "./src/advisories.ts";
export { Auth, type Credentials, type Token, type User } from "./src/auth.ts";
export { RegistryError } from "./src/http.ts";
export { isValidRange, isValidTag, isValidVersion, parsePackageName, type PackageName } from "./src/names.ts";
export { Packages, type AccessRules, type StoredPackage } from "./src/packages.ts";
export { abbreviatedContentType, type Packument, type VersionDocument } from "./src/packument.ts";
export { Registry, type RecordedRequest, type RegistryOptions } from "./src/registry.ts";
export { NodeServer, type Fault } from "./src/server.ts";
export { integrityOf, packumentsFromTarballs, readTar, readTarballManifest, shasumOf } from "./src/tgz.ts";
export { Uplink, type UplinkOptions, type UplinkStats } from "./src/uplink.ts";

const here = dirname(fileURLToPath(import.meta.url));
/** The repository root (tests/registry/../..). */
export const repoRoot = resolve(here, "..", "..");
/** The install tests of bun, the source of the fixtures. */
/** bun's checkout: `FNPM_BUN_DIR` (a worktree whose submodule is not checked out), else `submodule/bun`. */
const bunDir = process.env.FNPM_BUN_DIR || join(repoRoot, "submodule", "bun");
export const bunInstallTestDir = join(bunDir, "test", "cli", "install");
/** Verdaccio-layout fixture packages that the bun install tests share. */
export const fixturePackagesDir = join(bunInstallTestDir, "registry", "packages");

export interface StartOptions {
  /** 0 (the default) takes a free port. */
  port?: number;
  hostname?: string;
  /**
   * A verdaccio-layout directory (`<name>/package.json` + tarballs). The default is the bun fixture packages. Pass
   * `null` for an empty registry (an uplink-only mirror, or publish tests).
   */
  packagesDir?: string | null;
  /** Directories with loose `*.tgz` fixtures. The default is bun test/cli/install when `packagesDir` is the default. */
  tarballDirs?: string[];
  /** Upstream registry to proxy, e.g. `https://registry.npmjs.org`. */
  uplink?: string;
  /** Disk cache for the uplink. */
  cacheDir?: string;
  /** Re-validate cached uplink packuments with upstream (ETag). The default serves the disk copy. */
  revalidate?: boolean;
  /** Record every request in `requests` (the default is true). */
  recordRequests?: boolean;
  faults?: Fault[];
  latencyMs?: number;
  access?: RegistryOptions["access"];
  intercept?: RegistryOptions["intercept"];
}

export interface RunningRegistry {
  /** `http://localhost:<port>/`, with the trailing slash. */
  url: string;
  port: number;
  registry: Registry;
  /** Every request with the status of its answer (when `recordRequests` is on). */
  requests: RecordedRequest[];
  addFault(fault: Fault): Fault;
  clearFaults(): void;
  /** Creates (or reuses) a user and returns a new token for `Authorization: Bearer`. */
  createUser(name: string, password?: string): string;
  stop(): Promise<void>;
}

export async function start(options: StartOptions = {}): Promise<RunningRegistry> {
  const useDefault = options.packagesDir === undefined;
  const storage = options.packagesDir === null ? undefined : (options.packagesDir ?? fixturePackagesDir);
  const uplink: UplinkOptions | undefined =
    options.uplink === undefined
      ? undefined
      : { url: options.uplink, cacheDir: options.cacheDir, revalidate: options.revalidate };
  const registry = new Registry({
    storage,
    tarballDirs: options.tarballDirs ?? (useDefault ? [bunInstallTestDir] : []),
    uplink,
    hostname: options.hostname,
    port: options.port ?? 0,
    recordRequests: options.recordRequests ?? true,
    faults: options.faults,
    latencyMs: options.latencyMs,
    access: { "@needs-auth/*": { read: "authenticated" }, ...options.access },
    intercept: options.intercept,
  });
  await registry.ready();
  return {
    url: registry.url,
    port: registry.port,
    registry,
    requests: registry.requests,
    addFault: fault => registry.addFault(fault),
    clearFaults: () => registry.clearFaults(),
    createUser(name, password = name) {
      const user = registry.auth.users.get(name) ?? registry.auth.addUser(name, password);
      return registry.auth.createToken(user).token;
    },
    async stop() {
      registry.close();
    },
  };
}
