//! package.json model and editing, workspace discovery (`workspaces`, `pnpm-workspace.yaml`),
//! `--filter` selectors and topological order. See `docs/ARCHITECTURE.md#fnpm-workspace`.

pub mod filter;
pub mod manifest;

use std::path::{Path, PathBuf};

use fnpm_types::Catalogs;

pub use filter::{Selector, select};
pub use manifest::{PackageJson, PackageJsonEditor};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {message}")]
    InvalidPackageJson { path: PathBuf, message: String },
    #[error("no package.json found in {0} or any parent directory")]
    NoProject(PathBuf),
    #[error("invalid workspace pattern {pattern:?}: {message}")]
    BadPattern { pattern: String, message: String },
    #[error("duplicate workspace name \"{name}\" at {first} and {second}")]
    DuplicateName { name: String, first: String, second: String },
    #[error("invalid filter {filter:?}: {message}")]
    BadFilter { filter: String, message: String },
    #[error("No packages matched the filter")]
    NoMatch,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A workspace member (the root is not a member).
#[derive(Debug, Clone)]
pub struct Member {
    /// From package.json; members without a name get `fnpm_types::name::fallback_package_name`.
    pub name: String,
    pub version: String,
    /// Relative to the root, `/`-separated, no leading `./`.
    pub path: String,
    pub dir: PathBuf,
    pub manifest: PackageJson,
}

/// Root-level settings that `pnpm-workspace.yaml` can also provide.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSettings {
    pub catalogs: Catalogs,
    /// pnpm `overrides` (merged under package.json `overrides`).
    pub overrides: Vec<(String, String)>,
    /// pnpm `onlyBuiltDependencies` -> trustedDependencies.
    pub only_built_dependencies: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root_dir: PathBuf,
    pub root: PackageJson,
    /// Sorted by `path`.
    pub members: Vec<Member>,
    pub settings: WorkspaceSettings,
}

impl Workspace {
    /// Finds the project root from `cwd`: the nearest ancestor package.json whose
    /// `workspaces` (or sibling `pnpm-workspace.yaml`) includes `cwd`'s package, else the
    /// nearest package.json.
    pub fn find_root(_cwd: &Path) -> Result<PathBuf> {
        todo!()
    }

    /// Reads the root package.json and expands workspace globs with globset over a
    /// parallel walk that skips `node_modules` and dot-directories; `!pattern` excludes.
    /// Member package.json files are read in parallel (rayon).
    pub fn load(_root_dir: &Path) -> Result<Self> {
        todo!()
    }

    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    pub fn member_by_path(&self, path: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.path == path)
    }

    /// The member containing `dir`, if any (for `fnpm add` run inside a workspace).
    pub fn member_for_dir(&self, _dir: &Path) -> Option<&Member> {
        todo!()
    }

    /// Members in dependency order; each inner Vec can run in parallel. Cycles are broken by
    /// path order and reported in the second value.
    pub fn topo_levels(&self, _members: &[usize]) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
        todo!()
    }
}
