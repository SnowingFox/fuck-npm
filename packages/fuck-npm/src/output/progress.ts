import { createColors } from "picocolors";
import type { CoreEvent, EventSink } from "../core/types.ts";
import type { CliRuntime } from "../runtime/types.ts";
import { colorEnabled } from "./output.ts";

/**
 * A single-line progress renderer for stderr.
 *
 * Cheap by construction: events only update counters; drawing happens at most
 * once per THROTTLE_MS, with a trailing redraw so the final state of a burst is
 * shown. Warnings and errors are printed above the line as they arrive. Never
 * created unless stderr is a terminal (see `wantsProgress`).
 */

export const THROTTLE_MS = 50;

export type Progress = {
  sink: EventSink;
  /** Erase the line. Call before printing the summary. */
  stop(): void;
};

type Phase = "resolve" | "download" | "extract" | "link" | "script";

type State = {
  phase: Phase;
  resolved: number;
  resolveTotal: number;
  downloaded: number;
  downloadTotal: number;
  bytes: number;
  extracted: number;
  extractTotal: number;
  linked: number;
  linkTotal: number;
  script?: string;
};

const ERASE = "\r\u001B[2K";
const ANSI_RE = /\u001B\[[0-9;]*m/g;

export function wantsProgress(runtime: CliRuntime, opts: { silent: boolean; quiet: boolean; noProgress: boolean }): boolean {
  if (!runtime.stderrIsTTY || opts.silent || opts.quiet || opts.noProgress) return false;
  const { env } = runtime;
  // FNPM_PROGRESS (BUN_INSTALL_PROGRESS as fallback) = "0" turns it off; see docs/ARCHITECTURE.md.
  if ((env.FNPM_PROGRESS ?? env.BUN_INSTALL_PROGRESS) === "0") return false;
  return !CI_VARS.some((key) => {
    const value = env[key];
    return value !== undefined && value !== "" && value !== "0" && value !== "false";
  });
}

/** The CI markers bun checks (fnpm_config::Env::is_ci). */
const CI_VARS = ["CI", "TDDIUM", "GITHUB_ACTIONS", "JENKINS_URL", "bamboo.buildKey"] as const;

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}

export function createProgress(runtime: CliRuntime): Progress {
  const pc = createColors(colorEnabled(runtime, runtime.stderrIsTTY));
  const state: State = {
    phase: "resolve",
    resolved: 0,
    resolveTotal: 0,
    downloaded: 0,
    downloadTotal: 0,
    bytes: 0,
    extracted: 0,
    extractTotal: 0,
    linked: 0,
    linkTotal: 0,
  };
  let lastDraw = Number.NEGATIVE_INFINITY;
  let drawn = false;
  let pending: { cancel(): void } | undefined;
  let stopped = false;

  const text = (): string => {
    switch (state.phase) {
      case "resolve":
        return `${pc.cyan("Resolving")} ${state.resolved}/${state.resolveTotal || "?"}`;
      case "download":
      case "extract":
        return `${pc.cyan("Downloading")} ${state.downloaded}/${state.downloadTotal} ${pc.dim(formatBytes(state.bytes))}  ${pc.cyan("Extracted")} ${state.extracted}/${state.extractTotal || state.downloadTotal}`;
      case "link":
        return `${pc.cyan("Linking")} ${state.linked}/${state.linkTotal}`;
      case "script":
        return `${pc.cyan("Running")} ${state.script ?? ""}`;
    }
  };

  const draw = (): void => {
    pending = undefined;
    if (stopped) return;
    lastDraw = runtime.now();
    const width = Math.max(20, runtime.columns() - 1);
    const line = text();
    // Never wrap: a wrapped line cannot be erased with \r. Truncate the
    // visible text (dropping colour) when the terminal is too narrow.
    const visible = line.replaceAll(ANSI_RE, "");
    runtime.stderr.write(`${ERASE}${visible.length > width ? visible.slice(0, width) : line}`);
    drawn = true;
  };

  const schedule = (): void => {
    if (stopped || pending) return;
    const elapsed = runtime.now() - lastDraw;
    if (elapsed >= THROTTLE_MS) {
      draw();
      return;
    }
    pending = runtime.setTimer(draw, THROTTLE_MS - elapsed);
  };

  const above = (message: string): void => {
    runtime.stderr.write(`${drawn ? ERASE : ""}${message}\n`);
    drawn = false;
    schedule();
  };

  const sink = (event: CoreEvent): void => {
    switch (event.type) {
      case "resolve":
        state.phase = "resolve";
        state.resolved = event.resolved;
        state.resolveTotal = event.total;
        break;
      case "download":
        state.phase = "download";
        state.downloaded = event.done;
        state.downloadTotal = event.total;
        if (event.bytes !== undefined) state.bytes = event.bytes;
        break;
      case "extract":
        if (state.phase === "resolve") state.phase = "extract";
        state.extracted = event.done;
        state.extractTotal = event.total;
        break;
      case "link":
        state.phase = "link";
        state.linked = event.done;
        state.linkTotal = event.total;
        break;
      case "script":
        state.phase = "script";
        state.script = `${event.package} ${pc.dim(event.script)}`;
        break;
      case "warn":
        above(`${pc.yellow("warn:")} ${event.message}`);
        return;
      case "error":
        above(`${pc.red("error:")} ${event.message}`);
        return;
      case "summary":
        return;
    }
    schedule();
  };

  return {
    sink,
    stop() {
      stopped = true;
      pending?.cancel();
      pending = undefined;
      if (drawn) {
        runtime.stderr.write(ERASE);
        drawn = false;
      }
    },
  };
}

/**
 * Event handling when no progress line is drawn (piped, CI, --quiet): warnings
 * and errors still reach stderr, counters are dropped.
 */
export function diagnosticsSink(runtime: CliRuntime, silent: boolean): EventSink {
  const pc = createColors(colorEnabled(runtime, runtime.stderrIsTTY));
  return (event) => {
    if (silent) return;
    if (event.type === "warn") runtime.stderr.write(`${pc.yellow("warn:")} ${event.message}\n`);
    else if (event.type === "error") runtime.stderr.write(`${pc.red("error:")} ${event.message}\n`);
  };
}
