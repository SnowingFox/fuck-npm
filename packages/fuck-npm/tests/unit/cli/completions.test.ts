import { spawnSync } from "node:child_process";
import { describe, expect, test } from "vitest";
import { bashScript, completionEntries, fishScript } from "@/commands/completions.ts";
import { FNPM_REGISTRY } from "@/cli/trees/fnpm.ts";
import { testBed } from "../harness.ts";

describe("completions", () => {
  test("entries come from the registry, aliases included", () => {
    const entries = completionEntries(FNPM_REGISTRY);
    const root = entries.find((entry) => entry.path.length === 0);
    const cache = entries.find((entry) => entry.path.join(" ") === "pm cache");
    const add = entries.find((entry) => entry.path.join(" ") === "add");

    expect(root?.subcommands).toEqual(expect.arrayContaining(["install", "i", "pm", "whoami"]));
    expect(cache?.subcommands).toEqual(["path", "rm", "prune", "pack", "unpack"]);
    expect(add?.flags).toEqual(expect.arrayContaining(["--dev", "-d", "--exact", "--json"]));
  });

  test("the bash script is valid bash", () => {
    const check = spawnSync("bash", ["-n"], { input: bashScript(FNPM_REGISTRY), encoding: "utf-8" });
    if (check.error) return; // no bash on this machine
    expect(check.stderr).toBe("");
    expect(check.status).toBe(0);
  });

  test("fish completes subcommands under their parent", () => {
    expect(fishScript(FNPM_REGISTRY)).toContain("complete -c fnpm -n '__fish_seen_subcommand_from cache' -a 'path rm prune pack unpack'");
  });

  test("fnpm completions picks the shell from $SHELL", async () => {
    const bed = testBed({ env: { SHELL: "/usr/bin/zsh" } });

    expect(await bed.run(["completions"])).toBe(0);
    expect(bed.stdout.text()).toContain("bashcompinit");
    expect(bed.coreLoads()).toBe(0);
  });

  test("an unknown shell is a usage error", async () => {
    const bed = testBed();

    expect(await bed.run(["completions", "tcsh"])).toBe(1);
    expect(bed.stderr.text()).toContain('Unsupported shell "tcsh"');
  });
});
