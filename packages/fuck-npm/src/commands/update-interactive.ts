import { createColors } from "picocolors";
import { SilentError } from "../errors.ts";
import { installOptions } from "../core/options.ts";
import type { InstallSummary, OutdatedEntry } from "../core/types.ts";
import { colorEnabled } from "../output/output.ts";
import type { CommandContext } from "./context.ts";
import { finishInstall, printHeader, startListener } from "./install.ts";
import { initialState, picksLatest, press, render, targetOf, type PickerState } from "./update-picker.ts";

/**
 * `fnpm update -i` (bun `update_interactive_command.rs`): ask the core what is outdated, show
 * the picker, rewrite the chosen literals in package.json (keeping `^`/`~`/`npm:`), then run
 * `fnpm update <picked names>`. With `--dry-run`, print the moves instead.
 *
 * The keys come from stdin in raw mode, or from a pipe when FNPM_INTERNAL_INTERACTIVE_ASSUME_TTY
 * is set (tests); the end of the input confirms, like Enter.
 */
export async function runInteractiveUpdate(ctx: CommandContext): Promise<number> {
  if (ctx.output.mode === "json") {
    throw new SilentError(
      "`fnpm update --interactive` cannot be combined with --json. Pass the packages to update instead: fnpm update <pkg...>",
      { code: "NOT_INTERACTIVE" },
    );
  }
  const assumeTty = (ctx.runtime.env.FNPM_INTERNAL_INTERACTIVE_ASSUME_TTY ?? "") !== "";
  if (!assumeTty && !ctx.runtime.stdinIsTTY) {
    throw new SilentError(
      "`fnpm update --interactive` needs a terminal. Pass the packages to update instead: fnpm update <pkg...>",
      { code: "NOT_INTERACTIVE" },
    );
  }

  const options = installOptions("update", ctx.values, ctx.positionals, ctx.runtime.cwd);
  printHeader(ctx, options, "update --interactive");
  const core = await ctx.runtime.loadCore();
  const started = ctx.runtime.now();
  const outdated = await core.invoke({
    command: ["outdated"],
    cwd: options.cwd,
    positionals: ctx.positionals,
    passthrough: [],
    options: { ...options },
    json: true,
  });
  const all = (outdated.data?.packages ?? []) as OutdatedEntry[];
  // --dev / --prod / --no-optional pick the groups that are offered.
  const groups = options.updateGroups;
  const offered = (type: string) => {
    if (groups.dev || groups.prod) {
      return (groups.dev && type === "devDependencies") || (groups.prod && type === "dependencies");
    }
    return !(groups.noOptional && type === "optionalDependencies");
  };
  const entries = all.filter((entry) => offered(entry.dependencyType));
  const pc = createColors(colorEnabled(ctx.runtime, ctx.runtime.stdoutIsTTY));
  if (entries.length === 0) {
    const checked = Number(outdated.data?.checked ?? all.length);
    if (options.logLevel !== "silent") {
      const flags = [groups.dev && "--dev", groups.prod && "--prod", groups.noOptional && "--no-optional"].filter(Boolean);
      const what = flags.length === 0 ? "nothing to update " : `none selected by ${flags.join(" ")} ${pc.dim("(no changes)")} `;
      ctx.output.line(
        `\nChecked ${pc.green(String(checked))} dependenc${checked === 1 ? "y" : "ies"}, ${what}${pc.dim(`[${(ctx.runtime.now() - started).toFixed(2)}ms]`)}`,
      );
    }
    return 0;
  }

  const state = initialState(entries, Number(ctx.runtime.env.LINES ?? 0) || 24, options.latest === true);
  const confirmed = await pick(ctx, state, pc);
  if (!confirmed) {
    ctx.output.line(`\n${pc.red("x")} Cancelled`);
    return 1;
  }

  type Choice = { name: string; version: string; inRange: boolean; workspace?: string; catalog?: string };
  const choices: Choice[] = [];
  const names: string[] = [];
  const rows: [string, string, string][] = [];
  state.rows.forEach((row, i) => {
    if (!state.selected[i]) return;
    const target = targetOf(state, i);
    if (target === row.current) return;
    // A peer dependency is not something `fnpm update <name>` updates; package.json names it.
    if (row.dependencyType !== "peerDependencies" && !names.includes(row.name)) names.push(row.name);
    rows.push([row.name, row.current, target]);
    const catalog = row.workspace?.startsWith("catalog") ? /^catalog(?::(\S+))?/.exec(row.workspace)?.[1] ?? "" : undefined;
    choices.push({
      name: row.name,
      version: target,
      inRange: !picksLatest(state, i) || target === row.update,
      ...(catalog === undefined && row.workspace ? { workspace: row.workspace } : {}),
      ...(catalog !== undefined ? { catalog } : {}),
    });
  });
  if (choices.length === 0) {
    ctx.output.line(`${pc.yellow("!")} No packages selected for update`);
    return 0;
  }
  if (options.dryRun) {
    rows.sort((a, b) => a[0].localeCompare(b[0]) || a[1].localeCompare(b[1]));
    const [glyph, arrow] = ctx.runtime.stdoutIsTTY ? ["↑", "→"] : ["^", "->"];
    ctx.output.line("");
    for (const [name, from, to] of rows) {
      ctx.output.line(`${pc.cyan(glyph)} ${pc.bold(name)} ${pc.dim(`${from} ${arrow}`)} ${pc.bold(pc.cyan(to))}`);
    }
    const n = rows.length;
    ctx.output.line(
      `\n${pc.green(String(n))} package${n === 1 ? "" : "s"} would be updated ${pc.dim(`[${(ctx.runtime.now() - started).toFixed(2)}ms]`)}`,
    );
    return 0;
  }

  ctx.output.line(`\n${pc.cyan("Installing updates...")}`);
  await core.invoke({
    command: ["pm", "update-interactive-apply"],
    cwd: options.cwd,
    positionals: [],
    passthrough: [],
    options: { ...options, choices },
    json: true,
  });
  // package.json already names the chosen versions; `--latest` would re-pin every row.
  const install =
    names.length > 0
      ? { ...options, latest: false, packages: names }
      : { ...options, command: "install" as const, latest: false, packages: [] };
  const listener = startListener(ctx, install);
  let summary: InstallSummary;
  try {
    summary = await core.install(install, listener.sink);
  } finally {
    listener.progress?.stop();
  }
  return finishInstall(ctx, install, summary);
}

/** Draws the picker and applies keys until Enter, the end of the input, or a cancel. */
async function pick(ctx: CommandContext, state: PickerState, pc: ReturnType<typeof createColors>): Promise<boolean> {
  const out = ctx.runtime.stdout;
  const width = ctx.runtime.columns();
  const paint = {
    bold: pc.bold,
    dim: pc.dim,
    cyan: pc.cyan,
    red: pc.red,
    yellow: pc.yellow,
    green: pc.green,
    underline: pc.underline,
  };
  const colors = colorEnabled(ctx.runtime, ctx.runtime.stdoutIsTTY);
  let drawn = 0;
  const draw = () => {
    const lines = render(state, width, paint);
    let text = drawn > 0 ? `\x1b[${drawn}A\x1b[1G\x1b[0J` : "";
    text += lines.map((l) => `${l}\x1b[0K`).join("\n") + "\n";
    out.write(text);
    drawn = lines.length;
  };
  const finish = (confirmed: boolean) => {
    out.write(`\x1b[${drawn}A\x1b[1G\x1b[0J`);
    if (colors) out.write("\x1b[?1006l\x1b[?1000l");
    if (confirmed) {
      const count = state.selected.filter(Boolean).length;
      out.write(`${pc.green("✓")} Selected ${count} package${count === 1 ? "" : "s"} to update\n`);
    }
    return confirmed;
  };
  if (colors) out.write("\x1b[?1000h\x1b[?1006h");
  draw();

  const keys = ctx.runtime.keys?.() ?? stdinKeys();
  let pending = "";
  for await (const chunk of keys) {
    pending += typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("latin1");
    for (;;) {
      const { result, used } = press(state, pending);
      if (used === 0) break;
      pending = pending.slice(used);
      if (result === "confirm") return finish(true);
      if (result === "cancel") return finish(false);
    }
    draw();
  }
  // The end of the input confirms.
  return finish(true);
}

async function* stdinKeys(): AsyncGenerator<Buffer> {
  const stdin = process.stdin;
  const raw = stdin.isTTY === true;
  if (raw) stdin.setRawMode(true);
  try {
    for await (const chunk of stdin) {
      yield chunk as Buffer;
    }
  } finally {
    if (raw) stdin.setRawMode(false);
    stdin.pause();
  }
}
