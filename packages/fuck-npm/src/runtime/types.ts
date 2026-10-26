import type { Core } from "../core/types.ts";

/** Minimal writable: what the CLI needs from stdout/stderr. */
export type TextSink = { write(chunk: string): unknown };

/**
 * Prompt surface, narrowed to what the CLI uses. Injected (and loaded lazily
 * in production) so interactive flows are testable with scripted answers and
 * @clack/prompts stays off the hot path.
 */
export type PromptPort = {
  multiselect<T extends string>(options: {
    message: string;
    options: { value: T; label: string; hint?: string }[];
    initialValues?: T[];
    required?: boolean;
  }): Promise<T[] | symbol>;
  confirm(options: { message: string; initialValue?: boolean }): Promise<boolean | symbol>;
  text(options: {
    message: string;
    placeholder?: string;
    defaultValue?: string;
    initialValue?: string;
  }): Promise<string | symbol>;
  isCancel(value: unknown): boolean;
  intro(message: string): void;
  outro(message: string): void;
};

export type Timer = { cancel(): void };

/**
 * Every ambient dependency the CLI touches, in one injectable object. Only
 * `processRuntime()` reads process state; tests drive whole commands through
 * `runCli` with a fake one and never mock modules.
 */
export type CliRuntime = {
  env: NodeJS.ProcessEnv;
  cwd: string;
  platform: NodeJS.Platform;
  stdout: TextSink;
  stderr: TextSink;
  stdoutIsTTY: boolean;
  stderrIsTTY: boolean;
  stdinIsTTY: boolean;
  /** Terminal width for the progress line; 80 when unknown. */
  columns: () => number;
  now: () => number;
  /** One-shot timer (unref'd in production so it never holds the process open). */
  setTimer: (callback: () => void, ms: number) => Timer;
  /** Cleanup terminal state before exit or termination; returns an unsubscribe handle. */
  onExit?: (callback: () => void) => Timer;
  /** Load the native core. Called only by commands that need it. */
  loadCore: () => Promise<Core>;
  /** Load the prompt implementation. Called only on interactive paths. */
  prompts: () => Promise<PromptPort>;
  /** Raw key input for the `update -i` picker; stdin when absent. */
  keys?: () => AsyncIterable<Uint8Array | string>;
};

export type CliRuntimeOverrides = Partial<CliRuntime>;
