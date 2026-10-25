import { flag } from "../registry.ts";
import type { FlagSpec } from "../registry-types.ts";

/**
 * bun's package-manager flag tables, ported from
 * src/install/PackageManager/CommandLineArguments.rs. Names, short aliases,
 * value arity and descriptions follow bun; only `bunfig.toml` became
 * `fnpm.toml`. Order matters: help prints flags in declaration order.
 */

/** Global to every command of both bins. */
export const GLOBAL_FLAGS: readonly FlagSpec[] = Object.freeze([
  flag("-h, --help", "Display this message"),
  flag("-v, --version", "Print the version"),
  flag("--json", "Machine-readable output on stdout, including failures"),
  flag("-t, --tree", "Print the full command tree", { hidden: true }),
]);

const SHARED_HEAD: FlagSpec[] = [
  flag("-c, --config [path]", "Specify path to config file (fnpm.toml)", {
    optionalValue: true,
  }),
  flag("-y, --yarn", "Write a yarn.lock file (yarn v1)"),
];

const PRODUCTION: FlagSpec[] = [
  flag("-p, --production", "Don't install devDependencies"),
  flag("-P, --prod", "Alias of --production", { hidden: true }),
];

const SAVE: FlagSpec[] = [
  flag("--no-save", "Don't update package.json or save a lockfile"),
  flag("--save", "Save to package.json (true by default)"),
];

const LINK_SAVE: FlagSpec[] = [
  flag("--no-save", "Don't update package.json or save a lockfile (the default)"),
  flag("--save", "Update package.json and save a lockfile (false by default)"),
];

const SHARED_TAIL_AFTER_SAVE: FlagSpec[] = [
  flag("--ca [cert]", "Provide a Certificate Authority signing certificate", {
    repeatable: true,
  }),
  flag("--cafile [path]", "The same as `--ca`, but is a file path to the certificate"),
  flag("--dry-run", "Perform a dry run without making changes"),
  flag("--frozen-lockfile", "Disallow changes to lockfile"),
  flag(
    "-f, --force",
    "Always request the latest versions from the registry & reinstall all dependencies",
  ),
  flag("--cache-dir [path]", "Store & load cached data from a specific directory path"),
  flag("--no-cache", "Ignore manifest cache entirely"),
  flag("--silent", "Don't log anything"),
  flag("--quiet", "Only show tarball name when packing"),
  flag("--verbose", "Excessively verbose logging"),
  flag("--no-progress", "Disable the progress bar"),
  flag("--no-summary", "Don't print a summary"),
  flag("--no-verify", "Skip verifying integrity of newly downloaded packages"),
  flag(
    "--offline",
    "Never touch the network: resolve and install only from the local cache",
  ),
  flag(
    "--prefer-offline",
    "Use cached package metadata regardless of age; only fetch what is missing",
  ),
  flag(
    "--ignore-scripts",
    "Skip lifecycle scripts for all packages, including the project's package.json and trusted dependencies",
  ),
  flag(
    "--trust",
    "Add to trustedDependencies in the project's package.json and install the package(s)",
  ),
  flag("-g, --global", "Install globally"),
  flag("--cwd [dir]", "Set a specific cwd"),
  flag("--env-file [file]", "Load environment variables from the specified file(s)", {
    repeatable: true,
  }),
  flag("--no-env-file", "Disable automatic loading of .env files"),
  flag(
    "--backend [strategy]",
    'Platform-specific optimizations for installing dependencies: "clonefile" (macOS default), "hardlink" (default elsewhere), "symlink", "copyfile"',
  ),
  flag(
    "--registry [url]",
    "Use a specific registry by default, overriding .npmrc, fnpm.toml and environment variables",
  ),
  flag(
    "--concurrent-scripts [n]",
    "Maximum number of concurrent jobs for lifecycle scripts (default: 2x CPU cores)",
  ),
  flag(
    "--network-concurrency [n]",
    "Maximum number of concurrent network requests (default 64)",
  ),
  flag("--save-text-lockfile", "Save a text-based lockfile"),
  flag(
    "--omit [type]",
    "Exclude 'dev', 'optional', or 'peer' dependencies from install",
    { repeatable: true },
  ),
  flag("--lockfile-only", "Generate a lockfile without installing dependencies"),
  flag("--linker [strategy]", 'Linker strategy (one of "isolated" or "hoisted")'),
  flag(
    "--minimum-release-age [seconds]",
    "Only install packages published at least N seconds ago (security feature)",
  ),
  flag(
    "--minimum-release-age-excludes [pkg]",
    "Packages exempt from --minimum-release-age (comma-separated or repeated; replaces the configured list)",
    { repeatable: true },
  ),
  flag(
    "--cpu [arch]",
    "Override CPU architecture for optional dependencies (e.g., x64, arm64, * for all)",
    { repeatable: true },
  ),
  flag(
    "--os [os]",
    "Override operating system for optional dependencies (e.g., linux, darwin, * for all)",
    { repeatable: true },
  ),
  flag(
    "--libc [libc]",
    "Override libc for optional dependencies (e.g., glibc, musl, * for all)",
    { repeatable: true },
  ),
  flag("--use-system-ca", "Use the system's trusted certificate authorities"),
  flag("--use-openssl-ca", "Use OpenSSL's default CA store instead of the bundled one"),
  flag("--use-bundled-ca", "Use bundled CA store"),
];

/** bun's SHARED_PARAMS: every package-manager command. */
export const SHARED: readonly FlagSpec[] = [
  ...SHARED_HEAD,
  ...PRODUCTION,
  ...SAVE,
  ...SHARED_TAIL_AFTER_SAVE,
];

/** bun's LINK_SHARED_PARAMS: link and unlink only save when asked to. */
export const LINK_SHARED: readonly FlagSpec[] = [
  ...SHARED_HEAD,
  ...PRODUCTION,
  ...LINK_SAVE,
  ...SHARED_TAIL_AFTER_SAVE,
];

/**
 * bun's RUNTIME_FLAGS_PARAM: no-value runtime switches every package-manager
 * command silently accepts, so `bun --bun install` style muscle memory and
 * shared BUN_OPTIONS keep working. Spelled without dashes.
 */
export const IGNORED_RUNTIME_FLAGS: ReadonlySet<string> = new Set([
  "bun", "no-orphans", "workspaces", "parallel", "sequential", "no-exit-on-error",
  "revision", "watch", "hot", "no-clear-screen", "smol", "cpu-prof", "cpu-prof-md",
  "heap-prof", "heap-prof-md", "if-present", "no-install", "prefer-latest",
  "experimental-http2-fetch", "experimental-http3-fetch", "insecure-http-parser",
  "experimental-stream-iter", "expose-gc", "no-deprecation", "throw-deprecation",
  "no-warnings", "trace-warnings", "trace-deprecation", "pending-deprecation",
  "zero-fill-buffers", "tls-min-v1.0", "tls-min-v1.1", "tls-min-v1.2", "tls-min-v1.3",
  "tls-max-v1.2", "tls-max-v1.3", "redis-preconnect", "sql-preconnect", "no-addons",
  "no-ffi-cc", "trace-events-enabled", "trace-env", "trace-env-js-stack",
  "trace-env-native-stack", "trace-exit", "expose-internals", "preserve-symlinks",
  "preserve-symlinks-main", "no-macros", "jsx-side-effects", "ignore-dce-annotations",
]);
/** `-b` is RUNTIME_FLAGS_PARAM's only short spelling. */
export const IGNORED_RUNTIME_SHORT = "b";

const ADD_GROUP_FLAGS: FlagSpec[] = [
  flag("-d, --dev", 'Add dependency to "devDependencies"'),
  flag("-D, --development", "Alias of --dev", { hidden: true }),
  flag("--optional", 'Add dependency to "optionalDependencies"'),
  flag("--peer", 'Add dependency to "peerDependencies"'),
  flag(
    "--save-prod",
    'Add dependency to "dependencies", also when another group lists it',
  ),
  flag("-E, --exact", "Add the exact version instead of the ^range"),
];

const ADD_TAIL_FLAGS: FlagSpec[] = [
  flag(
    "--only-missing",
    "Only add dependencies to package.json if they are not already present",
  ),
  flag(
    "--catalog [name]",
    'Add the resolved version to the root package.json catalog and depend on it as "catalog:" (use --catalog=NAME for a named catalog)',
    { optionalValue: true },
  ),
];

export const INSTALL: readonly FlagSpec[] = [
  ...SHARED,
  ...ADD_GROUP_FLAGS,
  flag("-F, --filter [pattern]", "Install packages for the matching workspaces", {
    repeatable: true,
  }),
  flag(
    "-a, --analyze",
    "Analyze & install all dependencies of files passed as arguments recursively",
  ),
  ...ADD_TAIL_FLAGS,
];

export const ADD: readonly FlagSpec[] = [
  ...SHARED,
  ...ADD_GROUP_FLAGS,
  flag(
    "-F, --filter [pattern]",
    "Add the package(s) to the matching workspaces instead of the current package",
    { repeatable: true },
  ),
  flag(
    "-a, --analyze",
    "Recursively analyze & install dependencies of files passed as arguments",
  ),
  ...ADD_TAIL_FLAGS,
];

export const REMOVE: readonly FlagSpec[] = [
  ...SHARED,
  flag(
    "-F, --filter [pattern]",
    "Remove the package(s) from the matching workspaces instead of the current package",
    { repeatable: true },
  ),
];

export const UPDATE: readonly FlagSpec[] = [
  ...SHARED_HEAD,
  flag(
    "-p, --production",
    "Only update dependencies and optionalDependencies (alias: --prod)",
  ),
  flag("-P, --prod", "Alias of --production", { hidden: true }),
  ...SAVE,
  ...SHARED_TAIL_AFTER_SAVE,
  flag(
    "-L, --latest",
    "Update packages to their latest versions, ignoring the ranges in package.json",
  ),
  flag(
    "-i, --interactive",
    "Show an interactive list of outdated packages to select for update",
  ),
  flag("-F, --filter [pattern]", "Update packages for the matching workspaces", {
    repeatable: true,
  }),
  flag("-r, --recursive", "Update packages in all workspaces"),
  flag("-d, --dev", "Only update devDependencies"),
  flag("-D, --development", "Alias of --dev", { hidden: true }),
  flag("--no-optional", "Don't update optionalDependencies"),
  flag(
    "--depth [n]",
    "Only 0 is accepted: update direct dependencies and keep transitive dependencies at their locked versions",
  ),
  flag(
    "-E, --exact",
    "Write exact versions to package.json instead of ^ or ~ ranges",
  ),
];

export const OUTDATED: readonly FlagSpec[] = [
  ...SHARED,
  flag(
    "-F, --filter [pattern]",
    "Display outdated dependencies for each matching workspace",
    { repeatable: true },
  ),
  flag("-r, --recursive", "Check outdated packages in all workspaces"),
];

export const LINK: readonly FlagSpec[] = LINK_SHARED;
export const UNLINK: readonly FlagSpec[] = LINK_SHARED;

export const PATCH: readonly FlagSpec[] = [
  ...SHARED,
  flag("--commit", "Install a package containing modifications in `dir`"),
  flag(
    "--patches-dir [dir]",
    "The directory to put the patch file in (only if --commit is used)",
  ),
];

export const PATCH_COMMIT: readonly FlagSpec[] = [
  ...SHARED,
  flag("--patches-dir [dir]", "The directory to put the patch file"),
];

export const AUDIT: readonly FlagSpec[] = [
  ...SHARED,
  flag(
    "--audit-level [level]",
    "Only print advisories with severity greater than or equal to <level> (low, moderate, high, critical)",
  ),
  flag(
    "--ignore [id]",
    "Ignore advisories by GHSA or numeric advisory ID (repeatable)",
    { repeatable: true },
  ),
  flag(
    "-L, --latest",
    "Also apply fixes your declared ranges exclude, rewriting package.json",
  ),
];

export const INFO: readonly FlagSpec[] = SHARED;

const PACK_ONLY: FlagSpec[] = [
  flag("--destination [dir]", "The directory the tarball will be saved in"),
  flag("--filename [name]", "The filename of the tarball"),
  flag(
    "--gzip-level [level]",
    "Specify a custom compression level for gzip. Default is 9.",
  ),
];

export const PACK: readonly FlagSpec[] = [...SHARED, ...PACK_ONLY];

export const PUBLISH: readonly FlagSpec[] = [
  ...SHARED,
  flag("--access [level]", "Set access level for scoped packages"),
  flag("--tag [tag]", 'Tag the release. Default is "latest"'),
  flag("--otp [code]", "Provide a one-time password for authentication"),
  flag(
    "--auth-type [type]",
    "Specify the type of one-time password authentication (default is 'web')",
  ),
  flag(
    "--gzip-level [level]",
    "Specify a custom compression level for gzip. Default is 9.",
  ),
  flag(
    "--tolerate-republish",
    "Don't exit with code 1 when republishing over an existing version number",
  ),
  flag("-F, --filter [pattern]", "Publish each matching workspace package", {
    repeatable: true,
  }),
  flag(
    "-r, --recursive",
    "Publish every workspace package whose version is not on the registry",
  ),
  flag("--provenance [file]", "Generate provenance statements (or attach one with =FILE)", {
    optionalValue: true,
  }),
  flag("--no-provenance", "Do not generate provenance statements"),
  flag("--provenance-file [file]", "Attach an existing provenance bundle"),
];

const WHY_ONLY: FlagSpec[] = [
  flag("--top", "Show only the top dependency tree instead of nested ones"),
  flag("--depth [n]", "Maximum depth of the dependency tree to display"),
];

export const WHY: readonly FlagSpec[] = [...SHARED, ...WHY_ONLY];

export const DEDUPE: readonly FlagSpec[] = [
  ...SHARED,
  flag(
    "--check",
    "Exit with code 1 if the lockfile has duplicate versions that can be removed, without changing anything",
  ),
];

export const PRUNE: readonly FlagSpec[] = [
  ...SHARED,
  flag(
    "--check",
    "Exit with code 1 if node_modules has packages that can be removed, without deleting anything",
  ),
  flag(
    "-F, --filter [pattern]",
    "Only prune the node_modules folders of the matching workspaces",
    { repeatable: true },
  ),
];

/** `fnpm ls` / `fnpm pm ls`: bun's PM_SUBCOMMAND_FLAGS for `ls`. */
export const LS: readonly FlagSpec[] = [
  ...SHARED,
  flag("-a, --all", "List the entire dependency tree"),
  flag("--trusted", "List only trusted dependencies"),
  flag("--depth [n]", "Maximum depth of the dependency tree to display"),
];

/** Plain `bun pm <x>` subcommands accept SHARED plus nothing of their own. */
export const PM_PLAIN: readonly FlagSpec[] = SHARED;

export const PM_CACHE_PRUNE: readonly FlagSpec[] = [
  ...SHARED,
  flag(
    "--max-age [days]",
    "Remove cache entries older than this many days (default 30)",
  ),
];

export const PM_TRUST: readonly FlagSpec[] = [
  ...SHARED,
  flag("-a, --all", "Trust every untrusted dependency"),
];

export const PM_VERSION: readonly FlagSpec[] = [
  ...SHARED,
  flag("--git-tag-version [bool]", "Create a git commit and tag"),
  flag("--no-git-tag-version", "Do not create a git commit and tag"),
  flag("--allow-same-version", "Allow bumping to the same version"),
  flag("-m, --message [msg]", "Use the given message for the commit"),
  flag(
    "--preid [id]",
    "Identifier to be used to prefix premajor, preminor, prepatch or prerelease version increments",
  ),
];

export const PM_LICENSES: readonly FlagSpec[] = [
  ...SHARED,
  flag(
    "-F, --filter [pattern]",
    "List only the matching workspaces' dependencies",
    { repeatable: true },
  ),
  flag("-D, --dev", "List only the packages pulled in by devDependencies"),
  flag("--long", "Also print author, description and homepage"),
];

export const PM_DIFF: readonly FlagSpec[] = [
  ...SHARED,
  flag(
    "--diff [spec]",
    "A package spec or path to compare (may be given twice)",
    { repeatable: true },
  ),
  flag("--raw", "Compare file bytes as-is; skip the JS/CSS/JSON re-print"),
  flag("--unformatted", "Alias of --raw"),
  flag(
    "--unminify",
    "Rename short locals in lockstep in every JS file, not only ones that look minified",
  ),
  flag(
    "--minify",
    "Also normalise syntax (!0 vs true, quotes, parens…) so equivalent spellings collapse",
  ),
  flag(
    "-w, --ignore-space",
    "Show files that differ only in whitespace as 'whitespace only' instead of hunks",
  ),
  flag("--name-only", "Only list the files that differ"),
  flag("--stat", "Show a per-file change summary instead of hunks"),
  flag("-U, --unified [n]", "Lines of context around each change (default 3)"),
];

export const PM_PACK: readonly FlagSpec[] = PACK;
export const PM_WHY: readonly FlagSpec[] = WHY;

/** bun's RUN_ONLY_PARAMS + AUTO_OR_RUN_PARAMS + the BASE_PARAMS that apply to scripts. */
export const RUN: readonly FlagSpec[] = [
  flag("--silent", "Don't print the script command"),
  flag(
    "--elide-lines [n]",
    "Number of lines of script output shown when using --filter (default: 0, show all lines)",
  ),
  flag(
    "-F, --filter [pattern]",
    "Run a script in all workspace packages matching the pattern",
    { repeatable: true },
  ),
  flag(
    "-b, --bun",
    "Force a script or package to use the fnpm-managed node shim instead of the system node",
  ),
  flag(
    "--no-orphans",
    "Exit when the parent process dies, and on exit kill every descendant",
  ),
  flag(
    "--shell [shell]",
    "Control the shell used for package.json scripts. Supports either 'fnpm' or 'system'",
  ),
  flag(
    "--workspaces",
    'Run a script in all workspace packages (from the "workspaces" field in package.json)',
  ),
  flag("--parallel", "Run multiple scripts concurrently with Foreman-style output"),
  flag("--sequential", "Run multiple scripts sequentially with Foreman-style output"),
  flag(
    "--no-exit-on-error",
    "Continue running other scripts when one fails (with --parallel/--sequential)",
  ),
  flag("--if-present", "Exit 0 without an error when the script is missing"),
  flag("--env-file [file]", "Load environment variables from the specified file(s)", {
    repeatable: true,
  }),
  flag("--no-env-file", "Disable automatic loading of .env files"),
  flag("--cwd [dir]", "Run from this directory"),
  flag("-c, --config [path]", "Specify path to config file (fnpm.toml)", {
    optionalValue: true,
  }),
];

/** `fnpm exec`: runs a shell string the way a package.json script would. */
export const EXEC: readonly FlagSpec[] = [
  flag("--cwd [dir]", "Run from this directory"),
  flag("--env-file [file]", "Load environment variables from the specified file(s)", {
    repeatable: true,
  }),
  flag("--no-env-file", "Disable automatic loading of .env files"),
  flag("--shell [shell]", "Control the shell used. Supports either 'fnpm' or 'system'"),
  flag("--silent", "Don't print the command"),
];

/** bunx's own flags (CommandLineArguments.rs bunx parsing + bunx_command.rs). */
export const X: readonly FlagSpec[] = [
  flag("-p, --package [pkg]", "The package that provides the binary, when its name differs"),
  flag("--no-install", "Never install: run only what is on PATH, in node_modules or in the cache"),
  flag("-b, --bun", "Force the binary to run with the fnpm-managed node shim"),
  flag("--verbose", "Excessively verbose logging of the install"),
  flag("--silent", "Don't log anything from the install"),
  flag("--trust", "Allow the package's lifecycle scripts to run"),
  flag("--ignore-scripts", "Skip the package's lifecycle scripts"),
  flag("--env-file [file]", "Load environment variables from the specified file(s)", {
    repeatable: true,
  }),
  flag("--cwd [dir]", "Run from this directory"),
  flag(
    "--minimum-release-age [seconds]",
    "Only install versions published at least N seconds ago",
  ),
  flag("--registry [url]", "Use a specific registry for this install"),
  flag("--ca [cert]", "Provide a Certificate Authority signing certificate", {
    repeatable: true,
  }),
  flag("--cafile [path]", "The same as `--ca`, but is a file path to the certificate"),
  flag("--use-system-ca", "Use the system's trusted certificate authorities"),
  flag("--use-openssl-ca", "Use OpenSSL's default CA store instead of the bundled one"),
  flag("--use-bundled-ca", "Use bundled CA store"),
];

export const INIT: readonly FlagSpec[] = [
  flag("-y, --yes", "Accept every default without prompting"),
  flag("-m, --minimal", "Only write package.json"),
  flag("--cwd [dir]", "Initialize in this directory"),
];

const STORE_COMMON: FlagSpec[] = [
  flag("--cwd [dir]", "Set a specific cwd"),
  flag("--cache-dir [path]", "Use this store/cache directory"),
];

export const STORE_PATH: readonly FlagSpec[] = STORE_COMMON;
export const STORE_STATUS: readonly FlagSpec[] = STORE_COMMON;
export const STORE_PRUNE: readonly FlagSpec[] = [
  ...STORE_COMMON,
  flag("--dry-run", "Print what would be removed without deleting anything"),
];

export const DOCTOR: readonly FlagSpec[] = [
  flag("--cwd [dir]", "Diagnose this project"),
  flag("--skip-network", "Do not probe the configured registries"),
];
