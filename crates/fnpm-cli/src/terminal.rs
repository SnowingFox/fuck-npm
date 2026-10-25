//! The process surroundings the launcher reads: environment, terminal state and the two
//! output streams. Injected as a [`Terminal`] (like the TS CLI's `CliRuntime`) so whole
//! commands run in tests with captured output.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The environment, read once.
#[derive(Debug, Clone, Default)]
pub struct Env {
    vars: HashMap<String, String>,
}

impl Env {
    /// The process environment. Variables that are not UTF-8 are converted lossily; the
    /// launcher only reads a handful of ASCII names.
    pub fn from_process() -> Self {
        Self {
            vars: std::env::vars_os()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        }
    }

    /// A fixed environment (tests).
    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self {
            vars: pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    /// The value of `key`, if set (an empty value is still set).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }
}

/// A thread-safe text sink: stdout, stderr, or a capture buffer.
pub trait Stream: Send + Sync {
    /// Write `text` in full. Errors (a closed pipe) are ignored, like Node's streams.
    fn write(&self, text: &str);
}

/// The real stdout or stderr. Unbuffered beyond the std lock: every call is one write.
#[derive(Debug, Clone, Copy)]
pub enum StdStream {
    Stdout,
    Stderr,
}

impl Stream for StdStream {
    fn write(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let _ = match self {
            Self::Stdout => {
                let mut out = std::io::stdout().lock();
                out.write_all(text.as_bytes()).and_then(|()| out.flush())
            }
            Self::Stderr => std::io::stderr().lock().write_all(text.as_bytes()),
        };
    }
}

/// Collects everything written (tests).
#[derive(Debug, Default)]
pub struct Capture {
    text: Mutex<String>,
}

impl Capture {
    /// Everything written so far.
    pub fn text(&self) -> String {
        self.text
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

impl Stream for Capture {
    fn write(&self, text: &str) {
        self.text
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_str(text);
    }
}

/// Everything ambient a native command touches.
#[derive(Clone)]
pub struct Terminal {
    pub env: Env,
    /// The starting directory (`--cwd` is resolved against it).
    pub cwd: PathBuf,
    pub stdout: Arc<dyn Stream>,
    pub stderr: Arc<dyn Stream>,
    pub stdout_is_tty: bool,
    pub stderr_is_tty: bool,
    /// Terminal width for the progress line; 80 when unknown.
    pub columns: usize,
    /// When the process started (`main`): bun times an install from there. `None` keeps the
    /// core's own measure (tests).
    pub started: Option<std::time::Instant>,
}

impl Terminal {
    /// The real process: environment, cwd, std streams and their TTY state.
    pub fn from_process() -> Self {
        let stdout_is_tty = std::io::stdout().is_terminal();
        let stderr_is_tty = std::io::stderr().is_terminal();
        Self {
            env: Env::from_process(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            stdout: Arc::new(StdStream::Stdout),
            stderr: Arc::new(StdStream::Stderr),
            stdout_is_tty,
            stderr_is_tty,
            columns: terminal_columns(stdout_is_tty, stderr_is_tty),
            started: Some(std::time::Instant::now()),
        }
    }
}

/// `process.stderr.columns ?? process.stdout.columns ?? 80`: Node only knows a width for
/// a TTY.
#[cfg(unix)]
fn terminal_columns(stdout_is_tty: bool, stderr_is_tty: bool) -> usize {
    use std::os::fd::AsFd;
    let width = |fd: std::os::fd::BorrowedFd<'_>| {
        rustix::termios::tcgetwinsize(fd)
            .ok()
            .map(|size| usize::from(size.ws_col))
            .filter(|cols| *cols > 0)
    };
    let stderr = stderr_is_tty
        .then(|| width(std::io::stderr().as_fd()))
        .flatten();
    let stdout = || {
        stdout_is_tty
            .then(|| width(std::io::stdout().as_fd()))
            .flatten()
    };
    stderr.or_else(stdout).unwrap_or(80)
}

#[cfg(not(unix))]
fn terminal_columns(_stdout_is_tty: bool, _stderr_is_tty: bool) -> usize {
    80
}

/// `colorEnabled` in `src/output/output.ts`: `NO_COLOR` (non-empty) wins, then
/// `FORCE_COLOR` (anything but `0`/`false`), then `TERM=dumb`, then whether the stream is a
/// terminal.
pub fn color_enabled(env: &Env, is_tty: bool) -> bool {
    if env.get("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return false;
    }
    if let Some(force) = env.get("FORCE_COLOR") {
        return force != "0" && force != "false";
    }
    if env.get("TERM") == Some("dumb") {
        return false;
    }
    is_tty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_rules() {
        let env = |pairs: &[(&str, &str)]| Env::from_pairs(pairs.iter().copied());
        assert!(color_enabled(&env(&[]), true));
        assert!(!color_enabled(&env(&[]), false));
        assert!(!color_enabled(
            &env(&[("NO_COLOR", "1"), ("FORCE_COLOR", "1")]),
            true
        ));
        assert!(color_enabled(
            &env(&[("NO_COLOR", ""), ("FORCE_COLOR", "1")]),
            false
        ));
        assert!(!color_enabled(&env(&[("FORCE_COLOR", "0")]), true));
        assert!(!color_enabled(&env(&[("FORCE_COLOR", "false")]), true));
        assert!(color_enabled(&env(&[("FORCE_COLOR", "")]), false));
        assert!(!color_enabled(&env(&[("TERM", "dumb")]), true));
    }
}
