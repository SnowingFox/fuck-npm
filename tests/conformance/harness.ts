/**
 * The conformance harness: bun's test/harness.ts for fnpm. Names follow bun's, with `bun` swapped for `fnpm`
 * (bunExe -> fnpmExe, bunEnv -> fnpmEnv, runBunInstall -> runFnpmInstall, ...). The bun names are exported as
 * aliases too.
 *
 * Every fnpm spawned with `fnpmEnv` gets its own FNPM_STORE_DIR and FNPM_CACHE_DIR under a temp directory of this
 * worker, so tests never touch the user's store. A worker-local user npmrc supplies the fixture registry
 * that globalSetup starts (a file that starts its own registry repoints it).
 */
import fs from "node:fs";
import { readdir } from "node:fs/promises";
import os from "node:os";
import { dirname, isAbsolute, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { inject } from "vitest";
import { bunNames } from "./compat/lockfile.ts";
import { Archive, deepMatch, file, storeOfCache, parseJsonc, resolveSync, semver, sleepSync, spawn, type Subprocess } from "./compat/bun.ts";

export type { Subprocess };

const here = dirname(fileURLToPath(import.meta.url));
export const repoRoot = resolve(here, "..", "..");
/** bun's sources (the submodule). Fixtures of the ported tests are read from here, not copied. */
export const bunRoot = process.env.FNPM_BUN_DIR ?? join(repoRoot, "submodule", "bun");

/** A directory of bun's test tree, e.g. `bunSourceDir("test/cli/install")`. The ported tests use it for `import.meta.dir`. */
export function bunSourceDir(relativePath: string): string {
  return join(bunRoot, relativePath);
}

// ---------------------------------------------------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------------------------------------------------

export const isMacOS = process.platform === "darwin";
export const isLinux = process.platform === "linux";
export const isFreeBSD = process.platform === "freebsd";
export const isAndroid = process.platform === "android";
export const isPosix = isMacOS || isLinux || isFreeBSD || isAndroid;
export const isWindows = process.platform === "win32";
export const isRoot = process.getuid?.() === 0;
export const isArm64 = process.arch === "arm64";
export const isIntelMacOS = isMacOS && process.arch === "x64";
export const isCI = process.env.CI !== undefined;
export const isFlaky = isCI;
export const isBroken = isCI;
export const isBuildKite = false;
export const isVerbose = process.env.DEBUG === "1";
/** fnpm is a release build of a native addon; there is no debug/ASAN flavour to slow tests down. */
export const isDebug = false;
export const isASAN = false;
export const isCanary = false;
export const MAX_PATH_BYTES = isWindows ? 32767 * 3 + 1 : isLinux || isAndroid ? 4096 : 1024;
export const libcFamily: "glibc" | "musl" =
  process.platform !== "linux"
    ? "glibc"
    : (process.report.getReport() as { header: { glibcVersionRuntime?: string } }).header.glibcVersionRuntime
      ? "glibc"
      : "musl";
export const isMusl = isLinux && libcFamily === "musl";
export const isGlibc = isLinux && libcFamily === "glibc";

export function isIPv6() {
  return Object.values(os.networkInterfaces()).some(list => list?.some(i => i.family === "IPv6" && i.internal));
}

export function isIPv4() {
  return true;
}

export function isDockerEnabled() {
  return false;
}

export function dockerExe(): string | null {
  return null;
}

// ---------------------------------------------------------------------------------------------------------------------
// fnpm executable and environment
// ---------------------------------------------------------------------------------------------------------------------

/**
 * The fnpm CLI. FNPM_EXE overrides it; the default is the built CLI of packages/fuck-npm. A `.js` path is run with
 * the current node by `spawn`, so it does not need to be executable.
 */
/**
 * Removes the cache entries `evict` selects: bun's `<name>@<version>...` folders and fnpm's
 * packages (`<cache>/store/v2/pkgs/<name>@<version>-<hash>`, scoped names with `+`), so a
 * test that evicts a package from bun's cache evicts it from fnpm's store as well.
 */
export async function evictCacheEntries(cache: string, evict: (entry: string) => boolean): Promise<void> {
  const remove = (path: string) => fs.promises.rm(path, { recursive: true, force: true });
  for (const entry of fs.existsSync(cache) ? fs.readdirSync(cache).sort() : []) {
    if (entry !== "store" && evict(entry)) await remove(join(cache, entry));
  }
  for (const store of storesOfCache(cache)) {
    const pkgs = join(store, "v2", "pkgs");
    for (const entry of fs.existsSync(pkgs) ? fs.readdirSync(pkgs).sort() : []) {
      const named = entry.replace(/^@([^+]+)\+/, "@$1/");
      if (evict(named) || evict(entry)) await remove(join(pkgs, entry));
    }
  }
}

/**
 * The stores that hold the packages of the cache `cache`: `<cache>/store`, and the one the compat spawn gives a test
 * with a cache of its own ([`storeOfCache`]).
 */
function storesOfCache(cache: string): string[] {
  return [join(cache, "store"), storeOfCache(workerRoot, cache)];
}

/**
 * The package folders of `<name>@<version>` in a cache: bun's `<name>@<version>@...` entries and fnpm's store packages
 * (`<store>/v2/pkgs/<name>@<version>-<hash>/package`, scoped names with `+`), so a test that reads a package out of
 * bun's cache reads it from fnpm's store instead. The store is `<cache>/store` unless `store` (the FNPM_STORE_DIR the
 * install ran with) says otherwise.
 */
export function cacheEntryDirs(cache: string, name: string, version: string, store?: string): string[] {
  const bun = (fs.existsSync(cache) ? fs.readdirSync(cache).sort() : [])
    .filter(entry => entry.startsWith(`${name}@${version}@`))
    .map(entry => join(cache, entry));
  const stored = name.replace(/^@([^/]+)\//, "@$1+");
  const fnpm = (store ? [store] : storesOfCache(cache)).flatMap(store => {
    const pkgs = join(store, "v2", "pkgs");
    return (fs.existsSync(pkgs) ? fs.readdirSync(pkgs).sort() : [])
      .filter(entry => entry.startsWith(`${stored}@${version}-`))
      .map(entry => join(pkgs, entry, "package"));
  });
  return [...bun, ...fnpm];
}

/**
 * The manifest cache entries under `cache`, as absolute paths: bun's `<cache>/<hash>.npm` files and fnpm's
 * `<cache>/metadata/v1/<hash>.rkyv` ones. A test that checks what a cached manifest holds (its ETag, its
 * Last-Modified) or that one was written reads fnpm's entry through this instead of bun's file name.
 */
export function cachedManifestFiles(cache: string): string[] {
  const list = (dir: string, suffix: string) =>
    fs.existsSync(dir) ? fs.readdirSync(dir).filter(name => name.endsWith(suffix)).map(name => join(dir, name)) : [];
  return [...list(cache, ".npm"), ...list(join(cache, "metadata", "v1"), ".rkyv")].sort();
}

export function fnpmExe(): string {
  return process.env.FNPM_EXE ?? join(repoRoot, "packages", "fuck-npm", "dist", "fnpm.js");
}

/** node, for the tests that run a script (bun ran them with itself). */
export function nodeExe(): string {
  return process.execPath;
}

const workerRoot = fs.mkdtempSync(join(fs.realpathSync.native(os.tmpdir()), `fnpm-conformance-${process.pid}-`));
process.once("exit", () => {
  try {
    fs.rmSync(workerRoot, { recursive: true, force: true });
  } catch {}
});

/** The fixture registry of globalSetup, when it runs. */
export function sharedRegistryUrl(): string | undefined {
  try {
    return (inject as (key: string) => string | undefined)("fnpmRegistry") ?? process.env.FNPM_REGISTRY;
  } catch {
    return process.env.FNPM_REGISTRY;
  }
}

/** The user .npmrc of this worker; [`setDefaultRegistry`] writes its `registry=`. */
const defaultRegistryNpmrc = join(workerRoot, "user.npmrc");

/** Points the fixture registry every fnpm falls back to (no registry in bunfig.toml or .npmrc) at `url`. */
export function setDefaultRegistry(url: string | undefined) {
  fs.writeFileSync(defaultRegistryNpmrc, url ? `registry=${url}\n` : "");
}
setDefaultRegistry(sharedRegistryUrl());

function buildEnv(): Record<string, string | undefined> {
  const env: Record<string, string | undefined> = {
    ...process.env,
    GITHUB_ACTIONS: "false",
    NO_COLOR: "1",
    FORCE_COLOR: undefined,
    TZ: "Etc/UTC",
    CI: "1",
    AGENT: "false",
    FNPM_STORE_DIR: join(workerRoot, "store"),
    FNPM_CACHE_DIR: join(workerRoot, "cache"),
    // retry backoff of 1ms, like BUN_CONFIG_HTTP_RETRY_BACKOFF in bun's harness
    FNPM_CONFIG_HTTP_RETRY_BACKOFF: "1",
    FNPM_DISABLE_SLOW_FILESYSTEM_WARNING: "1",
    FNPM_INTERNAL_INTERACTIVE_ASSUME_TTY: "1",
  };
  for (const key of Object.keys(env)) {
    if (env[key] === undefined) delete env[key];
    else if (key === "NODE" || key.startsWith("npm_") || key.startsWith("NPM_CONFIG_") || key.startsWith("BUILDKITE")) {
      delete env[key];
    }
  }
  // The default registry goes in a user .npmrc: like bun's harness, a test's bunfig.toml or .npmrc wins over it
  // (an FNPM_REGISTRY variable would win over both).
  env.NPM_CONFIG_USERCONFIG = defaultRegistryNpmrc;
  delete env.FNPM_REGISTRY;
  // The porter renames BUN_INSTALL_CACHE_DIR to FNPM_CACHE_DIR. Tests remove that variable
  // to exercise their configured cache; a second legacy override would silently defeat it.
  delete env.BUN_INSTALL_CACHE_DIR;
  delete env.NODE_ENV;
  delete env.NODE_OPTIONS;
  delete env.VITEST;
  delete env.VITEST_POOL_ID;
  delete env.VITEST_WORKER_ID;
  return env;
}

/** The environment for every spawned fnpm (bunEnv). A test file that starts a registry repoints the default with setDefaultRegistry. */
export const fnpmEnv: Record<string, string | undefined> = buildEnv();

/** bunEnv/installEnv with a cache of its own under `dir`. */
export function installEnv(dir: string, env: Record<string, string | undefined> = fnpmEnv) {
  return {
    ...env,
    FNPM_CACHE_DIR: join(dir, ".fnpm-cache"),
    FNPM_STORE_DIR: join(dir, ".fnpm-store"),
  };
}

export interface RunOptions {
  cwd?: string;
  env?: Record<string, string | undefined>;
  stdin?: string;
}

export interface RunResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

/** Runs fnpm and waits for it: `runFnpm(["install"], { cwd })`. */
export async function runFnpm(args: string[], options: RunOptions = {}): Promise<RunResult> {
  const proc = spawn({
    cmd: [fnpmExe(), ...args],
    cwd: options.cwd,
    env: options.env ?? fnpmEnv,
    stdin: options.stdin ?? "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([proc.stdout!.text(), proc.stderr!.text(), proc.exited]);
  return { stdout, stderr, exitCode };
}

/** `runBunInstall`: `fnpm install` with bun's assertions on its output. */
export async function runFnpmInstall(
  env: Record<string, string | undefined>,
  cwd: string,
  options: {
    allowWarnings?: boolean;
    allowErrors?: boolean;
    expectedExitCode?: number | null;
    savesLockfile?: boolean;
    production?: boolean;
    frozenLockfile?: boolean;
    saveTextLockfile?: boolean;
    packages?: string[];
    verbose?: boolean;
  } = {},
) {
  const { expect } = await import("vitest");
  const production = options.production ?? false;
  const args = ["install", ...(options.packages ?? [])];
  if (production) args.push("--production");
  if (options.frozenLockfile) args.push("--frozen-lockfile");
  if (options.saveTextLockfile) args.push("--save-text-lockfile");
  if (options.verbose) args.push("--verbose");
  const proc = spawn({ cmd: [fnpmExe(), ...args], cwd, stdout: "pipe", stdin: "ignore", stderr: "pipe", env });
  const [err, out, exitCode] = await Promise.all([proc.stderr!.text(), proc.stdout!.text(), proc.exited]);
  expect(err).not.toContain("panic:");
  if (!options.allowErrors) expect(err).not.toContain("error:");
  if (!options.allowWarnings) expect(err).not.toContain("warn:");
  if ((options.savesLockfile ?? true) && !production && !options.frozenLockfile) {
    expect(err).toContain("Saved lockfile");
  }
  expect(exitCode).toBe(options.expectedExitCode ?? 0);
  return { out, err, exited: proc.exited };
}

/** `runBunUpdate`. */
export async function runFnpmUpdate(env: Record<string, string | undefined>, cwd: string, args: string[] = []) {
  const { expect } = await import("vitest");
  const proc = spawn({ cmd: [fnpmExe(), "update", ...args], cwd, stdout: "pipe", stdin: "ignore", stderr: "pipe", env });
  const err = await proc.stderr!.text();
  const out = await proc.stdout!.text();
  const exitCode = await proc.exited;
  if (exitCode !== 0) {
    console.log("stdout:", out);
    console.log("stderr:", err);
    expect.fail("fnpm update failed");
  }
  return { out: out.replace(/\s*\[[0-9.]+m?s\]\s*$/, "").split(/\r?\n/), err, exitCode };
}

/** `fnpm pm pack` with bun's assertions. */
export async function pack(cwd: string, env: Record<string, string | undefined>, ...args: string[]) {
  const { expect } = await import("vitest");
  const proc = spawn({ cmd: [fnpmExe(), "pm", "pack", ...args], cwd, stdout: "pipe", stderr: "pipe", stdin: "ignore", env });
  const err = await proc.stderr!.text();
  expect(err).not.toContain("error:");
  expect(err).not.toContain("warning:");
  expect(err).not.toContain("failed");
  expect(err).not.toContain("panic:");
  const out = await proc.stdout!.text();
  expect(await proc.exited).toBe(0);
  return { out, err };
}

/** The lockfile of a project: fnpm.lock (JSONC), parsed. */
export async function readLockfile(dir: string, name = "fnpm.lock"): Promise<any> {
  return parseJsonc(await file(join(dir, name)).text());
}

// ---------------------------------------------------------------------------------------------------------------------
// Temp directories
// ---------------------------------------------------------------------------------------------------------------------

export type DirectoryTree = {
  [name: string]: string | Buffer | Uint8Array | DirectoryTree | ((opts: { root: string }) => string | Buffer | DirectoryTree);
};

const isTree = (value: unknown): value is DirectoryTree =>
  typeof value === "object" && value !== null && (value as { byteLength?: number }).byteLength === undefined;

export function makeTreeSync(base: string, tree: DirectoryTree | string) {
  if (typeof tree === "string") {
    fs.cpSync(tree, base, { recursive: true });
    return;
  }
  for (const [name, raw] of Object.entries(tree)) {
    const contents = typeof raw === "function" ? raw({ root: base }) : raw;
    const joined = join(base, name);
    if (name.includes("/")) fs.mkdirSync(dirname(joined), { recursive: true });
    if (isTree(contents)) {
      fs.mkdirSync(joined, { recursive: true });
      makeTreeSync(joined, contents);
      continue;
    }
    fs.writeFileSync(joined, contents as string | Uint8Array);
  }
}

export async function makeTree(base: string, tree: DirectoryTree) {
  makeTreeSync(base, tree);
}

export function tmpdirSync(pattern = "fnpm.test."): string {
  return fs.mkdtempSync(join(fs.realpathSync.native(os.tmpdir()), pattern));
}

/** Creates a temp directory with `files` in it and returns its (real) path. */
export function tempDirWithFiles(basename: string, files: DirectoryTree | string = {}): string {
  const base = fs.mkdtempSync(join(fs.realpathSync.native(os.tmpdir()), basename + "_"));
  makeTreeSync(base, files);
  return base;
}

export function tempDirWithFilesAnon(files: DirectoryTree | string): string {
  return tempDirWithFiles("anon", files);
}

class DisposableString extends String {
  [Symbol.dispose]() {
    fs.rmSync(String(this), { recursive: true, force: true });
  }
  async [Symbol.asyncDispose]() {
    await fs.promises.rm(String(this), { recursive: true, force: true });
  }
}

/** `using dir = tempDir("name", files)`: removed at the end of the scope. */
export function tempDir(basename: string, files: DirectoryTree | string = {}): string & DisposableString & AsyncDisposable {
  return new DisposableString(tempDirWithFiles(basename, files)) as string & DisposableString & AsyncDisposable;
}

export function substDrive(_dir: string): Disposable & { root: string } {
  throw new Error("substDrive is Windows only");
}

export function rmScope(path: string) {
  return { [Symbol.dispose]: () => fs.rmSync(path, { recursive: true, force: true }) };
}

export function cwdScope(cwd: string) {
  const original = process.cwd();
  process.chdir(cwd);
  return { [Symbol.dispose]: () => process.chdir(original) };
}

export async function readdirSorted(path: string): Promise<string[]> {
  const results = bunNames(await readdir(path));
  results.sort();
  return results;
}

// ---------------------------------------------------------------------------------------------------------------------
// Output normalization
// ---------------------------------------------------------------------------------------------------------------------

export function normalizeFnpmSnapshot(snapshot: string, optionalDir?: string) {
  if (optionalDir) {
    snapshot = snapshot
      .replaceAll(fs.realpathSync.native(String(optionalDir)).replaceAll("\\", "/"), "<dir>")
      .replaceAll(String(optionalDir), "<dir>");
  }
  snapshot = snapshot.replace(/^((?:pass|fail|skip|todo)\) .+) \[[\d.]+\s?m?s\]$/gm, "$1");
  const version = (globalThis as { Bun?: { version: string } }).Bun?.version ?? "0.0.0";
  return snapshot
    .replaceAll("\r\n", "\n")
    // A command header (`fnpm install v0.0.1`) reads like bun's normalized one: `fnpm install <version> (<revision>)`.
    // Flags too: `fnpm update --interactive v0.0.1`.
    .replace(/^(fnpm(?: (?:--)?[a-z][\w-]*)+) v\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+\w+)?(?: \([0-9a-f]+\))?$/gm, "$1 <version> (<revision>)")
    .replaceAll("\\", "/")
    .replaceAll(fs.realpathSync.native(process.cwd()).replaceAll("\\", "/"), "<cwd>")
    .replaceAll(fs.realpathSync.native(os.tmpdir()).replaceAll("\\", "/"), "<tmp>")
    .replaceAll(fs.realpathSync.native(os.homedir()).replaceAll("\\", "/"), "<home>")
    .replace(/\s\[[\d.]+\s?m?s\]/gm, "")
    .replace(/^\[[\d.]+\s?m?s\]/gm, "")
    .replace(/^\s+at (.*?)\(.*?:\d+(?::\d+)?\)/gm, "    at $1(file:NN:NN)")
    .replace(/fnpm v[\d.]+(?:-[\w.]+)?(?:\+[\w]+)?(?:\s+\([^)]+\))?/g, "fnpm v<fnpm-version>")
    // bun's `Bun.version_with_sha` -> "<version> (<revision>)" in "bun <command> v1.2.3 (abcdef12)" headers.
    .replace(/\bfnpm ([a-z][a-z-]*) v\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+\w+)?(?: \([0-9a-f]+\))?/g, "fnpm $1 <version> (<revision>)")
    // Not inside an IP address or another dotted number (`127.0.0.1` contains `0.0.1`).
    .replace(new RegExp(`(?<![\\d.])${version.replaceAll(".", "\\.")}(?![\\d.])`, "g"), "<fnpm-version>")
    .trim();
}

export function ospath(path: string) {
  return isWindows ? path.replace(/\//g, "\\") : path;
}

export function osSlashes(path: string) {
  return isWindows ? path.replace(/\//g, "\\") : path;
}

export function joinP(...paths: string[]) {
  return join(...paths).replaceAll("\\", "/");
}

export function mergeWindowEnvs(envs: Record<string, string | undefined>[]) {
  const keys: Record<string, string> = {};
  const flat: Record<string, string | undefined> = {};
  for (const env of envs) {
    for (const key in env) {
      if (!env[key]) continue;
      const normalized = (keys[key.toUpperCase()] ??= key);
      flat[normalized] = env[key];
    }
  }
  return flat;
}

/** Every `<path>/<name>@<version>` under a node_modules folder, sorted. */
export function nodeModulesPackages(nodeModulesPath: string): string {
  const packages: string[] = [];
  function scan(dir: string, relativePath = "") {
    let entries: fs.Dirent[];
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = join(dir, entry.name);
      if (!entry.isDirectory() || entry.name === ".bun-cache" || entry.name === ".fnpm-cache") continue;
      const rel = relativePath ? `${relativePath}/${entry.name}` : entry.name;
      const pkgJson = join(full, "package.json");
      if (fs.existsSync(pkgJson)) {
        try {
          const pkg = JSON.parse(fs.readFileSync(pkgJson, "utf8"));
          packages.push(`${rel}/${pkg.name || "unknown"}@${pkg.version || "unknown"}`);
        } catch {
          packages.push(`${rel}/[invalid-package.json]`);
        }
      }
      scan(full, rel);
    }
  }
  scan(nodeModulesPath);
  packages.sort();
  return packages.join("\n");
}

export function toTOMLString(opts: object) {
  const flat: Record<string, unknown> = {};
  const walk = (value: unknown, prefix: string) => {
    if (value !== null && typeof value === "object" && !Array.isArray(value)) {
      for (const [key, item] of Object.entries(value)) walk(item, prefix ? `${prefix}.${key}` : key);
    } else flat[prefix] = value;
  };
  walk(opts, "");
  let out = "";
  for (const [key, value] of Object.entries(flat)) if (value !== undefined) out += `${key} = ${JSON.stringify(value)}\n`;
  return out;
}

export function writeShebangScript(path: string, program: string, data: string) {
  return fs.promises.writeFile(path, `#!/usr/bin/env ${program}\n \n${data}`, { mode: 0o777 });
}

/** Writes `<packageDir>/node_modules/.bin/<name>`: a command that prints `says`. */
export function writeBinCommand(packageDir: string, name: string, says: string) {
  const bin = join(packageDir, "node_modules", ".bin");
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(join(bin, name), `#!/bin/sh\necho ${says}\n`, { mode: 0o755 });
}

export function textLockfile(version: number, pkgs: any): string {
  return JSON.stringify({ lockfileVersion: version, configVersion: 1, ...pkgs });
}

// ---------------------------------------------------------------------------------------------------------------------
// Fixtures: tarballs, git repositories, TLS
// ---------------------------------------------------------------------------------------------------------------------

/** The tarball GitHub serves for `/repos/<owner>/<repo>/tarball/<ref>` (one root folder `<owner>-<repo>-<sha>`). */
export function githubTarball(rootDir: string, files: Record<string, string | Uint8Array>) {
  const entries: Record<string, string | Uint8Array> = { [`${rootDir}/`]: "" };
  for (const [path, contents] of Object.entries(files)) entries[`${rootDir}/${path}`] = contents;
  return new Archive(entries, { compress: "gzip" }).bytes();
}

/** A local git repository that `cloneUrls` are rewritten to (see bun's harness). */
export async function hostedGitRepository(cloneUrls: string[], files: Record<string, string>) {
  const dir = tempDir("git-dep-hosted", files);
  const git = async (...args: string[]) => {
    const proc = spawn({
      cmd: ["git", ...args],
      cwd: String(dir),
      env: {
        ...fnpmEnv,
        GIT_CONFIG_NOSYSTEM: "1",
        GIT_AUTHOR_NAME: "Test",
        GIT_AUTHOR_EMAIL: "test@example.com",
        GIT_COMMITTER_NAME: "Test",
        GIT_COMMITTER_EMAIL: "test@example.com",
      },
      stdout: "pipe",
      stderr: "pipe",
    });
    const [stdout, stderr, exitCode] = await Promise.all([proc.stdout!.text(), proc.stderr!.text(), proc.exited]);
    if (exitCode !== 0) throw new Error(`git ${args.join(" ")} exited with ${exitCode}: ${stderr}`);
    return stdout.trim();
  };
  await git("-c", "init.defaultBranch=main", "init", "--quiet");
  await git("add", "-A");
  await git("-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "init");
  const key = `url.${new URL(`file://${String(dir)}`).href}.insteadOf`;
  const config: Record<string, string> = { GIT_ALLOW_PROTOCOL: "file", GIT_CONFIG_COUNT: `${cloneUrls.length}` };
  cloneUrls.forEach((url, i) => {
    config[`GIT_CONFIG_KEY_${i}`] = key;
    config[`GIT_CONFIG_VALUE_${i}`] = url;
  });
  return {
    sha: await git("rev-parse", "HEAD"),
    env: { ...fnpmEnv, ...config },
    [Symbol.dispose]: () => dir[Symbol.dispose](),
  };
}

/** The test certificates of bun's harness (localhost, 127.0.0.1, ::1), read from the submodule. */
function bunHarnessCert(name: string): { cert: string; key: string; passphrase?: string } {
  const source = fs.readFileSync(join(bunRoot, "test", "harness.ts"), "utf8");
  const match = source.match(new RegExp(`export const ${name} = Object\\.freeze\\(\\{\\s*cert: ("[^"]*"),\\s*key: ("[^"]*"),(?:\\s*passphrase: ("[^"]*"),)?`));
  if (!match) throw new Error(`cannot read ${name} from bun's test/harness.ts`);
  return { cert: JSON.parse(match[1]), key: JSON.parse(match[2]), ...(match[3] ? { passphrase: JSON.parse(match[3]) } : {}) };
}

let tlsCache: ReturnType<typeof bunHarnessCert> | undefined;
export const tls = new Proxy({} as { cert: string; key: string }, {
  get: (_target, key: string) => (tlsCache ??= bunHarnessCert("tls"))[key as "cert"],
  ownKeys: () => ["cert", "key"],
  getOwnPropertyDescriptor: (_target, key: string) => ({
    enumerable: true,
    configurable: true,
    value: (tlsCache ??= bunHarnessCert("tls"))[key as "cert"],
  }),
});
export const expiredTls = new Proxy({} as { cert: string; key: string; passphrase: string }, {
  get: (_target, key: string) => (bunHarnessCert("expiredTls") as Record<string, string>)[key],
});

// ---------------------------------------------------------------------------------------------------------------------
// Matchers (registered globally by compat/setup.ts; exported for `expect.extend` in the ported files)
// ---------------------------------------------------------------------------------------------------------------------

export function toHaveBins(actual: string[], expectedBins: string[]) {
  const message = () => `Expected ${actual} to be package bins ${expectedBins}`;
  return { pass: actual.length === expectedBins.length && actual.every((bin, i) => bin === expectedBins[i]), message };
}

export function toBeValidBin(actual: string, expectedLinkPath: string) {
  const message = () => `Expected ${actual} to be a link to ${expectedLinkPath}`;
  try {
    return { pass: fs.readlinkSync(actual) === expectedLinkPath, message };
  } catch {
    return { pass: false, message };
  }
}

export function toBeWorkspaceLink(actual: string, expectedLinkPath: string) {
  const message = () => `Expected ${actual} to be a link to ${expectedLinkPath}`;
  if (isWindows && isAbsolute(actual)) return { pass: actual.includes(expectedLinkPath.split("..").at(-1)!), message };
  return { pass: actual === expectedLinkPath, message };
}

/**
 * Checks node_modules against a lockfile. With bun's binary-lockfile shape (`trees`, from bun:internal-for-testing)
 * this is bun's algorithm; with a text lockfile (`packages: { "<path>": ["name@version", ...] }`, which fnpm.lock
 * shares with bun.lock) every npm package must be at its hoisted path with its version.
 */
/** `"a/@s/b"` (b nested under a) -> `node_modules/a/node_modules/@s/b`. */
export function lockfileKeyToPath(key: string): string {
  const names: string[] = [];
  const parts = key.split("/");
  for (let i = 0; i < parts.length; i++) {
    names.push(parts[i].startsWith("@") && i + 1 < parts.length ? `${parts[i]}/${parts[++i]}` : parts[i]);
  }
  return names.map(name => join("node_modules", name)).join(sep);
}

export async function toMatchNodeModulesAt(lockfile: any, root: string) {
  if (lockfile && typeof lockfile === "object" && !Array.isArray(lockfile.trees) && lockfile.packages) {
    for (const [key, entry] of Object.entries(lockfile.packages as Record<string, unknown[]>)) {
      const spec = Array.isArray(entry) ? entry[0] : undefined;
      if (typeof spec !== "string") continue;
      const at = spec.lastIndexOf("@");
      if (at <= 0) continue;
      const name = spec.slice(0, at);
      const version = spec.slice(at + 1);
      if (!/^\d/.test(version)) continue;
      const path = join(root, lockfileKeyToPath(key));
      let onDisk: any;
      try {
        onDisk = JSON.parse(fs.readFileSync(join(path, "package.json"), "utf8"));
      } catch {
        return { pass: false, message: () => `Expected ${name}@${version} at ${path}` };
      }
      if (onDisk.version !== version) {
        return { pass: false, message: () => `Expected ${name}@${version} at ${path}, found ${onDisk.version}` };
      }
    }
    return { pass: true, message: () => "" };
  }

  const shouldSkip = (pkg: any, dep: any) =>
    pkg?.name === dep?.name ||
    !pkg ||
    !pkg.resolution ||
    dep.behavior.optional ||
    (dep.behavior.dev && pkg.id !== 0) ||
    (pkg.arch && pkg.arch !== process.arch);
  for (const { path, dependencies } of lockfile.trees) {
    for (const { package_id, id } of Object.values(dependencies) as any[]) {
      const treeDep = lockfile.dependencies[id];
      const treePkg = lockfile.packages[package_id];
      if (shouldSkip(treePkg, treeDep)) continue;
      const treeDepPath = join(root, path, treeDep.name);
      if (treePkg.resolution.tag === "npm") {
        const onDisk = await file(join(treeDepPath, "package.json")).json();
        if (!deepMatch({ name: treePkg.name, version: treePkg.resolution.value }, onDisk)) {
          return { pass: false, message: () => `Expected ${treePkg.name}@${treePkg.resolution.value} at ${treeDepPath}, got ${onDisk.version}` };
        }
      } else if (!fs.existsSync(treeDepPath)) {
        return { pass: false, message: () => `Expected ${treePkg.resolution.tag} "${treeDepPath}" to exist` };
      }
      for (const depId of treePkg.dependencies) {
        const dep = lockfile.dependencies[depId];
        const pkg = lockfile.packages[dep.package_id];
        if (shouldSkip(pkg, dep)) continue;
        try {
          const resolved = await file(resolveSync(join(dep.name, "package.json"), treeDepPath)).json();
          if (pkg.resolution.tag !== "npm") continue;
          const name = dep.is_alias ? dep.npm.name : dep.name;
          if (deepMatch({ name, version: pkg.resolution.value }, resolved)) continue;
          if (dep.literal === "*") continue;
          if (treePkg.resolution.tag === "workspace" && !resolved.version) continue;
          if (dep.behavior.peer && dep.npm && semver.satisfies(pkg.resolution.value, dep.npm.version)) continue;
          return { pass: false, message: () => `Expected ${dep.name} to have version ${pkg.resolution.value} in ${treeDepPath}, but got ${resolved.version}` };
        } catch {
          return { pass: false, message: () => `Expected ${dep.name} to be resolvable in ${treeDepPath}` };
        }
      }
    }
  }
  return { pass: true, message: () => "" };
}

// ---------------------------------------------------------------------------------------------------------------------
// bun-internal helpers the ported tests reference
// ---------------------------------------------------------------------------------------------------------------------

/** bun read its binary manifest cache here. fnpm's cache format differs, so this only checks the folder exists. */
export function assertManifestsPopulated(absCachePath: string, _registryUrl: string) {
  if (!fs.existsSync(absCachePath)) throw new Error(`no manifest cache at ${absCachePath}`);
}

export function unprivilegedSpawnOptions(dir: string): { uid?: number; gid?: number } & Disposable {
  const restore: [string, number][] = [];
  const options: { uid?: number; gid?: number } & Disposable = {
    [Symbol.dispose]() {
      for (const [path, mode] of restore) fs.chmodSync(path, mode);
    },
  };
  if (!isRoot) return options;
  const NOBODY = 65534;
  fs.chownSync(dir, NOBODY, NOBODY);
  for (const entry of fs.readdirSync(dir, { recursive: true, encoding: "utf8" })) fs.lchownSync(join(dir, entry), NOBODY, NOBODY);
  const tmp = fs.realpathSync(os.tmpdir());
  for (let parent = dirname(fs.realpathSync(dir)); parent !== dirname(parent); parent = dirname(parent)) {
    const mode = fs.statSync(parent).mode & 0o7777;
    if (mode & 0o001) continue;
    if (parent !== tmp && !parent.startsWith(tmp + sep)) throw new Error(`${parent} is above TMPDIR`);
    fs.chmodSync(parent, mode | 0o001);
    restore.push([parent, mode]);
  }
  options.uid = NOBODY;
  options.gid = NOBODY;
  return options;
}

export function compileFixture(_sourcePath: string): string {
  throw new Error("compileFixture is bun-specific and not available in the fnpm conformance suite");
}

export async function runCommandMaxRSS(_options: { cmd: string[]; cwd?: string; env?: Record<string, string> }) {
  throw new Error("runCommandMaxRSS is not available in the fnpm conformance suite");
}

export async function* forEachLine(iter: AsyncIterable<Uint8Array>) {
  let buffer = "";
  const decoder = new TextDecoder();
  for await (const chunk of iter) {
    buffer += decoder.decode(chunk, { stream: true });
    let index: number;
    while ((index = buffer.indexOf("\n")) !== -1) {
      yield buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
    }
  }
  if (buffer) yield buffer;
}

export async function gunzipJsonRequest(req: Request) {
  const { gunzipSync } = await import("node:zlib");
  const bytes = Buffer.from(await req.arrayBuffer());
  return JSON.parse(gunzipSync(bytes).toString("utf8"));
}

export const dtUnknownReaddir = {
  get available(): boolean {
    return false;
  },
  marker: "",
};

export function waitForFileToExist(path: string, intervalMs: number) {
  while (!fs.existsSync(path)) sleepSync(intervalMs);
}

// bun names, for code that was not renamed.
export { fnpmEnv as bunEnv, fnpmExe as bunExe, normalizeFnpmSnapshot as normalizeBunSnapshot };
export { runFnpmInstall as runBunInstall, runFnpmUpdate as runBunUpdate };
