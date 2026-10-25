import { readFileSync, statSync, type Stats } from "node:fs";
import { readFile, stat } from "node:fs/promises";
import { basename } from "node:path";

/**
 * A lazily read file with the part of the `Blob` interface the registry uses (`size`, `bytes()`), plus
 * `exists()`, `json()`, `name` and `lastModified` like `Bun.file`. Nothing is read until it is asked for.
 */
export class DiskFile {
  readonly name: string;
  #stats: Stats | undefined | null = null;

  constructor(path: string) {
    this.name = path;
  }

  #stat(): Stats | undefined {
    if (this.#stats === null) this.#stats = statSync(this.name, { throwIfNoEntry: false });
    return this.#stats;
  }

  get size(): number {
    return this.#stat()?.size ?? 0;
  }

  get lastModified(): number {
    return this.#stat()?.mtimeMs ?? Date.now();
  }

  get type(): string {
    return this.name.endsWith(".json") ? "application/json" : "application/octet-stream";
  }

  async exists(): Promise<boolean> {
    try {
      this.#stats = await stat(this.name);
      return this.#stats.isFile();
    } catch {
      this.#stats = undefined;
      return false;
    }
  }

  async bytes(): Promise<Uint8Array<ArrayBuffer>> {
    const buffer = await readFile(this.name);
    return new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength) as Uint8Array<ArrayBuffer>;
  }

  async arrayBuffer(): Promise<ArrayBuffer> {
    const bytes = await this.bytes();
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  }

  async text(): Promise<string> {
    return readFile(this.name, "utf8");
  }

  async json(): Promise<unknown> {
    return JSON.parse(await this.text());
  }

  bytesSync(): Buffer {
    return readFileSync(this.name);
  }

  toString() {
    return basename(this.name);
  }
}

/** `Bun.file` for the registry. Typed as a Blob because `StoredPackage.tarballs` holds Blobs and DiskFiles. */
export function diskFile(path: string): DiskFile & Blob {
  return new DiskFile(path) as DiskFile & Blob;
}
