#!/usr/bin/env node
// fnpm side of the real-world e2e corpus. For every project:
//
//   1. pristine checkout (only the project's own lockfile), empty isolated store and cache
//   2. cold `fnpm install` (migrates the foreign lockfile), node_modules snapshot compared with
//      the native-PM baseline: resolved name@version set, effective version of every declared
//      dependency, top-level hoisting, .bin entries and lifecycle scripts
//   3. the project's verify command
//   4. no-op re-run, warm re-install (node_modules removed) and frozen re-install
//      (`--frozen-lockfile`), each of which must leave the same tree and lockfile
//
// Before fnpm, bun (the parity target) installs the same pristine checkout (--ignore-scripts).
// A difference from the baseline that bun shows too is reported as bun parity rather than a
// failure; fnpm's own package set is also diffed against bun's. --no-bun skips this.
//
//   node tests/e2e/fnpm.ts                     every project
//   node tests/e2e/fnpm.ts --only vue-core     subset (--skip a,b to leave some out)
//   node tests/e2e/fnpm.ts --skip-verify       no verify step
//
// Binary: $FNPM_BIN, else target/release/fnpm. Registry: $FNPM_E2E_REGISTRY, else the local
// mirror. Checkouts and logs live in $FNPM_E2E_DIR (see README.md). Results:
// tests/e2e/results/<date>.{md,json} (--out <stem> to change).
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { fileURLToPath } from "node:url";
import {
  type CorpusEntry,
  DEFAULT_MIRROR,
  E2E_DIR,
  REPO_ROOT,
  type Snapshot,
  checkout,
  importers,
  loadCorpus,
  readScriptLog,
  resolutionDirs,
  sh,
  shq,
  snapshot,
  workRoot,
} from "./lib.ts";

const { values: args } = parseArgs({
  options: {
    only: { type: "string" },
    skip: { type: "string" },
    "skip-verify": { type: "boolean", default: false },
    "no-bun": { type: "boolean", default: false },
    out: { type: "string" },
  },
});

let FNPM = resolve(process.env.FNPM_BIN || join(REPO_ROOT, "target", "release", "fnpm"));
const BUN_VERSION = args["no-bun"] ? "" : sh("bun --version", REPO_ROOT, process.env, 30).stdout.trim();
const INSTALL_TIMEOUT_SEC = 1800;

type Status = "PASS" | "FAIL" | "KNOWN";
type StepName = "install" | "verify" | "noop" | "warm" | "frozen";

interface Step {
  ok: boolean;
  exitCode: number | null;
  ms: number;
  timedOut?: boolean;
  problems: string[];
}

interface Diff {
  missing: string[];
  extra: string[];
  declared: string[];
  hoisting: { compared: number; differ: number; sample: string[] };
  bins: { missing: string[]; extra: string[] };
  scripts: { baselineOnly: string[]; fnpmOnly: string[] };
  /** Baseline differences bun shows too (accepted as parity). */
  sameAsBun: string[];
  /** fnpm's package set against bun's for the same lockfile. */
  vsBun: { bun: string; ok: boolean; packages: number; missing: string[]; extra: string[] } | null;
}

interface Result {
  name: string;
  pm: string;
  lockfileFormat: string;
  status: Status;
  failures: string[];
  warnings: string[];
  known?: string;
  baseline: { installMs: number; packages: number; verifyOk: boolean | null };
  fnpm: { packages: number; instances: number; layout: string };
  steps: Partial<Record<StepName, Step>>;
  diff: Diff;
}

function log(msg: string): void {
  process.stderr.write(`[e2e:fnpm] ${msg}\n`);
}

function readJson(path: string): any {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return undefined;
  }
}

async function mirrorUp(url: string): Promise<boolean> {
  try {
    return (await fetch(new URL("-/ping", url), { signal: AbortSignal.timeout(2000) })).ok;
  } catch {
    return false;
  }
}

function emptyDiff(): Diff {
  return {
    missing: [],
    extra: [],
    declared: [],
    hoisting: { compared: 0, differ: 0, sample: [] },
    bins: { missing: [], extra: [] },
    scripts: { baselineOnly: [], fnpmOnly: [] },
    sameAsBun: [],
    vsBun: null,
  };
}

function env(entry: CorpusEntry, mirror: string, store: string): NodeJS.ProcessEnv {
  const e: NodeJS.ProcessEnv = {
    ...process.env,
    CI: "1",
    COREPACK_ENABLE_DOWNLOAD_PROMPT: "0",
    ...entry.env,
    FNPM_REGISTRY: mirror,
    FNPM_STORE_DIR: join(store, "store"),
    FNPM_CACHE_DIR: join(store, "cache"),
    // The run uses a private copy of the native binary; retain the repository's JS fallback.
    FNPM_JS_ENTRY: process.env.FNPM_JS_ENTRY ?? join(REPO_ROOT, "packages", "fuck-npm", "dist", "fnpm.js"),
    // Verify commands that go through the project's own PM (or npx) stay on the mirror.
    npm_config_registry: mirror,
    YARN_REGISTRY: mirror,
    YARN_NPM_REGISTRY_SERVER: mirror.replace(/\/$/, ""),
    YARN_UNSAFE_HTTP_WHITELIST: new URL(mirror).hostname,
    BUN_CONFIG_REGISTRY: mirror,
  };
  e.PATH = `${dirname(FNPM)}:${e.PATH ?? ""}`;
  return e;
}

/** Every node_modules directory fnpm may have created (root and workspace importers). */
function removeNodeModules(dir: string): void {
  for (const imp of resolutionDirs(dir)) rmSync(join(dir, imp, "node_modules"), { recursive: true, force: true });
}

function lockHash(dir: string): string | null {
  const p = join(dir, "fnpm.lock");
  return existsSync(p) ? createHash("sha256").update(readFileSync(p)).digest("hex").slice(0, 16) : null;
}

/** What node resolution finds for `name` from importer `imp`: its node_modules, then its ancestors'. */
function effective(imps: Snapshot["importers"], imp: string, name: string): string | undefined {
  let cur = imp;
  for (;;) {
    const v = imps[cur]?.[name];
    if (v !== undefined) return v;
    if (cur === ".") return undefined;
    const up = dirname(cur);
    cur = up === "" ? "." : up;
  }
}

function declared(dir: string, imp: string): string[] {
  const pkg = readJson(join(dir, imp, "package.json")) ?? {};
  const names = new Set<string>();
  for (const f of ["dependencies", "devDependencies", "optionalDependencies"]) for (const n of Object.keys(pkg[f] ?? {})) names.add(n);
  return [...names].sort();
}

function setDiff(a: string[], b: string[]): string[] {
  const s = new Set(b);
  return a.filter((x) => !s.has(x));
}

function layoutOf(dir: string): string {
  const nm = join(dir, "node_modules");
  try {
    if (readdirSync(join(nm, ".fnpm")).some((e) => e !== "node_modules" && existsSync(join(nm, ".fnpm", e, "node_modules")))) return "isolated";
  } catch {}
  return "hoisted";
}

function head(xs: string[], n: number): string {
  return `${xs.slice(0, n).join(", ")}${xs.length > n ? ` (+${xs.length - n})` : ""}`;
}

/** Differences between two snapshots of the same project (re-installs must be identical). */
function snapDiff(a: Snapshot, b: Snapshot): string[] {
  const out: string[] = [];
  const miss = setDiff(a.packages, b.packages);
  const extra = setDiff(b.packages, a.packages);
  if (miss.length) out.push(`packages gone: ${head(miss, 8)}`);
  if (extra.length) out.push(`packages new: ${head(extra, 8)}`);
  for (const imp of new Set([...Object.keys(a.importers), ...Object.keys(b.importers)])) {
    const x = a.importers[imp] ?? {};
    const y = b.importers[imp] ?? {};
    const changed = [...new Set([...Object.keys(x), ...Object.keys(y)])].filter((n) => x[n] !== y[n]);
    if (changed.length) out.push(`${imp}/node_modules changed: ${head(changed.map((n) => `${n} ${x[n] ?? "-"} -> ${y[n] ?? "-"}`), 6)}`);
  }
  for (const imp of new Set([...Object.keys(a.bins), ...Object.keys(b.bins)])) {
    const gone = setDiff(a.bins[imp] ?? [], b.bins[imp] ?? []);
    const added = setDiff(b.bins[imp] ?? [], a.bins[imp] ?? []);
    if (gone.length || added.length) out.push(`${imp}/.bin: -[${head(gone, 6)}] +[${head(added, 6)}]`);
  }
  if (a.instances !== b.instances) out.push(`instances ${a.instances} -> ${b.instances}`);
  return out;
}

function trimList(xs: string[], n = 40): string[] {
  return xs.length > n ? [...xs.slice(0, n), `... +${xs.length - n} more`] : xs;
}

function scrub(s: string): string {
  for (const [path, label] of [[workRoot(), "<e2e>"], [REPO_ROOT, "<repo>"]]) {
    try { s = s.replaceAll(realpathSync(path), label); } catch {}
    s = s.replaceAll(path, label);
  }
  return s;
}

function tailOf(file: string, n = 3): string {
  try {
    const lines = readFileSync(file, "utf8").split("\n").map((l) => l.trim()).filter(Boolean);
    // Jest can print an expected Error from a snapshot before the actual missing-module
    // diagnostic. Preserve the concrete cause for narrow known-failure signatures.
    const firstError = lines.find((l) => /Cannot find (?:module|package)/.test(l))
      ?? lines.find((l) => /\bError:|\berror:|ERR!/.test(l));
    const tail = lines.slice(-n).join(" | ");
    return [firstError && !tail.includes(firstError) ? firstError : "", tail].filter(Boolean).join(" | ").slice(0, 600);
  } catch {
    return "";
  }
}

function runStep(cmd: string, dir: string, e: NodeJS.ProcessEnv, timeoutSec: number, logFile: string): Step {
  const r = sh(cmd, dir, e, timeoutSec, logFile);
  return { ok: r.status === 0, exitCode: r.status, ms: r.ms, timedOut: r.timedOut || undefined, problems: [] };
}

async function project(entry: CorpusEntry, mirror: string): Promise<Result> {
  const root = workRoot();
  const dir = join(root, entry.name);
  const logs = join(root, "_logs");
  const store = join(root, "_fnpm");
  const base = readJson(join(REPO_ROOT, entry.baseline));
  const res: Result = {
    name: entry.name,
    pm: entry.pm,
    lockfileFormat: entry.lockfileFormat,
    status: "PASS",
    failures: [],
    warnings: [],
    baseline: { installMs: base?.install?.ms ?? 0, packages: base?.packages?.length ?? 0, verifyOk: base?.verify ? base.verify.ok : null },
    fnpm: { packages: 0, instances: 0, layout: "" },
    steps: {},
    diff: emptyDiff(),
  };
  const fail = (m: string) => res.failures.push(m);
  const finish = () => {
    if (entry.fnpm?.known) res.known = entry.fnpm.known;
    if (res.failures.length) {
      const expected = entry.fnpm?.knownFailures ?? [];
      res.status = entry.fnpm?.known && res.failures.every((f) => expected.some((p) => new RegExp(p).test(f))) ? "KNOWN" : "FAIL";
    }
    return res;
  };
  if (base && (base.name !== entry.name || base.repo !== entry.repo || base.commit !== entry.commit || base.pm !== entry.pm)) {
    fail("baseline does not match the corpus revision; regenerate it with run.ts");
    return finish();
  }
  if (!base?.install?.ok || !base.verify?.ok) {
    fail("no usable baseline (the native PM install and verification must both pass)");
    return finish();
  }

  rmSync(store, { recursive: true, force: true });
  mkdirSync(store, { recursive: true });
  const e = env(entry, mirror, store);
  const logOf = (step: string) => {
    const p = join(logs, `${entry.name}.fnpm.${step}.log`);
    rmSync(p, { force: true });
    return p;
  };

  // 0. bun on the same pristine checkout: the parity reference for differences from the baseline
  let bunSnap: Snapshot | null = null;
  if (BUN_VERSION) {
    log(`${entry.name}: bun ${BUN_VERSION} install (reference)`);
    checkout(entry, dir);
    const bunLog = logOf("bun");
    const b = runStep("bun install --ignore-scripts", dir, { ...e, BUN_INSTALL_CACHE_DIR: join(store, "bun-cache") }, INSTALL_TIMEOUT_SEC, bunLog);
    if (b.ok) bunSnap = snapshot(dir);
    else res.warnings.push(`bun ${BUN_VERSION} could not install this lockfile (exit ${b.exitCode}: ${tailOf(bunLog, 2)})`);
    log(`${entry.name}: bun exit=${b.exitCode} ${(b.ms / 1000).toFixed(1)}s`);
  }

  log(`${entry.name}: checkout`);
  checkout(entry, dir);
  const scriptLog = join(logs, `${entry.name}.fnpm.scripts.jsonl`);
  rmSync(scriptLog, { force: true });
  const installEnv = { ...e, FNPM_E2E_SCRIPT_LOG: scriptLog, NODE_OPTIONS: `${e.NODE_OPTIONS ?? ""} --require ${join(E2E_DIR, "script-logger.cjs")}`.trim() };

  // 1. cold install from the project's own lockfile
  log(`${entry.name}: fnpm install (cold, ${entry.lockfileFormat})`);
  const installLog = logOf("install");
  const install = runStep(`${shq(FNPM)} install`, dir, installEnv, INSTALL_TIMEOUT_SEC, installLog);
  res.steps.install = install;
  log(`${entry.name}: install exit=${install.exitCode} ${(install.ms / 1000).toFixed(1)}s`);
  if (!install.ok) {
    fail(`install exited ${install.exitCode}${install.timedOut ? " (timeout)" : ""}: ${tailOf(installLog)}`);
    return finish();
  }
  const lock0 = lockHash(dir);
  if (!lock0) fail("no fnpm.lock written");
  const snap = snapshot(dir);
  res.fnpm = { packages: snap.packages.length, instances: snap.instances, layout: layoutOf(dir) };

  // 2. compare with the baseline; what bun does the same way is parity, not a failure
  const allow = entry.fnpm?.allow ?? {};
  const allowed = (id: string, pats: string[] | undefined) => (pats ?? []).some((p) => new RegExp(p).test(id));
  const bunSet = new Set(bunSnap?.packages ?? []);
  const asBun = (what: string, ok: boolean) => {
    if (ok) res.diff.sameAsBun.push(what);
    return !ok;
  };
  if (bunSnap) {
    res.diff.vsBun = { bun: BUN_VERSION, ok: true, packages: bunSnap.packages.length, missing: setDiff(bunSnap.packages, snap.packages), extra: setDiff(snap.packages, bunSnap.packages) };
  }
  res.diff.missing = setDiff(base.packages, snap.packages);
  res.diff.extra = setDiff(snap.packages, base.packages);
  const missing = res.diff.missing.filter((id) => !allowed(id, allow.missing) && asBun(`not installed: ${id}`, bunSnap !== null && !bunSet.has(id)));
  const extra = res.diff.extra.filter((id) => !allowed(id, allow.extra) && asBun(`installed, not in baseline: ${id}`, bunSet.has(id)));
  if (missing.length) fail(`${missing.length} baseline packages not installed: ${head(missing, 10)}`);
  if (extra.length) fail(`${extra.length} packages not in the baseline: ${head(extra, 10)}`);

  for (const imp of importers(dir)) {
    for (const n of declared(dir, imp)) {
      const want = effective(base.importers, imp, n);
      const got = effective(snap.importers, imp, n);
      if (want === got || allowed(`${imp}:${n}`, allow.declared)) continue;
      const what = `${imp}: ${n} baseline=${want ?? "-"} fnpm=${got ?? "-"}`;
      if (asBun(what, bunSnap !== null && effective(bunSnap.importers, imp, n) === got)) res.diff.declared.push(what);
    }
  }
  if (res.diff.declared.length) fail(`${res.diff.declared.length} declared dependencies resolve differently: ${head(res.diff.declared, 6)}`);

  const baseIsolated = entry.pm === "pnpm";
  if (baseIsolated === (res.fnpm.layout === "isolated")) {
    const x = base.importers["."] ?? {};
    const y = snap.importers["."] ?? {};
    const names = [...new Set([...Object.keys(x), ...Object.keys(y)])].sort();
    const differ = names.filter((n) => x[n] !== y[n]);
    res.diff.hoisting = { compared: names.length, differ: differ.length, sample: differ.slice(0, 15).map((n) => `${n}: ${x[n] ?? "-"} vs ${y[n] ?? "-"}`) };
    if (differ.length) res.warnings.push(`top-level node_modules: ${differ.length}/${names.length} entries differ from the baseline`);
    for (const imp of Object.keys(base.bins)) {
      res.diff.bins.missing.push(...setDiff(base.bins[imp], snap.bins[imp] ?? []).map((b) => `${imp}:${b}`));
      res.diff.bins.extra.push(...setDiff(snap.bins[imp] ?? [], base.bins[imp]).map((b) => `${imp}:${b}`));
    }
    const bunBins = (b: string) => {
      const i = b.indexOf(":");
      return bunSnap !== null && !(bunSnap.bins[b.slice(0, i)] ?? []).includes(b.slice(i + 1));
    };
    const binMissing = res.diff.bins.missing.filter((b) => !allowed(b, allow.bins) && asBun(`.bin missing: ${b}`, bunBins(b)));
    if (binMissing.length) fail(`${binMissing.length} .bin entries missing: ${head(binMissing, 10)}`);
  } else {
    res.warnings.push(`layouts differ (baseline ${baseIsolated ? "isolated" : "hoisted"}, fnpm ${res.fnpm.layout}); hoisting and .bin not compared`);
  }

  const ran = readScriptLog(scriptLog, dir);
  res.diff.scripts = { baselineOnly: setDiff(base.scriptsRan ?? [], ran), fnpmOnly: setDiff(ran, base.scriptsRan ?? []) };
  // Workspace lifecycle scripts (the "(dir)" suffix) run under every PM. Dependency scripts only
  // run when trusted, which differs by design (bun's default trusted list vs npm/yarn running all).
  const wsMissed = res.diff.scripts.baselineOnly.filter((s) => / \(.+\)$/.test(s) && !allowed(s, allow.scripts));
  if (wsMissed.length) fail(`workspace lifecycle scripts not run: ${wsMissed.join(", ")}`);

  // 3. verify
  if (!args["skip-verify"]) {
    const verifyCmd = entry.fnpm?.verify ?? entry.verify;
    const verifyLog = logOf("verify");
    log(`${entry.name}: verify: ${verifyCmd}`);
    const v = runStep(verifyCmd, dir, e, entry.verifyTimeoutSec ?? 300, verifyLog);
    res.steps.verify = v;
    log(`${entry.name}: verify exit=${v.exitCode} ${(v.ms / 1000).toFixed(1)}s`);
    if (!v.ok) fail(`verify exited ${v.exitCode}${v.timedOut ? " (timeout)" : ""}: ${tailOf(verifyLog)}`);
  }

  // 4. no-op, warm, frozen: same tree, same lockfile
  const again = (step: "noop" | "warm" | "frozen", cmd: string) => {
    if (step !== "noop") removeNodeModules(dir);
    const stepLog = logOf(step);
    const s = runStep(cmd, dir, e, INSTALL_TIMEOUT_SEC, stepLog);
    res.steps[step] = s;
    if (!s.ok) {
      s.problems.push(`exit ${s.exitCode}`);
      fail(`${step} install exited ${s.exitCode}: ${tailOf(stepLog)}`);
    } else {
      s.problems.push(...snapDiff(snap, snapshot(dir)));
      const h = lockHash(dir);
      if (h !== lock0) s.problems.push(`fnpm.lock changed (${lock0} -> ${h})`);
      if (s.problems.length) fail(`${step} install changed the tree: ${s.problems.slice(0, 4).join("; ")}`);
    }
    log(`${entry.name}: ${step} exit=${s.exitCode} ${(s.ms / 1000).toFixed(2)}s${s.problems.length ? ` (${s.problems.length} problems)` : ""}`);
  };
  again("noop", `${shq(FNPM)} install`);
  again("warm", `${shq(FNPM)} install`);
  again("frozen", `${shq(FNPM)} install --frozen-lockfile`);
  return finish();
}

function sec(ms: number | undefined): string {
  return ms === undefined ? "-" : ms >= 10_000 ? `${(ms / 1000).toFixed(1)}s` : `${(ms / 1000).toFixed(2)}s`;
}

export function report(results: Result[], fnpmVersion: string, commit: string, date: string): string {
  const pass = results.filter((r) => r.status === "PASS").length;
  const known = results.filter((r) => r.status === "KNOWN").length;
  const lines = [
    `# e2e corpus: fnpm ${fnpmVersion} (${commit}), ${date}`,
    "",
    `${pass}/${results.length} PASS${known ? `, ${known} known incompatibilities` : ""}. Native fnpm (release build), local registry mirror,`,
    "empty isolated store and cache per project. Baselines are the project's own package manager and lockfile",
    "(`baselines/<name>.json`). Generated by `node tests/e2e/fnpm.ts`; times are wall clock on the shared",
    "8-core Linux devbox and only indicative.",
    "",
    "| project | PM / lockfile | status | packages (base/fnpm) | cold | noop | warm | frozen | verify | baseline PM install |",
    "|---|---|---|---|---|---|---|---|---|---|",
  ];
  for (const r of results) {
    const s = r.steps;
    const v = s.verify ? (s.verify.ok ? `ok ${sec(s.verify.ms)}` : "FAIL") : "-";
    lines.push(
      `| ${r.name} | ${r.pm} / ${r.lockfileFormat} | ${r.status} | ${r.baseline.packages}/${r.fnpm.packages} | ${sec(s.install?.ms)} | ${sec(s.noop?.ms)} | ${sec(s.warm?.ms)} | ${sec(s.frozen?.ms)} | ${v} | ${sec(r.baseline.installMs)} |`,
    );
  }
  lines.push("", "## Details", "");
  for (const r of results) {
    lines.push(`### ${r.name}: ${r.status}`, "");
    if (r.known) lines.push(`Known incompatibility: ${r.known}`, "");
    for (const f of r.failures) lines.push(`- FAIL: ${f}`);
    for (const w of r.warnings) lines.push(`- note: ${w}`);
    if (r.diff.hoisting.differ) lines.push(`- hoisting sample (baseline vs fnpm): ${r.diff.hoisting.sample.slice(0, 8).join("; ")}`);
    if (r.diff.bins.extra.length) lines.push(`- extra .bin entries: ${head(r.diff.bins.extra, 10)}`);
    if (r.diff.sameAsBun.length) lines.push(`- same as bun (parity, not a failure): ${head(r.diff.sameAsBun, 12)}`);
    const vb = r.diff.vsBun;
    if (vb) lines.push(vb.missing.length || vb.extra.length ? `- vs bun ${vb.bun} (${vb.packages} packages): fnpm lacks [${head(vb.missing, 8)}], fnpm adds [${head(vb.extra, 8)}]` : `- package set identical to bun ${vb.bun}`);
    const sc = r.diff.scripts;
    if (sc.baselineOnly.length || sc.fnpmOnly.length) lines.push(`- lifecycle scripts: baseline only [${head(sc.baselineOnly, 8)}]; fnpm only [${head(sc.fnpmOnly, 8)}]`);
    if (!r.failures.length && !r.warnings.length && !r.diff.sameAsBun.length) lines.push("- identical package set, declared dependencies, top level, .bin and re-installs");
    lines.push("");
  }
  return lines.join("\n");
}

async function main(): Promise<void> {
  if (!existsSync(FNPM)) throw new Error(`${FNPM} not found (cargo build --release -p fnpm-cli, or set FNPM_BIN)`);
  // A concurrent cargo build must not replace the executable between corpus projects.
  const binaryDir = mkdtempSync(join(tmpdir(), "fnpm-e2e-bin-"));
  const pinnedBinary = join(binaryDir, "fnpm");
  copyFileSync(FNPM, pinnedBinary);
  FNPM = pinnedBinary;
  const binarySha256 = createHash("sha256").update(readFileSync(FNPM)).digest("hex");
  try {
    const only = args.only?.split(",").map((s) => s.trim());
    const skip = new Set(args.skip?.split(",").map((s) => s.trim()) ?? []);
    const corpus = loadCorpus().filter((e) => (!only || only.includes(e.name)) && !skip.has(e.name));
    if (!corpus.length) throw new Error("no projects selected");
    const mirror = process.env.FNPM_E2E_REGISTRY ?? DEFAULT_MIRROR;
    if (!(await mirrorUp(mirror))) throw new Error(`registry mirror ${mirror} is down (node bench/mirror.ts start)`);
    mkdirSync(join(workRoot(), "_logs"), { recursive: true });
    const version = sh(`${shq(FNPM)} --version`, REPO_ROOT).stdout.trim();
    const headCommit = sh("git rev-parse --short HEAD", REPO_ROOT).stdout.trim();
    const dirty = sh("git diff --quiet -- crates tests/e2e/*.ts tests/e2e/corpus.json", REPO_ROOT).status !== 0;
    const commit = `${headCommit}${dirty ? "+dirty" : ""}`;
    const date = new Date().toISOString().slice(0, 10);
    const stem = args.out ?? join(E2E_DIR, "results", date);
    mkdirSync(dirname(stem), { recursive: true });

    const results: Result[] = [];
    for (const entry of corpus) {
      let r: Result;
      try {
        r = await project(entry, mirror);
      } catch (err: any) {
        r = {
          name: entry.name,
          pm: entry.pm,
          lockfileFormat: entry.lockfileFormat,
          status: "FAIL",
          failures: [`runner error: ${err?.message ?? err}`],
          warnings: [],
          baseline: { installMs: 0, packages: 0, verifyOk: null },
          fnpm: { packages: 0, instances: 0, layout: "" },
          steps: {},
          diff: emptyDiff(),
        };
      }
      log(`${entry.name}: ${r.status}${r.failures.length ? `\n    ${r.failures.join("\n    ")}` : ""}`);
      results.push(r);
      const compact = results.map((x) => ({
        ...x,
        diff: {
          ...x.diff,
          missing: trimList(x.diff.missing),
          extra: trimList(x.diff.extra),
          declared: trimList(x.diff.declared),
          sameAsBun: trimList(x.diff.sameAsBun),
          vsBun: x.diff.vsBun && { ...x.diff.vsBun, missing: trimList(x.diff.vsBun.missing), extra: trimList(x.diff.vsBun.extra) },
        },
      }));
      writeFileSync(`${stem}.json`, JSON.stringify({ fnpm: version, commit, binarySha256, date, results: compact }, (_key, value) => typeof value === "string" ? scrub(value) : value, 1) + "\n");
      writeFileSync(`${stem}.md`, scrub(report(results, version, commit, date)) + "\n");
    }
    rmSync(join(workRoot(), "_fnpm"), { recursive: true, force: true });
    log(`summary: ${results.map((r) => `${r.name}=${r.status}`).join(" ")}`);
    if (results.some((r) => r.status === "FAIL")) process.exitCode = 1;
  } finally {
    rmSync(binaryDir, { recursive: true, force: true });
  }
}

if (resolve(process.argv[1] ?? "") === fileURLToPath(import.meta.url)) {
  main().catch((e) => {
    log(String(e?.stack ?? e));
    process.exit(1);
  });
}
