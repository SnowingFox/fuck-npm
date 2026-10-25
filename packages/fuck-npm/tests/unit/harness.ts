import { runCli } from "@/cli/app.ts";
import type { BinName } from "@/entry.ts";
import { SilentError } from "@/errors.ts";
import { createFakeCore, type FakeCore, type FakeCoreScript } from "@/core/fake.ts";
import type { CliRuntime, PromptPort } from "@/runtime/types.ts";

/** Strip ANSI so assertions hold whether or not colour is on. */
export const plain = (value: string): string => value.replaceAll(/\u001B\[[0-9;]*[A-Za-z]/g, "");

type Capture = { chunks: string[]; write(chunk: string): boolean; text(): string };

const capture = (): Capture => {
  const chunks: string[] = [];
  return {
    chunks,
    write(chunk: string) {
      chunks.push(String(chunk));
      return true;
    },
    text: () => chunks.join(""),
  };
};

export type TestBedOptions = {
  stdoutIsTTY?: boolean;
  stderrIsTTY?: boolean;
  stdinIsTTY?: boolean;
  env?: NodeJS.ProcessEnv;
  cwd?: string;
  /** Scripted core behaviour; `null` simulates a missing native binding. */
  core?: FakeCoreScript | null;
  prompts?: Partial<PromptPort>;
  columns?: number;
  /** What the `update -i` picker reads, as one chunk. */
  keys?: string;
};

/**
 * A whole CLI invocation against an injected runtime: fake core, captured
 * streams, a manual clock and timer queue. No module mocking.
 */
export function testBed(options: TestBedOptions = {}) {
  const stdout = capture();
  const stderr = capture();
  const core: FakeCore = createFakeCore(options.core ?? {});
  let clock = 0;
  const timers: { at: number; callback: () => void; cancelled: boolean }[] = [];
  let coreLoads = 0;

  const prompts: PromptPort = {
    multiselect: async () => [],
    confirm: async () => true,
    text: async () => "",
    isCancel: (value) => typeof value === "symbol",
    intro: () => {},
    outro: () => {},
    ...options.prompts,
  };

  const runtime: CliRuntime = {
    env: { ...options.env },
    cwd: options.cwd ?? "/work/app",
    platform: "linux",
    stdout,
    stderr,
    stdoutIsTTY: options.stdoutIsTTY ?? false,
    stderrIsTTY: options.stderrIsTTY ?? false,
    stdinIsTTY: options.stdinIsTTY ?? false,
    columns: () => options.columns ?? 120,
    now: () => clock,
    setTimer: (callback, ms) => {
      const timer = { at: clock + ms, callback, cancelled: false };
      timers.push(timer);
      return { cancel: () => (timer.cancelled = true) };
    },
    loadCore: async () => {
      coreLoads += 1;
      if (options.core === null) {
        throw new SilentError("fnpm's native core could not be loaded.\n@fuck-npm/binding is not installed.", {
          code: "CORE_UNAVAILABLE",
        });
      }
      return core;
    },
    prompts: async () => prompts,
    ...(options.keys !== undefined
      ? {
          keys: async function* () {
            yield options.keys as string;
          },
        }
      : {}),
  };

  return {
    runtime,
    core,
    stdout,
    stderr,
    coreLoads: () => coreLoads,
    /** Advance the manual clock, firing due timers. */
    advance(ms: number) {
      clock += ms;
      for (const timer of timers) {
        if (!timer.cancelled && timer.at <= clock) {
          timer.cancelled = true;
          timer.callback();
        }
      }
    },
    run: (args: string[], bin: BinName = "fnpm") => runCli(bin, ["node", bin, ...args], runtime),
    json: () => JSON.parse(stdout.text()) as Record<string, unknown>,
  };
}
