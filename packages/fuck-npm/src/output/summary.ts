import type { InstallCommand, InstallSummary } from "../core/types.ts";
import type { Colors } from "./output.ts";

/**
 * bun's install summary (install_with_manager.rs), line for line:
 *
 *     installed react@18.3.1 with binaries:
 *       - loose-envify
 *
 *     + react@18.3.1
 *     ↑ zod 3.22.0 → 3.23.8
 *
 *     2 packages installed [123.00ms]
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
  const lines: string[] = [""];
  const time = pc.dim(formatElapsed(summary.elapsedMs));

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

  for (const pkg of summary.added) {
    const available = pkg.availableVersion ? pc.dim(` (v${pkg.availableVersion} available)`) : "";
    lines.push(`${pc.green("+")} ${pkg.name}${pc.dim(`@${pkg.version}`)}${available}`);
  }
  for (const pkg of summary.updated) {
    const available = pkg.availableVersion ? pc.dim(` (v${pkg.availableVersion} available)`) : "";
    const arrowUp = opts.color ? "↑" : "^";
    const arrow = opts.color ? "→" : "->";
    lines.push(`${pc.cyan(arrowUp)} ${pkg.name} ${pc.dim(pkg.from)} ${arrow} ${pkg.to}${available}`);
  }
  for (const name of summary.removed) {
    lines.push(`${pc.red("-")} ${name}`);
  }
  if (summary.added.length + summary.updated.length + summary.removed.length > 0) {
    lines.push("");
  }

  if (summary.failedCount > 0) {
    lines.push(pc.red(`Failed to install ${plural(summary.failedCount, "package")}`));
  }

  if (summary.installedCount > 0) {
    const count =
      command === "add" ? Math.max(summary.installedCount, summary.requested.length) : summary.installedCount;
    lines.push(`${pc.green(String(count))} ${count === 1 ? "package" : "packages"} installed ${time}`);
    if (summary.removedCount > 0) {
      lines.push(`Removed: ${pc.cyan(String(summary.removedCount))}`);
    }
  } else if (summary.removedCount > 0) {
    lines.push(`${pc.green(String(summary.removedCount))} ${summary.removedCount === 1 ? "package" : "packages"} removed ${time}`);
  } else if (summary.checked) {
    const { installs, packages } = summary.checked;
    lines.push(
      `Checked ${pc.green(String(installs))} ${installs === 1 ? "install" : "installs"} across ${packages} ${packages === 1 ? "package" : "packages"} ${pc.dim("(no changes)")} ${time}`,
    );
  } else {
    lines.push(`${time} done`);
  }

  if (summary.blockedScripts > 0) {
    lines.push("");
    lines.push(
      `Blocked ${plural(summary.blockedScripts, "postinstall")}. Run \`fnpm pm untrusted\` for details.`,
    );
  }
  return lines;
}
