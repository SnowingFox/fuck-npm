//! `fnpm run --parallel` / `--sequential` (bun `multi_run.rs`): several scripts at once with
//! Foreman-style `label | ` prefixes, `*` globs over script names, `pre`/`post` chaining and
//! `--no-exit-on-error`.

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::sync::mpsc;

use crate::env::{BaseEnv, PackageVars};
use crate::{Error, Result, RunOptions};

/// A package whose scripts can be selected.
#[derive(Debug, Clone)]
pub struct MultiPackage {
    /// Workspace mode: labels become `<prefix>:<script>`.
    pub label_prefix: Option<String>,
    pub dir: PathBuf,
    /// `None` when there is no package.json.
    pub package_json: Option<PathBuf>,
    /// `scripts` in package.json order.
    pub scripts: Vec<(String, String)>,
}

/// Flags of a multi-script run.
#[derive(Debug, Clone, Copy, Default)]
pub struct MultiOptions {
    /// `--sequential`: each script group starts after the previous one finished.
    pub sequential: bool,
    /// `--no-exit-on-error`: a failure does not stop the others.
    pub no_exit_on_error: bool,
    /// `--if-present` (workspace mode: packages without the script are skipped silently).
    pub if_present: bool,
    /// `--workspaces`: a package without the script is an error unless `if_present`.
    pub workspaces: bool,
}

/// One process of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiScript {
    pub label: String,
    /// The package.json script name; empty for a raw command.
    pub name: String,
    pub command: String,
    pub cwd: PathBuf,
    pub package_json: Option<PathBuf>,
    /// Index of the script group (pre/main/post of one name) for colors and chaining.
    pub group: usize,
}

/// bun `matches_glob`: `*` matches any sequence.
pub fn matches_glob(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star_pi, mut star_ni, mut have_star) = (0usize, 0usize, false);
    while ni < n.len() || pi < p.len() {
        if pi < p.len() && p[pi] == b'*' {
            have_star = true;
            star_pi = pi;
            star_ni = ni;
            pi += 1;
        } else if pi < p.len() && ni < n.len() && p[pi] == n[ni] {
            pi += 1;
            ni += 1;
        } else if have_star {
            pi = star_pi + 1;
            star_ni += 1;
            ni = star_ni;
            if ni > n.len() {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}

fn escape_label(label: &str) -> String {
    label
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// bun `add_script_configs`: a package.json script with its `pre`/`post` hooks, or a raw
/// command (a JavaScript file runs with `node`).
fn add_script(out: &mut Vec<MultiScript>, group: usize, raw: &str, pkg: &MultiPackage) {
    let label = escape_label(&match &pkg.label_prefix {
        Some(prefix) => format!("{prefix}:{raw}"),
        None => raw.to_owned(),
    });
    let get = |n: &str| {
        pkg.scripts
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    };
    let mut push = |name: String, command: String| {
        out.push(MultiScript {
            label: label.clone(),
            name,
            command,
            cwd: pkg.dir.clone(),
            package_json: pkg.package_json.clone(),
            group,
        });
    };
    if let Some(main) = get(raw) {
        let pre = format!("pre{raw}");
        let post = format!("post{raw}");
        if let Some(s) = get(&pre) {
            push(pre.clone(), crate::rewrite::replace_package_manager_run(s));
        }
        push(
            raw.to_owned(),
            crate::rewrite::replace_package_manager_run(main),
        );
        if let Some(s) = get(&post) {
            push(post.clone(), crate::rewrite::replace_package_manager_run(s));
        }
    } else {
        let is_js = [".js", ".mjs", ".cjs"].iter().any(|ext| raw.ends_with(ext));
        let command = if is_js {
            let mut c = String::from("node ");
            crate::shell::push_escaped(&mut c, raw);
            c
        } else {
            raw.to_owned()
        };
        push(String::new(), command);
    }
}

/// Expands `names` (globs allowed) against `packages` into the processes to start, with
/// bun's errors. One package without a prefix is single-package mode; otherwise packages are
/// sorted by label prefix, then directory.
pub fn plan(
    names: &[String],
    packages: &[MultiPackage],
    mo: MultiOptions,
) -> Result<Vec<MultiScript>> {
    let names: Vec<&String> = names.iter().filter(|n| !n.is_empty()).collect();
    if names.is_empty() {
        return Err(Error::Message(
            "--parallel/--sequential requires at least one script name".into(),
        ));
    }
    let mut out = Vec::new();
    let mut group = 0usize;
    let workspace_mode = packages.iter().any(|p| p.label_prefix.is_some());
    if workspace_mode {
        let mut sorted: Vec<&MultiPackage> = packages.iter().collect();
        sorted.sort_by(|a, b| {
            a.label_prefix
                .cmp(&b.label_prefix)
                .then_with(|| a.dir.cmp(&b.dir))
        });
        for pkg in sorted {
            for raw in &names {
                if raw.contains('*') {
                    let mut matches: Vec<&str> = pkg
                        .scripts
                        .iter()
                        .map(|(k, _)| k.as_str())
                        .filter(|k| matches_glob(raw, k))
                        .collect();
                    matches.sort_unstable();
                    for m in matches {
                        add_script(&mut out, group, m, pkg);
                        group += 1;
                    }
                } else if pkg.scripts.iter().any(|(k, _)| k == *raw) {
                    add_script(&mut out, group, raw, pkg);
                    group += 1;
                } else if mo.workspaces && !mo.if_present {
                    return Err(Error::Message(format!(
                        "Missing \"{raw}\" script in package \"{}\"",
                        pkg.label_prefix.as_deref().unwrap_or_default()
                    )));
                }
            }
        }
        if out.is_empty() && !mo.if_present {
            let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
            return Err(Error::Message(format!(
                "No packages matched the filter with script {}",
                quoted.join(", ")
            )));
        }
        return Ok(out);
    }
    let Some(pkg) = packages.first() else {
        return Err(Error::Message("No scripts to run".into()));
    };
    for raw in &names {
        if raw.contains('*') {
            if pkg.package_json.is_none() {
                return Err(Error::Message(format!(
                    "Cannot use glob pattern \"{raw}\" without package.json scripts"
                )));
            }
            let mut matches: Vec<&str> = pkg
                .scripts
                .iter()
                .map(|(k, _)| k.as_str())
                .filter(|k| matches_glob(raw, k))
                .collect();
            matches.sort_unstable();
            if matches.is_empty() {
                return Err(Error::Message(format!(
                    "No scripts match pattern \"{raw}\""
                )));
            }
            for m in matches {
                add_script(&mut out, group, m, pkg);
                group += 1;
            }
        } else {
            add_script(&mut out, group, raw, pkg);
            group += 1;
        }
    }
    if out.is_empty() {
        return Err(Error::Message("No scripts to run".into()));
    }
    Ok(out)
}

/// How long output is still read after a script exited while its pipes stay open.
const DRAIN_AFTER_EXIT: std::time::Duration = std::time::Duration::from_millis(100);

const COLORS: [&str; 6] = [
    "\x1b[36m", "\x1b[33m", "\x1b[35m", "\x1b[32m", "\x1b[34m", "\x1b[31m",
];
const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Running,
    Exited(i32),
    #[cfg(unix)]
    Signaled(i32),
    Error,
    Skipped,
}

enum Msg {
    Line {
        index: usize,
        stderr: bool,
        line: Vec<u8>,
    },
    Done {
        index: usize,
        status: Status,
        elapsed: std::time::Duration,
    },
}

/// Runs the planned scripts and returns the exit code (bun `finalize`): the first script
/// (in plan order) that failed decides it, a signal maps to `128 + signal`.
pub async fn run(scripts: Vec<MultiScript>, opts: &RunOptions, mo: MultiOptions) -> Result<i32> {
    let n = scripts.len();
    if n == 0 {
        return Err(Error::Message("No scripts to run".into()));
    }
    let base = BaseEnv::run(
        &opts.project_dir,
        std::env::vars_os(),
        &opts.env,
        "run-script",
    );
    let original_path = base.original_path().to_owned();
    let shell = crate::shell::select(opts.shell.as_deref(), &original_path, &opts.project_dir)
        .ok_or_else(|| Error::Message("Failed to find a shell".into()))?;
    let max_label = scripts.iter().map(|s| s.label.len()).max().unwrap_or(0);
    let colors = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();

    // Dependencies: pre -> main -> post inside a group (skipped on failure), and with
    // `--sequential` the last of a group -> the first of the next (continues with
    // `--no-exit-on-error`).
    let mut group_next: Vec<Option<usize>> = vec![None; n];
    let mut seq_next: Vec<Option<usize>> = vec![None; n];
    let mut waits: Vec<usize> = vec![0; n];
    for i in 0..n.saturating_sub(1) {
        if scripts[i].group == scripts[i + 1].group {
            group_next[i] = Some(i + 1);
            waits[i + 1] += 1;
        } else if mo.sequential {
            seq_next[i] = Some(i + 1);
            waits[i + 1] += 1;
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut status = vec![Status::Pending; n];
    let mut children: Vec<Option<u32>> = vec![None; n];
    let mut tasks = tokio::task::JoinSet::new();
    let mut aborted = false;
    let mut remaining = 0usize;

    let start = |i: usize,
                 status: &mut [Status],
                 children: &mut [Option<u32>],
                 tasks: &mut tokio::task::JoinSet<()>,
                 remaining: &mut usize| {
        let script = &scripts[i];
        let mut env = base.env().clone();
        if let Some(pj) = &script.package_json {
            let json = crate::json::read_package_json(pj).ok();
            let (name, version, config) = json
                .as_ref()
                .map(crate::env::package_json_vars)
                .unwrap_or_default();
            PackageVars {
                dir: &script.cwd,
                package_json: pj,
                name: name.as_deref().unwrap_or_default(),
                version: version.as_deref().unwrap_or_default(),
                config: &config,
            }
            .apply(&mut env);
        }
        env.set(
            "PATH",
            crate::env::path_for_run(&script.cwd, &original_path),
        );
        if !script.name.is_empty() {
            env.set_lifecycle(&script.name, &script.command);
        }
        let mut cmd = crate::runner::shell_command(&shell, &script.command);
        cmd.current_dir(&script.cwd)
            .env_clear()
            .envs(env.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        *remaining += 1;
        status[i] = Status::Running;
        let started = Instant::now();
        match cmd.spawn() {
            Ok(mut child) => {
                children[i] = child.id();
                let tx = tx.clone();
                let out = child.stdout.take();
                let err = child.stderr.take();
                tasks.spawn(async move {
                    let mut pumps = Box::pin({
                        let tx = tx.clone();
                        async move {
                            tokio::join!(pump(out, i, false, tx.clone()), pump(err, i, true, tx));
                        }
                    });
                    let mut pumps_done = false;
                    let st = loop {
                        tokio::select! {
                            st = child.wait() => break st,
                            () = &mut pumps, if !pumps_done => pumps_done = true,
                        }
                    };
                    // bun `drain_and_close_pipes`: a background grandchild may hold the pipes
                    // open; read what is buffered, then stop instead of waiting for it.
                    if !pumps_done {
                        let _ = tokio::time::timeout(DRAIN_AFTER_EXIT, &mut pumps).await;
                    }
                    drop(pumps);
                    let status = match st {
                        Ok(s) => exit_status(s),
                        Err(_) => Status::Error,
                    };
                    let _ = tx.send(Msg::Done {
                        index: i,
                        status,
                        elapsed: started.elapsed(),
                    });
                });
            }
            Err(_) => {
                let _ = tx.send(Msg::Done {
                    index: i,
                    status: Status::Error,
                    elapsed: started.elapsed(),
                });
            }
        }
    };

    let roots: Vec<usize> = (0..n).filter(|&i| waits[i] == 0).collect();
    for i in roots {
        start(i, &mut status, &mut children, &mut tasks, &mut remaining);
    }

    while remaining > 0 {
        let Some(msg) = rx.recv().await else { break };
        match msg {
            Msg::Line {
                index,
                stderr,
                line,
            } => {
                write_line(&scripts[index], max_label, colors, stderr, &line);
            }
            Msg::Done {
                index,
                status: st,
                elapsed,
            } => {
                remaining -= 1;
                status[index] = st;
                children[index] = None;
                let text = match st {
                    Status::Exited(0) => {
                        let ms = elapsed.as_secs_f64() * 1000.0;
                        if ms > 1000.0 {
                            format!("Done in {:.2}s", ms / 1000.0)
                        } else {
                            format!("Done in {ms:.0}ms")
                        }
                    }
                    Status::Exited(code) => format!("Exited with code {code}"),
                    #[cfg(unix)]
                    Status::Signaled(sig) => format!("Signaled: {}", crate::signal_name(sig)),
                    _ => "Error".to_owned(),
                };
                write_line(
                    &scripts[index],
                    max_label,
                    colors,
                    true,
                    format!("{text}\n").as_bytes(),
                );
                let failed = !matches!(st, Status::Exited(0));
                if failed && !mo.no_exit_on_error {
                    if !aborted {
                        aborted = true;
                        for pid in children.iter().flatten() {
                            interrupt(*pid);
                        }
                    }
                    continue;
                }
                if aborted {
                    continue;
                }
                if failed {
                    // The pre -> main -> post chain is broken; the next group still starts.
                    let mut j = index;
                    while let Some(next) = group_next[j] {
                        status[next] = Status::Skipped;
                        j = next;
                    }
                    if let Some(next) = seq_next[j] {
                        waits[next] -= 1;
                        if waits[next] == 0 {
                            start(next, &mut status, &mut children, &mut tasks, &mut remaining);
                        }
                    }
                    continue;
                }
                for next in [group_next[index], seq_next[index]].into_iter().flatten() {
                    waits[next] -= 1;
                    if waits[next] == 0 {
                        start(next, &mut status, &mut children, &mut tasks, &mut remaining);
                    }
                }
            }
        }
    }
    while tasks.join_next().await.is_some() {}
    for st in &status {
        match *st {
            Status::Exited(0) | Status::Pending | Status::Skipped | Status::Running => {}
            Status::Exited(code) => return Ok(code),
            #[cfg(unix)]
            Status::Signaled(sig) => return Ok(128 + sig),
            Status::Error => return Ok(1),
        }
    }
    Ok(0)
}

fn exit_status(s: std::process::ExitStatus) -> Status {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(sig) = s.signal() {
            return Status::Signaled(sig);
        }
    }
    Status::Exited(s.code().unwrap_or(1))
}

/// SIGINT to a running script (bun `abort`).
fn interrupt(pid: u32) {
    #[cfg(unix)]
    {
        if let Some(pid) = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

async fn pump<R: AsyncRead + Unpin>(
    reader: Option<R>,
    index: usize,
    stderr: bool,
    tx: mpsc::UnboundedSender<Msg>,
) {
    let Some(reader) = reader else { return };
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if !line.ends_with(b"\n") {
                    line.push(b'\n');
                }
                let _ = tx.send(Msg::Line {
                    index,
                    stderr,
                    line,
                });
            }
        }
    }
}

/// `label<padding> | line`, the label colored by group on a terminal.
fn write_line(script: &MultiScript, max_label: usize, colors: bool, stderr: bool, line: &[u8]) {
    let mut buf: Vec<u8> = Vec::with_capacity(line.len() + max_label + 16);
    if colors {
        buf.extend_from_slice(COLORS[script.group % COLORS.len()].as_bytes());
    }
    buf.extend_from_slice(script.label.as_bytes());
    buf.extend(std::iter::repeat_n(
        b' ',
        max_label.saturating_sub(script.label.len()),
    ));
    if colors {
        buf.extend_from_slice(RESET.as_bytes());
    }
    buf.extend_from_slice(b" | ");
    buf.extend_from_slice(line);
    if stderr {
        let _ = std::io::stderr().lock().write_all(&buf);
    } else {
        let _ = std::io::stdout().lock().write_all(&buf);
    }
}
