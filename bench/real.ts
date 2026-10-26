#!/usr/bin/env node
// Targeted real-project installs. Sources are immutable git archives, never corpus worktrees.
// node bench/real.ts --root /absolute/scratch --tools bun,pnpm,yarn --prepare-only
// node bench/real.ts --root /absolute/scratch --sources /absolute/checkouts --tools fnpm --fnpm /absolute/release/fnpm --source-revision <commit>
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { cpus, homedir, platform, totalmem } from "node:os";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { fileURLToPath } from "node:url";
import { measure } from "./lib/disk.ts";
import { declaredDependencies } from "./lib/declarations.ts";
import { importers, loadCorpus, snapshot, workRoot } from "../tests/e2e/lib.ts";

const { values: args } = parseArgs({ options: {
  sources: { type: "string" }, "source-revision": { type: "string" },
  root: { type: "string" }, tools: { type: "string", default: "fnpm,bun" },
  only: { type: "string" }, fnpm: { type: "string" }, runs: { type: "string", default: "3" },
  timeout: { type: "string", default: "0" }, "prepare-only": { type: "boolean", default: false },
  "prepare-timeout": { type: "string", default: "0" },
} });
if (!args.root) throw new Error("--root is required; use a new empty scratch directory");
const root = resolve(args.root);
const registry = process.env.BENCH_REGISTRY ?? "http://127.0.0.1:4873/";
const runs = Number(args.runs), timeout = Number(args.timeout), prepTimeout = Number(args["prepare-timeout"]);
if (!Number.isInteger(runs) || runs < 3 || !Number.isFinite(timeout) || timeout < 0 || !Number.isFinite(prepTimeout) || prepTimeout < 0) throw new Error("runs must be >=3 and timeouts nonnegative (0 disables the cap)");
const tools = args.tools!.split(",");
if (tools.some(t => !["fnpm", "bun", "pnpm", "yarn"].includes(t))) throw new Error("supported tools: fnpm,bun,pnpm,yarn");
if (tools.includes("fnpm") && (!args.fnpm || !existsSync(args.fnpm))) throw new Error("--fnpm must name the release executable");
const corpus = loadCorpus();
const sourceRoot = args.sources ? resolve(args.sources) : workRoot();
const selected = ["trpc", "excalidraw", "jest", "opencode"].map(name => {
  const e = corpus.find(e => e.name === name)!;
  const checks: Record<string, string> = {
    trpc: 'cd packages/server && PATH="$PWD/node_modules/.bin:$PATH" tsdown',
    excalidraw: "tsc",
    jest: "node ./scripts/build.mjs",
    opencode: "cd packages/util && bun check --no-script",
  };
  return { ...e, source: join(sourceRoot, name), verifyCommand: checks[name] };
});
selected.unshift({ name: "opennextjs-cloudflare", repo: "opennextjs/opennextjs-cloudflare", commit: "8ea9eb973b6d71743e4b37ab2fdf5d98fe4d0ddb", pm: "pnpm", lockfile: "pnpm-lock.yaml", lockfileFormat: "pnpm-v9", workspace: true, features: ["catalogs", "workspaces", "next", "cloudflare"], install: "pnpm install --frozen-lockfile", verify: "", baseline: "", source: join(sourceRoot, "opennextjs-cloudflare"), verifyCommand: 'cd packages/cloudflare && PATH="$PWD/node_modules/.bin:$PATH" tsc && PATH="$PWD/node_modules/.bin:$PATH" vitest --run' });
const entries = selected.filter(e => !args.only || args.only.split(",").includes(e.name));
if (!entries.length) throw new Error("no projects selected");
const binaryHash = tools.includes("fnpm") ? createHash("sha256").update(readFileSync(args.fnpm!)).digest("hex") : undefined;
if (binaryHash && !args["source-revision"]) throw new Error("--source-revision is required with --fnpm");
const out = join(root, "results.json");
mkdirSync(root, { recursive: true });
const results: any = existsSync(out) ? JSON.parse(readFileSync(out, "utf8")) : {
  date: new Date().toISOString(), harnessSha256: createHash("sha256").update(readFileSync(fileURLToPath(import.meta.url))).digest("hex"), harnessBaseRevision: spawnSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).stdout.trim(),
  machine: { os: platform(), arch: process.arch, cores: cpus().length, memoryBytes: totalmem(), node: process.version },
  methodology: { registry: "local warmed mirror", scripts: "disabled during timed installs; project build/check follows", runs, cold: "native tool lock, empty private PM cache/store and node_modules; OS and registry caches retained", warm: "native tool lock and populated PM cache/store, node_modules removed", noop: "native tool lock, caches and installed tree retained", migration: "untimed preparation from pristine committed original lock; logs disclose fallback resolution", timeoutSeconds: timeout, sharedMachine: true, disk: "st_blocks*512; dedupe device/inode across all node_modules and cache/store; no symlink traversal" }, projects: {},
};
const scrub = (s: string) => s.split(realpathSync(root)).join("<scratch>").split(root).join("<scratch>").split(realpathSync(homedir())).join("~").split(homedir()).join("~");
const save = () => writeFileSync(out, JSON.stringify(results, null, 2) + "\n");
const log = (s: string) => process.stderr.write(`[real-bench] ${s}\n`);
function run(command: string[], work: string, env: NodeJS.ProcessEnv, cap: number, logPath: string) {
  const start = performance.now();
  const r = spawnSync(cap > 0 ? "timeout" : command[0], cap > 0 ? ["-k", "10", String(cap), ...command] : command.slice(1), { cwd: work, env, encoding: "utf8", maxBuffer: 32 << 20, timeout: cap > 0 ? (cap + 20) * 1000 : undefined });
  const output = `${r.stdout ?? ""}${r.stderr ?? ""}`;
  writeFileSync(logPath, output);
  return { seconds: (performance.now() - start) / 1000, exitCode: r.status, timedOut: cap > 0 && ([124, 137].includes(r.status ?? 0) || (r.error as any)?.code === "ETIMEDOUT"), error: r.status === 0 ? undefined : scrub(output.slice(-4000)), log: scrub(logPath), command: command.map(scrub) };
}
const lockNames = ["fnpm.lock", "bun.lock", "bun.lockb", "pnpm-lock.yaml", "package-lock.json", "yarn.lock"];
for (const entry of entries) {
  const src = join(root, "archives", entry.name);
  if (!existsSync(join(src, "package.json"))) {
    mkdirSync(src, { recursive: true });
    const archive = spawnSync("git", ["-C", entry.source, "archive", entry.commit], { maxBuffer: 512 << 20 });
    if (archive.status !== 0) throw new Error(`git archive ${entry.name}: ${archive.stderr}`);
    const unpack = spawnSync("tar", ["-x", "-C", src], { input: archive.stdout });
    if (unpack.status !== 0) throw new Error(`tar ${entry.name}: ${unpack.stderr}`);
  }
  const own = entry.pm === "pnpm" ? "pnpm" : entry.pm.startsWith("yarn") ? "yarn" : "bun";
  const project = results.projects[entry.name] ??= { repo: entry.repo, commit: entry.commit, originalLock: entry.lockfile, originalLockSha256: createHash("sha256").update(readFileSync(join(src, entry.lockfile))).digest("hex"), workspaceCount: importers(src).length - 1, tools: {} };
  for (const tool of tools) {
    if (!["fnpm", "bun", own].includes(tool)) continue;
    const base = join(root, "runs", entry.name, tool), work = join(base, "work"), state = join(base, "state"), locked = join(base, "locks"), logs = join(base, "logs");
    mkdirSync(logs, { recursive: true });
    const caches = [join(state, "cache"), join(state, "store"), join(state, "xdg-cache"), join(state, "yarn-global")];
    const env: NodeJS.ProcessEnv = { ...process.env, ...entry.env, HOME: join(state, "home"), XDG_CONFIG_HOME: join(state, "home/.config"), XDG_DATA_HOME: join(state, "home/.local/share"), XDG_STATE_HOME: join(state, "home/.local/state"), XDG_CACHE_HOME: caches[2], COREPACK_HOME: process.env.COREPACK_HOME || join(homedir(), ".cache/node/corepack"), COREPACK_ENABLE_DOWNLOAD_PROMPT: "0", COREPACK_ENABLE_AUTO_PIN: "0", COREPACK_ENABLE_STRICT: "0", npm_config_registry: registry, npm_config_update_notifier: "false", npm_config_cache: caches[0], FNPM_REGISTRY: registry, FNPM_STORE_DIR: caches[1], FNPM_CACHE_DIR: caches[0], BUN_INSTALL_CACHE_DIR: caches[0], BUN_CONFIG_REGISTRY: registry, YARN_CACHE_FOLDER: caches[0], YARN_GLOBAL_FOLDER: caches[3], YARN_ENABLE_SCRIPTS: "false", YARN_NPM_REGISTRY_SERVER: registry.replace(/\/$/, ""), YARN_UNSAFE_HTTP_WHITELIST: new URL(registry).hostname, NO_UPDATE_NOTIFIER: "1", DO_NOT_TRACK: "1", TURBO_TELEMETRY_DISABLED: "1", npm_config_verify_deps_before_run: "false", NEXT_TELEMETRY_DISABLED: "1", FORCE_COLOR: "0", NO_COLOR: "1" };
    for (const key of ["CI", "CONTINUOUS_INTEGRATION", "GITHUB_ACTIONS", "BUILD_NUMBER", "RUN_ID", "YARN_IGNORE_PATH"]) delete env[key];
    // Yarn 4 rejects Yarn Classic's environment flag, and vice versa.
    if (entry.pm === "yarn-berry" && tool === "yarn") delete env.YARN_CACHE_FOLDER;
    const reset = (native: boolean, cold: boolean) => {
      rmSync(work, { recursive: true, force: true });
      cpSync(src, work, { recursive: true, verbatimSymlinks: true });
      if (native && existsSync(locked)) for (const name of readdirSync(locked)) cpSync(join(locked, name), join(work, name), { recursive: true });
      if (cold) rmSync(state, { recursive: true, force: true });
      mkdirSync(join(state, "home"), { recursive: true });
    };
    const yarnBerry = entry.pm === "yarn-berry" && tool === "yarn";
    const yarnPath = yarnBerry ? join(work, ".yarn/releases/yarn-4.18.0.cjs") : "yarn";
    const prefix = tool === "fnpm" ? [args.fnpm!] : yarnBerry ? [process.execPath, yarnPath] : [tool];
    const command = [...prefix, "install", ...(tool === "fnpm" || tool === "bun" ? ["--ignore-scripts", `--registry=${registry}`] : tool === "pnpm" ? ["--frozen-lockfile", "--ignore-scripts", `--store-dir=${caches[1]}`, `--config.cache-dir=${caches[0]}`, `--registry=${registry}`] : yarnBerry ? ["--immutable", "--mode=skip-build"] : ["--frozen-lockfile", "--ignore-scripts", "--non-interactive", `--registry=${registry}`])];
    if (tool === "bun") command.push("--no-progress");
    if (tool === "fnpm" && project.tools[tool]?.binarySha256 !== binaryHash) {
      project.tools[tool] = {};
      rmSync(locked, { recursive: true, force: true });
    }
    const result = project.tools[tool] ??= {};
    if (tool === "fnpm") result.sourceRevision = args["source-revision"];
    if (!result.migration?.ok) {
      reset(false, true);
      const version = spawnSync(...([prefix[0], [...prefix.slice(1), "--version"], { cwd: work, env, encoding: "utf8", timeout: 120000 }] as const));
      result.version = version.stdout?.trim();
      if (tool === "fnpm") result.binarySha256 = binaryHash;
      log(`${entry.name}/${tool}: preparing original-lock install (${result.version})`);
      const migration = run(command, work, env, prepTimeout, join(logs, "migration.log"));
      const text = readFileSync(join(logs, "migration.log"), "utf8");
      result.migration = { ...migration, ok: migration.exitCode === 0, warnings: scrub(text.split("\n").filter(l => /migrat|warn|lockfile|error/i.test(l)).join("\n")).slice(-6000) };
      if (!result.migration.ok) { log(`${entry.name}/${tool}: preparation FAILED exit=${migration.exitCode}`); save(); continue; }
      mkdirSync(locked, { recursive: true });
      for (const name of [...lockNames, "package.json", "pnpm-workspace.yaml"]) if (existsSync(join(work, name))) cpSync(join(work, name), join(locked, name), { recursive: true });
      result.lockHashes = Object.fromEntries(readdirSync(locked).map(name => [name, createHash("sha256").update(readFileSync(join(locked, name))).digest("hex")]));
      result.preparedSnapshot = snapshot(work);
      save();
    }
    if (args["prepare-only"]) continue;
    result.scenarios ??= {};
    for (const scenario of ["cold-lockfile", "warm", "noop"]) {
      if (result.scenarios[scenario]?.ok) continue;
      const samples: any[] = [];
      let seedOk = true;
      if (scenario !== "cold-lockfile") {
        reset(true, false);
        const seed = run(command, work, env, prepTimeout, join(logs, `${scenario}-seed.log`));
        seedOk = seed.exitCode === 0;
        if (!seedOk) samples.push({ ...seed, preparationFailed: true });
      }
      for (let i = 0; seedOk && i < runs; i++) {
        if (scenario !== "noop") reset(true, scenario === "cold-lockfile");
        const sample = run(command, work, env, timeout, join(logs, `${scenario}-${i + 1}.log`));
        samples.push(sample);
        log(`${entry.name}/${tool}/${scenario} ${i + 1}/${runs}: ${sample.seconds.toFixed(3)}s exit=${sample.exitCode}`);
        // Preserve a failed attempt and stop this scenario; completed samples remain available.
        if (sample.exitCode !== 0) break;
      }
      const times = samples.filter(s => s.exitCode === 0).map(s => s.seconds).sort((a, b) => a - b);
      result.scenarios[scenario] = { samples, ok: samples.length === runs && samples.every(s => s.exitCode === 0), median: times.length ? times[Math.floor(times.length / 2)] : undefined, min: times[0], max: times.at(-1) };
      save();
    }
    if (result.verify?.ok && result.verificationInstall?.exitCode === 0) continue;
    // Restore an installed pristine tree before checking it, including when resuming
    // after a project command modified node_modules or package-manager state.
    reset(true, false);
    result.verificationInstall = run(command, work, env, prepTimeout, join(logs, "verification-install.log"));
    if (result.verificationInstall.exitCode !== 0) {
      result.verify = { ok: false, error: "pre-verification install failed; no project check executed" };
      log(`${entry.name}/${tool}: verification install FAILED`);
      save(); continue;
    }
    const final = snapshot(work);
    result.snapshot = final;
    result.disk = measure(work, [state, ...(yarnBerry && existsSync(join(work, ".yarn/cache")) ? [join(work, ".yarn/cache")] : [])]);
    const declarations = declaredDependencies(work);
    result.correctness = { packageCount: final.packages.length, instances: final.instances, missingDeclaredDependencies: declarations.missing, platformExcludedDependencies: declarations.platformExcluded, stablePackageSet: JSON.stringify(final.packages) === JSON.stringify(result.preparedSnapshot.packages) };
    const verifyEnv = { ...env, PATH: join(work, "node_modules/.bin") + ":" + env.PATH };
    log(`${entry.name}/${tool}: verify ${entry.verifyCommand}`);
    result.verify = run(["bash", "-c", entry.verifyCommand], work, verifyEnv, timeout > 0 ? entry.verifyTimeoutSec ?? 300 : 0, join(logs, "verify.log"));
    result.verify.ok = result.verify.exitCode === 0;
    save();
  }
  if (project.tools.bun?.snapshot) for (const tool of Object.keys(project.tools)) {
    const candidate = project.tools[tool];
    if (!candidate.snapshot) continue;
    const bunPkgs = new Set<string>(project.tools.bun.snapshot.packages), pkgs = new Set<string>(candidate.snapshot.packages);
    candidate.compareBun = { missing: [...bunPkgs].filter(p => !pkgs.has(p)), extra: [...pkgs].filter(p => !bunPkgs.has(p)) };
  }
  save();
}
log(`results: ${out}`);
