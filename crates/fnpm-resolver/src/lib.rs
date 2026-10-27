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
    /// A remaining project of a pruned checkout needs a workspace that is not on disk. Each
    /// message is one of bun's `error:` lines.
    #[error("{}\nnote: a pruned checkout must keep every workspace that its remaining workspaces depend on", messages.join("\nerror: "))]
    PrunedWorkspace {
        /// What needs which workspace.
        messages: Vec<String>,
    },
    /// `--frozen-lockfile`: the declared dependencies differ from the lockfile. Prints bun's
    /// notes below the error: what changed, then how to fix it.
    #[error(
        "lockfile had changes, but lockfile is frozen{}",
        frozen_notes(changes, lockfile)
    )]
    FrozenLockfile {
        /// Each difference, in bun's words (`a changed from ^1 to ^2`, `b was added`, …).
        changes: Vec<String>,
        /// The lockfile the notes name (`fnpm.lock`, `bun.lockb`, `yarn.lock`, …).
        lockfile: String,
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
    /// Dependencies that failed to resolve, reported like bun: each row's error as it happened,
    /// then `<name>@<spec> failed to resolve` (or the catalog advice) for each, then the notes.
    /// `first` is the first row's error, for callers that look at the cause.
    #[error("{}", lines.join("\n"))]
    Unresolved {
        /// The first row's error.
        first: Box<Error>,
        /// The report; the first line has no `error: ` (the caller prints it), the others do.
        lines: Vec<String>,
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

impl Error {
    /// The error of the first failed row of an [`Error::Unresolved`], else `self`.
    pub fn cause(&self) -> &Error {
        match self {
            Self::Unresolved { first, .. } => first.cause(),
            other => other,
        }
    }
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// A [`Error::FrozenLockfile`] about fnpm.lock ([`Error::with_lockfile`] names another).
    pub fn frozen(changes: Vec<String>) -> Self {
        Self::FrozenLockfile {
            changes,
            lockfile: "fnpm.lock".into(),
        }
    }

    /// Names the lockfile a [`Error::FrozenLockfile`] was checked against.
    #[must_use]
    pub fn with_lockfile(self, name: &str) -> Self {
        match self {
            Self::FrozenLockfile { changes, .. } => Self::FrozenLockfile {
                changes,
                lockfile: name.to_owned(),
            },
            other => other,
        }
    }
}

/// bun's notes under `lockfile had changes, but lockfile is frozen`: the section that changed
/// (overrides, then the catalog, then packageExtensions, else the first dependency change),
/// then the way out.
fn frozen_notes(changes: &[String], lockfile: &str) -> String {
    let since = format!("in package.json changed since {lockfile} was saved");
    let section = frozen::SECTIONS
        .iter()
        .find(|s| changes.iter().any(|c| *c == format!("{s} changed")));
    let mut out = String::new();
    if let Some(section) = section {
        let section = if *section == "catalogs" {
            "the catalog"
        } else {
            section
        };
        out.push_str(&format!("\nnote: {section} {since}"));
    } else if let Some(change) = changes.iter().find(|c| frozen::is_dependency_change(c)) {
        out.push_str(&format!("\nnote: dependencies {since}: {change}"));
    }
    out.push_str(
        "\nnote: try re-running without --frozen-lockfile and commit the updated lockfile",
    );
    out
}

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
    /// `--frozen-lockfile` kept a lockfile that is behind package.json (a pruned workspace,
    /// patches or trust it does not record): nothing is saved, and the next plain install must
    /// not take the no-op fast path.
    pub behind: bool,
    /// For each `Request`, the package it resolved to (summary and package.json writes).
    pub requests: Vec<(Request, PackageId)>,
    /// Non-fatal problems (peer warnings, deprecations, ignored overrides).
    pub warnings: Vec<String>,
    /// A local dependency in an omitted group was missing and left unresolved; like bun, the
    /// lockfile is then not saved.
    pub unsaved: bool,
}

/// The lockfile records exactly what the package.json files declare, with every required
/// dependency bound: a resolve would reproduce it without asking any registry. Stricter than
/// `--frozen-lockfile`, which also accepts edits that keep every package where it is.
pub fn lockfile_matches(workspace: &Workspace, previous: &LockGraph) -> Result<bool> {
    Ok(frozen::complete(previous)
        && frozen::changes(workspace, previous)?.is_empty()
        && !frozen::stale_pin(previous))
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

        // Like bun, `--frozen-lockfile` without a lockfile resolves from package.json (and
        // saves nothing): there is no lockfile to keep.
        let mode = if mode == Mode::Frozen && previous.is_none() {
            Mode::Normal
        } else {
            mode
        };
        if mode == Mode::Frozen {
            let Some(prev) = previous else {
                unreachable!("a frozen resolve without a lockfile resolves normally");
            };
            let pruned = frozen::pruned(workspace, prev);
            // The lockfile is behind package.json (a pruned workspace, patches or trust it does
            // not record): the install keeps it, and the next plain install saves it.
            let behind = !pruned.is_empty() || !frozen::changes(workspace, prev)?.is_empty();
            warnings.extend(frozen::orphaned_patches(workspace, prev));
            let stripped;
            let prev = if pruned.is_empty() {
                prev
            } else {
                let dependents = frozen::pruned_dependents(workspace, prev, &pruned, true)?;
                if !dependents.is_empty() {
                    return Err(Error::PrunedWorkspace {
                        messages: dependents,
                    });
                }
                stripped = frozen::without_pruned(prev, &pruned);
                &stripped
            };
            // bun installs an edited `file:` folder as the lockfile has it, and says so.
            // Only its dependencies: bun links a folder's bins from its package.json.
            for path in frozen::folder_edits(workspace, prev, false) {
                events.emit(Event::warn(
                    format!(
                        "dependencies in {path}/package.json changed since lockfile was saved (run 'fnpm install')"
                    ),
                    None,
                ));
            }
            let (mut changes, edits) = frozen::classified(workspace, prev, true)?;
            // The note comes before most changes, but a changed section fails first.
            let section = changes.iter().any(|c| frozen::is_section_change(c));
            if !pruned.is_empty() && !section {
                let mut names: Vec<&str> = pruned
                    .iter()
                    .map(|(_, id)| prev.packages[id.index()].name.as_str())
                    .collect();
                names.sort_unstable();
                let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
                events.emit(Event::notice(format!(
                    "note: skipped {} workspace{} listed in fnpm.lock but not on disk: {}",
                    names.len(),
                    if names.len() == 1 { "" } else { "s" },
                    quoted.join(", ")
                )));
            }
            if !section {
                let declared =
                    catalogs::parse_catalogs(&workspace.root.raw, &workspace.settings.catalogs)?;
                if let Some(skipped) = frozen::skipped_catalogs(workspace, prev, &declared)
                    && !skipped.is_empty()
                {
                    let quoted: Vec<String> = skipped.iter().map(|n| format!("\"{n}\"")).collect();
                    events.emit(Event::notice(format!(
                        "note: skipped {} catalog {} not in fnpm.lock (unused by the workspaces on disk): {}",
                        skipped.len(),
                        if skipped.len() == 1 { "entry" } else { "entries" },
                        quoted.join(", ")
                    )));
                }
            }
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
            // A pin outside the range that package.json and the lockfile both record (one side of
            // a merge conflict taken) resolves again, and fails if that moves a package.
            let stale = changes.is_empty() && frozen::stale_pin(prev);
            if changes.is_empty() && !stale {
                let mut graph = prev.clone();
                graph.patched_dependencies = frozen::declared_patches(workspace);
                let mut out = self.unchanged(workspace, &graph, tx, events, warnings)?;
                out.behind = behind;
                return Ok(out);
            }
            // Like bun, edited dependency lists pass as long as resolving them against the
            // lockfile installs the same packages (a loosened range the locked version still
            // satisfies, a dependency moved to another group). Nothing is fetched until the
            // result is known to match.
            if changes.len() != edits {
                return Err(Error::frozen(changes));
            }
            let engine = engine::Engine::new(engine::EngineInput {
                cfg,
                patch_hashes: &self.patch_hashes,
                ws: workspace,
                prev: Some(prev),
                mode: Mode::Normal,
                requests,
                removals,
                manifests: self.manifests.clone(),
                fetcher: self.fetcher.clone(),
                events,
                tx: None,
                hints: Vec::new(),
            })?;
            let out = engine.run().await?;
            if !frozen::same_packages(prev, &out.graph) {
                return Err(Error::frozen(changes));
            }
            frozen::emit_all(&out.graph, cfg, tx.as_ref());
            warnings.extend(out.warnings);
            // The lockfile is behind package.json: the next plain install records the edits.
            return Ok(ResolveOutput {
                graph: out.graph,
                changed: false,
                behind: true,
                requests: out.requests,
                warnings,
                unsaved: out.unsaved,
            });
        }

        // A plain install drops a workspace that is gone from disk, but not one that a remaining
        // project still names with `workspace:`.
        if let Some(prev) = previous {
            let pruned = frozen::pruned(workspace, prev);
            if !pruned.is_empty() {
                let dependents = frozen::pruned_dependents(workspace, prev, &pruned, false)?;
                if !dependents.is_empty() {
                    return Err(Error::PrunedWorkspace {
                        messages: dependents,
                    });
                }
            }
        }

        if mode == Mode::Normal
            && requests.is_empty()
            && removals.is_empty()
            && let Some(prev) = previous
            && frozen::complete(prev)
            && frozen::changes(workspace, prev)?.is_empty()
            && !dedupe::overrides_unmet(prev)
            && !frozen::stale_pin(prev)
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
            behind: false,
            requests: out.requests,
            warnings,
            unsaved: out.unsaved,
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
            behind: false,
            requests: Vec::new(),
            warnings,
            unsaved: false,
        })
    }
}
