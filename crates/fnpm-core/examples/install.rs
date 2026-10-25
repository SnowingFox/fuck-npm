//! A minimal native driver for benchmarking the core without a CLI:
//!
//! ```text
//! cargo run --release -p fnpm-core --example install -- [install|add <specs>|remove <names>|update]
//!     [--frozen-lockfile] [--ignore-scripts] [--registry <url>] [--linker <l>] [--cwd <dir>]
//! ```
//!
//! Prints one summary line on stderr; exit code 1 on error.

use std::sync::Arc;

use fnpm_core::options::{InstallCommand, InstallOptions};
use fnpm_types::NoopSink;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let mut opts = InstallOptions::default();
    opts.pm.cwd = std::env::current_dir().unwrap_or_default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "install" | "i" => opts.command = InstallCommand::Install,
            "ci" => {
                opts.command = InstallCommand::Install;
                opts.pm.frozen_lockfile = true;
            }
            "add" => opts.command = InstallCommand::Add,
            "remove" | "rm" => opts.command = InstallCommand::Remove,
            "update" => opts.command = InstallCommand::Update,
            "--frozen-lockfile" => opts.pm.frozen_lockfile = true,
            "--ignore-scripts" => opts.pm.ignore_scripts = true,
            "--force" => opts.pm.force = true,
            "--registry" => opts.pm.registry = args.next(),
            "--cwd" => opts.pm.cwd = args.next().unwrap_or_default().into(),
            "--linker" => {
                opts.pm.linker = args.next().and_then(|l| match l.as_str() {
                    "hoisted" => Some(fnpm_config::LinkerSetting::Hoisted),
                    "isolated" => Some(fnpm_config::LinkerSetting::Isolated),
                    _ => None,
                })
            }
            spec => opts.packages.push(spec.to_owned()),
        }
    }
    // The no-op check needs no runtime; only a real install starts threads.
    let prepared = match fnpm_core::prepare_install(opts, fnpm_config::Env::from_process()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e} ({})", e.code());
            std::process::exit(1);
        }
    };
    if let Some(s) = prepared.noop() {
        eprintln!(
            "noop: {} packages checked [{} ms]",
            s.checked.map_or(0, |c| c.packages),
            s.elapsed_ms
        );
        return;
    }
    let runtime = fnpm_core::runtime();
    match runtime.block_on(prepared.run(Arc::new(NoopSink))) {
        Ok(s) => {
            eprintln!(
                "{} installed, {} removed, noop {}, {} blocked [{} ms]",
                s.installed_count, s.removed_count, s.noop, s.blocked_scripts, s.elapsed_ms
            );
        }
        Err(e) => {
            eprintln!("error: {e} ({})", e.code());
            std::process::exit(1);
        }
    }
}
