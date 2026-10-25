/**
 * The contract between the CLI and the native core (@fuck-npm/binding).
 *
 * Commands only ever talk to `Core`; `load.ts` adapts the napi module to it
 * and `fake.ts` implements it for tests. Everything crossing the boundary is
 * plain JSON-shaped data, so the Rust side can serde it directly.
 */

export type LogLevel = "silent" | "quiet" | "default" | "verbose";
export type Linker = "isolated" | "hoisted";
export type Backend = "clonefile" | "hardlink" | "symlink" | "copyfile";
export type OmitKind = "dev" | "optional" | "peer";
export type CaStore = "system" | "openssl" | "bundled";
export type DependencyGroup =
  | "dependencies"
  | "devDependencies"
  | "optionalDependencies"
  | "peerDependencies";

/** bun's SHARED_PARAMS, validated and normalized. */
export type PmOptions = {
  cwd: string;
  /** Config file path (fnpm.toml); `true` when `--config` was passed bare. */
  config?: string | true;
  yarn: boolean;
  production: boolean;
  /** false with --no-save. link/unlink default to false. */
  save: boolean;
  ca: string[];
  cafile?: string;
  dryRun: boolean;
  frozenLockfile: boolean;
  force: boolean;
  cacheDir?: string;
  noCache: boolean;
  logLevel: LogLevel;
  progress: boolean;
  summary: boolean;
  verify: boolean;
  offline: boolean;
  preferOffline: boolean;
  ignoreScripts: boolean;
  trust: boolean;
  global: boolean;
  envFile: string[];
  noEnvFile: boolean;
  backend?: Backend;
  registry?: string;
  concurrentScripts?: number;
  networkConcurrency?: number;
  saveTextLockfile: boolean;
  omit: OmitKind[];
  lockfileOnly: boolean;
  linker?: Linker;
  /** Milliseconds; absent when unset or 0 (no gate). */
  minimumReleaseAgeMs?: number;
  minimumReleaseAgeExcludes?: string[];
  cpu: string[];
  os: string[];
  libc: string[];
  caStore?: CaStore;
};

export type InstallCommand = "install" | "add" | "remove" | "update";

export type InstallOptions = PmOptions & {
  command: InstallCommand;
  /** Dependency specs (add/update) or names (remove). */
  packages: string[];
  /** add: group to write to (dev > optional > peer > dependencies). */
  group?: DependencyGroup;
  saveProd: boolean;
  exact: boolean;
  filter: string[];
  analyze: boolean;
  onlyMissing: boolean;
  /** add: catalog name; "" is the default catalog. */
  catalog?: string;
  /** update */
  latest: boolean;
  recursive: boolean;
  updateGroups: { dev: boolean; prod: boolean; noOptional: boolean };
  depth?: 0;
};

export type PackageRef = { name: string; version: string };

export type InstallSummary = {
  /** New direct dependencies (`+ name@version`). */
  added: (PackageRef & { alias?: string; availableVersion?: string })[];
  /** Direct dependencies whose version changed (`↑ name from → to`). */
  updated: { name: string; from: string; to: string; availableVersion?: string }[];
  /** Direct dependencies removed. */
  removed: string[];
  /** Packages the add/update request resolved to, with the binaries they link. */
  requested: (PackageRef & { realName?: string; bins: string[]; skipped?: boolean })[];
  /** Packages written to node_modules. */
  installedCount: number;
  removedCount: number;
  /** For a no-op: how many installs were checked across how many packages. */
  checked?: { installs: number; packages: number };
  failedCount: number;
  blockedScripts: number;
  elapsedMs: number;
  /** Lockfile and node_modules were already in sync; nothing written. */
  noop: boolean;
};

/** Progress and diagnostics streamed while the core works (batched ≤ every 50 ms). */
export type CoreEvent =
  | { type: "resolve"; resolved: number; total: number; name?: string }
  | { type: "download"; done: number; total: number; bytes?: number; name?: string }
  | { type: "extract"; done: number; total: number; name?: string }
  | { type: "link"; done: number; total: number }
  | {
      type: "script";
      package: string;
      script: string;
      phase: "start" | "end";
      exitCode?: number;
    }
  | { type: "warn"; message: string; code?: string }
  | { type: "error"; message: string; code?: string }
  | { type: "summary"; summary: InstallSummary };

export type EventSink = (event: CoreEvent) => void;

/**
 * `error.code` on a rejected install/invoke (fnpm_core::Error::code). Codes are
 * only ever added, never renamed; anything else surfaces as CORE_ERROR.
 */
export type CoreErrorCode =
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

/** Any command other than the install pipeline. */
export type CoreRequest = {
  /** Command path: `["pm", "cache", "rm"]`, or `["lock", "migrate"]` for fnpm-lock. */
  command: string[];
  cwd: string;
  positionals: string[];
  /** Arguments for a child process (run, exec, x), verbatim. */
  passthrough: string[];
  /** Flags, validated and keyed by camelCase name. */
  options: Record<string, unknown>;
  /** The output mode, so the core knows whether to render tables itself. */
  json: boolean;
};

export type CoreResult = {
  exitCode: number;
  /** Structured result; becomes the `--json` payload. */
  data?: Record<string, unknown>;
  /** Pre-rendered human output for plain/rich modes. */
  lines?: string[];
};

export type OutdatedEntry = {
  name: string;
  current: string;
  update: string;
  latest: string;
  dependencyType: DependencyGroup;
  workspace?: string;
};

export interface Core {
  /** Version of the native core. */
  version(): string;
  /** The install pipeline: install, ci, add, remove and update. */
  install(options: InstallOptions, onEvent?: EventSink): Promise<InstallSummary>;
  /** Every other command. */
  invoke(request: CoreRequest, onEvent?: EventSink): Promise<CoreResult>;
}
