//! `node_modules` materialisation: hoisted (npm-style) and isolated (pnpm-style, with the
//! global virtual store), bins, and the clone / hardlink / copy backends.
//! See `docs/ARCHITECTURE.md#fnpm-linker`.

pub mod bins;
pub mod isolated;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fnpm_config::{Backend, Config};
use fnpm_lockfile::tree::Tree;
use fnpm_store::{PackageEntry, Store};
use fnpm_types::{ConfigVersion, EventSink, LockGraph, NodeLinker, PackageId};

pub use isolated::IsolatedLayout;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not create the \"node_modules\" directory: {0}")]
    NodeModules(std::io::Error),
    #[error("{package}: {source}")]
    Materialize {
        package: String,
        #[source]
        source: fnpm_store::MaterializeError,
    },
    #[error("{package}: bin {bin:?} points outside the package")]
    UnsafeBin { package: String, bin: String },
    #[error("{package}: not in the store: {message}")]
    Missing { package: String, message: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// bun's `resolve_node_linker`: a configured linker wins; otherwise isolated when the
/// lockfile is configVersion 1 and has workspaces (and was not migrated from npm), or when
/// `node_modules/.fnpm` already exists; hoisted otherwise.
pub fn choose_linker(
    configured: Option<NodeLinker>,
    config_version: ConfigVersion,
    has_workspaces: bool,
    migrated_from_npm: bool,
    project_dir: &Path,
) -> NodeLinker {
    if let Some(l) = configured {
        return l;
    }
    let isolated_tree = project_dir.join("node_modules").join(".fnpm").is_dir();
    if (config_version == ConfigVersion::V1 && has_workspaces && !migrated_from_npm) || isolated_tree {
        NodeLinker::Isolated
    } else {
        NodeLinker::Hoisted
    }
}

/// What the linker should build.
pub enum Layout {
    Hoisted(Tree),
    Isolated(IsolatedLayout),
}

/// Store content for each package. The core implements this over in-flight downloads so
/// linking overlaps fetching: `wait` blocks the calling link thread until the package is in
/// the store (or failed).
pub trait PackageSource: Sync {
    fn wait(&self, id: PackageId) -> std::result::Result<PackageEntry, String>;
}

#[derive(Debug, Clone)]
pub struct LinkOptions {
    pub backend: Backend,
    /// `--force`: rebuild every directory.
    pub force: bool,
    /// Existing `node_modules` is known to be absent: skip all verify and delete checks.
    pub fresh: bool,
    /// Link threads (bun caps at 8 for directory inode contention).
    pub threads: usize,
    pub global_store: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkReport {
    /// Packages written (not already up to date).
    pub installed: u32,
    /// Directories removed because they are no longer in the layout.
    pub removed: u32,
    /// Packages skipped because they do not match the platform.
    pub skipped: u32,
    /// `(package id, absolute dir)` for every installed package, for the script runner.
    pub dirs: Vec<(PackageId, PathBuf)>,
    pub backend_used: Option<Backend>,
}

pub struct Linker {
    #[allow(dead_code)]
    config: Arc<Config>,
    #[allow(dead_code)]
    store: Store,
}

impl Linker {
    pub fn new(config: Arc<Config>, store: Store) -> Self {
        Self { config, store }
    }

    /// Plans the layout for `graph` (hoisted: `fnpm_lockfile::tree::hoist`; isolated:
    /// [`isolated::plan`]).
    pub fn plan(&self, _graph: &LockGraph, _linker: NodeLinker) -> Layout {
        todo!()
    }

    /// Builds `node_modules` under `project_dir`. Runs on a dedicated rayon pool of
    /// `opts.threads` threads; directories are created in one batch before files.
    /// Does not run scripts; links bins of every package placed (bins that depend on a
    /// script's output are relinked by the script runner afterwards).
    pub fn link(
        &self,
        _project_dir: &Path,
        _graph: &LockGraph,
        _layout: &Layout,
        _source: &dyn PackageSource,
        _opts: &LinkOptions,
        _events: &dyn EventSink,
    ) -> Result<LinkReport> {
        todo!()
    }
}
