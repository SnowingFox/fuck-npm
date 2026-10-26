/**
 * postinstall: put the native executables behind `bin/fnpm` and `bin/fnpm-lock`.
 *
 * Package managers point `node_modules/.bin/fnpm` at `bin/fnpm`, which is
 * published as a small JS shim (`lib/shim.cjs`). After this script replaces it
 * with (a hardlink to) the native executable from the platform package,
 * running `fnpm` starts the native launcher directly, with no Node startup.
 * The shape follows bun's `packages/bun-release/src/npm/install.ts`:
 *
 * Single-package releases first download the current ABI archive using the
 * embedded SHA-512 manifest. The source workspace retains this fallback:
 *
 *   1. resolve `@fuck-npm/binding-<triple>` (normally an installed
 *      optionalDependency),
 *   2. if it is missing (`--no-optional`, `--omit=optional`), `npm install` it
 *      into a temp dir, then fall back to downloading the tarball from the
 *      registry and checking it against the packument's integrity,
 *   3. run `<exe> --version` and require our own version,
 *   4. swap: link (or copy) to a temp name next to the shim, then rename over
 *      it. Never writes into an existing file, so a store-backed (hardlinked)
 *      install is never modified in place.
 *
 * Nothing here can fail the install: on any problem the shim stays, and the
 * shim retries the native download on first execution. In source workspaces
 * it runs the native executable as a child or falls back to the TS CLI.
 * The swap is skipped where it would break the bin entry: on Windows (cmd-shim
 * bakes `node` into `.bin/fnpm.cmd`), under pnpm and yarn berry (their `.bin`
 * entries are scripts that run the target with node), and in a development
 * checkout (not inside node_modules).
 */

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  linkSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, sep } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const native = require("../lib/native.cjs");
const { nativeVersion, extractFiles } = require("../lib/archive.cjs");
export { nativeVersion, extractFiles };

const DEFAULT_REGISTRY = "https://registry.npmjs.org";

const debugEnabled = () => process.env.FNPM_DEBUG_POSTINSTALL === "1";
const debug = (...args) => {
  if (debugEnabled()) process.stderr.write(`fnpm postinstall: ${args.join(" ")}\n`);
};

/**
 * Why the swap must not happen here, or null when it may.
 * `env` is the environment the package manager gave the script.
 */
export function skipReason({ env = process.env, platform = process.platform, root = native.PACKAGE_ROOT } = {}) {
  if (env.FNPM_SKIP_NATIVE_SWAP === "1") return "FNPM_SKIP_NATIVE_SWAP=1";
  if (platform === "win32") return "Windows keeps the JS shim (cmd-shim runs bin/fnpm with node)";
  if (!root.split(sep).includes("node_modules")) return "not installed under node_modules (development checkout)";
  const agent = env.npm_config_user_agent ?? "";
  if (/^pnpm\//.test(agent)) return "pnpm runs .bin entries through node";
  const yarn = /^yarn\/(\d+)/.exec(agent);
  if (yarn && Number(yarn[1]) >= 2) return "yarn berry runs bins through node";
  return null;
}

/**
 * Replace `target` with `source`: hardlink (or copy) to a sibling temp name,
 * make it executable, then rename over `target`.
 */
export function swapIn(source, target) {
  const temp = join(dirname(target), `.${process.pid}.${Date.now()}.tmp`);
  try {
    try {
      linkSync(source, temp);
    } catch (error) {
      debug(`link ${source} failed (${error.code}); copying`);
      copyFileSync(source, temp);
    }
    chmodSync(temp, 0o755);
    renameSync(temp, target);
  } finally {
    rmSync(temp, { force: true });
  }
}

/**
 * `npm install` the platform package into a temp dir and move it to
 * `<root>/node_modules/<name>` (bun's `installBun`). Returns whether it did.
 */
export function installWithNpm(name, version, root, { npm = "npm", env = process.env } = {}) {
  const cwd = mkdtempSync(join(tmpdir(), "fnpm-postinstall-"));
  try {
    writeFileSync(join(cwd, "package.json"), "{}");
    const result = spawnSync(
      npm,
      [
        "install",
        "--loglevel=error",
        "--prefer-offline",
        "--no-audit",
        "--no-fund",
        "--progress=false",
        "--ignore-scripts",
        "--no-package-lock",
        `${name}@${version}`,
      ],
      {
        cwd,
        stdio: "pipe",
        encoding: "utf8",
        timeout: 120_000,
        windowsHide: true,
        shell: process.platform === "win32",
        env: { ...env, npm_config_global: undefined, npm_config_prefix: undefined },
      },
    );
    if (result.error || result.status !== 0) {
      debug(`npm install ${name}@${version} failed:`, result.error?.message ?? result.stderr);
      return false;
    }
    const installed = join(cwd, "node_modules", name);
    if (!existsSync(installed)) return false;
    const destination = join(native.platformInstallRoot(root), "node_modules", name);
    mkdirSync(dirname(destination), { recursive: true });
    rmSync(destination, { recursive: true, force: true });
    renameSync(installed, destination);
    return true;
  } finally {
    rmSync(cwd, { recursive: true, force: true });
  }
}

/** Whether `bytes` match an SRI string (`sha512-…`, any listed algorithm). */
export function matchesIntegrity(bytes, integrity) {
  if (typeof integrity !== "string" || integrity === "") return false;
  return integrity.split(/\s+/).some((entry) => {
    const dash = entry.indexOf("-");
    if (dash === -1) return false;
    const algorithm = entry.slice(0, dash);
    if (!["sha512", "sha384", "sha256", "sha1"].includes(algorithm)) return false;
    return createHash(algorithm).update(bytes).digest("base64") === entry.slice(dash + 1);
  });
}

/**
 * Download `name@version` from the registry, check it against the packument's
 * integrity, and extract the executables into `<root>/node_modules/<name>`.
 */
export async function downloadFromRegistry(name, version, root, { env = process.env, fetchImpl = fetch } = {}) {
  const registry = (env.npm_config_registry || DEFAULT_REGISTRY).replace(/\/+$/, "");
  const packumentUrl = `${registry}/${name.replace("/", "%2f")}`;
  const packument = await fetchImpl(packumentUrl, { headers: { accept: "application/json" } });
  if (!packument.ok) throw new Error(`GET ${packumentUrl}: ${packument.status}`);
  const meta = await packument.json();
  const dist = meta?.versions?.[version]?.dist;
  if (!dist?.tarball || !dist?.integrity) throw new Error(`${name}@${version} has no tarball or integrity`);
  const response = await fetchImpl(dist.tarball);
  if (!response.ok) throw new Error(`GET ${dist.tarball}: ${response.status}`);
  const tgz = Buffer.from(await response.arrayBuffer());
  if (!matchesIntegrity(tgz, dist.integrity)) throw new Error(`${name}@${version}: integrity check failed`);
  const destination = join(native.platformInstallRoot(root), "node_modules", name);
  const abi = name.slice(native.PLATFORM_PACKAGE_PREFIX.length);
  const executables = native.BINS.map((bin) => native.executableName(bin));
  const wanted = ["package.json", `fnpm.${abi}.node`, ...executables];
  const written = extractFiles(tgz, destination, wanted);
  for (const file of executables) {
    if (written.includes(file)) chmodSync(join(destination, file), 0o755);
  }
  return wanted.every((file) => written.includes(file));
}

/** The version an installed platform package declares, or null. */
function installedVersion(dir) {
  try {
    return JSON.parse(readFileSync(join(dir, "package.json"), "utf8")).version ?? null;
  } catch {
    return null;
  }
}

/** Find the platform executable for `bin`, installing the platform package if needed. */
async function resolveExecutables(abi, version, root, options) {
  const name = `${native.PLATFORM_PACKAGE_PREFIX}${abi}`;
  const find = () => native.findNativeExecutable("fnpm", { abi, root });
  if (find()) return true;
  const installed = native.platformPackageDir(abi, root);
  if (installed && installedVersion(installed) === version) {
    // Installed at our version without the executables: installing it again changes nothing,
    // and would cost an npm run and a download on every install.
    debug(`${name}@${version} has no native executables`);
    return false;
  }
  debug(`${name} is not installed; trying npm`);
  if (installWithNpm(name, version, root, options) && find()) return true;
  debug(`npm could not install ${name}; downloading it`);
  try {
    if ((await downloadFromRegistry(name, version, root, options)) && find()) return true;
  } catch (error) {
    debug(`download failed: ${error.message}`);
  }
  return false;
}

/**
 * The whole script. Returns a status for tests: `swapped`, `skipped: <why>` or
 * `kept shim: <why>`. Never throws.
 */
export async function postinstall({ root = native.PACKAGE_ROOT, env = process.env, platform = process.platform, abi = native.platformArchAbi(), npm, fetchImpl } = {}) {
  try {
    if (existsSync(join(root, 'lib/native-release.json'))) {
      // Download on every supported platform, including those that retain Node bin shims.
      await (await import('./install-native.js')).installNative({ root, abi, env, ...(fetchImpl ? { fetchImpl } : {}) });
    }
    const reason = skipReason({ env, platform, root });
    if (reason) return `skipped: ${reason}`;
    if (!abi) return `kept shim: no prebuilt executable for ${process.platform}-${process.arch}`;
    const version = native.packageVersion(root);
    const options = { env, ...(npm ? { npm } : {}), ...(fetchImpl ? { fetchImpl } : {}) };
    if (!(await resolveExecutables(abi, version, root, options))) {
      return `kept shim: ${native.PLATFORM_PACKAGE_PREFIX}${abi}@${version} is not available`;
    }
    const executables = native.BINS.map((bin) => [bin, native.findNativeExecutable(bin, { abi, root })]);
    for (const [bin, exe] of executables) {
      if (!exe) return `kept shim: the platform package has no ${bin} executable`;
      const reported = nativeVersion(exe);
      if (reported !== version) return `kept shim: ${exe} reports ${reported ?? "nothing"}, expected ${version}`;
    }
    for (const [bin, exe] of executables) {
      swapIn(exe, join(root, "bin", bin));
    }
    // The native fnpm runs `fnpm x` when started under either alias.
    const fnpm = executables.find(([bin]) => bin === "fnpm")?.[1];
    for (const alias of ["fnpmx", "fnpx"]) {
      if (fnpm && existsSync(join(root, "bin", alias))) swapIn(fnpm, join(root, "bin", alias));
    }
    return "swapped";
  } catch (error) {
    return `kept shim: ${error?.message ?? String(error)}`;
  }
}

/** Whether node was asked to run this file (realpaths: the package may sit behind a symlink). */
function invokedDirectly() {
  try {
    return process.argv[1] !== undefined && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url));
  } catch {
    return false;
  }
}

if (invokedDirectly()) {
  const status = await postinstall();
  debug(status);
  // Package managers usually hide script output; when shown, say why the shim stays.
  if (status.startsWith("kept shim")) process.stderr.write(`fnpm: ${status}; the JS shim will be used\n`);
}
