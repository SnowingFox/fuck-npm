import { createColors } from "picocolors";
import { read } from "../cli/parse-flags.ts";
import { getCliVersion } from "../cli/version.ts";
import { SilentError } from "../errors.ts";
import { logLevelOf, requestOptions } from "../core/options.ts";
import type { CoreRequest } from "../core/types.ts";
import { colorEnabled } from "../output/output.ts";
import type { CommandContext } from "./context.ts";
import { startListener } from "./install.ts";

/**
 * Every command without a dedicated handler: validate, then hand the request
 * to the core as `{command, positionals, passthrough, options}`. The core
 * returns structured `data` (the `--json` payload) and pre-rendered `lines`.
 */
export async function runGeneric(ctx: CommandContext): Promise<number> {
  const [head] = ctx.path;
  const isPm = ctx.node.ignoresRuntimeFlags === true;
  const isLink = head === "link" || head === "unlink";

  if (head === "prune" && read.bool(ctx.values, "global")) {
    throw new SilentError(
      "--global cannot be used with fnpm prune. The global folder is where `fnpm link` registers packages.",
      { code: "FLAG_CONFLICT" },
    );
  }

  const options = requestOptions(ctx.values, ctx.runtime.cwd, isPm, !isLink);
  if (isLink) {
    // bun forces -g off for link/unlink: the link registry is always global.
    options.global = false;
  }
  if (head === "outdated") {
    // `outdated` only reads; bun forces dry-run.
    options.dryRun = true;
  }
  const logLevel = logLevelOf(ctx.values);

  const header = head === "outdated" ? "fnpm outdated" : ctx.path.join(" ") === "pm scan" ? "fnpm pm scan" : undefined;
  if (header !== undefined && ctx.output.mode !== "json" && logLevel !== "silent") {
    const pc = createColors(colorEnabled(ctx.runtime, ctx.runtime.stderrIsTTY));
    ctx.runtime.stderr.write(`${pc.bold(header)} ${pc.dim(`v${getCliVersion()}`)}\n`);
  }

  // Commands that print their own header (`fnpm audit fix vX`) print the CLI's version.
  (options as Record<string, unknown>).cliVersion = getCliVersion();
  const request: CoreRequest = {
    command: ctx.registry.bin === "fnpm-lock" ? ["lock", ...ctx.path] : ctx.path,
    cwd: String(options.cwd),
    positionals: ctx.positionals,
    passthrough: ctx.passthrough,
    options,
    json: ctx.output.mode === "json",
  };

  const core = await ctx.runtime.loadCore();
  const listener = startListener(ctx, {
    logLevel,
    progress: ctx.values.noProgress !== true,
  });
  let result;
  try {
    result = await core.invoke(request, listener.sink);
  } finally {
    listener.progress?.stop();
  }

  if (ctx.output.mode === "json") {
    const rawDocument = ctx.path.join(" ") === "pm diff" || ((head === "audit" || ctx.path.join(" ") === "pm audit") && ctx.positionals[0] === "fix");
    ctx.output.json(rawDocument ? result.data : { ok: result.exitCode === 0, command: ctx.path.join(" "), ...result.data });
    return result.exitCode;
  }
  if (logLevel !== "silent" || head === "run" || head === "exec" || head === "x") {
    for (const line of result.lines ?? []) {
      ctx.output.line(line);
    }
  }
  return result.exitCode;
}
