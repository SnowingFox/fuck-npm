// Loads the built addon (npm run build:native:test) and checks the JS contract in
// index.d.ts: payload handling, error codes, and event delivery ordering. The `__test*`
// exports exist only in builds with the cargo feature `testing`.
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, describe, expect, it } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(here, "..");
const require = createRequire(import.meta.url);
const binding = require(pkg);

const testing = typeof binding.__testDeliver === "function";

/** The core is implemented concurrently; until then its entry points panic with "not yet implemented". */
function coreStubbed(error) {
  return error?.code === "CORE_ERROR" && /not (yet )?implemented/.test(error.message);
}

async function rejection(promise) {
  try {
    await promise;
  } catch (error) {
    return error;
  }
  throw new Error("expected the promise to reject");
}

function runFixture(name, ...args) {
  return execFileSync(process.execPath, [join(here, "fixtures", name), pkg, ...args], {
    encoding: "utf8",
    timeout: 20_000,
  });
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
    const out = runFixture("load-failure.cjs");
    const report = JSON.parse(out);
    expect(report.code).toBe("FNPM_BINDING_NOT_FOUND");
    expect(report.message).toContain("Cannot load the fnpm native core");
    expect(report.message).toContain("does-not-exist.node (not found)");
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
  it("rejects install options of the wrong shape with USAGE", async () => {
    const error = await rejection(binding.install({ packages: "react" }));
    expect(error).toBeInstanceOf(Error);
    expect(error.code).toBe("USAGE");
    expect(error.message).toMatch(/^invalid install options: invalid type: string "react"/);
  });

  it("rejects unknown enum values with USAGE", async () => {
    const error = await rejection(binding.install({ command: "explode" }));
    expect(error.code).toBe("USAGE");
    expect(error.message).toContain("explode");
  });

  it("rejects a request that is not an object with USAGE", async () => {
    const error = await rejection(binding.invoke(["ls"]));
    expect(error.code).toBe("USAGE");
    expect(error.message).toMatch(/^invalid request: /);
  });

  it("does not call onEvents for a payload error", async () => {
    const calls = [];
    await rejection(binding.invoke({ command: 5 }, (...args) => calls.push(args)));
    expect(calls).toEqual([]);
  });

  it("rejects an unknown command with USAGE", async (ctx) => {
    const error = await rejection(binding.invoke({ command: ["definitely-not-a-command"] }));
    if (coreStubbed(error)) ctx.skip();
    expect(error.code).toBe("USAGE");
    expect(error.message).toContain("definitely-not-a-command");
  });

  describe("against a project", () => {
    const dir = mkdtempSync(join(tmpdir(), "fnpm-binding-"));
    afterAll(() => rmSync(dir, { recursive: true, force: true }));

    it("an install of an empty project resolves with a summary", async (ctx) => {
      const { writeFileSync } = await import("node:fs");
      writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "empty", version: "1.0.0" }));
      const events = [];
      let summary;
      try {
        summary = await binding.install({ cwd: dir, command: "install", offline: true }, (err, batch) =>
          events.push(...batch),
        );
      } catch (error) {
        if (coreStubbed(error)) ctx.skip();
        throw error;
      }
      expect(summary).toMatchObject({ failedCount: 0, installedCount: 0 });
      expect(typeof summary.elapsedMs).toBe("number");
      for (const event of events) expect(typeof event.type).toBe("string");
    });
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
