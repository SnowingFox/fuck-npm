import type { InstallCommand, InstallSummary } from "../core/types.ts";
import type { Colors } from "./output.ts";

/**
 * bun's install summary (install_with_manager.rs, lockfile/printer/tree_printer.rs), line for
 * line:
 *
 *     + react@18.3.1
 *     ↑ zod 3.22.0 → 3.23.8
 *
 *     installed typescript@5.9.3 with binaries:
 *       - tsc
 *
 *     3 packages installed [123.00ms]
 *
 * `remove` with nothing installed lists `- name` rows right above `N packages removed`;
 * `Failed to install N packages` follows the count line.
 */

/** `[12.34ms]` up to 1500 ms, `[1.23s]` above. */
export function formatElapsed(ms: number): string {
  return ms <= 1500 ? `[${ms.toFixed(2)}ms]` : `[${(ms / 1000).toFixed(2)}s]`;
}

const plural = (count: number, word: string): string => `${count} ${word}${count === 1 ? "" : "s"}`;

export function formatInstallSummary(
  summary: InstallSummary,
  command: InstallCommand,
  pc: Colors,
  opts: { color: boolean } = { color: true },
): string[] {
  // bun's tree printer opens with a newline; the header above ends with one.
  const lines: string[] = [""];
  const time = pc.dim(formatElapsed(summary.elapsedMs));

  // `+` / `↑` rows. bun's tree printer skips the packages an add or remove was asked for
  // (its update requests; `update` lists them): an add shows them once, in the `installed`
  // section below.
  const requestedNames = new Set(
    command === "add" || command === "remove" ? summary.requested.map((pkg) => pkg.name) : [],
  );
  let rows = 0;
  for (const pkg of summary.added) {
    if (requestedNames.has(pkg.name)) continue;
    const available = pkg.availableVersion ? pc.dim(` (v${pkg.availableVersion} available)`) : "";
    lines.push(`${pc.green("+")} ${pkg.name}${pc.dim(`@${pkg.version}`)}${available}`);
    rows += 1;
  }
  for (const pkg of summary.updated) {
    const available = pkg.availableVersion ? pc.dim(` (v${pkg.availableVersion} available)`) : "";
    const arrowUp = opts.color ? "↑" : "^";
    const arrow = opts.color ? "→" : "->";
    lines.push(`${pc.cyan(arrowUp)} ${pkg.name} ${pc.dim(pkg.from)} ${arrow} ${pkg.to}${available}`);
    rows += 1;
  }
  if (rows > 0) {
    lines.push("");
  }

  if (summary.requested.length > 0 && (command === "add" || command === "update")) {
    for (const pkg of summary.requested) {
      const shown = pkg.realName ? `${pkg.name}@npm:${pkg.realName}` : pkg.name;
      if (pkg.skipped) {
        lines.push(`${pc.yellow("skipped")} ${shown}@${pkg.version} ${pc.dim("(unsupported platform)")}`);
        continue;
      }
      if (pkg.bins.length > 0) {
        lines.push(`${pc.green("installed")} ${pc.bold(`${shown}@${pkg.version}`)} with binaries:`);
        for (const bin of pkg.bins) {
          lines.push(`  ${pc.dim("-")} ${pc.bold(bin)}`);
        }
      } else {
        lines.push(`${pc.green("installed")} ${pc.bold(`${shown}@${pkg.version}`)}`);
      }
    }
    lines.push("");
  }

  // bun lists removals only for `remove`, and only when nothing was installed, right above
  // the count.
  if (command === "remove" && summary.installedCount === 0) {
    for (const name of summary.removed) {
      lines.push(`${pc.red("-")} ${name}`);
    }
  }

  const blocked = (): void => {
    if (summary.blockedScripts > 0) {
      lines.push("");
      lines.push(
        pc.dim(`Blocked ${plural(summary.blockedScripts, "postinstall")}. Run \`fnpm pm untrusted\` for details.`),
      );
    }
  };
  let timed = true;
  if (summary.installedCount > 0) {
    const count =
      command === "add" ? Math.max(summary.installedCount, summary.requested.length) : summary.installedCount;
    lines.push(`${pc.green(String(count))} ${count === 1 ? "package" : "packages"} installed ${time}`);
    blocked();
    if (summary.removedCount > 0) {
      lines.push(`Removed: ${pc.cyan(String(summary.removedCount))}`);
    }
  } else if (summary.removedCount > 0) {
    lines.push(`${pc.bold(String(summary.removedCount))} ${summary.removedCount === 1 ? "package" : "packages"} removed ${time}`);
    blocked();
  } else if (summary.checked && summary.failedCount === 0) {
    const { installs, packages } = summary.checked;
    lines.push(
      `Checked ${pc.green(String(installs))} ${installs === 1 ? "install" : "installs"} across ${packages} ${packages === 1 ? "package" : "packages"} ${pc.dim("(no changes)")} ${time}`,
    );
    blocked();
  } else {
    timed = false;
  }

  // After the count, as bun prints it; with no count line, the time follows on its own.
  if (summary.failedCount > 0) {
    lines.push(`Failed to install ${pc.red(pc.bold(String(summary.failedCount)))} ${summary.failedCount === 1 ? "package" : "packages"}`);
  }
  if (!timed) {
    lines.push(`${time} ${pc.dim("done")}`);
  }
  return lines;
}
