import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** The published package name, used to identify our own manifest while walking up. */
const PACKAGE_NAME = "fuck-npm";

let cached: string | undefined;

/** Set by the build (tsup `define`); absent when the sources run unbundled (tests). */
declare const __FNPM_REVISION__: string | undefined;

/** The short git revision the build was made from, `""` when unknown. */
export function getCliRevision(): string {
  return typeof __FNPM_REVISION__ === "string" ? __FNPM_REVISION__ : "";
}

/** The version headers print, like bun's `version_with_sha`: `0.1.0 (abc1234)`, or the bare version. */
export function getCliVersionWithSha(): string {
  const revision = getCliRevision();
  return revision ? `${getCliVersion()} (${revision})` : getCliVersion();
}

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
