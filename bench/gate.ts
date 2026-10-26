#!/usr/bin/env node
// Benchmark gate: exit 1 if fnpm is slower than bun on any fixture x scenario.
//
//   node bench/gate.ts [results.json] [--scenarios warm,noop] [--tolerance 0.05]
//
// Defaults to the newest bench/results/*.json. --tolerance allows fnpm to be up to that
// fraction slower (default 0: any slowdown fails). A scenario that fnpm failed or has no
// result for, while bun has one, also fails the gate.
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { fmtTime, type Results } from "./lib/report.ts";
import { RESULTS_DIR } from "./lib/util.ts";

const { values, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    scenarios: { type: "string" },
    fixtures: { type: "string" },
    tolerance: { type: "string", default: "0" },
  },
});

function newestResults(): string {
  const files = existsSync(RESULTS_DIR) ? readdirSync(RESULTS_DIR).filter((f) => {
    if (!f.endsWith(".json")) return false;
    // Real-project reports use a different schema and do not feed this fixture gate.
    try { return Array.isArray(JSON.parse(readFileSync(join(RESULTS_DIR, f), "utf8")).results); }
    catch { return false; }
  }).sort() : [];
  if (!files.length) {
    console.error("gate: no results in bench/results; run node bench/run.ts first");
    process.exit(1);
  }
  return join(RESULTS_DIR, files[files.length - 1]);
}

const file = positionals[0] ?? newestResults();
const res: Results = JSON.parse(readFileSync(file, "utf8"));
if (!Array.isArray(res.results)) {
  console.error("gate: expected a synthetic fixture report from bench/run.ts; real-project reports are separate");
  process.exit(1);
}
const tolerance = Number(values.tolerance);
const onlyScenarios = values.scenarios?.split(",");
const onlyFixtures = values.fixtures?.split(",");

let failures = 0;
let compared = 0;
const lines: string[] = [];
for (const bun of res.results) {
  if (bun.tool !== "bun" || bun.mean === undefined) continue;
  if (onlyScenarios && !onlyScenarios.includes(bun.scenario)) continue;
  if (onlyFixtures && !onlyFixtures.includes(bun.fixture)) continue;
  const fnpm = res.results.find((r) => r.tool === "fnpm" && r.fixture === bun.fixture && r.scenario === bun.scenario);
  const label = `${bun.fixture}/${bun.scenario}`;
  compared++;
  if (!fnpm || fnpm.mean === undefined) {
    failures++;
    lines.push(`FAIL ${label}: fnpm ${fnpm ? "failed" : "has no result"} (bun ${fmtTime(bun.mean)})`);
    continue;
  }
  const ratio = fnpm.mean / bun.mean;
  const ok = ratio <= 1 + tolerance;
  if (!ok) failures++;
  lines.push(`${ok ? "ok  " : "FAIL"} ${label}: fnpm ${fmtTime(fnpm.mean)} vs bun ${fmtTime(bun.mean)} (×${ratio.toFixed(2)})`);
}

console.log(`gate: ${file}`);
for (const l of lines) console.log(l);
if (!compared) {
  console.error("gate: no bun results to compare against");
  process.exit(1);
}
console.log(failures ? `gate: FAILED (${failures}/${compared})` : `gate: passed (${compared} comparisons)`);
process.exit(failures ? 1 : 0);
