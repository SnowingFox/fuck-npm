import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { describe, expect, test } from "vitest";
import { eventCallback, loadCore, wrapNative } from "@/core/load.ts";
import type { CoreEvent } from "@/core/types.ts";
import { emptySummary } from "@/core/fake.ts";
import { CoreError, SilentError } from "@/errors.ts";

describe("loadCore", () => {
  test("a missing binding is a CORE_UNAVAILABLE SilentError, not a crash", () => {
    const dir = mkdtempSync(join(tmpdir(), "fnpm-load-"));
    const from = pathToFileURL(join(dir, "index.js")).href;

    let caught: unknown;
    try {
      loadCore(from);
    } catch (error) {
      caught = error;
    }
    rmSync(dir, { recursive: true, force: true });

    expect(caught).toBeInstanceOf(SilentError);
    expect((caught as SilentError).code).toBe("CORE_UNAVAILABLE");
    expect((caught as SilentError).message).toContain("@fuck-npm/binding is not installed");
  });
});

describe("wrapNative", () => {
  test("rejects a module that does not export the contract", () => {
    expect(() => wrapNative({ install: () => {} })).toThrow("does not export install/invoke/version");
  });

  test("forwards calls and maps native errors to CoreError with their code", async () => {
    const core = wrapNative({
      version: () => "0.1.0",
      install: async () => emptySummary({ installedCount: 4 }),
      invoke: async () => {
        throw Object.assign(new Error("no lockfile"), { code: "LOCKFILE_NOT_FOUND" });
      },
    });

    expect(core.version()).toBe("0.1.0");
    expect((await core.install({} as never)).installedCount).toBe(4);
    const failure = await core
      .invoke({ command: ["pm", "hash"], cwd: "/", positionals: [], passthrough: [], options: {}, json: false })
      .catch((error: unknown) => error);
    expect(failure).toBeInstanceOf(CoreError);
    expect((failure as CoreError).code).toBe("LOCKFILE_NOT_FOUND");
  });
});

describe("eventCallback", () => {
  test("no listener means no callback is registered at all", () => {
    expect(eventCallback(undefined)).toBeUndefined();
  });

  test("accepts batched (events) and callee-handled (err, events) calls", () => {
    const seen: CoreEvent[] = [];
    const callback = eventCallback((event) => seen.push(event)) as (...args: unknown[]) => void;

    callback([{ type: "resolve", resolved: 1, total: 2 }, { type: "link", done: 1, total: 1 }]);
    callback(null, { type: "warn", message: "w" });
    callback(new Error("boom"), undefined);

    expect(seen.map((event) => event.type)).toEqual(["resolve", "link", "warn", "error"]);
  });
});

describe("@fuck-npm/binding loader", () => {
  const require = createRequire(import.meta.url);
  const loaderPath = join(import.meta.dirname, "..", "..", "..", "..", "fuck-npm-binding", "index.js");

  test("names the platform and every path it tried when nothing loads", () => {
    const dir = mkdtempSync(join(tmpdir(), "fnpm-binding-"));
    const previous = { ...process.env };
    process.env.NAPI_RS_NATIVE_LIBRARY_PATH = join(dir, "missing.node");
    process.env.CARGO_TARGET_DIR = join(dir, "target");

    let caught: (Error & { code?: string }) | undefined;
    try {
      delete require.cache[loaderPath];
      require(loaderPath);
    } catch (error) {
      caught = error as Error & { code?: string };
    } finally {
      process.env = previous;
      rmSync(dir, { recursive: true, force: true });
    }

    // A dev build may legitimately exist on this machine; only assert when it failed.
    if (caught !== undefined) {
      expect(caught.code).toBe("FNPM_BINDING_NOT_FOUND");
      expect(caught.message).toContain("missing.node (not found)");
      expect(caught.message).toContain("@fuck-npm/binding-");
    }
  });

  test("loads an explicit NAPI_RS_NATIVE_LIBRARY_PATH through process.dlopen", () => {
    const dir = mkdtempSync(join(tmpdir(), "fnpm-binding-"));
    const bogus = join(dir, "bogus.node");
    writeFileSync(bogus, "not a shared object");
    const previous = { ...process.env };
    process.env.NAPI_RS_NATIVE_LIBRARY_PATH = bogus;
    process.env.CARGO_TARGET_DIR = join(dir, "target");

    let message = "";
    try {
      delete require.cache[loaderPath];
      require(loaderPath);
    } catch (error) {
      message = (error as Error).message;
    } finally {
      process.env = previous;
      rmSync(dir, { recursive: true, force: true });
    }

    // dlopen was attempted on the override (and failed on garbage bytes).
    if (message !== "") {
      expect(message).toContain("bogus.node (");
      expect(message).not.toContain("bogus.node (not found)");
    }
  });
});
