import { describe, expect, test } from "vitest";
import { emptySummary } from "@/core/fake.ts";
import { plain, testBed } from "../harness.ts";

describe("dispatch", () => {
  test("bare fnpm prints help and exits 0", async () => {
    const bed = testBed();

    expect(await bed.run([])).toBe(0);
    expect(plain(bed.stdout.text())).toContain("Commands:");
  });

  test("an unknown command fails with a pointer to help", async () => {
    const bed = testBed();

    expect(await bed.run(["instal"])).toBe(1);
    expect(bed.stderr.text()).toContain("Unknown command: instal");
    expect(bed.stderr.text()).toContain("fnpm --help");
  });

  test("an unknown subcommand names the full path", async () => {
    const bed = testBed();

    expect(await bed.run(["pm", "bogus"])).toBe(1);
    expect(bed.stderr.text()).toContain("Unknown command: fnpm pm bogus");
  });

  test("a group's default child runs when the group is bare", async () => {
    const bed = testBed();

    expect(await bed.run(["pm", "cache"])).toBe(0);
    expect(bed.core.requests[0]?.command).toEqual(["pm", "cache", "path"]);
  });

  test("a missing required positional is reported before the core loads", async () => {
    const bed = testBed();

    expect(await bed.run(["why"])).toBe(1);
    expect(bed.stderr.text()).toContain("`fnpm why` needs <package>");
    expect(bed.coreLoads()).toBe(0);
  });

  test("top-level aliases dispatch to their target", async () => {
    const bed = testBed();

    await bed.run(["whoami"]);
    expect(bed.core.requests[0]?.command).toEqual(["pm", "whoami"]);
  });

  test("fnpm-lock commands reach the core under lock", async () => {
    const bed = testBed();

    expect(await bed.run(["migrate", "--from", "pnpm", "--dry-run"], "fnpm-lock")).toBe(0);

    const request = bed.core.requests[0];
    expect(request?.command).toEqual(["lock", "migrate"]);
    expect(request?.options.from).toBe("pnpm");
    expect(request?.options.dryRun).toBe(true);
  });
});

describe("install family", () => {
  test("install sends validated options to core.install", async () => {
    const bed = testBed();

    expect(await bed.run(["install", "--linker", "hoisted", "--network-concurrency", "32", "-p"])).toBe(0);

    const options = bed.core.installs[0];
    expect(options?.command).toBe("install");
    expect(options?.linker).toBe("hoisted");
    expect(options?.networkConcurrency).toBe(32);
    expect(options?.production).toBe(true);
    expect(options?.cwd).toBe("/work/app");
  });

  test("ci is install with a frozen lockfile", async () => {
    const bed = testBed();

    await bed.run(["ci"]);

    expect(bed.core.installs[0]?.command).toBe("install");
    expect(bed.core.installs[0]?.frozenLockfile).toBe(true);
  });

  test.each([
    [["install", "react"]],
    [["i", "-g", "serve"]],
  ])("%j runs add", async (args) => {
    const bed = testBed();

    await bed.run(args);
    expect(bed.core.installs[0]?.command).toBe("add");
  });

  test("add without packages fails like bun", async () => {
    const bed = testBed();

    expect(await bed.run(["add"])).toBe(1);
    expect(bed.coreLoads()).toBe(0);
  });

  test("update --production selects groups instead of omitting dev", async () => {
    const bed = testBed();

    await bed.run(["update", "--production", "--latest"]);

    const options = bed.core.installs[0];
    expect(options?.production).toBe(false);
    expect(options?.updateGroups.prod).toBe(true);
    expect(options?.latest).toBe(true);
  });

  test("plain output: header on stderr, bun-style summary on stdout", async () => {
    const bed = testBed({
      core: {
        install: () =>
          emptySummary({
            added: [{ name: "react", version: "18.3.1" }],
            installedCount: 3,
            elapsedMs: 123,
          }),
      },
    });

    expect(await bed.run(["install"])).toBe(0);

    expect(bed.stderr.text()).toMatch(/^fnpm install v\d+\.\d+\.\d+\n\n$/);
    expect(bed.stdout.text()).toBe("\n+ react@18.3.1\n\n3 packages installed [123.00ms]\n");
  });

  test("--json prints only the summary document", async () => {
    const bed = testBed({ stdoutIsTTY: true, stderrIsTTY: true });

    expect(await bed.run(["add", "zod", "--json"])).toBe(0);

    const payload = bed.json();
    expect(payload.ok).toBe(true);
    expect(payload.command).toBe("add");
    expect(bed.stderr.text()).toBe("");
  });

  test("--silent prints nothing at all", async () => {
    const bed = testBed();

    expect(await bed.run(["install", "--silent"])).toBe(0);
    expect(bed.stdout.text()).toBe("");
    expect(bed.stderr.text()).toBe("");
  });

  test("failed packages make the exit code 1", async () => {
    const bed = testBed({ core: { install: () => emptySummary({ failedCount: 2 }) } });

    expect(await bed.run(["install"])).toBe(1);
    expect(bed.stdout.text()).toContain("Failed to install 2 packages");
  });

  test("a rich TTY gets a progress line on stderr, erased before the summary", async () => {
    const bed = testBed({
      stdoutIsTTY: true,
      stderrIsTTY: true,
      core: {
        install: (_options, emit) => {
          emit({ type: "resolve", resolved: 3, total: 10 });
          return emptySummary({ installedCount: 1, elapsedMs: 5 });
        },
      },
    });

    await bed.run(["install"]);

    const stderr = bed.stderr.text();
    expect(plain(stderr)).toContain("Resolving 3/10");
    expect(stderr.endsWith("\r\u001B[2K")).toBe(true);
  });

  test("warnings reach stderr even without a progress line", async () => {
    const bed = testBed({
      core: {
        install: (_options, emit) => {
          emit({ type: "warn", message: "peer dependency mismatch" });
          return emptySummary();
        },
      },
    });

    await bed.run(["install"]);
    expect(plain(bed.stderr.text())).toContain("warn: peer dependency mismatch");
  });
});

describe("update --interactive", () => {
  test("refuses without a terminal and names the scripted alternative", async () => {
    const bed = testBed();

    expect(await bed.run(["update", "-i"])).toBe(1);
    expect(bed.stderr.text()).toContain("needs a terminal");
  });

  test("updates exactly the picked packages", async () => {
    const bed = testBed({
      stdoutIsTTY: true,
      stdinIsTTY: true,
      core: {
        invoke: () => ({
          exitCode: 0,
          data: {
            packages: [
              { name: "react", current: "18.2.0", update: "18.3.1", latest: "19.0.0", dependencyType: "dependencies" },
              { name: "zod", current: "3.22.0", update: "3.23.8", latest: "3.23.8", dependencyType: "dependencies" },
            ],
          },
        }),
      },
      prompts: { multiselect: async () => ["react"] as never },
    });

    expect(await bed.run(["update", "-i", "--latest"])).toBe(0);

    expect(bed.core.requests[0]?.command).toEqual(["outdated"]);
    expect(bed.core.installs[0]?.packages).toEqual(["react@19.0.0"]);
  });
});

describe("generic commands", () => {
  test("lines go to stdout in plain mode", async () => {
    const bed = testBed({ core: { invoke: () => ({ exitCode: 0, lines: ["/home/u/.fnpm/cache"] }) } });

    expect(await bed.run(["pm", "cache"])).toBe(0);
    expect(bed.stdout.text()).toBe("/home/u/.fnpm/cache\n");
  });

  test("data becomes the --json payload and the exit code is the core's", async () => {
    const bed = testBed({ core: { invoke: () => ({ exitCode: 1, data: { vulnerabilities: 2 } }) } });

    expect(await bed.run(["audit", "--json"])).toBe(1);
    expect(bed.json()).toEqual({ ok: false, command: "audit", vulnerabilities: 2 });
  });

  test("link and unlink never run globally and only save when asked", async () => {
    const bed = testBed();

    await bed.run(["link", "-g", "pkg"]);

    expect(bed.core.requests[0]?.options.global).toBe(false);
    expect(bed.core.requests[0]?.options.save).toBe(false);
  });

  test("prune refuses --global", async () => {
    const bed = testBed();

    expect(await bed.run(["prune", "-g"])).toBe(1);
    expect(bed.stderr.text()).toContain("--global cannot be used with fnpm prune");
  });

  test("a missing native core is a clear error, and honours --json", async () => {
    const bed = testBed({ core: null });

    expect(await bed.run(["install", "--json"])).toBe(1);
    expect(bed.json()).toEqual({
      ok: false,
      error: { code: "CORE_UNAVAILABLE", message: expect.stringContaining("@fuck-npm/binding") },
    });
  });

  test("core errors surface their message without a stack", async () => {
    const { CoreError } = await import("@/errors.ts");
    const bed = testBed({
      core: {
        invoke: () => {
          throw new CoreError("lockfile is corrupt", "LOCKFILE_INVALID");
        },
      },
    });

    expect(await bed.run(["pm", "hash"])).toBe(1);
    expect(bed.stderr.text()).toBe("error: lockfile is corrupt\n");
  });
});
