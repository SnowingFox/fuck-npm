import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { afterAll, describe, expect, test } from "vitest";
import { planInvocation } from "@/cli/plan.ts";
import { FNPM_REGISTRY } from "@/cli/trees/fnpm.ts";
import type { InstallSummary } from "@/core/types.ts";
import { testBed } from "../harness.ts";

/**
 * Differential fuzzing of the native launcher's argv handling against the TS CLI.
 *
 * A seeded generator builds random invocations of the native commands from the command
 * spec (`crates/fnpm-cli/cli-spec.json`): flags in every spelling (`--x v`, `--x=v`,
 * `--no-x`, short clusters), foreign npm/yarn/pnpm flags, bun runtime flags, typos, flags
 * of other commands, positionals and `--`. The TS CLI's result is the reference and is
 * stored in `crates/fnpm-cli/tests/fixtures/cli-fuzz.json`; `crates/fnpm-cli/tests/
 * fixtures.rs` requires the same route, exit code, bytes and InstallOptions natively.
 *
 * Regenerate with `UPDATE_NATIVE_FIXTURES=1 npx vitest run tests/unit/cli/native-fuzz.test.ts`
 * (after a spec change, or to change SEED / COUNT).
 */

const UPDATE = process.env.UPDATE_NATIVE_FIXTURES === "1";
const SEED = 0x5eed_f00d;
const COUNT = 800;
const CWD = "/work/app";
const path = fileURLToPath(new URL("../../../../../crates/fnpm-cli/tests/fixtures/cli-fuzz.json", import.meta.url));
const specPath = fileURLToPath(new URL("../../../../../crates/fnpm-cli/cli-spec.json", import.meta.url));

type SpecFlag = { long: string; short?: string; takesValue: boolean; optionalValue?: boolean };
type SpecNode = { kind: string; name: string; aliases: string[]; flags?: number[]; children?: SpecNode[] };
type Spec = {
  flags: SpecFlag[];
  bins: { fnpm: { root: SpecNode; globalFlags: number[] } };
  foreignFlags: { rewrites: [string, string | null][]; ignoredFlags: string[]; ignoredOptions: string[]; ignoredPrefixes: string[] };
  runtimeFlags: { ignored: string[]; short: string };
};

const spec = JSON.parse(readFileSync(specPath, "utf8")) as Spec;
const NATIVE = ["install", "ci", "add", "remove", "update"];
/** Spellings that route to Node (help, tree, the interactive picker): out of scope here. */
const EXCLUDED_LONG = new Set(["help", "tree", "interactive"]);
const EXCLUDED_SHORT = new Set(["h", "t", "i"]);

const children = spec.bins.fnpm.root.children ?? [];
const nativeNodes = children.filter((child) => NATIVE.includes(child.name));
const commandWords = nativeNodes.flatMap((node) => [node.name, ...node.aliases]);
const flagIndexes = new Set<number>(spec.bins.fnpm.globalFlags);
for (const node of nativeNodes) for (const index of node.flags ?? []) flagIndexes.add(index);
// A few flags of other commands, for the "belongs to" hints.
for (const node of children.filter((child) => ["outdated", "pm", "publish", "ls"].includes(child.name))) {
  for (const index of node.flags ?? []) flagIndexes.add(index);
}
const flags = [...flagIndexes]
  .map((index) => spec.flags[index] as SpecFlag)
  .filter((flag) => !EXCLUDED_LONG.has(flag.long) && !(flag.short !== undefined && EXCLUDED_SHORT.has(flag.short)));
const shorts = [...new Set(flags.flatMap((flag) => (flag.short === undefined ? [] : [flag.short])))];

const VALUES = [
  "x", "", "-", "dev", "optional", "peer", "dev,peer", "0", "1", "12", "70000", "1e3", "true", "false", "maybe",
  "isolated", "hoisted", "clonefile", "a,b", " spaced ", "http://r.example/", "https://u:p@r.example/x?token=t&a=1",
  "ftp://r", "3d", "1.5 hours", "0.0004", "linux", "x64,!arm64", "musl", "*", "sparc", "--force", "-p", "react",
  "../up/./x/", "/abs/../y/", "fnpm.toml", "ünï",
];
const POSITIONALS = ["react", "lodash@^4", "@scope/pkg", "./local", "-", "x", "a b", "--", "github:o/r"];

/** mulberry32. */
function prng(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Terminal and colour surroundings, picked per case. */
const TERMINALS = [
  { stdoutIsTTY: false, stderrIsTTY: false },
  { stdoutIsTTY: true, stderrIsTTY: true },
  { stdoutIsTTY: true, stderrIsTTY: false },
  { stdoutIsTTY: false, stderrIsTTY: true },
];
const ENVS: Record<string, string>[] = [{}, { NO_COLOR: "1" }, { FORCE_COLOR: "1" }, { FORCE_COLOR: "0" }, { TERM: "dumb" }, { CI: "true" }];

type Generated = { args: string[]; terminal: number; env: number };

function generate(random: () => number): Generated[] {
  const pick = <T>(list: readonly T[]): T => list[Math.floor(random() * list.length)] as T;
  const chance = (p: number): boolean => random() < p;

  const flagTokens = (): string[] => {
    const flag = pick(flags);
    const roll = random();
    if (!flag.takesValue) {
      if (roll < 0.15) return [`--no-${flag.long}`];
      if (roll < 0.25) return [`--${flag.long}=${pick(["true", "false", "1", "no", "maybe", ""])}`];
      if (roll < 0.45 && flag.short !== undefined) return [`-${flag.short}`];
      return [`--${flag.long}`];
    }
    if (roll < 0.35) return [`--${flag.long}=${pick(VALUES)}`];
    if (roll < 0.45 && flag.short !== undefined) return [`-${flag.short}${pick(VALUES)}`];
    if (roll < 0.55 && flag.short !== undefined) return [`-${flag.short}`, pick(VALUES)];
    if (roll < 0.62) return [`--${flag.long}`];
    return [`--${flag.long}`, pick(VALUES)];
  };

  const foreignTokens = (): string[] => {
    const roll = random();
    if (roll < 0.45) {
      const [from] = pick(spec.foreignFlags.rewrites);
      const equals = from.indexOf("=");
      if (equals !== -1 && chance(0.4)) return [from.slice(0, equals), from.slice(equals + 1)];
      return [from];
    }
    if (roll < 0.65) {
      const name = pick(spec.foreignFlags.ignoredFlags);
      if (chance(0.2)) return [`--no-${name}`];
      return chance(0.3) ? [`--${name}`, pick(["true", "false", "x"])] : [`--${name}`];
    }
    if (roll < 0.85) {
      const name = pick(spec.foreignFlags.ignoredOptions);
      return chance(0.4) ? [`--${name}=${pick(VALUES)}`] : [`--${name}`, pick(VALUES)];
    }
    return [`--${pick(spec.foreignFlags.ignoredPrefixes)}${pick(["foo=bar", "registry", "x.y=1"])}`];
  };

  const token = (): string[] => {
    const roll = random();
    if (roll < 0.4) return flagTokens();
    if (roll < 0.6) return foreignTokens();
    if (roll < 0.66) return chance(0.7) ? [`--${pick(spec.runtimeFlags.ignored)}`] : [`-${spec.runtimeFlags.short}`];
    if (roll < 0.72) {
      const letters = Array.from({ length: 2 + Math.floor(random() * 2) }, () => pick(shorts));
      return [`-${letters.join("")}`];
    }
    if (roll < 0.76) return [pick(["--bogus", "-Z", "--noSave", "--save-devv", "-", "--=x", "---x"])];
    if (roll < 0.78) return [pick(["-v", "--version"])];
    return [pick(POSITIONALS)];
  };

  const cases: Generated[] = [];
  for (let n = 0; n < COUNT; n += 1) {
    const args: string[] = [];
    if (chance(0.25)) args.push(...flagTokens());
    args.push(pick(commandWords));
    const length = Math.floor(random() * 6);
    for (let index = 0; index < length; index += 1) args.push(...token());
    // Mostly piped and colourless, like CI and scripts.
    const terminal = chance(0.6) ? 0 : Math.floor(random() * TERMINALS.length);
    const env = chance(0.6) ? 0 : Math.floor(random() * ENVS.length);
    cases.push({ args, terminal, env });
  }
  return cases;
}

/** What the native launcher must do, decided by the TS CLI's own planner. */
function route(args: string[]): "native" | "node" {
  const plan = planInvocation(["node", "fnpm", ...args], FNPM_REGISTRY);
  if (plan.node.kind === "group") return plan.wantsVersion ? "native" : "node";
  if (!NATIVE.includes(plan.matched[0] ?? "")) return "node";
  if (plan.wantsHelp || plan.wantsTree) return "node";
  return "native";
}

/** InstallOptions minus the fields equal to the core's serde defaults, to keep the fixture small. */
function compact(options: Record<string, unknown> | null): Record<string, unknown> | null {
  if (options === null) return null;
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(options)) {
    if (value === false && !["save", "verify", "progress", "summary"].includes(key)) continue;
    // `minimumReleaseAgeExcludes` is optional: `[]` and absent differ.
    if (Array.isArray(value) && value.length === 0 && key !== "minimumReleaseAgeExcludes") continue;
    if (key === "logLevel" && value === "default") continue;
    if (key === "updateGroups" && Object.values(value as object).every((flag) => flag === false)) continue;
    out[key] = value;
  }
  return out;
}

const SUMMARY: InstallSummary = {
  added: [{ name: "react", version: "18.3.1" }],
  updated: [],
  removed: ["left-pad"],
  requested: [{ name: "react", version: "18.3.1", bins: [] }],
  installedCount: 1,
  removedCount: 1,
  failedCount: 0,
  blockedScripts: 0,
  elapsedMs: 7,
  noop: false,
} as InstallSummary;

type FuzzCase = Generated & {
  route: "native" | "node";
  exit?: number;
  stdout?: string;
  stderr?: string;
  install?: unknown;
};
type FuzzFile = {
  $comment: string;
  seed: number;
  cwd: string;
  summary: InstallSummary;
  terminals: typeof TERMINALS;
  envs: typeof ENVS;
  cases: FuzzCase[];
};

async function evaluate(input: Generated): Promise<FuzzCase> {
  const where = route(input.args);
  if (where === "node") return { ...input, route: where };
  const bed = testBed({
    cwd: CWD,
    ...TERMINALS[input.terminal],
    env: ENVS[input.env],
    core: { install: () => SUMMARY },
  });
  const exit = await bed.run(input.args);
  const install = (bed.core.installs[0] ?? null) as Record<string, unknown> | null;
  return { ...input, route: where, exit, stdout: bed.stdout.text(), stderr: bed.stderr.text(), install: compact(install) };
}

const stored: FuzzFile | undefined = UPDATE ? undefined : (JSON.parse(readFileSync(path, "utf8")) as FuzzFile);
const generated = generate(prng(SEED));
const results: FuzzCase[] = [];

describe("cli-fuzz.json", () => {
  test("the generator is stable", () => {
    if (UPDATE) return;
    expect(stored?.cases.map(({ args, terminal, env }) => ({ args, terminal, env }))).toEqual(generated);
  });

  test("every case matches the TS CLI", async () => {
    for (const input of generated) results.push(await evaluate(input));
    if (UPDATE) return;
    expect(results).toEqual(stored?.cases);
  });
});

afterAll(() => {
  if (!UPDATE) return;
  const file: FuzzFile = {
    $comment:
      "Generated by packages/fuck-npm/tests/unit/cli/native-fuzz.test.ts (UPDATE_NATIVE_FIXTURES=1); checked by crates/fnpm-cli/tests/fixtures.rs. Do not edit.",
    seed: SEED,
    cwd: CWD,
    summary: SUMMARY,
    terminals: TERMINALS,
    envs: ENVS,
    cases: results,
  };
  // One case per line: small diffs, small file.
  const lines = file.cases.map((entry) => `    ${JSON.stringify(entry)}`);
  const head = JSON.stringify({ ...file, cases: [] }, null, 2).replace(/\[\]\n\}$/, "");
  writeFileSync(path, `${head}[\n${lines.join(",\n")}\n  ]\n}\n`);
});
