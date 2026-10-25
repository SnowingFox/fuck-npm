'use strict';

/**
 * The JS shim behind `bin/fnpm` and `bin/fnpm-lock` when the postinstall swap
 * did not happen (`--ignore-scripts`, a package manager that does not run or
 * trust our postinstall, Windows).
 *
 * With a native executable for this platform it runs that as a child with
 * inherited stdio, forwards signals and exits the way the child did. Without
 * one it runs the TS CLI in this process. Either way the CLI behaves the same;
 * the shim only adds Node's startup.
 */

const { spawn } = require('node:child_process');
const { pathToFileURL } = require('node:url');
const { findNativeExecutable, jsEntry } = require('./native.cjs');

const FORWARDED_SIGNALS = ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT', 'SIGBREAK', 'SIGWINCH'];

/** Run the TS CLI for `bin` in this process, as if node had been given it directly. */
function runJs(bin) {
  const entry = jsEntry(bin);
  // The bundle only runs when it is the entry point (argv[1], compared by realpath).
  process.argv[1] = entry;
  return import(pathToFileURL(entry).href);
}

/** Run the native executable `exe` for `bin` with our argv; resolves never (exits). */
function runNative(bin, exe) {
  const env = { ...process.env };
  // The executable lives in the platform package; point it at our bundle.
  if (!env.FNPM_JS_ENTRY) env.FNPM_JS_ENTRY = jsEntry('fnpm');
  if (!env.FNPM_LOCK_JS_ENTRY) env.FNPM_LOCK_JS_ENTRY = jsEntry('fnpm-lock');

  const child = spawn(exe, process.argv.slice(2), { stdio: 'inherit', env, windowsHide: false });
  const listeners = new Map();
  for (const signal of FORWARDED_SIGNALS) {
    const forward = () => {
      try {
        child.kill(signal);
      } catch {
        // The child is already gone.
      }
    };
    try {
      process.on(signal, forward);
      listeners.set(signal, forward);
    } catch {
      // Not a signal on this platform.
    }
  }
  const cleanup = () => {
    for (const [signal, forward] of listeners) process.removeListener(signal, forward);
  };

  child.on('error', (error) => {
    cleanup();
    if (error && (error.code === 'ENOENT' || error.code === 'EACCES' || error.code === 'ENOEXEC')) {
      // An unusable executable (wrong libc, noexec mount): the TS CLI still works.
      runJs(bin).catch(fail);
      return;
    }
    fail(error);
  });
  child.on('exit', (code, signal) => {
    cleanup();
    if (signal) {
      // Die the same way, so a parent shell sees 128 + n.
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

function fail(error) {
  process.stderr.write(`${error && error.stack ? error.stack : String(error)}\n`);
  process.exitCode = 1;
}

/** Entry point of the bin shims. `fnpmx` is `fnpm x`. */
function main(bin) {
  if (bin === 'fnpmx') {
    process.argv.splice(2, 0, 'x');
    bin = 'fnpm';
  }
  const exe = findNativeExecutable(bin);
  if (exe) {
    runNative(bin, exe);
    return;
  }
  runJs(bin).catch(fail);
}

module.exports = { main, runJs, runNative };
