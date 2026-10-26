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
  return `[${elapsedValue(ms)}]`;
}

const elapsedValue = (ms: number): string => (ms <= 1500 ? `${ms.toFixed(2)}ms` : `${(ms / 1000).toFixed(2)}s`);

/** bun's `<d>[<b>12.00ms<r><d>]<r>`. */
const timeOf = (ms: number, pc: Colors): string => pc.dim(`[${pc.bold(elapsedValue(ms))}]`);

/** bun's ` <d>(<blue>v1.2.4 available<r><d>)<r>`. */
const availableOf = (version: string | undefined, pc: Colors): string =>
  version ? ` ${pc.dim(`(${pc.blue(`v${version} available`)})`)}` : "";

const plural = (count: number, word: string): string => `${count} ${word}${count === 1 ? "" : "s"}`;

export function formatInstallSummary(
  summary: InstallSummary,
  command: InstallCommand,
  pc: Colors,
  opts: { color: boolean } = { color: true },
): string[] {
  // bun's tree printer opens with a newline; the header above ends with one.
  const lines: string[] = [""];
  const time = timeOf(summary.elapsedMs, pc);

  // `--lockfile-only`: bun prints its one line and no package rows.
  if (summary.lockfileOnly) {
    const { saved, packages } = summary.lockfileOnly;
    const noun = packages === 1 ? "package" : "packages";
    lines.push(
      saved
        ? `Saved ${pc.green("fnpm.lock")} (${packages} ${noun}) ${time}`
        : `${pc.green("Done")}! Checked ${packages} ${noun} ${pc.dim("(no changes)")} ${time}`,
    );
    return lines;
  }

  // `+` / `↑` rows. bun's tree printer skips the packages an add or remove was asked for
  // (its update requests; `update` lists them): an add shows them once, in the `installed`
  // section below.
  const requestedNames = new Set(
    command === "add" || command === "remove" ? summary.requested.map((pkg) => pkg.name) : [],
  );
  let rows = 0;
  for (const pkg of summary.added) {
    if (requestedNames.has(pkg.name)) continue;
    lines.push(`${pc.green("+")} ${pc.bold(pkg.name)}${pc.dim(`@${pkg.version}`)}${availableOf(pkg.availableVersion, pc)}`);
    rows += 1;
  }
  for (const pkg of summary.updated) {
    const arrowUp = opts.color ? "↑" : "^";
    const arrow = opts.color ? "→" : "->";
    const to = opts.color ? pc.bold(pc.cyan(pkg.to)) : pc.bold(pkg.to);
    lines.push(
      `${pc.cyan(arrowUp)} ${pc.bold(pkg.name)} ${pc.dim(`${pkg.from} ${arrow}`)} ${to}${availableOf(pkg.availableVersion, pc)}`,
    );
    rows += 1;
  }
  if (rows > 0) {
    lines.push("");
  }

  if (summary.requested.length > 0 && (command === "add" || command === "update")) {
    for (const pkg of summary.requested) {
      if (pkg.skipped) {
        lines.push(
          `${pc.yellow("skipped")} ${pc.bold(pkg.name)}${pc.dim(`@${pkg.version}`)} ${pc.dim("(unsupported platform)")}`,
        );
        continue;
      }
      // bun: `<green>installed<r> <b>alias<r><d>@npm:<r><b>name<r><d>@1.0.0<r>`.
      const shown = pkg.realName ? `${pc.bold(pkg.name)}${pc.dim("@npm:")}${pc.bold(pkg.realName)}` : pc.bold(pkg.name);
      const installed = `${pc.green("installed")} ${shown}${pc.dim(`@${pkg.version}`)}`;
      if (pkg.bins.length > 0) {
        lines.push(`${installed} with binaries:`);
        for (const bin of pkg.bins) {
          lines.push(` ${pc.dim("- ")}${pc.bold(bin)}`);
        }
      } else {
        lines.push(installed);
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
    // bun shortens the line when every package has exactly one place.
    lines.push(
      installs === packages
        ? `${pc.green("Done")}! Checked ${installs} ${installs === 1 ? "package" : "packages"} ${pc.dim("(no changes)")} ${time}`
        : `Checked ${pc.green(`${installs} ${installs === 1 ? "install" : "installs"}`)} across ${packages} ${packages === 1 ? "package" : "packages"} ${pc.dim("(no changes)")} ${time}`,
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
