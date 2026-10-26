#!/usr/bin/env node
// Export a portable report from real.ts results and optional follow-up evidence.
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = process.argv[2] && resolve(process.argv[2]);
const output = process.argv[3] && resolve(process.argv[3]);
if (!root || !output) throw new Error("Usage: node bench/real-report.ts <scratch> <output-stem>");
const read = (path: string) => JSON.parse(readFileSync(path, "utf8"));
const results = read(join(root, "results.json"));
const cleanDisk = join(root, "clean-bun-trpc-disk.json");
if (existsSync(cleanDisk)) {
  const clean = read(cleanDisk);
  if (clean.exitCode === 0 && clean.samePackageSet === true) {
    const bun = results.projects.trpc.tools.bun;
    bun.disk = clean.disk;
    bun.diskRemeasured = "Pristine source/native-lock install with empty private state and scripts disabled; package-version set unchanged. Replaces earlier disk evidence polluted by a verification command.";
  }
}
const lifecycle = join(root, "opennext-lifecycle.json");
if (existsSync(lifecycle)) results.openNextLifecycle = read(lifecycle);
for (const project of Object.values<any>(results.projects)) {
  if (project.repo === "trpc/trpc" && project.tools.bun?.disk && !project.tools.bun.diskRemeasured) {
    project.tools.bun.diskInvalidated = "Earlier verification created unrelated caches; a clean measurement is required.";
    delete project.tools.bun.disk;
  }
}
results.harnessBaseRevision ??= results.harnessRevision;
delete results.harnessRevision;
results.harnessSha256 = createHash("sha256").update(readFileSync(join(dirname(fileURLToPath(import.meta.url)), "real.ts"))).digest("hex");
results.harnessAudit = "Final harness adds explicit OS/CPU declaration accounting, portable source/provenance inputs and uncapped execution. Earlier successful samples used GNU timeout wrappers; uncapped reruns launch tools directly. Package-manager install arguments are unchanged.";
results.methodology.registry = "Local warmed mirror; upstream tarball URLs pinned by original locks can bypass it.";
results.methodology.verification = "Restore pristine source and tool lock, install with scripts disabled, audit declarations and package-set stability, then run the listed direct project check.";
results.methodology.disk = "st_blocks*512, unique device/inode across all node_modules and private PM state before project builds; hardlinks counted once; reflink extents not identified.";

const replacements = new Map<string, string>();
for (const project of Object.values<any>(results.projects)) {
  const fnpm = project.tools.fnpm;
  for (const sample of [fnpm?.migration, fnpm?.verificationInstall, ...Object.values<any>(fnpm?.scenarios ?? {}).flatMap(s => s.samples ?? [])]) {
    if (sample?.command?.[0]) replacements.set(sample.command[0], "<fnpm-release>");
  }
}
for (const path of [root, realpathSync(root)]) replacements.set(path, "<scratch>");
const repository = resolve(dirname(fileURLToPath(import.meta.url)), "..");
replacements.set(repository, "<repo>");
for (const home of [homedir(), realpathSync(homedir())]) {
  replacements.set(home, "~");
  if (repository.startsWith(home + "/")) replacements.set("~" + repository.slice(home.length), "<repo>");
}
function portable(value: any): any {
  if (typeof value === "string") {
    for (const [from, to] of [...replacements].sort((a, b) => b[0].length - a[0].length)) value = value.split(from).join(to);
    // Older samples scrubbed a symlink home before its physical home prefix.
    return value.replace(/\/data\d+~\//g, "~/");
  }
  if (Array.isArray(value)) return value.map(portable);
  if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, portable(item)]));
  return value;
}
const data = portable(results);
const lines = ["# Real-project package manager measurements", "",
  "Three measured runs per completed scenario. Times are median seconds; failed scenarios are not speed wins. A passing check covers only the command listed below.", "",
  `${data.machine.cores} CPU cores, ${(data.machine.memoryBytes / 2 ** 30).toFixed(1)} GiB RAM, Node ${data.machine.node}, ${data.machine.os} ${data.machine.arch}. Immutable git archives; shared development machine; OS and mirror caches remain warm. Timed installs disable lifecycle scripts.`, "",
  "Cold installs use each tool's native lock after untimed preparation, with empty private PM state and no node_modules. Warm installs retain PM state and remove node_modules; no-op retains both. Each tool has private HOME/cache/store. Migration warnings and package-set differences remain in the JSON. npm is covered by the synthetic matrix; these projects use workspace/catalog/Bun formats it cannot install directly.", "",
  "Installs run to completion without a time cap. Earlier interrupted Excalidraw/Yarn and OpenCode/Bun cold attempts are preserved under priorCappedScenarios and rerun without a cap. Other successful samples are retained. Ranges show the shared-machine variation.", "",
  "| Project | Tool/version | Cold lock | Warm relink | No-op | Missing deps | Project check |",
  "|---|---|---:|---:|---:|---:|---|"];
const timing = (scenario: any) => !scenario ? "not run" : !scenario.ok ? (scenario.samples?.some((s: any) => s.timedOut) ? "timeout" : "failed") : scenario.median.toFixed(4);
for (const [name, project] of Object.entries<any>(data.projects)) for (const [tool, result] of Object.entries<any>(project.tools)) {
  const s = result.scenarios ?? {}, missing = result.correctness?.missingDeclaredDependencies;
  const check = result.verify?.ok ? (missing?.length ? "build passed; dependencies missing" : "pass") : result.verify ? "failed" : "not run";
  lines.push(`| ${name} | ${tool} ${result.version ?? "unknown"} | ${timing(s["cold-lockfile"])} | ${timing(s.warm)} | ${timing(s.noop)} | ${missing?.length ?? "not checked"} | ${check} |`);
}
lines.push("", "## Variation and package sets", "", "Ranges are minimum–maximum seconds across successful samples. Failed scenarios can have fewer than three samples.", "",
  "| Project/tool | Cold range | Warm range | No-op range | Unique versions | Instances | Bun versions missing / extra |", "|---|---:|---:|---:|---:|---:|---:|");
for (const [name, project] of Object.entries<any>(data.projects)) for (const [tool, result] of Object.entries<any>(project.tools)) {
  const s = result.scenarios ?? {}, c = result.correctness ?? {}, diff = result.compareBun;
  const span = (key: string) => s[key]?.min == null ? "not completed" : `${s[key].min.toFixed(4)}–${s[key].max.toFixed(4)}`;
  lines.push(`| ${name}/${tool} | ${span("cold-lockfile")} | ${span("warm")} | ${span("noop")} | ${c.packageCount ?? "not checked"} | ${c.instances ?? "not checked"} | ${diff ? `${diff.missing.length} / ${diff.extra.length}` : "not compared"} |`);
}
lines.push("", "## Physical disk usage", "", "MiB, unique device/inode across node_modules plus cache/store, measured before project builds. Hardlinks count once; shared reflink extents are not identified. Inspect package-set differences before comparing.", "",
  "| Project | Tool | node_modules | Extra cache/store | Combined |", "|---|---|---:|---:|---:|");
for (const [name, project] of Object.entries<any>(data.projects)) for (const [tool, result] of Object.entries<any>(project.tools)) {
  const d = result.disk;
  if (d) lines.push(`| ${name} | ${tool} | ${(d.nodeModulesBytes / 2 ** 20).toFixed(2)} | ${(d.storeBytes / 2 ** 20).toFixed(2)} | ${(d.totalBytes / 2 ** 20).toFixed(2)} |`);
}
lines.push("", "## Inputs and verification", "", "| Project | Revision | Workspaces | Verification command |", "|---|---|---:|---|");
for (const [name, project] of Object.entries<any>(data.projects)) {
  const command = Object.values<any>(project.tools).find(r => r.verify?.command)?.verify.command.at(-1) ?? "not executed";
  lines.push(`| ${name} | \`${project.commit}\` | ${project.workspaceCount} | \`${command}\` |`);
}
lines.push("", "## fnpm binary provenance", "", "| Source revision | SHA-256 |", "|---|---|");
const binaries = new Set<string>();
for (const project of Object.values<any>(data.projects)) {
  const f = project.tools.fnpm;
  if (!f?.binarySha256 || binaries.has(f.binarySha256)) continue;
  binaries.add(f.binarySha256);
  lines.push(`| \`${f.sourceRevision ?? "unknown"}\` | \`${f.binarySha256}\` |`);
}
lines.push("", `Final harness SHA-256: \`${data.harnessSha256}\`. ${data.harnessAudit}`, "");
lines.push("", "## Failures and limits", "");
for (const [name, project] of Object.entries<any>(data.projects)) for (const [tool, result] of Object.entries<any>(project.tools)) {
  const notes: string[] = [];
  if (!result.migration?.ok) notes.push("preparation failed or incomplete");
  for (const [scenario, sample] of Object.entries<any>(result.scenarios ?? {})) if (!sample.ok) notes.push(`${scenario}: ${timing(sample)}`);
  if (result.verify && !result.verify.ok) notes.push("project verification failed");
  if (result.correctness?.missingDeclaredDependencies.length) notes.push(`${result.correctness.missingDeclaredDependencies.length} declared dependencies missing`);
  if (result.correctness?.stablePackageSet === false) notes.push("package-version set changed after native-lock reinstall");
  if (result.compareBun && (result.compareBun.missing.length || result.compareBun.extra.length)) notes.push("installed version set differs from Bun; inspect JSON before comparing");
  if (result.correctness?.platformExcludedDependencies?.length) notes.push(`${result.correctness.platformExcludedDependencies.length} declarations target another OS/CPU; explicit lock metadata is retained in JSON and these are excluded from the missing count`);
  if (Object.values<any>(result.priorCappedScenarios ?? {}).some(s => !s.ok)) notes.push("earlier capped cold attempt interrupted; preserved and rerun without a cap");
  if (result.diskInvalidated) notes.push(result.diskInvalidated);
  if (result.diskRemeasured) notes.push(result.diskRemeasured);
  if (notes.length) lines.push(`- ${name}/${tool}: ${notes.map(note => note.replace(/\.+$/, "")).join("; ")}.`);
}
lines.push("", "## Script-enabled OpenNext validation", "");
const l = data.openNextLifecycle;
if (l) {
  lines.push(`Node ${l.node}; source \`${l.sourceRevision}\`. Install: exit ${l.install.exitCode}, ${l.install.seconds.toFixed(2)}s. Original pnpm lock unchanged: ${l.originalLockUnchanged}. Project tsc + Vitest exit: ${l.verification?.exitCode ?? "not run"}. better-sqlite3 SQL smoke exit: ${l.nativeSqlite?.exitCode ?? "not run"}. This is a Linux check; Node 26 on macOS is not covered.`);
  if (l.lifecyclePolicy) lines.push("", l.lifecyclePolicy);
  if (l.verification?.testsPassed) lines.push("", `Vitest: ${l.verification.testFilesPassed} test files / ${l.verification.testsPassed} tests passed; SQL result \`${JSON.stringify(l.nativeSqliteResult)}\`.`);
} else lines.push("Not completed. Timed installs do not validate native lifecycle scripts.");
lines.push("", "Full Bun parity and the all-scenario speed target remain open. Raw samples, package sets, differences, binary hashes, source revisions and evidence hashes are in the companion JSON.", "");
mkdirSync(dirname(output), { recursive: true });
writeFileSync(`${output}.json`, JSON.stringify(data, null, 2) + "\n");
writeFileSync(`${output}.md`, lines.join("\n"));
console.log(`${output}.{json,md}`);
