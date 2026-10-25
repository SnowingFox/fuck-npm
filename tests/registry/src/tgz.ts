import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";
import type { Packument, VersionDocument } from "./packument.ts";

/** One entry of a tar archive. Only regular files are returned. */
export interface TarEntry {
  path: string;
  mode: number;
  data: Buffer;
}

function readString(block: Buffer, offset: number, length: number): string {
  const end = block.indexOf(0, offset);
  return block.toString("utf8", offset, end === -1 || end > offset + length ? offset + length : end);
}

function readOctal(block: Buffer, offset: number, length: number): number {
  const text = readString(block, offset, length).trim();
  return text.length === 0 ? 0 : Number.parseInt(text, 8);
}

/** A small ustar/pax reader: enough for npm tarballs (regular files, long names via pax `path` or GNU `L`). */
export function readTar(tar: Buffer): TarEntry[] {
  const entries: TarEntry[] = [];
  let offset = 0;
  let longName: string | null = null;
  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;
    let name = readString(header, 0, 100);
    const mode = readOctal(header, 100, 8);
    const size = readOctal(header, 124, 12);
    const type = String.fromCharCode(header[156] || 48);
    const magic = readString(header, 257, 6);
    if (magic.startsWith("ustar")) {
      const prefix = readString(header, 345, 155);
      if (prefix.length > 0) name = `${prefix}/${name}`;
    }
    const dataStart = offset + 512;
    const data = tar.subarray(dataStart, dataStart + size);
    offset = dataStart + Math.ceil(size / 512) * 512;
    if (type === "x" || type === "g") {
      for (const record of data.toString("utf8").split("\n")) {
        const match = record.match(/^\d+ path=(.*)$/);
        if (match && type === "x") longName = match[1];
      }
      continue;
    }
    if (type === "L") {
      longName = data.toString("utf8").replace(/\0+$/, "");
      continue;
    }
    if (longName !== null) {
      name = longName;
      longName = null;
    }
    if (type === "0" || type === "\0" || type === "7") entries.push({ path: name, mode, data: Buffer.from(data) });
  }
  return entries;
}

/** The `package.json` at the top folder of an npm tarball (`package/package.json`, or another folder name). */
export function readTarballManifest(tgz: Buffer): Record<string, unknown> | null {
  const entries = readTar(gunzipSync(tgz));
  const candidates = entries.filter(entry => /^[^/]+\/package\.json$/.test(entry.path.replace(/^\.\//, "")));
  const entry = candidates.find(candidate => candidate.path.startsWith("package/")) ?? candidates[0];
  if (!entry) return null;
  try {
    return JSON.parse(entry.data.toString("utf8"));
  } catch {
    return null;
  }
}

export function shasumOf(bytes: Uint8Array): string {
  return createHash("sha1").update(bytes).digest("hex");
}

export function integrityOf(bytes: Uint8Array, algorithm: "sha512" | "sha256" | "sha1" = "sha512"): string {
  return `${algorithm}-${createHash(algorithm).update(bytes).digest("base64")}`;
}

/** Packuments generated from loose `*.tgz` files, with the path of each tarball by its file name. */
export interface TarballPackage {
  document: Packument;
  /** Tarball file name, as it appears at the end of `dist.tarball`, to the path on disk. */
  files: Map<string, string>;
}

/**
 * Reads each `*.tgz` of a directory (not recursive) and builds one packument per package name from the
 * `package.json` inside, with `dist.shasum` and `dist.integrity` computed from the bytes. A version that two files
 * claim keeps the first file in name order. `latest` is the highest version.
 */
export function packumentsFromTarballs(
  directory: string,
  compare: (a: string, b: string) => number,
): Map<string, TarballPackage> {
  const packages = new Map<string, TarballPackage>();
  const files = readdirSync(directory)
    .filter(file => file.endsWith(".tgz"))
    .sort();
  for (const file of files) {
    const path = join(directory, file);
    const bytes = readFileSync(path);
    let manifest: Record<string, unknown> | null;
    try {
      manifest = readTarballManifest(bytes);
    } catch {
      continue;
    }
    if (!manifest || typeof manifest.name !== "string" || typeof manifest.version !== "string") continue;
    const { name, version } = manifest as { name: string; version: string };
    let entry = packages.get(name);
    if (!entry) {
      entry = { document: { _id: name, name, "dist-tags": {}, versions: {}, time: {} }, files: new Map() };
      packages.set(name, entry);
    }
    if (Object.hasOwn(entry.document.versions, version)) continue;
    const { readme: _readme, ...rest } = manifest;
    const doc: VersionDocument = {
      ...(rest as VersionDocument),
      _id: `${name}@${version}`,
      dist: { shasum: shasumOf(bytes), integrity: integrityOf(bytes), tarball: `${name}/-/${file}` },
    };
    entry.document.versions[version] = doc;
    entry.files.set(file, path);
  }
  for (const { document } of packages.values()) {
    const versions = Object.keys(document.versions).sort(compare);
    document["dist-tags"].latest = versions.at(-1)!;
  }
  return packages;
}
