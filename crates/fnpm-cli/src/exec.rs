//! Handing an invocation to the TS CLI: `node <pkg>/dist/<bin>.js <argv…>`.
//!
//! On Unix the launcher `exec`s, so node takes over the pid, the terminal, signals and the
//! exit status. On Windows it spawns node with inherited stdio, ignores Ctrl-C itself so the
//! child handles it, and exits with the child's code.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use fnpm_core::x::{Display, Done};
use fnpm_types::EventSink;

use crate::color::{Colors, to_fixed};
use crate::plan::{consumes_next, is_flag_token};
use crate::progress::{ProgressSink, format_bytes, wants_progress};
use crate::spec::{BinDef, VERSION};
use crate::terminal::{Env, Stream, Terminal, color_enabled};

/// Where the JS entry of `bin` is: `FNPM_JS_ENTRY` (and, for `fnpm-lock`,
/// `FNPM_LOCK_JS_ENTRY` or `fnpm-lock.js` next to `FNPM_JS_ENTRY`), else `../dist/<bin>.js`
/// relative to the real path of the executable (the postinstall layout), else the
/// repository layout of a cargo build (`target/<profile>/` -> `packages/fuck-npm/dist/`).
pub fn js_entry(bin: &str, env: &Env, exe: Option<&Path>) -> Result<PathBuf, String> {
    let file = format!("{bin}.js");
    if bin == "fnpm-lock"
        && let Some(entry) = env
            .get("FNPM_LOCK_JS_ENTRY")
            .filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(entry));
    }
    if let Some(entry) = env.get("FNPM_JS_ENTRY").filter(|value| !value.is_empty()) {
        let entry = PathBuf::from(entry);
        if bin == "fnpm" {
            return Ok(entry);
        }
        return Ok(entry
            .parent()
            .map_or_else(|| PathBuf::from(&file), |dir| dir.join(&file)));
    }
    let real = exe.map(|exe| std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf()));
    let Some(dir) = real.as_deref().and_then(Path::parent) else {
        return Err(format!(
            "{bin}: cannot locate the executable to find dist/{file}; set FNPM_JS_ENTRY"
        ));
    };
    let candidates = [
        dir.join("..").join("dist").join(&file),
        dir.join("..")
            .join("..")
            .join("packages")
            .join("fuck-npm")
            .join("dist")
            .join(&file),
    ];
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    Err(format!(
        "{bin}: cannot find the fnpm JavaScript CLI (looked for {}); set FNPM_JS_ENTRY to the path of dist/fnpm.js",
        candidates[0].display()
    ))
}

/// The node executable: `FNPM_NODE`, else `node` on `PATH`.
pub fn node_program(env: &Env) -> OsString {
    env.get("FNPM_NODE")
        .filter(|value| !value.is_empty())
        .map_or_else(|| OsString::from("node"), OsString::from)
}

/// The command named in the "no node" message: the first argument that is neither a flag
/// nor a flag's value (`fnpm --cwd app outdated` is `outdated`, as the root is walked in
/// `plan::route`); the first argument when there is none, `help` for no arguments.
fn command_label(bin: &BinDef, args: &[OsString]) -> String {
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut index = 0;
    while index < args.len() && args[index] != "--" {
        let token = args[index].as_str();
        if !is_flag_token(token) {
            return token.to_owned();
        }
        if consumes_next(token, bin.root_long, bin.root_short) {
            index += 1;
        }
        index += 1;
    }
    args.first().cloned().unwrap_or_else(|| "help".to_owned())
}

/// The message printed when node cannot be started.
pub fn missing_node_message(bin: &BinDef, args: &[OsString], env: &Env) -> String {
    let wanted = format!(
        "{}: '{}' needs Node.js 20.19 or newer",
        bin.name,
        command_label(bin, args)
    );
    match env.get("FNPM_NODE").filter(|value| !value.is_empty()) {
        Some(node) => format!("{wanted}, and FNPM_NODE ({node}) was not found\n"),
        None => format!("{wanted}, and no 'node' was found on PATH\n"),
    }
}

/// Run the TS CLI for `bin` with `args` (argv after the program name, unchanged). Returns
/// the exit status to use; on Unix it only returns when the exec failed.
pub fn run_node(bin: &BinDef, args: &[OsString], env: &Env) -> i32 {
    let exe = std::env::current_exe().ok();
    let entry = match js_entry(bin.name, env, exe.as_deref()) {
        Ok(entry) => entry,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let mut command = Command::new(node_program(env));
    command.arg(&entry).args(args);
    if let Some(exe) = &exe {
        command.env("FNPM_LAUNCHER", exe);
    }
    launch(bin, args, env, command)
}

#[cfg(unix)]
fn launch(bin: &BinDef, args: &[OsString], env: &Env, mut command: Command) -> i32 {
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    if error.kind() == std::io::ErrorKind::NotFound {
        eprint!("{}", missing_node_message(bin, args, env));
    } else {
        eprintln!("{}: failed to start node: {error}", bin.name);
    }
    1
}

#[cfg(windows)]
fn launch(bin: &BinDef, args: &[OsString], env: &Env, mut command: Command) -> i32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    // SAFETY: a null handler with add = TRUE makes this process ignore Ctrl-C; the child
    // (in the same console) still receives it.
    unsafe {
        SetConsoleCtrlHandler(None, 1);
    }
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprint!("{}", missing_node_message(bin, args, env));
            1
        }
        Err(error) => {
            eprintln!("{}: failed to start node: {error}", bin.name);
            1
        }
    }
}

/// The install of `fnpm x` on a terminal: a `fnpm x <package>` header, the live progress
/// region of `fnpm install`, and one line saying what was installed before the bin's output.
pub struct XDisplay {
    pc: Colors,
    columns: usize,
    stderr: Arc<dyn Stream>,
    live: Mutex<Option<Arc<ProgressSink>>>,
}

impl XDisplay {
    /// Draws on `stderr`.
    pub fn new(pc: Colors, columns: usize, stderr: Arc<dyn Stream>) -> Self {
        Self {
            pc,
            columns,
            stderr,
            live: Mutex::new(None),
        }
    }

    /// A view for this terminal, when it shows progress (a TTY stderr, not CI or
    /// `FNPM_PROGRESS=0`).
    pub fn for_terminal(terminal: &Terminal) -> Option<Self> {
        wants_progress(&terminal.env, terminal.stderr_is_tty, false, false, false).then(|| {
            Self::new(
                Colors::new(color_enabled(&terminal.env, terminal.stderr_is_tty)),
                terminal.columns,
                Arc::clone(&terminal.stderr),
            )
        })
    }

    fn take_live(&self) -> Option<Arc<ProgressSink>> {
        self.live
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }

    /// The line for `done`.
    pub fn done_line(&self, done: &Done) -> String {
        let pc = self.pc;
        match done {
            Done::Installed {
                package,
                packages,
                bytes,
                elapsed_ms,
            } => {
                let mut details = Vec::new();
                if *packages > 0 {
                    details.push(format!(
                        "{packages} package{}",
                        if *packages == 1 { "" } else { "s" }
                    ));
                }
                if *bytes > 0 {
                    details.push(format!("{} downloaded", format_bytes(*bytes)));
                }
                details.push(duration(*elapsed_ms));
                format!(
                    "{} {} {} {}\n",
                    pc.green("✓"),
                    pc.bold(package),
                    if *packages > 0 {
                        "installed"
                    } else {
                        "up to date"
                    },
                    pc.dim(&format!("· {}", details.join(" · ")))
                )
            }
            Done::Cached { package } => {
                format!(
                    "{} {}\n",
                    pc.green("✓"),
                    pc.dim(&format!("{package} (cached)"))
                )
            }
        }
    }
}

/// `840ms`, `1.24s`.
fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{}s", to_fixed(ms as f64 / 1000.0, 2))
    }
}

impl Display for XDisplay {
    fn start(&self, title: &str) -> Arc<dyn EventSink> {
        if let Some(previous) = self.take_live() {
            previous.stop();
        }
        self.stderr.write(&format!(
            "{} {}\n",
            self.pc.bold(&format!("fnpm {title}")),
            self.pc.dim(&format!("v{}", crate::spec::version_with_sha()))
        ));
        let live = ProgressSink::start(self.pc, self.columns, Arc::clone(&self.stderr));
        *self
            .live
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(Arc::clone(&live));
        live
    }

    fn stop(&self, done: Option<Done>) {
        if let Some(live) = self.take_live() {
            live.stop();
        }
        if let Some(done) = done {
            self.stderr.write(&self.done_line(&done));
        }
    }
}

/// `fnpm x` / `fnpm create` / `fnpmx`: resolve (and install) the bin with `fnpm_core::x`, then
/// replace this process with it (Unix) or run it and exit with its status.
pub fn run_x(args: Vec<String>, create: bool) -> i32 {
    let terminal = Terminal::from_process();
    let request = fnpm_core::x::Request {
        args,
        cwd: terminal.cwd.clone(),
        create,
        version: VERSION.to_owned(),
        revision: crate::spec::REVISION.to_owned(),
        display: XDisplay::for_terminal(&terminal).map(|view| Arc::new(view) as Arc<dyn Display>),
    };
    match fnpm_core::x::plan(&request) {
        fnpm_core::x::Outcome::Exit(code) => code,
        fnpm_core::x::Outcome::Run(run) => exec_bin(&run),
    }
}

#[cfg(unix)]
fn exec_bin(run: &fnpm_core::x::Exec) -> i32 {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let _ = std::io::stdout().flush();
    let error = Command::new(&run.program)
        .args(&run.args)
        .current_dir(&run.cwd)
        .env_clear()
        .envs(run.env.iter().map(|(k, v)| (k, v)))
        .exec();
    eprintln!("error: failed to run {}: {error}", run.program.display());
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        126
    } else {
        1
    }
}

#[cfg(not(unix))]
fn exec_bin(run: &fnpm_core::x::Exec) -> i32 {
    fnpm_core::x::spawn(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Capture;
    use fnpm_types::Event;

    #[test]
    fn x_display_draws_and_reports() {
        let capture = Arc::new(Capture::default());
        let view = XDisplay::new(
            Colors::new(false),
            80,
            Arc::clone(&capture) as Arc<dyn Stream>,
        );
        let sink = view.start("x create-vite");
        sink.emit(Event::Resolve {
            resolved: 1,
            total: 3,
            name: Some("create-vite".into()),
        });
        view.stop(Some(Done::Installed {
            package: "create-vite@6.5.0".into(),
            packages: 3,
            bytes: 2048,
            elapsed_ms: 1240,
        }));
        let text = capture.text();
        assert!(
            text.starts_with(&format!("fnpm x create-vite v{}\n", crate::spec::version_with_sha())),
            "{text}"
        );
        assert!(text.contains("Resolving 1/3"), "{text}");
        assert!(
            text.ends_with(
                "✓ create-vite@6.5.0 installed · 3 packages · 2.0 KiB downloaded · 1.24s\n"
            ),
            "{text}"
        );
        // The progress region is erased before the result line.
        let tail = &text[text.rfind("\x1b[2K").unwrap()..];
        assert!(tail.contains("✓"), "{text}");
    }

    #[test]
    fn x_display_lines() {
        let view = XDisplay::new(Colors::new(false), 80, Arc::new(Capture::default()));
        assert_eq!(
            view.done_line(&Done::Installed {
                package: "cowsay@1.6.0".into(),
                packages: 0,
                bytes: 0,
                elapsed_ms: 310,
            }),
            "✓ cowsay@1.6.0 up to date · 310ms\n"
        );
        assert_eq!(
            view.done_line(&Done::Installed {
                package: "cowsay@1.6.0".into(),
                packages: 1,
                bytes: 0,
                elapsed_ms: 5,
            }),
            "✓ cowsay@1.6.0 installed · 1 package · 5ms\n"
        );
        assert_eq!(
            view.done_line(&Done::Cached {
                package: "cowsay@1.6.0".into()
            }),
            "✓ cowsay@1.6.0 (cached)\n"
        );
        // Stopping a view that never started prints only the result.
        let capture = Arc::new(Capture::default());
        let view = XDisplay::new(
            Colors::new(false),
            80,
            Arc::clone(&capture) as Arc<dyn Stream>,
        );
        view.stop(Some(Done::Cached {
            package: "a@1.0.0".into(),
        }));
        view.stop(None);
        assert_eq!(capture.text(), "✓ a@1.0.0 (cached)\n");
    }

    #[test]
    fn entry_overrides() {
        let env = Env::from_pairs([("FNPM_JS_ENTRY", "/pkg/dist/fnpm.js")]);
        assert_eq!(
            js_entry("fnpm", &env, None).unwrap(),
            PathBuf::from("/pkg/dist/fnpm.js")
        );
        assert_eq!(
            js_entry("fnpm-lock", &env, None).unwrap(),
            PathBuf::from("/pkg/dist/fnpm-lock.js")
        );
        let env = Env::from_pairs([
            ("FNPM_JS_ENTRY", "/a/fnpm.js"),
            ("FNPM_LOCK_JS_ENTRY", "/b/lock.js"),
        ]);
        assert_eq!(
            js_entry("fnpm-lock", &env, None).unwrap(),
            PathBuf::from("/b/lock.js")
        );
    }

    #[test]
    fn entry_next_to_the_binary() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        std::fs::create_dir_all(root.path().join("dist")).unwrap();
        std::fs::write(root.path().join("dist/fnpm.js"), "").unwrap();
        let exe = root.path().join("bin/fnpm");
        std::fs::write(&exe, "").unwrap();
        let found = js_entry("fnpm", &Env::default(), Some(&exe)).unwrap();
        assert!(found.ends_with("dist/fnpm.js"), "{}", found.display());
        assert!(js_entry("fnpm-lock", &Env::default(), Some(&exe)).is_err());
    }

    #[test]
    fn node_override_and_message() {
        assert_eq!(node_program(&Env::default()), OsString::from("node"));
        assert_eq!(
            node_program(&Env::from_pairs([("FNPM_NODE", "/opt/node")])),
            OsString::from("/opt/node")
        );
        let os = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();
        let none = Env::default();
        assert_eq!(
            missing_node_message(&crate::spec::FNPM, &os(&["--cwd", "x", "outdated"]), &none),
            "fnpm: 'outdated' needs Node.js 20.19 or newer, and no 'node' was found on PATH\n"
        );
        assert_eq!(
            missing_node_message(&crate::spec::FNPM, &os(&["--json", "why", "react"]), &none),
            "fnpm: 'why' needs Node.js 20.19 or newer, and no 'node' was found on PATH\n"
        );
        assert!(
            missing_node_message(&crate::spec::FNPM, &os(&["--help"]), &none).contains("'--help'")
        );
        assert!(missing_node_message(&crate::spec::FNPM, &[], &none).contains("'help'"));
        assert_eq!(
            missing_node_message(
                &crate::spec::FNPM_LOCK,
                &os(&["migrate"]),
                &Env::from_pairs([("FNPM_NODE", "/opt/node")])
            ),
            "fnpm-lock: 'migrate' needs Node.js 20.19 or newer, and FNPM_NODE (/opt/node) was not found\n"
        );
    }
}
