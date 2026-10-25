import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** The published package name, used to identify our own manifest while walking up. */
const PACKAGE_NAME = "fuck-npm";

let cached: string | undefined;

/**
 * Read the version from the nearest `fuck-npm` package.json.
 *
 * Walks up from this module rather than importing the manifest, so the bundle
 * reports the installed package rather than a build-time literal. The name
 * match is why a test asserts the result is not "0.0.0": renaming the package
 * without updating PACKAGE_NAME makes every `--version` read the fallback.
 */
export function getCliVersion(): string {
  if (cached !== undefined) {
    return cached;
  }
  let current = dirname(fileURLToPath(import.meta.url));
  for (let depth = 0; depth < 6; depth += 1) {
    const packagePath = join(current, "package.json");
    if (existsSync(packagePath)) {
      try {
        const pkg = JSON.parse(readFileSync(packagePath, "utf-8")) as {
          name?: string;
          version?: string;
        };
        if (pkg.name === PACKAGE_NAME && pkg.version) {
          cached = pkg.version;
          return cached;
        }
      } catch {
        // An unreadable manifest on the way up is not ours; keep walking.
      }
    }
    const next = dirname(current);
    if (next === current) {
      break;
    }
    current = next;
  }
  cached = "0.0.0";
  return cached;
}
