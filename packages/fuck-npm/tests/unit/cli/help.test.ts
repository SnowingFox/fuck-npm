import { describe, expect, test } from "vitest";
import { renderHelp, renderTree } from "@/cli/help.ts";
import { FNPM_LOCK_REGISTRY } from "@/cli/trees/fnpm-lock.ts";
import { FNPM_REGISTRY } from "@/cli/trees/fnpm.ts";
import { plain, testBed } from "../harness.ts";

const R = FNPM_REGISTRY;

describe("renderHelp", () => {
  test("root help lists commands, aliases and the detailed-help pointers", () => {
    const help = plain(renderHelp(R));

    expect(help).toContain("fnpm <command> [...args]");
    expect(help).toContain("install, i");
    expect(help).toContain("whoami");
    for (const [command] of R.detailedHelp) {
      expect(help).toContain(command);
    }
  });

  test("leaf help renders usage, arguments, flags, examples and notes", () => {
    const help = plain(renderHelp(R, "install"));

    expect(help).toContain("fnpm install");
    expect(help).toContain("Aliases: i");
    expect(help).toContain("--frozen-lockfile");
    expect(help).toContain("Examples:");
    expect(help).toContain("Notes:");
    // Hidden flags stay out of help.
    expect(help).not.toContain("-P, --prod");
  });

  test("group help lists subcommands and only their specific flags", () => {
    const help = plain(renderHelp(R, "pm"));

    expect(help).toContain("fnpm pm <subcommand> [...args]");
    expect(help).toContain("cache <subcommand>");
    expect(help).toContain("pm version");
    expect(help).toContain("--git-tag-version");
    expect(help).not.toContain("--network-concurrency");
  });

  test("a group's default child is marked", () => {
    expect(plain(renderHelp(R, "pm cache"))).toMatch(/path\s+Print the cache directory \(default\)/);
  });

  test("an alias renders the help of its target", () => {
    expect(plain(renderHelp(R, "whoami"))).toContain("fnpm pm whoami");
    expect(plain(renderHelp(R, "i"))).toContain("fnpm install");
  });

  test("an unknown command falls back to root help", () => {
    expect(plain(renderHelp(R, "definitely-not-a-command"))).toContain("Commands:");
  });

  test("fnpm-lock has its own root", () => {
    const help = plain(renderHelp(FNPM_LOCK_REGISTRY));

    expect(help).toContain("fnpm-lock <command> [...args]");
    expect(help).toContain("migrate");
    expect(help).not.toContain("install, i");
  });

  test("renders without escape codes unless colours are passed", () => {
    expect(renderHelp(R)).not.toContain("\u001B[");
  });
});

describe("renderTree", () => {
  test("matches the registered command surface", () => {
    // Deliberate snapshot: a new command must update it.
    expect(plain(renderTree(R))).toMatchSnapshot();
    expect(plain(renderTree(FNPM_LOCK_REGISTRY))).toMatchSnapshot();
  });
});

describe("help and tree entry points", () => {
  test.each([
    [["help", "--tree"]],
    [["help", "-t"]],
    [["-h", "--tree"]],
    [["--tree"]],
    [["-t"]],
  ])("%j prints the tree", async (args) => {
    const bed = testBed();

    expect(await bed.run(args)).toBe(0);
    const output = plain(bed.stdout.text());
    expect(output).toContain("├── install");
    expect(output).toContain("└── completions");
  });

  test.each([
    [["add", "-h"], "fnpm add"],
    [["add", "react", "--help"], "fnpm add"],
    [["help", "pm", "cache", "rm"], "fnpm pm cache rm"],
    [["pm", "cache", "rm", "-h"], "fnpm pm cache rm"],
    [["pm"], "Usage: fnpm pm [flags] [<command>]"],
    [[], "fnpm <command>"],
  ])("%j prints %s help without loading the core", async (args, expected) => {
    const bed = testBed();

    expect(await bed.run(args)).toBe(0);
    expect(plain(bed.stdout.text())).toContain(expected);
    expect(bed.coreLoads()).toBe(0);
  });

  test("help output has no escape codes when FORCE_COLOR=0, even on a TTY", async () => {
    const bed = testBed({ stdoutIsTTY: true, env: { FORCE_COLOR: "0" } });

    await bed.run(["--help"]);
    expect(bed.stdout.text()).not.toContain("\u001B[");
  });

  test("help is coloured on a TTY", async () => {
    const bed = testBed({ stdoutIsTTY: true });

    await bed.run(["--help"]);
    expect(bed.stdout.text()).toContain("\u001B[");
  });
});
