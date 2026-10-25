import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  FOREIGN_FLAGS,
  IGNORED_FOREIGN_FLAGS,
  IGNORED_FOREIGN_OPTIONS,
  IGNORED_FOREIGN_PREFIXES,
  UPDATE_FILTERS,
} from "../src/cli/foreign-flags.ts";
import { longName, shortName, takesValue } from "../src/cli/registry.ts";
import type { CommandNode, FlagSpec, Registry } from "../src/cli/registry-types.ts";
import { IGNORED_RUNTIME_FLAGS, IGNORED_RUNTIME_SHORT } from "../src/cli/trees/flags.ts";
import { FNPM_LOCK_REGISTRY } from "../src/cli/trees/fnpm-lock.ts";
import { FNPM_REGISTRY } from "../src/cli/trees/fnpm.ts";

/**
 * Serialise the command registry to `crates/fnpm-cli/cli-spec.json`, the
 * compile-time input of the native launcher (`crates/fnpm-cli/build.rs`).
 *
 * The tree stays defined once, in `src/cli/trees/*.ts`; this file only
 * flattens it to data. Flags are deduplicated into one table (most leaves
 * share bun's SHARED_PARAMS) and nodes refer to them by index. Everything the
 * native parser needs to reproduce `plan.ts` and `parse-flags.ts` is here:
 * names, aliases, value arity, repeatable/optional values, passthrough and
 * foreign-flag modes, the foreign-flag tables and the ignored runtime flags.
 *
 * `tests/unit/cli/cli-spec.test.ts` fails when the committed file is stale.
 */

export const SPEC_PATH = fileURLToPath(new URL("../../../crates/fnpm-cli/cli-spec.json", import.meta.url));

type SpecFlag = {
  key: string;
  raw: string;
  long: string;
  short?: string;
  /** Whether the flag takes a value (`[placeholder]` in `raw`). */
  takesValue: boolean;
  /** bun's `<STR>?`: the value only attaches with `=`. */
  optionalValue: boolean;
  repeatable: boolean;
  hidden: boolean;
  /** Spelled `--no-<name>`: passing it stores `true` under `key`. */
  negated: boolean;
  description: string;
};

type SpecNode = {
  kind: "group" | "leaf";
  name: string;
  aliases: string[];
  hidden: boolean;
  description: string;
  /** Leaves only. */
  positionals?: { name: string; required: boolean; variadic: boolean; description: string }[];
  /** Indexes into `flags`. Leaves only. */
  flags?: number[];
  passthrough?: true | number;
  foreignFlags?: "install" | "update";
  ignoresRuntimeFlags?: boolean;
  /** Groups only. */
  defaultChild?: string;
  children?: SpecNode[];
};

type SpecBin = {
  root: SpecNode;
  aliases: { name: string; target: string[] }[];
  globalFlags: number[];
};

export type CliSpec = {
  $comment: string;
  /** The npm package version; the native `--version` prints it. */
  version: string;
  flags: SpecFlag[];
  bins: Record<string, SpecBin>;
  foreignFlags: {
    /** `[foreign, alias]`; a null alias means "accepted and already satisfied". */
    rewrites: [string, string | null][];
    ignoredFlags: string[];
    ignoredOptions: string[];
    ignoredPrefixes: string[];
    updateFilters: string[];
  };
  runtimeFlags: { ignored: string[]; short: string };
};

function packageVersion(): string {
  const manifest = join(dirname(fileURLToPath(import.meta.url)), "..", "package.json");
  const pkg = JSON.parse(readFileSync(manifest, "utf8")) as { version: string };
  return pkg.version;
}

export function buildSpec(): CliSpec {
  const flags: SpecFlag[] = [];
  const flagIndex = new Map<string, number>();

  const flagId = (spec: FlagSpec): number => {
    const long = longName(spec.raw);
    const short = shortName(spec.raw);
    const entry: SpecFlag = {
      key: spec.key,
      raw: spec.raw,
      long,
      ...(short === undefined ? {} : { short }),
      takesValue: takesValue(spec),
      optionalValue: spec.optionalValue === true,
      repeatable: spec.repeatable === true,
      hidden: spec.hidden === true,
      negated: long.startsWith("no-"),
      description: spec.description,
    };
    const identity = JSON.stringify(entry);
    const existing = flagIndex.get(identity);
    if (existing !== undefined) {
      return existing;
    }
    flags.push(entry);
    flagIndex.set(identity, flags.length - 1);
    return flags.length - 1;
  };

  const node = (current: CommandNode): SpecNode => {
    const common = {
      name: current.name,
      aliases: [...(current.aliases ?? [])],
      hidden: current.hidden === true,
      description: current.description,
    };
    if (current.kind === "group") {
      return {
        kind: "group",
        ...common,
        ...(current.defaultChild === undefined ? {} : { defaultChild: current.defaultChild }),
        children: current.children.map(node),
      };
    }
    return {
      kind: "leaf",
      ...common,
      positionals: (current.positionals ?? []).map((positional) => ({
        name: positional.name,
        required: positional.required,
        variadic: positional.variadic === true,
        description: positional.description,
      })),
      flags: current.flags.map(flagId),
      ...(current.passthrough === undefined ? {} : { passthrough: current.passthrough }),
      ...(current.foreignFlags === undefined ? {} : { foreignFlags: current.foreignFlags }),
      ...(current.ignoresRuntimeFlags === true ? { ignoresRuntimeFlags: true } : {}),
    };
  };

  const bin = (registry: Registry): SpecBin => ({
    root: node(registry.root),
    aliases: registry.aliases.map((alias) => ({ name: alias.name, target: [...alias.target] })),
    globalFlags: registry.globalFlags.map(flagId),
  });

  const bins = { fnpm: bin(FNPM_REGISTRY), "fnpm-lock": bin(FNPM_LOCK_REGISTRY) };
  return {
    $comment: "Generated by `npm run build:spec` in packages/fuck-npm from src/cli/trees. Do not edit.",
    version: packageVersion(),
    flags,
    bins,
    foreignFlags: {
      rewrites: FOREIGN_FLAGS.map(([foreign, kind]) => [foreign, kind === "satisfied" ? null : kind.alias]),
      ignoredFlags: [...IGNORED_FOREIGN_FLAGS].sort(),
      ignoredOptions: [...IGNORED_FOREIGN_OPTIONS].sort(),
      ignoredPrefixes: [...IGNORED_FOREIGN_PREFIXES],
      updateFilters: [...UPDATE_FILTERS].sort(),
    },
    runtimeFlags: { ignored: [...IGNORED_RUNTIME_FLAGS].sort(), short: IGNORED_RUNTIME_SHORT },
  };
}

/** The exact bytes committed at SPEC_PATH. */
export function renderSpec(spec: CliSpec = buildSpec()): string {
  return `${JSON.stringify(spec, null, 2)}\n`;
}
