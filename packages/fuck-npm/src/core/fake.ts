import type {
  Core,
  CoreEvent,
  CoreRequest,
  CoreResult,
  InstallOptions,
  InstallSummary,
} from "./types.ts";

/**
 * An in-memory Core for tests. Records every call; behaviour is scripted per
 * test through `install` / `invoke` overrides. Never imported by the CLI, so it
 * is not in the bundle.
 */
export type FakeCore = Core & {
  installs: InstallOptions[];
  requests: CoreRequest[];
};

export type FakeCoreScript = {
  version?: string;
  install?: (options: InstallOptions, emit: (event: CoreEvent) => void) => InstallSummary | Promise<InstallSummary>;
  invoke?: (request: CoreRequest, emit: (event: CoreEvent) => void) => CoreResult | Promise<CoreResult>;
};

export function emptySummary(overrides: Partial<InstallSummary> = {}): InstallSummary {
  return {
    added: [],
    updated: [],
    removed: [],
    requested: [],
    installedCount: 0,
    removedCount: 0,
    failedCount: 0,
    blockedScripts: 0,
    elapsedMs: 0,
    noop: false,
    ...overrides,
  };
}

export function createFakeCore(script: FakeCoreScript = {}): FakeCore {
  const installs: InstallOptions[] = [];
  const requests: CoreRequest[] = [];
  return {
    installs,
    requests,
    version: () => script.version ?? "0.0.1",
    async install(options, onEvent) {
      installs.push(options);
      const emit = (event: CoreEvent) => onEvent?.(event);
      if (script.install) {
        return script.install(options, emit);
      }
      emit({ type: "resolve", resolved: 1, total: 1 });
      emit({ type: "download", done: 1, total: 1, bytes: 1024 });
      emit({ type: "extract", done: 1, total: 1 });
      emit({ type: "link", done: 1, total: 1 });
      return emptySummary({
        added: options.packages.map((spec) => ({ name: spec.replace(/(.)@.*$/, "$1"), version: "1.0.0" })),
        installedCount: Math.max(1, options.packages.length),
        elapsedMs: 12,
      });
    },
    async invoke(request, onEvent) {
      requests.push(request);
      const emit = (event: CoreEvent) => onEvent?.(event);
      if (script.invoke) {
        return script.invoke(request, emit);
      }
      return { exitCode: 0, data: { command: request.command }, lines: [`ran ${request.command.join(" ")}`] };
    },
  };
}
