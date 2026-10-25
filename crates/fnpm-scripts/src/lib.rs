//! Lifecycle scripts: which packages may run them (trustedDependencies and the default
//! list), the postinstall optimizer, environment construction, and a concurrent runner in
//! dependency order. Also the single-script runner used by `fnpm run` / `exec`.
//! See `docs/ARCHITECTURE.md#fnpm-scripts`.

pub mod env;
pub mod optimizer;
pub mod trust;

use std::path::PathBuf;

use fnpm_types::{EventSink, PackageId};

pub use trust::TrustPolicy;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{package}: {script} script exited with code {code}")]
    Failed { package: String, script: String, code: i32 },
    #[error("{package}: {script} script was terminated by signal {signal}")]
    Signaled { package: String, script: String, signal: i32 },
    #[error("could not spawn {shell}: {source}")]
    Spawn {
        shell: String,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The lifecycle scripts fnpm runs, in order (bun `Scripts::NAMES`).
pub const LIFECYCLE: [&str; 6] = ["preinstall", "install", "postinstall", "preprepare", "prepare", "postprepare"];

/// One package's scripts to run, in order. Scripts of one package run sequentially; the next
/// starts after the previous exits 0.
#[derive(Debug, Clone)]
pub struct Job {
    pub package: PackageId,
    pub name: String,
    pub version: String,
    pub dir: PathBuf,
    pub package_json: PathBuf,
    /// `(event name, command)`; `node-gyp rebuild` for an implicit `binding.gyp` install.
    pub scripts: Vec<(String, String)>,
    /// Optional dependencies may fail without failing the install.
    pub optional: bool,
    /// Root/workspace scripts run in the foreground (inherited stdio, command echoed).
    pub foreground: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub ran: u32,
    pub failed_optional: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Default `2 * cpus`.
    pub concurrency: usize,
    /// `--shell` / `run.shell`; default `sh` (POSIX) or `cmd.exe` (Windows).
    pub shell: Option<String>,
    /// Extra environment (`.env` files loaded by the caller, `npm_config_*`).
    pub env: Vec<(String, String)>,
    pub silent: bool,
    pub project_dir: PathBuf,
}

/// Runs `levels` (dependency order: every package in level N depends only on packages in
/// levels < N) with at most `concurrency` scripts at once. Child stdout/stderr is captured
/// and shown only on failure (bun), except for `foreground` jobs.
pub async fn run_jobs(_levels: Vec<Vec<Job>>, _opts: &RunOptions, _events: &dyn EventSink) -> Result<Report> {
    todo!()
}

/// `fnpm run <script>` / `fnpm exec`: runs one command with `node_modules/.bin` on PATH,
/// `pre`/`post` hooks handled by the caller; returns the exit code (signals re-raised by the
/// caller).
pub async fn run_command(
    _cwd: &std::path::Path,
    _event_name: &str,
    _command: &str,
    _args: &[String],
    _opts: &RunOptions,
) -> Result<i32> {
    todo!()
}
