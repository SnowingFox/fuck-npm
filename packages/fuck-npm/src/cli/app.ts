import type { BinName } from "../entry.ts";
import { processRuntime } from "../runtime/process.ts";
import type { CliRuntime } from "../runtime/types.ts";
import { runParsedCli } from "./runtime.ts";
import type { Registry } from "./registry-types.ts";
import { FNPM_LOCK_REGISTRY } from "./trees/fnpm-lock.ts";
import { FNPM_REGISTRY } from "./trees/fnpm.ts";

export function registryFor(bin: BinName): Registry {
  return bin === "fnpm-lock" ? FNPM_LOCK_REGISTRY : FNPM_REGISTRY;
}

/**
 * Run one invocation of a bin. Tests pass a fake runtime (fake core, captured
 * streams); production builds the real one from `process`.
 */
export async function runCli(
  bin: BinName,
  argv: string[] = process.argv,
  runtime: CliRuntime = processRuntime(),
): Promise<number> {
  return runParsedCli(registryFor(bin), argv, runtime);
}
