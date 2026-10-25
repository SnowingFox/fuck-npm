import module from "node:module";
import { realpathSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { getCliVersion } from "./cli/version.ts";

/**
 * Shared startup for both bins.
 *
 * No shebang in any source file: tsup's banner adds one to each bundle, and a
 * second copy would land on line 2 as a syntax error.
 *
 * Everything statically imported from here must parse on old Node, because the
 * version guard can only explain a bad runtime if this file loads.
 */

export type BinName = "fnpm" | "fnpm-lock";

/** Oldest Node we support; `engines.node` in package.json must agree. */
export const MIN_NODE_VERSION = "20.19.0";

export function isSupportedNodeVersion(version: string): boolean {
  const actual = version.split(".").map((part) => Number.parseInt(part, 10));
  const required = MIN_NODE_VERSION.split(".").map((part) =>
    Number.parseInt(part, 10),
  );
  for (let index = 0; index < required.length; index += 1) {
    const left = actual[index] ?? 0;
    const right = required[index] ?? 0;
    if (left !== right) {
      return left > right;
    }
  }
  return true;
}

export function formatUnsupportedNodeVersionMessage(
  version: string,
  bin: BinName = "fnpm",
): string {
  return [
    `${bin} requires Node ${MIN_NODE_VERSION} or newer (found ${version}).`,
    "",
    "Upgrade Node, or run it through a version manager:",
    "  nvm install --lts && nvm use --lts",
    "",
  ].join("\n");
}

/**
 * Node's on-disk compile cache (22.1+) roughly halves the parse cost of the
 * bundle on every run after the first. Absent on 20.x, hence the optional call.
 */
export function enableCompileCache(): void {
  try {
    (module as { enableCompileCache?: () => unknown }).enableCompileCache?.();
  } catch {
    // A read-only cache dir must never stop the CLI from running.
  }
}

/**
 * Guard the runtime, then load the app.
 *
 * The dynamic import is load-bearing: a static import is hoisted above the
 * guard, so an old Node would die on a syntax error in a dependency instead of
 * printing the message below. `--version` alone is answered here, before cac
 * or the registry is loaded, because it is the one invocation scripts run in a
 * loop.
 */
export async function main(
  bin: BinName,
  argv: string[] = process.argv,
  nodeVersion: string = process.versions.node,
): Promise<number> {
  if (!isSupportedNodeVersion(nodeVersion)) {
    process.stderr.write(formatUnsupportedNodeVersionMessage(nodeVersion, bin));
    return 1;
  }

  const args = argv.slice(2);
  if (args.length === 1 && (args[0] === "--version" || args[0] === "-v")) {
    process.stdout.write(`${getCliVersion()}\n`);
    return 0;
  }

  const { runCli } = await import("./cli/app.ts");
  return runCli(bin, argv);
}

/**
 * Whether the module at `moduleUrl` is the script node was asked to run.
 *
 * Both sides go through realpathSync: `npm link` and global installs put a
 * symlink on PATH, so argv[1] is the link while import.meta.url is the real
 * file. Comparing them raw is false for exactly the installed case, and the CLI
 * exits 0 having printed nothing.
 */
export function isEntryPoint(moduleUrl: string, invokedPath: string): boolean {
  try {
    return realpathSync(fileURLToPath(moduleUrl)) === realpathSync(invokedPath);
  } catch {
    return false;
  }
}
