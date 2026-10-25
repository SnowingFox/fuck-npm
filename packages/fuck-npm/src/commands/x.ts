import { getCliVersion } from "../cli/version.ts";
import type { CommandContext } from "./context.ts";

/**
 * `fnpm x` / `fnpm dlx` / `fnpm create`. The arguments arrive unparsed (`rawArgs`): the core
 * parses them like bunx, finds or installs the bin, and runs it with inherited stdio. Every
 * message is written by the core; the exit status is the bin's.
 */
export async function runX(ctx: CommandContext): Promise<number> {
  const core = await ctx.runtime.loadCore();
  const result = await core.invoke({
    command: [ctx.path[0] === "create" ? "create" : "x"],
    cwd: ctx.runtime.cwd,
    positionals: [],
    passthrough: ctx.passthrough,
    options: { cliVersion: getCliVersion() },
    json: false,
  });
  return result.exitCode;
}
