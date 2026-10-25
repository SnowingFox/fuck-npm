//! Output of the commands that print as they go (`pack`, `publish`, `patch`). bun prints
//! their lines between the output of the lifecycle scripts they run, so these write straight
//! to the process' stdout and stderr (flushed per line) instead of returning `lines`.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write};

/// A failure that ends the command: `text` is printed on stderr as it is, exit code 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fatal(pub String);

pub(crate) type FResult<T> = Result<T, Fatal>;

fn colors_for(is_tty: bool) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if let Some(force) = std::env::var_os("FORCE_COLOR") {
        return force != "0" && force != "false";
    }
    is_tty
}

/// stdout / stderr with bun's colours.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Out {
    pub color_out: bool,
    pub color_err: bool,
}

impl Out {
    pub fn new() -> Self {
        Self {
            color_out: colors_for(std::io::stdout().is_terminal()),
            color_err: colors_for(std::io::stderr().is_terminal()),
        }
    }

    /// `text` on stdout, flushed.
    pub fn print(&self, text: &str) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    pub fn println(&self, text: &str) {
        self.print(&format!("{text}\n"));
    }

    /// `text` on stderr, flushed.
    pub fn eprint(&self, text: &str) {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(text.as_bytes());
        let _ = err.flush();
    }

    pub fn eprintln(&self, text: &str) {
        self.eprint(&format!("{text}\n"));
    }

    /// `text` in the SGR `codes` on stdout (`"1;36"` is bold cyan).
    pub fn paint(&self, codes: &str, text: &str) -> String {
        paint(self.color_out, codes, text)
    }

    pub fn paint_err(&self, codes: &str, text: &str) -> String {
        paint(self.color_err, codes, text)
    }

    /// bun `Output::err_generic`: `error: <message>`.
    pub fn error(&self, message: &str) -> Fatal {
        let message = escape_control(message);
        Fatal(format!(
            "{}{} {message}",
            self.paint_err("31", "error"),
            self.paint_err("2", ":")
        ))
    }

    /// bun `Output::err` with an error name: `<NAME>: <message>`.
    pub fn named(&self, name: &str, message: &str) -> Fatal {
        let message = escape_control(message);
        Fatal(format!(
            "{}{} {message}",
            self.paint_err("31", name),
            self.paint_err("2", ":")
        ))
    }

    /// bun `Output::err` with a system error: `ENOENT: No such file or directory: <message>
    /// (open)`.
    pub fn sys(&self, err: &std::io::Error, syscall: &str, message: &str) -> Fatal {
        let message = escape_control(message);
        let (name, label) = errno_text(err);
        let mut text = format!(
            "{}{} ",
            self.paint_err("31", &name),
            self.paint_err("2", ":")
        );
        if let Some(label) = label {
            let _ = write!(text, "{label}: ");
        }
        let _ = write!(
            text,
            "{message} {}",
            self.paint_err("2", &format!("({syscall})"))
        );
        Fatal(text)
    }
}

pub(crate) fn paint(color: bool, codes: &str, text: &str) -> String {
    if color {
        format!("\x1b[{codes}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// bun `fmt::size` without the space: `0KB`, `31B`, `1.50KB`, `2.1MB`.
pub(crate) fn fmt_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0KB".to_owned();
    }
    if bytes < 512 {
        return format!("{bytes}B");
    }
    const MAGS: &[u8] = b" KMGTPEZY";
    let log2 = 63 - bytes.leading_zeros() as usize;
    let magnitude = (log2 / 9).min(MAGS.len() - 1);
    let value = bytes as f64 / 1000f64.powi(magnitude as i32);
    if magnitude == 0 {
        return format!("{:.2}KB", value / 1000.0);
    }
    let precision = if (value - value.trunc()).abs() <= 0.100 {
        1
    } else {
        2
    };
    format!("{value:.precision$}{}B", MAGS[magnitude] as char)
}

/// bun `fmt::EscapeControlChars`: C0 and C1 controls, DEL and the bidi controls as escapes,
/// so a file name cannot move the cursor or reorder the line.
pub(crate) fn escape_control(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0'..='\x1f' | '\x7f' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            '\u{80}'..='\u{9f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// A path for display: invalid UTF-8 is shown lossily.
pub(crate) fn show(bytes: &[u8]) -> String {
    escape_control(&String::from_utf8_lossy(bytes))
}

/// The errno name (`ENOENT`) and bun's description of it.
pub(crate) fn errno_text(err: &std::io::Error) -> (String, Option<&'static str>) {
    let Some(code) = err.raw_os_error() else {
        return match err.kind() {
            std::io::ErrorKind::NotFound => ("ENOENT".into(), Some("No such file or directory")),
            std::io::ErrorKind::PermissionDenied => ("EACCES".into(), Some("Permission denied")),
            kind => (format!("{kind:?}"), None),
        };
    };
    match errno(code) {
        Some((name, label)) => (name.to_owned(), Some(label)),
        None => (format!("E{code}"), None),
    }
}

/// Just the errno name.
pub(crate) fn errno_name(err: &std::io::Error) -> String {
    errno_text(err).0
}

#[cfg(unix)]
fn errno(code: i32) -> Option<(&'static str, &'static str)> {
    let table: &[(i32, &str, &str)] = &[
        (libc::EPERM, "EPERM", "Operation not permitted"),
        (libc::ENOENT, "ENOENT", "No such file or directory"),
        (libc::EIO, "EIO", "Input/output error"),
        (libc::ENXIO, "ENXIO", "No such device or address"),
        (libc::EBADF, "EBADF", "Bad file descriptor"),
        (libc::EAGAIN, "EAGAIN", "Resource temporarily unavailable"),
        (libc::ENOMEM, "ENOMEM", "Cannot allocate memory"),
        (libc::EACCES, "EACCES", "Permission denied"),
        (libc::EBUSY, "EBUSY", "Device or resource busy"),
        (libc::EEXIST, "EEXIST", "File exists"),
        (libc::EXDEV, "EXDEV", "Invalid cross-device link"),
        (libc::ENODEV, "ENODEV", "No such device"),
        (libc::ENOTDIR, "ENOTDIR", "Not a directory"),
        (libc::EISDIR, "EISDIR", "Is a directory"),
        (libc::EINVAL, "EINVAL", "Invalid argument"),
        (libc::ENFILE, "ENFILE", "Too many open files in system"),
        (libc::EMFILE, "EMFILE", "Too many open files"),
        (libc::ETXTBSY, "ETXTBSY", "Text file busy"),
        (libc::EFBIG, "EFBIG", "File too large"),
        (libc::ENOSPC, "ENOSPC", "No space left on device"),
        (libc::ESPIPE, "ESPIPE", "Illegal seek"),
        (libc::EROFS, "EROFS", "Read-only file system"),
        (libc::EMLINK, "EMLINK", "Too many links"),
        (libc::EPIPE, "EPIPE", "Broken pipe"),
        (libc::ENAMETOOLONG, "ENAMETOOLONG", "File name too long"),
        (libc::ENOTEMPTY, "ENOTEMPTY", "Directory not empty"),
        (libc::ELOOP, "ELOOP", "Too many levels of symbolic links"),
        (libc::EDQUOT, "EDQUOT", "Disk quota exceeded"),
        (libc::ECONNREFUSED, "ECONNREFUSED", "Connection refused"),
        (libc::ECONNRESET, "ECONNRESET", "Connection reset by peer"),
        (libc::ETIMEDOUT, "ETIMEDOUT", "Connection timed out"),
    ];
    table
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|&(_, name, label)| (name, label))
}

#[cfg(not(unix))]
fn errno(code: i32) -> Option<(&'static str, &'static str)> {
    match code {
        2 => Some(("ENOENT", "No such file or directory")),
        5 => Some(("EACCES", "Permission denied")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(fmt_size(0), "0KB");
        assert_eq!(fmt_size(31), "31B");
        assert_eq!(fmt_size(511), "511B");
        assert_eq!(fmt_size(580), "0.58KB");
        assert_eq!(fmt_size(1_500), "1.50KB");
        assert_eq!(fmt_size(2_000_000), "2.0MB");
    }

    #[test]
    fn sys_errors() {
        let out = Out {
            color_out: false,
            color_err: false,
        };
        let err = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(
            out.sys(&err, "open", "failed to open x").0,
            "ENOENT: No such file or directory: failed to open x (open)"
        );
        assert_eq!(out.error("x").0, "error: x");
    }
}
