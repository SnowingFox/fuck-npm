import type { CliRuntime, CliRuntimeOverrides, PromptPort } from "./types.ts";

/** The real runtime. The only place that reads `process` state. */
export function processRuntime(overrides: CliRuntimeOverrides = {}): CliRuntime {
  return {
    env: process.env,
    cwd: process.cwd(),
    platform: process.platform,
    stdout: process.stdout,
    stderr: process.stderr,
    stdoutIsTTY: process.stdout.isTTY === true,
    stderrIsTTY: process.stderr.isTTY === true,
    // A getter: touching process.stdin constructs a socket (~1 ms), and only
    // interactive commands ever ask.
    get stdinIsTTY() {
      return process.stdin.isTTY === true;
    },
    columns: () => process.stderr.columns ?? process.stdout.columns ?? 80,
    now: () => performance.now(),
    setTimer: (callback, ms) => {
      const handle = setTimeout(callback, ms);
      handle.unref();
      return { cancel: () => clearTimeout(handle) };
    },
    loadCore: async () => {
      const { loadCore } = await import("../core/load.ts");
      return loadCore();
    },
    prompts: loadClackPrompts,
    ...overrides,
  };
}

/** @clack/prompts, adapted to PromptPort. Dynamic so it is its own lazy chunk. */
async function loadClackPrompts(): Promise<PromptPort> {
  const clack = await import("@clack/prompts");
  return {
    multiselect: (options) => clack.multiselect(options as Parameters<typeof clack.multiselect>[0]) as never,
    confirm: (options) => clack.confirm(options),
    text: (options) => clack.text(options),
    isCancel: (value) => clack.isCancel(value),
    intro: (message) => clack.intro(message),
    outro: (message) => clack.outro(message),
  };
}
