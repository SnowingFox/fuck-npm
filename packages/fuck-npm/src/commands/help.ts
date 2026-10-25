import { renderHelp } from "../cli/help.ts";
import type { CommandContext } from "./context.ts";

/** `fnpm help [...command]`: the page for a path, or root help. */
export async function runHelp(ctx: CommandContext): Promise<number> {
  ctx.runtime.stdout.write(
    renderHelp(ctx.registry, ctx.positionals.join(" "), { colors: ctx.output.colors }),
  );
  return 0;
}
