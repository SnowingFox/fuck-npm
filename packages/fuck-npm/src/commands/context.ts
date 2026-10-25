import type { ParsedFlags } from "../cli/parse-flags.ts";
import type { CommandLeaf, Registry } from "../cli/registry-types.ts";
import type { Output } from "../output/output.ts";
import type { CliRuntime } from "../runtime/types.ts";

/** Everything a command handler gets. Flags are already validated against the leaf. */
export type CommandContext = {
  registry: Registry;
  runtime: CliRuntime;
  node: CommandLeaf;
  /** Canonical command path, e.g. `["pm", "cache", "rm"]`. */
  path: string[];
  values: ParsedFlags["values"];
  positionals: string[];
  /** Arguments for a child process, verbatim (run, exec, x, create). */
  passthrough: string[];
  output: Output;
};

export type Handler = (ctx: CommandContext) => Promise<number>;
