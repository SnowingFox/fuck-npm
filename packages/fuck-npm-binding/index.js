'use strict';

/**
 * Loader for the fnpm native core.
 *
 * Resolution order, first hit wins:
 *   1. $NAPI_RS_NATIVE_LIBRARY_PATH (napi-rs convention; an explicit override)
 *   2. ./fnpm.<abi>.node next to this file (`npm run build:native`)
 *   3. ../../crates/fnpm-napi/fnpm.<abi>.node (`napi build` run in the crate)
 *   4. the published platform package @fuck-npm/binding-<abi>
 *   5. a raw cargo artifact in $CARGO_TARGET_DIR or ../../target, release then debug
 *
 * Every candidate goes through process.dlopen, which loads any extension, so a
 * cargo `libfnpm_napi.so` works without being renamed to `.node`.
 */

const { existsSync, readFileSync } = require('node:fs');
const { join } = require('node:path');

const BINARY_NAME = 'fnpm';
const PACKAGE_NAME = '@fuck-npm/binding';
const CRATE_LIB = 'fnpm_napi';

function isMusl() {
  if (process.platform !== 'linux') return false;
  try {
    // Cheapest probe first: glibc's ldd is a shell script naming GNU libc.
    return readFileSync('/usr/bin/ldd', 'utf8').includes('musl');
  } catch {
    // No ldd (minimal images); fall back to the runtime's own report.
  }
  try {
    const report = typeof process.report?.getReport === 'function' ? process.report.getReport() : null;
    const header = report && (typeof report === 'string' ? JSON.parse(report).header : report.header);
    return !(header && header.glibcVersionRuntime);
  } catch {
    return false;
  }
}

/** napi-rs platform-arch-abi suffix, e.g. `linux-x64-gnu`, `darwin-arm64`, `win32-x64-msvc`. */
function platformArchAbi(platform = process.platform, arch = process.arch, musl = isMusl) {
  switch (platform) {
    case 'darwin':
      if (arch === 'arm64' || arch === 'x64') return `darwin-${arch}`;
      break;
    case 'linux':
      if (arch === 'arm64' || arch === 'x64') return `linux-${arch}-${musl() ? 'musl' : 'gnu'}`;
      break;
    case 'win32':
      if (arch === 'arm64' || arch === 'x64') return `win32-${arch}-msvc`;
      break;
    default:
      break;
  }
  return null;
}

function cargoArtifact(platform) {
  if (platform === 'win32') return `${CRATE_LIB}.dll`;
  if (platform === 'darwin') return `lib${CRATE_LIB}.dylib`;
  return `lib${CRATE_LIB}.so`;
}

function candidates(abi) {
  const list = [];
  const override = process.env.NAPI_RS_NATIVE_LIBRARY_PATH;
  if (override) list.push({ kind: 'file', path: override });
  if (abi) {
    const file = `${BINARY_NAME}.${abi}.node`;
    list.push({ kind: 'file', path: join(__dirname, file) });
    list.push({ kind: 'file', path: join(__dirname, '..', '..', 'crates', 'fnpm-napi', file) });
    list.push({ kind: 'package', path: `${PACKAGE_NAME}-${abi}` });
  }
  const targetDirs = [];
  if (process.env.CARGO_TARGET_DIR) targetDirs.push(process.env.CARGO_TARGET_DIR);
  targetDirs.push(join(__dirname, '..', '..', 'target'));
  for (const dir of targetDirs) {
    for (const profile of ['release', 'debug']) {
      list.push({ kind: 'file', path: join(dir, profile, cargoArtifact(process.platform)) });
    }
  }
  return list;
}

function dlopen(path) {
  const mod = { exports: {} };
  process.dlopen(mod, path);
  return mod.exports;
}

function load() {
  const abi = platformArchAbi();
  const attempts = [];
  for (const candidate of candidates(abi)) {
    try {
      if (candidate.kind === 'file') {
        if (!existsSync(candidate.path)) {
          attempts.push(`${candidate.path} (not found)`);
          continue;
        }
        return dlopen(candidate.path);
      }
      return require(candidate.path);
    } catch (error) {
      const reason = error && error.code === 'MODULE_NOT_FOUND' ? 'not installed' : String(error && error.message);
      attempts.push(`${candidate.path} (${reason})`);
    }
  }
  const error = new Error(
    [
      `Cannot load the fnpm native core for ${abi ?? `${process.platform}-${process.arch} (unsupported platform)`}.`,
      'Tried:',
      ...attempts.map((attempt) => `  - ${attempt}`),
      '',
      'Reinstall fuck-npm so npm picks the matching @fuck-npm/binding-* package,',
      'or build it locally: npm run build:native -w @fuck-npm/binding',
    ].join('\n'),
  );
  error.code = 'FNPM_BINDING_NOT_FOUND';
  throw error;
}

module.exports = load();
module.exports.__platformArchAbi = platformArchAbi;
