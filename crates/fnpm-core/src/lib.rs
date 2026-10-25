//! The orchestrator: owns the tokio runtime and rayon pools, runs the install pipeline and
//! every other command, batches progress events, and implements the no-op fast path.
//! See `docs/ARCHITECTURE.md#fnpm-core`.

#![warn(missing_docs)]

pub mod commands;
pub mod events;
pub mod fast_path;
mod fetch;
pub(crate) mod install;
pub mod options;
pub mod project;
pub mod spec;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use fnpm_config::Env;
use fnpm_types::{EventSink, InstallSummary};

pub use install::{PreparedInstall, config_hash};
pub use options::{CoreRequest, CoreResult, InstallOptions, PmOptions};

/// Errors carry a stable `code` the CLI maps to messages and exit codes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Loading the configuration failed.
    #[error(transparent)]
    Config(#[from] fnpm_config::Error),
    /// The project root, a package.json or the workspace globs are invalid.
    #[error(transparent)]
    Workspace(#[from] fnpm_workspace::Error),
    /// A registry request failed.
    #[error(transparent)]
    Registry(#[from] fnpm_registry::Error),
    /// Writing or reading the store failed.
    #[error(transparent)]
    Store(#[from] fnpm_store::Error),
    /// Resolution failed.
    #[error(transparent)]
    Resolve(#[from] fnpm_resolver::Error),
    /// A lockfile could not be read or written.
    #[error(transparent)]
    Lockfile(#[from] fnpm_lockfile::Error),
    /// Building `node_modules` failed.
    #[error(transparent)]
    Link(#[from] fnpm_linker::Error),
    /// A lifecycle script failed.
    #[error(transparent)]
    Scripts(#[from] fnpm_scripts::Error),
    /// A file outside the crates above could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// Wrong arguments or flags, worded like bun.
    #[error("{message}")]
    Usage {
        /// The message.
        message: String,
    },
    /// The command path is not one fnpm knows.
    #[error("unknown command {0:?}")]
    UnknownCommand(Vec<String>),
    /// A package could not be downloaded or put into the store.
    #[error("{package}: {message}")]
    Fetch {
        /// `name@version`.
        package: String,
        /// What failed.
        message: String,
        /// The code of the underlying failure (`INTEGRITY`, `NETWORK`, `NOT_FOUND`, ...).
        code: &'static str,
    },
    /// A dependency name in the graph (from fnpm.lock) that cannot be a folder under
    /// `node_modules` (bun's tree builder refuses it before anything is installed).
    #[error("invalid dependency name \"{0}\"")]
    UnsafeName(String),
    /// A command that belongs to a later parity phase (see `docs/ROADMAP.md`).
    #[error("`fnpm {command}` is not implemented in the native core yet")]
    NotImplemented {
        /// The command path, space-separated.
        command: String,
    },
}

pub(crate) fn registry_code(e: &fnpm_registry::Error) -> &'static str {
    use fnpm_registry::Error as R;
    match e {
        R::NotFound(_) => "NOT_FOUND",
        R::Integrity { .. } => "INTEGRITY",
        R::Network { .. } | R::Status { .. } | R::Offline(_) | R::Registry { .. } => "NETWORK",
        _ => "CORE_ERROR",
    }
}

impl Error {
    /// `FROZEN_LOCKFILE`, `NO_MATCHING_VERSION`, `NOT_FOUND`, `INTEGRITY`, `NETWORK`,
    /// `SCRIPT_FAILED`, `INVALID_PACKAGE_JSON`, `USAGE`, `NOT_IMPLEMENTED`, `CORE_ERROR`
    /// (fallback).
    pub fn code(&self) -> &'static str {
        use fnpm_resolver::Error as Rv;
        match self {
            Self::Resolve(Rv::FrozenLockfile { .. }) => "FROZEN_LOCKFILE",
            Self::Resolve(
                Rv::NoMatchingVersion { .. } | Rv::DistTagNotFound { .. } | Rv::TooRecent { .. },
            ) => "NO_MATCHING_VERSION",
            Self::Resolve(Rv::NotFound(_) | Rv::WorkspaceNotFound { .. }) => "NOT_FOUND",
            Self::Resolve(Rv::Registry(e)) | Self::Registry(e) => registry_code(e),
            Self::Resolve(Rv::InvalidPackageJson { .. }) => "INVALID_PACKAGE_JSON",
            Self::Resolve(Rv::InvalidOption { .. }) => "USAGE",
            Self::Store(fnpm_store::Error::Integrity { .. }) => "INTEGRITY",
            Self::Scripts(
                fnpm_scripts::Error::Failed { .. } | fnpm_scripts::Error::Signaled { .. },
            ) => "SCRIPT_FAILED",
            Self::Workspace(fnpm_workspace::Error::InvalidPackageJson { .. }) => {
                "INVALID_PACKAGE_JSON"
            }
            Self::Config(fnpm_config::Error::InvalidValue { .. }) => "USAGE",
            Self::Usage { .. } | Self::UnknownCommand(_) => "USAGE",
            Self::NotImplemented { .. } => "NOT_IMPLEMENTED",
            Self::Fetch { code, .. } => code,
            _ => "CORE_ERROR",
        }
    }

    /// A [`Error::Usage`].
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage {
            message: message.into(),
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { path, source }
    }
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The version of the native core (`CARGO_PKG_VERSION`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Worker threads of [`runtime`]: `FNPM_IO_THREADS` if set (1-64), else `min(cpus, 4)`. The
/// workers only drive network I/O and the pipeline; CPU work (extraction, hashing, linking)
/// runs on rayon pools, so more workers only add thread start-up cost.
pub fn io_threads() -> usize {
    std::env::var("FNPM_IO_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| (1..=64).contains(n))
        .unwrap_or_else(|| fnpm_types::available_parallelism().clamp(1, 4))
}

/// The process-wide runtime: tokio multi-thread for network and file I/O, plus the global
/// rayon pool for CPU work. Created on first use ([`io_threads`] workers); the napi layer and
/// the native launcher call into it.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let workers = io_threads();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("fnpm-io")
            .enable_all()
            .build()
            .expect("the tokio runtime can be created")
    })
}

/// `fnpm install | add | remove | update | ci`. Pipeline: fast path -> config -> workspace
/// -> lockfile -> resolve (streaming) -> fetch+extract (overlapping) -> link (overlapping) ->
/// scripts -> save lockfile, package.json, state file.
pub async fn install(opts: InstallOptions, events: Arc<dyn EventSink>) -> Result<InstallSummary> {
    install_with_env(opts, events, Env::from_process()).await
}

/// [`install`] with an explicit environment snapshot instead of the process environment
/// (tests, embedders). Lifecycle scripts still inherit the process environment.
pub async fn install_with_env(
    opts: InstallOptions,
    events: Arc<dyn EventSink>,
    env: Env,
) -> Result<InstallSummary> {
    install::run(opts, events, env).await
}

/// The synchronous start of [`install`]: options, project, configuration and the no-op check,
/// with no runtime. A launcher answers a no-op install from [`PreparedInstall::noop`] without
/// starting [`runtime`]; otherwise it continues with [`PreparedInstall::run`] on the runtime.
/// For an install that is not a no-op, [`runtime`] is started on a background thread while
/// this reads the workspace and the lockfile.
pub fn prepare_install(opts: InstallOptions, env: Env) -> Result<PreparedInstall> {
    install::prepare(opts, env, true)
}

/// Every other command, dispatched on `request.command` (see `commands`).
pub async fn invoke(request: CoreRequest, events: Arc<dyn EventSink>) -> Result<CoreResult> {
    commands::dispatch(request, events, Env::from_process()).await
}

/// [`invoke`] with an explicit environment snapshot.
pub async fn invoke_with_env(
    request: CoreRequest,
    events: Arc<dyn EventSink>,
    env: Env,
) -> Result<CoreResult> {
    commands::dispatch(request, events, env).await
}
