import { longName, takesValue } from "./registry.ts";
import type { CommandLeaf } from "./registry-types.ts";
import { GLOBAL_FLAGS, IGNORED_RUNTIME_FLAGS, IGNORED_RUNTIME_SHORT } from "./trees/flags.ts";

/**
 * npm, yarn and pnpm flags on `install`, `ci`, `add`, `remove` and `update`,
 * ported from bun's CommandLineArguments.rs (FOREIGN_FLAGS,
 * IGNORED_FOREIGN_FLAGS, UPDATE_FILTERS and clap's IgnoredLongFlags rules) so
 * scripts written for those tools keep working.
 */

type ForeignFlag = { alias: string } | "satisfied";

/** An entry written `--name=value` also matches the two tokens `--name value`. */
export const FOREIGN_FLAGS: ReadonlyArray<readonly [string, ForeignFlag]> = [
  ["--save-dev", { alias: "--dev" }],
  ["--save-exact", { alias: "--exact" }],
  ["--save-optional", { alias: "--optional" }],
  ["--save-peer", { alias: "--peer" }],
  ["--save-catalog", { alias: "--catalog" }],
  ["--location=global", { alias: "--global" }],
  ["--no-optional", { alias: "--omit=optional" }],
  ["--ignore-optional", { alias: "--omit=optional" }],
  // Not `--production`, which also freezes the lockfile.
  ["--only=prod", { alias: "--omit=dev" }],
  ["--only=production", { alias: "--omit=dev" }],
  ["--production=true", { alias: "--production" }],
  ["--package-lock-only", { alias: "--lockfile-only" }],
  ["--immutable", { alias: "--frozen-lockfile" }],
  ["--shamefully-hoist", { alias: "--linker=hoisted" }],
  ["--progress=false", { alias: "--no-progress" }],
  ["--loglevel=silent", { alias: "--silent" }],
  ["--loglevel=error", { alias: "--quiet" }],
  ["--loglevel=warn", { alias: "--quiet" }],
  ["--loglevel=notice", "satisfied"],
  ["--loglevel=http", "satisfied"],
  ["--loglevel=info", "satisfied"],
  ["--loglevel=verbose", { alias: "--verbose" }],
  ["--loglevel=silly", { alias: "--verbose" }],
  // fnpm install never audits, prints funding messages or checks for updates of itself.
  ["--no-audit", "satisfied"],
  ["--audit=false", "satisfied"],
  ["--no-fund", "satisfied"],
  ["--fund=false", "satisfied"],
  ["--no-update-notifier", "satisfied"],
  ["--update-notifier=false", "satisfied"],
  // Peer conflicts only warn, engines are not enforced, scripts run as the
  // current user, and nothing prompts.
  ["--legacy-peer-deps", "satisfied"],
  ["--ignore-engines", "satisfied"],
  ["--unsafe-perm", "satisfied"],
  ["--non-interactive", "satisfied"],
  ["--prefer-frozen-lockfile", "satisfied"],
  ["--install-strategy=hoisted", { alias: "--linker=hoisted" }],
  ["--install-strategy=linked", { alias: "--linker=isolated" }],
  ["--node-linker=hoisted", { alias: "--linker=hoisted" }],
  ["--node-linker=node-modules", { alias: "--linker=hoisted" }],
  ["--node-linker=isolated", { alias: "--linker=isolated" }],
  ["--node-linker=pnpm", { alias: "--linker=isolated" }],
];

/** Boolean flags of other tools with no fnpm equivalent: skipped. */
export const IGNORED_FOREIGN_FLAGS: ReadonlySet<string> = new Set([
  "aggregate-output", "all", "allow-same-version", "audit", "auto-dedupe",
  "auto-install-peers", "bin-links", "cached", "caret", "changeset", "check-cache",
  "check-files", "check-resolutions", "ci", "color", "dangerously-allow-all-builds",
  "dangerously-allow-all-scripts", "dedupe-peer-dependents", "default-rc", "disable-pnp",
  "emoji", "enable-pnp", "engine-strict", "fail-if-no-match", "fix-lockfile", "fixed",
  "flat", "focus", "foreground-scripts", "format-package-lock", "frozen-store", "fund",
  "global-style", "har", "hoist", "ignore-manifest-check", "ignore-platform",
  "ignore-pnpmfile", "ignore-workspace", "ignore-workspace-root-check", "immutable-cache",
  "include-github-actions", "include-workspace-root", "inline-builds", "install-links", "iwr",
  "json", "legacy-bundling", "link-duplicates", "link-workspace-packages",
  "lockfile", "long", "merge-git-branch-lockfiles", "no-frozen-lockfile", "no-immutable",
  "no-prefer-frozen-lockfile", "no-production", "no-proxy", "node-version-check",
  "omit-lockfile-registry-resolved", "package-lock", "parseable", "patches", "pnp",
  "prefer-dedupe", "prefer-online", "prefer-workspace-packages", "progress", "pure-lockfile",
  "rebuild-bundle", "recursive", "refresh-lockfile", "resolution-only",
  "resolve-peers-from-workspace-root", "reverse", "runtime", "save-build", "save-bundle",
  "save-types", "scripts-prepend-node-path", "shared-workspace-lockfile",
  "shrinkwrap", "side-effects-cache", "skip-integrity-check", "sort", "stream",
  "strict-allow-scripts", "strict-peer-dependencies", "strict-peer-deps", "strict-semver",
  "strict-ssl", "symlink", "tilde", "timing", "trust-lockfile", "unicode",
  "update-checksums", "update-notifier", "use-running-store-server", "use-stderr",
  "use-store-server", "verify-store-integrity", "workspace-root", "workspaces-update", "ws",
  "yes",
  // Also in FOREIGN_FLAGS, which does not match them when they carry a value.
  "ignore-engines", "ignore-optional", "immutable", "legacy-peer-deps", "non-interactive",
  "package-lock-only", "prefer-frozen-lockfile", "save-catalog", "save-dev", "save-exact",
  "save-optional", "save-peer", "save-prod", "shamefully-hoist", "unsafe-perm",
]);

/** Value-taking options of other tools: skipped along with their value. */
export const IGNORED_FOREIGN_OPTIONS: ReadonlySet<string> = new Set([
  "allow-build", "allow-directory", "allow-file", "allow-git", "allow-remote",
  "allow-scripts", "also", "before", "cache", "cache-folder", "cache-max",
  "cache-min", "cert", "changed-files-ignore-pattern", "child-concurrency", "dir",
  "enjoy-by", "fetch-min-speed-ki-bps", "fetch-retries", "fetch-retry-factor", "fetch-retry-maxtimeout",
  "fetch-retry-mintimeout", "fetch-timeout", "fetch-warn-timeout-ms", "filter-prod",
  "global-folder", "global-pnpmfile", "globalconfig", "hoist-pattern", "http-proxy",
  "https-proxy", "include", "install-strategy", "key", "link-folder", "local-address",
  "location", "lockfile-dir", "lockfile-version", "loglevel", "logs-dir", "logs-max",
  "maxsockets", "merge-git-branch-lockfiles-branch-pattern", "min-release-age", "mode",
  "mutex", "network-timeout", "node-gyp", "node-linker", "node-options",
  "noproxy", "npmrc-auth-file", "only", "otp", "package-import-method", "pnpmfile",
  "pnpr-server", "preferred-cache-folder", "prefix", "proxy", "public-hoist-pattern",
  "replace-registry-host", "reporter", "save-catalog-name", "save-prefix", "scope",
  "script-shell", "state-dir", "store-dir", "tag", "test-pattern", "umask", "use-yarnrc",
  "user-agent", "userconfig", "virtual-store-dir", "workspace", "workspace-concurrency",
  "workspace-packages",
]);

/** pnpm's `--config.<setting>=<value>`. */
export const IGNORED_FOREIGN_PREFIXES: readonly string[] = ["config."];

/** On `update` these select which dependencies to update, so nothing aliases to them. */
const UPDATE_FILTERS = new Set(["--dev", "--production"]);

type OwnFlags = { names: Set<string>; booleans: Set<string> };

const ownCache = new WeakMap<CommandLeaf, OwnFlags>();

/** The leaf's flags plus the globals (`--json` is ours, not npm's ignored one). */
function ownFlags(leaf: CommandLeaf): OwnFlags {
  const cached = ownCache.get(leaf);
  if (cached) {
    return cached;
  }
  const names = new Set<string>();
  const booleans = new Set<string>();
  for (const spec of [...leaf.flags, ...GLOBAL_FLAGS]) {
    const name = longName(spec.raw);
    names.add(name);
    if (!takesValue(spec)) {
      booleans.add(name);
    }
  }
  const own = { names, booleans };
  ownCache.set(leaf, own);
  return own;
}

const rewriteCache = new WeakMap<CommandLeaf, Map<string, string>>();

/**
 * bun's per-command rewrite table: a foreign flag is skipped when the command
 * has a flag of that name, and an alias applies only when the command accepts
 * the fnpm flag it maps to. Otherwise `--save-*` (which only picks a group for
 * a new dependency) is dropped, and anything else is left for the unknown-flag
 * check. An empty replacement removes the flag.
 */
function rewritesFor(leaf: CommandLeaf, own: OwnFlags): Map<string, string> {
  const cached = rewriteCache.get(leaf);
  if (cached) {
    return cached;
  }
  const rewrites = new Map<string, string>();
  for (const [foreign, kind] of FOREIGN_FLAGS) {
    if (own.names.has(foreign.slice(2))) {
      continue;
    }
    if (kind === "satisfied") {
      rewrites.set(foreign, "");
      continue;
    }
    const target = kind.alias;
    const targetName = target.slice(2).split("=")[0] as string;
    const isFilter = leaf.foreignFlags === "update" && UPDATE_FILTERS.has(target);
    if (own.names.has(targetName) && !isFilter) {
      rewrites.set(foreign, target);
    } else if (foreign.startsWith("--save-")) {
      rewrites.set(foreign, "");
    }
  }
  rewriteCache.set(leaf, rewrites);
  return rewrites;
}

/**
 * clap's IgnoredLongFlags::value_is_next_arg: undefined when `name` is not
 * skipped, otherwise whether the following token is its value.
 */
function ignoredValueIsNext(
  name: string,
  next: string | undefined,
  own: OwnFlags,
): boolean | undefined {
  if (IGNORED_FOREIGN_OPTIONS.has(name)) {
    return next !== undefined && !next.startsWith("-");
  }
  const positive = name.startsWith("no-") ? name.slice(3) : undefined;
  if (
    IGNORED_FOREIGN_FLAGS.has(name) ||
    (positive !== undefined &&
      (IGNORED_FOREIGN_FLAGS.has(positive) ||
        IGNORED_FOREIGN_OPTIONS.has(positive) ||
        own.names.has(positive)))
  ) {
    return next === "true" || next === "false";
  }
  if (IGNORED_FOREIGN_PREFIXES.some((prefix) => name.startsWith(prefix))) {
    return false;
  }
  return undefined;
}

/**
 * Apply the foreign-flag table to a leaf's own tokens. Returns the tokens with
 * aliases rewritten and ignored flags (and their values) removed. Positionals
 * and the command's own flags pass through untouched.
 */
export function translateForeignFlags(tokens: readonly string[], leaf: CommandLeaf): string[] {
  if (leaf.foreignFlags === undefined) {
    return [...tokens];
  }
  const own = ownFlags(leaf);
  const rewrites = rewritesFor(leaf, own);
  const out: string[] = [];

  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index] as string;
    if (!token.startsWith("--") || token === "--") {
      out.push(token);
      continue;
    }
    const next = tokens[index + 1];
    const equals = token.indexOf("=");
    const name = equals === -1 ? token.slice(2) : token.slice(2, equals);

    const direct = rewrites.get(token);
    if (direct !== undefined) {
      if (direct !== "") out.push(direct);
      continue;
    }
    if (equals === -1 && next !== undefined) {
      const joined = rewrites.get(`${token}=${next}`);
      if (joined !== undefined) {
        if (joined !== "") out.push(joined);
        index += 1;
        continue;
      }
    }

    // A flag of the command's own (or `--no-<own boolean>`) is not foreign.
    if (own.names.has(name)) {
      out.push(token);
      continue;
    }
    if (name.startsWith("no-") && own.booleans.has(name.slice(3)) && equals === -1) {
      out.push(token);
      continue;
    }

    const valueIsNext = ignoredValueIsNext(name, next, own);
    if (valueIsNext !== undefined) {
      if (valueIsNext && equals === -1) {
        index += 1;
      }
      continue;
    }
    out.push(token);
  }
  return out;
}

/**
 * Drop bun's runtime-only switches (`--bun`, `--watch`, `-b`, ...) that every
 * package-manager command accepts and ignores, unless the leaf declares one
 * itself.
 */
export function stripRuntimeFlags(tokens: readonly string[], leaf: CommandLeaf): string[] {
  if (leaf.ignoresRuntimeFlags !== true) {
    return [...tokens];
  }
  const own = ownFlags(leaf);
  return tokens.filter((token) => {
    if (token === `-${IGNORED_RUNTIME_SHORT}`) {
      return false;
    }
    if (!token.startsWith("--")) {
      return true;
    }
    const name = token.slice(2);
    return own.names.has(name) || !IGNORED_RUNTIME_FLAGS.has(name);
  });
}
