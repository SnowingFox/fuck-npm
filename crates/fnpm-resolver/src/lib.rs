//! Dependency resolution: turns the workspace's declared dependencies (plus an optional
//! previous [`LockGraph`]) into a new `LockGraph`. Handles every protocol, `npm:` aliases,
//! catalogs, overrides, packageExtensions, peers, optional/platform filtering,
//! `minimumReleaseAge`, `link-workspace-packages` and look-ahead prefetch.
//! See `docs/ARCHITECTURE.md#fnpm-resolver`.

pub mod overrides;
pub mod prefetch;

use std::sync::Arc;

use fnpm_config::Config;
use fnpm_registry::RegistryClient;
use fnpm_types::{DependencyGroup, EventSink, LockGraph, PackageId};
use fnpm_workspace::Workspace;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("No version matching \"{spec}\" found for specifier \"{name}\" (but package exists)")]
    NoMatchingVersion { name: String, spec: String },
    #[error("package \"{0}\" not found")]
    NotFound(String),
    #[error("Workspace dependency \"{name}\" not found")]
    WorkspaceNotFound { name: String },
    #[error("catalog:{catalog} has no entry for \"{name}\"")]
    CatalogEntryMissing { catalog: String, name: String },
    #[error("lockfile had changes, but lockfile is frozen")]
    FrozenLockfile { changes: Vec<String> },
    #[error("{name}@{spec}: {message}")]
    InvalidSpec { name: String, spec: String, message: String },
    #[error(transparent)]
    Registry(#[from] fnpm_registry::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Keep every locked resolution that still satisfies its declared spec; resolve the rest.
    Normal,
    /// Any difference from the previous graph is an error (`--frozen-lockfile`, `ci`).
    Frozen,
    /// `fnpm update [names]`: re-resolve the named packages (all if empty), optionally to
    /// `latest` ignoring the range (`--latest`).
    Update { names: Vec<String>, latest: bool },
}

/// A dependency to add or change in a package.json (`fnpm add`, `fnpm update <pkg>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Workspace member path (`""` for the root) whose package.json changes.
    pub workspace: String,
    pub name: String,
    /// The specifier to resolve (`latest` when the user gave none).
    pub spec: String,
    pub group: DependencyGroup,
}

pub struct ResolveInput<'a> {
    pub workspace: &'a Workspace,
    pub previous: Option<&'a LockGraph>,
    pub mode: Mode,
    pub requests: Vec<Request>,
    /// Remove these names from these workspaces (`fnpm remove`).
    pub removals: Vec<(String, String)>,
}

/// Streamed while resolving so fetching and linking can start before resolution ends.
#[derive(Debug, Clone)]
pub enum Resolved {
    /// A package whose content must be in the store. Sent once per package as soon as its
    /// resolution (and integrity, if known) is final.
    Package { id: PackageId },
}

#[derive(Debug, Clone)]
pub struct ResolveOutput {
    pub graph: LockGraph,
    /// The graph differs from `previous` (the lockfile must be saved).
    pub changed: bool,
    /// For each `Request`, the package it resolved to (summary and package.json writes).
    pub requests: Vec<(Request, PackageId)>,
    /// Non-fatal problems (peer warnings, deprecations, ignored overrides).
    pub warnings: Vec<String>,
}

pub struct Resolver {
    #[allow(dead_code)]
    config: Arc<Config>,
    #[allow(dead_code)]
    client: RegistryClient,
}

impl Resolver {
    pub fn new(config: Arc<Config>, client: RegistryClient) -> Self {
        Self { config, client }
    }

    /// Resolves the whole graph. `tx` receives [`Resolved`] events as packages become final;
    /// it may be `None`. Manifests are requested concurrently through the client (64-way, or
    /// the configured limit); CPU-heavy steps (manifest parsing, peer resolution, dedupe)
    /// run on rayon.
    pub async fn resolve(
        &self,
        _input: ResolveInput<'_>,
        _tx: Option<mpsc::UnboundedSender<Resolved>>,
        _events: &dyn EventSink,
    ) -> Result<ResolveOutput> {
        todo!()
    }
}
