import { createColors } from "picocolors";
import type { Colors } from "../output/output.ts";
import { getCliVersion } from "./version.ts";
import { collectFlags, resolveCommandPath, visibleChildren } from "./registry.ts";
import type {
  CommandGroup,
  CommandLeaf,
  CommandNode,
  FlagSpec,
  Registry,
} from "./registry-types.ts";

/**
 * Help and `--tree`, rendered from the registry only. Loaded lazily: nothing
 * on the install hot path imports this module.
 */

const INDENT = "  ";

/**
 * Colours for the render in progress. Set by each entry point from the caller's
 * choice, so `FORCE_COLOR=0` and piped output render plain. (picocolors' own
 * default treats any FORCE_COLOR, even "0", as "on".)
 */
let pc: Colors = createColors(false);

export type RenderOptions = { colors?: Colors };

const useColors = (options: RenderOptions): void => {
  pc = options.colors ?? createColors(false);
};
const BAR = (): string => pc.gray("│");

/**
 * Help for a space-separated command path, falling back to root help when the
 * path names nothing.
 */
export function renderHelp(registry: Registry, cmd?: string, options: RenderOptions = {}): string {
  useColors(options);
  if (cmd !== undefined && cmd.trim() !== "") {
    const resolved = resolveCommandPath(splitPath(cmd), registry);
    // A leaf may have leftovers (its positionals); a group with leftovers was
    // given a subcommand that does not exist.
    if (
      resolved.node !== registry.root &&
      (resolved.node.kind === "leaf" || resolved.rest.length === 0)
    ) {
      return renderCommandHelp(registry, resolved.node, resolved.matched);
    }
  }
  return renderRootHelp(registry);
}

/** The whole registered surface as a tree; recurses, so every node appears. */
export function renderTree(registry: Registry, options: RenderOptions = {}): string {
  useColors(options);
  const lines = [`${pc.gray("┌")}  ${pc.cyan(pc.bold(registry.bin))}`, BAR()];
  appendTreeLevel(lines, registry.root, "");
  lines.push(pc.gray("└"));
  return `${lines.join("\n")}\n`;
}

const TREE_NAME_WIDTH = 20;

const appendTreeLevel = (lines: string[], group: CommandGroup, prefix: string): void => {
  const children = visibleChildren(group);
  for (const [index, child] of children.entries()) {
    const isLast = index === children.length - 1;
    const branch = isLast ? "└──" : "├──";
    const label = `${prefix}${pc.gray(branch)} `;
    const visibleWidth = Math.max(1, TREE_NAME_WIDTH - stripAnsi(prefix).length);
    lines.push(
      `${BAR()}  ${label}${pc.cyan(child.name.padEnd(visibleWidth))} ${child.description}`,
    );
    if (child.kind === "group") {
      appendTreeLevel(lines, child, `${prefix}${isLast ? "    " : "│   "}`);
    }
  }
};

const renderRootHelp = (registry: Registry): string => {
  const visible = visibleChildren(registry.root);
  const rows: [string, string][] = visible.map((command) => [
    [command.name, ...(command.aliases ?? [])].join(", "),
    command.description,
  ]);
  for (const alias of registry.aliases) {
    rows.push([alias.name, alias.description]);
  }
  const commandWidth = Math.max(8, ...rows.map(([name]) => name.length));
  const lines = [pc.gray("┌")];
  lines.push(`${BAR()}  ${pc.cyan(pc.bold(registry.bin))} ${pc.dim(`v${getCliVersion()}`)}`);
  lines.push(`${BAR()}  ${pc.dim(registry.tagline)}`);
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.bold("Usage:")}`);
  lines.push(`${BAR()}  ${INDENT}${pc.magenta(registry.bin)} <command> [...args]`);
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.bold("Commands:")}`);
  for (const [name, description] of rows) {
    lines.push(`${BAR()}  ${INDENT}${pc.cyan(name.padEnd(commandWidth))}  ${description}`);
  }
  const globalFlags = visibleGlobalFlags(registry);
  if (globalFlags.length > 0) {
    lines.push(BAR());
    lines.push(`${BAR()}  ${pc.bold("Global flags:")}`);
    lines.push(...renderFlagRows(globalFlags));
  }
  if (registry.detailedHelp.length > 0) {
    lines.push(BAR());
    lines.push(`${BAR()}  ${pc.bold("Detailed help:")}`);
    lines.push(...renderCommandRows(registry.detailedHelp));
  }
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.dim(`Run '${registry.bin} help <command>' for details.`)}`);
  lines.push(pc.gray("└"));
  return `${lines.join("\n")}\n`;
};

const renderCommandHelp = (registry: Registry, node: CommandNode, path: string[]): string => {
  const fullName = [registry.bin, ...path].join(" ");
  const lines = [`${pc.gray("┌")}  ${pc.cyan(pc.bold(fullName))}`, BAR()];
  lines.push(`${BAR()}  ${node.description}`);
  if (node.aliases && node.aliases.length > 0) {
    lines.push(`${BAR()}  ${pc.dim(`Aliases: ${node.aliases.join(", ")}`)}`);
  }
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.bold("Usage:")}  ${usageLine(registry, node, path)}`);

  if (node.kind === "group") {
    appendSubcommands(lines, node);
  } else {
    appendPositionals(lines, node);
  }
  appendFlags(lines, node);
  appendExamples(lines, node);
  appendNotes(lines, node);

  const globalFlags = visibleGlobalFlags(registry);
  if (globalFlags.length > 0) {
    lines.push(BAR());
    lines.push(`${BAR()}  ${pc.bold("Global flags:")}`);
    lines.push(...renderFlagRows(globalFlags));
  }
  if (node.kind === "group") {
    lines.push(BAR());
    lines.push(
      `${BAR()}  ${pc.dim(`Run '${registry.bin} help ${path.join(" ")} <subcommand>' for details.`)}`,
    );
  }
  lines.push(pc.gray("└"));
  return `${lines.join("\n")}\n`;
};

const positionalLabel = (positional: { name: string; required: boolean; variadic?: boolean }) => {
  const inner = positional.variadic ? `...${positional.name}` : positional.name;
  return positional.required ? `<${inner}>` : `[${inner}]`;
};

const usageLine = (registry: Registry, node: CommandNode, path: string[]): string => {
  if (node.usageText) {
    return node.usageText;
  }
  const base = [registry.bin, ...path].join(" ");
  if (node.kind === "group") {
    return `${base} <subcommand> [...args]`;
  }
  const parts = [base, ...(node.positionals ?? []).map(positionalLabel)];
  if (node.flags.length > 0) {
    parts.push("[options]");
  }
  if (node.passthrough === true) {
    parts.push("[-- ...args]");
  }
  return parts.join(" ");
};

const appendSubcommands = (lines: string[], group: CommandGroup): void => {
  const children = visibleChildren(group);
  if (children.length === 0) {
    return;
  }
  const rows = children.map((child) => {
    const usage = [child.name];
    if (child.kind === "group") {
      usage.push("<subcommand>");
    } else {
      usage.push(...(child.positionals ?? []).map(positionalLabel));
    }
    const suffix = group.defaultChild === child.name ? " (default)" : "";
    return [usage.join(" "), `${child.description}${suffix}`] as const;
  });
  const width = Math.max(24, ...rows.map(([usage]) => usage.length));
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.bold("Commands:")}`);
  for (const [usage, description] of rows) {
    lines.push(`${BAR()}  ${INDENT}${pc.cyan(usage.padEnd(width))}  ${description}`);
  }
};

const appendPositionals = (lines: string[], leaf: CommandLeaf): void => {
  const positionals = leaf.positionals ?? [];
  if (positionals.length === 0) {
    return;
  }
  const rows = positionals.map((positional) => [positionalLabel(positional), positional.description] as const);
  const width = Math.max(18, ...rows.map(([name]) => name.length));
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.bold("Arguments:")}`);
  for (const [name, description] of rows) {
    lines.push(`${BAR()}  ${INDENT}${pc.cyan(name.padEnd(width))}  ${description}`);
  }
};

/**
 * A leaf lists its own flags. A group lists only the flags that are specific
 * to one child, per child, so the shared package-manager table is not printed
 * twenty times.
 */
const appendFlags = (lines: string[], node: CommandNode): void => {
  if (node.kind === "leaf") {
    const flags = node.flags.filter((spec) => spec.hidden !== true);
    if (flags.length === 0) {
      return;
    }
    lines.push(BAR());
    lines.push(`${BAR()}  ${pc.bold("Flags:")}`);
    lines.push(...renderFlagRows(flags.map(toFlagRow)));
    return;
  }

  const children = visibleChildren(node);
  const counts = new Map<string, number>();
  for (const child of children) {
    for (const spec of collectFlags(child)) {
      counts.set(spec.key, (counts.get(spec.key) ?? 0) + 1);
    }
  }
  const shared = children.length > 1
    ? new Set([...counts].filter(([, count]) => count === children.length).map(([key]) => key))
    : new Set<string>();
  const groups = children
    .map((child) => ({
      title: `${node.name} ${child.name}`,
      flags: collectFlags(child).filter((spec) => spec.hidden !== true && !shared.has(spec.key)),
    }))
    .filter((group) => group.flags.length > 0);
  if (groups.length === 0) {
    return;
  }
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.bold("Flags:")}`);
  for (const group of groups) {
    lines.push(`${BAR()}  ${INDENT}${pc.bold(group.title)}`);
    lines.push(...renderFlagRows(group.flags.map(toFlagRow), INDENT.length));
  }
  if (shared.size > 0) {
    lines.push(
      `${BAR()}  ${INDENT}${pc.dim(`Shared package-manager flags: run '${node.name} <subcommand> -h'.`)}`,
    );
  }
};

const appendExamples = (lines: string[], node: CommandNode): void => {
  const groups = node.exampleGroups ?? [];
  if (groups.length === 0) {
    return;
  }
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.bold("Examples:")}`);
  for (const group of groups) {
    lines.push(`${BAR()}  ${INDENT}${pc.bold(group.title)}`);
    for (const [example, why] of group.examples) {
      lines.push(`${BAR()}  ${INDENT}${INDENT}${pc.dim(example)}`);
      lines.push(`${BAR()}  ${INDENT}${INDENT}${INDENT}${why}`);
    }
  }
};

const appendNotes = (lines: string[], node: CommandNode): void => {
  const notes = node.notes ?? [];
  if (notes.length === 0) {
    return;
  }
  lines.push(BAR());
  lines.push(`${BAR()}  ${pc.bold("Notes:")}`);
  for (const note of notes) {
    lines.push(`${BAR()}  ${INDENT}${pc.dim(note)}`);
  }
};

const toFlagRow = (spec: FlagSpec): [string, string] => [spec.raw, spec.description];

const renderFlagRows = (flags: [rawName: string, description: string][], extraIndent = 0): string[] => {
  const flagWidth = Math.max(18, ...flags.map(([rawName]) => rawName.length));
  const indent = INDENT + " ".repeat(extraIndent);
  return flags.map(
    ([rawName, description]) =>
      `${BAR()}  ${indent}${pc.dim(rawName.padEnd(flagWidth))}  ${description}`,
  );
};

const renderCommandRows = (rows: [command: string, description: string][]): string[] => {
  const commandWidth = Math.max(18, ...rows.map(([command]) => command.length));
  return rows.map(
    ([command, description]) =>
      `${BAR()}  ${INDENT}${pc.cyan(command.padEnd(commandWidth))}  ${description}`,
  );
};

const visibleGlobalFlags = (registry: Registry): [string, string][] =>
  registry.globalFlags
    .filter((spec) => spec.hidden !== true)
    .map((spec) => [spec.raw, spec.description]);

const splitPath = (cmd: string): string[] => cmd.split(/\s+/).filter((segment) => segment !== "");

const ANSI_RE = /\u001B\[[0-9;]*m/g;

export const stripAnsi = (value: string): string => value.replaceAll(ANSI_RE, "");
