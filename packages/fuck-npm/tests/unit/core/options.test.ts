import { describe, expect, test } from "vitest";
import { installOptions, parseReleaseAge, pmOptions } from "@/core/options.ts";
import { testBed } from "../harness.ts";

const pm = (values: Record<string, boolean | string | string[]>) => pmOptions(values, "/start");

describe("bun's flag validation messages", () => {
  test.each([
    [{ linker: "pnpm" }, `Invalid value for --linker: "pnpm". Must be 'isolated' or 'hoisted'.`],
    [{ omit: ["dev", "bogus"] }, "invalid `omit` value: 'bogus'"],
    [{ networkConcurrency: "70000" }, "Expected --network-concurrency to be a number between 0 and 65535: 70000"],
    [{ networkConcurrency: "abc" }, "Expected --network-concurrency to be a number between 0 and 65535: abc"],
    [{ minimumReleaseAge: "soon" }, "Expected --minimum-release-age to be a positive number: soon"],
    [{ cpu: ["sparc"] }, "invalid cpu: 'sparc'. valid values: *, any, arm, arm64"],
    [{ os: ["!beos"] }, "invalid os: '!beos'"],
    [{ libc: ["uclibc"] }, "invalid libc: 'uclibc'"],
    [{ production: true, trust: true }, "'--production' and '--trust'"],
    [{ frozenLockfile: true, trust: true }, "'--frozen-lockfile' and '--trust'"],
  ])("%j", (values, message) => {
    expect(() => pm(values)).toThrow(message);
  });

  test("a bad registry URL is rejected with credentials redacted", () => {
    expect(() => pm({ registry: "ftp://user:secret@host" })).toThrow(
      `Registry URL must start with 'https://' or 'http://': "ftp://***@host"`,
    );
  });
});

describe("normalization", () => {
  test("defaults", () => {
    const options = pm({});

    expect(options).toMatchObject({
      cwd: "/start",
      save: true,
      logLevel: "default",
      progress: true,
      summary: true,
      verify: true,
      omit: [],
    });
    expect(options.linker).toBeUndefined();
  });

  test("log level precedence: silent > verbose > quiet", () => {
    expect(pm({ quiet: true, verbose: true }).logLevel).toBe("verbose");
    expect(pm({ quiet: true, verbose: true, silent: true }).logLevel).toBe("silent");
  });

  test("--cwd resolves against the starting directory", () => {
    expect(pm({ cwd: "../other" }).cwd).toBe("/other");
    expect(pm({ cwd: "/abs" }).cwd).toBe("/abs");
  });

  test("an unknown --backend is ignored, like bun", () => {
    expect(pm({ backend: "reflink" }).backend).toBeUndefined();
    expect(pm({ backend: "hardlink" }).backend).toBe("hardlink");
  });

  test("--concurrent-scripts 0 means the default", () => {
    expect(pm({ concurrentScripts: "0" }).concurrentScripts).toBeUndefined();
    expect(pm({ concurrentScripts: "8" }).concurrentScripts).toBe(8);
  });

  test("--omit and the platform lists accept commas and repeats", () => {
    const options = pm({ omit: ["dev,peer", "dev"], cpu: ["x64,!arm64"], os: ["*"] });

    expect(options.omit).toEqual(["dev", "peer"]);
    expect(options.cpu).toEqual(["x64", "!arm64"]);
    expect(options.os).toEqual(["*"]);
  });

  test("--minimum-release-age-excludes splits commas", () => {
    expect(pm({ minimumReleaseAgeExcludes: ["a,b", "c"] }).minimumReleaseAgeExcludes).toEqual(["a", "b", "c"]);
  });

  test.each([
    ["60", 60_000],
    ["90m", 5_400_000],
    ["12 hours", 43_200_000],
    ["3d", 259_200_000],
    ["2 weeks", 1_209_600_000],
    ["0", undefined],
  ])("parseReleaseAge(%s) = %s ms", (input, ms) => {
    expect(parseReleaseAge(input)).toBe(ms);
  });
});

describe("installOptions conflicts", () => {
  test("--filter with --global", () => {
    expect(() => installOptions("add", { global: true, filter: ["app"] }, ["x"], "/s")).toThrow(
      "--filter cannot be used with --global",
    );
  });

  test("--catalog with --global", () => {
    expect(() => installOptions("add", { global: true, catalog: true }, ["x"], "/s")).toThrow(
      "--catalog cannot be used with --global",
    );
  });

  test("--analyze needs files", () => {
    expect(() => installOptions("install", { analyze: true }, [], "/s")).toThrow("Missing script(s) to analyze");
  });

  test("update --depth only accepts 0", () => {
    expect(() => installOptions("update", { depth: "1" }, [], "/s")).toThrow("--depth only accepts 0");
    expect(installOptions("update", { depth: "0" }, [], "/s").depth).toBe(0);
  });

  test("group precedence is dev > optional > peer > dependencies", () => {
    expect(installOptions("add", { optional: true, dev: true }, ["x"], "/s").group).toBe("devDependencies");
    expect(installOptions("add", { peer: true, optional: true }, ["x"], "/s").group).toBe("optionalDependencies");
    expect(installOptions("add", { saveProd: true }, ["x"], "/s").group).toBe("dependencies");
    expect(installOptions("add", {}, ["x"], "/s").group).toBeUndefined();
  });

  test("--catalog bare is the default catalog; =NAME is trimmed", () => {
    expect(installOptions("add", { catalog: true }, ["x"], "/s").catalog).toBe("");
    expect(installOptions("add", { catalog: " r19 " }, ["x"], "/s").catalog).toBe("r19");
  });

  test("--global silently disables --recursive", () => {
    expect(installOptions("update", { global: true, recursive: true }, [], "/s").recursive).toBe(false);
  });
});

describe("pm subcommand values", () => {
  test.each([
    [["pm", "diff", "--unified", "x"], "invalid --unified value: x"],
    [["pm", "cache", "prune", "--max-age", "-1"], "`--max-age` requires a value."],
    [["pm", "cache", "prune", "--max-age", "ten"], "invalid --max-age value: ten"],
    [["why", "react", "--depth", "deep"], "invalid depth value: deep"],
    [["pm", "version", "--git-tag-version", "maybe"], "invalid --git-tag-version value: maybe"],
    [["run", "--parallel", "--sequential", "a"], "--parallel and --sequential cannot be used together"],
  ])("%j", async (args, message) => {
    const bed = testBed();

    expect(await bed.run(args)).toBe(1);
    expect(bed.stderr.text()).toContain(message);
  });

  test("--no-git-tag-version becomes gitTagVersion: false", async () => {
    const bed = testBed();

    await bed.run(["pm", "version", "patch", "--no-git-tag-version"]);
    expect(bed.core.requests[0]?.options.gitTagVersion).toBe(false);
  });
});
