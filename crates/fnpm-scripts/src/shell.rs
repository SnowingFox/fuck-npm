//! Shell selection, argv and argument escaping (bun `run_command.rs` `find_shell`,
//! `shell_parser` `escape_8bit` / `needs_escape_utf8_ascii_latin1`).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The shell a script runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
    /// `-c` (POSIX) or `/d /s /c` (cmd.exe).
    pub flags: Vec<OsString>,
}

impl Shell {
    /// `program flags... script`.
    pub fn argv(&self, script: &str) -> Vec<OsString> {
        let mut argv = Vec::with_capacity(self.flags.len() + 2);
        argv.push(self.program.clone().into_os_string());
        argv.extend(self.flags.iter().cloned());
        argv.push(script.into());
        argv
    }

    fn for_program(program: PathBuf) -> Self {
        let name = program
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let flags = if name == "cmd" {
            vec!["/d".into(), "/s".into(), "/c".into()]
        } else if cfg!(windows) && (name == "powershell" || name == "pwsh") {
            vec!["-Command".into()]
        } else {
            vec!["-c".into()]
        };
        Self { program, flags }
    }
}

/// bun `SHELLS_TO_SEARCH`.
#[cfg(not(windows))]
const SHELLS_TO_SEARCH: [&str; 3] = ["bash", "sh", "zsh"];

/// bun `HARDCODED_POPULAR_ONES`.
#[cfg(not(windows))]
const HARDCODED: [&str; 8] = [
    "/bin/bash",
    "/usr/bin/bash",
    "/usr/local/bin/bash",
    "/bin/sh",
    "/usr/bin/sh",
    "/usr/bin/zsh",
    "/usr/local/bin/zsh",
    "/system/bin/sh",
];

/// The shell for scripts. `choice` is `--shell` / `run.shell`: `None`, `"system"` or `"bun"`
/// select the default (bash, sh or zsh from `path`, then well-known locations, cached for the
/// process; `cmd.exe` on Windows); anything else names a program, searched on `path` unless
/// it is a path.
pub fn select(choice: Option<&str>, path: &OsStr, cwd: &Path) -> Option<Shell> {
    match choice
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != "system" && *c != "bun")
    {
        None => find_default(path, cwd),
        Some(program) => {
            let p = Path::new(program);
            let found = if p.components().count() > 1 || p.is_absolute() {
                Some(cwd.join(p))
            } else {
                which(program, path, cwd)
            };
            found.map(Shell::for_program)
        }
    }
}

fn find_default(path: &OsStr, cwd: &Path) -> Option<Shell> {
    static CACHE: OnceLock<Option<Shell>> = OnceLock::new();
    CACHE
        .get_or_init(|| find_default_uncached(path, cwd))
        .clone()
}

#[cfg(windows)]
fn find_default_uncached(_path: &OsStr, _cwd: &Path) -> Option<Shell> {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    Some(Shell::for_program(
        Path::new(&system_root).join("System32").join("cmd.exe"),
    ))
}

#[cfg(not(windows))]
fn find_default_uncached(path: &OsStr, cwd: &Path) -> Option<Shell> {
    for name in SHELLS_TO_SEARCH {
        if let Some(found) = which(name, path, cwd) {
            return Some(Shell::for_program(found));
        }
    }
    HARDCODED
        .iter()
        .map(PathBuf::from)
        .find(|p| is_executable(p))
        .map(Shell::for_program)
}

/// Finds `name` in the `PATH`-style list `path` (relative entries resolve against `cwd`).
pub fn which(name: &str, path: &OsStr, cwd: &Path) -> Option<PathBuf> {
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let dir = if dir.is_absolute() {
            dir
        } else {
            cwd.join(dir)
        };
        #[cfg(windows)]
        {
            // npm writes an extensionless POSIX shim beside the Windows launcher.
            // Prefer a native executable or batch file when the command has no extension.
            let extensions: &[&str] = if Path::new(name).extension().is_some() {
                &[""]
            } else {
                &[".exe", ".com", ".cmd", ".bat", ""]
            };
            for ext in extensions {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// A regular file with an execute bit (POSIX) or any file (Windows).
pub fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        #[cfg(unix)]
        Ok(m) => {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        Ok(m) => m.is_file(),
        Err(_) => false,
    }
}

/// bun `SPECIAL_CHARS`: an argument with any of these (or an empty one) is quoted.
const SPECIAL_CHARS: &[u8] = b"~[]#;\n\t\r*?{,}`$=()0123456789|><&'\" \\\x08";

/// Whether `arg` must be quoted to stay one shell word (false positives allowed, like bun).
pub fn needs_escape(arg: &str) -> bool {
    arg.is_empty() || arg.bytes().any(|c| SPECIAL_CHARS.contains(&c))
}

/// Appends `arg` as one shell word: as it is, or double-quoted with `$`, `` ` ``, `"` and `\`
/// backslashed (bun `escape_8bit::<true, false>`).
pub fn push_escaped(out: &mut String, arg: &str) {
    if !needs_escape(arg) {
        out.push_str(arg);
        return;
    }
    out.reserve(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if matches!(c, '$' | '`' | '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

/// cmd.exe metacharacters that cannot be passed safely through `cmd /c` (bun
/// `batch_arg_has_cmd_metachars`).
pub fn has_cmd_metachars(arg: &str) -> bool {
    arg.bytes().any(|c| {
        matches!(
            c,
            b'%' | b'^' | b'&' | b'|' | b'<' | b'>' | b'!' | b'"' | b'\n' | b'\r'
        )
    })
}

/// The script with package-manager calls rewritten and `args` appended, each escaped.
pub fn script_with_args(script: &str, args: &[String]) -> String {
    let mut out = crate::rewrite::replace_package_manager_run(script);
    for arg in args {
        out.push(' ');
        push_escaped(&mut out, arg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(s: &str) -> String {
        let mut out = String::new();
        push_escaped(&mut out, s);
        out
    }

    #[test]
    fn escaping() {
        assert_eq!(esc("abc"), "abc");
        assert_eq!(esc("--flag"), "--flag");
        assert_eq!(esc(""), "\"\"");
        assert_eq!(esc("a b"), "\"a b\"");
        assert_eq!(esc("--x=1"), "\"--x=1\"");
        assert_eq!(esc("$HOME"), "\"\\$HOME\"");
        assert_eq!(esc("a\"b"), "\"a\\\"b\"");
        assert_eq!(esc("a\\b"), "\"a\\\\b\"");
        assert_eq!(esc("`x`"), "\"\\`x\\`\"");
        assert_eq!(esc("héllo wörld"), "\"héllo wörld\"");
        assert_eq!(
            script_with_args("echo", &["a b".into(), "c".into()]),
            "echo \"a b\" c"
        );
        assert_eq!(script_with_args("npm run x", &[]), "fnpm run x");
    }

    #[cfg(windows)]
    #[test]
    fn windows_lookup_prefers_launchers_to_posix_shims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().as_os_str();
        let cwd = dir.path();
        std::fs::write(dir.path().join("fixture"), "#!/bin/sh\nexit 1\n").unwrap();
        for ext in ["bat", "cmd", "com", "exe"] {
            let launcher = dir.path().join(format!("fixture.{ext}"));
            std::fs::write(&launcher, "fixture").unwrap();
            assert_eq!(which("fixture", path, cwd), Some(launcher.clone()));
            assert_eq!(which(&format!("fixture.{ext}"), path, cwd), Some(launcher));
        }
    }

    #[cfg(unix)]
    #[test]
    fn shell_lookup() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let sh = select(None, &path, Path::new("/")).expect("a shell");
        assert_eq!(sh.flags, vec![OsString::from("-c")]);
        assert!(is_executable(&sh.program));
        let explicit = select(Some("sh"), &path, Path::new("/")).expect("sh");
        assert_eq!(explicit.program.file_name().unwrap(), "sh");
        assert_eq!(explicit.argv("echo")[1], "-c");
        assert!(select(Some("definitely-not-a-shell-fnpm"), &path, Path::new("/")).is_none());
        assert!(which("sh", OsStr::new(""), Path::new("/")).is_none());
        assert!(has_cmd_metachars("a&b") && !has_cmd_metachars("ab"));
    }
}
