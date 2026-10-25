import type { CAC, Command } from "cac";
import { collectFlags } from "./registry.ts";
import type { CommandNode, Registry } from "./registry-types.ts";

/**
 * Register the global flags on cac. `-h, --help` is skipped because
 * `cli.help()` registers it itself.
 */
export function registerGlobalFlags(cli: CAC, registry: Registry): void {
  for (const spec of registry.globalFlags) {
    if (spec.key === "help") {
      continue;
    }
    cli.option(spec.raw, spec.description);
  }
}

/**
 * Register one top-level registry node as a cac command: name, aliases,
 * description, usage and flags all come from the node.
 *
 * Only the node the invocation resolved to is registered (see runtime.ts),
 * which keeps startup flat as the tree grows. Every node takes `[...args]`
 * and allows unknown options: positionals and flags are validated against the
 * resolved leaf by `rejectUnknownFlags` and `parseFlags`, which give better
 * messages than cac and know about per-subcommand scoping.
 */
export function registerNode(
  cli: CAC,
  node: CommandNode,
  action: (...args: never[]) => unknown,
): Command {
  const command = cli.command(`${node.name} [...args]`, node.description);
  if (node.usageText) {
    command.usage(node.usageText);
  }
  for (const alias of node.aliases ?? []) {
    command.alias(alias);
  }
  for (const spec of collectFlags(node)) {
    command.option(spec.raw, spec.description);
  }
  command.allowUnknownOptions();
  command.action(action as (...args: unknown[]) => unknown);
  return command;
}
