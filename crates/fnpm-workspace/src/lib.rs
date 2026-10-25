//! package.json model and editing, workspace discovery (`workspaces`, `pnpm-workspace.yaml`),
//! `--filter` selectors and topological order. See `docs/ARCHITECTURE.md#fnpm-workspace`.
//!
//! Ports bun's `install/lockfile/Package/WorkspaceMap.rs`,
//! `install/PackageManager/{workspace_selection,PackageJSONEditor}.rs` and
//! `runtime/cli/{filter_arg,filter_run}.rs`.

mod discover;
mod editor;
pub mod filter;
pub mod glob;
pub mod graph;
mod jsonc;
pub mod manifest;
mod paths;
pub mod run;

use std::path::{Path, PathBuf};

use fnpm_types::Catalogs;

pub use discover::{LoadOptions, MissingWorkspace};
pub use filter::{
    Candidate, RootSelection, SelectOptions, Selection, Selector, Target, quote_patterns, select,
    select_candidates, select_with, unmatched_message,
};
pub use graph::WorkspaceGraph;
pub use manifest::{PackageJson, PackageJsonEditor};
pub use run::{RunPackage, RunPackages};

/// Every error this crate returns. Messages follow bun's wording where bun has one.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `path` is empty when the text did not come from a file.
    #[error("{}{message}", path_prefix(.path))]
    InvalidPackageJson { path: PathBuf, message: String },
    #[error("no package.json found in {0} or any parent directory")]
    NoProject(PathBuf),
    #[error("invalid workspace pattern {pattern:?}: {message}")]
    BadPattern { pattern: String, message: String },
    /// `first` and `second` are the member paths (relative to the root) that share `name`.
    #[error("Workspace name \"{name}\" already exists")]
    DuplicateName {
        name: String,
        first: String,
        second: String,
    },
    #[error("--filter \"{filter}\" {message}")]
    BadFilter { filter: String, message: String },
    /// Nothing was selected. Use [`unmatched_message`] for bun's full sentence with patterns.
    #[error("No workspace packages matched the filter")]
    NoMatch,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A literal (non-glob) `workspaces` entry whose package.json does not exist.
    #[error("Workspace not found \"{0}\"")]
    WorkspaceNotFound(String),
    /// `workspace:<range>` that the member's version does not satisfy.
    #[error("No matching version for workspace dependency \"{name}\". Version: \"{literal}\"")]
    NoMatchingWorkspaceVersion { name: String, literal: String },
    /// A filesystem error while expanding a glob `workspaces` entry.
    #[error("Failed to run workspace pattern {pattern} due to error {} ({syscall} \"{}\")", errno_name(.source), .path.display())]
    GlobFailed {
        pattern: String,
        syscall: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn path_prefix(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}: ", path.display())
    }
}

/// The errno spelling node and bun print (`ENOENT`), or the error kind.
pub(crate) fn errno_name(err: &std::io::Error) -> String {
    #[cfg(unix)]
    if let Some(code) = err.raw_os_error() {
        let name = match code {
            libc_codes::ENOENT => "ENOENT",
            libc_codes::EACCES => "EACCES",
            libc_codes::EPERM => "EPERM",
            libc_codes::ENOTDIR => "ENOTDIR",
            libc_codes::EISDIR => "EISDIR",
            libc_codes::ELOOP => "ELOOP",
            libc_codes::EMFILE => "EMFILE",
            libc_codes::ENAMETOOLONG => "ENAMETOOLONG",
            libc_codes::EIO => "EIO",
            _ => "",
        };
        if !name.is_empty() {
            return name.to_owned();
        }
    }
    match err.kind() {
        std::io::ErrorKind::NotFound => "ENOENT".to_owned(),
        std::io::ErrorKind::PermissionDenied => "EACCES".to_owned(),
        kind => format!("{kind:?}"),
    }
}

#[cfg(unix)]
mod libc_codes {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EACCES: i32 = 13;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EMFILE: i32 = 24;
    #[cfg(target_os = "linux")]
    pub const ENAMETOOLONG: i32 = 36;
    #[cfg(not(target_os = "linux"))]
    pub const ENAMETOOLONG: i32 = 63;
    #[cfg(target_os = "linux")]
    pub const ELOOP: i32 = 40;
    #[cfg(not(target_os = "linux"))]
    pub const ELOOP: i32 = 62;
}

/// A `workspace:` range that names no version, so any version of the member links (bun
/// `range.is_star() || is_tagged_version_only`, and paths).
fn unchecked_range(range: &str) -> bool {
    let r = range.trim();
    matches!(r, "" | "*" | "^" | "~" | "x" | "X")
        || r.starts_with('.')
        || r.starts_with('/')
        || r.as_bytes()[0].is_ascii_alphabetic() && r.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A workspace member (the root is not a member).
#[derive(Debug, Clone)]
pub struct Member {
    /// From package.json. Members without a usable `name` are skipped during discovery, like
    /// bun, pnpm and npm do, so this is never empty.
    pub name: String,
    /// From package.json; empty when the member has no string `version`.
    pub version: String,
    /// Relative to the root, `/`-separated, no leading `./`. Members outside the root start
    /// with `../`.
    pub path: String,
    /// Absolute, lexically normalised.
    pub dir: PathBuf,
    pub manifest: PackageJson,
}

/// Root-level settings that `pnpm-workspace.yaml` can also provide.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSettings {
    /// package.json catalogs (`workspaces.catalog(s)`, else top-level `catalog(s)` when the root
    /// has `workspaces`), plus `pnpm-workspace.yaml` entries that package.json does not have.
    /// `catalogs.default` is merged into [`Catalogs::default`].
    pub catalogs: Catalogs,
    /// pnpm-workspace.yaml `overrides`, in file order (merged under package.json `overrides` by
    /// the resolver).
    pub overrides: Vec<(String, String)>,
    /// pnpm-workspace.yaml `onlyBuiltDependencies` plus package.json
    /// `pnpm.onlyBuiltDependencies` -> trustedDependencies. `None` when neither is present.
    pub only_built_dependencies: Option<Vec<String>>,
}

/// A loaded project: the root manifest and its members.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Absolute, lexically normalised.
    pub root_dir: PathBuf,
    pub root: PackageJson,
    /// Sorted by `path`.
    pub members: Vec<Member>,
    pub settings: WorkspaceSettings,
}

impl Workspace {
    /// Finds the project root from `cwd`: the nearest ancestor package.json whose
    /// `workspaces` (or sibling `pnpm-workspace.yaml`) includes `cwd`'s package, else the
    /// nearest package.json. Like bun, the first ancestor that declares workspaces decides: if
    /// it does not list the package, the package is its own root. A package without a usable
    /// `name` is always its own root, and a package.json in a sticky, group- or world-writable
    /// directory above `cwd` (such as `/tmp`) is ignored, as anyone could have written it.
    pub fn find_root(cwd: &Path) -> Result<PathBuf> {
        discover::find_root(cwd)
    }

    /// Reads the root package.json and expands workspace globs over a parallel walk that skips
    /// `node_modules`, `.git`, `CMakeFiles` and dot-directories; `!pattern` excludes. Member
    /// package.json files are read in parallel (rayon). Missing literal members are an error
    /// ([`MissingWorkspace::Error`]); warnings are dropped (see [`Workspace::load_with`]).
    pub fn load(root_dir: &Path) -> Result<Self> {
        Self::load_with(root_dir, &LoadOptions::default()).map(|(ws, _)| ws)
    }

    /// [`Workspace::load`] with options; also returns bun's warnings (for example
    /// `skipping workspace "x": package.json has no "name"`).
    pub fn load_with(root_dir: &Path, options: &LoadOptions) -> Result<(Self, Vec<String>)> {
        discover::load(root_dir, options)
    }

    /// The member called `name`.
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    /// The member at `path` (relative to the root, `/`-separated; `./` and a trailing `/` are
    /// accepted).
    pub fn member_by_path(&self, path: &str) -> Option<&Member> {
        let path = paths::clean_relative(path);
        self.members.iter().find(|m| m.path == path)
    }

    /// The member containing `dir`, if any (for `fnpm add` run inside a workspace). With nested
    /// members the deepest one wins.
    pub fn member_for_dir(&self, dir: &Path) -> Option<&Member> {
        let dir = paths::absolute(dir);
        self.members
            .iter()
            .filter(|m| dir.starts_with(&m.dir))
            .max_by_key(|m| m.dir.components().count())
    }

    /// The manifest for an index from [`select`] (`usize::MAX` is the root).
    pub fn manifest(&self, index: usize) -> Option<&PackageJson> {
        if index == usize::MAX {
            Some(&self.root)
        } else {
            self.members.get(index).map(|m| &m.manifest)
        }
    }

    /// Members in dependency order for `run --filter` and lifecycle scripts; each inner Vec can
    /// run in parallel. `members` holds indices into [`Workspace::members`] (`usize::MAX` is
    /// the root). Edges are the names in `dependencies`, `devDependencies` and
    /// `optionalDependencies` that name another package of the set (bun `filter_run.rs`).
    ///
    /// Like bun, a dependency cycle anywhere in the set drops the ordering completely: the
    /// result is then one level with every package (in input order), and the second value
    /// lists the cycles (each in input order). Without cycles the second value is empty.
    pub fn topo_levels(&self, members: &[usize]) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
        let manifests: Vec<&PackageJson> = members
            .iter()
            .map(|&i| self.manifest(i).expect("index from this workspace"))
            .collect();
        let names: Vec<&str> = manifests
            .iter()
            .map(|m| m.name.as_deref().unwrap_or(""))
            .collect();
        let (levels, cycles) =
            graph::topo_levels_by_names(&names, |i| graph::run_dependency_names(manifests[i]));
        let map = |v: Vec<usize>| v.into_iter().map(|i| members[i]).collect::<Vec<_>>();
        (
            levels.into_iter().map(map).collect(),
            cycles.into_iter().map(map).collect(),
        )
    }

    /// Resolves a `workspace:` dependency declared in the package at `from_dir` (bun
    /// `Package.rs`): `workspace:*`/`^`/`~`/empty name the member called `alias`;
    /// `workspace:name@range` names `name`; a range must be satisfied by the member's version
    /// unless it names no version (`*`, `^`, `~`, a tag such as `beta`, or a path); a name that
    /// is no member but is the root's name is the root (self-reference); otherwise the payload
    /// is a path from `from_dir` (`workspace:../pkg`, `workspace:.`). Returns a member index,
    /// `usize::MAX` for the root, or `None` when nothing matches. `literal` may include the
    /// `workspace:` prefix.
    pub fn resolve_workspace_spec(
        &self,
        from_dir: &Path,
        alias: &str,
        literal: &str,
        satisfies: fn(&str, &str) -> bool,
    ) -> Result<Option<usize>> {
        let input = literal.trim_start();
        let input = input.strip_prefix("workspace:").unwrap_or(input);
        let trimmed = input.trim();
        let (name, range) = if matches!(trimmed, "" | "*" | "^" | "~") {
            (alias, None)
        } else {
            match input.rfind('@').filter(|&at| at > 0) {
                Some(at) => (&input[..at], Some(&input[at + 1..])),
                None => (alias, Some(input)),
            }
        };
        let by_name = self
            .members
            .iter()
            .position(|m| m.name == name)
            .map(|i| (i, self.members[i].version.as_str()));
        let by_name = by_name.or_else(|| {
            (self.root.name.as_deref() == Some(name))
                .then(|| (usize::MAX, self.root.version.as_deref().unwrap_or("")))
        });
        if let Some((index, version)) = by_name {
            let Some(range) = range else {
                return Ok(Some(index));
            };
            if unchecked_range(range) || !version.is_empty() && satisfies(version, range.trim()) {
                return Ok(Some(index));
            }
            return Err(Error::NoMatchingWorkspaceVersion {
                name: alias.to_owned(),
                literal: literal.trim().to_owned(),
            });
        }
        if matches!(trimmed, "" | "*" | "^" | "~") {
            return Ok(None);
        }
        let from = paths::absolute(from_dir);
        let dir = paths::normalize(&from.join(trimmed));
        if dir == self.root_dir {
            return Ok(Some(usize::MAX));
        }
        Ok(self.members.iter().position(|m| m.dir == dir))
    }

    /// `indices` (from [`select`]) with each one after the workspaces it depends on, the order
    /// `publish -r` uses (bun `workspace_manifests.rs` `selected_in_dependency_order`). A cycle
    /// is broken at one of its members.
    pub fn dependency_order(&self, indices: &[usize], options: &SelectOptions) -> Vec<usize> {
        let graph = filter::workspace_graph(self, options);
        let pos: Vec<usize> = indices
            .iter()
            .map(|&i| if i == usize::MAX { 0 } else { i + 1 })
            .collect();
        graph
            .subgraph(&pos)
            .dependency_order()
            .into_iter()
            .map(|k| indices[k as usize])
            .collect()
    }
}
