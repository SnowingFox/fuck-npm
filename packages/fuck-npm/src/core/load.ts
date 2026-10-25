import { createRequire } from "node:module";
import { CoreError, SilentError } from "../errors.ts";
import type {
  Core,
  CoreEvent,
  CoreRequest,
  CoreResult,
  EventSink,
  InstallOptions,
  InstallSummary,
} from "./types.ts";

/** What the napi module exports (see packages/fuck-npm-binding/index.d.ts). */
export type NativeBinding = {
  version(): string;
  install(options: InstallOptions, onEvents?: (...args: unknown[]) => void): Promise<unknown>;
  invoke(request: CoreRequest, onEvents?: (...args: unknown[]) => void): Promise<unknown>;
};

const BINDING = "@fuck-npm/binding";

/**
 * Load the native core. Synchronous `require` under the hood (the fastest way
 * to dlopen a .node), but only called from a command that needs the core, so
 * `--help`, `--version` and usage errors never pay for it.
 */
export function loadCore(requireFrom: string = import.meta.url): Core {
  const require = createRequire(requireFrom);
  let native: unknown;
  try {
    native = require(BINDING);
  } catch (error) {
    const detail =
      error instanceof Error && (error as { code?: string }).code === "MODULE_NOT_FOUND"
        ? `${BINDING} is not installed.`
        : error instanceof Error
          ? error.message
          : String(error);
    throw new SilentError(
      `fnpm's native core could not be loaded.\n${detail}`,
      { code: "CORE_UNAVAILABLE", cause: error },
    );
  }
  return wrapNative(native);
}

export function wrapNative(native: unknown): Core {
  if (!isNativeBinding(native)) {
    throw new SilentError(
      `${BINDING} loaded, but it does not export install/invoke/version. Rebuild the native core.`,
      { code: "CORE_UNAVAILABLE" },
    );
  }
  return {
    version: () => native.version(),
    install: async (options, onEvent) =>
      callNative(() => native.install(options, eventCallback(onEvent))) as Promise<InstallSummary>,
    invoke: async (request, onEvent) =>
      callNative(() => native.invoke(request, eventCallback(onEvent))) as Promise<CoreResult>,
  };
}

function isNativeBinding(value: unknown): value is NativeBinding {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const candidate = value as Record<string, unknown>;
  return (
    typeof candidate.install === "function" &&
    typeof candidate.invoke === "function" &&
    typeof candidate.version === "function"
  );
}

/**
 * napi ThreadsafeFunctions call back either `(events)` or, with callee-handled
 * errors, `(err, events)`; events arrive batched as an array. No callback is
 * registered when nobody listens (`--json`, `--silent`), which lets the core
 * skip building events entirely.
 */
export function eventCallback(onEvent?: EventSink): ((...args: unknown[]) => void) | undefined {
  if (onEvent === undefined) {
    return undefined;
  }
  return (...args: unknown[]) => {
    const payload = args.length >= 2 ? args[1] : args[0];
    if (args.length >= 2 && args[0] instanceof Error) {
      onEvent({ type: "error", message: args[0].message });
      return;
    }
    const events = Array.isArray(payload) ? payload : [payload];
    for (const event of events) {
      if (typeof event === "object" && event !== null && "type" in event) {
        onEvent(event as CoreEvent);
      }
    }
  };
}

async function callNative(call: () => Promise<unknown>): Promise<unknown> {
  try {
    return await call();
  } catch (error) {
    if (error instanceof Error) {
      const code = (error as { code?: unknown }).code;
      throw new CoreError(error.message, typeof code === "string" ? code : "CORE_ERROR", {
        cause: error,
      });
    }
    throw new CoreError(String(error));
  }
}
