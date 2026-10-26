//! The package store, streaming tarball extraction, package file index, filesystem
//! materialisation primitives and prune. See `docs/ARCHITECTURE.md#fnpm-store`.
//!
//! Layout under `Config::store_dir` (`~/.fnpm/store/v2`):
//! ```text
//! pkgs/<name+escaped>@<version>-<integrity hex16>/
//!     package/...                          the package's files (the package root)
//!     index.bin                            rkyv FileIndex
//! links/<store key>-<closure hash16>/node_modules/<name>   global virtual store (isolated)
//! tmp/                                     staging, same filesystem, renamed into place
//! projects.json                            projects using this store (for prune)
//! ```
//!
//! Importing a tarball is one pass: the compressed bytes are hashed for the integrity check
//! while they are inflated and untarred, every file is written once into a staging package
//! (identical files of one package share an inode), `index.bin` is written and the staging
//! directory is renamed into `pkgs/`. The linker hard links (or clones) from there, so a
//! file costs one write when it is imported and one link per install.

#![warn(missing_docs)]

pub mod archive;
pub mod cache;
pub mod cache_pack;
pub mod dlx;
pub mod fsops;
pub mod git;
pub mod index;
pub mod integrity;
pub mod pack;
pub mod patch;
pub mod tar;
pub mod tuning;

mod extract;
mod gzip;
mod hash;
mod prune;
mod publish;
mod staging;
#[cfg(test)]
mod store_tests;
mod sys;
#[cfg(test)]
mod testutil;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use fnpm_types::{HashAlgorithm, Integrity};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

pub use fsops::{Backend, MaterializeError};
pub use index::{FileIndex, IndexMeta, IndexedFile};
pub use prune::CachePruneReport;
pub use tuning::Tuning;

use extract::{Built, StreamExtractor, Target};
use publish::Published;
use staging::Staging;

/// Errors of the store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The tarball bytes do not match the expected integrity.
    #[error("integrity check failed for {package}: expected {expected}, got {actual}")]
    Integrity {
        /// Package name.
        package: String,
        /// The expected SRI.
        expected: String,
        /// The SRI of the bytes received.
        actual: String,
    },
    /// The body is not a usable (gzipped) tar archive.
    #[error("{package}: invalid tarball: {message}")]
    InvalidTarball {
        /// Package name.
        package: String,
        /// What is wrong (libarchive-style wording).
        message: String,
    },
    /// An entry would be written outside the package.
    #[error("{package}: tarball entry {path:?} escapes the package")]
    PathTraversal {
        /// Package name.
        package: String,
        /// The entry path.
        path: String,
    },
    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path operated on.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// A patch file could not be parsed or applied.
    #[error("{package}: {message}")]
    Patch {
        /// Package name.
        package: String,
        /// What failed.
        message: String,
    },
    /// An operation refused to run (an unsafe cache directory, an invalid cache pack).
    #[error("{message}")]
    Refused {
        /// Why.
        message: String,
    },
}

/// The store result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

fn io_err(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Appends `s` with the characters that could split or escape a path component replaced (see
/// [`PackageKey::dir_name`]).
fn push_component_safe(out: &mut String, s: &str) {
    for c in s.chars() {
        out.push(match c {
            '/' | '\\' => '+',
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        });
    }
}

/// Identifies a package directory in the store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackageKey {
    /// Package name (`@scope/name` or `name`).
    pub name: String,
    /// Exact version.
    pub version: String,
    /// The tarball integrity. For git/folder imports, the integrity computed over the
    /// packed directory.
    pub integrity: Integrity,
}

impl PackageKey {
    /// `pkgs/` directory name: `@scope+name@1.2.3-<first 16 hex of digest>`.
    ///
    /// The name and version come from registries, lockfiles and tarballs, so both are made
    /// safe as one path component: `/` and `\` become `+`; NUL, control characters and the
    /// characters Windows rejects in file names (`:*?"<>|`) become `_`. A version such as
    /// `1/../../x` therefore cannot point outside `pkgs/`.
    pub fn dir_name(&self) -> String {
        let hex = self.integrity.to_hex();
        let mut out = String::with_capacity(self.name.len() + self.version.len() + 18);
        push_component_safe(&mut out, &self.name);
        out.push('@');
        push_component_safe(&mut out, &self.version);
        out.push('-');
        out.push_str(&hex[..16.min(hex.len())]);
        out
    }

    /// The directory of a patched copy: [`PackageKey::dir_name`] + `_patch_hash=<hex>` (bun's
    /// `_patch_hash=` suffix).
    pub fn patched_dir_name(&self, patch_hash: u64) -> String {
        format!("{}_patch_hash={patch_hash:x}", self.dir_name())
    }
}

/// A complete package directory in the store.
#[derive(Debug, Clone)]
pub struct PackageEntry {
    /// `pkgs/<dir>/package`: the directory to clone/link from.
    pub root: PathBuf,
    /// The package's files.
    pub index: Arc<FileIndex>,
}

/// Options of a tarball import.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    /// `--no-verify`: skip the integrity comparison (the tarball is not hashed at all).
    pub skip_verify: bool,
}

/// Options of [`Store::prune`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneOptions {
    /// Report what would be removed without removing it.
    pub dry_run: bool,
}

/// What [`Store::prune`] removed (or would remove).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    /// `links/` entries.
    pub removed_links: u64,
    /// `pkgs/` entries.
    pub removed_packages: u64,
    /// Files of the removed packages that nothing else linked to.
    pub removed_files: u64,
    /// Bytes freed: files of the removed packages that nothing else linked to.
    pub bytes_freed: u64,
}

/// [`Store::status`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreStatus {
    /// The store root.
    pub path: PathBuf,
    /// `pkgs/` entries.
    pub packages: u64,
    /// Files in `pkgs/` (hard links within a package counted once).
    pub files: u64,
    /// `links/` entries.
    pub links: u64,
    /// Physical bytes of the files in `pkgs/` (hard links counted once).
    pub bytes: u64,
}

/// A patched package and the patch sections that were skipped.
#[derive(Debug, Clone)]
pub struct PatchedPackage {
    /// The patched copy in the store.
    pub entry: PackageEntry,
    /// `skipping <path>: <reason>` for each section the patch could not apply (bun warns
    /// about these).
    pub warnings: Vec<String>,
}

/// A handle to the store. `Clone` + `Send` + `Sync`; all methods are safe to call
/// concurrently from many threads and processes (atomic renames, EEXIST means another writer
/// won).
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    tmp: PathBuf,
    tuning: Tuning,
}

/// The integrities of the packages this process imported (or found published while
/// importing), whichever [`Store`] value did it: the resolver and the fetcher open their own.
static IMPORTED: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<Integrity>>> =
    std::sync::LazyLock::new(Default::default);

fn spawn_cpu<T, F>(f: F) -> tokio::sync::oneshot::Receiver<Result<T>>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    rayon::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

async fn recv<T>(rx: &mut tokio::sync::oneshot::Receiver<Result<T>>, what: &Path) -> Result<T> {
    match rx.await {
        Ok(r) => r,
        Err(_) => Err(Error::Io {
            path: what.to_path_buf(),
            source: io::Error::other("extraction task did not finish"),
        }),
    }
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_device(_a: &Path, _b: &Path) -> bool {
    false
}

impl Store {
    /// Creates the directory skeleton if needed. Tuning comes from the environment
    /// ([`Tuning::from_env`]).
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_tuning(root, Tuning::from_env())
    }

    /// [`Store::open`] with explicit tuning.
    pub fn open_with_tuning(root: &Path, tuning: Tuning) -> Result<Self> {
        for sub in ["pkgs", "links", "tmp"] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        }
        let own_tmp = root.join("tmp");
        let tmp = match &tuning.tmpdir {
            Some(t) if t.is_absolute() && same_device(t, &own_tmp) => t.clone(),
            _ => own_tmp,
        };
        Ok(Self {
            root: root.to_path_buf(),
            inner: Arc::new(Inner { tmp, tuning }),
        })
    }

    /// Whether this process imported the package with `integrity` (an install with the cache
    /// disabled takes only those from the store, and downloads the others again).
    pub fn imported_here(&self, integrity: &Integrity) -> bool {
        IMPORTED.lock().contains(integrity)
    }

    /// The store root (`…/store/v2`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `links/`: the global virtual store.
    pub fn links_dir(&self) -> PathBuf {
        self.root.join("links")
    }

    /// `pkgs/`.
    pub fn pkgs_dir(&self) -> PathBuf {
        self.root.join("pkgs")
    }

    /// Where packages are staged before they are renamed into place.
    pub fn tmp_dir(&self) -> &Path {
        &self.inner.tmp
    }

    /// The tuning in effect.
    pub fn tuning(&self) -> &Tuning {
        &self.inner.tuning
    }

    /// `urls/<hash>`: the record of an unpinned download (see [`Store::remember_url`]).
    fn url_record(&self, name: &str, version: &str, url: &str) -> PathBuf {
        let hash = xxhash_rust::xxh3::xxh3_128(format!("{name}\0{version}\0{url}").as_bytes());
        self.root.join("urls").join(format!("{hash:032x}"))
    }

    /// The integrity a tarball URL without one in the lockfile or manifest had when it was
    /// last imported, recorded by [`Store::remember_url`]. Lets a later install (an offline
    /// one too) reuse the package instead of downloading it again, as bun's name@version
    /// cache does.
    pub fn integrity_for_url(&self, name: &str, version: &str, url: &str) -> Option<Integrity> {
        let text = std::fs::read_to_string(self.url_record(name, version, url)).ok()?;
        Integrity::parse(text.trim()).ok()
    }

    /// Records what an unpinned tarball URL imported as. Best effort: a failed write only
    /// means the next install downloads it again.
    pub fn remember_url(&self, name: &str, version: &str, url: &str, integrity: &Integrity) {
        let record = self.url_record(name, version, url);
        let Some(dir) = record.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let tmp = dir.join(format!(".tmp-{}-{nanos}", std::process::id()));
        if std::fs::write(&tmp, integrity.to_string()).is_ok()
            && std::fs::rename(&tmp, &record).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// `pkgs/<dir>` of `key`.
    pub fn package_dir(&self, key: &PackageKey) -> PathBuf {
        self.root.join("pkgs").join(key.dir_name())
    }

    /// `pkgs/<dir>_patch_hash=<hex>` of a patched copy.
    pub fn patched_package_dir(&self, key: &PackageKey, patch_hash: u64) -> PathBuf {
        self.root
            .join("pkgs")
            .join(key.patched_dir_name(patch_hash))
    }

    fn read_index(dir: &Path) -> io::Result<Option<(FileIndex, IndexMeta)>> {
        let path = dir.join("index.bin");
        #[cfg(not(target_os = "macos"))]
        let bytes = std::fs::read(&path);
        #[cfg(target_os = "macos")]
        let bytes = (|| {
            use std::io::Read;
            let mut file = std::fs::File::open(&path)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            // APFS can leave atime unchanged after reads. Refresh it at most
            // once a day, like relatime, so an actively used package is not age-pruned.
            let now = std::time::SystemTime::now();
            if file
                .metadata()
                .and_then(|meta| meta.accessed())
                .ok()
                .and_then(|used| now.duration_since(used).ok())
                .is_none_or(|age| age >= std::time::Duration::from_secs(86_400))
            {
                let _ = file.set_times(std::fs::FileTimes::new().set_accessed(now));
            }
            Ok::<_, io::Error>(bytes)
        })();
        match bytes {
            Ok(bytes) => Ok(FileIndex::from_bytes(&bytes)),
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::NotADirectory =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn read_entry(
        dir: &Path,
        integrity: Option<Integrity>,
        patch_hash: Option<u64>,
    ) -> io::Result<Option<PackageEntry>> {
        let Some((index, meta)) = Self::read_index(dir)? else {
            return Ok(None);
        };
        if let (Some(want), Some(have)) = (integrity, meta.integrity)
            && want != have
        {
            return Ok(None);
        }
        if meta.patch_hash != patch_hash {
            return Ok(None);
        }
        Ok(Some(PackageEntry {
            root: dir.join("package"),
            index: Arc::new(index),
        }))
    }

    /// One `stat` of `index.bin`; loads the index if present.
    pub fn get(&self, key: &PackageKey) -> Result<Option<PackageEntry>> {
        let dir = self.package_dir(key);
        Self::read_entry(&dir, Some(key.integrity), None).map_err(io_err(&dir))
    }

    /// The patched copy of `key` for the patch with hash `patch_hash`, if it was built.
    pub fn get_patched(&self, key: &PackageKey, patch_hash: u64) -> Result<Option<PackageEntry>> {
        let dir = self.patched_package_dir(key, patch_hash);
        Self::read_entry(&dir, Some(key.integrity), Some(patch_hash)).map_err(io_err(&dir))
    }

    fn new_staging(&self) -> Result<Arc<Staging>> {
        Staging::create(&self.inner.tmp)
            .map(Arc::new)
            .map_err(io_err(&self.inner.tmp))
    }

    fn publish_staging(
        &self,
        staging: &Staging,
        dest: &Path,
        meta: IndexMeta,
        built: Built,
    ) -> Result<PackageEntry> {
        let index = FileIndex::from_files(built.files, built.package_json);
        let index_path = staging.root().join("index.bin");
        std::fs::write(&index_path, index.to_bytes(&meta)).map_err(io_err(&index_path))?;
        let published = publish::publish(
            staging.root(),
            dest,
            self.inner.tuning.windows_rename_retry_ms,
            |d| {
                Self::read_entry(d, meta.integrity, meta.patch_hash)
                    .ok()
                    .flatten()
            },
        )
        .map_err(io_err(dest))?;
        if let Some(integrity) = meta.integrity {
            IMPORTED.lock().insert(integrity);
        }
        match published {
            Published::Ours => {
                staging.mark_published();
                Ok(PackageEntry {
                    root: dest.join("package"),
                    index: Arc::new(index),
                })
            }
            Published::Theirs(entry) => Ok(entry),
        }
    }

    fn finish_import(
        &self,
        staging: Arc<Staging>,
        target: Target,
        computed: Option<Integrity>,
        built: Built,
    ) -> Result<(PackageKey, PackageEntry)> {
        let key = target.resolve(computed)?;
        let dest = self.package_dir(&key);
        let entry = self.publish_staging(
            &staging,
            &dest,
            IndexMeta {
                integrity: Some(key.integrity),
                patch_hash: None,
            },
            built,
        )?;
        Ok((key, entry))
    }

    fn import_buffered(&self, target: Target, tgz: &[u8]) -> Result<(PackageKey, PackageEntry)> {
        let staging = self.new_staging()?;
        let (computed, built) = extract::hash_and_extract(&staging, target.hasher(), tgz);
        // As bun's buffered `extract_tarball`, a body that fails its integrity check is
        // reported as such even when it is also not a valid tarball.
        target.check(computed.as_ref())?;
        let built = built.map_err(|e| extract::with_package(e, target.name()))?;
        self.finish_import(staging, target, computed, built)
    }

    async fn import_stream<S>(
        &self,
        target: Target,
        mut body: S,
    ) -> Result<(PackageKey, PackageEntry)>
    where
        S: Stream<Item = io::Result<Bytes>> + Send + Unpin,
    {
        let label = PathBuf::from(target.name());
        let body_err = |source: io::Error| Error::Io {
            path: label.clone(),
            source,
        };
        let min = self.inner.tuning.effective_streaming_min_size();
        let threshold = self.inner.tuning.drain_threshold;

        // Small bodies are buffered whole and take the one-shot path.
        let mut head: Vec<u8> = Vec::new();
        loop {
            match body.next().await {
                Some(Ok(chunk)) => {
                    head.extend_from_slice(&chunk);
                    if head.len() as u64 >= min {
                        break;
                    }
                }
                Some(Err(e)) => return Err(body_err(e)),
                None => {
                    let store = self.clone();
                    let mut rx = spawn_cpu(move || store.import_buffered(target, &head));
                    return recv(&mut rx, &label).await;
                }
            }
        }

        // Large bodies are extracted while they download: one drain at a time on the rayon
        // pool, the next batch accumulating meanwhile.
        let name = target.name().to_owned();
        let store = self.clone();
        let hasher = target.hasher();
        let mut inflight = Some(spawn_cpu(move || {
            let mut x = Box::new(StreamExtractor::new(store.new_staging()?, hasher));
            x.feed(&head).map_err(|e| extract::with_package(e, &name))?;
            Ok(x)
        }));
        let mut idle: Option<Box<StreamExtractor>> = None;
        let mut pending: Vec<u8> = Vec::new();
        let mut ended = false;
        loop {
            if let Some(rx) = inflight.as_mut() {
                if ended {
                    let x = recv(rx, &label).await?;
                    inflight = None;
                    idle = Some(x);
                } else {
                    enum Step<T> {
                        Drained(
                            std::result::Result<Result<T>, tokio::sync::oneshot::error::RecvError>,
                        ),
                        Chunk(Option<io::Result<Bytes>>),
                    }
                    let step = tokio::select! {
                        biased;
                        r = &mut *rx => Step::Drained(r),
                        chunk = body.next() => Step::Chunk(chunk),
                    };
                    match step {
                        Step::Drained(r) => {
                            inflight = None;
                            idle = Some(match r {
                                Ok(r) => r?,
                                Err(_) => {
                                    return Err(Error::Io {
                                        path: label.clone(),
                                        source: io::Error::other("extraction task did not finish"),
                                    });
                                }
                            });
                        }
                        Step::Chunk(Some(Ok(c))) => pending.extend_from_slice(&c),
                        Step::Chunk(Some(Err(e))) => return Err(body_err(e)),
                        Step::Chunk(None) => ended = true,
                    }
                }
            } else if !ended && pending.len() < threshold {
                match body.next().await {
                    Some(Ok(c)) => pending.extend_from_slice(&c),
                    Some(Err(e)) => return Err(body_err(e)),
                    None => ended = true,
                }
            }
            if inflight.is_none()
                && let Some(mut x) = idle.take()
            {
                if !pending.is_empty() && (pending.len() >= threshold || ended) {
                    let batch = std::mem::take(&mut pending);
                    let name = target.name().to_owned();
                    inflight = Some(spawn_cpu(move || {
                        x.feed(&batch)
                            .map_err(|e| extract::with_package(e, &name))?;
                        Ok(x)
                    }));
                } else if ended {
                    idle = Some(x);
                    break;
                } else {
                    idle = Some(x);
                }
            }
        }
        let x = idle.expect("the loop ends with the extractor idle");
        let store = self.clone();
        let mut rx = spawn_cpu(move || {
            let staging = x.staging().clone();
            let (computed, built) = x
                .finish()
                .map_err(|e| extract::with_package(e, target.name()))?;
            store.finish_import(staging, target, computed, built)
        });
        recv(&mut rx, &label).await
    }

    /// Streams `gzip -> tar -> staging` in one pass while hashing the compressed bytes with the
    /// key's algorithm; verifies, writes `index.bin`, renames the staged directory into
    /// `pkgs/`. Blocking work runs on rayon / `spawn_blocking`; the future only awaits network
    /// chunks.
    ///
    /// Bodies smaller than [`Tuning::streaming_min_size`] are buffered and take the one-shot
    /// path; larger ones are extracted while they download.
    pub async fn import_tarball<S>(
        &self,
        key: &PackageKey,
        body: S,
        opts: ImportOptions,
    ) -> Result<PackageEntry>
    where
        S: Stream<Item = io::Result<Bytes>> + Send + Unpin,
    {
        let target = Target::Pinned {
            key: key.clone(),
            verify: !opts.skip_verify,
        };
        Ok(self.import_stream(target, body).await?.1)
    }

    /// [`Store::import_tarball`] for a tarball whose integrity is not known yet (a URL or
    /// `file:` dependency without one in the lockfile): the SHA-512 of the bytes is computed
    /// and returned in the key, which is what the lockfile should record.
    pub async fn import_tarball_unpinned<S>(
        &self,
        name: &str,
        version: &str,
        body: S,
    ) -> Result<(PackageKey, PackageEntry)>
    where
        S: Stream<Item = io::Result<Bytes>> + Send + Unpin,
    {
        let target = Target::Unpinned {
            name: name.to_owned(),
            version: version.to_owned(),
        };
        self.import_stream(target, body).await
    }

    /// The whole compressed tarball is in memory: libdeflate one-shot, then the same path.
    pub fn import_tarball_bytes(
        &self,
        key: &PackageKey,
        tgz: &[u8],
        opts: ImportOptions,
    ) -> Result<PackageEntry> {
        let target = Target::Pinned {
            key: key.clone(),
            verify: !opts.skip_verify,
        };
        Ok(self.import_buffered(target, tgz)?.1)
    }

    /// [`Store::import_tarball_bytes`] computing the SHA-512 integrity (see
    /// [`Store::import_tarball_unpinned`]).
    pub fn import_tarball_bytes_unpinned(
        &self,
        name: &str,
        version: &str,
        tgz: &[u8],
    ) -> Result<(PackageKey, PackageEntry)> {
        self.import_buffered(
            Target::Unpinned {
                name: name.to_owned(),
                version: version.to_owned(),
            },
            tgz,
        )
    }

    /// A directory (git checkout, `file:` folder for `pack`) imported as a package; the key's
    /// integrity is computed by the caller with [`Store::pack_integrity`].
    ///
    /// Every regular file below `src` is imported; symlinks and special files are skipped
    /// (a git checkout has its links replaced by [`git::finish_checkout`] first).
    pub fn import_directory(&self, key: &PackageKey, src: &Path) -> Result<PackageEntry> {
        let files = pack::walk_files(src)?;
        let staging = self.new_staging()?;
        let mut dirs: Vec<String> = Vec::new();
        for f in &files {
            if let Some(i) = f.path.rfind('/') {
                dirs.push(f.path[..i].to_owned());
            }
        }
        dirs.sort_unstable();
        dirs.dedup();
        for d in &dirs {
            staging
                .ensure_dir(d)
                .map_err(io_err(staging.package_root()))?;
        }
        let mut records = Vec::with_capacity(files.len());
        let mut package_json = None;
        for f in &files {
            let full = src.join(&f.path);
            let data = std::fs::read(&full).map_err(io_err(&full))?;
            if f.path == "package.json" {
                package_json = Some(String::from_utf8_lossy(&data).into_owned());
            }
            let mode = if f.mode & 0o111 != 0 { 0o755 } else { 0o644 };
            records.push(
                staging
                    .add_bytes(&f.path, mode, &data)
                    .map_err(io_err(&full))?,
            );
        }
        let built = Built {
            files: records,
            package_json,
        };
        let dest = self.package_dir(key);
        self.publish_staging(
            &staging,
            &dest,
            IndexMeta {
                integrity: Some(key.integrity),
                patch_hash: None,
            },
            built,
        )
    }

    /// The integrity of a directory as fnpm packs it (sorted entries, deterministic tar):
    /// SHA-512 over the uncompressed tar that [`pack::write_tar`] produces for every regular
    /// file below `dir`, so the value does not depend on the compressor.
    pub fn pack_integrity(dir: &Path) -> Result<Integrity> {
        let files = pack::walk_files(dir)?;
        let entries: Vec<pack::PackEntry> = files
            .into_iter()
            .map(|f| pack::PackEntry {
                source: pack::PackSource::File(dir.join(&f.path)),
                path: f.path,
                mode: f.mode,
            })
            .collect();
        let mut hasher = hash::IntegrityHasher::new(HashAlgorithm::Sha512);
        pack::write_tar(&entries, &mut hasher)?;
        Ok(hasher.finish())
    }

    /// Builds the patched copy of `base` (the store entry of `key`) by applying `patch_text`
    /// (a `git diff` style patch, bun `patch` / patch-package format), stored under
    /// [`Store::patched_package_dir`]. Files the patch does not touch stay hard links to the
    /// base package's files; touched files are copies, so the base package is never modified.
    pub fn import_patched(
        &self,
        key: &PackageKey,
        base: &PackageEntry,
        patch_text: &str,
        patch_hash: u64,
    ) -> Result<PatchedPackage> {
        let patch_err = |message: String| Error::Patch {
            package: key.name.clone(),
            message,
        };
        let parsed = patch::parse_patch_file(patch_text.as_bytes())
            .map_err(|e| patch_err(format!("failed to parse patchfile: {}", e.err)))?;
        let warnings = parsed.skipped_warnings();
        let touched = parsed.touched_paths();
        let staging = self.new_staging()?;
        staging
            .create_dirs(&base.index.dirs)
            .map_err(io_err(staging.package_root()))?;
        for f in &base.index.files {
            let from = base.root.join(&f.path);
            let to = staging.package_root().join(&f.path);
            // Touched files must be private copies; the rest share their blob when possible.
            if touched.contains(f.path.as_str()) || std::fs::hard_link(&from, &to).is_err() {
                fsops::copy_file(&from, &to).map_err(io_err(&from))?;
            }
        }
        parsed
            .apply(staging.package_root())
            .map_err(|e| patch_err(format!("failed applying patch file: {e}")))?;
        // Re-index: untouched files keep their records, the rest is recorded as written.
        let mut records = Vec::with_capacity(base.index.files.len());
        for f in pack::walk_files(staging.package_root())? {
            match base.index.file(&f.path) {
                Some(old) if !touched.contains(f.path.as_str()) => records.push(old.clone()),
                _ => {
                    let full = staging.package_root().join(&f.path);
                    let data = std::fs::read(&full).map_err(io_err(&full))?;
                    let mode = if f.mode & 0o111 != 0 { 0o755 } else { 0o644 };
                    records.push(
                        staging
                            .add_bytes(&f.path, mode, &data)
                            .map_err(io_err(&full))?,
                    );
                }
            }
        }
        let package_json =
            std::fs::read_to_string(staging.package_root().join("package.json")).ok();
        let built = Built {
            files: records,
            package_json,
        };
        let dest = self.patched_package_dir(key, patch_hash);
        let entry = self.publish_staging(
            &staging,
            &dest,
            IndexMeta {
                integrity: Some(key.integrity),
                patch_hash: Some(patch_hash),
            },
            built,
        )?;
        Ok(PatchedPackage { entry, warnings })
    }

    /// Records `project_dir` in `projects.json` (for prune reference counting).
    pub fn register_project(&self, project_dir: &Path) -> Result<()> {
        prune::register_project(self, project_dir, None)
    }

    /// Records the store packages (`pkgs/` directory names, [`PackageKey::dir_name`] or
    /// [`PackageKey::patched_dir_name`]) that `project_dir` installed, replacing the previous
    /// list. Prune keeps every package some live project lists; a project registered without
    /// a list keeps every package.
    pub fn set_project_packages<I, S>(&self, project_dir: &Path, packages: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let list: Vec<String> = packages.into_iter().map(Into::into).collect();
        prune::register_project(self, project_dir, Some(list))
    }

    /// Removes `links/` and `pkgs/` entries no registered project references.
    pub fn prune(&self, opts: PruneOptions) -> Result<PruneReport> {
        prune::prune(self, opts)
    }

    /// `pm cache prune`: removes `pkgs/` entries neither imported nor read for `max_age_days`
    /// (bun default 30). `links/` (the global store) and manifests are kept.
    pub fn prune_older_than(&self, max_age_days: u32, dry_run: bool) -> Result<CachePruneReport> {
        prune::prune_by_age(self, max_age_days, dry_run)
    }

    /// Counts and physical size.
    pub fn status(&self) -> Result<StoreStatus> {
        prune::status(self)
    }
}
