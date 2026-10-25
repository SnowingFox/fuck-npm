import type {
  CommandAlias,
  CommandGroup,
  CommandNode,
  FlagSpec,
  Registry,
  ResolvedCommand,
} from "./registry-types.ts";

/**
 * Tree operations shared by both bins. Every function takes the registry it
 * works on: `fnpm` and `fnpm-lock` are two roots over the same machinery.
 */

const VALUE_PLACEHOLDER = /[<[][^>\]]*[>\]]/;

export const camelize = (value: string): string =>
  value.replaceAll(/-([a-z0-9])/g, (_match, char: string) => char.toUpperCase());

type FlagMeta = { long: string; short: string | undefined; value: boolean };

/** Parsed once per raw spelling: these run for every flag on every invocation. */
const metaCache = new Map<string, FlagMeta>();

function metaOf(raw: string): FlagMeta {
  const cached = metaCache.get(raw);
  if (cached) {
    return cached;
  }
  const parts = raw.replace(VALUE_PLACEHOLDER, "").split(",").map((part) => part.trim());
  const long = (parts.find((part) => part.startsWith("--")) ?? parts[0] ?? raw)
    .replace(/^-{1,2}/, "")
    .trim();
  const short = parts.find((part) => /^-[^-]$/.test(part))?.slice(1);
  const meta = { long, short, value: VALUE_PLACEHOLDER.test(raw) };
  metaCache.set(raw, meta);
  return meta;
}

/** Long name of a raw spelling without dashes: `config` for `-c, --config [p]`. */
export function longName(raw: string): string {
  return metaOf(raw).long;
}

/** Single-letter alias of a raw spelling, if any: `c` for `-c, --config`. */
export function shortName(raw: string): string | undefined {
  return metaOf(raw).short;
}

export function takesValue(spec: FlagSpec): boolean {
  return metaOf(spec.raw).value;
}

/** Build a FlagSpec, deriving the key from the long name. */
export function flag(
  raw: string,
  description: string,
  extra: Omit<Partial<FlagSpec>, "raw" | "description"> = {},
): FlagSpec {
  return { key: camelize(longName(raw)), raw, description, ...extra };
}

/** Children a user should see: everything not explicitly hidden. */
export function visibleChildren(group: CommandGroup): CommandNode[] {
  return group.children.filter((child) => child.hidden !== true);
}

export const matchesSegment = (node: CommandNode, segment: string): boolean =>
  node.name === segment || (node.aliases?.includes(segment) ?? false);

export function findAlias(
  registry: Registry,
  segment: string | undefined,
): CommandAlias | undefined {
  return segment === undefined
    ? undefined
    : registry.aliases.find((entry) => entry.name === segment);
}

/**
 * Walk `path` down the tree, stopping at the first segment no child matches.
 * Returns the deepest node plus the leftovers, so callers can tell "this leaf
 * takes positionals" from "this group got a typo".
 */
export function resolveCommandPath(
  path: string[],
  registry: Registry,
): ResolvedCommand {
  let segments = path;
  const alias = findAlias(registry, segments[0]);
  if (alias) {
    segments = [...alias.target, ...segments.slice(1)];
  }

  let node: CommandNode = registry.root;
  const matched: string[] = [];
  let index = 0;
  while (index < segments.length && node.kind === "group") {
    const segment = segments[index] as string;
    const child: CommandNode | undefined = node.children.find((candidate) =>
      matchesSegment(candidate, segment),
    );
    if (!child) {
      break;
    }
    node = child;
    matched.push(child.name);
    index += 1;
  }

  const resolved: ResolvedCommand = { node, matched, rest: segments.slice(index) };
  return alias ? { ...resolved, viaAlias: alias } : resolved;
}

/** The node a path names exactly, or undefined when the path has leftovers. */
export function findCommandNode(
  path: string[],
  registry: Registry,
): CommandNode | undefined {
  const resolved = resolveCommandPath(path, registry);
  return resolved.rest.length > 0 ? undefined : resolved.node;
}

/**
 * Flags a node accepts. A group returns the union of its descendants' flags,
 * deduped by key: cac matches `fnpm pm ls --all` against `pm`, so the flag
 * must be registered there. Whether it is legal on the resolved leaf is
 * checked separately.
 */
const collectCache = new WeakMap<CommandNode, FlagSpec[]>();

export function collectFlags(node: CommandNode): FlagSpec[] {
  const cached = collectCache.get(node);
  if (cached) {
    return cached;
  }
  const collected = collectUncached(node);
  collectCache.set(node, collected);
  return collected;
}

function collectUncached(node: CommandNode): FlagSpec[] {
  if (node.kind === "leaf") {
    return [...node.flags];
  }
  const seen = new Map<string, FlagSpec>();
  const walk = (current: CommandNode): void => {
    if (current.kind === "leaf") {
      for (const spec of current.flags) {
        if (!seen.has(spec.key)) {
          seen.set(spec.key, spec);
        }
      }
      return;
    }
    for (const child of current.children) {
      walk(child);
    }
  };
  walk(node);
  return [...seen.values()];
}

/**
 * Every spelling of a flag accepted on the command line: comma-separated
 * aliases without dashes or placeholders, camelCase keys, and for a negated
 * `--no-x` both `no-x` and `x` (cac stores it under the positive name).
 */
const acceptedCache = new Map<string, Set<string>>();

export function acceptedFlagNames(raw: string): Set<string> {
  const cached = acceptedCache.get(raw);
  if (cached) {
    return cached;
  }
  const names = new Set<string>();
  acceptedCache.set(raw, names);
  const withoutPlaceholder = raw.replaceAll(/[<[][^>\]]*[>\]]/g, "");
  for (const part of withoutPlaceholder.split(",")) {
    const trimmed = part.trim().replace(/^-{1,2}/, "");
    if (trimmed === "") {
      continue;
    }
    names.add(trimmed);
    names.add(camelize(trimmed));
    if (trimmed.startsWith("no-")) {
      const positive = trimmed.slice(3);
      names.add(positive);
      names.add(camelize(positive));
    }
  }
  return names;
}

/**
 * Flag names legal on `node`, including the global ones. A boolean long flag
 * also accepts `--no-<name>`, like bun's clap does for a command's own flags.
 */
const allowedCache = new WeakMap<Registry, WeakMap<CommandNode, Set<string>>>();

export function allowedFlagNames(node: CommandNode, registry: Registry): Set<string> {
  let perRegistry = allowedCache.get(registry);
  if (!perRegistry) {
    perRegistry = new WeakMap();
    allowedCache.set(registry, perRegistry);
  }
  const cached = perRegistry.get(node);
  if (cached) {
    return cached;
  }
  const names = new Set<string>();
  perRegistry.set(node, names);
  for (const spec of [...collectFlags(node), ...registry.globalFlags]) {
    for (const name of acceptedFlagNames(spec.raw)) {
      names.add(name);
    }
    if (!takesValue(spec)) {
      names.add(`no-${longName(spec.raw)}`);
    }
  }
  return names;
}

/** The flags `node` parses: its own (or its descendants') plus the globals. */
const specsCache = new WeakMap<Registry, WeakMap<CommandNode, FlagSpec[]>>();

export function flagSpecsFor(node: CommandNode, registry: Registry): FlagSpec[] {
  let perRegistry = specsCache.get(registry);
  if (!perRegistry) {
    perRegistry = new WeakMap();
    specsCache.set(registry, perRegistry);
  }
  let specs = perRegistry.get(node);
  if (!specs) {
    specs = [...collectFlags(node), ...registry.globalFlags];
    perRegistry.set(node, specs);
  }
  return specs;
}

const ownersCache = new WeakMap<Registry, Map<string, string[]>>();

/**
 * Every flag the tree declares, mapped to the command paths that accept it,
 * reported under a top-level alias when one exists. Built only on the error
 * path, then cached.
 */
export function flagOwners(registry: Registry): Map<string, string[]> {
  const cached = ownersCache.get(registry);
  if (cached) {
    return cached;
  }
  const owners = new Map<string, string[]>();
  const walk = (node: CommandNode, path: string[]): void => {
    if (node.kind === "leaf") {
      const owner = preferredSpelling(path, registry);
      for (const spec of node.flags) {
        for (const name of acceptedFlagNames(spec.raw)) {
          const existing = owners.get(name);
          if (existing === undefined) {
            owners.set(name, [owner]);
          } else if (!existing.includes(owner)) {
            existing.push(owner);
          }
        }
      }
      return;
    }
    for (const child of node.children) {
      walk(child, [...path, child.name]);
    }
  };
  for (const child of registry.root.children) {
    walk(child, [child.name]);
  }
  ownersCache.set(registry, owners);
  return owners;
}

/** The shortest spelling of a command path: its alias when one exists. */
export function preferredSpelling(path: string[], registry: Registry): string {
  const alias = registry.aliases.find(
    (entry) =>
      entry.target.length === path.length &&
      entry.target.every((segment, index) => segment === path[index]),
  );
  return alias?.name ?? path.join(" ");
}
