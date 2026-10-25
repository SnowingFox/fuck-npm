import { describe, expect, test } from "vitest";
import { stripRuntimeFlags, translateForeignFlags } from "@/cli/foreign-flags.ts";
import { findCommandNode } from "@/cli/registry.ts";
import type { CommandLeaf } from "@/cli/registry-types.ts";
import { FNPM_REGISTRY } from "@/cli/trees/fnpm.ts";
import { testBed } from "../harness.ts";

const leaf = (name: string): CommandLeaf => findCommandNode([name], FNPM_REGISTRY) as CommandLeaf;
const t = (name: string, tokens: string[]) => translateForeignFlags(tokens, leaf(name));

describe("FOREIGN_FLAGS aliases", () => {
  test.each([
    [["--save-dev"], ["--dev"]],
    [["--save-exact"], ["--exact"]],
    [["--save-optional"], ["--optional"]],
    [["--save-peer"], ["--peer"]],
    [["--save-catalog"], ["--catalog"]],
    [["--location=global"], ["--global"]],
    [["--location", "global"], ["--global"]],
    [["--no-optional"], ["--omit=optional"]],
    [["--ignore-optional"], ["--omit=optional"]],
    [["--only=prod"], ["--omit=dev"]],
    [["--only", "production"], ["--omit=dev"]],
    [["--production=true"], ["--production"]],
    [["--package-lock-only"], ["--lockfile-only"]],
    [["--immutable"], ["--frozen-lockfile"]],
    [["--shamefully-hoist"], ["--linker=hoisted"]],
    [["--progress=false"], ["--no-progress"]],
    [["--loglevel=silent"], ["--silent"]],
    [["--loglevel", "error"], ["--quiet"]],
    [["--loglevel=warn"], ["--quiet"]],
    [["--loglevel=verbose"], ["--verbose"]],
    [["--install-strategy=linked"], ["--linker=isolated"]],
    [["--node-linker=pnpm"], ["--linker=isolated"]],
    [["--node-linker", "node-modules"], ["--linker=hoisted"]],
  ])("install %j → %j", (input, output) => {
    expect(t("install", input)).toEqual(output);
  });

  test.each([
    ["--loglevel=info"], ["--no-audit"], ["--audit=false"], ["--no-fund"], ["--fund=false"],
    ["--no-update-notifier"], ["--legacy-peer-deps"], ["--ignore-engines"], ["--unsafe-perm"],
    ["--non-interactive"], ["--prefer-frozen-lockfile"],
  ])("install %s is satisfied and dropped", (flag) => {
    expect(t("install", [flag, "react"])).toEqual(["react"]);
  });

  test("--save-* is dropped on a command that adds nothing", () => {
    expect(t("remove", ["--save-dev", "react"])).toEqual(["react"]);
  });

  test("update does not alias to its --dev/--production filters", () => {
    expect(t("update", ["--save-dev", "react"])).toEqual(["react"]);
    // update has its own --no-optional, so the table must not touch it.
    expect(t("update", ["--no-optional"])).toEqual(["--no-optional"]);
  });

  test("the command's own flags pass through untouched", () => {
    expect(t("install", ["--frozen-lockfile", "--omit", "dev", "--json"])).toEqual([
      "--frozen-lockfile",
      "--omit",
      "dev",
      "--json",
    ]);
  });
});

describe("IGNORED_FOREIGN_FLAGS", () => {
  test("boolean flags are skipped, with an optional true/false", () => {
    expect(t("install", ["--prefer-online", "--strict-ssl", "false", "--no-package-lock", "react"])).toEqual(["react"]);
  });

  test("options are skipped with their value unless the value is a flag", () => {
    expect(t("add", ["--cache-folder", "/tmp/c", "--tag", "beta", "react"])).toEqual(["react"]);
    expect(t("add", ["--userconfig", "--dev", "react"])).toEqual(["--dev", "react"]);
  });

  test("pnpm's --config.<x>=<y> is skipped", () => {
    expect(t("install", ["--config.store-dir=/s"])).toEqual([]);
  });

  test("unknown flags are left for the unknown-flag check", () => {
    expect(t("install", ["--frozen-lockfil"])).toEqual(["--frozen-lockfil"]);
  });

  test("commands without translation keep every token", () => {
    expect(translateForeignFlags(["--save-dev"], leaf("outdated"))).toEqual(["--save-dev"]);
  });
});

describe("runtime switches", () => {
  test("are dropped unless the leaf declares them", () => {
    expect(stripRuntimeFlags(["--bun", "-b", "--hot", "--frozen-lockfile"], leaf("install"))).toEqual([
      "--frozen-lockfile",
    ]);
    expect(stripRuntimeFlags(["--bun"], leaf("run"))).toEqual(["--bun"]);
  });
});

describe("end to end", () => {
  test("npm ci-style flags reach the core translated", async () => {
    const bed = testBed();

    expect(await bed.run(["install", "--immutable", "--no-optional", "--loglevel", "error", "--no-fund"])).toBe(0);

    const options = bed.core.installs[0];
    expect(options?.frozenLockfile).toBe(true);
    expect(options?.omit).toEqual(["optional"]);
    expect(options?.logLevel).toBe("quiet");
  });

  test("npm add --save-dev lands in devDependencies", async () => {
    const bed = testBed();

    await bed.run(["add", "--save-dev", "--save-exact", "typescript"]);

    expect(bed.core.installs[0]?.group).toBe("devDependencies");
    expect(bed.core.installs[0]?.exact).toBe(true);
  });
});
