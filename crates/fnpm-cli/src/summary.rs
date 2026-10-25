//! bun's install summary, line for line: a port of `formatInstallSummary` in
//! `packages/fuck-npm/src/output/summary.ts`.

use fnpm_core::options::InstallCommand;
use fnpm_types::InstallSummary;

use crate::color::{Colors, to_fixed};

/// `[12.34ms]` up to 1500 ms, `[1.23s]` above.
pub fn format_elapsed(ms: f64) -> String {
    if ms <= 1500.0 {
        format!("[{}ms]", to_fixed(ms, 2))
    } else {
        format!("[{}s]", to_fixed(ms / 1000.0, 2))
    }
}

fn plural(count: u32, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

/// The summary lines (each printed with a trailing newline). `color` selects the Unicode
/// arrows; `pc` is the stdout palette.
pub fn format_install_summary(
    summary: &InstallSummary,
    command: InstallCommand,
    pc: Colors,
    color: bool,
) -> Vec<String> {
    // bun's tree printer opens with a newline; the header above ends with one.
    let mut lines = vec![String::new()];
    let time = pc.dim(&format_elapsed(summary.elapsed_ms as f64));
    let available = |version: &Option<String>| match version {
        Some(version) if !version.is_empty() => pc.dim(&format!(" (v{version} available)")),
        _ => String::new(),
    };

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
            pkg.name,
            pc.dim(&format!("@{}", pkg.version)),
            available(&pkg.available_version)
        ));
        rows += 1;
    }
    let (arrow_up, arrow) = if color { ("↑", "→") } else { ("^", "->") };
    for pkg in &summary.updated {
        lines.push(format!(
            "{} {} {} {arrow} {}{}",
            pc.cyan(arrow_up),
            pkg.name,
            pc.dim(&pkg.from),
            pkg.to,
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
            let shown = match &pkg.real_name {
                Some(real) if !real.is_empty() => format!("{}@npm:{real}", pkg.name),
                _ => pkg.name.clone(),
            };
            if pkg.skipped == Some(true) {
                lines.push(format!(
                    "{} {shown}@{} {}",
                    pc.yellow("skipped"),
                    pkg.version,
                    pc.dim("(unsupported platform)")
                ));
                continue;
            }
            let installed = format!(
                "{} {}",
                pc.green("installed"),
                pc.bold(&format!("{shown}@{}", pkg.version))
            );
            if pkg.bins.is_empty() {
                lines.push(installed);
            } else {
                lines.push(format!("{installed} with binaries:"));
                for bin in &pkg.bins {
                    lines.push(format!("  {} {}", pc.dim("-"), pc.bold(bin)));
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
        lines.push(format!(
            "Checked {} {} across {} {} {} {time}",
            pc.green(&checked.installs.to_string()),
            if checked.installs == 1 {
                "install"
            } else {
                "installs"
            },
            checked.packages,
            noun(checked.packages),
            pc.dim("(no changes)"),
        ));
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
