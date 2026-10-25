// Disk usage measurement.
//
// Physical bytes = st_blocks * 512, summed over unique (dev, ino) pairs so hardlinks between
// node_modules and a store (pnpm, bun, fnpm) are counted once. Symlinks are counted as their
// own inode and never followed. Reflinks/clones can't be detected via stat; on filesystems
// that support them use the statfs free-space delta instead.
import { lstatSync, readdirSync, statfsSync } from "node:fs";
import { join } from "node:path";

export interface TreeUsage {
  /** Physical bytes of inodes first seen in this tree. */
  bytes: number;
  /** Directory entries (files, dirs, symlinks) visited, including hardlink duplicates. */
  entries: number;
  /** Unique inodes first seen in this tree. */
  inodes: number;
}

export class InodeSet {
  seen = new Set<string>();
}

/** Walk `root` with lstat. Inodes already in `seen` are not counted again. */
export function walk(root: string, seen: InodeSet): TreeUsage {
  const usage: TreeUsage = { bytes: 0, entries: 0, inodes: 0 };
  const stack = [root];
  while (stack.length) {
    const path = stack.pop()!;
    let st;
    try {
      st = lstatSync(path, { bigint: true });
    } catch {
      continue;
    }
    usage.entries++;
    const key = `${st.dev}:${st.ino}`;
    if (!seen.seen.has(key)) {
      seen.seen.add(key);
      usage.inodes++;
      usage.bytes += Number(st.blocks) * 512;
    }
    if (st.isDirectory()) {
      let names: string[];
      try {
        names = readdirSync(path);
      } catch {
        continue;
      }
      for (const n of names) stack.push(join(path, n));
    }
  }
  return usage;
}

/** Every node_modules directory under `root` (not descending into node_modules). */
export function findNodeModules(root: string): string[] {
  const out: string[] = [];
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop()!;
    let ents;
    try {
      ents = readdirSync(dir, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const e of ents) {
      if (!e.isDirectory()) continue;
      const p = join(dir, e.name);
      if (e.name === "node_modules") out.push(p);
      else if (!e.name.startsWith(".")) stack.push(p);
    }
  }
  return out;
}

export interface DiskReport {
  /** Physical bytes of all node_modules trees (hardlink-deduped). */
  nodeModulesBytes: number;
  /** Physical bytes in cache/store not already counted in node_modules. */
  storeBytes: number;
  totalBytes: number;
  /** Entries in node_modules (what a tool has to create on every install). */
  nodeModulesEntries: number;
  /** Unique inodes across node_modules + store. */
  inodes: number;
  storeEntries: number;
}

export function measure(work: string, cacheDirs: string[]): DiskReport {
  const seen = new InodeSet();
  let nm = { bytes: 0, entries: 0, inodes: 0 };
  for (const dir of findNodeModules(work)) {
    const u = walk(dir, seen);
    nm = { bytes: nm.bytes + u.bytes, entries: nm.entries + u.entries, inodes: nm.inodes + u.inodes };
  }
  let store = { bytes: 0, entries: 0, inodes: 0 };
  for (const dir of cacheDirs) {
    const u = walk(dir, seen);
    store = { bytes: store.bytes + u.bytes, entries: store.entries + u.entries, inodes: store.inodes + u.inodes };
  }
  return {
    nodeModulesBytes: nm.bytes,
    storeBytes: store.bytes,
    totalBytes: nm.bytes + store.bytes,
    nodeModulesEntries: nm.entries,
    storeEntries: store.entries,
    inodes: nm.inodes + store.inodes,
  };
}

/** Bytes currently used on the filesystem holding `path` (total - free). */
export function fsUsedBytes(path: string): number {
  const s = statfsSync(path, { bigint: true });
  return Number((s.blocks - s.bfree) * s.bsize);
}
