#!/usr/bin/env node
// Real-world e2e corpus runner: check out each project at its pinned commit, install it with
// its own package manager (lifecycle scripts enabled), snapshot node_modules, run the project's
// verify command, and write tests/e2e/baselines/<name>.json.
//
//   node tests/e2e/run.ts                       all projects, install + verify
//   node tests/e2e/run.ts --only vue-core,hono  subset
//   node tests/e2e/run.ts --skip-verify         install + snapshot only
//   node tests/e2e/run.ts --verify-only         re-run verify against the existing install
//   node tests/e2e/run.ts --no-mirror           use registry.npmjs.org instead of the local mirror
//
// Checkouts go to $FNPM_E2E_DIR (default ~/.cache/fnpm-e2e). Logs: $FNPM_E2E_DIR/_logs.
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { type CorpusEntry, DEFAULT_MIRROR, E2E_DIR, REPO_ROOT, checkout, loadCorpus, readScriptLog, sh, snapshot, workRoot } from "./lib.ts";

const { values: args } = parseArgs({
  options: {
    only: { type: "string" },
    "skip-verify": { type: "boolean", default: false },
    "verify-only": { type: "boolean", default: false },
    "no-mirror": { type: "boolean", default: false },
  },
});

const PM_BIN: Record<CorpusEntry["pm"], string> = { npm: "npm", "yarn-classic": "yarn", "yarn-berry": "yarn", pnpm: "pnpm", bun: "bun" };

function baseEnv(entry: CorpusEntry, mirror: string | null): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env, CI: "1", COREPACK_ENABLE_DOWNLOAD_PROMPT: "0", ...entry.env };
  if (mirror) {
    Object.assign(env, {
      npm_config_registry: mirror,
      YARN_REGISTRY: mirror,
      YARN_NPM_REGISTRY_SERVER: mirror.replace(/\/$/, ""),
      YARN_UNSAFE_HTTP_WHITELIST: new URL(mirror).hostname,
      BUN_CONFIG_REGISTRY: mirror,
    });
  }
  return env;
}

async function mirrorUp(url: string): Promise<boolean> {
  try {
    const r = await fetch(new URL("-/ping", url), { signal: AbortSignal.timeout(2000) });
    return r.ok;
  } catch {
    return false;
  }
}

function write(path: string, data: unknown): void {
  writeFileSync(path, JSON.stringify(data, null, 1) + "\n");
}

function log(msg: string): void {
  process.stderr.write(`[e2e] ${msg}\n`);
}

async function main(): Promise<void> {
  const only = args.only?.split(",").map((s) => s.trim());
  const corpus = loadCorpus().filter((e) => !only || only.includes(e.name));
  if (!corpus.length) throw new Error("no projects selected");
  const mirror = args["no-mirror"] ? null : (process.env.FNPM_E2E_REGISTRY ?? DEFAULT_MIRROR);
  if (mirror && !(await mirrorUp(mirror))) throw new Error(`registry mirror ${mirror} is down (node bench/mirror.ts start, or pass --no-mirror)`);
  const root = workRoot();
  const logs = join(root, "_logs");
  mkdirSync(logs, { recursive: true });
  const logger = join(E2E_DIR, "script-logger.cjs");
  const summary: string[] = [];

  for (const entry of corpus) {
    const dir = join(root, entry.name);
    const outFile = join(REPO_ROOT, entry.baseline);
    let base: any = existsSync(outFile) ? JSON.parse(readFileSync(outFile, "utf8")) : {};

    if (!args["verify-only"]) {
      log(`${entry.name}: checkout ${entry.repo}@${entry.commit.slice(0, 12)}`);
      checkout(entry, dir);
      const env = baseEnv(entry, mirror);
      const pmVersion = sh(`${PM_BIN[entry.pm]} --version`, dir, env, 300).stdout.trim().split("\n").pop();
      const scriptLog = join(logs, `${entry.name}.scripts.jsonl`);
      rmSync(scriptLog, { force: true });
      const installLog = join(logs, `${entry.name}.install.log`);
      rmSync(installLog, { force: true });
      env.FNPM_E2E_SCRIPT_LOG = scriptLog;
      env.NODE_OPTIONS = `${env.NODE_OPTIONS ?? ""} --require ${logger}`.trim();
      log(`${entry.name}: ${entry.install} (${entry.pm} ${pmVersion})`);
      const inst = sh(entry.install, dir, env, 1800, installLog);
      log(`${entry.name}: install exit=${inst.status} ${(inst.ms / 1000).toFixed(1)}s`);
      const snap = snapshot(dir);
      base = {
        name: entry.name,
        repo: entry.repo,
        commit: entry.commit,
        pm: entry.pm,
        pmVersion,
        node: process.version,
        registry: mirror ? "local-mirror" : "registry.npmjs.org",
        install: { command: entry.install, ok: inst.status === 0, exitCode: inst.status, ms: inst.ms },
        scriptsRan: readScriptLog(scriptLog, dir),
        verify: null,
        counts: { packages: snap.packages.length, instances: snap.instances, importers: Object.keys(snap.importers).length },
        ...snap,
      };
      mkdirSync(join(E2E_DIR, "baselines"), { recursive: true });
      write(outFile, base);
      if (inst.status !== 0) {
        summary.push(`${entry.name}: INSTALL FAILED (see ${installLog})`);
        continue;
      }
    }

    if (!args["skip-verify"]) {
      const verifyLog = join(logs, `${entry.name}.verify.log`);
      rmSync(verifyLog, { force: true });
      log(`${entry.name}: verify: ${entry.verify}`);
      const v = sh(entry.verify, dir, baseEnv(entry, mirror), entry.verifyTimeoutSec ?? 300, verifyLog);
      base.verify = { command: entry.verify, ok: v.status === 0, exitCode: v.status, timedOut: v.timedOut, ms: v.ms };
      write(outFile, base);
      log(`${entry.name}: verify exit=${v.status}${v.timedOut ? " (timeout)" : ""} ${(v.ms / 1000).toFixed(1)}s`);
    }
    summary.push(
      `${entry.name}: install ${base.install?.ok ? "ok" : "FAIL"} ${((base.install?.ms ?? 0) / 1000).toFixed(1)}s, ` +
        `${base.counts?.packages} pkgs, verify ${base.verify ? (base.verify.ok ? "ok" : "FAIL") : "skipped"}`,
    );
  }
  log("summary:\n  " + summary.join("\n  "));
}

main().catch((e) => {
  log(String(e?.stack ?? e));
  process.exit(1);
});
