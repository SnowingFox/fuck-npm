import { spawnSync } from "node:child_process";
import { statSync } from "node:fs";
import { resolve as resolvePath } from "node:path";
import cac from "cac";
import { errorCode, formatError, SilentError } from "../errors.ts";
import { loadHandler } from "../commands/index.ts";
import { createColors } from "picocolors";
import { colorEnabled, createOutput, resolveMode } from "../output/output.ts";
import type { CliRuntime } from "../runtime/types.ts";
import { parseFlags } from "./parse-flags.ts";
import { PLAIN_USAGE, planInvocation, rejectPmSubcommandFlags, rejectUnknownFlags, unknownFlagCommand, type Invocation } from "./plan.ts";
import { setCurrentArgv } from "./raw-flags.ts";
import { registerGlobalFlags, registerNode } from "./register.ts";
import { flagSpecsFor } from "./registry.ts";
import type { CommandLeaf, CommandNode, Registry } from "./registry-types.ts";
import { getCliVersion } from "./version.ts";

/**
 * Dispatch one argv through the registry and cac.
 *
 * Order of business: `--version`, `--tree`, help (all answered from the
 * registry, with the help renderer loaded lazily), then unknown commands,
 * unknown flags, flag parsing and required positionals, and only then the
 * handler. Nothing before the handler loads the native core.
 */
const SCRIPT_EXTENSIONS = [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts"];

/** An existing file with a JavaScript or TypeScript extension. */
function isScriptFile(cwd: string, name: string): boolean {
  if (!SCRIPT_EXTENSIONS.some((ext) => name.endsWith(ext))) return false;
  try {
    return statSync(resolvePath(cwd, name)).isFile();
  } catch {
    return false;
  }
}

/** Runs `file` with the current Node, with inherited stdio; its exit status is fnpm's. */
function runScriptFile(runtime: CliRuntime, file: string, args: string[]): number {
  const result = spawnSync(process.execPath, [file, ...args], { cwd: runtime.cwd, env: runtime.env, stdio: "inherit" });
  if (result.error) throw result.error;
  return result.status ?? 1;
}

export async function runParsedCli(
  registry: Registry,
  argv: string[],
  runtime: CliRuntime,
): Promise<number> {
  setCurrentArgv(argv);
  let plan: Invocation | undefined;
  try {
    if (argv.slice(2).length === 0) {
      return await writeHelp(registry, runtime);
    }

    plan = planInvocation(argv, registry);

    // Everything after a script path belongs to the script, including help/version flags.
    const script = argv[2];
    if (registry.bin === "fnpm" && plan.matched.length === 0 && script !== undefined && isScriptFile(runtime.cwd, script)) {
      return runScriptFile(runtime, script, argv.slice(3));
    }

    if (plan.wantsVersion && plan.node.kind === "group") {
      runtime.stdout.write(`${getCliVersion()}\n`);
      return 0;
    }
    if (plan.wantsTree) {
      const { renderTree } = await import("./help.ts");
      runtime.stdout.write(renderTree(registry, { colors: stdoutColors(runtime) }));
      return 0;
    }
    if (plan.wantsHelp) {
      if (isPm(registry, plan.matched)) return await writePmHelp(runtime);
      return await writeHelp(registry, runtime, plan.matched.join(" "));
    }
    if (plan.wantsVersion) {
      runtime.stdout.write(`${getCliVersion()}\n`);
      return 0;
    }

    if (plan.matched.length === 0) {
      const typed = plan.own.find((token) => !token.startsWith("-"));
      if (typed === undefined) {
        // Only global flags (`fnpm --json`): there is nothing to run.
        return await writeHelp(registry, runtime);
      }
      throw new SilentError(`Unknown command: ${typed}`, { code: "UNKNOWN_COMMAND" });
    }

    let node: CommandNode = plan.node;
    let path = plan.matched;
    if (node.kind === "group") {
      if (plan.unknownSegment !== undefined) {
        if (registry.bin === "fnpm" && path.join(" ") === "pm cache") {
          // bun's wording: `rm`, `prune`, `pack` and `unpack` are the only words it takes.
          runtime.stderr.write(
            `error: unexpected argument "${plan.unknownSegment}"\nusage: fnpm pm cache [rm | prune | pack <file> | unpack <file>]\n`,
          );
          return 1;
        }
        if (isPm(registry, path)) {
          await writePmHelp(runtime);
          runtime.stderr.write(`\nerror: "${plan.unknownSegment}" unknown command\n`);
          return 1;
        }
        throw new SilentError(
          `Unknown command: ${[registry.bin, ...path, plan.unknownSegment].join(" ")}`,
          { code: "UNKNOWN_COMMAND" },
        );
      }
      const group = node;
      const fallback: CommandNode | undefined =
        group.defaultChild === undefined
          ? undefined
          : group.children.find((child) => child.name === group.defaultChild);
      if (fallback === undefined || fallback.kind !== "leaf") {
        // A bare group prints its own help, like `bun pm`.
        if (isPm(registry, path)) return await writePmHelp(runtime);
        return await writeHelp(registry, runtime, path.join(" "));
      }
      node = fallback;
      path = [...path, fallback.name];
      plan = { ...plan, node, matched: path };
    }
    const leaf = node as CommandLeaf;

    rejectPmSubcommandFlags(plan, argv, registry);
    rejectUnknownFlags(plan, registry);
    const parsed = parseFlags(plan.own, flagSpecsFor(leaf, registry));
    const positionals =
      leaf.passthrough === undefined ? [...parsed.positionals, ...plan.forwarded] : parsed.positionals;
    const passthrough = leaf.passthrough === undefined ? [] : plan.forwarded;
    if (leaf.rawArgs !== true) {
      requirePositionals(registry, leaf, path, positionals);
    }

    const output = createOutput(runtime, resolveMode(runtime, plan.wantsJson));
    const handler = await loadHandler(registry.bin, path);

    // cac still owns command matching and the action call; values come from
    // parseFlags because mri coerces numeric-looking strings.
    const cli = cac(registry.bin);
    registerGlobalFlags(cli, registry);
    const top = registry.root.children.find((child) => child.name === path[0]) ?? leaf;
    registerNode(cli, top, () =>
      handler({ registry, runtime, node: leaf, path, values: parsed.values, positionals, passthrough, output }),
    );
    cli.parse(plan.cacArgv, { run: false });
    return await resolveCliResult(cli.runMatchedCommand());
  } catch (error) {
    return reportError(registry, runtime, argv, plan, error);
  }
}

function requirePositionals(
  registry: Registry,
  leaf: CommandLeaf,
  path: string[],
  positionals: string[],
): void {
  const required = (leaf.positionals ?? []).filter((positional) => positional.required);
  if (positionals.length >= required.length) {
    return;
  }
  const missing = required[positionals.length];
  if (missing === undefined) {
    return;
  }
  throw new SilentError(
    `\`${[registry.bin, ...path].join(" ")}\` needs <${missing.name}>: ${missing.description}.`,
    { code: "MISSING_ARGUMENT" },
  );
}

async function writeHelp(registry: Registry, runtime: CliRuntime, path?: string): Promise<number> {
  const { renderHelp } = await import("./help.ts");
  runtime.stdout.write(renderHelp(registry, path, { colors: stdoutColors(runtime) }));
  return 0;
}

/** `fnpm pm` itself (not one of its commands). */
const isPm = (registry: Registry, path: string[]) => registry.bin === "fnpm" && path.length === 1 && path[0] === "pm";

async function writePmHelp(runtime: CliRuntime): Promise<number> {
  const { renderPmHelp } = await import("./pm-help.ts");
  runtime.stdout.write(renderPmHelp(stdoutColors(runtime)));
  return 0;
}

const stdoutColors = (runtime: CliRuntime) =>
  createColors(colorEnabled(runtime, runtime.stdoutIsTTY));

export async function resolveCliResult(result: unknown): Promise<number> {
  if (isPromiseLike(result)) {
    return (await result) as number;
  }
  return typeof result === "number" ? result : 0;
}

/**
 * Every failure path honours `--json`: `{ok: false, error: {code, message}}`
 * on stdout, so the case a script most needs to handle is one it can read.
 */
function reportError(
  registry: Registry,
  runtime: CliRuntime,
  argv: string[],
  plan: Invocation | undefined,
  error: unknown,
): number {
  const message = formatError(error);
  const wantsJson =
    plan?.wantsJson ?? (argv.slice(2, argv.includes("--") ? argv.indexOf("--") : undefined).includes("--json"));
  if (errorCode(error) === "REPORTED") {
    // The core already printed why (on stderr, also with --json).
    return 1;
  }
  if (errorCode(error) === PLAIN_USAGE) {
    runtime.stderr.write(`error: ${message}\n`);
    return 1;
  }
  if (wantsJson) {
    runtime.stdout.write(`${JSON.stringify({ ok: false, error: { code: errorCode(error), message } })}\n`);
    return 1;
  }
  runtime.stderr.write(`error: ${message}\n`);
  if (error instanceof SilentError && error.code === "UNKNOWN_FLAG") {
    runtime.stderr.write(`note: Run '${unknownFlagCommand(registry, plan?.matched ?? [])} --help' for more information\n`);
  } else if (error instanceof SilentError && error.code !== "CORE_UNAVAILABLE") {
    const path = plan?.matched ?? [];
    runtime.stderr.write(
      path.length === 0
        ? `Run '${registry.bin} --help' for usage.\n`
        : `Run '${[registry.bin, ...path].join(" ")} --help' for detailed usage.\n`,
    );
  } else if (!(error instanceof SilentError) && (!(error instanceof Error) || error.name !== "CoreError")) {
    // An unexpected exception is a bug; keep the stack for the report.
    if (error instanceof Error && error.stack) {
      runtime.stderr.write(`${error.stack}\n`);
    }
  }
  return 1;
}

function isPromiseLike(value: unknown): value is Promise<unknown> {
  return (
    typeof value === "object" &&
    value !== null &&
    "then" in value &&
    typeof (value as Promise<unknown>).then === "function"
  );
}
