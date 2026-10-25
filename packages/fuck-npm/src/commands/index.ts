import type { Handler } from "./context.ts";

/**
 * Command path → handler, loaded on demand so each invocation only parses the
 * module it runs. A path missing here goes to the generic core handler, which
 * is how most of the tree works: the CLI validates flags and the core does the
 * rest.
 */
const INSTALL_FAMILY = new Set(["install", "ci", "add", "remove", "update"]);

export async function loadHandler(bin: string, path: string[]): Promise<Handler> {
  const key = path.join(" ");
  if (key === "help") {
    return (await import("./help.ts")).runHelp;
  }
  if (bin === "fnpm") {
    if (INSTALL_FAMILY.has(key)) {
      return (await import("./install.ts")).runInstallFamily;
    }
    if (key === "x" || key === "create") {
      return (await import("./x.ts")).runX;
    }
    if (key === "completions") {
      return (await import("./completions.ts")).runCompletions;
    }
  }
  return (await import("./generic.ts")).runGeneric;
}
