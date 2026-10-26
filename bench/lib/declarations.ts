// Check importer declarations while recording explicit foreign-platform exclusions.
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { importers } from "../../tests/e2e/lib.ts";

function accepts(value: string | string[] | undefined, current: string): boolean {
  const values = value === undefined ? [] : Array.isArray(value) ? value : [value];
  if (values.includes(`!${current}`)) return false;
  const positive = values.filter(v => !v.startsWith("!"));
  return positive.length === 0 || positive.includes(current) || positive.includes("any");
}

export function declaredDependencies(work: string) {
  const metadata = new Map<string, { os?: string | string[]; cpu?: string | string[] }>();
  const lock = ["fnpm.lock", "bun.lock"].find(name => existsSync(join(work, name)));
  if (lock) for (const line of readFileSync(join(work, lock), "utf8").split("\n")) {
    // Package tuples occupy one line in Bun/fnpm text locks. Unknown rows remain missing.
    const row = /^\s*("(?:[^"\\]|\\.)*"):\s*(\[.*\]),?\s*$/.exec(line);
    if (!row) continue;
    try {
      const tuple = JSON.parse(row[2]);
      if (tuple[2] && typeof tuple[2] === "object") metadata.set(JSON.parse(row[1]), tuple[2]);
    } catch {}
  }
  const missing: string[] = [];
  const platformExcluded: { dependency: string; os?: string | string[]; cpu?: string | string[] }[] = [];
  for (const imp of importers(work)) {
    const pkg = JSON.parse(readFileSync(join(work, imp, "package.json"), "utf8"));
    for (const name of Object.keys({ ...pkg.dependencies, ...pkg.devDependencies })) {
      let at = join(work, imp), found = false;
      while (at === work || at.startsWith(work + "/")) {
        if (existsSync(join(at, "node_modules", name, "package.json"))) { found = true; break; }
        if (at === work) break;
        at = dirname(at);
      }
      if (found) continue;
      const meta = metadata.get(name);
      const dependency = `${imp}:${name}`;
      if (meta && (!accepts(meta.os, process.platform) || !accepts(meta.cpu, process.arch))) {
        platformExcluded.push({ dependency, os: meta.os, cpu: meta.cpu });
      } else missing.push(dependency);
    }
  }
  return { missing, platformExcluded };
}
