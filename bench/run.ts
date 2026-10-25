#!/usr/bin/env node
// Install benchmark: fixture x tool x scenario, timed with hyperfine against a local mirror.
//
//   node bench/run.ts [--fixtures small,medium] [--tools bun,pnpm] [--scenarios warm,noop]
//                     [--runs 5] [--warmup 1] [--no-disk] [--regen-lockfiles]
//
// See bench/README.md for methodology.
import { createHash } from "node:crypto";
import { cpSync, existsSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { arch, cpus, homedir, platform, release, totalmem, type } from "node:os";
import { join, relative } from "node:path";
import { parseArgs } from "node:util";
import { fsUsedBytes, measure, type DiskReport } from "./lib/disk.ts";
import { renderMarkdown, type Results, type ScenarioResult, type DiskResult } from "./lib/report.ts";
import { TOOLS, TOOL_NAMES, type Ctx, type Tool, type ToolName } from "./lib/tools.ts";
import {
  BENCH_DIR,
  FIXTURES_DIR,
  REPO_ROOT,
  RESULTS_DIR,
  TMP_DIR,
  ensureDir,
  log,
  parseList,
  registryUrl,
  rmrf,
  run,
  runOk,
  shjoin,
  which,
} from "./lib/util.ts";
import { startMirror } from "./mirror.ts";

export const SCENARIOS = ["cold", "cold-lockfile", "warm", "noop", "add", "ci"] as const;
export type Scenario = (typeof SCENARIOS)[number];

const DEFAULT_ADD_SPEC = "nanoid@5.1.5";
const RESET = join(BENCH_DIR, "lib/reset.sh");

function fixtureNames(): string[] {
  return readdirSync(FIXTURES_DIR)
    .filter((n) => existsSync(join(FIXTURES_DIR, n, "package.json")))
    .sort();
}

function hashDir(dir: string, h = createHash("sha256")): string {
  for (const name of readdirSync(dir).sort()) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) hashDir(p, h);
    else h.update(relative(FIXTURES_DIR, p)).update(readFileSync(p));
  }
  return h.digest("hex");
}

interface Paths {
  src: string; // pristine project (fixture + tool extras), no lockfile
  locked: string; // project files after one install: manifests + lockfile, no node_modules
  work: string; // where the tool runs
  state: string; // caches/store/HOME for this tool+fixture
  scripts: string;
}

function pathsFor(tool: ToolName, fixture: string): Paths {
  return {
    src: join(TMP_DIR, "src", tool, fixture),
    locked: join(TMP_DIR, "locked", tool, fixture),
    work: join(TMP_DIR, "work", tool, fixture),
    state: join(TMP_DIR, "state", tool, fixture),
    scripts: join(TMP_DIR, "scripts", tool, fixture),
  };
}

function toolEnv(tool: Tool, ctx: Ctx): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env, ...tool.env(ctx) };
  // CI detection changes tool behaviour (pnpm defaults to frozen-lockfile, bun/npm change output).
  for (const k of ["CI", "CONTINUOUS_INTEGRATION", "GITHUB_ACTIONS", "BUILD_NUMBER", "RUN_ID"]) delete env[k];
  return env;
}

/** Copy `from` into a fresh `to`, skipping every node_modules directory. */
function copyProject(from: string, to: string): void {
  rmrf(to);
  cpSync(from, to, {
    recursive: true,
    verbatimSymlinks: true,
    filter: (p) => !p.split("/").includes("node_modules"),
  });
}

function prepareSources(tool: Tool, fixture: string, ctx: Ctx, p: Paths): void {
  const fixtureDir = join(FIXTURES_DIR, fixture);
  copyProject(fixtureDir, p.src);
  for (const [name, content] of Object.entries(tool.extraFiles?.(ctx, fixtureDir) ?? {})) {
    writeFileSync(join(p.src, name), content);
  }
  ensureDir(p.work);
  ensureDir(p.scripts);
  ensureDir(join(p.state, "home"));
}

/** Generate the tool's lockfile once per (fixture content, tool version, registry). */
function ensureLockfile(tool: Tool, fixture: string, ctx: Ctx, p: Paths, regen: boolean): void {
  const stamp = createHash("sha256")
    .update(hashDir(join(FIXTURES_DIR, fixture)))
    .update(tool.version())
    .update(ctx.registry)
    .digest("hex");
  const stampFile = join(TMP_DIR, "locked", tool.name, `${fixture}.stamp`);
  if (!regen && existsSync(stampFile) && readFileSync(stampFile, "utf8") === stamp && existsSync(p.locked)) return;
  log(`${tool.name}/${fixture}: generating lockfile`);
  const env = toolEnv(tool, ctx);
  runOk(["bash", RESET, "fresh", p.work, p.src], { env });
  const t0 = Date.now();
  runOk(tool.install(ctx), { cwd: p.work, env });
  log(`${tool.name}/${fixture}: lockfile generated in ${((Date.now() - t0) / 1000).toFixed(1)}s`);
  copyProject(p.work, p.locked);
  writeFileSync(stampFile, stamp);
}

interface Hooks {
  setup?: string;
  prepare?: string;
  command: string[];
}

function hooksFor(scenario: Scenario, tool: Tool, ctx: Ctx, p: Paths): Hooks {
  const caches = tool.cacheDirs(ctx);
  const install = shjoin(tool.install(ctx));
  const reset = (mode: string, src: string, extra: string[] = []) => shjoin(["bash", RESET, mode, p.work, src, ...extra]);
  const primed = `${reset("fresh", p.locked)} && ${install}`;
  switch (scenario) {
    case "cold":
      return { prepare: reset("fresh", p.src, caches), command: tool.install(ctx) };
    case "cold-lockfile":
      return { prepare: reset("fresh", p.locked, caches), command: tool.install(ctx) };
    case "warm":
      return { setup: primed, prepare: reset("nm", p.locked), command: tool.install(ctx) };
    case "noop":
      return { setup: primed, command: tool.install(ctx) };
    case "add":
      // Delete node_modules and reinstall from the lockfile: not every tool prunes the
      // previously added package on a plain install (bun keeps it), which would make the
      // next add a no-op.
      return { setup: primed, prepare: `${reset("nm", p.locked)} && ${install}`, command: tool.add(ctx) };
    case "ci":
      return { setup: primed, prepare: reset("nm", p.locked), command: tool.frozen(ctx) };
  }
}

/** Hooks run as scripts so hyperfine can run everything with --shell=none. */
function hookScript(p: Paths, scenario: Scenario, kind: string, body: string): string {
  const file = join(p.scripts, `${scenario}.${kind}.sh`);
  writeFileSync(file, `#!/usr/bin/env bash\nset -euo pipefail\ncd ${shjoin([p.work])}\n{ ${body}; } >/dev/null 2>&1\n`);
  return shjoin(["bash", file]);
}

function runScenario(
  scenario: Scenario,
  tool: Tool,
  fixture: string,
  ctx: Ctx,
  p: Paths,
  runs: number,
  warmup: number,
): ScenarioResult {
  const hooks = hooksFor(scenario, tool, ctx, p);
  const exportFile = join(TMP_DIR, "hyperfine", `${fixture}-${tool.name}-${scenario}.json`);
  ensureDir(join(TMP_DIR, "hyperfine"));
  rmrf(exportFile);
  const args = ["-N", "--style", "basic", "--runs", String(runs), "--warmup", String(warmup), "--export-json", exportFile];
  args.push("--command-name", `${tool.name} ${scenario} ${fixture}`);
  if (hooks.setup) args.push("--setup", hookScript(p, scenario, "setup", hooks.setup));
  if (hooks.prepare) args.push("--prepare", hookScript(p, scenario, "prepare", hooks.prepare));
  args.push(shjoin(hooks.command));
  log(`${fixture} / ${tool.name} / ${scenario}: hyperfine --runs ${runs}`);
  const env = toolEnv(tool, ctx);
  const r = run(["hyperfine", ...args], { cwd: p.work, env, stdio: ["ignore", "inherit", "inherit"] });
  const base = { fixture, tool: tool.name, scenario, command: displayCommand(tool, hooks.command) };
  if (r.status !== 0 || !existsSync(exportFile)) {
    // Re-run once outside hyperfine to capture the tool's own error output.
    if (hooks.setup) run(["bash", "-c", hooks.setup], { cwd: p.work, env });
    if (hooks.prepare) run(["bash", "-c", hooks.prepare], { cwd: p.work, env });
    const again = run(hooks.command, { cwd: p.work, env });
    const tail = `${again.stderr}\n${again.stdout}`.trim().split("\n").slice(-15).join("\n");
    log(`${fixture} / ${tool.name} / ${scenario}: FAILED\n${tail}`);
    return { ...base, error: scrub(tail) || `hyperfine exited ${r.status}` };
  }
  const json = JSON.parse(readFileSync(exportFile, "utf8")).results[0];
  return {
    ...base,
    runs: json.times.length,
    mean: json.mean,
    stddev: json.stddev ?? 0,
    median: json.median,
    min: json.min,
    max: json.max,
    times: json.times,
  };
}

/** Strip machine-specific absolute paths so results files can be committed. */
function scrub(text: string): string {
  return text.split(REPO_ROOT + "/").join("").split(REPO_ROOT).join(".").split(homedir()).join("~");
}

/** The timed command as it would be typed: tool name instead of the resolved binary path. */
function displayCommand(tool: Tool, argv: string[]): string {
  const prefix = tool.argv0() ?? [];
  const rest = argv.slice(prefix.length);
  const head = tool.name === "fnpm" ? ["node", "packages/fuck-npm/dist/fnpm.js"] : [tool.name];
  return scrub(shjoin([...head, ...rest]));
}

function runDisk(tool: Tool, fixture: string, ctx: Ctx, p: Paths): DiskResult {
  const env = toolEnv(tool, ctx);
  const caches = tool.cacheDirs(ctx);
  log(`${fixture} / ${tool.name}: disk`);
  // 1. Cold install (lockfile, empty cache): filesystem used-space delta.
  runOk(["bash", RESET, "fresh", p.work, p.locked, ...caches], { env });
  run(["sync"]);
  const before = fsUsedBytes(p.work);
  const t0 = Date.now();
  runOk(tool.install(ctx), { cwd: p.work, env });
  const coldSeconds = (Date.now() - t0) / 1000;
  run(["sync"]);
  const coldDfDeltaBytes = fsUsedBytes(p.work) - before;
  // 2. Warm install (cache present, fresh node_modules): walk node_modules + store.
  runOk(["bash", RESET, "fresh", p.work, p.locked], { env });
  runOk(tool.install(ctx), { cwd: p.work, env });
  const report: DiskReport = measure(p.work, caches.filter(existsSync));
  return { fixture, tool: tool.name, ...report, coldDfDeltaBytes, coldSeconds };
}

function machineInfo(): Results["machine"] {
  const fsType =
    platform() === "linux"
      ? run(["findmnt", "-n", "-o", "FSTYPE", "-T", TMP_DIR]).stdout.trim()
      : run(["sh", "-c", `mount | grep " on $(df -P "$0" | tail -1 | awk '{print $6}') "`, TMP_DIR]).stdout.trim();
  return {
    os: `${type()} ${release()}`,
    platform: platform(),
    arch: arch(),
    cpu: cpus()[0]?.model ?? "unknown",
    cores: cpus().length,
    memoryGiB: Math.round((totalmem() / 2 ** 30) * 10) / 10,
    filesystem: fsType || "unknown",
    node: process.version,
    hyperfine: run(["hyperfine", "--version"]).stdout.trim(),
  };
}

function resultsPath(): string {
  const date = new Date().toISOString().slice(0, 10);
  return join(RESULTS_DIR, `${date}-${platform()}-${arch()}`);
}

/** Merge into today's results file so separate invocations (e.g. per fixture) accumulate. */
function mergeResults(base: string, update: Results): Results {
  const file = `${base}.json`;
  if (!existsSync(file)) return update;
  const prev: Results = JSON.parse(readFileSync(file, "utf8"));
  const key = (r: { fixture: string; tool: string; scenario?: string }) => `${r.fixture}|${r.tool}|${r.scenario ?? ""}`;
  const merge = <T extends { fixture: string; tool: string; scenario?: string }>(a: T[], b: T[]): T[] => {
    const m = new Map(a.map((r) => [key(r), r]));
    for (const r of b) m.set(key(r), r);
    return [...m.values()];
  };
  return {
    ...update,
    tools: { ...prev.tools, ...update.tools },
    results: merge(prev.results, update.results),
    disk: merge(prev.disk, update.disk),
  };
}

async function main(): Promise<void> {
  const { values } = parseArgs({
    options: {
      fixtures: { type: "string" },
      tools: { type: "string" },
      scenarios: { type: "string" },
      runs: { type: "string", default: "5" },
      warmup: { type: "string", default: "1" },
      disk: { type: "boolean", default: true },
      "no-disk": { type: "boolean", default: false },
      "regen-lockfiles": { type: "boolean", default: false },
      add: { type: "string", default: DEFAULT_ADD_SPEC },
      help: { type: "boolean", short: "h", default: false },
    },
  });
  if (values.help) {
    console.log(
      "usage: node bench/run.ts [--fixtures a,b] [--tools a,b] [--scenarios a,b] [--runs N] [--warmup N] [--no-disk] [--regen-lockfiles] [--add spec]\n" +
        `fixtures:  ${fixtureNames().join(", ")}\ntools:     ${TOOL_NAMES.join(", ")}\nscenarios: ${SCENARIOS.join(", ")}`,
    );
    return;
  }
  if (!which("hyperfine")) throw new Error("hyperfine not found on PATH");
  const fixtures = parseList(values.fixtures, fixtureNames());
  const scenarios = parseList(values.scenarios, SCENARIOS) as Scenario[];
  const requested = parseList(values.tools, TOOL_NAMES) as ToolName[];
  const runs = Number(values.runs);
  const warmup = Number(values.warmup);
  const doDisk = values.disk && !values["no-disk"];

  const registry = registryUrl();
  await startMirror();

  const tools: Tool[] = [];
  const versions: Record<string, string> = {};
  for (const name of requested) {
    const tool = TOOLS[name];
    if (!tool.argv0()) {
      log(name === "fnpm" ? "fnpm: packages/fuck-npm/dist/fnpm.js not built, skipping" : `${name}: not on PATH, skipping`);
      continue;
    }
    versions[name] = tool.version();
    tools.push(tool);
  }
  log(`tools: ${tools.map((t) => `${t.name} ${versions[t.name]}`).join(", ")}`);

  const update: Results = {
    generatedAt: new Date().toISOString(),
    registry,
    machine: machineInfo(),
    tools: versions,
    results: [],
    disk: [],
  };
  const base = resultsPath();
  const flush = () => {
    ensureDir(RESULTS_DIR);
    const merged = mergeResults(base, update);
    writeFileSync(`${base}.json`, `${JSON.stringify(merged, null, 2)}\n`);
    writeFileSync(`${base}.md`, renderMarkdown(merged));
  };

  for (const fixture of fixtures) {
    const workspaces = !!JSON.parse(readFileSync(join(FIXTURES_DIR, fixture, "package.json"), "utf8")).workspaces;
    for (const tool of tools) {
      const p = pathsFor(tool.name, fixture);
      const ctx: Ctx = { work: p.work, state: p.state, registry, workspaces, addSpec: values.add! };
      try {
        prepareSources(tool, fixture, ctx, p);
        ensureLockfile(tool, fixture, ctx, p, values["regen-lockfiles"]!);
      } catch (err) {
        const msg = String((err as Error).message ?? err);
        log(`${tool.name}/${fixture}: setup failed: ${msg}`);
        for (const scenario of scenarios) update.results.push({ fixture, tool: tool.name, scenario, command: "", error: scrub(msg).slice(0, 2000) });
        flush();
        continue;
      }
      for (const scenario of scenarios) {
        update.results.push(runScenario(scenario, tool, fixture, ctx, p, runs, warmup));
        flush();
      }
      if (doDisk) {
        try {
          update.disk.push(runDisk(tool, fixture, ctx, p));
        } catch (err) {
          log(`${tool.name}/${fixture}: disk measurement failed: ${(err as Error).message}`);
        }
        flush();
      }
      // Leave the work tree empty so the next tool's measurements aren't fighting page cache
      // pressure from a large node_modules it doesn't own.
      rmrf(p.work);
    }
  }
  flush();
  log(`wrote ${relative(process.cwd(), base)}.{json,md}`);
  console.log(readFileSync(`${base}.md`, "utf8"));
}

await main();
