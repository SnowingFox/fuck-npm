import { mkdir, readFile, rename, stat, writeFile } from "node:fs/promises";
import http from "node:http";
import https from "node:https";
import { dirname, join } from "node:path";
import { brotliDecompressSync, gunzipSync, inflateSync } from "node:zlib";
import { diskFile, type DiskFile } from "./file.ts";
import type { PackageName } from "./names.ts";
import type { StoredPackage } from "./packages.ts";
import { abbreviatedContentType, type Packument } from "./packument.ts";

export interface UplinkOptions {
  /** The upstream registry, for example `https://registry.npmjs.org`. */
  url: string;
  /** Where packuments and tarballs are kept between runs. Without it, the cache is in memory only. */
  cacheDir?: string;
  /**
   * Ask upstream again for a packument that is on disk. The default, false, serves the disk copy forever, which is
   * what a benchmark wants: no network once the cache is warm.
   */
  revalidate?: boolean;
  /** Environment for `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY`. The default is `process.env`. */
  env?: NodeJS.ProcessEnv;
  /** Attempts for one upstream request. The default is 3. */
  attempts?: number;
}

export interface UpstreamResponse {
  status: number;
  headers: http.IncomingHttpHeaders;
  body: Buffer;
}

export type PackumentForm = "abbreviated" | "full";

/** Counts what the uplink did, for tests and for benchmark logs. */
export interface UplinkStats {
  packumentHits: number;
  packumentFetches: number;
  tarballHits: number;
  tarballFetches: number;
  bytesFetched: number;
}

function proxyFor(url: URL, env: NodeJS.ProcessEnv): string | undefined {
  const noProxy = env.NO_PROXY ?? env.no_proxy ?? "";
  const host = url.hostname.toLowerCase();
  for (const raw of noProxy.split(/[\s,]+/)) {
    const entry = raw.trim().toLowerCase().replace(/:\d+$/, "");
    if (entry.length === 0) continue;
    if (entry === "*") return undefined;
    const suffix = entry.startsWith(".") ? entry : `.${entry}`;
    if (host === entry || host.endsWith(suffix)) return undefined;
  }
  const value =
    url.protocol === "https:"
      ? (env.HTTPS_PROXY ?? env.https_proxy ?? env.HTTP_PROXY ?? env.http_proxy)
      : (env.HTTP_PROXY ?? env.http_proxy);
  return value && value.length > 0 ? value : undefined;
}

/**
 * One keep-alive agent per upstream origin. Through a proxy, `https.Agent` gets `proxyEnv` (Node 24.5+), which opens
 * a CONNECT tunnel; without one it connects directly.
 */
function createAgent(url: URL, env: NodeJS.ProcessEnv): http.Agent {
  const proxy = proxyFor(url, env);
  const options: Record<string, unknown> = { keepAlive: true, maxSockets: 64 };
  if (proxy !== undefined) {
    options.proxyEnv = {
      HTTPS_PROXY: proxy,
      HTTP_PROXY: proxy,
      NO_PROXY: env.NO_PROXY ?? env.no_proxy ?? "",
    };
  }
  return url.protocol === "https:" ? new https.Agent(options) : new http.Agent(options);
}

function decode(body: Buffer, encoding: string | undefined): Buffer {
  switch (encoding?.trim().toLowerCase()) {
    case "gzip":
    case "x-gzip":
      return gunzipSync(body);
    case "br":
      return brotliDecompressSync(body);
    case "deflate":
      return inflateSync(body);
    default:
      return body;
  }
}

async function writeAtomic(path: string, data: Uint8Array | string) {
  await mkdir(dirname(path), { recursive: true });
  const temp = `${path}.${process.pid}.${Math.random().toString(36).slice(2)}.tmp`;
  await writeFile(temp, data);
  await rename(temp, path);
}

async function exists(path: string): Promise<boolean> {
  try {
    return (await stat(path)).isFile();
  } catch {
    return false;
  }
}

/** A name as one path segment: `@scope/name` becomes `@scope%2fname`. */
const fileSafe = (name: string) => name.replace("/", "%2f");

/**
 * Proxies packuments and tarballs of an upstream registry and caches them, on disk when `cacheDir` is set. A
 * packument is cached per form (abbreviated and full are separate upstream documents); tarballs are immutable and
 * cached by name and file name. Concurrent requests for the same resource share one upstream request.
 */
export class Uplink {
  readonly url: URL;
  readonly options: UplinkOptions;
  readonly stats: UplinkStats = { packumentHits: 0, packumentFetches: 0, tarballHits: 0, tarballFetches: 0, bytesFetched: 0 };
  readonly #agent: http.Agent;
  readonly #packuments = new Map<string, Promise<Packument | null>>();
  readonly #tarballs = new Map<string, Promise<DiskFile | Buffer | null>>();

  constructor(options: UplinkOptions) {
    this.options = options;
    this.url = new URL(options.url.endsWith("/") ? options.url : `${options.url}/`);
    this.#agent = createAgent(this.url, options.env ?? process.env);
  }

  close() {
    this.#agent.destroy();
  }

  /** GET against upstream with retries for network errors, 429 and 5xx. */
  async request(path: string, headers: Record<string, string> = {}): Promise<UpstreamResponse> {
    const target = new URL(path, this.url);
    const attempts = this.options.attempts ?? 3;
    let lastError: unknown;
    for (let attempt = 1; attempt <= attempts; attempt++) {
      try {
        const response = await this.#once(target, headers);
        if ((response.status >= 500 || response.status === 429) && attempt < attempts) {
          lastError = new Error(`${target.href}: HTTP ${response.status}`);
        } else {
          return response;
        }
      } catch (error) {
        lastError = error;
      }
      await new Promise(resolve => setTimeout(resolve, 100 * 2 ** (attempt - 1)));
    }
    throw lastError;
  }

  #once(target: URL, headers: Record<string, string>): Promise<UpstreamResponse> {
    const client = target.protocol === "https:" ? https : http;
    return new Promise((resolve, reject) => {
      const request = client.request(
        target,
        {
          method: "GET",
          agent: this.#agent,
          headers: { "accept-encoding": "gzip, br", "user-agent": "fnpm-test-registry", ...headers },
          timeout: 60_000,
        },
        response => {
          const chunks: Buffer[] = [];
          response.on("data", chunk => chunks.push(chunk));
          response.on("error", reject);
          response.on("end", () => {
            try {
              const raw = Buffer.concat(chunks);
              this.stats.bytesFetched += raw.byteLength;
              const encoding = response.headers["content-encoding"];
              resolve({
                status: response.statusCode ?? 0,
                headers: response.headers,
                body: decode(raw, typeof encoding === "string" ? encoding : undefined),
              });
            } catch (error) {
              reject(error);
            }
          });
        },
      );
      request.on("timeout", () => request.destroy(new Error(`${target.href}: timed out`)));
      request.on("error", reject);
      request.end();
    });
  }

  #packumentPath(name: string, form: PackumentForm): string | undefined {
    const dir = this.options.cacheDir;
    return dir === undefined ? undefined : join(dir, "packuments", form, `${fileSafe(name)}.json`);
  }

  /** The upstream packument of `name` in one form, or null when upstream does not have it. */
  packument(name: string, form: PackumentForm): Promise<Packument | null> {
    const key = `${form}\0${name}`;
    let pending = this.#packuments.get(key);
    if (pending === undefined) {
      pending = this.#loadPackument(name, form);
      this.#packuments.set(key, pending);
      pending.catch(() => this.#packuments.delete(key));
    }
    return pending;
  }

  async #loadPackument(name: string, form: PackumentForm): Promise<Packument | null> {
    const path = this.#packumentPath(name, form);
    let cachedEtag: string | undefined;
    let cached: Packument | undefined;
    if (path !== undefined && (await exists(path))) {
      const text = await readFile(path, "utf8");
      cached = JSON.parse(text);
      if (!this.options.revalidate) {
        this.stats.packumentHits++;
        return cached!;
      }
      cachedEtag = await readFile(`${path}.etag`, "utf8").catch(() => undefined);
    }
    this.stats.packumentFetches++;
    const headers: Record<string, string> = {
      accept: form === "abbreviated" ? `${abbreviatedContentType}; q=1.0, application/json; q=0.8, */*` : "application/json",
    };
    if (cachedEtag) headers["if-none-match"] = cachedEtag;
    const response = await this.request(fileSafe(name), headers);
    if (response.status === 304 && cached) return cached;
    if (response.status === 404) return null;
    if (response.status !== 200) {
      throw new Error(`uplink ${name}: HTTP ${response.status} ${response.body.toString("utf8").slice(0, 200)}`);
    }
    const document = JSON.parse(response.body.toString("utf8")) as Packument;
    if (path !== undefined) {
      await writeAtomic(path, response.body);
      const etag = response.headers.etag;
      if (typeof etag === "string") await writeAtomic(`${path}.etag`, etag);
    }
    return document;
  }

  /** A StoredPackage for the registry to render. Tarball URLs are rewritten to the local registry when rendered. */
  async stored(name: PackageName, form: PackumentForm): Promise<StoredPackage | null> {
    const document = await this.packument(name.name, form);
    if (document === null) return null;
    return {
      name,
      document,
      access: "public",
      tarballs: new Map(),
      unpublished: new Set(),
      modified: new Date(typeof document.modified === "string" ? document.modified : 0),
      rendered: new Map(),
      tarballHashes: new Map(),
    };
  }

  /** The bytes of `<name>/-/<filename>` from upstream, from the cache when it is there. */
  tarball(name: string, filename: string): Promise<DiskFile | Buffer | null> {
    const key = `${name}\0${filename}`;
    let pending = this.#tarballs.get(key);
    if (pending === undefined) {
      pending = this.#loadTarball(name, filename);
      this.#tarballs.set(key, pending);
      pending.then(
        result => {
          // Keep only on-disk results in memory; a Buffer is dropped so memory stays flat.
          if (result === null || Buffer.isBuffer(result)) this.#tarballs.delete(key);
        },
        () => this.#tarballs.delete(key),
      );
    }
    return pending;
  }

  async #loadTarball(name: string, filename: string): Promise<DiskFile | Buffer | null> {
    const dir = this.options.cacheDir;
    const path = dir === undefined ? undefined : join(dir, "tarballs", fileSafe(name), filename);
    if (path !== undefined && (await exists(path))) {
      this.stats.tarballHits++;
      return diskFile(path);
    }
    this.stats.tarballFetches++;
    const response = await this.request(`${name}/-/${filename}`, { accept: "*/*", "accept-encoding": "identity" });
    if (response.status === 404) return null;
    if (response.status !== 200) throw new Error(`uplink ${name}/-/${filename}: HTTP ${response.status}`);
    if (path === undefined) return response.body;
    await writeAtomic(path, response.body);
    return diskFile(path);
  }
}
