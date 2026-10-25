// Shared helpers for the real-world e2e corpus: corpus loading, checkout, and the
// node_modules snapshot that baselines (and later fnpm runs) are compared on.
import { spawnSync } from "node:child_process";
import { existsSync, lstatSync, mkdirSync, readFileSync, readdirSync, realpathSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

export const E2E_DIR = dirname(fileURLToPath(import.meta.url));
export const REPO_ROOT = resolve(E2E_DIR, "..", "..");
export const CORPUS_FILE = join(E2E_DIR, "corpus.json");
export const DEFAULT_MIRROR = "http://127.0.0.1:4873/";

/** Where project checkouts live. Never inside the repo. */
export function workRoot(): string {
  return process.env.FNPM_E2E_DIR || join(homedir(), ".cache", "fnpm-e2e");
}

export type Pm = "npm" | "yarn-classic" | "yarn-berry" | "pnpm" | "bun";

export interface CorpusEntry {
  name: string;
  repo: string;
  commit: string;
  pm: Pm;
  /** Version pinned by the project (packageManager field / yarnPath), if any. */
  pmPinned?: string;
  lockfile: string;
  lockfileFormat: string;
  workspace: boolean;
  features: string[];
  install: string;
  /** Extra env for install and verify (e.g. YARN_NODE_LINKER for the PnP migration case). */
  env?: Record<string, string>;
  verify: string;
  verifyTimeoutSec?: number;
  baseline: string;
  notes?: string;
  /** fnpm side (tests/e2e/fnpm.ts). */
  fnpm?: {
    /** Verify command when the project's own one cannot run on a tree it did not install. */
    verify?: string;
    /** A documented genuine incompatibility, restricted to knownFailures signatures. */
    known?: string;
    knownFailures?: string[];
    /** Regexes of accepted baseline differences, each with its reason in `notes`. */
    allow?: { missing?: string[]; extra?: string[]; declared?: string[]; bins?: string[]; scripts?: string[] };
  };
}

export function loadCorpus(): CorpusEntry[] {
  return JSON.parse(readFileSync(CORPUS_FILE, "utf8")).projects as CorpusEntry[];
}

export interface Sh {
  status: number | null;
  stdout: string;
  stderr: string;
  ms: number;
  timedOut: boolean;
}

const HAS_TIMEOUT = process.platform === "linux" && spawnSync("timeout", ["--version"]).status === 0;

export function sh(cmd: string, cwd: string, env: NodeJS.ProcessEnv = process.env, timeoutSec = 1800, logFile?: string): Sh {
  const t0 = performance.now();
  const body = logFile ? `(${cmd}) >>${shq(logFile)} 2>&1` : cmd;
  // GNU timeout signals the whole process group, so test runners' workers do not outlive a
  // timed-out step (spawnSync's own timeout only kills bash).
  const full = HAS_TIMEOUT ? `timeout -k 10 ${timeoutSec} bash -c ${shq(body)}` : body;
  const r = spawnSync("bash", ["-c", full], { cwd, env, encoding: "utf8", timeout: (timeoutSec + 30) * 1000, maxBuffer: 256 << 20 });
  const timedOut = (r.error as NodeJS.ErrnoException | undefined)?.code === "ETIMEDOUT" || (HAS_TIMEOUT && (r.status === 124 || r.status === 137));
  return { status: r.status, stdout: r.stdout ?? "", stderr: r.stderr ?? "", ms: Math.round(performance.now() - t0), timedOut };
}

export function shq(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

/** Shallow checkout of exactly `commit`, reset to a pristine tree (untracked + ignored files removed). */
export function checkout(entry: CorpusEntry, dir: string): void {
  const git = (args: string) => {
    const r = sh(`git ${args}`, dir);
    if (r.status !== 0) throw new Error(`git ${args} failed in ${dir}:\n${r.stderr}`);
    return r.stdout.trim();
  };
  if (!existsSync(join(dir, ".git"))) {
    mkdirSync(dir, { recursive: true });
    git("init -q");
  }
  const head = sh("git rev-parse HEAD", dir).stdout.trim();
  if (head !== entry.commit) {
    sh("git remote remove origin", dir);
    git(`remote add origin https://github.com/${entry.repo}.git`);
    git(`fetch -q --depth 1 origin ${entry.commit}`);
    git(`checkout -q --force ${entry.commit}`);
  }
  git("reset -q --hard");
  git("clean -q -ffdx");
}

// ---------------------------------------------------------------------------------------------
// Workspace discovery

function readJson(path: string): any {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return undefined;
  }
}

function workspacePatterns(root: string): string[] {
  const pkg = readJson(join(root, "package.json")) ?? {};
  const pats: string[] = [];
  const ws = pkg.workspaces;
  if (Array.isArray(ws)) pats.push(...ws);
  else if (ws && Array.isArray(ws.packages)) pats.push(...ws.packages);
  const pnpmWs = join(root, "pnpm-workspace.yaml");
  if (existsSync(pnpmWs)) {
    // Minimal YAML: the `packages:` block sequence of (optionally quoted) scalars.
    let inPackages = false;
    for (const line of readFileSync(pnpmWs, "utf8").split("\n")) {
      if (/^packages:\s*$/.test(line)) {
        inPackages = true;
        continue;
      }
      if (inPackages) {
        const m = /^\s+-\s+['"]?([^'"#]+?)['"]?\s*(#.*)?$/.exec(line);
        if (m) pats.push(m[1]);
        else if (/^\S/.test(line)) inPackages = false;
      }
    }
  }
  return pats;
}

function globDirs(root: string, pattern: string): string[] {
  const segs = pattern.replace(/^\.\//, "").replace(/\/$/, "").split("/");
  const out: string[] = [];
  const walk = (dir: string, i: number) => {
    if (i === segs.length) {
      if (existsSync(join(dir, "package.json"))) out.push(dir);
      return;
    }
    const seg = segs[i];
    if (seg === "**") {
      walk(dir, i + 1);
      for (const d of subdirs(dir)) walk(join(dir, d), i);
      return;
    }
    if (!seg.includes("*")) {
      const next = join(dir, seg);
      if (existsSync(next)) walk(next, i + 1);
      return;
    }
    const re = new RegExp(`^${seg.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`);
    for (const d of subdirs(dir)) if (re.test(d)) walk(join(dir, d), i + 1);
  };
  walk(root, 0);
  return out;
}

function subdirs(dir: string): string[] {
  try {
    return readdirSync(dir, { withFileTypes: true })
      .filter((e) => e.isDirectory() && e.name !== "node_modules" && !e.name.startsWith("."))
      .map((e) => e.name);
  } catch {
    return [];
  }
}

/** Workspace package dirs relative to root ("." first). */
export function importers(root: string): string[] {
  const inc = new Set<string>();
  const exc = new Set<string>();
  for (const p of workspacePatterns(root)) {
    const neg = p.startsWith("!");
    for (const d of globDirs(root, neg ? p.slice(1) : p)) (neg ? exc : inc).add(relative(root, d) || ".");
  }
  return [".", ...[...inc].filter((d) => !exc.has(d) && d !== ".").sort()];
}

// ---------------------------------------------------------------------------------------------
// node_modules snapshot

export interface Snapshot {
  /** Unique name@version across the whole install (path independent). */
  packages: string[];
  /** Number of physical package directories (after realpath dedupe). */
  instances: number;
  /** Per importer: direct entries of <importer>/node_modules → version, "name@version" for aliases, "link:<path>" for links. */
  importers: Record<string, Record<string, string>>;
  /** Per importer: names in <importer>/node_modules/.bin. */
  bins: Record<string, string[]>;
  /** name@version of packages that declare preinstall/install/postinstall (or an implicit node-gyp build). */
  installScripts: string[];
}

function isDir(p: string): boolean {
  try {
    return statSync(p).isDirectory();
  } catch {
    return false;
  }
}

function insideNodeModules(real: string): boolean {
  return real.split(sep).includes("node_modules");
}

export function snapshot(root: string): Snapshot {
  const rootReal = realpathSync(root);
  const visited = new Set<string>();
  const pkgs = new Set<string>();
  const scripts = new Set<string>();
  let instances = 0;

  const visitPkg = (dir: string) => {
    let real: string;
    try {
      real = realpathSync(dir);
    } catch {
      return; // dangling symlink
    }
    if (!insideNodeModules(real) || visited.has(real)) return;
    visited.add(real);
    const pkg = readJson(join(real, "package.json"));
    if (pkg && typeof pkg.name === "string") {
      const id = `${pkg.name}@${pkg.version ?? ""}`;
      pkgs.add(id);
      instances++;
      const s = pkg.scripts ?? {};
      if (s.preinstall || s.install || s.postinstall || (!s.install && !s.preinstall && existsSync(join(real, "binding.gyp")))) scripts.add(id);
    }
    const nm = join(real, "node_modules");
    if (isDir(nm)) walkNm(nm);
  };

  const walkNm = (nm: string) => {
    let entries: string[];
    try {
      entries = readdirSync(nm);
    } catch {
      return;
    }
    for (const e of entries) {
      if (e === ".bin") continue;
      if (e === ".pnpm" || e === ".bun" || e === ".fnpm") {
        // Isolated-linker stores: <store>/<id>/node_modules/<pkg>.
        let ids: string[] = [];
        try {
          ids = readdirSync(join(nm, e));
        } catch {}
        for (const id of ids) {
          const inner = join(nm, e, id, "node_modules");
          if (id !== "node_modules" && isDir(inner)) walkNm(inner);
        }
        continue;
      }
      if (e.startsWith(".")) continue;
      const p = join(nm, e);
      if (e.startsWith("@")) {
        if (!isDir(p)) continue;
        for (const s of readdirSync(p)) visitPkg(join(p, s));
      } else visitPkg(p);
    }
  };

  const imps: Record<string, Record<string, string>> = {};
  const bins: Record<string, string[]> = {};
  for (const imp of importers(root)) {
    const nm = join(root, imp, "node_modules");
    if (!isDir(nm)) continue;
    walkNm(nm);
    const direct: Record<string, string> = {};
    const names: string[] = [];
    for (const e of readdirSync(nm)) {
      if (e.startsWith(".")) continue;
      if (e.startsWith("@") && isDir(join(nm, e))) for (const s of readdirSync(join(nm, e))) names.push(`${e}/${s}`);
      else names.push(e);
    }
    for (const n of names.sort()) {
      const p = join(nm, n);
      let real: string;
      try {
        real = realpathSync(p);
      } catch {
        direct[n] = "dangling";
        continue;
      }
      if (!insideNodeModules(real)) {
        direct[n] = `link:${relative(rootReal, real) || "."}`;
        continue;
      }
      const pkg = readJson(join(real, "package.json"));
      if (!pkg) continue;
      direct[n] = pkg.name === n ? `${pkg.version ?? ""}` : `${pkg.name}@${pkg.version ?? ""}`;
    }
    if (Object.keys(direct).length) imps[imp] = direct;
    const binDir = join(nm, ".bin");
    if (isDir(binDir)) {
      const b = readdirSync(binDir)
        .filter((f) => !/\.(cmd|ps1)$/.test(f))
        .sort();
      if (b.length) bins[imp] = b;
    }
  }
  return { packages: [...pkgs].sort(), instances, importers: imps, bins, installScripts: [...scripts].sort() };
}

/** Parse the script-logger output into sorted unique "<name>@<version> <event>" (or "<cwd> <event>"). */
export function readScriptLog(file: string, root: string): string[] {
  if (!existsSync(file)) return [];
  const out = new Set<string>();
  const rootReal = realpathSync(root);
  for (const line of readFileSync(file, "utf8").split("\n")) {
    if (!line) continue;
    try {
      const r = JSON.parse(line);
      let cwdReal = r.cwd as string;
      try {
        cwdReal = realpathSync(r.cwd);
      } catch {}
      const rel = relative(rootReal, cwdReal);
      const where = insideNodeModules(cwdReal) || rel.startsWith("..") ? "" : ` (${rel || "."})`;
      out.add(`${r.name ?? "?"}@${r.version ?? ""} ${r.event}${where}`);
    } catch {}
  }
  return [...out].sort();
}

export function isSymlink(p: string): boolean {
  try {
    return lstatSync(p).isSymbolicLink();
  } catch {
    return false;
  }
}
