//! The machinery of `fnpm run` / `exec` / `x` and of the version/pack/publish hooks (bun
//! `run_command.rs`: `exec_with_cfg`, `run_package_script_foreground`,
//! `run_lifecycle_script_foreground`, `run_binary`, `path_for_run`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::env::{BaseEnv, EnvMap, PackageVars};
use crate::{Error, Result, RunOptions};

/// The nearest package.json at or above `cwd`, parsed.
pub fn find_package_json(cwd: &Path) -> Result<Option<(PathBuf, serde_json::Value)>> {
    for dir in cwd.ancestors() {
        let path = dir.join("package.json");
        match crate::json::read_package_json(&path) {
            Ok(json) => return Ok(Some((path, json))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::Io { path, source }),
        }
    }
    Ok(None)
}

/// The `scripts` of a package.json in file order (string values only).
pub fn scripts_of(json: &serde_json::Value) -> Vec<(String, String)> {
    json.get("scripts")
        .and_then(|s| s.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// What `fnpm run <name>` ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// A package.json script (with its `pre`/`post` hooks); `code` is the first non-zero exit
    /// code of the chain, or 0.
    Script { code: i32 },
    /// A binary from `node_modules/.bin` or PATH, run with the arguments directly.
    Binary { path: PathBuf, code: i32 },
    /// Nothing by that name. With `if_present` the caller exits 0 silently; otherwise it
    /// prints `error: Script not found "<name>"` and exits 1.
    NotFound,
}

impl RunOutcome {
    /// The process exit code: `if_present` turns [`RunOutcome::NotFound`] into 0.
    pub fn exit_code(&self, if_present: bool) -> i32 {
        match self {
            Self::Script { code } | Self::Binary { code, .. } => *code,
            Self::NotFound => i32::from(!if_present),
        }
    }
}

/// One `fnpm run` request.
#[derive(Debug, Clone)]
pub struct RunRequest<'a> {
    /// The process cwd: the package.json lookup starts here and binaries run here.
    pub cwd: &'a Path,
    pub name: &'a str,
    /// Passed to the main script (shell-escaped) or the binary, never to `pre`/`post`.
    pub args: &'a [String],
    /// `npm_command`: `run-script` for `run`, `exec` for `exec` / `x`.
    pub npm_command: &'a str,
}

/// bun `exec_with_cfg` for a script name: the nearest package.json's `scripts[name]` with
/// `pre<name>` and `post<name>` (a non-zero exit stops the chain), run in that package.json's
/// directory; else a binary found in the `node_modules/.bin` chain or PATH, run in `cwd`.
/// Messages for non-zero exits are printed unless `opts.silent` (see [`exit_message`]).
/// A signal ends the chain with [`Error::Signaled`]; the caller re-raises it.
pub async fn run_script(req: &RunRequest<'_>, opts: &RunOptions) -> Result<RunOutcome> {
    let found = find_package_json(req.cwd)?;
    let base = BaseEnv::run(
        &opts.project_dir,
        std::env::vars_os(),
        &opts.env,
        req.npm_command,
    );
    let original_path = base.original_path().to_owned();
    let mut env = base.env().clone();
    let mut package_name = String::new();
    if let Some((path, json)) = &found {
        let (name, version, config) = crate::env::package_json_vars(json);
        package_name = name.clone().unwrap_or_default();
        PackageVars {
            dir: path.parent().unwrap_or(req.cwd),
            package_json: path,
            name: name.as_deref().unwrap_or_default(),
            version: version.as_deref().unwrap_or_default(),
            config: &config,
        }
        .apply(&mut env);
    }
    let path_dir = found
        .as_ref()
        .and_then(|(p, _)| p.parent())
        .unwrap_or(req.cwd);
    env.set("PATH", crate::env::path_for_run(path_dir, &original_path));

    if let Some((path, json)) = &found {
        let scripts = scripts_of(json);
        let get = |n: &str| scripts.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        if let Some(main) = get(req.name) {
            let dir = path.parent().unwrap_or(req.cwd);
            let pre = format!("pre{}", req.name);
            let post = format!("post{}", req.name);
            let chain = [
                (pre.as_str(), get(&pre), false),
                (req.name, Some(main), true),
                (post.as_str(), get(&post), false),
            ];
            for (event, script, with_args) in chain {
                let Some(script) = script else { continue };
                let args: &[String] = if with_args { req.args } else { &[] };
                env.set_lifecycle(event, &script);
                let code = exec_shell(
                    dir,
                    &package_name,
                    event,
                    &script,
                    args,
                    &env,
                    &original_path,
                    opts,
                )
                .await?;
                if code != 0 {
                    if !opts.silent && code != 2 {
                        eprintln!("{}", exit_message(event, code));
                    }
                    return Ok(RunOutcome::Script { code });
                }
            }
            return Ok(RunOutcome::Script { code: 0 });
        }
    }

    let search = env.get("PATH").unwrap_or_default().to_owned();
    if let Some(bin) = crate::shell::which(req.name, &search, req.cwd) {
        let code = exec_binary(&bin, req.args, req.cwd, &env, opts.silent).await?;
        if code != 0 && !opts.silent {
            eprintln!("{}", binary_exit_message(&bin, req.name, code));
        }
        return Ok(RunOutcome::Binary { path: bin, code });
    }
    Ok(RunOutcome::NotFound)
}

/// bun's message for a script that exited non-zero: `error: script "<name>" exited with code
/// <code>` (not printed for code 2, like bun).
pub fn exit_message(name: &str, code: i32) -> String {
    format!("error: script \"{name}\" exited with code {code}")
}

/// bun's message for a script killed by a signal (not printed for SIGINT).
pub fn signal_message(name: &str, signal: i32) -> String {
    format!(
        "error: script \"{name}\" was terminated by signal {}",
        crate::signal_name(signal)
    )
}

/// bun `run_binary_without_bunx_path` messages. `test` exiting 1 and `install`/`kill`/`link`
/// exiting 2 most likely mean a missing package.json script.
pub fn binary_exit_message(bin: &Path, requested: &str, code: i32) -> String {
    let base = bin.file_name().map_or_else(
        || bin.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    if (code == 1 && requested == "test")
        || (code == 2 && matches!(requested, "install" | "kill" | "link"))
    {
        format!(
            "error: \"{}\" exited with code {code}\nnote: a package.json script \"{requested}\" was not found",
            bin.display()
        )
    } else if code > 0 && code != 130 {
        format!("error: \"{base}\" exited with code {code}")
    } else {
        format!("error: Failed to run \"{base}\" due to exit code {code}")
    }
}

/// `run_lifecycle_script_foreground`: runs hook `name` (`preversion`, `prepack`, ...) of the
/// package in `dir` with the `.bin` chain of `dir` in front of PATH and inherited stdio.
/// Returns the exit code.
pub async fn run_lifecycle_script_foreground(
    dir: &Path,
    name: &str,
    script: &str,
    opts: &RunOptions,
) -> Result<i32> {
    run_shell_command(dir, name, script, &[], opts, "run-script").await
}

/// Runs `command` + escaped `args` through the shell in `cwd` with the `fnpm run`
/// environment (`npm_package_*` of the nearest package.json, `.bin` chain on PATH).
pub(crate) async fn run_shell_command(
    cwd: &Path,
    event: &str,
    command: &str,
    args: &[String],
    opts: &RunOptions,
    npm_command: &str,
) -> Result<i32> {
    let base = BaseEnv::run(
        &opts.project_dir,
        std::env::vars_os(),
        &opts.env,
        npm_command,
    );
    let original_path = base.original_path().to_owned();
    let mut env = base.env().clone();
    let mut package_name = String::new();
    if let Some((path, json)) = find_package_json(cwd)? {
        let (name, version, config) = crate::env::package_json_vars(&json);
        package_name = name.clone().unwrap_or_default();
        PackageVars {
            dir: path.parent().unwrap_or(cwd),
            package_json: &path,
            name: name.as_deref().unwrap_or_default(),
            version: version.as_deref().unwrap_or_default(),
            config: &config,
        }
        .apply(&mut env);
    }
    env.set("PATH", crate::env::path_for_run(cwd, &original_path));
    env.set_lifecycle(event, command);
    exec_shell(
        cwd,
        &package_name,
        event,
        command,
        args,
        &env,
        &original_path,
        opts,
    )
    .await
}

/// bun `run_package_script_foreground_with_shell_path`: the shell is looked up on the
/// original PATH, the command is echoed as `$ <command>` unless silent.
#[allow(clippy::too_many_arguments)]
async fn exec_shell(
    cwd: &Path,
    package: &str,
    event: &str,
    script: &str,
    args: &[String],
    env: &EnvMap,
    shell_path: &std::ffi::OsStr,
    opts: &RunOptions,
) -> Result<i32> {
    let shell = crate::shell::select(opts.shell.as_deref(), shell_path, cwd).ok_or_else(|| {
        Error::Spawn {
            shell: opts.shell.clone().unwrap_or_else(|| "sh".into()),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no shell found on PATH"),
        }
    })?;
    if cfg!(windows)
        && shell.flags.iter().any(|f| f == "/c")
        && let Some(bad) = args.iter().find(|a| crate::shell::has_cmd_metachars(a))
    {
        return Err(Error::Message(format!(
            "Failed to run script {event}: argument \"{bad}\" contains a cmd.exe special character and cannot be passed to the system shell"
        )));
    }
    let command = crate::shell::script_with_args(script, args);
    if !opts.silent {
        crate::runner::print_command(&command);
    }
    let mut cmd = crate::runner::shell_command(&shell, &command);
    cmd.current_dir(cwd).env_clear().envs(env.iter());
    let status = spawn_foreground(cmd, env, &shell.program.display().to_string()).await?;
    finish(status, package, event, opts.silent)
}

async fn exec_binary(
    bin: &Path,
    args: &[String],
    cwd: &Path,
    env: &EnvMap,
    _silent: bool,
) -> Result<i32> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args).current_dir(cwd).env_clear().envs(env.iter());
    let status = spawn_foreground(cmd, env, &bin.display().to_string()).await?;
    let name = bin
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    finish(status, &name, &name, true)
}

/// Spawns with inherited stdio. `NODE_CHANNEL_FD` stays open in the child for node-style
/// IPC; with `FNPM_FEATURE_FLAG_NO_ORPHANS=1` / `BUN_FEATURE_FLAG_NO_ORPHANS=1` the child gets
/// a parent-death signal (Linux).
async fn spawn_foreground(
    mut cmd: tokio::process::Command,
    env: &EnvMap,
    program: &str,
) -> Result<std::process::ExitStatus> {
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    crate::runner::keep_stdio(&mut cmd);
    let no_orphans = [
        "FNPM_FEATURE_FLAG_NO_ORPHANS",
        "BUN_FEATURE_FLAG_NO_ORPHANS",
    ]
    .iter()
    .any(|k| env.get(k).is_some_and(|v| v == "1"));
    if no_orphans {
        crate::runner::set_parent_death_signal(&mut cmd);
    }
    keep_ipc_fd(&mut cmd, env);
    let mut child = cmd.spawn().map_err(|source| Error::Spawn {
        shell: program.to_owned(),
        source,
    })?;
    child.wait().await.map_err(|source| Error::Spawn {
        shell: program.to_owned(),
        source,
    })
}

/// Clears `FD_CLOEXEC` on `NODE_CHANNEL_FD` in the child so node-style IPC reaches it.
fn keep_ipc_fd(cmd: &mut tokio::process::Command, env: &EnvMap) {
    #[cfg(unix)]
    {
        let fd = env
            .get("NODE_CHANNEL_FD")
            .and_then(|v| v.to_str())
            .and_then(|v| v.trim().parse::<i32>().ok());
        if let Some(fd) = fd.filter(|fd| *fd > 2) {
            // SAFETY: runs in the forked child before exec; `fcntl` is async-signal-safe and
            // `fd` is only borrowed for the call.
            unsafe {
                cmd.pre_exec(move || {
                    let borrowed = std::os::fd::BorrowedFd::borrow_raw(fd);
                    let _ = rustix::io::fcntl_setfd(borrowed, rustix::io::FdFlags::empty());
                    Ok(())
                });
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (cmd, env);
}

fn finish(
    status: std::process::ExitStatus,
    package: &str,
    event: &str,
    silent: bool,
) -> Result<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            // SIGINT (Ctrl+C) is silent.
            if !silent && signal != 2 {
                eprintln!("{}", signal_message(event, signal));
            }
            return Err(Error::Signaled {
                package: package.to_owned(),
                script: event.to_owned(),
                signal,
            });
        }
    }
    let _ = (package, event, silent);
    Ok(status.code().unwrap_or(1))
}
