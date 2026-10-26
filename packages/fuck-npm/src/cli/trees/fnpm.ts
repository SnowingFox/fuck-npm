import type {
  CommandAlias,
  CommandGroup,
  CommandLeaf,
  PositionalSpec,
  Registry,
} from "../registry-types.ts";
import * as F from "./flags.ts";

/**
 * The `fnpm` command surface, as data. Help, `--tree`, cac registration,
 * flag scoping and completions all read this tree, so a command is one node
 * here plus (optionally) a handler in commands/index.ts. Commands without a
 * dedicated handler go to the native core as `{command: path, ...}`.
 */

const pos = (
  name: string,
  description: string,
  extra: Partial<PositionalSpec> = {},
): PositionalSpec => ({ name, description, required: false, ...extra });

const pm = (
  name: string,
  description: string,
  extra: Partial<CommandLeaf> = {},
): CommandLeaf => ({
  kind: "leaf",
  name,
  description,
  flags: F.PM_PLAIN,
  ignoresRuntimeFlags: true,
  ...extra,
});

const HELP: CommandLeaf = {
  kind: "leaf",
  name: "help",
  description: "Show help for a command",
  usageText: "fnpm help [...command] [--tree]",
  positionals: [
    pos("command", "Command path to describe, e.g. pm cache rm", { variadic: true }),
  ],
  flags: [],
  exampleGroups: [
    {
      title: "Explore the CLI",
      examples: [
        ["fnpm help", "Root help with the full command list."],
        ["fnpm help pm cache rm", "Detailed usage for one nested command."],
        ["fnpm help --tree", "Every registered command at a glance (useful for agents)."],
      ],
    },
  ],
};

const INSTALL: CommandLeaf = {
  kind: "leaf",
  name: "install",
  aliases: ["i"],
  description: "Install all dependencies of the current project",
  positionals: [
    pos("packages", "Packages to add instead (fnpm install <pkg> runs fnpm add)", {
      variadic: true,
    }),
  ],
  flags: F.INSTALL,
  foreignFlags: "install",
  ignoresRuntimeFlags: true,
  exampleGroups: [
    {
      title: "Everyday installs",
      examples: [
        ["fnpm install", "Install from fnpm.lock, resolving whatever changed."],
        ["fnpm install --frozen-lockfile", "Fail instead of changing the lockfile (CI)."],
        ["fnpm install --production", "Skip devDependencies."],
        ["fnpm install --linker hoisted", "Lay out node_modules like npm."],
      ],
    },
  ],
  notes: [
    "npm, yarn and pnpm flags are translated (--save-dev → --dev, --immutable → --frozen-lockfile, ...).",
    "`fnpm install -g <pkg>` and `fnpm install <pkg>` run `fnpm add`.",
  ],
};

const CI: CommandLeaf = {
  kind: "leaf",
  name: "ci",
  description: "Install exactly what fnpm.lock says (install --frozen-lockfile)",
  flags: F.INSTALL,
  foreignFlags: "install",
  ignoresRuntimeFlags: true,
};

const ADD: CommandLeaf = {
  kind: "leaf",
  name: "add",
  aliases: ["a"],
  description: "Add dependencies to package.json and install them",
  positionals: [
    pos("packages", '"name" or "name@version" of package(s) to install', {
      required: true,
      variadic: true,
    }),
  ],
  flags: F.ADD,
  foreignFlags: "install",
  ignoresRuntimeFlags: true,
  exampleGroups: [
    {
      title: "Adding packages",
      examples: [
        ["fnpm add react", "Add the latest react to dependencies with a ^range."],
        ["fnpm add -d typescript", "Add to devDependencies."],
        ["fnpm add -E zod@3.23.8", "Pin an exact version."],
        ["fnpm add -g serve", "Install a global binary."],
      ],
    },
  ],
};

const REMOVE: CommandLeaf = {
  kind: "leaf",
  name: "remove",
  aliases: ["rm", "r", "uninstall", "un"],
  description: "Remove dependencies from package.json",
  positionals: [
    pos("packages", '"name" of package(s) to remove from package.json', {
      required: true,
      variadic: true,
    }),
  ],
  flags: F.REMOVE,
  foreignFlags: "install",
  ignoresRuntimeFlags: true,
};

const UPDATE: CommandLeaf = {
  kind: "leaf",
  name: "update",
  aliases: ["up", "upgrade"],
  description: "Update dependencies to the newest versions their ranges allow",
  positionals: [
    pos("packages", '"name" or pattern ("@scope/*", "!name") of packages to update', {
      variadic: true,
    }),
  ],
  flags: F.UPDATE,
  foreignFlags: "update",
  ignoresRuntimeFlags: true,
  exampleGroups: [
    {
      title: "Updating",
      examples: [
        ["fnpm update", "Update everything within its range."],
        ["fnpm update --latest react", "Move react to latest, rewriting its range."],
        ["fnpm update -i", "Pick what to update from an interactive list."],
        ["fnpm update -r", "Update across every workspace."],
      ],
    },
  ],
};

const OUTDATED: CommandLeaf = {
  kind: "leaf",
  name: "outdated",
  description: "Show dependencies with newer versions available",
  positionals: [pos("patterns", "Package patterns to filter by", { variadic: true })],
  flags: F.OUTDATED,
  ignoresRuntimeFlags: true,
};

const WHY: CommandLeaf = {
  kind: "leaf",
  name: "why",
  description: "Explain why a package is installed",
  positionals: [
    pos("package", "Package name or glob to explain"),
  ],
  flags: F.WHY,
  ignoresRuntimeFlags: true,
};

const LS: CommandLeaf = {
  kind: "leaf",
  name: "ls",
  aliases: ["list"],
  description: "List the installed dependency tree",
  positionals: [pos("packages", "Only show these packages", { variadic: true })],
  flags: F.LS,
  ignoresRuntimeFlags: true,
};

const INFO: CommandLeaf = {
  kind: "leaf",
  name: "info",
  aliases: ["view", "show"],
  description: "Show registry metadata for a package",
  positionals: [
    pos("package", "Package name or path to package.json"),
    pos("property", "Property path to print, e.g. versions or dist.tarball", {
      variadic: true,
    }),
  ],
  flags: F.INFO,
  ignoresRuntimeFlags: true,
};

const LINK: CommandLeaf = {
  kind: "leaf",
  name: "link",
  description: "Register this package for linking, or link a registered package",
  positionals: [pos("packages", '"name" install package as a link', { variadic: true })],
  flags: F.LINK,
  ignoresRuntimeFlags: true,
};

const UNLINK: CommandLeaf = {
  kind: "leaf",
  name: "unlink",
  description: "Unregister this package, or remove a linked package",
  positionals: [pos("packages", '"name" uninstall package as a link', { variadic: true })],
  flags: F.UNLINK,
  ignoresRuntimeFlags: true,
};

const PATCH: CommandLeaf = {
  kind: "leaf",
  name: "patch",
  description: "Prepare a package for patching",
  positionals: [
    // Checked by the core, which prints bun's usage example.
    pos("package", '"name" (or name@version, or path) of the package to patch'),
  ],
  flags: F.PATCH,
  ignoresRuntimeFlags: true,
};

const PATCH_COMMIT: CommandLeaf = {
  kind: "leaf",
  name: "patch-commit",
  description: "Save the changes made to a patched package",
  positionals: [pos("dir", '"dir" containing changes to a package')],
  flags: F.PATCH_COMMIT,
  ignoresRuntimeFlags: true,
};

const PRUNE: CommandLeaf = {
  kind: "leaf",
  name: "prune",
  description: "Remove packages from node_modules that nothing needs",
  flags: F.PRUNE,
  ignoresRuntimeFlags: true,
};

const DEDUPE: CommandLeaf = {
  kind: "leaf",
  name: "dedupe",
  aliases: ["ddp"],
  description: "Collapse duplicate versions in the lockfile",
  flags: F.DEDUPE,
  ignoresRuntimeFlags: true,
};

const AUDIT: CommandLeaf = {
  kind: "leaf",
  name: "audit",
  description: "Check installed packages for known vulnerabilities",
  usageText: "fnpm audit [fix] [options]",
  positionals: [pos("action", "`fix` applies the available fixes")],
  flags: F.AUDIT,
  ignoresRuntimeFlags: true,
};

const PACK: CommandLeaf = {
  kind: "leaf",
  name: "pack",
  description: "Create a tarball of the current package",
  flags: F.PACK,
  ignoresRuntimeFlags: true,
};

const PUBLISH: CommandLeaf = {
  kind: "leaf",
  name: "publish",
  description: "Publish a package to the registry",
  positionals: [pos("tarball", "Package tarball to publish")],
  flags: F.PUBLISH,
  ignoresRuntimeFlags: true,
};

const RUN: CommandLeaf = {
  kind: "leaf",
  name: "run",
  description: "Run a package.json script or a node_modules/.bin binary",
  usageText: "fnpm run [flags] <script> [...args]",
  positionals: [
    pos("script", "Script name, binary, or file to run"),
    pos("args", "Arguments passed to the script", { variadic: true }),
  ],
  flags: F.RUN,
  passthrough: 1,
  notes: ["Flags for fnpm go before the script name; everything after it goes to the script."],
};

const EXEC: CommandLeaf = {
  kind: "leaf",
  name: "exec",
  description: "Run a shell command with node_modules/.bin on PATH",
  usageText: "fnpm exec [flags] <command> [...args]",
  positionals: [
    pos("command", "Shell command string", { required: true }),
    pos("args", "Extra arguments appended to the command", { variadic: true }),
  ],
  flags: F.EXEC,
  passthrough: 1,
};

const X: CommandLeaf = {
  kind: "leaf",
  name: "x",
  aliases: ["dlx"],
  description: "Run a package binary, installing it into the cache if needed",
  usageText: "fnpm x [flags] <package>[@version] [...args]",
  positionals: [
    pos("package", "Package (and binary) to run", { required: true }),
    pos("args", "Arguments passed to the binary", { variadic: true }),
  ],
  flags: F.X,
  passthrough: 1,
  rawArgs: true,
  notes: ["Flags for the package go after its name."],
};

const CREATE: CommandLeaf = {
  kind: "leaf",
  name: "create",
  description: "Scaffold a project from a create-<template> package",
  usageText: "fnpm create <template> [...args]",
  positionals: [
    pos("template", "Template, resolved to the create-<template> package", {
      required: true,
    }),
    pos("args", "Arguments passed to the template", { variadic: true }),
  ],
  flags: F.X,
  passthrough: 1,
  rawArgs: true,
};

const INIT: CommandLeaf = {
  kind: "leaf",
  name: "init",
  description: "Create a package.json in the current directory",
  positionals: [pos("folder", "Directory to initialize (default: cwd)")],
  flags: F.INIT,
};

const PM_CACHE: CommandGroup = {
  kind: "group",
  name: "cache",
  description: "Inspect and clean the package cache",
  defaultChild: "path",
  children: [
    pm("path", "Print the cache directory"),
    pm("rm", "Delete the whole cache"),
    pm("prune", "Remove cache entries older than --max-age days", {
      flags: F.PM_CACHE_PRUNE,
    }),
    pm("pack", "Write cache entries to an archive", {
      positionals: [pos("file", "Archive to write", { required: true })],
    }),
    pm("unpack", "Restore cache entries from an archive", {
      positionals: [pos("file", "Archive to read", { required: true })],
    }),
  ],
};

// One leaf like bun: the core reads the subcommand, prints help for `help` (or none) and
// rejects unknown ones, and `--json` means "parse the values as JSON".
const PM_PKG: CommandLeaf = pm("pkg", "Read and edit package.json (get, set, delete, fix)", {
  usageText: "fnpm pm pkg <get|set|delete|fix|help> [...args]",
  positionals: [
    pos("subcommand", "get | set | delete | fix | help"),
    pos("args", "Property paths, or key=value pairs for set (values parse as JSON with --json)", {
      variadic: true,
    }),
  ],
});

const PM: CommandGroup = {
  kind: "group",
  name: "pm",
  description: "Package manager utilities",
  children: [
    pm("bin", "Print the bin directory (-g for the global one)"),
    pm("ls", "List the installed dependency tree", {
      aliases: ["list"],
      flags: F.LS,
      positionals: [pos("packages", "Only show these packages", { variadic: true })],
    }),
    pm("why", "Explain why a package is installed", {
      flags: F.PM_WHY,
      positionals: [pos("package", "Package name or glob")],
    }),
    pm("hash", "Print the hash of the current lockfile"),
    pm("hash-string", "Print the string the lockfile hash is computed from"),
    pm("hash-print", "Print the hash stored in the current lockfile"),
    PM_CACHE,
    pm("migrate", "Write fnpm.lock from another package manager's lockfile"),
    pm("untrusted", "List dependencies whose lifecycle scripts were blocked"),
    pm("trust", "Run and allow the scripts of untrusted dependencies", {
      flags: F.PM_TRUST,
      positionals: [pos("packages", "Dependencies to trust", { variadic: true })],
    }),
    pm("default-trusted", "Print the built-in list of trusted dependencies"),
    pm("version", "Bump the package version (patch, minor, major, ...)", {
      flags: F.PM_VERSION,
      positionals: [
        pos(
          "increment",
          "patch | minor | major | prepatch | preminor | premajor | prerelease | from-git | <version>",
        ),
      ],
    }),
    PM_PKG,
    pm("whoami", "Print the user name the registry knows you as"),
    pm("licenses", "List the licenses of installed packages", {
      flags: F.PM_LICENSES,
      positionals: [pos("action", "`ls` or `list` (the default)")],
    }),
    pm("view", "Show registry metadata for a package", {
      positionals: [
        pos("package", "Package name or path to package.json"),
        pos("property", "Property path to print", { variadic: true }),
      ],
    }),
    pm("pack", "Create a tarball of the current package", { flags: F.PM_PACK }),
    pm("diff", "Compare two versions of a package, file by file", {
      flags: F.PM_DIFF,
      positionals: [pos("specs", "Package specs or paths to compare", { variadic: true })],
    }),
    pm("scan", "Run the configured security scanner over the lockfile"),
    pm("fetch", "Download every lockfile package into the cache without installing"),
  ],
};

const STORE: CommandGroup = {
  kind: "group",
  name: "store",
  description: "Manage the content-addressable package store",
  children: [
    { kind: "leaf", name: "path", description: "Print the store directory", flags: F.STORE_PATH },
    {
      kind: "leaf",
      name: "status",
      description: "Check store files against their recorded integrity",
      flags: F.STORE_STATUS,
    },
    {
      kind: "leaf",
      name: "prune",
      description: "Remove store entries no project references",
      flags: F.STORE_PRUNE,
    },
  ],
};

const DOCTOR: CommandLeaf = {
  kind: "leaf",
  name: "doctor",
  description: "Diagnose the environment, config, registry access and store",
  flags: F.DOCTOR,
};

const COMPLETIONS: CommandLeaf = {
  kind: "leaf",
  name: "completions",
  description: "Print a shell completion script (bash, zsh or fish)",
  positionals: [pos("shell", "bash | zsh | fish (default: from $SHELL)")],
  flags: [],
  exampleGroups: [
    {
      title: "Install completions",
      examples: [
        ['eval "$(fnpm completions bash)"', "Add to ~/.bashrc."],
        ['eval "$(fnpm completions zsh)"', "Add to ~/.zshrc."],
        ["fnpm completions fish > ~/.config/fish/completions/fnpm.fish", "Fish."],
      ],
    },
  ],
};

export const FNPM_ROOT: CommandGroup = {
  kind: "group",
  name: "fnpm",
  description: "fnpm package manager",
  usageText: "fnpm <command> [...args]",
  children: [
    HELP,
    INSTALL,
    CI,
    ADD,
    REMOVE,
    UPDATE,
    OUTDATED,
    WHY,
    LS,
    INFO,
    LINK,
    UNLINK,
    PATCH,
    PATCH_COMMIT,
    PRUNE,
    DEDUPE,
    AUDIT,
    PACK,
    PUBLISH,
    RUN,
    EXEC,
    X,
    CREATE,
    INIT,
    PM,
    STORE,
    DOCTOR,
    COMPLETIONS,
  ],
};

/** Top-level shorthands; each one is listed once, under its target. */
export const FNPM_ALIASES: readonly CommandAlias[] = Object.freeze([
  {
    kind: "alias",
    name: "whoami",
    description: "Print the user name the registry knows you as (fnpm pm whoami)",
    target: ["pm", "whoami"],
  },
  {
    kind: "alias",
    name: "migrate",
    description: "Write fnpm.lock from another lockfile (fnpm pm migrate)",
    target: ["pm", "migrate"],
  },
]);

export const FNPM_REGISTRY: Registry = {
  bin: "fnpm",
  tagline: "fnpm: a fast, disk-saving package manager.",
  root: FNPM_ROOT,
  aliases: FNPM_ALIASES,
  globalFlags: F.GLOBAL_FLAGS,
  detailedHelp: [
    ["fnpm install -h", "Every install flag, including npm/yarn/pnpm translations"],
    ["fnpm add -h", "Choose the dependency group, exact pins, catalogs"],
    ["fnpm update -h", "Ranges, --latest, interactive and recursive updates"],
    ["fnpm run -h", "Scripts, workspaces filters and parallel runs"],
    ["fnpm pm -h", "Cache, lockfile hashes, trust, version bumps, package.json edits"],
    ["fnpm-lock -h", "The lockfile toolkit: migrate, export, verify, diff"],
  ],
};
