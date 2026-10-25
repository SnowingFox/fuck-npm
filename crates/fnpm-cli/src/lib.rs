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

/// mimalloc's `arena_eager_commit` option (the same index in mimalloc v2 and v3).
const MI_OPTION_ARENA_EAGER_COMMIT: std::ffi::c_int = 4;

/// The global allocator of both executables (see [`native_allocator!`]).
pub use mimalloc::MiMalloc;

/// Allocator settings for a short-lived CLI.
///
/// mimalloc reserves a 1 GiB arena on the first allocation and, on Linux (overcommit),
/// commits all of it with `MADV_HUGEPAGE`, so the first touch faults in a zeroed 2 MiB page:
/// about 1 ms of a ~2.5 ms `fnpm --version`. Committing on demand costs nothing measurable
/// on an install. `MIMALLOC_ARENA_EAGER_COMMIT` in the environment still wins.
///
/// It must run before the first allocation, and Rust's runtime allocates before `main`, so
/// the executables run it as a load-time constructor ([`native_allocator!`]). Idempotent.
pub fn configure_allocator() {
    if std::env::var_os("MIMALLOC_ARENA_EAGER_COMMIT").is_none() {
        // SAFETY: sets a plain integer option; mimalloc reads it when it reserves an arena.
        // Valid before and after mimalloc's own initialisation.
        unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_ARENA_EAGER_COMMIT, 0) };
    }
}

/// [`configure_allocator`] with the C ABI, for the load-time constructor.
pub extern "C" fn configure_allocator_at_load() {
    configure_allocator();
}

/// Declares mimalloc as the global allocator and registers [`configure_allocator`] to run
/// when the executable is loaded, before Rust's runtime makes its first allocation. Used
/// once in each executable's `main.rs`.
#[macro_export]
macro_rules! native_allocator {
    () => {
        #[global_allocator]
        static GLOBAL: $crate::MiMalloc = $crate::MiMalloc;

        #[used]
        #[cfg_attr(
            any(target_os = "linux", target_os = "android"),
            unsafe(link_section = ".init_array")
        )]
        #[cfg_attr(
            target_vendor = "apple",
            unsafe(link_section = "__DATA,__mod_init_func")
        )]
        #[cfg_attr(windows, unsafe(link_section = ".CRT$XCU"))]
        static CONFIGURE_ALLOCATOR: extern "C" fn() = $crate::configure_allocator_at_load;
    };
}

/// Run `bin` with `args` against `terminal` and `core`; returns the exit status, or `None`
/// when the TS CLI has to take over.
pub fn run_with(
    bin: Bin,
    args: &[OsString],
    terminal: impl FnOnce() -> Terminal,
    core: &dyn Core,
) -> Option<i32> {
    run_route(bin, route_args(bin, args), terminal, core)
}

fn run_route(
    bin: Bin,
    route: Route,
    terminal: impl FnOnce() -> Terminal,
    core: &dyn Core,
) -> Option<i32> {
    match route {
        Route::Node => None,
        Route::Version => {
            terminal().stdout.write(&format!("{VERSION}\n"));
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
///
/// `fnpm-lock` has no native commands, so its executable calls [`main_lock`] directly and
/// never links the core.
pub fn main(bin: Bin) -> i32 {
    if bin == Bin::FnpmLock {
        return main_lock();
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let route = route_args(bin, &args);
    if let Route::Version = route {
        return print_version();
    }
    match run_route(bin, route, Terminal::from_process, &NativeCore) {
        Some(code) => code,
        None => exec::run_node(bin.def().name, &args, &Env::from_process()),
    }
}

/// `fnpm-lock`: answers `--version`, execs the TS CLI for everything else. References
/// nothing of `fnpm-core`, so the `fnpm-lock` executable stays small.
pub fn main_lock() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match route_args(Bin::FnpmLock, &args) {
        Route::Version => print_version(),
        // There are no native `fnpm-lock` commands; anything else is Node's.
        Route::Node | Route::Native(_) => {
            exec::run_node(FNPM_LOCK.name, &args, &Env::from_process())
        }
    }
}

/// `--version`: no environment snapshot and no terminal probing for the one invocation
/// scripts loop on.
fn print_version() -> i32 {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out
        .write_all(VERSION.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    0
}
