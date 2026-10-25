import { flag } from "../registry.ts";
import type { CommandGroup, CommandLeaf, FlagSpec, Registry } from "../registry-types.ts";
import { GLOBAL_FLAGS } from "./flags.ts";

/**
 * The `fnpm-lock` command surface: the lockfile toolkit. Same machinery as
 * `fnpm`, a different root. Core requests are sent as `["lock", ...path]`.
 */

export const LOCKFILE_FORMATS = ["npm", "yarn", "berry", "pnpm", "bun", "fnpm"] as const;

const COMMON: FlagSpec[] = [
  flag("--cwd [dir]", "Project directory (default: cwd)"),
  flag("--lockfile [path]", "Lockfile to read (default: fnpm.lock in --cwd)"),
];

const leaf = (
  name: string,
  description: string,
  extra: Partial<CommandLeaf> = {},
): CommandLeaf => ({ kind: "leaf", name, description, flags: COMMON, ...extra });

const HELP: CommandLeaf = {
  kind: "leaf",
  name: "help",
  description: "Show help for a command",
  usageText: "fnpm-lock help [...command] [--tree]",
  positionals: [
    { name: "command", required: false, variadic: true, description: "Command path to describe" },
  ],
  flags: [],
};

export const FNPM_LOCK_ROOT: CommandGroup = {
  kind: "group",
  name: "fnpm-lock",
  description: "fnpm lockfile toolkit",
  usageText: "fnpm-lock <command> [...args]",
  children: [
    HELP,
    leaf("migrate", "Convert another tool's lockfile into fnpm.lock", {
      flags: [
        ...COMMON,
        flag("--from [format]", `Source format: ${LOCKFILE_FORMATS.slice(0, 5).join(" | ")} (default: auto-detect)`),
        flag("--dry-run", "Print what would be written without writing it"),
        flag("-f, --force", "Overwrite an existing fnpm.lock"),
      ],
    }),
    leaf("export", "Write fnpm.lock out as another tool's lockfile", {
      flags: [
        ...COMMON,
        flag("--to [format]", `Target format: ${LOCKFILE_FORMATS.slice(0, 5).join(" | ")}`),
        flag("-o, --output [path]", "Where to write it (default: the format's usual file name)"),
        flag("--dry-run", "Print the lockfile to stdout instead of writing it"),
      ],
    }),
    leaf("verify", "Check integrity hashes and that the lockfile matches package.json", {
      flags: [...COMMON, flag("--integrity", "Also re-hash every package in the store")],
    }),
    leaf("diff", "Show what changed between two lockfiles (or a lockfile and a git ref)", {
      positionals: [
        { name: "a", required: true, description: "Lockfile path or git ref (e.g. HEAD~1)" },
        { name: "b", required: false, description: "Second lockfile or ref (default: working tree)" },
      ],
      flags: [...COMMON, flag("--name-only", "Only list the package names that changed")],
    }),
    leaf("why", "Explain why a package is in the lockfile", {
      positionals: [{ name: "package", required: true, description: "Package name or glob" }],
      flags: [...COMMON, flag("--depth [n]", "Maximum depth of the dependency paths to show")],
    }),
    leaf("print", "Print the lockfile in a human-readable form", {
      flags: [...COMMON, flag("--yarn", "Print in yarn.lock v1 syntax")],
    }),
    leaf("dedupe", "Collapse duplicate versions in the lockfile", {
      flags: [
        ...COMMON,
        flag("--check", "Exit with code 1 if duplicates could be removed, without changing anything"),
        flag("--dry-run", "Print the duplicates that would be removed"),
      ],
    }),
    leaf("audit", "Check lockfile packages for known vulnerabilities", {
      flags: [
        ...COMMON,
        flag(
          "--audit-level [level]",
          "Only print advisories at or above this severity (low, moderate, high, critical)",
        ),
        flag("--ignore [id]", "Ignore advisories by GHSA or numeric ID (repeatable)", {
          repeatable: true,
        }),
      ],
    }),
  ],
};

export const FNPM_LOCK_REGISTRY: Registry = {
  bin: "fnpm-lock",
  tagline: "fnpm-lock: migrate, export, verify and inspect lockfiles.",
  root: FNPM_LOCK_ROOT,
  aliases: [],
  globalFlags: GLOBAL_FLAGS,
  detailedHelp: [
    ["fnpm-lock migrate -h", "Adopt fnpm from npm, yarn, pnpm or bun"],
    ["fnpm-lock export -h", "Keep another tool's lockfile in sync for teammates"],
    ["fnpm-lock diff -h", "Review lockfile changes in a PR"],
  ],
};
