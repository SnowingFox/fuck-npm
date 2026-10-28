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

/** FORCE_COLOR / NO_COLOR in bun's order (FORCE_COLOR wins), from the injected env. */
export function colorEnabled(runtime: CliRuntime, isTTY: boolean): boolean {
  const { env } = runtime;
  // bun's order: FORCE_COLOR wins over NO_COLOR.
  if (env.FORCE_COLOR !== undefined) return env.FORCE_COLOR !== "0" && env.FORCE_COLOR !== "false";
  if (env.NO_COLOR !== undefined && env.NO_COLOR !== "") return false;
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

/**
 * bun's `escape_control_chars` for a message that may span lines on purpose: C0 and C1
 * controls, DEL and the bidi controls are written as escapes, a newline stays one.
 */
export function escapeControlInLines(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/[\x00-\x09\x0b-\x1f\x7f-\x9f\u2028-\u202e\u2066-\u2069]/g, (c) => {
    if (c === "\r") return "\\r";
    if (c === "\t") return "\\t";
    const code = c.charCodeAt(0);
    return code < 0x80 ? `\\x${code.toString(16).padStart(2, "0")}` : `\\u${code.toString(16).padStart(4, "0")}`;
  });
}
