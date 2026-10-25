//! `--filter` / `-F` selectors (bun `workspace_selection.rs`, pnpm grammar).
//!
//! `name` and globs (`@scope/*`), `./path` and `{path}`, `!` negation, `...pkg` (with
//! dependents), `pkg...` (with dependencies), `^` (exclude the package itself), `[ref]` is
//! not supported (error).

use crate::{Result, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Glob over member names (`*` matches the root as well, like bun).
    Name(String),
    /// Directory glob relative to the cwd.
    Path(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub target: Target,
    pub exclude: bool,
    pub include_dependencies: bool,
    pub include_dependents: bool,
    pub exclude_self: bool,
}

impl Selector {
    pub fn parse(_s: &str) -> Result<Self> {
        todo!()
    }
}

/// Indices into `ws.members`; `usize::MAX` stands for the root package. Positive selectors are
/// unioned, then negative ones subtracted.
pub fn select(_ws: &Workspace, _selectors: &[Selector], _cwd: &std::path::Path) -> Result<Vec<usize>> {
    todo!()
}
