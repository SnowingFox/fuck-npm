//! The native `fnpm` and `fnpm-lock` launchers.
//!
//! `install`, `i`, `ci`, `add`, `remove` and `update` (with every alias) run in process
//! through `fnpm-core`, with argument parsing, validation, progress and the summary ported
//! byte for byte from the TS CLI (`packages/fuck-npm`). Everything else (other commands,
//! `--help`, `--tree`, unknown commands, `update -i`) is handed to the TS CLI by exec with
//! the argv unchanged. `--version` is answered here. The command tree comes from the TS
//! registry through `cli-spec.json`, compiled into `match` tables by `build.rs`.
//!
//! See `docs/ARCHITECTURE.md#fnpm-cli`.

pub mod color;
pub mod error;
pub mod exec;
pub mod install;
pub mod options;
pub mod parse;
pub mod plan;
pub mod progress;
pub mod spec;
pub mod summary;
pub mod terminal;

use std::ffi::OsString;

use crate::install::{Core, NativeCore, Outcome};
use crate::plan::Route;
use crate::spec::{BinDef, FNPM, FNPM_LOCK, VERSION};
use crate::terminal::{Env, Terminal};

/// Which executable is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bin {
    Fnpm,
    FnpmLock,
}

impl Bin {
    /// The bin's command tables.
    pub fn def(self) -> &'static BinDef {
        match self {
            Self::Fnpm => &FNPM,
            Self::FnpmLock => &FNPM_LOCK,
        }
    }
}

/// The package version `--version` prints.
pub fn version() -> &'static str {
    VERSION
}

/// What `main` should do for `args` (argv after the program name). Arguments that are not
/// valid UTF-8 always go to Node, which handles them like any other string.
pub fn route_args(bin: Bin, args: &[OsString]) -> Route {
    let mut strings = Vec::with_capacity(args.len());
    for arg in args {
        match arg.to_str() {
            Some(text) => strings.push(text.to_owned()),
            None => return Route::Node,
        }
    }
    plan::route(bin.def(), &strings)
}

/// Run `bin` with `args` against `terminal` and `core`; returns the exit status, or `None`
/// when the TS CLI has to take over.
pub fn run_with(
    bin: Bin,
    args: &[OsString],
    terminal: impl FnOnce() -> Terminal,
    core: &dyn Core,
) -> Option<i32> {
    match route_args(bin, args) {
        Route::Node => None,
        Route::Version => {
            let terminal = terminal();
            terminal.stdout.write(&format!("{VERSION}\n"));
            Some(0)
        }
        Route::Native(invocation) => {
            match install::run(&invocation, bin.def(), &terminal(), core) {
                Outcome::Exit(code) => Some(code),
                Outcome::Node => None,
            }
        }
    }
}

/// The whole program: route, run natively or exec node. Returns the exit status.
pub fn main(bin: Bin) -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // The version path writes directly: no environment snapshot, no terminal probing.
    if let Route::Version = route_args(bin, &args) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out
            .write_all(VERSION.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush());
        return 0;
    }
    match run_with(bin, &args, Terminal::from_process, &NativeCore) {
        Some(code) => code,
        None => exec::run_node(bin.def().name, &args, &Env::from_process()),
    }
}
