import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { abbreviatedContentType, bunInstallTestDir, packumentsFromTarballs, readTarballManifest, start, type RunningRegistry } from "../index.ts";
import { compareVersions } from "../src/names.ts";

const sha1 = (bytes: Uint8Array) => createHash("sha1").update(bytes).digest("hex");
const sha512 = (bytes: Uint8Array) => `sha512-${createHash("sha512").update(bytes).digest("base64")}`;
const temp = (name: string) => mkdtempSync(join(realpathSync(tmpdir()), `fnpm-registry-${name}-`));

let registry: RunningRegistry;
beforeAll(async () => {
  registry = await start({ port: 0 });
});
afterAll(async () => {
  await registry.stop();
});

describe("packuments", () => {
  test("full packument with tarball URLs rewritten to the local registry", async () => {
    const response = await fetch(`${registry.url}no-deps`);
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("application/json");
    const doc = await response.json();
    expect(doc.name).toBe("no-deps");
    expect(doc["dist-tags"].latest).toBe("2.0.0");
    expect(Object.keys(doc.versions).sort()).toEqual(["1.0.0", "1.0.1", "1.1.0", "2.0.0"]);
    expect(doc.versions["1.0.0"].dist.tarball).toBe(`${registry.url}no-deps/-/no-deps-1.0.0.tgz`);
    expect(doc._attachments).toBeUndefined();
    expect(doc.time).toBeDefined();
  });

  test("abbreviated (corgi) packument for Accept: application/vnd.npm.install-v1+json", async () => {
    const response = await fetch(`${registry.url}no-deps`, {
      headers: { accept: `${abbreviatedContentType}; q=1.0, application/json; q=0.8, */*` },
    });
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe(abbreviatedContentType);
    const doc = await response.json();
    expect(Object.keys(doc).sort()).toEqual(["dist-tags", "modified", "name", "versions"]);
    expect(doc.versions["2.0.0"]._id).toBeUndefined();
    expect(doc.versions["2.0.0"].dist.tarball).toBe(`${registry.url}no-deps/-/no-deps-2.0.0.tgz`);
  });

  test("hasInstallScript is set in the abbreviated document", async () => {
    const doc = await (
      await fetch(`${registry.url}all-lifecycle-scripts`, { headers: { accept: abbreviatedContentType } })
    ).json();
    expect(Object.values(doc.versions).some((v: any) => v.hasInstallScript === true)).toBe(true);
  });

  test("scoped names, encoded and not", async () => {
    for (const path of ["@scoped%2fno-deps", "@scoped/no-deps", "@scoped%2Fno-deps"]) {
      const response = await fetch(`${registry.url}${path}`);
      expect(response.status).toBe(200);
      const doc = await response.json();
      expect(doc.name).toBe("@scoped/no-deps");
      expect(doc.versions["1.0.0"].dist.tarball).toBe(`${registry.url}@scoped/no-deps/-/no-deps-1.0.0.tgz`);
    }
  });

  test("a single version and a dist-tag", async () => {
    expect((await (await fetch(`${registry.url}no-deps/1.1.0`)).json()).version).toBe("1.1.0");
    expect((await (await fetch(`${registry.url}no-deps/latest`)).json()).version).toBe("2.0.0");
    expect((await fetch(`${registry.url}no-deps/9.9.9`)).status).toBe(404);
  });

  test("dist-tags endpoint", async () => {
    const response = await fetch(`${registry.url}-/package/no-deps/dist-tags`);
    expect(await response.json()).toEqual({ latest: "2.0.0" });
  });

  test("unknown package is 404", async () => {
    expect((await fetch(`${registry.url}does-not-exist-anywhere`)).status).toBe(404);
  });

  test("ETag and 304 for packuments", async () => {
    const first = await fetch(`${registry.url}no-deps`);
    const etag = first.headers.get("etag");
    // undici asks for gzip, so the tag is weak (W/"md5"), as on registry.npmjs.org.
    expect(etag).toMatch(/^(W\/)?"[0-9a-f]{32}"$/);
    const second = await fetch(`${registry.url}no-deps`, { headers: { "if-none-match": etag! } });
    expect(second.status).toBe(304);
    expect(await second.text()).toBe("");
  });

  test("gzip and brotli content encoding", async () => {
    const body = await new Promise<{ encoding?: string; length: number }>((resolve, reject) => {
      http
        .get(`${registry.url}no-deps`, { headers: { "accept-encoding": "gzip" } }, res => {
          const chunks: Buffer[] = [];
          res.on("data", c => chunks.push(c));
          res.on("end", () => resolve({ encoding: res.headers["content-encoding"], length: Buffer.concat(chunks).length }));
        })
        .on("error", reject);
    });
    expect(body.encoding).toBe("gzip");
    expect(body.length).toBeGreaterThan(0);
  });
});

describe("tarballs", () => {
  test("bytes match dist.shasum and dist.integrity of the packument", async () => {
    const doc = await (await fetch(`${registry.url}no-deps`)).json();
    for (const version of Object.values(doc.versions) as any[]) {
      const response = await fetch(version.dist.tarball);
      expect(response.status).toBe(200);
      expect(response.headers.get("content-type")).toBe("application/octet-stream");
      const bytes = new Uint8Array(await response.arrayBuffer());
      expect(sha1(bytes)).toBe(version.dist.shasum);
      expect(sha512(bytes)).toBe(version.dist.integrity);
    }
  });

  test("ETag and 304 for tarballs", async () => {
    const url = `${registry.url}no-deps/-/no-deps-1.0.0.tgz`;
    const etag = (await fetch(url)).headers.get("etag")!;
    expect((await fetch(url, { headers: { "if-none-match": etag } })).status).toBe(304);
  });

  test("range requests", async () => {
    const response = await fetch(`${registry.url}no-deps/-/no-deps-1.0.0.tgz`, { headers: { range: "bytes=0-1" } });
    expect(response.status).toBe(206);
    expect([...new Uint8Array(await response.arrayBuffer())]).toEqual([0x1f, 0x8b]);
  });

  test("loose tgz fixtures get packuments with correct shasum and integrity", async () => {
    // bar-0.0.2.tgz lives in test/cli/install, next to the verdaccio packument of bar@0.0.7.
    const doc = await (await fetch(`${registry.url}bar`)).json();
    expect(Object.keys(doc.versions)).toEqual(expect.arrayContaining(["0.0.2", "0.0.7"]));
    expect(doc["dist-tags"].latest).toBe("0.0.7");
    const v = doc.versions["0.0.2"];
    const bytes = readFileSync(join(bunInstallTestDir, "bar-0.0.2.tgz"));
    expect(v.dist.shasum).toBe(sha1(bytes));
    expect(v.dist.integrity).toBe(sha512(bytes));
    const served = new Uint8Array(await (await fetch(v.dist.tarball)).arrayBuffer());
    expect(Buffer.compare(Buffer.from(served), bytes)).toBe(0);
  });

  test("packumentsFromTarballs reads every loose fixture", () => {
    const packages = packumentsFromTarballs(bunInstallTestDir, compareVersions);
    expect(packages.size).toBeGreaterThan(30);
    const baz = packages.get("baz")!;
    expect(Object.keys(baz.document.versions).sort()).toEqual(["0.0.3", "0.0.5"]);
    expect(baz.document["dist-tags"].latest).toBe("0.0.5");
    const manifest = readTarballManifest(readFileSync(join(bunInstallTestDir, "moo-0.1.0.tgz")));
    expect(manifest).toMatchObject({ name: "@barn/moo", version: "0.1.0" });
  });
});

describe("auth", () => {
  test("@needs-auth requires credentials (Bearer and Basic)", async () => {
    expect((await fetch(`${registry.url}@needs-auth%2ftest-pkg`)).status).toBe(404);
    const token = registry.createUser("alice", "secret-pw");
    const bearer = await fetch(`${registry.url}@needs-auth%2ftest-pkg`, { headers: { authorization: `Bearer ${token}` } });
    expect(bearer.status).toBe(200);
    const basic = await fetch(`${registry.url}@needs-auth%2ftest-pkg`, {
      headers: { authorization: `Basic ${Buffer.from("alice:secret-pw").toString("base64")}` },
    });
    expect(basic.status).toBe(200);
    const tarball = (await bearer.json()).versions["1.0.0"].dist.tarball;
    expect((await fetch(tarball)).status).toBe(404);
    expect((await fetch(tarball, { headers: { authorization: `Bearer ${token}` } })).status).toBe(200);
  });

  test("whoami", async () => {
    const token = registry.createUser("bob");
    const response = await fetch(`${registry.url}-/whoami`, { headers: { authorization: `Bearer ${token}` } });
    expect(await response.json()).toEqual({ username: "bob" });
    expect((await fetch(`${registry.url}-/whoami`)).status).toBe(401);
  });
});

describe("publish", () => {
  test("PUT a package, then read it back", async () => {
    const token = registry.createUser("publisher");
    const tarball = readFileSync(join(bunInstallTestDir, "moo-0.1.0.tgz"));
    const body = {
      _id: "fnpm-published",
      name: "fnpm-published",
      "dist-tags": { latest: "1.0.0" },
      versions: {
        "1.0.0": { name: "fnpm-published", version: "1.0.0", dist: { shasum: sha1(tarball), integrity: sha512(tarball) } },
      },
      _attachments: {
        "fnpm-published-1.0.0.tgz": { content_type: "application/octet-stream", data: tarball.toString("base64"), length: tarball.length },
      },
    };
    const put = await fetch(`${registry.url}fnpm-published`, {
      method: "PUT",
      headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
      body: JSON.stringify(body),
    });
    expect(put.status).toBe(200);
    const doc = await (await fetch(`${registry.url}fnpm-published`)).json();
    expect(doc["dist-tags"].latest).toBe("1.0.0");
    const served = Buffer.from(await (await fetch(doc.versions["1.0.0"].dist.tarball)).arrayBuffer());
    expect(Buffer.compare(served, tarball)).toBe(0);

    const again = await fetch(`${registry.url}fnpm-published`, {
      method: "PUT",
      headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
      body: JSON.stringify(body),
    });
    expect(again.status).toBe(403);
  });

  test("publish without credentials is 401", async () => {
    const response = await fetch(`${registry.url}nope`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "nope", versions: {}, _attachments: {} }),
    });
    expect(response.status).toBe(401);
  });
});

describe("fault injection", () => {
  test("500 a fixed number of times, then the real answer", async () => {
    const fault = registry.addFault({ path: "/a-dep", status: 500, times: 2 });
    expect((await fetch(`${registry.url}a-dep`)).status).toBe(500);
    expect((await fetch(`${registry.url}a-dep`)).status).toBe(500);
    expect((await fetch(`${registry.url}a-dep`)).status).toBe(200);
    expect((fault as { hits?: number }).hits).toBe(2);
    registry.clearFaults();
  });

  test("429 with Retry-After", async () => {
    registry.addFault({ path: /^\/b-dep-a/, status: 429, retryAfter: 1, times: 1 });
    const response = await fetch(`${registry.url}b-dep-a`);
    expect(response.status).toBe(429);
    expect(response.headers.get("retry-after")).toBe("1");
    expect((await fetch(`${registry.url}b-dep-a`)).status).toBe(200);
    registry.clearFaults();
  });

  test("connection reset", async () => {
    registry.addFault({ path: "/a-dep-b", reset: true, times: 1 });
    await expect(fetch(`${registry.url}a-dep-b`)).rejects.toThrow();
    expect((await fetch(`${registry.url}a-dep-b`)).status).toBe(200);
    registry.clearFaults();
  });

  test("truncated body", async () => {
    registry.addFault({ path: /\.tgz$/, truncate: true, times: 1 });
    await expect(fetch(`${registry.url}no-deps/-/no-deps-1.0.0.tgz`).then(r => r.arrayBuffer())).rejects.toThrow();
    registry.clearFaults();
  });

  test("latency", async () => {
    registry.addFault({ path: "/no-deps", delayMs: 150, times: 1 });
    const started = performance.now();
    expect((await fetch(`${registry.url}no-deps`)).status).toBe(200);
    expect(performance.now() - started).toBeGreaterThanOrEqual(140);
    registry.clearFaults();
  });
});

test("records requests with their status", async () => {
  const before = registry.requests.length;
  await fetch(`${registry.url}no-deps`, { headers: { accept: abbreviatedContentType } });
  await fetch(`${registry.url}missing-pkg-xyz`);
  const recent = registry.requests.slice(before);
  expect(recent.map(r => [r.method, r.path, r.status])).toEqual([
    ["GET", "/no-deps", 200],
    ["GET", "/missing-pkg-xyz", 404],
  ]);
  expect(recent[0].headers.accept).toBe(abbreviatedContentType);
});

describe("uplink", () => {
  test("proxies an upstream registry and serves from the disk cache afterwards", async () => {
    const upstream = await start({ port: 0 });
    const cacheDir = temp("uplink");
    try {
      const mirror = await start({ port: 0, packagesDir: null, uplink: upstream.url, cacheDir });
      const corgi = await (await fetch(`${mirror.url}no-deps`, { headers: { accept: abbreviatedContentType } })).json();
      expect(corgi.versions["1.0.0"].dist.tarball).toBe(`${mirror.url}no-deps/-/no-deps-1.0.0.tgz`);
      const full = await (await fetch(`${mirror.url}@scoped%2fno-deps`)).json();
      expect(full.name).toBe("@scoped/no-deps");
      const tgz = new Uint8Array(await (await fetch(corgi.versions["1.0.0"].dist.tarball)).arrayBuffer());
      expect(sha512(tgz)).toBe(corgi.versions["1.0.0"].dist.integrity);
      expect((await fetch(`${mirror.url}nope-not-upstream`)).status).toBe(404);
      expect(mirror.registry.uplink!.stats.tarballFetches).toBe(1);
      await mirror.stop();

      expect(existsSync(join(cacheDir, "packuments", "abbreviated", "no-deps.json"))).toBe(true);
      expect(existsSync(join(cacheDir, "packuments", "full", "@scoped%2fno-deps.json"))).toBe(true);
      expect(existsSync(join(cacheDir, "tarballs", "no-deps", "no-deps-1.0.0.tgz"))).toBe(true);

      // Upstream gone: the cache answers.
      const upstreamRequests = upstream.requests.length;
      await upstream.stop();
      const offline = await start({ port: 0, packagesDir: null, uplink: upstream.url, cacheDir });
      const again = await (await fetch(`${offline.url}no-deps`, { headers: { accept: abbreviatedContentType } })).json();
      expect(again.versions["1.0.0"].dist.tarball).toBe(`${offline.url}no-deps/-/no-deps-1.0.0.tgz`);
      const tgz2 = new Uint8Array(await (await fetch(again.versions["1.0.0"].dist.tarball)).arrayBuffer());
      expect(Buffer.compare(Buffer.from(tgz2), Buffer.from(tgz))).toBe(0);
      expect(offline.registry.uplink!.stats).toMatchObject({ packumentFetches: 0, tarballFetches: 0 });
      expect(upstreamRequests).toBeGreaterThan(0);
      await offline.stop();
    } finally {
      await upstream.stop();
      rmSync(cacheDir, { recursive: true, force: true });
    }
  });

  test("local packages win over the uplink", async () => {
    const upstream = await start({ port: 0, packagesDir: null, tarballDirs: [] });
    const mirror = await start({ port: 0, uplink: upstream.url });
    try {
      expect((await fetch(`${mirror.url}no-deps`)).status).toBe(200);
      expect(upstream.requests.length).toBe(0);
    } finally {
      await mirror.stop();
      await upstream.stop();
    }
  });

  test("goes through HTTP_PROXY when it is set", async () => {
    const upstream = await start({ port: 0 });
    const seen: string[] = [];
    // A forward proxy for http:// targets (absolute-form request lines).
    const proxy = http.createServer((req, res) => {
      seen.push(req.url!);
      const target = new URL(req.url!);
      const forward = http.request(target, { method: req.method, headers: req.headers }, upstreamRes => {
        res.writeHead(upstreamRes.statusCode!, upstreamRes.headers);
        upstreamRes.pipe(res);
      });
      req.pipe(forward);
    });
    await new Promise<void>(resolve => proxy.listen(0, "127.0.0.1", resolve));
    const proxyUrl = `http://127.0.0.1:${(proxy.address() as { port: number }).port}`;
    const { Uplink } = await import("../src/uplink.ts");
    const uplink = new Uplink({ url: upstream.url, env: { HTTP_PROXY: proxyUrl, NO_PROXY: "" } });
    try {
      const doc = await uplink.packument("no-deps", "full");
      expect(doc?.name).toBe("no-deps");
      expect(seen).toEqual([`${upstream.url}no-deps`]);
    } finally {
      uplink.close();
      proxy.closeAllConnections();
      proxy.close();
      await upstream.stop();
    }
  });
});
