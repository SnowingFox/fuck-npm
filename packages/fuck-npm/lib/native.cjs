'use strict';

/**
 * Locating the native fnpm executables. Shared by the bin shims (`bin/fnpm`,
 * `bin/fnpm-lock`) and the postinstall swap (`scripts/postinstall.js`).
 *
 * The executables ship in the napi-rs platform packages
 * (`@fuck-npm/binding-<platform>-<arch>[-gnu|-musl|-msvc]`) next to
 * `fnpm.<triple>.node`. CommonJS and Node built-ins only: the shim runs this on
 * every invocation that did not get the postinstall swap, so it must stay cheap.
 */

const { existsSync, readFileSync } = require('node:fs');
const { dirname, join } = require('node:path');

const PACKAGE_ROOT = join(__dirname, '..');
const PLATFORM_PACKAGE_PREFIX = '@fuck-npm/binding-';
const BINS = ['fnpm', 'fnpm-lock'];

/** musl libc: glibc's ldd is a shell script naming GNU libc, musl's is not. */
function isMusl(readFile = readFileSync, report = process.report) {
  try {
    return readFile('/usr/bin/ldd', 'utf8').includes('musl');
  } catch {
    // No ldd (minimal images); fall back to the runtime's own report.
  }
  try {
    const raw = report && typeof report.getReport === 'function' ? report.getReport() : null;
    const header = raw && (typeof raw === 'string' ? JSON.parse(raw).header : raw.header);
    return !(header && header.glibcVersionRuntime);
  } catch {
    return false;
  }
}

/**
 * The napi-rs suffix for this machine (`linux-x64-gnu`, `darwin-arm64`,
 * `win32-x64-msvc`), or null when no prebuilt exists. Must agree with
 * `packages/fuck-npm-binding/index.js`.
 */
function platformArchAbi(platform = process.platform, arch = process.arch, musl = isMusl) {
  if (arch !== 'arm64' && arch !== 'x64') return null;
  switch (platform) {
    case 'darwin':
      return `darwin-${arch}`;
    case 'linux':
      return `linux-${arch}-${musl() ? 'musl' : 'gnu'}`;
    case 'win32':
      return `win32-${arch}-msvc`;
    default:
      return null;
  }
}

/** `fnpm` / `fnpm.exe`. */
function executableName(bin, platform = process.platform) {
  return platform === 'win32' ? `${bin}.exe` : bin;
}

/**
 * Path of the native executable `bin` inside the installed platform package,
 * or null. Resolved from this package, so it finds the copy the package
 * manager installed for us (hoisted or nested).
 */
function findNativeExecutable(bin, options = {}) {
  const abi = options.abi !== undefined ? options.abi : platformArchAbi();
  if (!abi) return null;
  const paths = options.paths || [PACKAGE_ROOT];
  let manifest;
  try {
    manifest = require.resolve(`${PLATFORM_PACKAGE_PREFIX}${abi}/package.json`, { paths });
  } catch {
    return null;
  }
  const exe = join(dirname(manifest), executableName(bin, options.platform));
  return existsSync(exe) ? exe : null;
}

/** The TS CLI bundle for `bin`. */
function jsEntry(bin, root = PACKAGE_ROOT) {
  return join(root, 'dist', `${bin}.js`);
}

/** Our own version, which the native executables must report. */
function packageVersion(root = PACKAGE_ROOT) {
  return JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')).version;
}

module.exports = {
  BINS,
  PACKAGE_ROOT,
  PLATFORM_PACKAGE_PREFIX,
  executableName,
  findNativeExecutable,
  isMusl,
  jsEntry,
  packageVersion,
  platformArchAbi,
};
