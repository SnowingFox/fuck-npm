import { isAbsolute, resolve } from "node:path";
import { SilentError } from "../errors.ts";
import { read, type ParsedFlags } from "../cli/parse-flags.ts";
import type {
  Backend,
  CaStore,
  DependencyGroup,
  InstallCommand,
  InstallOptions,
  Linker,
  LogLevel,
  OmitKind,
  PmOptions,
} from "./types.ts";

/**
 * Flag values → validated, typed options, with bun's messages for invalid
 * values and conflicting flags (CommandLineArguments.rs). Validation happens
 * here, before the core is loaded, so a typo costs no native startup.
 */

type Values = ParsedFlags["values"];

const invalid = (message: string): SilentError =>
  new SilentError(message, { code: "INVALID_FLAG_VALUE" });
const conflict = (message: string): SilentError =>
  new SilentError(message, { code: "FLAG_CONFLICT" });

const VALID_CPU = ["arm", "arm64", "ia32", "mips", "mipsel", "ppc", "ppc64", "s390", "s390x", "x32", "x64"];
const VALID_OS = ["aix", "darwin", "freebsd", "linux", "openbsd", "sunos", "win32", "android"];
const VALID_LIBC = ["glibc", "musl"];
const BACKENDS: Backend[] = ["clonefile", "hardlink", "symlink", "copyfile"];

/** `--cwd` is resolved against the starting directory, like bun's chdir. */
export function resolveCwd(values: Values, startCwd: string): string {
  const cwd = read.string(values, "cwd");
  if (cwd === undefined || cwd === "") {
    return startCwd;
  }
  return isAbsolute(cwd) ? cwd : resolve(startCwd, cwd);
}

export function logLevelOf(values: Values): LogLevel {
  if (read.bool(values, "silent")) return "silent";
  if (read.bool(values, "verbose")) return "verbose";
  if (read.bool(values, "quiet")) return "quiet";
  return "default";
}

function parseLinker(value: string | undefined): Linker | undefined {
  if (value === undefined) return undefined;
  if (value === "isolated" || value === "hoisted") return value;
  throw invalid(`Invalid value for --linker: "${value}". Must be 'isolated' or 'hoisted'.`);
}

function parseOmit(values: string[]): OmitKind[] {
  const out: OmitKind[] = [];
  for (const value of values.flatMap((entry) => entry.split(","))) {
    const trimmed = value.trim();
    if (trimmed === "dev" || trimmed === "optional" || trimmed === "peer") {
      if (!out.includes(trimmed)) out.push(trimmed);
      continue;
    }
    throw invalid(`invalid \`omit\` value: '${trimmed}'`);
  }
  return out;
}

function parseU16(name: string, value: string | undefined): number | undefined {
  if (value === undefined) return undefined;
  const parsed = /^\d+$/.test(value) ? Number(value) : Number.NaN;
  if (!Number.isInteger(parsed) || parsed < 0 || parsed > 65535) {
    throw invalid(`Expected --${name} to be a number between 0 and 65535: ${value}`);
  }
  return parsed;
}

/** `--concurrent-scripts`: 0 or anything unparsable means the default. */
function parseConcurrentScripts(value: string | undefined): number | undefined {
  if (value === undefined || !/^\d+$/.test(value)) return undefined;
  const parsed = Number(value);
  return parsed > 0 ? parsed : undefined;
}

const UNIT_SECONDS: Record<string, number> = {
  s: 1, sec: 1, secs: 1, second: 1, seconds: 1,
  m: 60, min: 60, mins: 60, minute: 60, minutes: 60,
  h: 3600, hr: 3600, hrs: 3600, hour: 3600, hours: 3600,
  d: 86400, day: 86400, days: 86400,
  w: 604800, week: 604800, weeks: 604800,
};

/** bun's parse_seconds: a number of seconds, or `90m`, `12 hours`, `3d`, `2 weeks`. Returns ms. */
export function parseReleaseAge(value: string | undefined): number | undefined {
  if (value === undefined) return undefined;
  const match = /^\s*(\d+(?:\.\d+)?)\s*([a-z]*)\s*$/i.exec(value);
  const unit = match?.[2]?.toLowerCase() ?? "";
  const factor = unit === "" ? 1 : UNIT_SECONDS[unit];
  if (!match || factor === undefined) {
    throw invalid(`Expected --minimum-release-age to be a positive number: ${value}`);
  }
  const ms = Math.round(Number(match[1]) * factor * 1000);
  return ms === 0 ? undefined : ms;
}

/**
 * --cpu/--os/--libc: repeatable, comma-separated, `*` / `any` / `none`, and
 * `!name` to negate. Unknown names are fatal with the valid list.
 */
function parsePlatformList(what: string, values: string[], valid: string[]): string[] {
  const out: string[] = [];
  for (const raw of values.flatMap((entry) => entry.split(","))) {
    const value = raw.trim();
    if (value === "") continue;
    const name = value.replace(/^!+/, "");
    if (value === "*" || value === "any" || value === "none" || valid.includes(name)) {
      out.push(value);
      continue;
    }
    throw invalid(
      `invalid ${what}: '${value}'. valid values: *, any, ${valid.join(", ")}. use !name to negate`,
    );
  }
  return out;
}

function parseRegistry(value: string | undefined): string | undefined {
  if (value === undefined) return undefined;
  if (/^https?:\/\//.test(value)) return value;
  throw invalid(`Registry URL must start with 'https://' or 'http://': "${redactUrl(value)}"`);
}

/** Never echo credentials back: strip userinfo and anything that looks like a token. */
function redactUrl(value: string): string {
  return value.replace(/\/\/[^/@]*@/, "//<redacted>@").replace(/(_authToken|token)=[^&]*/gi, "$1=<redacted>");
}

function caStoreOf(values: Values): CaStore | undefined {
  if (read.bool(values, "useSystemCa")) return "system";
  if (read.bool(values, "useOpensslCa")) return "openssl";
  if (read.bool(values, "useBundledCa")) return "bundled";
  return undefined;
}

/** bun's SHARED_PARAMS → PmOptions. `saveDefault` is false for link/unlink. */
export function pmOptions(values: Values, startCwd: string, saveDefault = true): PmOptions {
  const production = read.bool(values, "production") || read.bool(values, "prod");
  const frozenLockfile = read.bool(values, "frozenLockfile");
  const trust = read.bool(values, "trust");
  if (production && trust) {
    throw conflict("The '--production' and '--trust' flags together are not supported because the --trust flag potentially modifies the lockfile after installing packages");
  }
  if (frozenLockfile && trust) {
    throw conflict("The '--frozen-lockfile' and '--trust' flags together are not supported because the --trust flag potentially modifies the lockfile after installing packages");
  }

  const backendValue = read.string(values, "backend");
  // bun silently ignores an unknown or unsupported backend.
  const backend = BACKENDS.find((candidate) => candidate === backendValue);
  const config = values.config;
  const releaseAge = parseReleaseAge(read.string(values, "minimumReleaseAge"));
  const excludes = read.strings(values, "minimumReleaseAgeExcludes");
  const save = values.noSave === true ? false : values.save === true ? true : saveDefault;

  const options: PmOptions = {
    cwd: resolveCwd(values, startCwd),
    yarn: read.bool(values, "yarn"),
    production,
    save,
    ca: read.strings(values, "ca"),
    dryRun: read.bool(values, "dryRun"),
    frozenLockfile,
    force: read.bool(values, "force"),
    noCache: read.bool(values, "noCache"),
    logLevel: logLevelOf(values),
    progress: values.noProgress !== true,
    summary: values.noSummary !== true,
    verify: values.noVerify !== true,
    offline: read.bool(values, "offline"),
    preferOffline: read.bool(values, "preferOffline"),
    ignoreScripts: read.bool(values, "ignoreScripts"),
    trust,
    global: read.bool(values, "global"),
    envFile: read.strings(values, "envFile"),
    noEnvFile: read.bool(values, "noEnvFile"),
    saveTextLockfile: read.bool(values, "saveTextLockfile"),
    omit: parseOmit(read.strings(values, "omit")),
    lockfileOnly: read.bool(values, "lockfileOnly"),
    cpu: parsePlatformList("cpu", read.strings(values, "cpu"), VALID_CPU),
    os: parsePlatformList("os", read.strings(values, "os"), VALID_OS),
    libc: parsePlatformList("libc", read.strings(values, "libc"), VALID_LIBC),
  };
  if (typeof config === "string") options.config = config;
  else if (config === true) options.config = true;
  const cafile = read.string(values, "cafile");
  if (cafile !== undefined) options.cafile = cafile;
  const cacheDir = read.string(values, "cacheDir");
  if (cacheDir !== undefined) options.cacheDir = cacheDir;
  if (backend !== undefined) options.backend = backend;
  const registry = parseRegistry(read.string(values, "registry"));
  if (registry !== undefined) options.registry = registry;
  const concurrentScripts = parseConcurrentScripts(read.string(values, "concurrentScripts"));
  if (concurrentScripts !== undefined) options.concurrentScripts = concurrentScripts;
  const networkConcurrency = parseU16("network-concurrency", read.string(values, "networkConcurrency"));
  if (networkConcurrency !== undefined) options.networkConcurrency = networkConcurrency;
  const linker = parseLinker(read.string(values, "linker"));
  if (linker !== undefined) options.linker = linker;
  if (releaseAge !== undefined) options.minimumReleaseAgeMs = releaseAge;
  if (excludes.length > 0) {
    options.minimumReleaseAgeExcludes = excludes
      .flatMap((entry) => entry.split(","))
      .map((entry) => entry.trim())
      .filter((entry) => entry !== "");
  }
  if (read.bool(values, "json")) options.json = true;
  const caStore = caStoreOf(values);
  if (caStore !== undefined) options.caStore = caStore;
  return options;
}

/** Group precedence for `add`: dev > optional > peer > dependencies. */
function addGroup(values: Values): DependencyGroup | undefined {
  if (read.bool(values, "dev") || read.bool(values, "development")) return "devDependencies";
  if (read.bool(values, "optional")) return "optionalDependencies";
  if (read.bool(values, "peer")) return "peerDependencies";
  if (read.bool(values, "saveProd")) return "dependencies";
  return undefined;
}

/** install / ci / add / remove / update → InstallOptions, with bun's conflict checks. */
export function installOptions(
  command: InstallCommand,
  values: Values,
  positionals: string[],
  startCwd: string,
  extra: { frozen?: boolean } = {},
): InstallOptions {
  const base = pmOptions(values, startCwd);
  const filter = read.strings(values, "filter");
  const catalogValue = values.catalog;
  const catalog =
    typeof catalogValue === "string" ? catalogValue.trim() : catalogValue === true ? "" : undefined;
  const analyze = read.bool(values, "analyze");

  if (base.global && filter.length > 0 && command !== "update") {
    throw conflict("--filter cannot be used with --global");
  }
  if (catalog !== undefined && base.global) {
    throw conflict("--catalog cannot be used with --global");
  }
  if (analyze && positionals.length === 0) {
    throw new SilentError("Missing script(s) to analyze. Pass paths to scripts to install.", {
      code: "MISSING_ARGUMENT",
    });
  }
  if ((command === "add" || command === "remove") && positionals.length === 0) {
    throw new SilentError(
      command === "add" ? "no package specified to add" : "no package specified to remove",
      { code: "MISSING_ARGUMENT" },
    );
  }
  if (command === "install" && catalog !== undefined && positionals.length === 0) {
    throw new SilentError("no package specified to add", { code: "MISSING_ARGUMENT" });
  }

  let depth: 0 | undefined;
  const depthValue = read.string(values, "depth");
  if (command === "update" && depthValue !== undefined) {
    if (depthValue !== "0") {
      throw invalid(
        `fnpm update --depth only accepts 0 (update direct dependencies, keep transitive dependencies locked), got '${depthValue}'`,
      );
    }
    depth = 0;
  }

  // On update, --production/--prod pick which entries to update; they do not
  // omit devDependencies or freeze the lockfile.
  const updateGroups = {
    dev: command === "update" && (read.bool(values, "dev") || read.bool(values, "development")),
    prod: command === "update" && base.production,
    noOptional: command === "update" && read.bool(values, "noOptional"),
  };
  const options: InstallOptions = {
    ...base,
    production: command === "update" ? false : base.production,
    frozenLockfile: base.frozenLockfile || extra.frozen === true,
    command,
    packages: positionals,
    saveProd: read.bool(values, "saveProd"),
    exact: read.bool(values, "exact"),
    filter,
    analyze,
    onlyMissing: read.bool(values, "onlyMissing") || analyze,
    latest: read.bool(values, "latest"),
    // --global silently disables --recursive.
    recursive: read.bool(values, "recursive") && !base.global,
    updateGroups,
  };
  if (command !== "update") {
    const group = addGroup(values);
    if (group !== undefined) options.group = group;
  }
  if (catalog !== undefined) options.catalog = catalog;
  if (depth !== undefined) options.depth = depth;
  return options;
}

/**
 * Leaf-specific numeric flags with bun's validation (`bun pm` and friends).
 * Returns the full option bag sent to the core for non-install commands.
 */
export function requestOptions(
  values: Values,
  startCwd: string,
  shared: boolean,
  saveDefault = true,
): Record<string, unknown> {
  const out: Record<string, unknown> = shared ? { ...pmOptions(values, startCwd, saveDefault) } : { cwd: resolveCwd(values, startCwd) };
  for (const [key, value] of Object.entries(values)) {
    if (!(key in out)) {
      out[key] = value;
    }
  }
  const unified = read.string(values, "unified");
  if (unified !== undefined) {
    if (!/^\d+$/.test(unified)) throw invalid(`invalid --unified value: ${unified}`);
    out.unified = Number(unified);
  }
  const maxAge = read.string(values, "maxAge");
  if (maxAge !== undefined) {
    if (!/^\d+$/.test(maxAge) || Number(maxAge) > 0xffff_ffff) throw invalid(`invalid --max-age value: ${maxAge}`);
    out.maxAge = Number(maxAge);
  }
  const depth = read.string(values, "depth");
  if (depth !== undefined) {
    if (!/^\d+$/.test(depth)) throw invalid(`invalid depth value: ${depth}`);
    out.depth = Number(depth);
  }
  const elideLines = read.string(values, "elideLines");
  if (elideLines !== undefined) {
    if (!/^\d+$/.test(elideLines)) throw invalid(`invalid --elide-lines value: ${elideLines}`);
    out.elideLines = Number(elideLines);
  }
  // `--gzip-level` is checked by the core, after the scripts that run before packing (bun).
  const gitTag = read.string(values, "gitTagVersion");
  if (gitTag !== undefined) {
    if (gitTag !== "true" && gitTag !== "false") throw invalid(`invalid --git-tag-version value: ${gitTag}`);
    out.gitTagVersion = gitTag === "true";
  }
  if (values.noGitTagVersion === true) {
    out.gitTagVersion = false;
    delete out.noGitTagVersion;
  }
  const minAge = read.string(values, "minimumReleaseAge");
  if (minAge !== undefined && !shared) {
    out.minimumReleaseAgeMs = parseReleaseAge(minAge);
    delete out.minimumReleaseAge;
  }
  const registry = read.string(values, "registry");
  if (registry !== undefined && !shared) {
    out.registry = parseRegistry(registry);
  }
  if (read.bool(values, "parallel") && read.bool(values, "sequential")) {
    throw conflict("--parallel and --sequential cannot be used together");
  }
  return out;
}
