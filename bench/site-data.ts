#!/usr/bin/env node
// Turns a real.ts results.json into the documentation site's chart data
// (website/src/data/benchmarks.json). Every number comes from the results file.
// node bench/site-data.ts <results.json> <out.json>
import { readFileSync, writeFileSync } from "node:fs";

const [input, output] = process.argv.slice(2);
if (!input || !output) throw new Error("Usage: node bench/site-data.ts <results.json> <out.json>");
const results = JSON.parse(readFileSync(input, "utf8"));

// Display order and names; projects without results are left out.
const projects: [string, string, string][] = [
  ["opennextjs-cloudflare", "OpenNext", "OpenNext Cloudflare"],
  ["opencode", "OpenCode", "OpenCode"],
  ["trpc", "tRPC", "tRPC"],
  ["excalidraw", "Excalidraw", "Excalidraw"],
  ["jest", "Jest", "Jest"],
  ["react", "React", "React"],
  ["vite", "Vite", "Vite"],
  ["vue-core", "Vue", "Vue core"],
  ["webpack", "webpack", "webpack"],
  ["uptime-kuma", "Uptime Kuma", "Uptime Kuma"],
];
const tools = ["fnpm", "bun", "pnpm", "yarn", "npm"];
const scenarios = ["cold-lockfile", "warm", "noop"];
const originalPm: Record<string, string> = { "yarn.lock": "Yarn", "pnpm-lock.yaml": "pnpm", "package-lock.json": "npm", "bun.lock": "Bun", "bun.lockb": "Bun" };
// A short reason for a tool that could not install a repository; the log line stays in the JSON.
const reason = (tool: string, raw: string) => {
  if (/EUNSUPPORTEDPROTOCOL|Unsupported URL Type "workspace:"|Unsupported URL Type "catalog:"/.test(raw)) return `${tool} cannot read workspace: or catalog: dependency specifiers`;
  if (/"catalog:"|catalog:/.test(raw)) return `${tool} cannot read catalog: dependency specifiers`;
  if (/"workspace:|workspace:\*/.test(raw)) return `${tool} cannot read workspace: dependency specifiers`;
  if (/Workspace name .* already exists/.test(raw)) return `${tool} rejects duplicate workspace package names`;
  return raw;
};
const mib = (bytes: number) => Math.round((bytes / 1024 / 1024) * 100) / 100;
const round = (s: number) => Math.round(s * 10000) / 10000;

// Repositories still being measured are listed, not charted.
const pending: string[] = [];
const out = projects.flatMap(([id, short, name]) => {
  const p = results.projects[id];
  if (!p) { pending.push(name); return []; }
  const rows = tools.flatMap((tool) => {
    const t = p.tools[tool];
    if (!t) return [];
    const done = scenarios.every((s) => t.scenarios?.[s]?.ok);
    const failed = scenarios.find((s) => t.scenarios?.[s] && !t.scenarios[s].ok);
    const unsupported = t.unsupported ? reason(tool, t.unsupported) : failed ? `the ${failed} install failed` : undefined;
    // A tool counts once it was checked after its timings (disk is measured then), or failed.
    if (!unsupported && !(done && t.verify)) return [];
    const command = t.verify?.command?.at(-1);
    return [{
      name: tool,
      version: t.version ?? "",
      lockSource: t.lockSource,
      ...(unsupported ? { unsupported } : {}),
      times: done ? scenarios.map((s) => round(t.scenarios[s].median)) : null,
      ranges: done ? scenarios.map((s) => [round(t.scenarios[s].min), round(t.scenarios[s].max)]) : null,
      disk: t.disk ? { nodeModules: mib(t.disk.nodeModulesBytes), store: mib(t.disk.storeBytes), total: mib(t.disk.totalBytes) } : null,
      packages: t.correctness?.packageCount ?? null,
      instances: t.correctness?.instances ?? null,
      missingDeclared: typeof t.correctness?.missingDeclaredDependencies === "number" ? t.correctness.missingDeclaredDependencies : (t.correctness?.missingDeclaredDependencies?.length ?? null),
      versusBun: t.compareBun ? { missing: t.compareBun.missing.length, extra: t.compareBun.extra.length } : null,
      check: t.verify ? (t.verify.ok ? "pass" : "fail") : null,
      ...(command ? { checkCommand: command } : {}),
    }];
  });
  if (rows.length < tools.length) { pending.push(name); return []; }
  const check = rows.find((r) => r.checkCommand)?.checkCommand;
  return [{
    id, short, name,
    repo: p.repo,
    commit: p.commit,
    workspaces: p.workspaceCount,
    originalLock: p.originalLock,
    originalPm: originalPm[p.originalLock],
    check,
    tools: rows.map(({ checkCommand, ...row }) => row),
  }];
});

writeFileSync(output, JSON.stringify({
  measured: String(results.date).slice(0, 10),
  machine: results.machine,
  fnpm: Object.values<any>(results.projects).map((p) => p.tools.fnpm).find((t) => t?.sourceRevision)?.sourceRevision,
  projects: out,
  pending,
}, null, 2) + "\n");
console.log(`${out.length} projects, ${out.reduce((n, p) => n + p.tools.length, 0)} tool rows -> ${output}`);
