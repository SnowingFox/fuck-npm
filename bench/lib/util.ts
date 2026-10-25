// Shared helpers for the benchmark harness. Runs under Node >= 22 with type stripping
// (only erasable TypeScript syntax is used: no enums, namespaces or parameter properties).
import { spawnSync, type SpawnSyncOptions } from "node:child_process";
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const BENCH_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const REPO_ROOT = resolve(BENCH_DIR, "..");
export const TMP_DIR = join(BENCH_DIR, ".tmp");
export const FIXTURES_DIR = join(BENCH_DIR, "fixtures");
export const RESULTS_DIR = join(BENCH_DIR, "results");

export const DEFAULT_REGISTRY = "http://127.0.0.1:4873/";

/** Registry URL every tool is forced to use. Always ends with a slash. */
export function registryUrl(): string {
  const url = process.env.BENCH_REGISTRY || DEFAULT_REGISTRY;
  return url.endsWith("/") ? url : `${url}/`;
}

export function ensureDir(path: string): string {
  mkdirSync(path, { recursive: true });
  return path;
}

export function rmrf(path: string): void {
  if (existsSync(path)) rmSync(path, { recursive: true, force: true });
}

export interface RunResult {
  status: number;
  stdout: string;
  stderr: string;
}

/** Run argv synchronously (no shell). Never throws on non-zero exit. */
export function run(argv: string[], opts: SpawnSyncOptions = {}): RunResult {
  const r = spawnSync(argv[0], argv.slice(1), {
    encoding: "utf8",
    maxBuffer: 256 * 1024 * 1024,
    ...opts,
  });
  return {
    status: r.status ?? (r.error ? 127 : 1),
    stdout: String(r.stdout ?? ""),
    stderr: String(r.stderr ?? (r.error ? r.error.message : "")),
  };
}

/** Run argv and throw with captured output if it fails. */
export function runOk(argv: string[], opts: SpawnSyncOptions = {}): RunResult {
  const r = run(argv, opts);
  if (r.status !== 0) {
    const tail = (r.stderr || r.stdout).split("\n").slice(-30).join("\n");
    throw new Error(`command failed (${r.status}): ${argv.join(" ")}\n${tail}`);
  }
  return r;
}

/** Absolute path of an executable on PATH, or null. */
export function which(bin: string): string | null {
  const r = run(["sh", "-c", 'command -v "$0"', bin]);
  const out = r.stdout.trim();
  return r.status === 0 && out ? out : null;
}

/** POSIX shell quoting for a single word. */
export function shq(word: string): string {
  return /^[A-Za-z0-9_\/.,:=@%+-]+$/.test(word) ? word : `'${word.replace(/'/g, `'\\''`)}'`;
}

export function shjoin(argv: string[]): string {
  return argv.map(shq).join(" ");
}

export function log(msg: string): void {
  process.stderr.write(`[bench] ${msg}\n`);
}

export function parseList(value: string | undefined, all: readonly string[]): string[] {
  if (!value || value === "all") return [...all];
  const out = value.split(",").map((s) => s.trim()).filter(Boolean);
  for (const v of out) {
    if (!all.includes(v)) throw new Error(`unknown value "${v}" (expected one of: ${all.join(", ")})`);
  }
  return out;
}

export function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/** Resolves true if the registry answers /-/ping within the timeout. */
export async function registryAlive(url: string, timeoutMs = 2000): Promise<boolean> {
  try {
    const res = await fetch(new URL("-/ping", url), { signal: AbortSignal.timeout(timeoutMs) });
    return res.ok;
  } catch {
    return false;
  }
}

/** True when the module at `url` is the entry point (works on Node 22, unlike import.meta.main). */
export function isMain(url: string): boolean {
  return !!process.argv[1] && resolve(process.argv[1]) === fileURLToPath(url);
}
