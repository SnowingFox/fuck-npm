// Loads the built addon (npm run build:native:test) and checks the JS contract in
// index.d.ts: payload handling, error codes, event delivery ordering, and real installs
// against a local registry. The `__test*` exports exist only in builds with the cargo
// feature `testing`.
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startRegistry } from "./helpers/registry.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(here, "..");
const require = createRequire(import.meta.url);
const binding = require(pkg);

const testing = typeof binding.__testDeliver === "function";

async function rejection(promise) {
  try {
    await promise;
  } catch (error) {
    return error;
  }
  throw new Error("expected the promise to reject");
}

/** Calls `fn` and returns the rejection; a synchronous throw fails the test. */
async function rejectionOf(fn) {
  let promise;
  expect(() => {
    promise = fn();
  }).not.toThrow();
  expect(promise).toBeInstanceOf(Promise);
  return rejection(promise);
}

function runFixture(name, ...args) {
  return execFileSync(process.execPath, [join(here, "fixtures", name), pkg, ...args], {
    encoding: "utf8",
    timeout: 20_000,
  });
}

function tempDir(prefix) {
  return mkdtempSync(join(tmpdir(), prefix));
}

describe("loader", () => {
  const abi = binding.__platformArchAbi;

  it("maps every published target", () => {
    expect(abi("darwin", "arm64")).toBe("darwin-arm64");
    expect(abi("darwin", "x64")).toBe("darwin-x64");
    expect(abi("linux", "x64", () => false)).toBe("linux-x64-gnu");
    expect(abi("linux", "arm64", () => false)).toBe("linux-arm64-gnu");
    expect(abi("linux", "x64", () => true)).toBe("linux-x64-musl");
    expect(abi("linux", "arm64", () => true)).toBe("linux-arm64-musl");
    expect(abi("win32", "x64")).toBe("win32-x64-msvc");
    expect(abi("win32", "arm64")).toBe("win32-arm64-msvc");
  });

  it("returns null for platforms without a build", () => {
    expect(abi("linux", "ia32", () => false)).toBeNull();
    expect(abi("freebsd", "x64")).toBeNull();
    expect(abi("darwin", "ppc64")).toBeNull();
  });

  it("targets in package.json match the loader", () => {
    const { napi } = JSON.parse(readFileSync(join(pkg, "package.json"), "utf8"));
    expect(napi.binaryName).toBe("fnpm");
    expect(napi.targets).toHaveLength(8);
  });

  it("reports every candidate when nothing loads", () => {
    const report = JSON.parse(runFixture("load-failure.cjs", "repository"));
    expect(report.code).toBe("FNPM_BINDING_NOT_FOUND");
    expect(report.message).toContain("Cannot load the fnpm native core");
    expect(report.message).toContain("does-not-exist.node (not found)");
    expect(report.message).toContain("@fuck-npm/binding-darwin-x64 (not installed)");
    expect(report.message).toMatch(/release-napi[\\/]libfnpm_napi\.dylib \(not found\)/);
  });

  it("never loads development builds from an installed copy's node_modules", () => {
    // index.js sits in node_modules/@fuck-npm/binding next to a planted
    // node_modules/target/release/libfnpm_napi.so and node_modules/crates/...
    const report = JSON.parse(runFixture("load-failure.cjs", "installed"));
    expect(report.code).toBe("FNPM_BINDING_NOT_FOUND");
    expect(report.message).toContain("does-not-exist.node (not found)");
    expect(report.message).toContain("@fuck-npm/binding-darwin-x64 (not installed)");
    expect(report.message).not.toContain("libfnpm_napi");
    expect(report.message).not.toContain("crates");
    expect(binding.__inRepository("/home/u/project/node_modules/@fuck-npm/binding")).toBe(false);
    expect(binding.__inRepository("C:\\u\\node_modules\\@fuck-npm\\binding")).toBe(false);
    expect(binding.__inRepository(pkg)).toBe(true);
  });
});

describe("load", () => {
  it.runIf(process.platform === "linux")("starts no threads until the core runs", () => {
    // napi-rs's `tokio_rt` feature would start its own runtime (one thread per CPU) here.
    const { before, loaded, afterVersion } = JSON.parse(runFixture("threads.cjs"));
    expect(loaded).toBe(before);
    expect(afterVersion).toBe(before);
  });
});

describe("version", () => {
  it("is the crate version", () => {
    const cargo = readFileSync(join(pkg, "..", "..", "Cargo.toml"), "utf8");
    const workspaceVersion = /\[workspace\.package\][^[]*?version\s*=\s*"([^"]+)"/s.exec(cargo)[1];
    expect(binding.version()).toBe(workspaceVersion);
  });
});

describe("payloads", () => {
  const cwd = tempDir("fnpm-binding-payload-");
  afterAll(() => rmSync(cwd, { recursive: true, force: true }));

  it("rejects install options of the wrong shape with USAGE", async () => {
    const error = await rejectionOf(() => binding.install({ cwd, packages: "react" }));
    expect(error).toBeInstanceOf(Error);
    expect(error.code).toBe("USAGE");
    expect(error.message).toMatch(/^invalid install options: invalid type: string "react"/);
  });

  it("rejects unknown enum values with USAGE", async () => {
    const error = await rejectionOf(() => binding.install({ cwd, command: "explode" }));
    expect(error.code).toBe("USAGE");
    expect(error.message).toContain("explode");
  });

  it("rejects a request that is not an object with USAGE", async () => {
    const error = await rejectionOf(() => binding.invoke(["ls"]));
    expect(error.code).toBe("USAGE");
    expect(error.message).toMatch(/^invalid request: /);
  });

  it.each([
    ["no options at all", () => binding.install()],
    ["undefined options", () => binding.install(undefined)],
    ["NaN", () => binding.install({ cwd, networkConcurrency: Number.NaN })],
    ["Infinity", () => binding.install({ cwd, networkConcurrency: Number.POSITIVE_INFINITY })],
    ["a function", () => binding.install({ cwd, extra: () => 1 })],
    ["a symbol", () => binding.invoke({ cwd, command: ["ls"], options: { s: Symbol("s") } })],
    [
      "nesting deeper than JSON allows",
      () => {
        const options = {};
        let node = options;
        for (let i = 0; i < 500; i++) node = node.a = {};
        return binding.invoke({ cwd, command: ["ls"], options });
      },
    ],
  ])("rejects %s with USAGE instead of throwing", async (_name, call) => {
    const error = await rejectionOf(call);
    expect(error.code).toBe("USAGE");
    expect(error.message).toMatch(/^invalid (install options|request): /);
  });

  it.each([
    ["missing", {}],
    ["empty", { cwd: "" }],
    ["relative", { cwd: "some/dir" }],
  ])("rejects a %s cwd with USAGE, before touching any directory", async (_name, payload) => {
    let error = await rejectionOf(() => binding.install(payload));
    expect(error.code).toBe("USAGE");
    expect(error.message).toMatch(/^invalid install options: `cwd` must be an absolute path/);
    error = await rejectionOf(() => binding.invoke({ command: ["ls"], ...payload }));
    expect(error.code).toBe("USAGE");
    expect(error.message).toMatch(/^invalid request: `cwd` must be an absolute path/);
  });

  it("rejects an onEvents that is not a function with USAGE", async () => {
    const error = await rejectionOf(() => binding.install({ cwd }, 5));
    expect(error.code).toBe("USAGE");
    expect(error.message).toBe("invalid onEvents: expected a function, got number");
  });

  it("does not call onEvents for a payload error", async () => {
    const calls = [];
    await rejection(binding.invoke({ command: 5, cwd }, (...args) => calls.push(args)));
    expect(calls).toEqual([]);
  });

  it("accepts integers beyond 32 bits (60 days in ms)", async () => {
    writeFileSync(join(cwd, "package.json"), JSON.stringify({ name: "p", version: "1.0.0" }));
    const summary = await binding.install({ cwd, offline: true, minimumReleaseAgeMs: 60 * 86_400_000 });
    expect(summary.failedCount).toBe(0);
  });

  it("rejects an unknown command with USAGE", async () => {
    const error = await rejectionOf(() => binding.invoke({ command: ["definitely-not-a-command"], cwd }));
    expect(error.code).toBe("USAGE");
    expect(error.message).toContain("definitely-not-a-command");
  });
});

describe("installs against a local registry", () => {
  let registry;
  let home;
  const saved = {};
  const env = { FNPM_HOME: "", FNPM_CONFIG_REGISTRY: "", NO_PROXY: "127.0.0.1,localhost", no_proxy: "127.0.0.1,localhost" };

  beforeAll(async () => {
    registry = await startRegistry({
      "tiny-dep": { "1.0.0": {} },
      tiny: { "1.0.0": { dependencies: { "tiny-dep": "^1.0.0" } }, "1.1.0": { dependencies: { "tiny-dep": "^1.0.0" } } },
    });
    home = tempDir("fnpm-binding-home-");
    env.FNPM_HOME = home;
    env.FNPM_CONFIG_REGISTRY = registry.url;
    for (const [key, value] of Object.entries(env)) {
      saved[key] = process.env[key];
      process.env[key] = value;
    }
  });

  afterAll(async () => {
    for (const [key, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    await registry?.close();
    rmSync(home, { recursive: true, force: true });
  });

  function project() {
    const dir = tempDir("fnpm-binding-project-");
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "app", version: "1.0.0" }, null, 2));
    return dir;
  }

  it("add: delivers every event batch before resolving with the summary", async () => {
    const cwd = project();
    const batches = [];
    let settled = false;
    const summary = await binding
      .install({ cwd, command: "add", packages: ["tiny"], registry: registry.url }, (err, batch) => {
        expect(err).toBeNull();
        expect(settled).toBe(false);
        batches.push(batch);
      })
      .finally(() => {
        settled = true;
      });
    expect(summary).toMatchObject({
      added: [{ name: "tiny", version: "1.1.0" }],
      installedCount: 2,
      failedCount: 0,
      noop: false,
    });
    expect(batches.length).toBeGreaterThan(0);
    const events = batches.flat();
    for (const event of events) expect(typeof event.type).toBe("string");
    expect(events.some((event) => event.type === "resolve")).toBe(true);
    expect(JSON.parse(readFileSync(join(cwd, "package.json"), "utf8")).dependencies).toEqual({ tiny: "^1.1.0" });
    expect(existsSync(join(cwd, "node_modules", "tiny", "index.js"))).toBe(true);

    // Nothing changed: the fast path answers, without a callback too.
    const again = await binding.install({ cwd, registry: registry.url });
    expect(again).toMatchObject({ noop: true, installedCount: 0 });

    // invoke resolves with CoreResult: `data` for --json, `lines` for humans.
    const listed = await binding.invoke({ command: ["pm", "ls"], cwd, json: true }, () => {});
    expect(listed.exitCode).toBe(0);
    expect(JSON.stringify(listed.data)).toContain("tiny");
    for (const line of listed.lines ?? []) expect(typeof line).toBe("string");
    rmSync(cwd, { recursive: true, force: true });
  });

  it("maps core errors to their codes", async () => {
    const cwd = project();
    let error = await rejection(
      binding.install({ cwd, command: "add", packages: ["no-such-package"], registry: registry.url }, () => {}),
    );
    expect(error).toBeInstanceOf(Error);
    expect(error.code).toBe("NOT_FOUND");
    expect(error.message).toContain("no-such-package");

    error = await rejection(binding.install({ cwd, command: "add", packages: ["tiny@^9"], registry: registry.url }));
    expect(error.code).toBe("NO_MATCHING_VERSION");

    await binding.install({ cwd, command: "add", packages: ["tiny@1.0.0"], registry: registry.url });
    const manifest = JSON.parse(readFileSync(join(cwd, "package.json"), "utf8"));
    manifest.dependencies["tiny-dep"] = "1.0.0";
    writeFileSync(join(cwd, "package.json"), JSON.stringify(manifest));
    error = await rejection(binding.install({ cwd, frozenLockfile: true, registry: registry.url }));
    expect(error.code).toBe("FROZEN_LOCKFILE");

    mkdirSync(join(cwd, "broken"));
    writeFileSync(join(cwd, "broken", "package.json"), "{ not json");
    error = await rejection(binding.install({ cwd: join(cwd, "broken"), registry: registry.url }));
    expect(error.code).toBe("INVALID_PACKAGE_JSON");
    rmSync(cwd, { recursive: true, force: true });
  });

  it("runs installs concurrently in one process", async () => {
    const dirs = Array.from({ length: 4 }, project);
    const summaries = await Promise.all(
      dirs.map((cwd) => binding.install({ cwd, command: "add", packages: ["tiny-dep"], registry: registry.url }, () => {})),
    );
    for (const summary of summaries) expect(summary.added).toEqual([{ name: "tiny-dep", version: "1.0.0" }]);
    for (const cwd of dirs) rmSync(cwd, { recursive: true, force: true });
  });
});

describe.runIf(testing)("event delivery", () => {
  it("calls onEvents as (null, batch) for every batch, in order, before resolving", async () => {
    const batches = Array.from({ length: 50 }, (_, i) => [
      { type: "resolve", resolved: i, total: 50 },
      { type: "warn", message: `w${i}` },
    ]);
    const seen = [];
    const result = binding.__testDeliver(batches, (err, batch) => {
      expect(err).toBeNull();
      seen.push(batch);
    });
    expect(seen).toEqual([]); // nothing runs on the JS thread synchronously
    expect(await result).toEqual({ delivered: 50 });
    expect(seen).toEqual(batches);
  });

  it("delivers every batch before a rejection too", async () => {
    const seen = [];
    const error = await rejection(
      binding.__testDeliver(
        [[{ type: "link", done: 1, total: 1 }]],
        (_err, batch) => seen.push(batch),
        { code: "NETWORK", message: "registry unreachable" },
      ),
    );
    expect(seen).toHaveLength(1);
    expect(error.code).toBe("NETWORK");
    expect(error.message).toBe("registry unreachable");
  });

  it("keeps up with many small batches", async () => {
    const batches = Array.from({ length: 2_000 }, (_, i) => [{ type: "link", done: i, total: 2_000 }]);
    let last = -1;
    let ordered = true;
    await binding.__testDeliver(batches, (_err, [event]) => {
      ordered &&= event.done === last + 1;
      last = event.done;
    });
    expect(ordered).toBe(true);
    expect(last).toBe(1_999);
  });

  it("works without a callback", async () => {
    expect(await binding.__testDeliver([[{ type: "link", done: 1, total: 1 }]])).toEqual({ delivered: 0 });
  });

  it("through the batching sink: nothing lost, nothing reordered, all before settling", async () => {
    // Emission spans several 50 ms flushes, so the timer flushes while the work ends.
    for (let round = 0; round < 8; round++) {
      const events = Array.from({ length: 120 }, (_, i) => ({ type: "warn", message: `${round}-${i}` }));
      const seen = [];
      let settled = false;
      const result = await binding
        .__testEmit(events, (_err, batch) => {
          expect(settled).toBe(false);
          seen.push(...batch);
        }, 3, 2)
        .finally(() => {
          settled = true;
        });
      expect(result).toEqual({ emitted: 120 });
      expect(seen).toEqual(events);
    }
  });

  it("through the batching sink: progress coalesces to the latest value", async () => {
    const events = Array.from({ length: 500 }, (_, i) => ({ type: "link", done: i + 1, total: 500 }));
    const seen = [];
    await binding.__testEmit(events, (_err, batch) => seen.push(...batch));
    expect(seen.length).toBeGreaterThan(0);
    expect(seen.length).toBeLessThan(500);
    expect(seen.at(-1)).toEqual({ type: "link", done: 500, total: 500 });
    const done = seen.map((event) => event.done);
    expect(done).toEqual([...done].sort((a, b) => a - b));
  });

  it("a throwing callback is an uncaught exception and the promise still settles", () => {
    const report = JSON.parse(runFixture("throwing-callback.cjs"));
    expect(report).toEqual({ uncaught: ["listener blew up"], settled: { delivered: 3 }, calls: 3 });
  });

  it("does not keep the process alive after settling", () => {
    // The fixture never exits explicitly; execFileSync would time out if the
    // ThreadsafeFunction (or anything else) kept the event loop running.
    expect(runFixture("exits.cjs").trim()).toBe("settled");
  });
});

describe.runIf(testing)("rejections", () => {
  it("carry the code as a property of a real Error", async () => {
    const error = await rejection(binding.__testRejectNow("FROZEN_LOCKFILE", "lockfile had changes"));
    expect(error).toBeInstanceOf(Error);
    expect(error.code).toBe("FROZEN_LOCKFILE");
    expect(error.message).toBe("lockfile had changes");
    expect(Object.keys(error)).toContain("code");
  });

  it("turn a panic in the work into CORE_ERROR", async () => {
    const error = await rejection(binding.__testDeliver([], undefined, null, "kaboom"));
    expect(error.code).toBe("CORE_ERROR");
    expect(error.message).toBe("internal error in the fnpm core: kaboom");
    // The runtime survives a panicking task.
    expect(await binding.__testDeliver([])).toEqual({ delivered: 0 });
  });
});
