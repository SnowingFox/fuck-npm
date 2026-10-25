/**
 * Error codes surfaced in `--json` failures (`{ok: false, error: {code}}`).
 * Scripts branch on these, so renaming one is a breaking change.
 */
export type ErrorCode =
  | "USAGE"
  | "UNKNOWN_COMMAND"
  | "UNKNOWN_FLAG"
  | "INVALID_FLAG_VALUE"
  | "FLAG_CONFLICT"
  | "MISSING_ARGUMENT"
  | "CORE_UNAVAILABLE"
  | "CORE_ERROR"
  | "NOT_INTERACTIVE"
  | "CANCELLED"
  | "INTERNAL";

/**
 * A failure whose message is the whole story: the user did something we can
 * name, so no stack trace, just the message plus a pointer to the right help
 * page.
 */
export class SilentError extends Error {
  readonly code: ErrorCode;

  constructor(
    message: string,
    options?: { cause?: unknown; code?: ErrorCode },
  ) {
    super(message);
    this.name = "SilentError";
    this.code = options?.code ?? "USAGE";
    this.cause = options?.cause;
  }
}

/** A failure reported by the native core. `code` is the core's own code. */
export class CoreError extends Error {
  readonly code: string;

  constructor(message: string, code = "CORE_ERROR", options?: { cause?: unknown }) {
    super(message);
    this.name = "CoreError";
    this.code = code;
    this.cause = options?.cause;
  }
}

export function formatError(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

export function errorCode(error: unknown): string {
  if (error instanceof SilentError || error instanceof CoreError) {
    return error.code;
  }
  return "INTERNAL";
}
