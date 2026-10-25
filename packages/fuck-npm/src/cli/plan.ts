import { SilentError } from "../errors.ts";
import { stripRuntimeFlags, translateForeignFlags } from "./foreign-flags.ts";
import {
  allowedFlagNames,
  findAlias,
  flagSpecsFor,
  longName,
  matchesSegment,
  shortName,
  takesValue,
} from "./registry.ts";
import type { CommandAlias, CommandNode, FlagSpec, Registry } from "./registry-types.ts";

/** Everything derived from argv before any command code runs. */
export type Invocation = {
  node: CommandNode;
  /** Canonical command path the registry consumed (aliases expanded). */
  matched: string[];
  viaAlias?: CommandAlias;
  /** Our tokens: flags and positionals after the command path. */
  own: string[];
  /** Tokens for a child process (passthrough leaves), or after a user-typed `--`. */
  forwarded: string[];
  /** A group was given a positional that names none of its children. */
  unknownSegment?: string;
  wantsHelp: boolean;
  wantsTree: boolean;
  wantsVersion: boolean;
  wantsJson: boolean;
  /** argv rebuilt for cac: canonical path, then our own tokens. */
  cacArgv: string[];
};

const isFlagToken = (token: string): boolean =>
  token.startsWith("-") && token !== "-" && token !== "--";

/** Whether `token` names a flag in `specs` that takes its value from the next token. */
function consumesNext(token: string, specs: readonly FlagSpec[]): boolean {
  if (token.includes("=")) {
    return false;
  }
  const index = specIndex(specs);
  const spec = token.startsWith("--")
    ? index.long.get(token.slice(2))
    : token.length === 2
      ? index.short.get(token.slice(1))
      : undefined;
  return spec !== undefined && takesValue(spec) && spec.optionalValue !== true;
}

type SpecIndex = { long: Map<string, FlagSpec>; short: Map<string, FlagSpec> };
const specIndexCache = new WeakMap<readonly FlagSpec[], SpecIndex>();

function specIndex(specs: readonly FlagSpec[]): SpecIndex {
  let index = specIndexCache.get(specs);
  if (!index) {
    index = { long: new Map(), short: new Map() };
    for (const spec of specs) {
      const long = longName(spec.raw);
      if (!index.long.has(long)) index.long.set(long, spec);
      const short = shortName(spec.raw);
      if (short !== undefined && !index.short.has(short)) index.short.set(short, spec);
    }
    specIndexCache.set(specs, index);
  }
  return index;
}

/**
 * Resolve argv against the registry and split it into our tokens and the
 * child's.
 *
 * The command path is found by walking positionals down the tree while
 * stepping over flags (and their values), so `fnpm pm --cwd app cache rm`
 * resolves like `fnpm pm cache rm --cwd app`. After the leaf is known:
 *   1. npm/yarn/pnpm flags are translated (install, ci, add, remove, update),
 *   2. bun's ignored runtime switches are dropped,
 *   3. a passthrough leaf splits its tokens: `passthrough: N` keeps parsing
 *      until N positionals have been seen and forwards the rest verbatim.
 */
export function planInvocation(argv: string[], registry: Registry): Invocation {
  const args = argv.slice(2);
  const dashIndex = args.indexOf("--");
  const head = dashIndex === -1 ? args : args.slice(0, dashIndex);
  const tail = dashIndex === -1 ? [] : args.slice(dashIndex + 1);

  let node: CommandNode = registry.root;
  const matched: string[] = [];
  let viaAlias: CommandAlias | undefined;
  let unknownSegment: string | undefined;
  const rest: string[] = [];
  let index = 0;

  while (index < head.length) {
    const token = head[index] as string;
    if (node.kind !== "group") {
      break;
    }
    if (isFlagToken(token)) {
      rest.push(token);
      if (consumesNext(token, flagSpecsFor(node, registry)) && index + 1 < head.length) {
        rest.push(head[index + 1] as string);
        index += 1;
      }
      index += 1;
      continue;
    }
    if (matched.length === 0 && viaAlias === undefined) {
      const alias = findAlias(registry, token);
      if (alias) {
        viaAlias = alias;
        let target: CommandNode = registry.root;
        for (const segment of alias.target) {
          if (target.kind !== "group") break;
          const child: CommandNode | undefined = target.children.find((candidate) =>
            matchesSegment(candidate, segment),
          );
          if (!child) break;
          target = child;
          matched.push(child.name);
        }
        node = target;
        index += 1;
        continue;
      }
    }
    const child: CommandNode | undefined = node.children.find((candidate) =>
      matchesSegment(candidate, token),
    );
    if (!child) {
      if (node !== registry.root || matched.length > 0) {
        unknownSegment = token;
      }
      break;
    }
    node = child;
    matched.push(child.name);
    index += 1;
  }
  rest.push(...head.slice(index));

  let tokens = rest;
  let forwarded: string[] = [];
  if (node.kind === "leaf") {
    tokens = stripRuntimeFlags(translateForeignFlags(tokens, node), node);
    if (node.passthrough !== undefined) {
      const split =
        node.passthrough === true
          ? partitionDeclared(tokens, node, registry)
          : splitAfterPositionals(tokens, node.passthrough, flagSpecsFor(node, registry));
      tokens = split.own;
      forwarded = split.forwarded;
    }
  }
  forwarded = [...forwarded, ...tail];

  const has = (short: string, long: string): boolean =>
    tokens.some((token) => token === short || token === long);

  return {
    node,
    matched,
    ...(viaAlias ? { viaAlias } : {}),
    own: tokens,
    forwarded,
    ...(unknownSegment !== undefined ? { unknownSegment } : {}),
    wantsHelp: has("-h", "--help"),
    wantsTree: has("-t", "--tree"),
    wantsVersion: has("-v", "--version"),
    wantsJson: tokens.includes("--json"),
    cacArgv: [...argv.slice(0, 2), ...matched, ...tokens],
  };
}

/** bun's `stop_after_positional_at`: parse ours until N positionals, forward the rest. */
function splitAfterPositionals(
  tokens: readonly string[],
  limit: number,
  specs: readonly FlagSpec[],
): { own: string[]; forwarded: string[] } {
  const own: string[] = [];
  let seen = 0;
  for (let index = 0; index < tokens.length; index += 1) {
    if (seen >= limit) {
      return { own, forwarded: tokens.slice(index) };
    }
    const token = tokens[index] as string;
    own.push(token);
    if (isFlagToken(token)) {
      if (consumesNext(token, specs) && index + 1 < tokens.length) {
        own.push(tokens[index + 1] as string);
        index += 1;
      }
      continue;
    }
    seen += 1;
  }
  return { own, forwarded: [] };
}

/**
 * `passthrough: true`: keep the flags the leaf declares (with their values)
 * and every bare word, forward every other flag.
 */
function partitionDeclared(
  tokens: readonly string[],
  node: CommandNode,
  registry: Registry,
): { own: string[]; forwarded: string[] } {
  const declared = allowedFlagNames(node, registry);
  const specs = flagSpecsFor(node, registry);
  const own: string[] = [];
  const forwarded: string[] = [];
  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index] as string;
    const name = flagName(token);
    if (name === undefined) {
      own.push(token);
      continue;
    }
    if (!declared.has(name)) {
      forwarded.push(token);
      continue;
    }
    own.push(token);
    if (consumesNext(token, specs) && index + 1 < tokens.length) {
      own.push(tokens[index + 1] as string);
      index += 1;
    }
  }
  return { own, forwarded };
}

/** `json` from `--json=x`, `h` from `-h`; undefined for non-flag tokens. */
export function flagName(token: string): string | undefined {
  if (!isFlagToken(token)) {
    return undefined;
  }
  const withoutDashes = token.replace(/^-{1,2}/, "");
  const [name] = withoutDashes.split("=");
  return name === "" ? undefined : name;
}

const spell = (name: string): string => (name.length > 1 ? `--${name}` : `-${name}`);

/**
 * Reject flags the resolved command does not accept.
 *
 * cac cannot do this: groups are registered with the union of their children's
 * flags, so `fnpm pm ls --max-age 3` would parse cleanly and silently ignore
 * the flag. A flag declared elsewhere gets a pointer to its owner (bun's
 * "is only supported by `bun pm cache`"); a flag nothing declares is a typo.
 */
/** `fnpm install` for `fnpm ci`, `fnpm pm` for `fnpm pm ls`: the name bun's unknown-flag error uses. */
export function unknownFlagCommand(registry: Registry, matched: readonly string[]): string {
  const top = matched[0] === "ci" ? "install" : matched[0];
  return top === undefined ? registry.bin : `${registry.bin} ${top}`;
}

export function rejectUnknownFlags(plan: Invocation, registry: Registry): void {
  if (plan.own.length === 0) {
    return;
  }
  const allowed = allowedFlagNames(plan.node, registry);
  const specs = flagSpecsFor(plan.node, registry);
  // bun names the command (`fnpm pm has no --nope flag.`), even when another command has the flag.
  const command = unknownFlagCommand(registry, plan.matched);

  for (let index = 0; index < plan.own.length; index += 1) {
    const token = plan.own[index] as string;
    const name = flagName(token);
    if (name === undefined) {
      continue;
    }
    const names = token.startsWith("--") ? [name] : clusterLetters(name, specs);
    for (const candidate of names) {
      if (allowed.has(candidate)) {
        continue;
      }
      throw new SilentError(`${command} has no ${spell(candidate)} flag.`, {
        code: "UNKNOWN_FLAG",
      });
    }
    if (consumesNext(token, specs)) {
      index += 1;
    }
  }
}

/** Letters of a short cluster up to (and including) the first value flag. */
function clusterLetters(body: string, specs: readonly FlagSpec[]): string[] {
  const letters: string[] = [];
  const index = specIndex(specs);
  for (const letter of body) {
    letters.push(letter);
    const spec = index.short.get(letter);
    if (spec !== undefined && takesValue(spec)) {
      break;
    }
  }
  return letters;
}
