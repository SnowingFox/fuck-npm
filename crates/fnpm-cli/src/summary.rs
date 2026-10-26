//! bun's install summary, line for line: a port of `formatInstallSummary` in
//! `packages/fuck-npm/src/output/summary.ts`.

use fnpm_core::options::InstallCommand;
use fnpm_types::InstallSummary;

use crate::color::{Colors, to_fixed};

/// `[12.34ms]` up to 1500 ms, `[1.23s]` above.
pub fn format_elapsed(ms: f64) -> String {
    format!("[{}]", elapsed_value(ms))
}

fn elapsed_value(ms: f64) -> String {
    if ms <= 1500.0 {
        format!("{}ms", to_fixed(ms, 2))
    } else {
        format!("{}s", to_fixed(ms / 1000.0, 2))
    }
}

/// bun's `<d>[<b>12.00ms<r><d>]<r>`.
fn time_of(ms: f64, pc: Colors) -> String {
    pc.dim(&format!("[{}]", pc.bold(&elapsed_value(ms))))
}

/// bun's ` <d>(<blue>v1.2.4 available<r><d>)<r>`.
fn available_of(version: &Option<String>, pc: Colors) -> String {
    match version {
        Some(version) if !version.is_empty() => format!(
            " {}",
            pc.dim(&format!("({})", pc.blue(&format!("v{version} available"))))
        ),
        _ => String::new(),
    }
}

fn plural(count: u32, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

/// `fnpm update --dry-run`: the rows of what would move and bun's
/// `N packages would be updated` count, or `Checked N packages, nothing to update`.
pub fn format_update_plan(
    summary: &InstallSummary,
    pc: Colors,
    color: bool,
    elapsed_ms: f64,
) -> Vec<String> {
    let mut lines = vec![String::new()];
    let time = time_of(elapsed_ms, pc);
    let (arrow_up, arrow) = if color { ("↑", "→") } else { ("^", "->") };
    for pkg in &summary.updated {
        let to = if color {
            pc.bold(&pc.cyan(&pkg.to))
        } else {
            pc.bold(&pkg.to)
        };
        lines.push(format!(
            "{} {} {} {to}{}",
            pc.cyan(arrow_up),
            pc.bold(&pkg.name),
            pc.dim(&format!("{} {arrow}", pkg.from)),
            available_of(&pkg.available_version, pc)
        ));
    }
    let n = summary.updated.len() as u32;
    if n > 0 {
        lines.push(String::new());
        lines.push(format!(
            "{} package{} would be updated {time}",
            pc.green(&n.to_string()),
            if n == 1 { "" } else { "s" }
        ));
    } else {
        let packages = summary.checked.map_or(0, |c| c.packages);
        lines.push(format!(
            "Checked {}, nothing to update {time}",
            plural(packages, "package")
        ));
    }
    lines
}

/// The summary lines (each printed with a trailing newline). `color` selects the Unicode
/// arrows; `pc` is the stdout palette.
pub fn format_install_summary(
    summary: &InstallSummary,
    command: InstallCommand,
    pc: Colors,
    color: bool,
) -> Vec<String> {
    format_install_summary_at(summary, command, pc, color, summary.elapsed_ms as f64)
}

/// [`format_install_summary`] with the time measured by the caller (fractional
/// milliseconds: a no-op install takes less than one).
pub fn format_install_summary_at(
    summary: &InstallSummary,
    command: InstallCommand,
    pc: Colors,
    color: bool,
    elapsed_ms: f64,
) -> Vec<String> {
    // bun's tree printer opens with a newline; the header above ends with one.
    let mut lines = vec![String::new()];
    let time = time_of(elapsed_ms, pc);
    let available = |version: &Option<String>| available_of(version, pc);

    // `+` / `↑` rows. bun's tree printer skips the packages an add or remove was asked for
    // (its update requests; `update` lists them): an add shows them once, in the `installed`
    // section below.
    let requested_name = |name: &str| {
        matches!(command, InstallCommand::Add | InstallCommand::Remove)
            && summary.requested.iter().any(|pkg| pkg.name == name)
    };
    let mut rows = 0;
    for pkg in &summary.added {
        if requested_name(&pkg.name) {
            continue;
        }
        lines.push(format!(
            "{} {}{}{}",
            pc.green("+"),
            pc.bold(&pkg.name),
            pc.dim(&format!("@{}", pkg.version)),
            available(&pkg.available_version)
        ));
        rows += 1;
    }
    let (arrow_up, arrow) = if color { ("↑", "→") } else { ("^", "->") };
    for pkg in &summary.updated {
        let to = if color {
            pc.bold(&pc.cyan(&pkg.to))
        } else {
            pc.bold(&pkg.to)
        };
        lines.push(format!(
            "{} {} {} {to}{}",
            pc.cyan(arrow_up),
            pc.bold(&pkg.name),
            pc.dim(&format!("{} {arrow}", pkg.from)),
            available(&pkg.available_version)
        ));
        rows += 1;
    }
    if rows > 0 {
        lines.push(String::new());
    }

    if !summary.requested.is_empty()
        && matches!(command, InstallCommand::Add | InstallCommand::Update)
    {
        for pkg in &summary.requested {
            if pkg.skipped == Some(true) {
                lines.push(format!(
                    "{} {}{} {}",
                    pc.yellow("skipped"),
                    pc.bold(&pkg.name),
                    pc.dim(&format!("@{}", pkg.version)),
                    pc.dim("(unsupported platform)")
                ));
                continue;
            }
            // bun: `<green>installed<r> <b>alias<r><d>@npm:<r><b>name<r><d>@1.0.0<r>`.
            let shown = match &pkg.real_name {
                Some(real) if !real.is_empty() => {
                    format!("{}{}{}", pc.bold(&pkg.name), pc.dim("@npm:"), pc.bold(real))
                }
                _ => pc.bold(&pkg.name),
            };
            let installed = format!(
                "{} {shown}{}",
                pc.green("installed"),
                pc.dim(&format!("@{}", pkg.version))
            );
            if pkg.bins.is_empty() {
                lines.push(installed);
            } else {
                lines.push(format!("{installed} with binaries:"));
                for bin in &pkg.bins {
                    lines.push(format!(" {}{}", pc.dim("- "), pc.bold(bin)));
                }
            }
        }
        lines.push(String::new());
    }

    // bun lists removals only for `remove`, and only when nothing was installed, right above
    // the count.
    if command == InstallCommand::Remove && summary.installed_count == 0 {
        for name in &summary.removed {
            lines.push(format!("{} {name}", pc.red("-")));
        }
    }

    let blocked = |lines: &mut Vec<String>| {
        if summary.blocked_scripts > 0 {
            lines.push(String::new());
            lines.push(pc.dim(&format!(
                "Blocked {}. Run `fnpm pm untrusted` for details.",
                plural(summary.blocked_scripts, "postinstall")
            )));
        }
    };
    let noun = |count: u32| if count == 1 { "package" } else { "packages" };
    let mut timed = true;
    if summary.installed_count > 0 {
        let count = if command == InstallCommand::Add {
            summary.installed_count.max(summary.requested.len() as u32)
        } else {
            summary.installed_count
        };
        lines.push(format!(
            "{} {} installed {time}",
            pc.green(&count.to_string()),
            noun(count)
        ));
        blocked(&mut lines);
        if summary.removed_count > 0 {
            lines.push(format!(
                "Removed: {}",
                pc.cyan(&summary.removed_count.to_string())
            ));
        }
    } else if summary.removed_count > 0 {
        lines.push(format!(
            "{} {} removed {time}",
            pc.bold(&summary.removed_count.to_string()),
            noun(summary.removed_count)
        ));
        blocked(&mut lines);
    } else if let Some(checked) = summary.checked.filter(|_| summary.failed_count == 0) {
        // bun shortens the line when every package has exactly one place.
        if checked.installs == checked.packages {
            lines.push(format!(
                "{}! Checked {} {} {} {time}",
                pc.green("Done"),
                checked.installs,
                noun(checked.installs),
                pc.dim(if summary.package_json_synced {
                    "(up to date, package.json synced)"
                } else {
                    "(no changes)"
                }),
            ));
        } else {
            lines.push(format!(
                "Checked {} across {} {} {} {time}",
                pc.green(&plural(checked.installs, "install")),
                checked.packages,
                noun(checked.packages),
                pc.dim(if summary.package_json_synced {
                    "(up to date, package.json synced)"
                } else {
                    "(no changes)"
                }),
            ));
        }
        blocked(&mut lines);
    } else {
        timed = false;
    }

    // After the count, as bun prints it; with no count line, the time follows on its own.
    if summary.failed_count > 0 {
        lines.push(format!(
            "Failed to install {} {}",
            pc.red(&pc.bold(&summary.failed_count.to_string())),
            noun(summary.failed_count)
        ));
    }
    if !timed {
        lines.push(format!("{time} {}", pc.dim("done")));
    }
    lines
}
