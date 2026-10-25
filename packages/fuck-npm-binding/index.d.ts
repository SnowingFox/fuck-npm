/**
 * Placeholder contract for the fnpm native core (crates/fnpm-napi).
 *
 * `napi build` writes the generated declarations to native.d.ts; this file is
 * the hand-written shape the CLI programs against until the generated one
 * replaces it. Payloads are plain objects (serde on the Rust side). Progress
 * arrives batched, at most every 50 ms, through a napi ThreadsafeFunction, so
 * the callback may be invoked as `(events)` or, with callee-handled errors, as
 * `(err, events)`.
 */

export type NativeEvent = { type: string; [key: string]: unknown };

export type NativeEventCallback =
  | ((events: NativeEvent[] | NativeEvent) => void)
  | ((err: Error | null, events: NativeEvent[] | NativeEvent) => void);

/** Version of the native core; the CLI warns when it differs from its own. */
export declare function version(): string;

/** The install pipeline: install, ci, add, remove, update. */
export declare function install(
  options: Record<string, unknown>,
  onEvents?: NativeEventCallback,
): Promise<Record<string, unknown>>;

/** Every other command: `{command: ["pm", "cache", "rm"], positionals, passthrough, options}`. */
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
