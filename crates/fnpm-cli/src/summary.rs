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
    let mut lines = vec![String::new()];
    let time = pc.dim(&format_elapsed(summary.elapsed_ms as f64));

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

    let available = |version: &Option<String>| match version {
        Some(version) if !version.is_empty() => pc.dim(&format!(" (v{version} available)")),
        _ => String::new(),
    };
    for pkg in &summary.added {
        lines.push(format!(
            "{} {}{}{}",
            pc.green("+"),
            pkg.name,
            pc.dim(&format!("@{}", pkg.version)),
            available(&pkg.available_version)
        ));
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
    }
    for name in &summary.removed {
        lines.push(format!("{} {name}", pc.red("-")));
    }
    if summary.added.len() + summary.updated.len() + summary.removed.len() > 0 {
        lines.push(String::new());
    }

    if summary.failed_count > 0 {
        lines.push(pc.red(&format!(
            "Failed to install {}",
            plural(summary.failed_count, "package")
        )));
    }

    if summary.installed_count > 0 {
        let count = if command == InstallCommand::Add {
            summary.installed_count.max(summary.requested.len() as u32)
        } else {
            summary.installed_count
        };
        let noun = if count == 1 { "package" } else { "packages" };
        lines.push(format!(
            "{} {noun} installed {time}",
            pc.green(&count.to_string())
        ));
        if summary.removed_count > 0 {
            lines.push(format!(
                "Removed: {}",
                pc.cyan(&summary.removed_count.to_string())
            ));
        }
    } else if summary.removed_count > 0 {
        let noun = if summary.removed_count == 1 {
            "package"
        } else {
            "packages"
        };
        lines.push(format!(
            "{} {noun} removed {time}",
            pc.green(&summary.removed_count.to_string())
        ));
    } else if let Some(checked) = summary.checked {
        lines.push(format!(
            "Checked {} {} across {} {} {} {time}",
            pc.green(&checked.installs.to_string()),
            if checked.installs == 1 {
                "install"
            } else {
                "installs"
            },
            checked.packages,
            if checked.packages == 1 {
                "package"
            } else {
                "packages"
            },
            pc.dim("(no changes)"),
        ));
    } else {
        lines.push(format!("{time} done"));
    }

    if summary.blocked_scripts > 0 {
        lines.push(String::new());
        lines.push(format!(
            "Blocked {}. Run `fnpm pm untrusted` for details.",
            plural(summary.blocked_scripts, "postinstall")
        ));
    }
    lines
}
