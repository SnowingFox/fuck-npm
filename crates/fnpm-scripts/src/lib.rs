//! Lifecycle scripts: which packages may run them (trustedDependencies and the default
//! list), the postinstall optimizer, environment construction, and a concurrent runner in
//! dependency order. Also the single-script runner used by `fnpm run` / `exec`, multi-script
//! runs (`--parallel` / `--sequential`) and the `fnpm pm untrusted|trust|default-trusted`
//! support functions. See `docs/ARCHITECTURE.md#fnpm-scripts`.

pub mod env;
pub mod json;
pub mod multi;
pub mod node_gyp;
pub mod optimizer;
mod phf_hash;
pub mod pm;
pub mod rewrite;
pub mod run;
mod runner;
pub mod schedule;
pub mod scripts;
pub mod shell;
pub mod trust;

use std::path::PathBuf;

use fnpm_types::{EventSink, PackageId};

pub use optimizer::{Decision, Optimizer};
pub use rewrite::replace_package_manager_run;
pub use runner::signal_name;
pub use scripts::{PackageScripts, ScriptSource};
pub use trust::TrustPolicy;

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A required script exited with a non-zero code (bun: `<script> script from "<pkg>"
    /// exited with <code>`).
    #[error("{script} script from \"{package}\" exited with {code}")]
    Failed {
        package: String,
        script: String,
        code: i32,
    },
    /// A script was killed by a signal; the caller re-raises it.
    #[error("{script} script from \"{package}\" terminated by {}", signal_name(*signal))]
    Signaled {
        package: String,
        script: String,
        signal: i32,
    },
    /// The shell could not be started.
    #[error("could not spawn {shell}: {source}")]
    Spawn {
        shell: String,
        #[source]
        source: std::io::Error,
    },
    /// Reading a package.json or another file failed.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A usage error with bun's message (`fnpm pm trust`, multi-run).
    #[error("{0}")]
    Message(String),
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The lifecycle scripts fnpm runs, in order (bun `Scripts::NAMES`).
pub const LIFECYCLE: [&str; 6] = [
    "preinstall",
    "install",
    "postinstall",
    "preprepare",
    "prepare",
    "postprepare",
];

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
    /// The root's scripts run in the foreground (inherited stdio, command echoed, one at a
    /// time); everything else, workspaces included, is captured and shown only on failure.
    pub foreground: bool,
}

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Scripts started.
    pub ran: u32,
    /// Names of optional dependencies whose script failed; the caller removes them.
    pub failed_optional: Vec<String>,
    /// The directories of those packages (`Job::dir`), sorted.
    pub failed_optional_dirs: Vec<PathBuf>,
}

/// Settings of a run.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Default `2 * cpus` (0 also means the default).
    pub concurrency: usize,
    /// `--shell` / `run.shell`; default `sh` (POSIX) or `cmd.exe` (Windows).
    pub shell: Option<String>,
    /// Extra environment (`.env` files loaded by the caller, `npm_config_*`).
    pub env: Vec<(String, String)>,
    pub silent: bool,
    pub project_dir: PathBuf,
}

impl RunOptions {
    /// Defaults for `project_dir`.
    pub fn new(project_dir: impl Into<PathBuf>) -> Self {
        Self {
            concurrency: 0,
            shell: None,
            env: Vec::new(),
            silent: false,
            project_dir: project_dir.into(),
        }
    }
}

/// bun's default `max_concurrent_lifecycle_scripts`: twice the CPU count.
pub fn default_concurrency() -> usize {
    fnpm_types::available_parallelism() * 2
}

/// `--concurrent-scripts <N>`: `0` or an invalid value keeps the default.
pub fn parse_concurrency(value: &str) -> usize {
    match value.trim().parse::<usize>() {
        Ok(n) if n > 0 => n,
        _ => default_concurrency(),
    }
}

/// Runs `levels` (dependency order: every package in level N depends only on packages in
/// levels < N) with at most `concurrency` scripts at once. Child stdout/stderr is captured
/// and shown only on failure (bun), except for `foreground` jobs.
///
/// Prefer [`run_dag`] with [`schedule::dependencies`]: a level barrier makes a slow script
/// delay unrelated packages of the next level.
pub async fn run_jobs(
    levels: Vec<Vec<Job>>,
    opts: &RunOptions,
    events: &dyn EventSink,
) -> Result<Report> {
    let mut nodes = Vec::new();
    let mut deps: Vec<Vec<usize>> = Vec::new();
    let mut previous_barrier: Option<usize> = None;
    for level in levels {
        let first = nodes.len();
        for job in level {
            nodes.push(runner::Node::Job(Box::new(job)));
            deps.push(previous_barrier.into_iter().collect());
        }
        let barrier = nodes.len();
        let mut barrier_deps: Vec<usize> = (first..barrier).collect();
        barrier_deps.extend(previous_barrier);
        nodes.push(runner::Node::Barrier);
        deps.push(barrier_deps);
        previous_barrier = Some(barrier);
    }
    execute_with_node_gyp(nodes, deps, opts, events, None).await
}

/// Runs `jobs`, each after the jobs listed in `deps[i]` (indices into `jobs`) finished. A
/// dependency cycle is broken by starting the first waiting job.
pub async fn run_dag(
    jobs: Vec<Job>,
    deps: Vec<Vec<usize>>,
    opts: &RunOptions,
    events: &dyn EventSink,
) -> Result<Report> {
    run_dag_with_cleanup(jobs, deps, opts, events, None).await
}

/// Cleanup invoked after an optional lifecycle failure, before dependents run.
pub type OptionalCleanup<'a> = dyn Fn(&std::path::Path) -> std::io::Result<()> + Sync + 'a;

/// Like [`run_dag`], with optional-package cleanup before dependent jobs are released.
pub async fn run_dag_with_cleanup(
    jobs: Vec<Job>,
    deps: Vec<Vec<usize>>,
    opts: &RunOptions,
    events: &dyn EventSink,
    cleanup: Option<&OptionalCleanup<'_>>,
) -> Result<Report> {
    let mut deps = deps;
    deps.resize(jobs.len(), Vec::new());
    let nodes = jobs
        .into_iter()
        .map(|j| runner::Node::Job(Box::new(j)))
        .collect();
    execute_with_node_gyp(nodes, deps, opts, events, cleanup).await
}

async fn execute_with_node_gyp(
    nodes: Vec<runner::Node>,
    deps: Vec<Vec<usize>>,
    opts: &RunOptions,
    events: &dyn EventSink,
    cleanup: Option<&OptionalCleanup<'_>>,
) -> Result<Report> {
    let any = nodes
        .iter()
        .any(|n| matches!(n, runner::Node::Job(j) if !j.scripts.is_empty()));
    if !any {
        return Ok(Report::default());
    }
    let shim = node_gyp::NodeGypShim::create(&temp_dir(opts));
    if let Err(e) = &shim {
        events.emit(fnpm_types::Event::warn(
            format!("could not create the node-gyp shim: {e}"),
            None,
        ));
    }
    let dir = shim.as_ref().ok().map(|s| s.dir().to_path_buf());
    let report = runner::execute(nodes, deps, opts, events, dir.as_deref(), cleanup).await;
    drop(shim);
    report
}

/// `FNPM_TMPDIR`, `BUN_TMPDIR`, then the system temp dir.
fn temp_dir(opts: &RunOptions) -> PathBuf {
    for key in ["FNPM_TMPDIR", "BUN_TMPDIR"] {
        let v = opts
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(key).ok());
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            return PathBuf::from(v);
        }
    }
    std::env::temp_dir()
}

/// `fnpm run <script>` / `fnpm exec`: runs one command with `node_modules/.bin` on PATH,
/// `pre`/`post` hooks handled by the caller; returns the exit code (signals re-raised by the
/// caller). See [`run::run_script`] for the full `fnpm run` lookup with hooks.
pub async fn run_command(
    cwd: &std::path::Path,
    event_name: &str,
    command: &str,
    args: &[String],
    opts: &RunOptions,
) -> Result<i32> {
    run::run_shell_command(cwd, event_name, command, args, opts, "run-script").await
}
