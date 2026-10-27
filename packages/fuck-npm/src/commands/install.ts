import { createColors } from "picocolors";
import { read } from "../cli/parse-flags.ts";
import { getCliVersionWithSha } from "../cli/version.ts";
import { installOptions } from "../core/options.ts";
import type { EventSink, InstallCommand, InstallOptions, InstallSummary } from "../core/types.ts";
import { colorEnabled } from "../output/output.ts";
import { createProgress, diagnosticsSink, wantsProgress, type Progress } from "../output/progress.ts";
import { formatInstallSummary } from "../output/summary.ts";
import type { CommandContext } from "./context.ts";

/**
 * install, ci, add, remove, update.
 *
 * Routing follows bun: `install -g` and `install <pkgs>` are `add`; `ci` is
 * `install --frozen-lockfile`; `update -i` is the interactive picker.
 */
export async function runInstallFamily(ctx: CommandContext): Promise<number> {
  const leaf = ctx.path[0] as "install" | "ci" | "add" | "remove" | "update";
  let command: InstallCommand = leaf === "ci" ? "install" : leaf;
  if (leaf === "install" && (read.bool(ctx.values, "global") || ctx.positionals.length > 0)) {
    command = "add";
  }
  if (command === "update" && read.bool(ctx.values, "interactive")) {
    return (await import("./update-interactive.ts")).runInteractiveUpdate(ctx);
  }

  const options = installOptions(command, ctx.values, ctx.positionals, ctx.runtime.cwd, {
    frozen: leaf === "ci",
  });
  printHeader(ctx, options, leaf === "ci" ? "ci" : command);
  const core = await ctx.runtime.loadCore();
  const listener = startListener(ctx, options);
  let summary: InstallSummary;
  try {
    summary = await core.install(options, listener.sink);
  } finally {
    listener.progress?.stop();
  }
  return finishInstall(ctx, options, summary);
}

/**
 * `fnpm add v0.1.0` on stdout, like bun's header (`<b>bun add <r><d>v1.2.3<r>`). One newline:
 * the summary opens with the blank line (bun's tree printer starts with `\n`), so there is
 * exactly one blank line between them, as in bun's output.
 */
export function printHeader(ctx: CommandContext, options: InstallOptions, label: string): void {
  if (ctx.output.mode === "json" || options.logLevel === "silent" || !options.summary) {
    return;
  }
  const pc = createColors(colorEnabled(ctx.runtime, ctx.runtime.stdoutIsTTY));
  ctx.runtime.stdout.write(`${pc.bold(`fnpm ${label} `)}${pc.dim(`v${getCliVersionWithSha()}`)}\n`);
}

/**
 * Who hears core events. `--json` and `--silent` register no callback at all,
 * so the core can skip building events; a terminal gets the progress line;
 * anything else only sees warnings and errors.
 */
export function startListener(
  ctx: CommandContext,
  options: Pick<InstallOptions, "logLevel" | "progress">,
): { sink?: EventSink; progress?: Progress } {
  if (ctx.output.mode === "json" || options.logLevel === "silent") {
    return {};
  }
  if (
    wantsProgress(ctx.runtime, {
      silent: false,
      quiet: options.logLevel === "quiet",
      noProgress: !options.progress,
    })
  ) {
    const progress = createProgress(ctx.runtime);
    return { sink: progress.sink, progress };
  }
  return { sink: diagnosticsSink(ctx.runtime, false) };
}

export function finishInstall(
  ctx: CommandContext,
  options: InstallOptions,
  summary: InstallSummary,
): number {
  const exitCode = summary.failedCount > 0 ? 1 : 0;
  if (ctx.output.mode === "json") {
    ctx.output.json({ ok: exitCode === 0, command: options.command, summary });
    return exitCode;
  }
  if (options.logLevel !== "silent" && options.summary) {
    const color = ctx.output.mode === "rich" && colorEnabled(ctx.runtime, ctx.runtime.stdoutIsTTY);
    // Like bun, the time since the process started.
    const elapsedMs = Math.max(summary.elapsedMs, ctx.runtime.now());
    for (const line of formatInstallSummary({ ...summary, elapsedMs }, options.command, ctx.output.colors, { color, global: options.global })) {
      ctx.output.line(line);
    }
  }
  return exitCode;
}
