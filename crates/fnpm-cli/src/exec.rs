//! Handing an invocation to the TS CLI: `node <pkg>/dist/<bin>.js <argv…>`.
//!
//! On Unix the launcher `exec`s, so node takes over the pid, the terminal, signals and the
//! exit status. On Windows it spawns node with inherited stdio, ignores Ctrl-C itself so the
//! child handles it, and exits with the child's code.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::terminal::Env;

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

/// The command name used in the "no node" message: the first non-flag argument.
fn command_label(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .find(|arg| !arg.starts_with('-'))
        .or_else(|| args.first().map(|arg| arg.to_string_lossy()))
        .map_or_else(|| "help".to_owned(), |arg| arg.into_owned())
}

/// The message printed when node cannot be started.
pub fn missing_node_message(bin: &str, args: &[OsString]) -> String {
    format!(
        "{bin}: '{}' needs Node.js 20.19 or newer, and no 'node' was found on PATH\n",
        command_label(args)
    )
}

/// Run the TS CLI for `bin` with `args` (argv after the program name, unchanged). Returns
/// the exit status to use; on Unix it only returns when the exec failed.
pub fn run_node(bin: &str, args: &[OsString], env: &Env) -> i32 {
    let exe = std::env::current_exe().ok();
    let entry = match js_entry(bin, env, exe.as_deref()) {
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
    launch(bin, args, command)
}

#[cfg(unix)]
fn launch(bin: &str, args: &[OsString], mut command: Command) -> i32 {
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    if error.kind() == std::io::ErrorKind::NotFound {
        eprint!("{}", missing_node_message(bin, args));
    } else {
        eprintln!("{bin}: failed to start node: {error}");
    }
    1
}

#[cfg(windows)]
fn launch(bin: &str, args: &[OsString], mut command: Command) -> i32 {
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
            eprint!("{}", missing_node_message(bin, args));
            1
        }
        Err(error) => {
            eprintln!("{bin}: failed to start node: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let args: Vec<OsString> = ["--cwd", "x", "outdated"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(
            missing_node_message("fnpm", &args),
            "fnpm: 'x' needs Node.js 20.19 or newer, and no 'node' was found on PATH\n"
        );
        assert!(missing_node_message("fnpm", &[]).contains("'help'"));
    }
}
