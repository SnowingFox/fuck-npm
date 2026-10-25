/**
 * The CLI's `Core` over the real addon (`loadCore` → @fuck-npm/binding), when one is built:
 * options built by the CLI deserialize in Rust, events reach the sink one by one, and
 * rejections become `CoreError` with the core's code. Skipped when no build loads.
 */
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { loadCore } from "@/core/load.ts";
import { installOptions } from "@/core/options.ts";
import type { Core, CoreEvent, InstallCommand } from "@/core/types.ts";
import { CoreError } from "@/errors.ts";

function tryLoad(): Core | undefined {
  try {
    return loadCore();
  } catch {
    return undefined;
  }
}

const core = tryLoad();

describe.runIf(core !== undefined)("the real native core", () => {
  const native = core as Core;
  let root: string;
  let previousHome: string | undefined;

  beforeAll(() => {
    root = mkdtempSync(join(tmpdir(), "fnpm-native-"));
    previousHome = process.env.FNPM_HOME;
    process.env.FNPM_HOME = join(root, "home");
  });

  afterAll(() => {
    if (previousHome === undefined) delete process.env.FNPM_HOME;
    else process.env.FNPM_HOME = previousHome;
    rmSync(root, { recursive: true, force: true });
  });

  async function rejection(promise: Promise<unknown>): Promise<CoreError> {
    try {
      await promise;
    } catch (error) {
      expect(error).toBeInstanceOf(CoreError);
      return error as CoreError;
    }
    throw new Error("expected a rejection");
  }

  test("an empty project installs with events, then hits the no-op fast path", async () => {
    const dir = mkdtempSync(join(root, "empty-"));
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "empty", version: "1.0.0" }));
    const events: CoreEvent[] = [];
    const options = installOptions("install", { offline: true }, [], dir);
    const summary = await native.install(options, (event) => events.push(event));
    expect(summary).toMatchObject({ installedCount: 0, failedCount: 0, noop: false });
    expect(events.length).toBeGreaterThan(0);
    for (const event of events) expect(typeof event.type).toBe("string");
    expect(await native.install(options)).toMatchObject({ noop: true });
  });

  const flagCases: [InstallCommand, Record<string, boolean | string | string[]>, string[]][] = [
    [
      "install",
      {
        yarn: true,
        production: true,
        ca: ["-----BEGIN CERTIFICATE-----"],
        frozenLockfile: true,
        force: true,
        cacheDir: "cache",
        noCache: true,
        verbose: true,
        noProgress: true,
        noSummary: true,
        noVerify: true,
        offline: true,
        preferOffline: true,
        ignoreScripts: true,
        noEnvFile: true,
        envFile: [".env.test"],
        backend: "copyfile",
        registry: "http://127.0.0.1:9/",
        concurrentScripts: "4",
        networkConcurrency: "48",
        saveTextLockfile: true,
        omit: ["dev", "optional"],
        lockfileOnly: true,
        linker: "isolated",
        minimumReleaseAge: "60 days",
        minimumReleaseAgeExcludes: ["a,b"],
        cpu: ["x64"],
        os: ["!win32"],
        libc: ["glibc"],
        useSystemCa: true,
        filter: ["./packages/*"],
        config: "fnpm.toml",
      },
      [],
    ],
    ["add", { dev: true, exact: true, catalog: "", offline: true }, ["left-pad@1"]],
    ["update", { latest: true, dev: true, noOptional: true, depth: "0", offline: true }, ["left-pad"]],
    ["remove", { global: false, offline: true }, ["left-pad"]],
  ];

  test.each(flagCases)("every %s flag the CLI sends deserializes in the core", async (command, values, positionals) => {
    // No package.json anywhere up the tree, so the core fails after accepting the payload.
    const dir = mkdtempSync(join(root, `${command}-`));
    const options = installOptions(command, values, positionals, dir);
    const error = await rejection(native.install(options));
    expect(error.message).not.toMatch(/^invalid install options/);
  });

  test("invoke resolves with the command's result", async () => {
    const dir = mkdtempSync(join(root, "invoke-"));
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "invoked", version: "2.0.0" }));
    const result = await native.invoke({
      command: ["pkg", "get"],
      cwd: dir,
      positionals: ["name"],
      passthrough: [],
      options: {},
      json: true,
    });
    expect(result.exitCode).toBe(0);
    expect(JSON.stringify(result.data)).toContain("invoked");
  });

  test("rejections carry the core's code", async () => {
    const dir = mkdtempSync(join(root, "codes-"));
    let error = await rejection(
      native.invoke({ command: ["no-such-command"], cwd: dir, positionals: [], passthrough: [], options: {}, json: false }),
    );
    expect(error.code).toBe("USAGE");
    expect(error.message).toContain("no-such-command");

    error = await rejection(native.install({ ...installOptions("install", {}, [], dir), cwd: "relative" }));
    expect(error.code).toBe("USAGE");
    expect(error.message).toMatch(/`cwd` must be an absolute path/);
  });
});
