import { describe, expect, test } from "vitest";
import { plain, testBed } from "../harness.ts";

/**
 * `run`, `exec`, `x` and `create` stop parsing after their first positional,
 * like bun's `stop_after_positional_at`: flags before the script/package are
 * ours, everything after belongs to the child. Unknown-flag rejection happens
 * against the resolved leaf, with a pointer to the command that owns the flag.
 */

describe("passthrough leaves", () => {
  test("flags after the script name go to the script, flags before are ours", async () => {
    const bed = testBed();

    expect(await bed.run(["run", "--silent", "build", "--watch", "--json", "-x"])).toBe(0);

    const request = bed.core.requests[0];
    expect(request?.command).toEqual(["run"]);
    expect(request?.positionals).toEqual(["build"]);
    expect(request?.passthrough).toEqual(["--watch", "--json", "-x"]);
    expect(request?.options.silent).toBe(true);
    // `--json` after the script is the script's, so our output stays human.
    expect(() => JSON.parse(bed.stdout.text())).toThrow();
  });

  test("a user-typed -- forwards everything after it", async () => {
    const bed = testBed();

    await bed.run(["run", "test", "--", "--coverage", "-h"]);

    expect(bed.core.requests[0]?.passthrough).toEqual(["--coverage", "-h"]);
    expect(plain(bed.stdout.text())).not.toContain("Usage:");
  });

  test("--help after the script name is the script's, not ours", async () => {
    const bed = testBed();

    await bed.run(["run", "build", "--help"]);

    expect(bed.core.requests[0]?.passthrough).toEqual(["--help"]);
  });

  test("a value flag before the script keeps its value", async () => {
    const bed = testBed();

    await bed.run(["run", "--filter", "app", "--shell", "system", "dev", "--port", "3000"]);

    const request = bed.core.requests[0];
    expect(request?.options.filter).toEqual(["app"]);
    expect(request?.options.shell).toBe("system");
    expect(request?.positionals).toEqual(["dev"]);
    expect(request?.passthrough).toEqual(["--port", "3000"]);
  });

  test("x hands its arguments to the core unparsed, digits intact", async () => {
    const bed = testBed();

    await bed.run(["x", "-p", "@scope/tool", "tool", "--version", "007"]);

    // The core parses them like bunx: flags before the package are fnpm's, the rest the bin's.
    const request = bed.core.requests[0];
    expect(request?.command).toEqual(["x"]);
    expect(request?.positionals).toEqual([]);
    expect(request?.passthrough).toEqual(["-p", "@scope/tool", "tool", "--version", "007"]);
  });

  test("dlx is x", async () => {
    const bed = testBed();

    await bed.run(["dlx", "cowsay", "hi"]);

    expect(bed.core.requests[0]?.command).toEqual(["x"]);
  });
});

describe("unknown flag rejection", () => {
  test("a sibling's flag is unknown too, like bun", async () => {
    const bed = testBed();

    expect(await bed.run(["pm", "ls", "--max-age", "3"])).toBe(1);

    expect(bed.stderr.text()).toBe(
      "error: fnpm pm has no --max-age flag.\nnote: Run 'fnpm pm --help' for more information\n",
    );
    expect(bed.coreLoads()).toBe(0);
  });

  test("a typo is an error, like bun's unknown_long_flags_are_errors", async () => {
    const bed = testBed();

    expect(await bed.run(["install", "--frozen-lockfil"])).toBe(1);
    expect(bed.stderr.text()).toBe(
      "error: fnpm install has no --frozen-lockfil flag.\nnote: Run 'fnpm install --help' for more information\n",
    );
  });

  test("unknown short flags inside a cluster are caught", async () => {
    const bed = testBed();

    expect(await bed.run(["add", "-dZ", "react"])).toBe(1);
    expect(bed.stderr.text()).toContain("fnpm add has no -Z flag.");
  });

  test("a flag another command has is still unknown", async () => {
    const bed = testBed();

    expect(await bed.run(["doctor", "--latest"])).toBe(1);
    expect(bed.stderr.text()).toContain("error: fnpm doctor has no --latest flag.");
  });

  test("--json failures are a JSON document on stdout", async () => {
    const bed = testBed();

    expect(await bed.run(["pm", "ls", "--max-age", "3", "--json"])).toBe(1);

    expect(bed.json()).toEqual({
      ok: false,
      error: { code: "UNKNOWN_FLAG", message: expect.stringContaining("--max-age") },
    });
    expect(bed.stderr.text()).toBe("");
  });

  test("bun's ignored runtime switches are accepted on package-manager commands", async () => {
    const bed = testBed();

    expect(await bed.run(["install", "--bun", "--watch", "-b", "--smol"])).toBe(0);
    expect(bed.core.installs).toHaveLength(1);
  });

  test("...but not on commands that are not package-manager commands", async () => {
    const bed = testBed();

    expect(await bed.run(["doctor", "--watch"])).toBe(1);
  });
});

describe("global flags before the command", () => {
  test("flags and their values before the path still resolve the command", async () => {
    const bed = testBed();

    expect(await bed.run(["pm", "--cwd", "pkg", "cache", "rm"])).toBe(0);

    const request = bed.core.requests[0];
    expect(request?.command).toEqual(["pm", "cache", "rm"]);
    expect(request?.cwd).toBe("/work/app/pkg");
  });

  test("--json before the command", async () => {
    const bed = testBed();

    expect(await bed.run(["--json", "pm", "whoami"])).toBe(0);
    expect(bed.json().ok).toBe(true);
  });
});
