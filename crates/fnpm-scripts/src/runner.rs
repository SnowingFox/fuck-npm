//! The concurrent lifecycle script runner (bun `lifecycle_script_runner.rs`,
//! `PackageManagerLifecycle.rs`, `PackageInstaller.rs` script gating).
//!
//! Jobs form a DAG: a job starts once every job it depends on finished. At most
//! `concurrency` background jobs (one package's script chain each) run at once; foreground
//! jobs (the root) run one at a time with inherited stdio. Background output is
//! captured and printed to stderr only when a script fails.

use std::collections::VecDeque;
use std::io::{IsTerminal as _, Write as _};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fnpm_types::event::ScriptPhase;
use fnpm_types::{Event, EventSink};
use parking_lot::Mutex;
use tokio::io::AsyncReadExt as _;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::env::{BaseEnv, PackageVars};
use crate::shell::Shell;
use crate::{Error, Job, Report, Result, RunOptions};

/// One node of the schedule: a package's scripts or a level barrier.
pub(crate) enum Node {
    Job(Box<Job>),
    Barrier,
}

/// Settings read from the environment once per run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Flags {
    pub verbose: bool,
    pub slow_warning: bool,
}

impl Flags {
    pub(crate) fn from_options(opts: &RunOptions) -> Self {
        let get = |keys: &[&str]| {
            keys.iter().find_map(|k| {
                opts.env
                    .iter()
                    .rev()
                    .find(|(ek, _)| ek == k)
                    .map(|(_, v)| v.clone())
                    .or_else(|| std::env::var(k).ok())
            })
        };
        let truthy = |v: Option<String>| {
            v.is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
        };
        Self {
            verbose: truthy(get(&["FNPM_VERBOSE", "BUN_INSTALL_VERBOSE"])),
            slow_warning: !truthy(get(&[
                "FNPM_DISABLE_SLOW_LIFECYCLE_SCRIPT_LOGGING",
                "BUN_DISABLE_SLOW_LIFECYCLE_SCRIPT_LOGGING",
            ])),
        }
    }
}

struct Ctx {
    base: BaseEnv,
    project_dir: std::path::PathBuf,
    shell: Shell,
    silent: bool,
    verbose: bool,
    background: Semaphore,
    foreground: Semaphore,
    /// `(node index, package name, started)` of every script running now.
    active: Mutex<Vec<(usize, String, Instant)>>,
}

/// Why a job stopped.
enum Failure {
    Exited(i32),
    #[cfg(unix)]
    Signaled(i32),
    Spawn(std::io::Error),
}

impl Failure {
    fn is_signal(&self) -> bool {
        #[cfg(unix)]
        {
            matches!(self, Self::Signaled(_))
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

struct JobFailure {
    script: String,
    failure: Failure,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct JobDone {
    index: usize,
    name: String,
    dir: std::path::PathBuf,
    optional: bool,
    is_dependency: bool,
    ran: u32,
    result: std::result::Result<(), JobFailure>,
}

/// Runs the DAG. `deps[i]` lists the nodes `i` waits for.
pub(crate) async fn execute(
    nodes: Vec<Node>,
    deps: Vec<Vec<usize>>,
    opts: &RunOptions,
    events: &dyn EventSink,
    node_gyp_dir: Option<&std::path::Path>,
    cleanup: Option<&crate::OptionalCleanup<'_>>,
) -> Result<Report> {
    let n = nodes.len();
    debug_assert_eq!(deps.len(), n);
    let mut report = Report::default();
    if nodes
        .iter()
        .all(|node| !matches!(node, Node::Job(j) if !j.scripts.is_empty()))
    {
        return Ok(report);
    }

    let flags = Flags::from_options(opts);
    let base = BaseEnv::lifecycle(
        &opts.project_dir,
        std::env::vars_os(),
        &opts.env,
        node_gyp_dir,
    );
    let shell = crate::shell::select(
        opts.shell.as_deref(),
        base.original_path(),
        &opts.project_dir,
    )
    .ok_or_else(|| Error::Spawn {
        shell: "sh".into(),
        source: std::io::Error::other("no shell found on PATH"),
    })?;
    let concurrency = if opts.concurrency == 0 {
        crate::default_concurrency()
    } else {
        opts.concurrency
    };
    let ctx = Arc::new(Ctx {
        base,
        project_dir: opts.project_dir.clone(),
        shell,
        silent: opts.silent,
        verbose: flags.verbose,
        background: Semaphore::new(concurrency),
        foreground: Semaphore::new(1),
        active: Mutex::new(Vec::new()),
    });

    let mut remaining: Vec<usize> = vec![0; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ds) in deps.iter().enumerate() {
        for &d in ds {
            if d < n && d != i {
                remaining[i] += 1;
                dependents[d].push(i);
            }
        }
    }
    let mut nodes: Vec<Option<Node>> = nodes.into_iter().map(Some).collect();
    let mut done = vec![false; n];
    let mut finished = 0usize;
    let mut ready: VecDeque<usize> = (0..n).filter(|&i| remaining[i] == 0).collect();

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let mut set: JoinSet<JobDone> = JoinSet::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let interval = if flags.verbose {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(30)
    };
    let mut last_slow_report: Option<Instant> = None;

    let mut complete =
        |i: usize, ready: &mut VecDeque<usize>, done: &mut Vec<bool>, finished: &mut usize| {
            if std::mem::replace(&mut done[i], true) {
                return;
            }
            *finished += 1;
            for &d in &dependents[i] {
                remaining[d] -= 1;
                if remaining[d] == 0 {
                    ready.push_back(d);
                }
            }
        };

    loop {
        while let Some(i) = ready.pop_front() {
            match nodes[i].take() {
                Some(Node::Job(job)) if !job.scripts.is_empty() => {
                    let ctx = Arc::clone(&ctx);
                    let tx = tx.clone();
                    set.spawn(async move { run_job(i, *job, ctx, tx).await });
                }
                _ => complete(i, &mut ready, &mut done, &mut finished),
            }
        }
        if set.is_empty() {
            if finished == n {
                break;
            }
            // A dependency cycle among the remaining nodes: start the first one anyway.
            if let Some(i) = (0..n).find(|&i| !done[i] && nodes[i].is_some()) {
                ready.push_back(i);
                continue;
            }
            break;
        }
        tokio::select! {
            biased;
            Some(event) = rx.recv() => events.emit(event),
            joined = set.join_next() => {
                let Some(joined) = joined else { continue };
                let done_job = match joined {
                    Ok(d) => d,
                    Err(e) => {
                        set.abort_all();
                        return Err(Error::Spawn { shell: ctx.shell.program.display().to_string(), source: std::io::Error::other(e) });
                    }
                };
                // Deliver this job's events before acting on its result.
                while let Ok(event) = rx.try_recv() {
                    events.emit(event);
                }
                report.ran += done_job.ran;
                let i = done_job.index;
                match done_job.result {
                    Ok(()) => complete(i, &mut ready, &mut done, &mut finished),
                    Err(failure) => {
                        let tolerated = done_job.optional && !failure.failure.is_signal();
                        if tolerated {
                            if let Some(cleanup) = cleanup
                                && let Err(source) = cleanup(&done_job.dir)
                            {
                                set.abort_all();
                                while set.join_next().await.is_some() {}
                                return Err(Error::Io { path: done_job.dir, source });
                            }
                            report.failed_optional.push(done_job.name.clone());
                            report.failed_optional_dirs.push(done_job.dir.clone());
                            if ctx.verbose {
                                events.emit(Event::warn(
                                    format!(
                                        "deleting optional dependency '{}' due to failed '{}' script",
                                        done_job.name, failure.script
                                    ),
                                    Some("OPTIONAL_SCRIPT_FAILED"),
                                ));
                            }
                            complete(i, &mut ready, &mut done, &mut finished);
                            continue;
                        }
                        set.abort_all();
                        while set.join_next().await.is_some() {}
                        while let Ok(event) = rx.try_recv() {
                            events.emit(event);
                        }
                        if !ctx.silent {
                            print_failure_output(&failure, done_job.is_dependency);
                        }
                        return Err(match failure.failure {
                            Failure::Exited(code) => Error::Failed { package: done_job.name, script: failure.script, code },
                            #[cfg(unix)]
                            Failure::Signaled(signal) => Error::Signaled { package: done_job.name, script: failure.script, signal },
                            Failure::Spawn(source) => Error::Spawn { shell: ctx.shell.program.display().to_string(), source },
                        });
                    }
                }
            }
            _ = ticker.tick(), if flags.slow_warning && !ctx.silent => {
                if let Some(message) = slow_script_message(&ctx, interval, &mut last_slow_report) {
                    events.emit(Event::warn(message, Some("SLOW_SCRIPT")));
                }
            }
        }
    }
    while let Ok(event) = rx.try_recv() {
        events.emit(event);
    }
    report.failed_optional.sort();
    report.failed_optional_dirs.sort();
    Ok(report)
}

/// Runs one package's scripts in order; the next starts after the previous exits 0.
async fn run_job(
    index: usize,
    job: Job,
    ctx: Arc<Ctx>,
    tx: mpsc::UnboundedSender<Event>,
) -> JobDone {
    let semaphore = if job.foreground {
        &ctx.foreground
    } else {
        &ctx.background
    };
    let _permit = semaphore.acquire().await.ok();
    let is_dependency = job_is_dependency(&job, &ctx.project_dir);
    let mut done = JobDone {
        index,
        name: job.name.clone(),
        dir: job.dir.clone(),
        optional: job.optional,
        is_dependency,
        ran: 0,
        result: Ok(()),
    };

    // npm_package_* come from the package.json on disk, like bun's `put_npm_package_env`.
    let (name, version, config) = match tokio::fs::read(&job.package_json).await {
        Ok(bytes) => std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| crate::json::parse_package_json(text).ok())
            .map(|json| crate::env::package_json_vars(&json))
            .unwrap_or_default(),
        Err(_) => Default::default(),
    };
    let name = name.unwrap_or_else(|| job.name.clone());
    let version = version.unwrap_or_else(|| job.version.clone());
    let vars = PackageVars {
        dir: &job.dir,
        package_json: &job.package_json,
        name: &name,
        version: &version,
        config: &config,
    };
    let mut env = ctx.base.for_package(&vars, is_dependency);

    for (event, script) in &job.scripts {
        env.set_lifecycle(event, script);
        let command = crate::rewrite::replace_package_manager_run(script);
        if job.foreground && !ctx.silent {
            print_command(&command);
        }
        let _ = tx.send(Event::Script {
            package: job.name.clone(),
            script: event.clone(),
            phase: ScriptPhase::Start,
            exit_code: None,
        });
        ctx.active
            .lock()
            .push((index, job.name.clone(), Instant::now()));
        let outcome = spawn_script(&ctx, &job, &env, &command).await;
        ctx.active.lock().retain(|(i, _, _)| *i != index);
        done.ran += 1;

        let (failure, stdout, stderr) = match outcome {
            Ok((status, stdout, stderr)) => match exit_failure(status) {
                None => {
                    let _ = tx.send(Event::Script {
                        package: job.name.clone(),
                        script: event.clone(),
                        phase: ScriptPhase::End,
                        exit_code: Some(0),
                    });
                    continue;
                }
                Some(f) => (f, stdout, stderr),
            },
            Err(e) => (Failure::Spawn(e), Vec::new(), Vec::new()),
        };
        let exit_code = match &failure {
            Failure::Exited(code) => Some(*code),
            #[cfg(unix)]
            Failure::Signaled(signal) => Some(128 + *signal),
            Failure::Spawn(_) => None,
        };
        let _ = tx.send(Event::Script {
            package: job.name.clone(),
            script: event.clone(),
            phase: ScriptPhase::End,
            exit_code,
        });
        done.result = Err(JobFailure {
            script: event.clone(),
            failure,
            stdout,
            stderr,
        });
        break;
    }
    done
}

/// bun `List::is_dependency`: not the project's or a workspace's own scripts. Workspaces run
/// in the background like dependencies, so this is read from where the package lives: inside
/// a `node_modules` (or the global store) below the project.
fn job_is_dependency(job: &Job, project_dir: &std::path::Path) -> bool {
    !job.foreground
        && crate::env::is_dependency_dir(job.dir.strip_prefix(project_dir).unwrap_or(&job.dir))
}

/// `None` for success (bun treats only `code > 0` and signals as failures).
fn exit_failure(status: ExitStatus) -> Option<Failure> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return Some(Failure::Signaled(signal));
        }
    }
    match status.code() {
        Some(code) if code > 0 => Some(Failure::Exited(code)),
        Some(code) if code < 0 => Some(Failure::Exited(1)),
        _ => None,
    }
}

/// Spawns `command` in the job's directory. Background output is captured (or dropped when
/// silent, or inherited when verbose); foreground jobs inherit stdio.
async fn spawn_script(
    ctx: &Ctx,
    job: &Job,
    env: &crate::env::EnvMap,
    command: &str,
) -> std::io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut cmd = shell_command(&ctx.shell, command);
    cmd.current_dir(&job.dir)
        .env_clear()
        .envs(env.iter())
        .kill_on_drop(true);
    let capture = !job.foreground && !ctx.verbose && !ctx.silent;
    let output = || {
        if ctx.silent {
            Stdio::null()
        } else if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        }
    };
    cmd.stdin(if job.foreground {
        Stdio::inherit()
    } else {
        Stdio::null()
    })
    .stdout(output())
    .stderr(output());
    keep_stdio(&mut cmd);
    set_parent_death_signal(&mut cmd);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (out, err, status) = tokio::join!(read_all(stdout), read_all(stderr), child.wait());
    Ok((status?, out, err))
}

/// `shell -c command` (POSIX) or `cmd /d /s /c command` with the command passed verbatim.
pub(crate) fn shell_command(shell: &Shell, command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&shell.program);
    cmd.args(&shell.flags);
    #[cfg(windows)]
    {
        if shell.flags.iter().any(|f| f == "/c") {
            cmd.raw_arg(crate::env::until_nul(command));
            return cmd;
        }
    }
    // A NUL cannot be passed in argv; bun's C string of the script ends there too.
    cmd.arg(crate::env::until_nul(command));
    cmd
}

/// Clears `FD_CLOEXEC` on fds 0-2 in the child. `Stdio::inherit` leaves the parent's fds as
/// they are, and node (which hosts the napi core) marks its stdio close-on-exec, so a script
/// started from there would run without a stdout.
pub(crate) fn keep_stdio(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // SAFETY: the closure runs in the forked child before exec and only makes the
        // async-signal-safe `fcntl` calls on fds that are borrowed for the call.
        unsafe {
            cmd.pre_exec(|| {
                for fd in 0..=2 {
                    let borrowed = std::os::fd::BorrowedFd::borrow_raw(fd);
                    if let Ok(flags) = rustix::io::fcntl_getfd(borrowed) {
                        let _ = rustix::io::fcntl_setfd(
                            borrowed,
                            flags.difference(rustix::io::FdFlags::CLOEXEC),
                        );
                    }
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// The kernel kills the child when its parent dies, so no script outlives the install
/// (bun `linux_pdeathsig: SIGKILL`). Linux only.
pub(crate) fn set_parent_death_signal(cmd: &mut tokio::process::Command) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: the closure runs in the forked child before exec and only makes the
        // async-signal-safe `prctl(PR_SET_PDEATHSIG)` syscall.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::KILL,
                ))
                .map_err(std::io::Error::from)
            });
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = cmd;
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(reader: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut r) = reader {
        let _ = r.read_to_end(&mut buf).await;
    }
    buf
}

/// bun `Output::command`: `$ <command>` on stderr, dimmed on a terminal.
pub(crate) fn print_command(command: &str) {
    let mut stderr = std::io::stderr().lock();
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        let _ = writeln!(
            stderr,
            "\x1b[0m\x1b[2m\x1b[35m$\x1b[0m \x1b[2m\x1b[1m{command}\x1b[0m"
        );
    } else {
        let _ = writeln!(stderr, "$ {command}");
    }
    let _ = stderr.flush();
}

/// bun `print_output`: the captured stdout, then stderr, each followed by a newline, raw bytes.
fn print_failure_output(failure: &JobFailure, is_dependency: bool) {
    let mut stderr = std::io::stderr().lock();
    for buf in [&failure.stdout, &failure.stderr] {
        if !buf.is_empty() {
            let _ = stderr.write_all(buf);
            let _ = stderr.write_all(b"\n");
        }
    }
    // Git dependencies are the only dependencies whose `prepare` scripts run.
    if is_dependency
        && failure.script.ends_with("prepare")
        && matches!(failure.failure, Failure::Exited(_))
    {
        let _ = writeln!(
            stderr,
            "note: devDependencies of a git dependency are not installed before its prepare scripts"
        );
    }
    let _ = stderr.flush();
}

/// bun `report_slow_lifecycle_scripts`: `<pkg>'s postinstall cost you <dur>` for the longest
/// running script, at most once per interval.
fn slow_script_message(
    ctx: &Ctx,
    interval: Duration,
    last: &mut Option<Instant>,
) -> Option<String> {
    let now = Instant::now();
    let (name, started) = {
        let active = ctx.active.lock();
        let (_, name, started) = active.iter().min_by_key(|(_, _, started)| *started)?;
        (name.clone(), *started)
    };
    let running = now.duration_since(started);
    if running <= interval || last.is_some_and(|l| now.duration_since(l) <= interval) {
        return None;
    }
    *last = Some(now);
    Some(slow_message(&name, running))
}

pub(crate) fn slow_message(package: &str, running: Duration) -> String {
    let ends_in_s = package.len() > 1 && package.ends_with('s');
    format!(
        "{package}'{} postinstall cost you {}",
        if ends_in_s { "" } else { "s" },
        format_duration_one_decimal(running.as_nanos().min(u128::from(u64::MAX)) as u64)
    )
}

/// bun `fmt_duration_one_decimal`: `[#y][#w][#d][#h][#m]#[.#](s|ms|us)` or `#ns`.
pub(crate) fn format_duration_one_decimal(ns: u64) -> String {
    const NS_PER_US: u64 = 1_000;
    const NS_PER_MS: u64 = 1_000_000;
    const NS_PER_S: u64 = 1_000_000_000;
    const NS_PER_MIN: u64 = 60 * NS_PER_S;
    const NS_PER_HOUR: u64 = 60 * NS_PER_MIN;
    const NS_PER_DAY: u64 = 24 * NS_PER_HOUR;
    const NS_PER_WEEK: u64 = 7 * NS_PER_DAY;
    let mut out = String::new();
    let mut rest = ns;
    for (unit, sep) in [
        (365 * NS_PER_DAY, 'y'),
        (NS_PER_WEEK, 'w'),
        (NS_PER_DAY, 'd'),
        (NS_PER_HOUR, 'h'),
        (NS_PER_MIN, 'm'),
    ] {
        if rest >= unit {
            let units = rest / unit;
            out.push_str(&units.to_string());
            out.push(sep);
            rest -= units * unit;
            if rest == 0 {
                return out;
            }
        }
    }
    for (unit, sep) in [(NS_PER_S, "s"), (NS_PER_MS, "ms"), (NS_PER_US, "us")] {
        let kunits = u128::from(rest) * 1000 / u128::from(unit);
        if kunits >= 1000 {
            out.push_str(&(kunits / 1000).to_string());
            let frac = (kunits % 1000) / 100;
            if frac > 0 {
                out.push('.');
                out.push_str(&frac.to_string());
            }
            out.push_str(sep);
            return out;
        }
    }
    out.push_str(&rest.to_string());
    out.push_str("ns");
    out
}

/// `SIGTERM`-style names for messages.
pub fn signal_name(signal: i32) -> String {
    const NAMES: [&str; 32] = [
        "",
        "SIGHUP",
        "SIGINT",
        "SIGQUIT",
        "SIGILL",
        "SIGTRAP",
        "SIGABRT",
        "SIGBUS",
        "SIGFPE",
        "SIGKILL",
        "SIGUSR1",
        "SIGSEGV",
        "SIGUSR2",
        "SIGPIPE",
        "SIGALRM",
        "SIGTERM",
        "SIGSTKFLT",
        "SIGCHLD",
        "SIGCONT",
        "SIGSTOP",
        "SIGTSTP",
        "SIGTTIN",
        "SIGTTOU",
        "SIGURG",
        "SIGXCPU",
        "SIGXFSZ",
        "SIGVTALRM",
        "SIGPROF",
        "SIGWINCH",
        "SIGIO",
        "SIGPWR",
        "SIGSYS",
    ];
    #[cfg(target_os = "macos")]
    let name = match signal {
        7 => Some("SIGEMT"),
        10 => Some("SIGBUS"),
        12 => Some("SIGSYS"),
        16 => Some("SIGURG"),
        17 => Some("SIGSTOP"),
        18 => Some("SIGTSTP"),
        19 => Some("SIGCONT"),
        20 => Some("SIGCHLD"),
        23 => Some("SIGIO"),
        29 => Some("SIGINFO"),
        30 => Some("SIGUSR1"),
        31 => Some("SIGUSR2"),
        _ => None,
    };
    #[cfg(not(target_os = "macos"))]
    let name: Option<&str> = None;
    name.or_else(|| {
        usize::try_from(signal)
            .ok()
            .and_then(|i| NAMES.get(i).copied())
            .filter(|n| !n.is_empty())
    })
    .map_or_else(|| format!("signal {signal}"), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_like_bun() {
        let s = 1_000_000_000u64;
        assert_eq!(format_duration_one_decimal(31 * s), "31s");
        assert_eq!(format_duration_one_decimal(31 * s + 250_000_000), "31.2s");
        assert_eq!(format_duration_one_decimal(90 * s), "1m30s");
        assert_eq!(format_duration_one_decimal(60 * s), "1m");
        assert_eq!(format_duration_one_decimal(1_500_000), "1.5ms");
        assert_eq!(format_duration_one_decimal(2_000), "2us");
        assert_eq!(format_duration_one_decimal(999), "999ns");
        assert_eq!(format_duration_one_decimal(3600 * s + 5 * s), "1h5s");
    }

    #[test]
    fn slow_script_messages() {
        assert_eq!(
            slow_message("esbuild", Duration::from_secs(31)),
            "esbuild's postinstall cost you 31s"
        );
        assert_eq!(
            slow_message("canvas", Duration::from_secs(45)),
            "canvas' postinstall cost you 45s"
        );
        assert_eq!(
            slow_message("s", Duration::from_secs(45)),
            "s's postinstall cost you 45s"
        );
    }

    #[test]
    fn signal_names() {
        assert_eq!(signal_name(9), "SIGKILL");
        assert_eq!(signal_name(15), "SIGTERM");
        assert_eq!(signal_name(2), "SIGINT");
        assert_eq!(signal_name(0), "signal 0");
        assert_eq!(signal_name(99), "signal 99");
    }

    #[test]
    fn flags_from_options() {
        let mut o = RunOptions::new("/");
        o.env.push(("FNPM_VERBOSE".into(), "1".into()));
        o.env.push((
            "BUN_DISABLE_SLOW_LIFECYCLE_SCRIPT_LOGGING".into(),
            "true".into(),
        ));
        let f = Flags::from_options(&o);
        assert!(f.verbose && !f.slow_warning);
    }
}
