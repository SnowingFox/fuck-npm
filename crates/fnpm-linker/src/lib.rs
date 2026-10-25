//! `node_modules` materialisation: hoisted (npm-style) and isolated (pnpm-style, with the
//! global virtual store), bins, and the clone / hardlink / copy backends.
//! See `docs/ARCHITECTURE.md#fnpm-linker`.
//!
//! [`Linker::plan`] turns a [`LockGraph`] into a [`Layout`]; [`Linker::link`] builds it on disk
//! on a dedicated pool of at most 8 threads, waiting on [`PackageSource::wait`] per package so
//! linking overlaps downloads. A brand-new `node_modules` skips every verification and delete;
//! an existing one gets a cheap per-package check (`package.json` name/version, `.fnpm-tag`, or
//! an inode compare for links) and only mismatches are replaced (moved aside, deleted in the
//! background).

pub mod bins;
pub mod fsutil;
mod hoisted;
pub mod isolated;
mod isolated_link;
pub mod matcher;
pub mod prune;
pub mod store_key;
pub mod verify;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use fnpm_config::{Backend, Config};
use fnpm_lockfile::tree::{HoistOptions, Tree};
use fnpm_store::{PackageEntry, Store};
use fnpm_types::{Behavior, ConfigVersion, Event, EventSink, LockGraph, NodeLinker, PackageId};

pub use fsutil::flush_background_deletes;
pub use isolated::IsolatedLayout;

/// Every error of this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `node_modules` itself could not be created or opened.
    #[error("could not create the \"node_modules\" directory: {0}")]
    NodeModules(std::io::Error),
    /// Copying a package out of the store failed.
    #[error("{package}: {source}")]
    Materialize {
        /// `name@version`.
        package: String,
        /// The failed file operation.
        #[source]
        source: fnpm_store::MaterializeError,
    },
    /// A bin target leaves its package (kept for API compatibility; such bins are skipped).
    #[error("{package}: bin {bin:?} points outside the package")]
    UnsafeBin {
        /// `name@version`.
        package: String,
        /// The bin name.
        bin: String,
    },
    /// The package source could not provide the package.
    #[error("{package}: not in the store: {message}")]
    Missing {
        /// `name@version`.
        package: String,
        /// Why (download or extraction error).
        message: String,
    },
    /// A dependency name or path that would escape `node_modules`.
    #[error("{package}: refusing to install to unsafe path {path:?}")]
    UnsafePath {
        /// `name@version` of the declaring package.
        package: String,
        /// The offending alias or path.
        path: String,
    },
    /// Any other filesystem error.
    #[error("{path}: {source}")]
    Io {
        /// The path being worked on.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// bun's `resolve_node_linker`: a configured linker wins; otherwise isolated when the
/// lockfile is configVersion 1 and has workspaces (and was not migrated from npm), or when
/// `node_modules/.fnpm` already holds an isolated tree; hoisted otherwise.
///
/// fnpm keeps its no-op state in `node_modules/.fnpm` for both layouts, so "an isolated tree"
/// means `.fnpm` has an entry other than that state ([`is_isolated_tree`]).
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
    if (config_version == ConfigVersion::V1 && has_workspaces && !migrated_from_npm)
        || is_isolated_tree(project_dir)
    {
        NodeLinker::Isolated
    } else {
        NodeLinker::Hoisted
    }
}

/// Files fnpm keeps in `node_modules/.fnpm` for every layout.
const FNPM_META_FILES: [&str; 3] = ["state", "lock.bin", "layout"];

/// True when `node_modules/.fnpm` holds isolated store entries (stops at the first one).
pub fn is_isolated_tree(project_dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(project_dir.join("node_modules").join(".fnpm")) else {
        return false;
    };
    rd.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        !name.starts_with('.') && !FNPM_META_FILES.contains(&name.as_ref())
    })
}

/// `--linker` / `install.linker` values: `hoisted` or `isolated`; anything else is `None`
/// (bun: `Invalid value for --linker`).
pub fn parse_linker(value: &str) -> Option<NodeLinker> {
    match value {
        "hoisted" => Some(NodeLinker::Hoisted),
        "isolated" => Some(NodeLinker::Isolated),
        _ => None,
    }
}

/// `--backend` values (bun `CommandLineArguments`): `clonefile`, `clonefile_each_dir`,
/// `hardlink`, `copyfile`, `symlink`. Unknown values are `None` and silently ignored.
pub fn parse_backend(value: &str) -> Option<Backend> {
    match value {
        "clonefile" | "clonefile_each_dir" => Some(Backend::Clonefile),
        "hardlink" => Some(Backend::Hardlink),
        "copyfile" => Some(Backend::Copyfile),
        "symlink" => Some(Backend::Symlink),
        _ => None,
    }
}

/// The platform default backend: clonefile on macOS, hardlink elsewhere.
pub fn default_backend() -> Backend {
    if cfg!(target_os = "macos") {
        Backend::Clonefile
    } else {
        Backend::Hardlink
    }
}

/// What the linker should build.
#[derive(Debug, Clone)]
pub enum Layout {
    /// npm-style tree (`fnpm_lockfile::tree::hoist`).
    Hoisted(Tree),
    /// pnpm-style store with symlinks.
    Isolated(IsolatedLayout),
}

/// Store content for each package. The core implements this over in-flight downloads so
/// linking overlaps fetching: `wait` blocks the calling link thread until the package is in
/// the store (or failed).
pub trait PackageSource: Sync {
    /// The store entry of package `id` (for `file:` folders, the imported directory).
    fn wait(&self, id: PackageId) -> std::result::Result<PackageEntry, String>;

    /// The number of files of `id` if it is already available, without waiting. The linker
    /// starts the biggest packages first: one package's files go into the same directories,
    /// so the largest one (lodash: ~1,000 files in one directory) bounds the link time.
    fn size_hint(&self, id: PackageId) -> Option<usize> {
        let _ = id;
        None
    }

    /// The hash of the patch currently applied to `id` (`patchedDependencies`), if any. The
    /// linker writes it to [`verify::PATCH_TAG_FILE`] and reinstalls a package whose tag
    /// differs, so an edited patch file is applied again (bun `.bun-patch-tag`).
    fn patch_tag(&self, id: PackageId) -> Option<String> {
        let _ = id;
        None
    }
}

/// `items` ordered by the [`PackageSource::size_hint`] of their package, largest first
/// (unknown sizes keep their order, after the known ones).
pub(crate) fn largest_first<T: Copy>(
    source: &dyn PackageSource,
    items: &[T],
    package: impl Fn(T) -> PackageId,
) -> Vec<T> {
    let mut keyed: Vec<(usize, T)> = items
        .iter()
        .map(|&t| (source.size_hint(package(t)).unwrap_or(0), t))
        .collect();
    keyed.sort_by_key(|k| std::cmp::Reverse(k.0));
    keyed.into_iter().map(|(_, t)| t).collect()
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct LinkOptions {
    /// The backend to start with (degrades on failure, see `fsutil::Materializer`).
    pub backend: Backend,
    /// `--force`: rebuild every directory.
    pub force: bool,
    /// Existing `node_modules` is known to be absent: skip all verify and delete checks.
    /// (The linker also detects a missing `node_modules` itself.)
    pub fresh: bool,
    /// Link threads (capped at 8 for directory inode contention, like bun).
    pub threads: usize,
    /// Use `<store>/links` for eligible isolated entries.
    pub global_store: bool,
    /// A package without a patch must also have no [`verify::PATCH_TAG_FILE`] (set when the
    /// previous install had patches: a removed patch reinstalls the package unpatched).
    pub check_unpatched: bool,
}

impl Default for LinkOptions {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            force: false,
            fresh: false,
            threads: fnpm_types::available_parallelism().min(8),
            global_store: true,
            check_unpatched: false,
        }
    }
}

/// What [`Linker::link`] did.
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
    /// The weakest backend that was used for a file, if any file was placed.
    pub backend_used: Option<Backend>,
}

/// Decides which packages count as trusted (their scripts may run), so they are never shared
/// through the global virtual store.
pub type TrustFn = Arc<dyn Fn(&LockGraph, PackageId) -> bool + Send + Sync>;

/// Builds `node_modules`.
pub struct Linker {
    config: Arc<Config>,
    store: Store,
    trusted: TrustFn,
    native_binlinks: Vec<String>,
    pool: parking_lot::Mutex<Option<(usize, Arc<rayon::ThreadPool>)>>,
}

/// The process-wide link pool of `threads` threads (1-8). Starting 8 threads costs 0.5-1 ms, so
/// one pool is kept for the process (napi runs many commands in one); [`prewarm_pool`] starts
/// it ahead of time.
fn link_pool(threads: usize) -> Result<Arc<rayon::ThreadPool>> {
    static POOL: parking_lot::Mutex<Option<(usize, Arc<rayon::ThreadPool>)>> =
        parking_lot::Mutex::new(None);
    let threads = threads.clamp(1, 8);
    let mut guard = POOL.lock();
    if let Some((n, p)) = guard.as_ref()
        && *n == threads
    {
        return Ok(p.clone());
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("fnpm-link-{i}"))
        .build()
        .map_err(|e| Error::NodeModules(std::io::Error::other(e.to_string())))?;
    let pool = Arc::new(pool);
    *guard = Some((threads, pool.clone()));
    Ok(pool)
}

/// Starts the link pool for `threads` (see [`LinkOptions::threads`]) on a background thread,
/// so [`Linker::link`] finds it running. Cheap to call more than once.
pub fn prewarm_pool(threads: usize) {
    let _ = std::thread::Builder::new()
        .name("fnpm-link-warm".into())
        .spawn(move || {
            let _ = link_pool(threads);
        });
}

/// bun's default native-binlink packages (`postinstall_optimizer.rs`).
pub const DEFAULT_NATIVE_BINLINKS: [&str; 2] = ["esbuild", "@anthropic-ai/claude-code"];

impl Linker {
    /// A linker over `store` with `config`. By default every package that declares an install
    /// script counts as trusted (the conservative choice for global-store sharing) unless
    /// scripts are ignored; [`Linker::with_trust`] replaces that.
    pub fn new(config: Arc<Config>, store: Store) -> Self {
        let ignore = config.ignore_scripts;
        Self {
            config,
            store,
            trusted: Arc::new(move |g: &LockGraph, id: PackageId| {
                !ignore && g.get(id).is_some_and(|p| p.meta.needs_scripts())
            }),
            native_binlinks: DEFAULT_NATIVE_BINLINKS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            pool: parking_lot::Mutex::new(None),
        }
    }

    /// Replaces the trust policy (fnpm-core passes the scripts crate's `TrustPolicy`).
    pub fn with_trust(mut self, trusted: TrustFn) -> Self {
        self.trusted = trusted;
        self
    }

    /// `package.json#nativeDependencies` (bun): `Some(list)` replaces the default list of
    /// packages whose bins link straight into their platform package; `Some(vec![])` turns the
    /// optimisation off (also `FNPM_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER`, which the
    /// core maps to this).
    pub fn with_native_dependencies(mut self, list: Option<Vec<String>>) -> Self {
        if let Some(l) = list {
            self.native_binlinks = l;
        }
        self
    }

    /// The configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Which edges are installed, from the configuration (`--omit`, `--production`).
    pub fn include(&self) -> Behavior {
        Self::include_for(&self.config)
    }

    /// [`Linker::include`] for `c` without building a linker.
    pub fn include_for(c: &Config) -> Behavior {
        let mut b = Behavior::PROD | Behavior::WORKSPACE;
        if !c.omit.dev && !c.production {
            b |= Behavior::DEV;
        }
        if !c.omit.optional {
            b |= Behavior::OPTIONAL;
        }
        if !c.omit.peer {
            b |= Behavior::PEER;
        }
        b
    }

    /// Plans the layout for `graph` (hoisted: `fnpm_lockfile::tree::hoist`; isolated:
    /// [`isolated::plan_filtered`]).
    pub fn plan(&self, graph: &LockGraph, linker: NodeLinker) -> Layout {
        match linker {
            NodeLinker::Hoisted => Layout::Hoisted(fnpm_lockfile::tree::hoist(
                graph,
                &HoistOptions {
                    include: self.include(),
                    platform: self.config.platform,
                    workspaces: None,
                },
            )),
            NodeLinker::Isolated => Layout::Isolated(self.plan_isolated(graph, None)),
        }
    }

    /// The isolated plan with a `--filter` workspace selection.
    pub fn plan_isolated(
        &self,
        graph: &LockGraph,
        workspaces: Option<Vec<PackageId>>,
    ) -> IsolatedLayout {
        let c = &self.config;
        let trusted = |id: PackageId| (self.trusted)(graph, id);
        let opts = isolated::PlanOptions {
            hoist: c.hoist,
            hoist_pattern: &c.hoist_pattern,
            public_hoist_pattern: &c.public_hoist_pattern,
            global_store: c.global_store,
            trusted: &trusted,
        };
        let filter = isolated::PlanFilter {
            include: self.include(),
            platform: c.platform,
            workspaces,
            no_verify: !c.verify_integrity,
            project_basename: c
                .project_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        isolated::plan_filtered(graph, &opts, &filter)
    }

    fn pool(&self, threads: usize) -> Result<Arc<rayon::ThreadPool>> {
        let mut guard = self.pool.lock();
        if let Some((n, p)) = guard.as_ref()
            && *n == threads.clamp(1, 8)
        {
            return Ok(p.clone());
        }
        let pool = link_pool(threads)?;
        *guard = Some((threads.clamp(1, 8), pool.clone()));
        Ok(pool)
    }

    /// Builds `node_modules` under `project_dir`. Runs on a dedicated rayon pool of
    /// `opts.threads` threads (max 8). Does not run scripts; links bins of every package placed
    /// (bins that depend on a script's output are relinked by the script runner afterwards).
    pub fn link(
        &self,
        project_dir: &Path,
        graph: &LockGraph,
        layout: &Layout,
        source: &dyn PackageSource,
        opts: &LinkOptions,
        events: &dyn EventSink,
    ) -> Result<LinkReport> {
        // Read the umask before any link thread creates files (see `bins::exec_mode`).
        let _ = bins::exec_mode();
        let pool = self.pool(opts.threads)?;
        let mat = fsutil::Materializer::new(opts.backend);
        let cx = LinkCx {
            project_dir,
            graph,
            source,
            opts,
            events,
            mat: &mat,
            store: &self.store,
            platform: self.config.platform,
            native: &self.native_binlinks,
            global_link_dir: self.config.global_dir.join("links"),
            done: AtomicU32::new(0),
            total: AtomicU32::new(0),
        };
        let mut report = pool.install(|| match layout {
            Layout::Hoisted(tree) => hoisted::link(&cx, tree),
            Layout::Isolated(l) => isolated_link::link(&cx, l),
        })?;
        report.backend_used = mat.used();
        if events.enabled() {
            let total = cx.total.load(Ordering::Relaxed);
            events.emit(Event::Link { done: total, total });
        }
        Ok(report)
    }
}

/// State shared by both link implementations.
pub(crate) struct LinkCx<'a> {
    pub project_dir: &'a Path,
    pub graph: &'a LockGraph,
    pub source: &'a dyn PackageSource,
    pub opts: &'a LinkOptions,
    pub events: &'a dyn EventSink,
    pub mat: &'a fsutil::Materializer,
    #[allow(dead_code)]
    pub store: &'a Store,
    pub platform: fnpm_types::Platform,
    pub native: &'a [String],
    /// Where `fnpm link` registers packages: `link:<name>` resolves to `<this>/<name>`.
    pub global_link_dir: PathBuf,
    pub done: AtomicU32,
    pub total: AtomicU32,
}

impl LinkCx<'_> {
    /// `name@version` (or `name@<resolution>`) for messages.
    pub fn label(&self, id: PackageId) -> String {
        self.graph
            .get(id)
            .map_or_else(|| format!("#{}", id.0), |p| p.key())
    }

    /// Progress: one more package handled.
    /// The installed copy at `dir` carries the patch `id` should have (none, or the current
    /// patch hash).
    pub fn patch_ok(&self, id: PackageId, dir: &Path) -> bool {
        let tag = dir.join(verify::PATCH_TAG_FILE);
        match self.source.patch_tag(id) {
            Some(t) => std::fs::read(&tag).is_ok_and(|b| b == t.as_bytes()),
            None => !self.opts.check_unpatched || std::fs::symlink_metadata(&tag).is_err(),
        }
    }

    pub fn tick(&self) {
        let done = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        if done.is_multiple_of(64) && self.events.enabled() {
            self.events.emit(Event::Link {
                done,
                total: self.total.load(Ordering::Relaxed),
            });
        }
    }

    /// Waits for the package and copies it into `dest` (which must not exist), then writes the
    /// `.fnpm-tag` for git/tarball packages.
    pub fn materialize(&self, id: PackageId, dest: &Path) -> Result<()> {
        let entry = self.source.wait(id).map_err(|message| Error::Missing {
            package: self.label(id),
            message,
        })?;
        self.mat
            .materialize(&entry.root, &entry.index, dest)
            .map_err(|source| Error::Materialize {
                package: self.label(id),
                source,
            })?;
        if let Some(tag) = self.source.patch_tag(id) {
            let p = dest.join(verify::PATCH_TAG_FILE);
            write_new_file(&p, tag.as_bytes()).map_err(|source| Error::Io { path: p, source })?;
        }
        if let Some(pkg) = self.graph.get(id)
            && let Some(tag) = verify::tag_for(&pkg.resolution)
        {
            let p = dest.join(verify::TAG_FILE);
            write_new_file(&p, tag.as_bytes()).map_err(|source| Error::Io { path: p, source })?;
        }
        Ok(())
    }

    /// The directory a root, workspace or `link:` package lives in (`None` for packages that
    /// come from the store).
    pub fn local_target(&self, resolution: &fnpm_types::Resolution) -> Option<PathBuf> {
        hoisted::local_target_with(self.project_dir, Some(&self.global_link_dir), resolution)
    }

    /// bun native binlink: for a package on the native list, its optional dependency built for
    /// this platform (both `os` and `cpu` declared and matching).
    pub fn native_target(&self, id: PackageId) -> Option<PackageId> {
        let pkg = self.graph.get(id)?;
        if pkg.bin.is_none() || !self.native.contains(&pkg.name) {
            return None;
        }
        pkg.dependencies.iter().filter_map(|e| e.target).find(|t| {
            self.graph.get(*t).is_some_and(|p| {
                p.platform.os != fnpm_types::OsSet::ALL
                    && p.platform.cpu != fnpm_types::CpuSet::ALL
                    && p.platform.matches(&self.platform)
            })
        })
    }
}

/// Writes `bytes` to a new file at `p`. Whatever the package shipped under that name is
/// unlinked first, never written through: with the hardlink backend it is a store blob shared
/// with other packages, and it could be a symlink.
fn write_new_file(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)?
        .write_all(bytes)
}

/// Node's lookup of `name` from a package directory: its own `node_modules`, then every
/// ancestor `node_modules`. Used to find a native binlink's platform package on disk.
pub(crate) fn node_resolve(package_dir: &Path, name: &str) -> Option<PathBuf> {
    let own = fsutil::join_name(&package_dir.join("node_modules"), name);
    if own.exists() {
        return Some(own);
    }
    let mut cur = package_dir.parent();
    while let Some(dir) = cur {
        if dir.file_name().is_some_and(|n| n == "node_modules") {
            let p = fsutil::join_name(dir, name);
            if p.exists() {
                return Some(p);
            }
        }
        cur = dir.parent();
    }
    None
}

/// Whether a workspace directory lies inside the project (bun `is_member_path_outside`): the
/// linker never moves or deletes anything in a member outside of it.
pub(crate) fn is_inside_project(project_dir: &Path, dir: &Path) -> bool {
    fsutil::normalize(dir).starts_with(fsutil::normalize(project_dir))
}

/// Entries a package manager creates in `node_modules` (bun `is_a_linkers`): every non-dot
/// name, plus the dot entries of fnpm, bun, npm, yarn and pnpm.
pub(crate) fn is_a_linkers(name: &str) -> bool {
    const OF_A_PACKAGE_MANAGER: [&str; 7] = [
        ".bin",
        ".fnpm",
        ".bun",
        ".modules.yaml",
        ".package-lock.json",
        ".yarn-integrity",
        ".yarn-state.yml",
    ];
    !name.starts_with('.')
        || OF_A_PACKAGE_MANAGER.contains(&name)
        || name.starts_with(".pnpm")
        || fsutil::is_leftover(name)
}

/// Moves every linker-made entry of `node_modules` (except `keep`) into
/// `node_modules/.old_modules-<hex>` and deletes that in the background (switching linkers,
/// bun `move_node_modules_aside`). Scoped folders are moved whole. Returns how many entries
/// moved.
pub(crate) fn move_node_modules_aside(node_modules: &Path, keep: &[&str]) -> u32 {
    // Nothing is moved out of a `node_modules` that is a symlink (bun `LinkedNodeModules`):
    // it is shared with something else.
    if fsutil::kind(node_modules) != fsutil::Kind::Dir {
        return 0;
    }
    let Ok(rd) = std::fs::read_dir(node_modules) else {
        return 0;
    };
    // List first: the `.old_modules-<hex>` folder created below must not be seen (and queued
    // for deletion as a leftover) while entries are still being moved into it.
    let names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let old = node_modules.join(format!(
        "{}{:016x}",
        fsutil::OLD_MODULES_PREFIX,
        fsutil::random_suffix()
    ));
    let mut moved = 0;
    let mut made = false;
    for name in names {
        if !is_a_linkers(&name) || keep.contains(&name.as_str()) {
            continue;
        }
        if fsutil::is_leftover(&name) {
            // Leftovers of earlier runs: delete in the background, in place.
            let aside = node_modules.join(&name);
            fsutil::delete_in_background(aside);
            continue;
        }
        if !made {
            if std::fs::create_dir(&old).is_err() {
                return moved;
            }
            made = true;
        }
        let from = node_modules.join(&name);
        if std::fs::rename(&from, old.join(&name)).is_err() {
            let _ = fsutil::remove_any(&from);
        }
        moved += 1;
    }
    if made {
        fsutil::delete_in_background(old);
    }
    moved
}

#[cfg(all(test, unix))]
mod tests;
