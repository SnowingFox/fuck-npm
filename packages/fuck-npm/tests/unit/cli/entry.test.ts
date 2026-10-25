import { mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { describe, expect, test, vi } from "vitest";
import {
  formatUnsupportedNodeVersionMessage,
  isEntryPoint,
  isSupportedNodeVersion,
  main,
  MIN_NODE_VERSION,
} from "@/entry.ts";

describe("isSupportedNodeVersion", () => {
  test.each([
    ["20.19.0", true],
    ["20.19.1", true],
    ["22.0.0", true],
    ["24.22.1", true],
    ["20.18.9", false],
    ["18.20.0", false],
    ["16.0.0", false],
  ])("%s -> %s", (version, expected) => {
    expect(isSupportedNodeVersion(version)).toBe(expected);
  });

  test("compares numerically, not lexically", () => {
    // "20.9.0" > "20.19.0" as strings: the classic way this check lets an
    // unsupported runtime through.
    expect(isSupportedNodeVersion("20.9.0")).toBe(false);
  });
});

describe("formatUnsupportedNodeVersionMessage", () => {
  test("names both versions, the bin, and a way out", () => {
    const message = formatUnsupportedNodeVersionMessage("18.20.0", "fnpm-lock");

    expect(message).toContain(MIN_NODE_VERSION);
    expect(message).toContain("18.20.0");
    expect(message).toContain("fnpm-lock requires");
    expect(message).toContain("nvm");
  });
});

describe("main", () => {
  test("refuses an unsupported runtime without loading the app", async () => {
    const written: string[] = [];
    const spy = vi.spyOn(process.stderr, "write").mockImplementation((chunk) => {
      written.push(String(chunk));
      return true;
    });

    const code = await main("fnpm", ["node", "fnpm", "--version"], "18.20.0");

    spy.mockRestore();
    expect(code).toBe(1);
    expect(written.join("")).toContain("requires Node");
  });

  test("answers --version on the fast path", async () => {
    const written: string[] = [];
    const spy = vi.spyOn(process.stdout, "write").mockImplementation((chunk) => {
      written.push(String(chunk));
      return true;
    });

    const code = await main("fnpm", ["node", "fnpm", "--version"], "22.0.0");

    spy.mockRestore();
    expect(code).toBe(0);
    expect(written.join("").trim()).toMatch(/^\d+\.\d+\.\d+/);
  });

  test("runs the CLI on a supported runtime", async () => {
    const spy = vi.spyOn(process.stdout, "write").mockImplementation(() => true);

    const code = await main("fnpm-lock", ["node", "fnpm-lock", "--tree"], "22.0.0");

    spy.mockRestore();
    expect(code).toBe(0);
  });
});

describe("isEntryPoint", () => {
  test("is true through a symlink, as npm link and global installs create", () => {
    // Without realpath on both sides, the installed (symlinked) bin compares
    // unequal and exits 0 having printed nothing.
    const dir = mkdtempSync(join(tmpdir(), "fnpm-entry-"));
    const real = join(dir, "fnpm.js");
    const link = join(dir, "fnpm");
    writeFileSync(real, "");
    symlinkSync(real, link);

    expect(isEntryPoint(pathToFileURL(real).href, link)).toBe(true);
    expect(isEntryPoint(pathToFileURL(real).href, join(dir, "other.js"))).toBe(false);

    rmSync(dir, { recursive: true, force: true });
  });
});
