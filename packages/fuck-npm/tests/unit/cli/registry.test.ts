import { describe, expect, test } from "vitest";
import {
  acceptedFlagNames,
  allowedFlagNames,
  collectFlags,
  findCommandNode,
  longName,
  resolveCommandPath,
  visibleChildren,
} from "@/cli/registry.ts";
import type { CommandGroup, CommandLeaf, CommandNode, Registry } from "@/cli/registry-types.ts";
import { FNPM_LOCK_REGISTRY } from "@/cli/trees/fnpm-lock.ts";
import { FNPM_REGISTRY } from "@/cli/trees/fnpm.ts";

const R = FNPM_REGISTRY;

const leafAt = (path: string[], registry: Registry = R): CommandLeaf => {
  const node = findCommandNode(path, registry);
  expect(node?.kind, path.join(" ")).toBe("leaf");
  return node as CommandLeaf;
};

const flagNames = (leaf: CommandLeaf): string[] => leaf.flags.map((spec) => longName(spec.raw));

describe("command tree lookup", () => {
  test("walks down to a nested leaf", () => {
    expect(leafAt(["pm", "cache", "rm"]).name).toBe("rm");
  });

  test("returns undefined for a path the tree cannot consume", () => {
    expect(findCommandNode(["pm", "bogus"], R)).toBeUndefined();
    expect(findCommandNode(["nope"], R)).toBeUndefined();
  });

  test("reports the deepest match plus the leftovers", () => {
    const resolved = resolveCommandPath(["pm", "bogus", "x"], R);

    expect(resolved.node.name).toBe("pm");
    expect(resolved.matched).toEqual(["pm"]);
    expect(resolved.rest).toEqual(["bogus", "x"]);
  });

  test.each([
    ["i", "install"],
    ["a", "add"],
    ["rm", "remove"],
    ["r", "remove"],
    ["uninstall", "remove"],
    ["up", "update"],
    ["list", "ls"],
    ["view", "info"],
    ["dlx", "x"],
  ])("segment alias %s resolves to %s", (alias, name) => {
    expect(findCommandNode([alias], R)?.name).toBe(name);
  });

  test("a top-level alias expands into its target path", () => {
    const resolved = resolveCommandPath(["whoami"], R);

    expect(resolved.matched).toEqual(["pm", "whoami"]);
    expect(resolved.viaAlias?.name).toBe("whoami");
  });
});

describe("bun command parity", () => {
  test.each([
    ["install"], ["ci"], ["add"], ["remove"], ["update"], ["outdated"], ["why"], ["ls"],
    ["info"], ["link"], ["unlink"], ["patch"], ["patch-commit"], ["prune"], ["dedupe"],
    ["audit"], ["pack"], ["publish"], ["run"], ["exec"], ["x"], ["init"], ["create"],
    ["pm", "bin"], ["pm", "ls"], ["pm", "why"], ["pm", "hash"], ["pm", "hash-string"],
    ["pm", "hash-print"], ["pm", "cache", "path"], ["pm", "cache", "rm"],
    ["pm", "cache", "prune"], ["pm", "cache", "pack"], ["pm", "cache", "unpack"],
    ["pm", "migrate"], ["pm", "untrusted"], ["pm", "trust"], ["pm", "default-trusted"],
    ["pm", "version"], ["pm", "pkg"], ["pm", "whoami"], ["pm", "licenses"], ["pm", "view"],
    ["pm", "pack"], ["pm", "diff"], ["pm", "scan"], ["pm", "fetch"],
    ["store", "path"], ["store", "prune"], ["store", "status"], ["doctor"], ["completions"],
  ])("fnpm %s is registered", (...path) => {
    expect(findCommandNode(path, R)?.kind).toBe("leaf");
  });

  test.each([["migrate"], ["export"], ["verify"], ["diff"], ["why"], ["print"], ["dedupe"], ["audit"]])(
    "fnpm-lock %s is registered",
    (name) => {
      expect(findCommandNode([name], FNPM_LOCK_REGISTRY)?.kind).toBe("leaf");
    },
  );

  // bun's SHARED_PARAMS, minus -h (global here).
  const SHARED = [
    "config", "yarn", "production", "prod", "no-save", "save", "ca", "cafile", "dry-run",
    "frozen-lockfile", "force", "cache-dir", "no-cache", "silent", "quiet", "verbose",
    "no-progress", "no-summary", "no-verify", "offline", "prefer-offline", "ignore-scripts",
    "trust", "global", "cwd", "env-file", "no-env-file", "backend", "registry",
    "concurrent-scripts", "network-concurrency", "save-text-lockfile", "omit", "lockfile-only",
    "linker", "minimum-release-age", "minimum-release-age-excludes", "cpu", "os", "libc",
    "use-system-ca", "use-openssl-ca", "use-bundled-ca",
  ];

  test.each([
    ["install"], ["add"], ["remove"], ["update"], ["outdated"], ["link"], ["unlink"], ["patch"],
    ["patch-commit"], ["audit"], ["info"], ["pack"], ["publish"], ["why"], ["dedupe"], ["prune"],
    ["pm", "cache", "rm"], ["pm", "version"],
  ])("fnpm %s accepts every shared package-manager flag", (...path) => {
    expect(flagNames(leafAt(path))).toEqual(expect.arrayContaining(SHARED));
  });

  test("add-only and update-only flags", () => {
    expect(flagNames(leafAt(["add"]))).toEqual(
      expect.arrayContaining(["dev", "development", "optional", "peer", "save-prod", "exact", "filter", "analyze", "only-missing", "catalog"]),
    );
    expect(flagNames(leafAt(["update"]))).toEqual(
      expect.arrayContaining(["latest", "interactive", "filter", "recursive", "dev", "no-optional", "depth", "exact"]),
    );
    expect(flagNames(leafAt(["remove"]))).not.toContain("dev");
  });

  test("bun pm per-subcommand flags stay on their subcommand", () => {
    expect(flagNames(leafAt(["pm", "cache", "prune"]))).toContain("max-age");
    expect(flagNames(leafAt(["pm", "ls"]))).not.toContain("max-age");
    expect(flagNames(leafAt(["pm", "version"]))).toEqual(
      expect.arrayContaining(["git-tag-version", "no-git-tag-version", "allow-same-version", "message", "preid"]),
    );
    expect(flagNames(leafAt(["pm", "diff"]))).toEqual(
      expect.arrayContaining(["diff", "raw", "unformatted", "unminify", "minify", "ignore-space", "name-only", "stat", "unified"]),
    );
  });

  test("run, exec, x and create forward everything after their first positional", () => {
    for (const name of ["run", "exec", "x", "create"]) {
      expect(leafAt([name]).passthrough).toBe(1);
    }
  });

  test("foreign flag translation is on exactly the install family", () => {
    const translated = visibleChildren(R.root)
      .filter((node): node is CommandLeaf => node.kind === "leaf" && node.foreignFlags !== undefined)
      .map((node) => node.name);
    expect(translated.sort()).toEqual(["add", "ci", "install", "remove", "update"]);
    expect(leafAt(["update"]).foreignFlags).toBe("update");
  });
});

describe("flag collection", () => {
  test("a group exposes the union of its children's flags, deduped by key", () => {
    const pm = findCommandNode(["pm"], R) as CommandGroup;
    const keys = collectFlags(pm).map((spec) => spec.key);

    expect(keys).toContain("maxAge");
    expect(keys).toContain("gitTagVersion");
    expect(keys.length).toBe(new Set(keys).size);
  });

  test("allowedFlagNames merges node flags with the globals, plus --no-<boolean>", () => {
    const names = allowedFlagNames(leafAt(["add"]), R);

    expect(names.has("dev")).toBe(true);
    expect(names.has("no-dev")).toBe(true);
    expect(names.has("cache-dir")).toBe(true);
    expect(names.has("cacheDir")).toBe(true);
    expect(names.has("json")).toBe(true);
    expect(names.has("h")).toBe(true);
    expect(names.has("tree")).toBe(true);
    expect(names.has("no-cache-dir")).toBe(false);
  });
});

describe("acceptedFlagNames", () => {
  test("splits short and long spellings", () => {
    expect([...acceptedFlagNames("-h, --help")].sort()).toEqual(["h", "help"]);
  });

  test("accepts both spellings of a negated flag", () => {
    const names = acceptedFlagNames("--no-save");

    expect(names.has("no-save")).toBe(true);
    expect(names.has("save")).toBe(true);
  });

  test("strips value placeholders and adds the camelCase key", () => {
    const names = acceptedFlagNames("--network-concurrency [n]");

    expect(names.has("network-concurrency")).toBe(true);
    expect(names.has("networkConcurrency")).toBe(true);
    expect(names.has("[n]")).toBe(false);
  });
});

describe("registry data", () => {
  test.each([["fnpm", R], ["fnpm-lock", FNPM_LOCK_REGISTRY]] as const)(
    "%s: every node has a name and description, and value flags use [brackets]",
    (_bin, registry) => {
      const walk = (node: CommandNode): void => {
        expect(node.name).not.toBe("");
        expect(node.description).not.toBe("");
        if (node.kind === "group") {
          node.children.forEach(walk);
          return;
        }
        for (const spec of node.flags) {
          // <angles> make cac's negation default trip its required-value check.
          expect(spec.raw, spec.raw).not.toMatch(/</);
          expect(spec.description, spec.raw).not.toBe("");
        }
      };
      registry.root.children.forEach(walk);
    },
  );

  test("short flags are unambiguous within each leaf", () => {
    const walk = (node: CommandNode, path: string[]): void => {
      if (node.kind === "group") {
        for (const child of node.children) walk(child, [...path, child.name]);
        return;
      }
      const shorts = [...node.flags, ...R.globalFlags]
        .map((spec) => /(?:^|,\s*)-([^-\s,])\b/.exec(spec.raw)?.[1])
        .filter((letter): letter is string => letter !== undefined);
      expect(shorts.length, path.join(" ")).toBe(new Set(shorts).size);
    };
    walk(R.root, []);
  });
});
