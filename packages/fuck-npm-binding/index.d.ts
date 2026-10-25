/**
 * The JS API of the fnpm native core (crates/fnpm-napi). See
 * docs/ARCHITECTURE.md, "napi JS API and the event schema"; the CLI-side types
 * are packages/fuck-npm/src/core/types.ts and both must change together.
 *
 * Payloads are plain JSON-shaped objects (serde on the Rust side, camelCase,
 * unknown fields ignored); `cwd` must be absolute. Neither function throws: a
 * payload or callback that does not fit is a `USAGE` rejection. Work runs on
 * the core's own threads, never on the JS thread. Progress arrives in batches, at most every 50 ms, as
 * `onEvents(null, events)` through a napi ThreadsafeFunction; every batch is
 * delivered before the returned promise settles. If `onEvents` throws, the
 * exception surfaces as an uncaught exception (like an event listener's) and
 * the promise still settles. Leave `onEvents` out when nobody renders progress
 * (`--json`, `--silent`): the core then builds no events at all.
 *
 * `napi build` writes the generated declarations to native.d.ts (not committed);
 * this file is the contract.
 */

/** One progress or diagnostic event (`fnpm_types::Event`, tagged by `type`). */
export type NativeEvent = { type: string; [key: string]: unknown };

/** Receives one batch of events. `err` is always `null` (the core reports failures by rejecting). */
export type NativeEventCallback = (err: Error | null, events: NativeEvent[]) => void;

/** `error.code` on a rejected `install`/`invoke` (`fnpm_core::Error::code`). Codes are only ever added. */
export type NativeErrorCode =
  | "FROZEN_LOCKFILE"
  | "NO_MATCHING_VERSION"
  | "NOT_FOUND"
  | "INTEGRITY"
  | "NETWORK"
  | "SCRIPT_FAILED"
  | "INVALID_PACKAGE_JSON"
  | "USAGE"
  | "NOT_IMPLEMENTED"
  | "CORE_ERROR";

/**
 * A rejection. `USAGE` also covers a payload that does not match the Rust
 * types, a relative `cwd` and an `onEvents` that is not a function;
 * `NOT_IMPLEMENTED` is a command of a later parity phase; `CORE_ERROR` also
 * covers a panic inside the core.
 */
export interface NativeError extends Error {
  code: NativeErrorCode;
}

/** Version of the native core (the crate version); the CLI warns when it differs from its own. */
export declare function version(): string;

/** The install pipeline: install, ci, add, remove, update. Resolves with `InstallSummary`. */
export declare function install(
  options: Record<string, unknown>,
  onEvents?: NativeEventCallback,
): Promise<Record<string, unknown>>;

/** Every other command: `{command: ["pm", "cache", "rm"], cwd, positionals, passthrough, options, json}`. */
export declare function invoke(
  request: Record<string, unknown>,
  onEvents?: NativeEventCallback,
): Promise<{ exitCode: number; data?: Record<string, unknown>; lines?: string[] }>;

/** napi-rs `<platform>-<arch>[-gnu|-musl|-msvc]` suffix this loader resolves. */
export declare function __platformArchAbi(
  platform?: NodeJS.Platform,
  arch?: string,
  musl?: () => boolean,
): string | null;

/** Whether `dir` (default: this package) is the repository checkout rather than an installed copy; development builds are only loaded then. */
export declare function __inRepository(dir?: string): boolean;
