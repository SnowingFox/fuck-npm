import { SilentError } from "../errors.ts";
import { installOptions } from "../core/options.ts";
import type { InstallSummary, OutdatedEntry } from "../core/types.ts";
import type { CommandContext } from "./context.ts";
import { finishInstall, printHeader, startListener } from "./install.ts";

/**
 * `fnpm update -i`: ask the core what is outdated, let the user pick, then run
 * a normal update for the picks. The picker is the only part that is
 * interactive, so a scripted equivalent is always `fnpm update <names...>`.
 */
export async function runInteractiveUpdate(ctx: CommandContext): Promise<number> {
  if (ctx.output.mode === "json") {
    throw new SilentError(
      "`fnpm update --interactive` cannot be combined with --json. Pass the packages to update instead: fnpm update <pkg...>",
      { code: "NOT_INTERACTIVE" },
    );
  }
  if (!ctx.runtime.stdinIsTTY || !ctx.runtime.stdoutIsTTY) {
    throw new SilentError(
      "`fnpm update --interactive` needs a terminal. Pass the packages to update instead: fnpm update <pkg...>",
      { code: "NOT_INTERACTIVE" },
    );
  }

  const options = installOptions("update", ctx.values, ctx.positionals, ctx.runtime.cwd);
  printHeader(ctx, options, "update --interactive");
  const core = await ctx.runtime.loadCore();
  const outdated = await core.invoke({
    command: ["outdated"],
    cwd: options.cwd,
    positionals: ctx.positionals,
    passthrough: [],
    options: { ...options },
    json: true,
  });
  const entries = (outdated.data?.packages ?? []) as OutdatedEntry[];
  if (entries.length === 0) {
    ctx.output.line("All dependencies are up to date.");
    return 0;
  }

  const prompts = await ctx.runtime.prompts();
  const pc = ctx.output.colors;
  const picked = await prompts.multiselect({
    message: "Select packages to update",
    options: entries.map((entry) => ({
      value: entry.name,
      label: `${entry.name} ${pc.dim(entry.current)} → ${options.latest ? entry.latest : entry.update}`,
      hint: entry.workspace ? `${entry.dependencyType}, ${entry.workspace}` : entry.dependencyType,
    })),
    required: false,
  });
  if (prompts.isCancel(picked) || !Array.isArray(picked) || picked.length === 0) {
    ctx.output.line("No packages selected.");
    return 0;
  }

  const byName = new Map(entries.map((entry) => [entry.name, entry]));
  const packages = picked.map((name) => {
    const entry = byName.get(name);
    return options.latest && entry ? `${name}@${entry.latest}` : name;
  });
  const listener = startListener(ctx, options);
  let summary: InstallSummary;
  try {
    summary = await core.install({ ...options, packages }, listener.sink);
  } finally {
    listener.progress?.stop();
  }
  return finishInstall(ctx, { ...options, packages }, summary);
}
