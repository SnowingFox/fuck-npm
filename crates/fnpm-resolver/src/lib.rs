//! Dependency resolution: turns the workspace's declared dependencies (plus an optional
//! previous [`LockGraph`]) into a new `LockGraph`. Handles every protocol, `npm:` aliases,
//! catalogs, overrides, packageExtensions, peers, optional/platform filtering,
//! `minimumReleaseAge`, `link-workspace-packages` and look-ahead prefetch.
//! See `docs/ARCHITECTURE.md#fnpm-resolver`.
//!
//! The resolver is a port of bun's (`install/PackageManager/PackageManagerEnqueue.rs` and
//! friends). Data comes through two traits, [`ManifestSource`] and [`PackageFetcher`], which
//! [`RegistrySource`] implements over `fnpm-registry` and `fnpm-store`; tests use fakes.

#![warn(missing_docs)]

pub mod catalogs;
pub mod dedupe;
mod engine;
pub mod extensions;
mod frozen;
mod local;
pub mod options;
pub mod overrides;
pub mod pick;
pub mod prefetch;
pub mod source;
mod tarball;

use std::sync::Arc;

use fnpm_config::Config;
use fnpm_registry::RegistryClient;
use fnpm_types::{DependencyGroup, Event, EventSink, LockGraph, PackageId, Resolution};
use fnpm_workspace::Workspace;
use tokio::sync::mpsc;

pub use dedupe::{DedupeReport, DedupeRow, dedupe};
pub use source::{
    BoxFuture, FetchRequest, FetchedPackage, ManifestSource, PackageFetcher, RegistrySource,
};

/// Why a resolve failed. Messages follow bun's wording.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The package exists but no published version satisfies the range.
    #[error("No version matching \"{spec}\" found for specifier \"{name}\" (but package exists)")]
    NoMatchingVersion {
        /// The registry name.
        name: String,
        /// The range as written.
        spec: String,
    },
    /// The registry has no such package.
    #[error("package \"{0}\" not found")]
    NotFound(String),
    /// The registry answered 404 for the manifest of `name` (bun: `GET <url> - 404`).
    #[error("GET {url} - 404")]
    ManifestNotFound {
        /// The package name.
        name: String,
        /// The manifest URL, without credentials.
        url: String,
    },
    /// A `workspace:` dependency names no workspace member.
    #[error("Workspace dependency \"{name}\" not found")]
    WorkspaceNotFound {
        /// The dependency (or, for requests, the workspace path).
        name: String,
    },
    /// A `catalog:` dependency whose catalog has no entry for it.
    #[error("catalog:{catalog} has no entry for \"{name}\"")]
    CatalogEntryMissing {
        /// The catalog name (`""` is the default catalog).
        catalog: String,
        /// The dependency name.
        name: String,
    },
    /// `--frozen-lockfile`: the declared dependencies differ from the lockfile.
    #[error("lockfile had changes, but lockfile is frozen")]
    FrozenLockfile {
        /// Each difference, in bun's words (`a changed from ^1 to ^2`, `b was added`, …).
        changes: Vec<String>,
    },
    /// A specifier that cannot be used (unparsable, unsafe name, forbidden path).
    #[error("{name}@{spec}: {message}")]
    InvalidSpec {
        /// The dependency name.
        name: String,
        /// The specifier as written.
        spec: String,
        /// What is wrong with it.
        message: String,
    },
    /// A dependency name (or the registry name it asks for) that cannot be a folder under
    /// `node_modules`. bun reports it, then the dependency that failed.
    #[error("invalid dependency name \"{bad}\"\nerror: {name}@{spec} failed to resolve")]
    InvalidName {
        /// The dependency name.
        name: String,
        /// The specifier as written.
        spec: String,
        /// The unsafe name, escaped for the terminal.
        bad: String,
    },
    /// The registry, git or a tarball download failed.
    #[error(transparent)]
    Registry(#[from] fnpm_registry::Error),
    /// A dist-tag the manifest does not have.
    #[error("Package \"{name}\" with tag \"{tag}\" not found, but package exists")]
    DistTagNotFound {
        /// The registry name.
        name: String,
        /// The tag.
        tag: String,
    },
    /// Every matching version is newer than `minimumReleaseAge` allows. `message` is bun's.
    #[error("{message}")]
    TooRecent {
        /// The registry name.
        name: String,
        /// The range or tag as written.
        spec: String,
        /// The full message.
        message: String,
    },
    /// A root-level field (overrides, catalogs, trustedDependencies, a dependency name, …) is
    /// malformed.
    #[error("{path}: {message}")]
    InvalidPackageJson {
        /// The file (`package.json` for the root).
        path: String,
        /// What is wrong.
        message: String,
    },
    /// A `file:` / `link:` / `workspace:` target has no package.json (or does not exist).
    #[error("{message}")]
    MissingPackageJson {
        /// The dependency name.
        name: String,
        /// bun's message.
        message: String,
    },
    /// A `patchedDependencies` entry whose patch file cannot be read.
    #[error("patchedDependencies \"{key}\": {path}: {message}")]
    PatchFile {
        /// The `name@version` key.
        key: String,
        /// The patch path as written.
        path: String,
        /// The read error.
        message: String,
    },
    /// A resolver-owned command-line value is invalid (`--minimum-release-age`, `--cpu`, …).
    #[error("{message}")]
    InvalidOption {
        /// The flag.
        flag: String,
        /// bun's message.
        message: String,
    },
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What a resolve may change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Keep every locked resolution that still satisfies its declared spec; resolve the rest.
    Normal,
    /// Any difference from the previous graph is an error (`--frozen-lockfile`, `ci`).
    Frozen,
    /// `fnpm update [names]`: re-resolve the named packages (all if empty), optionally to
    /// `latest` ignoring the range (`--latest`).
    Update {
        /// Package names to update (direct or transitive); empty updates every direct one.
        names: Vec<String>,
        /// `--latest`: move direct dependencies to the `latest` tag.
        latest: bool,
    },
}

/// A dependency to add or change in a package.json (`fnpm add`, `fnpm update <pkg>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Workspace member path (`""` for the root) whose package.json changes.
    pub workspace: String,
    /// The dependency name (the key written to package.json).
    pub name: String,
    /// The specifier to resolve (`latest` when the user gave none).
    pub spec: String,
    /// The group the dependency is written to (it is moved out of the others).
    pub group: DependencyGroup,
}

/// Everything one resolve reads besides configuration.
pub struct ResolveInput<'a> {
    /// The root and member package.json files.
    pub workspace: &'a Workspace,
    /// The lockfile's graph; its resolutions are reused where they still satisfy.
    pub previous: Option<&'a LockGraph>,
    /// What the resolve may change.
    pub mode: Mode,
    /// Dependencies to add or change (`fnpm add`).
    pub requests: Vec<Request>,
    /// Remove these names from these workspaces (`fnpm remove`), as `(workspace path, name)`.
    pub removals: Vec<(String, String)>,
}

/// Streamed while resolving so fetching and linking can start before resolution ends.
#[derive(Debug, Clone)]
pub enum Resolved {
    /// A package whose content must be in the store. Sent once per package as soon as its
    /// resolution (and integrity, if known) is final.
    ///
    /// `name`, `version` and `resolution` are everything the fetcher needs (tarball URL or
    /// default location, integrity, git commit), so the fetch can start before the graph exists.
    /// `id` is the package's index in the graph under construction: it is the id in
    /// [`ResolveOutput::graph`] when the previous graph is reused unchanged (no-op and frozen
    /// installs), but a full resolve may compact the graph afterwards (dropping versions that
    /// deduplication made unreachable), so match by `(name, resolution)` there
    /// ([`LockGraph::find`]).
    Package {
        /// Index in the graph under construction (see above).
        id: PackageId,
        /// The package name.
        name: String,
        /// The package version.
        version: String,
        /// Where its content comes from.
        resolution: Resolution,
    },
}

impl Resolved {
    pub(crate) fn of(id: PackageId, package: &fnpm_types::Package) -> Self {
        Self::Package {
            id,
            name: package.name.clone(),
            version: package.version.clone(),
            resolution: package.resolution.clone(),
        }
    }
}

/// The result of a resolve.
#[derive(Debug, Clone)]
pub struct ResolveOutput {
    /// The resolved graph (root at 0, unreachable packages dropped, edges sorted).
    pub graph: LockGraph,
    /// The graph differs from `previous` (the lockfile must be saved).
    pub changed: bool,
    /// For each `Request`, the package it resolved to (summary and package.json writes).
    pub requests: Vec<(Request, PackageId)>,
    /// Non-fatal problems (peer warnings, deprecations, ignored overrides).
    pub warnings: Vec<String>,
}

/// Resolves workspaces into [`LockGraph`]s. Cheap to create; holds no per-resolve state.
pub struct Resolver {
    config: Arc<Config>,
    manifests: Arc<dyn ManifestSource>,
    fetcher: Arc<dyn PackageFetcher>,
    patch_hashes: std::collections::BTreeMap<String, String>,
}

impl Resolver {
    /// A resolver over the registry client (manifests, GitHub, git) and the store (git and
    /// tarball packages are imported while their package.json is read).
    pub fn new(config: Arc<Config>, client: RegistryClient) -> Self {
        let source = Arc::new(RegistrySource::new(config.clone(), client));
        Self {
            config,
            manifests: source.clone(),
            fetcher: source,
            patch_hashes: Default::default(),
        }
    }

    /// A resolver over custom sources (tests, `fnpm x`, offline mirrors).
    pub fn with_sources(
        config: Arc<Config>,
        manifests: Arc<dyn ManifestSource>,
        fetcher: Arc<dyn PackageFetcher>,
    ) -> Self {
        Self {
            config,
            manifests,
            fetcher,
            patch_hashes: Default::default(),
        }
    }

    /// Supplies hashes for patch files a command will write after resolution succeeds.
    pub fn with_patch_hashes(mut self, hashes: std::collections::BTreeMap<String, String>) -> Self {
        self.patch_hashes = hashes;
        self
    }

    /// Resolves the whole graph. `tx` receives [`Resolved`] events as packages become final;
    /// it may be `None`. Manifests are requested concurrently through the source (the registry
    /// client bounds the concurrency); the queue itself is processed in order so results never
    /// depend on network timing. Must run inside a tokio runtime.
    pub async fn resolve(
        &self,
        input: ResolveInput<'_>,
        tx: Option<mpsc::UnboundedSender<Resolved>>,
        events: &dyn EventSink,
    ) -> Result<ResolveOutput> {
        let ResolveInput {
            workspace,
            previous,
            mode,
            requests,
            removals,
        } = input;
        let cfg = &*self.config;
        let mut warnings = Vec::new();
        let previous = match previous {
            Some(p) => match p.validate() {
                Ok(()) => Some(p),
                Err(e) => {
                    warnings.push(format!("ignoring the lockfile: {e}"));
                    None
                }
            },
            None => None,
        };

        if mode == Mode::Frozen {
            let Some(prev) = previous else {
                return Err(Error::FrozenLockfile {
                    changes: vec!["lockfile is missing".into()],
                });
            };
            let mut changes = frozen::changes(workspace, prev)?;
            for r in &requests {
                changes.push(format!("{} was added", r.name));
            }
            for (ws, name) in &removals {
                let suffix = if ws.is_empty() {
                    String::new()
                } else {
                    format!(" in {ws}")
                };
                changes.push(format!("{name} was removed{suffix}"));
            }
            if !frozen::complete(prev) {
                changes.push("lockfile is missing resolutions".into());
            }
            if !changes.is_empty() {
                return Err(Error::FrozenLockfile { changes });
            }
            return self.unchanged(workspace, prev, tx, events, warnings);
        }

        if mode == Mode::Normal
            && requests.is_empty()
            && removals.is_empty()
            && let Some(prev) = previous
            && frozen::complete(prev)
            && frozen::changes(workspace, prev)?.is_empty()
        {
            return self.unchanged(workspace, prev, tx, events, warnings);
        }

        let hints = if previous.is_none() {
            prefetch::load_hints(&cfg.cache_dir, &workspace.root_dir)
        } else {
            Vec::new()
        };
        let engine = engine::Engine::new(engine::EngineInput {
            cfg,
            patch_hashes: &self.patch_hashes,
            ws: workspace,
            prev: previous,
            mode,
            requests,
            removals,
            manifests: self.manifests.clone(),
            fetcher: self.fetcher.clone(),
            events,
            tx,
            hints,
        })?;
        let out = engine.run().await?;
        if !out.manifest_order.is_empty() {
            let (cache, project, order) = (
                cfg.cache_dir.clone(),
                workspace.root_dir.clone(),
                out.manifest_order.clone(),
            );
            tokio::task::spawn_blocking(move || prefetch::save_hints(&cache, &project, &order));
        }
        let changed = previous.is_none_or(|p| !frozen::equivalent(p, &out.graph));
        warnings.extend(out.warnings);
        if events.enabled() {
            let n = out.graph.len() as u32;
            events.emit(Event::Resolve {
                resolved: n,
                total: n,
                name: None,
            });
        }
        Ok(ResolveOutput {
            graph: out.graph,
            changed,
            requests: out.requests,
            warnings,
        })
    }

    /// The previous graph as the result. The lockfile records patch paths only, so the patch
    /// hashes (part of the patched packages' store keys) are computed from the files as they
    /// are now; a missing patch file fails here like it does on a full resolve.
    fn unchanged(
        &self,
        workspace: &Workspace,
        prev: &LockGraph,
        tx: Option<mpsc::UnboundedSender<Resolved>>,
        events: &dyn EventSink,
        warnings: Vec<String>,
    ) -> Result<ResolveOutput> {
        let mut graph = prev.clone();
        // Text locks omit trust names absent from the tree. Restore the explicit manifest
        // policy so an unused allowlist never falls back to the built-in trusted list.
        if let Some(trusted) = frozen::trusted(workspace)? {
            graph.trusted_dependencies = Some(trusted);
        }
        for (key, patch) in &mut graph.patched_dependencies {
            patch.hash = Some(match self.patch_hashes.get(key) {
                Some(hash) => hash.clone(),
                None => engine::patch_hash(&workspace.root_dir, key, &patch.path)?,
            });
        }
        frozen::emit_all(&graph, &self.config, tx.as_ref());
        if events.enabled() {
            let n = prev.len() as u32;
            events.emit(Event::Resolve {
                resolved: n,
                total: n,
                name: None,
            });
        }
        Ok(ResolveOutput {
            graph,
            changed: false,
            requests: Vec::new(),
            warnings,
        })
    }
}
