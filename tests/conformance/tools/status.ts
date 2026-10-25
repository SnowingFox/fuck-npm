#!/usr/bin/env node
/**
 * Runs the conformance suite one file at a time and writes the pass counts to tests/conformance/STATUS.md.
 *
 *   FNPM_EXE=target/release/fnpm node tests/conformance/tools/status.ts                 # every file
 *   FNPM_EXE=target/release/fnpm node tests/conformance/tools/status.ts bun-add bun-remove  # some files
 *
 * A run of some files keeps the rows of the others. Files run one after the other (each file may still run its
 * tests concurrently), with a time limit per file (FNPM_STATUS_TIMEOUT seconds, 900 by default).
 * Set FNPM_STATUS_REPORT_DIR to keep JSON failure reports for debugging.
 */
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const conformanceDir = resolve(here, "..");
const testsDir = resolve(conformanceDir, "..");
const installDir = join(conformanceDir, "install");
const statusPath = join(conformanceDir, "STATUS.md");
const vitestBin = join(dirname(createRequire(join(testsDir, "package.json")).resolve("vitest/package.json")), "vitest.mjs");

interface Row {
  file: string;
  passed: number;
  failed: number;
  skipped: number;
  note: string;
}

function allFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory() && entry.name !== "__snapshots__") out.push(...allFiles(path));
    else if (entry.name.endsWith(".test.ts")) out.push(relative(installDir, path));
  }
  return out.sort();
}

function readRows(): Map<string, Row> {
  const rows = new Map<string, Row>();
  if (!fs.existsSync(statusPath)) return rows;
  for (const line of fs.readFileSync(statusPath, "utf8").split("\n")) {
    const m = /^\| `([^`]+)` \| (\d+) \| (\d+) \| (\d+) \| [^|]* \| (.*) \|$/.exec(line);
    if (m) rows.set(m[1], { file: m[1], passed: +m[2], failed: +m[3], skipped: +m[4], note: m[5].trim() });
  }
  return rows;
}

/** The ported snapshot files: vitest rewrites them after a run, so each run puts them back. */
function snapshots(dir = installDir, all = new Map<string, string>()): Map<string, string> {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (!entry.isDirectory()) continue;
    if (entry.name !== "__snapshots__") snapshots(path, all);
    else for (const name of fs.readdirSync(path)) all.set(join(path, name), fs.readFileSync(join(path, name), "utf8"));
  }
  return all;
}

function run(file: string): Row {
  const kept = snapshots();
  try {
    return runFile(file);
  } finally {
    for (const path of snapshots().keys()) if (!kept.has(path)) fs.rmSync(path);
    for (const [path, text] of kept) if (!fs.existsSync(path) || fs.readFileSync(path, "utf8") !== text) fs.writeFileSync(path, text);
  }
}

function runFile(file: string): Row {
  const out = join(fs.mkdtempSync(join(os.tmpdir(), "fnpm-status-")), "report.json");
  const timeout = Number(process.env.FNPM_STATUS_TIMEOUT ?? 900) * 1000;
  const res = spawnSync(
    process.execPath,
    [
      vitestBin,
      "run",
      "--project",
      "conformance",
      join("conformance", "install", file),
      "--reporter=json",
      `--outputFile=${out}`,
    ],
    {
      cwd: testsDir,
      env: { ...process.env, CI: "1" },
      stdio: "ignore",
      detached: process.platform !== "win32",
      timeout,
      killSignal: "SIGKILL",
    },
  );
  // spawnSync kills only Vitest on timeout. Its workers and spawned CLIs share this
  // process group; kill them too, so a timed-out file cannot keep running into the next.
  if (res.error && res.pid && process.platform !== "win32") {
    try { process.kill(-res.pid, "SIGKILL"); } catch {}
  }
  if (!fs.existsSync(out)) {
    fs.rmSync(dirname(out), { recursive: true, force: true });
    const why = res.error
      ? (res.error as NodeJS.ErrnoException).code === "ETIMEDOUT"
        ? `timed out after ${timeout / 1000}s`
        : res.error.message
      : `vitest exited ${res.status}`;
    return { file, passed: 0, failed: 0, skipped: 0, note: why };
  }
  if (process.env.FNPM_STATUS_REPORT_DIR) {
    const reportPath = join(resolve(process.env.FNPM_STATUS_REPORT_DIR), file.replace(/\.test\.ts$/, ".json"));
    fs.mkdirSync(dirname(reportPath), { recursive: true });
    fs.copyFileSync(out, reportPath);
  }
  const report = JSON.parse(fs.readFileSync(out, "utf8"));
  fs.rmSync(dirname(out), { recursive: true, force: true });
  const skipped = (report.numPendingTests ?? 0) + (report.numTodoTests ?? 0);
  const loadError = report.testResults?.some((r: { message?: string; assertionResults: unknown[] }) => r.message && r.assertionResults.length === 0);
  return {
    file,
    passed: report.numPassedTests,
    failed: report.numFailedTests,
    skipped,
    note: loadError ? "file failed to load"
      : res.error ? res.error.message
      : res.signal ? `vitest killed by ${res.signal}`
      : report.numFailedTests === 0 && (report.success === false || res.status !== 0)
        ? `suite failed (vitest exited ${res.status})` : "",
  };
}

function write(rows: Map<string, Row>) {
  const list = [...rows.values()].sort((a, b) => a.file.localeCompare(b.file));
  const total = list.reduce((t, r) => ({ passed: t.passed + r.passed, failed: t.failed + r.failed, skipped: t.skipped + r.skipped }), { passed: 0, failed: 0, skipped: 0 });
  const rate = (r: { passed: number; failed: number }) => (r.passed + r.failed === 0 ? "-" : `${((100 * r.passed) / (r.passed + r.failed)).toFixed(1)}%`);
  const lines = [
    "# Conformance status",
    "",
    "Pass counts of the ported bun tests (tests/conformance/install) against the native release binary",
    "(`FNPM_EXE=target/release/fnpm`). Regenerate with `node tests/conformance/tools/status.ts [file ...]`;",
    "`FNPM_BUN_DIR` points at a bun checkout when the submodule is not initialised.",
    "Only listed files have been measured. Each row keeps its latest per-file result; a partial run preserves other rows.",
    "Timeouts and load failures are explicit in the note column; files without counts are excluded from the pass rate.",
    "",
    `Total: ${total.passed} passed, ${total.failed} failed, ${total.skipped} skipped (${rate(total)} of the tests that ran).`,
    "",
    "| file | passed | failed | skipped | pass rate | note |",
    "| --- | ---: | ---: | ---: | ---: | --- |",
    ...list.map(r => `| \`${r.file}\` | ${r.passed} | ${r.failed} | ${r.skipped} | ${rate(r)} | ${r.note} |`),
    "",
  ];
  fs.writeFileSync(statusPath, lines.join("\n"));
}

const wanted = process.argv.slice(2);
const files = allFiles(installDir).filter(
  f => wanted.length === 0 || wanted.some(w => f === w || f === `${w}.test.ts` || f.replace(/\.test\.ts$/, "") === w),
);
const rows = readRows();
for (const file of files) {
  const started = Date.now();
  const row = run(file);
  rows.set(file, row);
  write(rows);
  console.log(`${file}: ${row.passed} passed, ${row.failed} failed, ${row.skipped} skipped ${row.note} (${((Date.now() - started) / 1000).toFixed(0)}s)`);
}
