import fs from "node:fs";
import os from "node:os";
import { join } from "node:path";
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

    expect(await bed.run(["store", "bogus"])).toBe(1);
    expect(bed.stderr.text()).toContain("Unknown command: fnpm store bogus");
  });

  test("a group's default child runs when the group is bare", async () => {
    const bed = testBed();

    expect(await bed.run(["pm", "cache"])).toBe(0);
    expect(bed.core.requests[0]?.command).toEqual(["pm", "cache", "path"]);
  });

  test("a missing required positional is reported before the core loads", async () => {
    const bed = testBed();

    expect(await bed.run(["exec"])).toBe(1);
    expect(bed.stderr.text()).toContain("`fnpm exec` needs <command>");
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

  test("plain output: header and bun-style summary on stdout", async () => {
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

    // Header, one blank line, rows, as bun prints them (`bun install v…\n` then its tree
    // printer's leading `\n`).
    expect(bed.stderr.text()).toBe("");
    expect(bed.stdout.text()).toMatch(/^fnpm install v\d+\.\d+\.\d+\n\n\+ react@18\.3\.1\n\n3 packages installed \[123\.00ms\]\n$/);
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
      // The first row (react) is under the cursor: Space selects it, Enter confirms.
      keys: " \r",
    });

    expect(await bed.run(["update", "-i", "--latest"])).toBe(0);

    expect(bed.core.requests[0]?.command).toEqual(["outdated"]);
    expect(bed.core.requests).toHaveLength(1);
    expect(bed.core.installs[0]?.interactiveChoices).toEqual([{ name: "react", version: "19.0.0", inRange: false }]);
    expect(bed.core.installs[0]?.packages).toEqual(["react"]);
    expect(bed.core.installs[0]?.latest).toBe(false);
  });

  test("a then Enter selects every row; --dry-run prints the moves", async () => {
    const bed = testBed({
      stdinIsTTY: true,
      core: {
        invoke: () => ({
          exitCode: 0,
          data: {
            packages: [
              { name: "zod", current: "3.22.0", update: "3.23.8", latest: "3.23.8", dependencyType: "devDependencies" },
              { name: "react", current: "18.2.0", update: "18.3.1", latest: "19.0.0", dependencyType: "dependencies" },
            ],
          },
        }),
      },
      keys: "a\r",
    });

    expect(await bed.run(["update", "-i", "--dry-run"])).toBe(0);
    const out = plain(bed.stdout.text());
    expect(out).toContain("Selected 2 packages to update");
    expect(out).toContain("^ react 18.2.0 -> 18.3.1");
    expect(out).toContain("^ zod 3.22.0 -> 3.23.8");
    expect(out).toContain("2 packages would be updated");
    expect(bed.core.installs).toEqual([]);
  });

  test("Ctrl+C cancels", async () => {
    const bed = testBed({
      stdinIsTTY: true,
      core: {
        invoke: () => ({
          exitCode: 0,
          data: { packages: [{ name: "a", current: "1.0.0", update: "1.0.1", latest: "1.0.1", dependencyType: "dependencies" }] },
        }),
      },
      keys: "\x03",
    });

    expect(await bed.run(["update", "-i"])).toBe(1);
    expect(plain(bed.stdout.text())).toContain("Cancelled");
    expect(bed.core.installs).toEqual([]);
  });
});

describe("generic commands", () => {
  test("lines go to stdout in plain mode", async () => {
    const bed = testBed({ core: { invoke: () => ({ exitCode: 0, lines: ["a", "b"] }) } });

    expect(await bed.run(["pm", "hash"])).toBe(0);
    expect(bed.stdout.text()).toBe("a\nb\n");
  });

  test("the cache directory is printed without a newline, like bun", async () => {
    const bed = testBed({ core: { invoke: () => ({ exitCode: 0, lines: ["/home/u/.fnpm/install/cache"] }) } });

    expect(await bed.run(["pm", "cache"])).toBe(0);
    expect(bed.stdout.text()).toBe("/home/u/.fnpm/install/cache");
  });

  test("data becomes the --json payload and the exit code is the core's", async () => {
    const bed = testBed({ core: { invoke: () => ({ exitCode: 1, data: { vulnerabilities: 2 } }) } });

    expect(await bed.run(["outdated", "--json"])).toBe(1);
    expect(bed.json()).toEqual({ ok: false, command: "outdated", vulnerabilities: 2 });
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

test("script paths own help, tree and version arguments", async () => {
  const dir = fs.mkdtempSync(join(os.tmpdir(), "fnpm-script-dispatch-"));
  const result = join(dir, "args.json");
  fs.writeFileSync(join(dir, "cli.cjs"), `require("node:fs").writeFileSync(${JSON.stringify(result)}, JSON.stringify(process.argv.slice(2)));`);
  try {
    const bed = testBed({ cwd: dir });
    expect(await bed.run(["cli.cjs", "--help", "--version", "--tree"])).toBe(0);
    expect(JSON.parse(fs.readFileSync(result, "utf8"))).toEqual(["--help", "--version", "--tree"]);
    expect(bed.stdout.text()).toBe("");
    expect(bed.coreLoads()).toBe(0);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
