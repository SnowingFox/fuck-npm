// Results schema and Markdown rendering.
import type { DiskReport } from "./disk.ts";

export interface ScenarioResult {
  fixture: string;
  tool: string;
  scenario: string;
  command: string;
  runs?: number;
  /** Seconds. */
  mean?: number;
  stddev?: number;
  median?: number;
  min?: number;
  max?: number;
  times?: number[];
  error?: string;
}

export interface DiskResult extends DiskReport {
  fixture: string;
  tool: string;
  scenario?: undefined;
  /** Filesystem used-space growth across a cold install (lockfile present, empty cache). */
  coldDfDeltaBytes: number;
  coldSeconds: number;
}

export interface Results {
  generatedAt: string;
  /** Source revision and exact executable used by this invocation. */
  sourceCommit?: string;
  binarySha256?: string;
  registry: string;
  machine: {
    os: string;
    platform: string;
    arch: string;
    cpu: string;
    cores: number;
    memoryGiB: number;
    filesystem: string;
    node: string;
    hyperfine: string;
  };
  tools: Record<string, string>;
  results: ScenarioResult[];
  disk: DiskResult[];
}

const SCENARIO_ORDER = ["cold", "cold-lockfile", "warm", "noop", "add", "ci"];
const TOOL_ORDER = ["fnpm", "bun", "pnpm", "npm", "yarn"];

const byOrder = (order: string[]) => (a: string, b: string) => {
  const ia = order.indexOf(a);
  const ib = order.indexOf(b);
  return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib) || a.localeCompare(b);
};

export function fmtTime(s: number): string {
  if (s < 1) return `${(s * 1000).toFixed(s < 0.1 ? 1 : 0)} ms`;
  return `${s.toFixed(s < 10 ? 2 : 1)} s`;
}

export function fmtBytes(n: number): string {
  const units = ["B", "KiB", "MiB", "GiB"];
  let v = Math.abs(n);
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${n < 0 ? "-" : ""}${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function fmtCount(n: number): string {
  return n.toLocaleString("en-US");
}

/** Fastest successful mean per fixture|scenario. */
export function fastest(results: ScenarioResult[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const r of results) {
    if (r.mean === undefined) continue;
    const k = `${r.fixture}|${r.scenario}`;
    if (!m.has(k) || r.mean < m.get(k)!) m.set(k, r.mean);
  }
  return m;
}

export function renderMarkdown(res: Results): string {
  const out: string[] = [];
  const m = res.machine;
  out.push(`# Install benchmark: ${res.generatedAt.slice(0, 10)} ${m.platform}-${m.arch}`, "");
  out.push(`- Machine: ${m.cpu}, ${m.cores} cores, ${m.memoryGiB} GiB RAM, ${m.os}, filesystem ${m.filesystem}`);
  out.push(`- Node ${m.node}, ${m.hyperfine}`);
  if (res.sourceCommit) out.push(`- Source revision: ${res.sourceCommit}`);
  if (res.binarySha256) out.push(`- Native fnpm SHA-256: ${res.binarySha256}`);
  out.push(`- Registry: local verdaccio mirror (${res.registry}), pre-warmed`);
  const tools = Object.keys(res.tools).sort(byOrder(TOOL_ORDER));
  out.push(`- Tools: ${tools.map((t) => `${t} ${res.tools[t]}`).join(", ")}`);
  out.push(`- Methodology: bench/README.md. Times are wall-clock seconds; relative is mean / fastest mean in the row.`, "");

  const best = fastest(res.results);
  const fixtures = [...new Set(res.results.map((r) => r.fixture))].sort();
  for (const fixture of fixtures) {
    const rows = res.results.filter((r) => r.fixture === fixture);
    const fixtureTools = [...new Set(rows.map((r) => r.tool))].sort(byOrder(TOOL_ORDER));
    const scenarios = [...new Set(rows.map((r) => r.scenario))].sort(byOrder(SCENARIO_ORDER));
    out.push(`## ${fixture}`, "");
    out.push(`| scenario | ${fixtureTools.join(" | ")} |`);
    out.push(`|---|${fixtureTools.map(() => "---:").join("|")}|`);
    for (const scenario of scenarios) {
      const cells = fixtureTools.map((tool) => {
        const r = rows.find((x) => x.tool === tool && x.scenario === scenario);
        if (!r) return "";
        if (r.mean === undefined) return "failed";
        const rel = r.mean / best.get(`${fixture}|${scenario}`)!;
        const cell = `${fmtTime(r.mean)} ± ${fmtTime(r.stddev ?? 0)} (×${rel.toFixed(2)})`;
        return rel === 1 ? `**${cell}**` : cell;
      });
      out.push(`| ${scenario} | ${cells.join(" | ")} |`);
    }
    out.push("");
    out.push("<details><summary>min / max / runs</summary>", "");
    out.push("| scenario | tool | mean | σ | min | max | runs |", "|---|---|---:|---:|---:|---:|---:|");
    for (const scenario of scenarios) {
      for (const tool of fixtureTools) {
        const r = rows.find((x) => x.tool === tool && x.scenario === scenario);
        if (!r || r.mean === undefined) continue;
        out.push(
          `| ${scenario} | ${tool} | ${fmtTime(r.mean)} | ${fmtTime(r.stddev ?? 0)} | ${fmtTime(r.min!)} | ${fmtTime(r.max!)} | ${r.runs} |`,
        );
      }
    }
    out.push("", "</details>", "");

    const disk = res.disk.filter((d) => d.fixture === fixture).sort((a, b) => byOrder(TOOL_ORDER)(a.tool, b.tool));
    if (disk.length) {
      out.push(`### ${fixture}: disk`, "");
      out.push("| tool | node_modules | store/cache (extra) | total physical | node_modules entries | unique inodes | cold df delta |");
      out.push("|---|---:|---:|---:|---:|---:|---:|");
      for (const d of disk) {
        out.push(
          `| ${d.tool} | ${fmtBytes(d.nodeModulesBytes)} | ${fmtBytes(d.storeBytes)} | ${fmtBytes(d.totalBytes)} | ${fmtCount(d.nodeModulesEntries)} | ${fmtCount(d.inodes)} | ${fmtBytes(d.coldDfDeltaBytes)} |`,
        );
      }
      out.push("");
    }

    const failed = rows.filter((r) => r.error);
    if (failed.length) {
      out.push(`### ${fixture}: failures`, "");
      for (const r of failed) {
        out.push(`- ${r.tool} ${r.scenario}:`, "", "```", r.error!.slice(0, 1500), "```", "");
      }
    }
  }
  return `${out.join("\n")}\n`;
}
