//! The orchestrator: owns the tokio runtime and rayon pools, runs the install pipeline and
//! every other command, batches progress events, and implements the no-op fast path.
//! See `docs/ARCHITECTURE.md#fnpm-core`.

pub mod commands;
pub mod events;
pub mod fast_path;
pub mod options;

use std::sync::Arc;

use fnpm_types::{EventSink, InstallSummary};

pub use options::{CoreRequest, CoreResult, InstallOptions, PmOptions};

/// Errors carry a stable `code` the CLI maps to messages and exit codes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] fnpm_config::Error),
    #[error(transparent)]
    Workspace(#[from] fnpm_workspace::Error),
    #[error(transparent)]
    Registry(#[from] fnpm_registry::Error),
    #[error(transparent)]
    Store(#[from] fnpm_store::Error),
    #[error(transparent)]
    Resolve(#[from] fnpm_resolver::Error),
    #[error(transparent)]
    Lockfile(#[from] fnpm_lockfile::Error),
    #[error(transparent)]
    Link(#[from] fnpm_linker::Error),
    #[error(transparent)]
    Scripts(#[from] fnpm_scripts::Error),
    #[error("{message}")]
    Usage { message: String },
    #[error("unknown command {0:?}")]
    UnknownCommand(Vec<String>),
}

impl Error {
    /// `FROZEN_LOCKFILE`, `NO_MATCHING_VERSION`, `NOT_FOUND`, `INTEGRITY`, `NETWORK`,
    /// `SCRIPT_FAILED`, `INVALID_PACKAGE_JSON`, `USAGE`, `CORE_ERROR` (fallback).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Resolve(fnpm_resolver::Error::FrozenLockfile { .. }) => "FROZEN_LOCKFILE",
            Self::Resolve(fnpm_resolver::Error::NoMatchingVersion { .. }) => "NO_MATCHING_VERSION",
            Self::Resolve(fnpm_resolver::Error::NotFound(_))
            | Self::Registry(fnpm_registry::Error::NotFound(_)) => "NOT_FOUND",
            Self::Store(fnpm_store::Error::Integrity { .. })
            | Self::Registry(fnpm_registry::Error::Integrity { .. }) => "INTEGRITY",
            Self::Registry(
                fnpm_registry::Error::Network { .. } | fnpm_registry::Error::Status { .. },
            ) => "NETWORK",
            Self::Scripts(
                fnpm_scripts::Error::Failed { .. } | fnpm_scripts::Error::Signaled { .. },
            ) => "SCRIPT_FAILED",
            Self::Workspace(fnpm_workspace::Error::InvalidPackageJson { .. }) => {
                "INVALID_PACKAGE_JSON"
            }
            Self::Usage { .. } | Self::UnknownCommand(_) => "USAGE",
            _ => "CORE_ERROR",
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The version of the native core (`CARGO_PKG_VERSION`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The process-wide runtime: tokio multi-thread for network and file I/O, plus the global
/// rayon pool for CPU work. Created on first use; the napi layer calls into it from JS.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    todo!("OnceLock<Runtime>, worker_threads = min(cpus, 8)")
}

/// `fnpm install | add | remove | update | ci`. Pipeline: fast path -> config -> workspace
/// -> lockfile -> resolve (streaming) -> fetch+extract (overlapping) -> link (overlapping) ->
/// scripts -> save lockfile, package.json, state file.
pub async fn install(_opts: InstallOptions, _events: Arc<dyn EventSink>) -> Result<InstallSummary> {
    todo!()
}

/// Every other command, dispatched on `request.command` (see `commands`).
pub async fn invoke(request: CoreRequest, events: Arc<dyn EventSink>) -> Result<CoreResult> {
    commands::dispatch(request, events).await
}
