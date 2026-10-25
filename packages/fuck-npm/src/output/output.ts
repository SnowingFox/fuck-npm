import { createColors } from "picocolors";
import type { CliRuntime } from "../runtime/types.ts";

/**
 * The three output modes:
 *   - `json`:  one JSON document on stdout, success or failure; nothing else.
 *   - `rich`:  stdout is a terminal: colour, plus a live progress line on stderr.
 *   - `plain`: piped or redirected: the same text, no colour, no progress.
 */
export type OutputMode = "json" | "rich" | "plain";

export type Colors = ReturnType<typeof createColors>;

export type Output = {
  mode: OutputMode;
  colors: Colors;
  /** A line of human output on stdout. Swallowed in json mode. */
  line(text?: string): void;
  /** A diagnostic on stderr (warnings, notes). Shown in every mode. */
  note(text: string): void;
  /** The json payload. Only writes in json mode. */
  json(payload: unknown): void;
};

/** FORCE_COLOR / NO_COLOR, as picocolors reads them, but from the injected env. */
export function colorEnabled(runtime: CliRuntime, isTTY: boolean): boolean {
  const { env } = runtime;
  if (env.NO_COLOR !== undefined && env.NO_COLOR !== "") return false;
  if (env.FORCE_COLOR !== undefined) return env.FORCE_COLOR !== "0" && env.FORCE_COLOR !== "false";
  if (env.TERM === "dumb") return false;
  return isTTY;
}

export function resolveMode(runtime: CliRuntime, wantsJson: boolean): OutputMode {
  if (wantsJson) return "json";
  return runtime.stdoutIsTTY ? "rich" : "plain";
}

export function createOutput(runtime: CliRuntime, mode: OutputMode): Output {
  const colors = createColors(mode !== "json" && colorEnabled(runtime, runtime.stdoutIsTTY));
  return {
    mode,
    colors,
    line(text = "") {
      if (mode !== "json") runtime.stdout.write(`${text}\n`);
    },
    note(text) {
      runtime.stderr.write(`${text}\n`);
    },
    json(payload) {
      if (mode === "json") runtime.stdout.write(`${JSON.stringify(payload)}\n`);
    },
  };
}
