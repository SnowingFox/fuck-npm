//! Failures the launcher reports, with the TS CLI's codes (`packages/fuck-npm/src/errors.ts`).

use std::fmt;

/// How a failure is reported (`reportError` in `src/cli/runtime.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A usage error (`SilentError`): message plus a pointer to the command's help.
    Usage,
    /// A failure reported by the core (`CoreError`): message only.
    Core,
}

/// A failure with a stable code for `--json` (`{ok: false, error: {code, message}}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub kind: ErrorKind,
    /// `UNKNOWN_FLAG`, `INVALID_FLAG_VALUE`, `FROZEN_LOCKFILE`, ...
    pub code: String,
    pub message: String,
}

impl CliError {
    /// A usage error with one of the TS CLI's `ErrorCode`s.
    pub fn usage(code: &str, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Usage,
            code: code.to_owned(),
            message: message.into(),
        }
    }

    /// `INVALID_FLAG_VALUE`.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::usage("INVALID_FLAG_VALUE", message)
    }

    /// `FLAG_CONFLICT`.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::usage("FLAG_CONFLICT", message)
    }

    /// `MISSING_ARGUMENT`.
    pub fn missing(message: impl Into<String>) -> Self {
        Self::usage("MISSING_ARGUMENT", message)
    }

    /// A core failure carrying `fnpm_core::Error::code`.
    pub fn core(code: &str, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Core,
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}
