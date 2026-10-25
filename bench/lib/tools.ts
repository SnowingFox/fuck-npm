// Package manager definitions: how to invoke each tool, where its caches live, and how to
// force it onto the benchmark registry. Each tool gets a private HOME/XDG tree so user-level
// config (~/.npmrc, ~/.bunfig.toml, ~/.yarnrc, pnpm global config) can't leak into results.
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { REPO_ROOT, run, which } from "./util.ts";

export const TOOL_NAMES = ["fnpm", "bun", "pnpm", "npm", "yarn"] as const;
export type ToolName = (typeof TOOL_NAMES)[number];

export interface Ctx {
  /** Project directory the tool runs in. */
  work: string;
  /** Root of this tool+fixture's private state (caches, store, HOME). */
  state: string;
  registry: string;
  /** Fixture declares npm-style workspaces. */
  workspaces: boolean;
  /** Package spec used by the `add` scenario. */
  addSpec: string;
}

export interface Tool {
  name: ToolName;
  /** argv prefix to invoke the tool, or null if it is not available. */
  argv0(): string[] | null;
  version(): string;
  /** Directories that hold the tool's cache/store. Cleared for cold runs, counted for disk. */
  cacheDirs(ctx: Ctx): string[];
  env(ctx: Ctx): Record<string, string>;
  install(ctx: Ctx): string[];
  frozen(ctx: Ctx): string[];
  add(ctx: Ctx): string[];
  /** Extra files the tool needs in the project copy (e.g. pnpm-workspace.yaml). */
  extraFiles?(ctx: Ctx, fixtureDir: string): Record<string, string>;
}

/** HOME/XDG isolation shared by every tool. */
function baseEnv(ctx: Ctx): Record<string, string> {
  const home = join(ctx.state, "home");
  const realHome = homedir();
  return {
    HOME: home,
    XDG_CONFIG_HOME: join(home, ".config"),
    XDG_DATA_HOME: join(home, ".local/share"),
    XDG_STATE_HOME: join(home, ".local/state"),
    XDG_CACHE_HOME: join(ctx.state, "xdg-cache"),
    // corepack-managed binaries (pnpm, yarn) must not be re-downloaded under the fake HOME.
    COREPACK_HOME: process.env.COREPACK_HOME || join(realHome, ".cache/node/corepack"),
    COREPACK_ENABLE_DOWNLOAD_PROMPT: "0",
    COREPACK_ENABLE_AUTO_PIN: "0",
    COREPACK_ENABLE_STRICT: "0",
    npm_config_registry: ctx.registry,
    npm_config_update_notifier: "false",
    NO_UPDATE_NOTIFIER: "1",
    FORCE_COLOR: "0",
    NO_COLOR: "1",
  };
}

function firstLine(argv: string[] | null, args: string[]): string {
  if (!argv) return "n/a";
  const r = run([...argv, ...args], { env: { ...process.env, COREPACK_ENABLE_DOWNLOAD_PROMPT: "0" } });
  return r.status === 0 ? r.stdout.trim().split("\n")[0] : "n/a";
}

function binArgv(bin: string): () => string[] | null {
  let cached: string[] | null | undefined;
  return () => {
    if (cached === undefined) {
      const p = which(bin);
      cached = p ? [p] : null;
    }
    return cached;
  };
}

const npmArgv = binArgv("npm");
const npmFlags = (ctx: Ctx) => ["--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error", `--registry=${ctx.registry}`];
const npm: Tool = {
  name: "npm",
  argv0: npmArgv,
  version: () => firstLine(npmArgv(), ["--version"]),
  cacheDirs: (ctx) => [join(ctx.state, "cache")],
  env: (ctx) => ({ ...baseEnv(ctx), npm_config_cache: join(ctx.state, "cache") }),
  install: (ctx) => [...npmArgv()!, "install", ...npmFlags(ctx)],
  frozen: (ctx) => [...npmArgv()!, "ci", ...npmFlags(ctx)],
  add: (ctx) => [...npmArgv()!, "install", ctx.addSpec, ...npmFlags(ctx)],
};

const pnpmArgv = binArgv("pnpm");
const pnpmFlags = (ctx: Ctx) => [
  "--ignore-scripts",
  `--store-dir=${join(ctx.state, "store")}`,
  `--config.cache-dir=${join(ctx.state, "cache")}`,
  `--registry=${ctx.registry}`,
];
const pnpm: Tool = {
  name: "pnpm",
  argv0: pnpmArgv,
  version: () => firstLine(pnpmArgv(), ["--version"]),
  cacheDirs: (ctx) => [join(ctx.state, "store"), join(ctx.state, "cache"), join(ctx.state, "xdg-cache")],
  env: (ctx) => ({
    ...baseEnv(ctx),
    npm_config_store_dir: join(ctx.state, "store"),
    pnpm_config_store_dir: join(ctx.state, "store"),
    npm_config_cache_dir: join(ctx.state, "cache"),
    pnpm_config_cache_dir: join(ctx.state, "cache"),
    pnpm_config_registry: ctx.registry,
  }),
  install: (ctx) => [...pnpmArgv()!, "install", ...pnpmFlags(ctx)],
  frozen: (ctx) => [...pnpmArgv()!, "install", "--frozen-lockfile", ...pnpmFlags(ctx)],
  add: (ctx) => [...pnpmArgv()!, "add", ctx.addSpec, ...(ctx.workspaces ? ["-w"] : []), ...pnpmFlags(ctx)],
  extraFiles: (ctx, fixtureDir): Record<string, string> => {
    if (!ctx.workspaces) return {};
    const pkg = JSON.parse(readFileSync(join(fixtureDir, "package.json"), "utf8"));
    const globs: string[] = Array.isArray(pkg.workspaces) ? pkg.workspaces : pkg.workspaces?.packages ?? [];
    const yaml = ["packages:", ...globs.map((g) => `  - ${JSON.stringify(g)}`), "linkWorkspacePackages: true", ""];
    return { "pnpm-workspace.yaml": yaml.join("\n") };
  },
};

const yarnArgv = binArgv("yarn");
const yarnFlags = (ctx: Ctx) => [
  "--ignore-scripts",
  "--non-interactive",
  "--no-progress",
  "--silent",
  `--cache-folder=${join(ctx.state, "cache")}`,
  `--registry=${ctx.registry}`,
];
const yarn: Tool = {
  name: "yarn",
  argv0: yarnArgv,
  version: () => firstLine(yarnArgv(), ["--version"]),
  cacheDirs: (ctx) => [join(ctx.state, "cache")],
  env: (ctx) => ({ ...baseEnv(ctx), YARN_CACHE_FOLDER: join(ctx.state, "cache"), YARN_REGISTRY: ctx.registry }),
  install: (ctx) => [...yarnArgv()!, "install", ...yarnFlags(ctx)],
  frozen: (ctx) => [...yarnArgv()!, "install", "--frozen-lockfile", ...yarnFlags(ctx)],
  add: (ctx) => [...yarnArgv()!, "add", ctx.addSpec, ...(ctx.workspaces ? ["-W"] : []), ...yarnFlags(ctx)],
};

const bunArgv = binArgv("bun");
const bunFlags = (ctx: Ctx) => ["--ignore-scripts", "--no-progress", `--registry=${ctx.registry}`];
const bun: Tool = {
  name: "bun",
  argv0: bunArgv,
  version: () => firstLine(bunArgv(), ["--version"]),
  cacheDirs: (ctx) => [join(ctx.state, "cache")],
  env: (ctx) => ({
    ...baseEnv(ctx),
    BUN_INSTALL_CACHE_DIR: join(ctx.state, "cache"),
    BUN_CONFIG_REGISTRY: ctx.registry,
    DO_NOT_TRACK: "1",
  }),
  install: (ctx) => [...bunArgv()!, "install", ...bunFlags(ctx)],
  frozen: (ctx) => [...bunArgv()!, "install", "--frozen-lockfile", ...bunFlags(ctx)],
  add: (ctx) => [...bunArgv()!, "add", ctx.addSpec, ...bunFlags(ctx)],
};

const FNPM_ENTRY = join(REPO_ROOT, "packages/fuck-npm/dist/fnpm.js");
/**
 * The native launcher, which is what users run after the postinstall swap: `FNPM_BIN`, else
 * a release build of `crates/fnpm-cli` (`cargo build --release -p fnpm-cli`).
 */
export const fnpmNative = (): string | null => {
  const target = process.env.CARGO_TARGET_DIR || join(REPO_ROOT, "target");
  const exe = process.env.FNPM_BIN || join(target, "release", process.platform === "win32" ? "fnpm.exe" : "fnpm");
  return existsSync(exe) ? exe : null;
};
const fnpmArgv = (): string[] | null => {
  const native = fnpmNative();
  if (native) return [native];
  const node = which("node");
  return node && existsSync(FNPM_ENTRY) ? [node, FNPM_ENTRY] : null;
};
const fnpmFlags = (ctx: Ctx) => ["--ignore-scripts", `--registry=${ctx.registry}`];
const fnpm: Tool = {
  name: "fnpm",
  argv0: fnpmArgv,
  version: () => firstLine(fnpmArgv(), ["--version"]),
  cacheDirs: (ctx) => [join(ctx.state, "store"), join(ctx.state, "cache")],
  env: (ctx) => ({
    ...baseEnv(ctx),
    FNPM_STORE_DIR: join(ctx.state, "store"),
    FNPM_CACHE_DIR: join(ctx.state, "cache"),
    FNPM_REGISTRY: ctx.registry,
  }),
  install: (ctx) => [...fnpmArgv()!, "install", ...fnpmFlags(ctx)],
  frozen: (ctx) => [...fnpmArgv()!, "install", "--frozen-lockfile", ...fnpmFlags(ctx)],
  add: (ctx) => [...fnpmArgv()!, "add", ctx.addSpec, ...fnpmFlags(ctx)],
};

export const TOOLS: Record<ToolName, Tool> = { fnpm, bun, pnpm, npm, yarn };
