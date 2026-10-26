//! Fetch + extract, overlapping resolution and linking.
//!
//! The resolver streams `Resolved::Package` as soon as a resolution is final; [`Fetcher::ensure`]
//! starts the download (or store lookup) for it right away on the tokio runtime. The linker's
//! [`fnpm_linker::PackageSource::wait`] blocks a link thread on the package's [`Slot`] until the
//! import finished, so linking starts while downloads are still in flight.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use fnpm_registry::RegistryClient;
use fnpm_store::{ImportOptions, PackageEntry, PackageKey, Store};
use fnpm_types::{Event, EventSink, Integrity, LockGraph, Package, PackageId, Resolution};
use futures_util::StreamExt;
use parking_lot::{Condvar, Mutex};

/// A package that is in the store.
#[derive(Debug, Clone)]
pub(crate) struct Fetched {
    /// Its store entry.
    pub entry: PackageEntry,
    /// Its key (the integrity is computed for packages the lockfile had none for).
    pub key: PackageKey,
}

/// Why a package could not be put into the store, with the error code the CLI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchFailure {
    /// The message.
    pub message: String,
    /// A [`crate::Error::code`] value (`INTEGRITY`, `NETWORK`, `NOT_FOUND`, `CORE_ERROR`).
    pub code: &'static str,
}

impl FetchFailure {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: "CORE_ERROR",
        }
    }
}

impl std::fmt::Display for FetchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<FetchFailure> for String {
    fn from(f: FetchFailure) -> Self {
        f.message
    }
}

impl From<fnpm_registry::Error> for FetchFailure {
    fn from(e: fnpm_registry::Error) -> Self {
        Self {
            code: crate::registry_code(&e),
            message: e.to_string(),
        }
    }
}

impl From<fnpm_store::Error> for FetchFailure {
    fn from(e: fnpm_store::Error) -> Self {
        let code = match e {
            fnpm_store::Error::Integrity { .. } => "INTEGRITY",
            _ => "CORE_ERROR",
        };
        Self {
            message: e.to_string(),
            code,
        }
    }
}

type Outcome = Result<Fetched, FetchFailure>;

/// One package's fetch: filled once, waited on by link threads (blocking) and the pipeline
/// (async).
#[derive(Default)]
pub(crate) struct Slot {
    result: Mutex<Option<Outcome>>,
    cv: Condvar,
    notify: tokio::sync::Notify,
}

impl Slot {
    fn set(&self, outcome: Outcome) {
        *self.result.lock() = Some(outcome);
        self.cv.notify_all();
        self.notify.notify_waiters();
    }

    /// Blocks the calling (non-runtime) thread until the fetch finished.
    pub fn wait_blocking(&self) -> Outcome {
        let mut guard = self.result.lock();
        loop {
            if let Some(r) = guard.as_ref() {
                return r.clone();
            }
            self.cv.wait(&mut guard);
        }
    }

    /// The number of files, if the fetch already succeeded (never waits).
    pub fn files_if_ready(&self) -> Option<usize> {
        match self.result.lock().as_ref() {
            Some(Ok(f)) => Some(f.entry.index.files.len()),
            _ => None,
        }
    }

    /// Waits on the runtime until the fetch finished.
    pub async fn wait(&self) -> Outcome {
        loop {
            let notified = self.notify.notified();
            if let Some(r) = self.result.lock().as_ref() {
                return r.clone();
            }
            notified.await;
        }
    }
}

/// Download counters for progress events and the summary.
#[derive(Default)]
struct Counters {
    total: AtomicU32,
    done: AtomicU32,
    bytes: AtomicU64,
}

struct Inner {
    store: Store,
    client: RegistryClient,
    root_dir: PathBuf,
    cache_dir: PathBuf,
    verify: bool,
    handle: tokio::runtime::Handle,
    events: Arc<dyn EventSink>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    counters: Counters,
    reused_packages: Option<Arc<AtomicU64>>,
}

/// Starts and tracks the imports of one install. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Fetcher {
    inner: Arc<Inner>,
}

/// The version in a package's store key. Only registry packages have one every graph source
/// agrees on. fnpm.lock (bun's format) records none for tarball, git and folder resolutions,
/// while a resolve or a migrated npm lockfile knows it, so keying those by version would miss
/// the store entry on the next install from fnpm.lock and download them again.
pub(crate) fn store_key_version<'a>(resolution: &Resolution, version: &'a str) -> &'a str {
    if matches!(resolution, Resolution::Npm { .. }) {
        version
    } else {
        ""
    }
}

/// The store key of a graph package with a known integrity.
pub(crate) fn store_key(p: &Package) -> Option<PackageKey> {
    Some(PackageKey {
        name: p.name.clone(),
        version: store_key_version(&p.resolution, &p.version).to_owned(),
        integrity: *p.resolution.integrity()?,
    })
}

/// Whether a resolution has content the store holds (registry, git, tarballs, folders).
pub(crate) fn needs_content(resolution: &Resolution) -> bool {
    resolution.is_storable() || matches!(resolution, Resolution::Folder { .. })
}

impl Fetcher {
    /// A fetcher importing into `store`; downloads run on the current runtime.
    pub fn new(
        store: Store,
        client: RegistryClient,
        root_dir: PathBuf,
        cache_dir: PathBuf,
        verify: bool,
        events: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                client,
                root_dir,
                cache_dir,
                verify,
                handle: tokio::runtime::Handle::current(),
                events,
                slots: Mutex::new(HashMap::new()),
                counters: Counters::default(),
                reused_packages: None,
            }),
        }
    }

    pub(crate) fn with_reuse_counter(mut self, counter: Option<Arc<AtomicU64>>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("fetcher not started")
            .reused_packages = counter;
        self
    }

    /// The store.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The slot of `name@resolution`, starting its fetch if this is the first request.
    pub fn ensure(&self, name: &str, version: &str, resolution: &Resolution) -> Arc<Slot> {
        let key = format!("{name}@{resolution}");
        let mut slots = self.inner.slots.lock();
        if let Some(s) = slots.get(&key) {
            return s.clone();
        }
        let slot = Arc::new(Slot::default());
        slots.insert(key, slot.clone());
        drop(slots);
        self.inner.counters.total.fetch_add(1, Ordering::Relaxed);
        start_fetch(
            &self.inner,
            slot.clone(),
            name.to_owned(),
            version.to_owned(),
            resolution.clone(),
        );
        slot
    }

    /// The slot of a package whose fetch was already started (never starts one).
    pub fn existing(&self, p: &Package) -> Option<Arc<Slot>> {
        self.inner
            .slots
            .lock()
            .get(&format!("{}@{}", p.name, p.resolution))
            .cloned()
    }

    /// [`Fetcher::ensure`] for a graph package.
    pub fn ensure_package(&self, p: &Package) -> Arc<Slot> {
        self.ensure(&p.name, &p.version, &p.resolution)
    }

    /// The `pkgs/` directory names of the installed packages that this install fetched or found
    /// in the store (patched variants included), for the store's project record.
    pub async fn store_dir_names(&self, graph: &LockGraph, installed: &[bool]) -> Vec<String> {
        let mut names = Vec::new();
        for (i, p) in graph.packages.iter().enumerate() {
            if !installed.get(i).copied().unwrap_or(false) || !needs_content(&p.resolution) {
                continue;
            }
            // Only what this install fetched: packages that were not linked (another
            // platform's optional binaries) must not be downloaded now. A package the linker
            // found in place was not fetched; its key comes from the graph.
            let key = match self.existing(p) {
                Some(slot) => match slot.wait().await {
                    Ok(f) => f.key,
                    Err(_) => continue,
                },
                None => match store_key(p) {
                    Some(key) => key,
                    None => continue,
                },
            };
            names.push(key.dir_name());
            let patch_key = p.patch_key();
            if let Some(patch) = graph.patched_dependencies.get(&patch_key)
                && let Ok((hash, _)) = read_patch(&self.inner.root_dir, patch)
            {
                names.push(key.patched_dir_name(hash));
            }
        }
        names.sort();
        names.dedup();
        names
    }

    /// The link source over `graph`.
    pub fn source<'g>(&'g self, graph: &'g LockGraph) -> GraphSource<'g> {
        GraphSource {
            fetcher: self,
            graph,
            patched: Mutex::new(HashMap::new()),
        }
    }
}

/// Runs the fetch of one package on the runtime and fills `slot`.
fn start_fetch(
    inner: &Arc<Inner>,
    slot: Arc<Slot>,
    name: String,
    version: String,
    resolution: Resolution,
) {
    let task_inner = inner.clone();
    inner.handle.spawn(async move {
        let inner = task_inner;
        if inner.events.wants_progress() {
            inner.events.emit(Event::Download {
                done: inner.counters.done.load(Ordering::Relaxed),
                total: inner.counters.total.load(Ordering::Relaxed),
                bytes: Some(inner.counters.bytes.load(Ordering::Relaxed)),
                name: Some(name.clone()),
            });
        }
        let outcome = fetch_one(&inner, &name, &version, &resolution).await;
        if outcome.is_ok() {
            let done = inner.counters.done.fetch_add(1, Ordering::Relaxed) + 1;
            if inner.events.enabled() {
                let total = inner.counters.total.load(Ordering::Relaxed);
                inner.events.emit(Event::Download {
                    done,
                    total,
                    bytes: Some(inner.counters.bytes.load(Ordering::Relaxed)),
                    name: Some(name.clone()),
                });
                // The tarball is extracted while it streams in: a finished fetch is both.
                inner.events.emit(Event::Extract {
                    done,
                    total,
                    name: Some(name.clone()),
                });
            }
        }
        slot.set(outcome);
    });
}

fn to_io(e: fnpm_registry::Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, FetchFailure> + Send + 'static,
) -> Result<T, FetchFailure> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| FetchFailure::other(e.to_string()))?
}

async fn fetch_one(inner: &Arc<Inner>, name: &str, version: &str, res: &Resolution) -> Outcome {
    let version = store_key_version(res, version);
    match res {
        Resolution::Npm {
            tarball_url,
            integrity,
            ..
        } => {
            let url = tarball_url
                .clone()
                .unwrap_or_else(|| inner.client.default_tarball_url(name, version));
            remote(inner, name, version, &url, *integrity).await
        }
        Resolution::RemoteTarball { url, integrity } => {
            remote(inner, name, version, url, *integrity).await
        }
        Resolution::Github {
            owner,
            repo,
            resolved_commit,
            integrity,
            ..
        } => {
            let url = inner
                .client
                .github_tarball_url(owner, repo, resolved_commit);
            remote(inner, name, version, &url, *integrity).await
        }
        Resolution::LocalTarball { path, integrity } => {
            // Resolution labels preserve the requested spelling, including redundant
            // components. Normalise before opening so a long label remains a valid path.
            let mut file = PathBuf::new();
            for component in inner.root_dir.join(path).components() {
                match component {
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => {
                        file.pop();
                    }
                    other => file.push(other.as_os_str()),
                }
            }
            if let Some(hit) = cached(inner, name, version, *integrity) {
                return Ok(hit);
            }
            let bytes = tokio::fs::read(&file)
                .await
                .map_err(|e| FetchFailure::other(format!("{}: {e}", file.display())))?;
            let (store, n, v, integrity) = (
                inner.store.clone(),
                name.to_owned(),
                version.to_owned(),
                *integrity,
            );
            let skip_verify = !inner.verify;
            blocking(move || match integrity {
                Some(integrity) => {
                    let key = PackageKey {
                        name: n,
                        version: v,
                        integrity,
                    };
                    store
                        .import_tarball_bytes(&key, &bytes, ImportOptions { skip_verify })
                        .map(|entry| Fetched { entry, key })
                        .map_err(FetchFailure::from)
                }
                None => store
                    .import_tarball_bytes_unpinned(&n, &v, &bytes)
                    .map(|(key, entry)| Fetched { entry, key })
                    .map_err(FetchFailure::from),
            })
            .await
        }
        Resolution::Git {
            url,
            resolved_commit,
            integrity,
            ..
        } => {
            if let Some(hit) = cached(inner, name, version, *integrity) {
                return Ok(hit);
            }
            git(inner, name, version, url, resolved_commit).await
        }
        Resolution::Folder { path } => {
            let dir = inner.root_dir.join(path);
            let (store, n, v) = (inner.store.clone(), name.to_owned(), version.to_owned());
            blocking(move || import_folder(&store, &n, &v, &dir)).await
        }
        other => Err(FetchFailure::other(format!(
            "{name}@{other} has no content to fetch"
        ))),
    }
}

fn cached(
    inner: &Inner,
    name: &str,
    version: &str,
    integrity: Option<Integrity>,
) -> Option<Fetched> {
    let key = PackageKey {
        name: name.to_owned(),
        version: version.to_owned(),
        integrity: integrity?,
    };
    let entry = inner.store.get(&key).ok().flatten()?;
    if let Some(counter) = &inner.reused_packages {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    Some(Fetched { entry, key })
}

async fn remote(
    inner: &Arc<Inner>,
    name: &str,
    version: &str,
    url: &str,
    integrity: Option<Integrity>,
) -> Outcome {
    // A URL without an integrity is found again by what it imported as last time.
    let known = integrity.or_else(|| inner.store.integrity_for_url(name, version, url));
    if let Some(hit) = cached(inner, name, version, known) {
        return Ok(hit);
    }
    if inner.client.offline_mode() == fnpm_config::OfflineMode::Offline {
        return Err(FetchFailure {
            code: "OFFLINE",
            message: format!("--offline: \"{name}\" is not in the cache"),
        });
    }
    let body = inner
        .client
        .tarball(url, integrity.as_ref())
        .await
        .map_err(FetchFailure::from)?;
    let counter = inner.clone();
    let stream = body.stream.map(move |chunk| {
        if let Ok(b) = &chunk {
            counter
                .counters
                .bytes
                .fetch_add(b.len() as u64, Ordering::Relaxed);
        }
        chunk.map_err(to_io)
    });
    match integrity {
        Some(integrity) => {
            let key = PackageKey {
                name: name.to_owned(),
                version: version.to_owned(),
                integrity,
            };
            let entry = inner
                .store
                .import_tarball(
                    &key,
                    stream,
                    ImportOptions {
                        skip_verify: !inner.verify,
                    },
                )
                .await
                .map_err(FetchFailure::from)?;
            Ok(Fetched { entry, key })
        }
        None => {
            let (key, entry) = inner
                .store
                .import_tarball_unpinned(name, version, stream)
                .await
                .map_err(FetchFailure::from)?;
            inner.store.remember_url(name, version, url, &key.integrity);
            Ok(Fetched { entry, key })
        }
    }
}

async fn git(inner: &Arc<Inner>, name: &str, version: &str, url: &str, sha: &str) -> Outcome {
    static CHECKOUTS: AtomicU64 = AtomicU64::new(0);
    let n = CHECKOUTS.fetch_add(1, Ordering::Relaxed);
    let dest = inner
        .cache_dir
        .join("tmp")
        .join(format!("core-git-{sha}-{}-{n}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dest).await;
    fnpm_registry::git::checkout(&inner.cache_dir, url, sha, None, &dest)
        .await
        .map_err(FetchFailure::from)?;
    let (store, nm, v, d) = (
        inner.store.clone(),
        name.to_owned(),
        version.to_owned(),
        dest.clone(),
    );
    let out = blocking(move || import_folder(&store, &nm, &v, &d)).await;
    let _ = tokio::fs::remove_dir_all(&dest).await;
    out
}

fn import_folder(store: &Store, name: &str, version: &str, dir: &Path) -> Outcome {
    let integrity = Store::pack_integrity(dir).map_err(FetchFailure::from)?;
    let key = PackageKey {
        name: name.to_owned(),
        version: version.to_owned(),
        integrity,
    };
    if let Ok(Some(entry)) = store.get(&key) {
        return Ok(Fetched { entry, key });
    }
    store
        .import_directory(&key, dir)
        .map(|entry| Fetched { entry, key })
        .map_err(FetchFailure::from)
}

/// The `_patch_hash` of a `patchedDependencies` file as it is now (the store keys patched
/// copies by it), with its text.
pub(crate) fn read_patch(
    root: &Path,
    patch: &fnpm_types::PatchedDependency,
) -> Result<(u64, String), String> {
    let path = root.join(&patch.path);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((fnpm_store::patch::hash_patch(text.as_bytes()), text))
}

/// [`fnpm_linker::PackageSource`] over a resolved graph: maps ids to slots (starting any fetch
/// that has not started), waits, and applies `patchedDependencies`.
pub(crate) struct GraphSource<'g> {
    fetcher: &'g Fetcher,
    graph: &'g LockGraph,
    patched: Mutex<HashMap<PackageId, Result<PackageEntry, String>>>,
}

impl GraphSource<'_> {
    fn patch(&self, pkg: &Package, base: Fetched) -> Result<PackageEntry, String> {
        let key = pkg.patch_key();
        let Some(patch) = self.graph.patched_dependencies.get(&key) else {
            return Ok(base.entry);
        };
        let store = self.fetcher.store();
        let (hash, text) = read_patch(&self.fetcher.inner.root_dir, patch)?;
        if let Ok(Some(entry)) = store.get_patched(&base.key, hash) {
            return Ok(entry);
        }
        let patched = store
            .import_patched(&base.key, &base.entry, &text, hash)
            .map_err(FetchFailure::from)?;
        for w in patched.warnings {
            self.fetcher.inner.events.emit(Event::warn(w, None));
        }
        Ok(patched.entry)
    }
}

impl fnpm_linker::PackageSource for GraphSource<'_> {
    fn wait(&self, id: PackageId) -> Result<PackageEntry, String> {
        let pkg = self
            .graph
            .get(id)
            .ok_or_else(|| format!("unknown package id {}", id.0))?;
        if !needs_content(&pkg.resolution) {
            return Err(format!("{} is not stored", pkg.key()));
        }
        let base = self.fetcher.ensure_package(pkg).wait_blocking()?;
        if self.graph.patched_dependencies.is_empty() {
            return Ok(base.entry);
        }
        if let Some(done) = self.patched.lock().get(&id) {
            return done.clone();
        }
        let out = self.patch(pkg, base);
        self.patched.lock().insert(id, out.clone());
        out
    }

    fn size_hint(&self, id: PackageId) -> Option<usize> {
        let pkg = self.graph.get(id)?;
        self.fetcher.existing(pkg)?.files_if_ready()
    }

    fn patch_tag(&self, id: PackageId) -> Option<String> {
        if self.graph.patched_dependencies.is_empty() {
            return None;
        }
        let pkg = self.graph.get(id)?;
        let patch = self.graph.patched_dependencies.get(&pkg.patch_key())?;
        // A patch file that cannot be read fails in `wait`; no tag then.
        let (hash, _) = read_patch(&self.fetcher.inner.root_dir, patch).ok()?;
        Some(format!("{hash:016x}"))
    }
}
