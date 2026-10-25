//! Content-addressable store, streaming tarball extraction, package file index, filesystem
//! materialisation primitives and prune. See `docs/ARCHITECTURE.md#fnpm-store`.
//!
//! Layout under `Config::store_dir` (`~/.fnpm/store/v1`):
//! ```text
//! files/<2 hex>/<sha512 hex>[-x]          one blob per unique file content (-x: executable)
//! pkgs/<name+escaped>@<version>-<integrity hex16>/
//!     package/...                          hardlinks to blobs (the package root)
//!     index.bin                            rkyv FileIndex
//! links/<store key>-<closure hash16>/node_modules/<name>   global virtual store (isolated)
//! tmp/                                     staging, same filesystem, renamed into place
//! projects.json                            projects using this store (for prune)
//! ```

pub mod fsops;
pub mod index;
pub mod tar;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use fnpm_types::Integrity;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

pub use fsops::{Backend, MaterializeError};
pub use index::{FileIndex, IndexedFile};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("integrity check failed for {package}: expected {expected}, got {actual}")]
    Integrity { package: String, expected: String, actual: String },
    #[error("{package}: invalid tarball: {message}")]
    InvalidTarball { package: String, message: String },
    #[error("{package}: tarball entry {path:?} escapes the package")]
    PathTraversal { package: String, path: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Identifies a package directory in the store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackageKey {
    pub name: String,
    pub version: String,
    /// The tarball integrity. For git/folder imports, the integrity computed over the
    /// packed directory.
    pub integrity: Integrity,
}

impl PackageKey {
    /// `pkgs/` directory name: `@scope+name@1.2.3-<first 16 hex of digest>`.
    pub fn dir_name(&self) -> String {
        let name = self.name.replace('/', "+");
        let hex = self.integrity.to_hex();
        format!("{name}@{}-{}", self.version, &hex[..16.min(hex.len())])
    }
}

/// A complete package directory in the store.
#[derive(Debug, Clone)]
pub struct PackageEntry {
    /// `pkgs/<dir>/package`: the directory to clone/link from.
    pub root: PathBuf,
    pub index: Arc<FileIndex>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    /// `--no-verify`: skip the integrity comparison (hashing still happens for the CAS).
    pub skip_verify: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneOptions {
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    pub removed_links: u64,
    pub removed_packages: u64,
    pub removed_files: u64,
    pub bytes_freed: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreStatus {
    pub path: PathBuf,
    pub packages: u64,
    pub files: u64,
    pub links: u64,
    /// Physical bytes of `files/` (hardlinks counted once).
    pub bytes: u64,
}

/// A handle to the store. `Clone` + `Send` + `Sync`; all methods are safe to call
/// concurrently from many threads and processes (atomic renames, EEXIST means another writer
/// won).
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Creates the directory skeleton if needed.
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self { root: root.to_path_buf() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn links_dir(&self) -> PathBuf {
        self.root.join("links")
    }

    pub fn package_dir(&self, key: &PackageKey) -> PathBuf {
        self.root.join("pkgs").join(key.dir_name())
    }

    /// One `stat` of `index.bin`; loads the index if present.
    pub fn get(&self, _key: &PackageKey) -> Result<Option<PackageEntry>> {
        todo!()
    }

    /// Streams `gzip -> tar -> CAS` in one pass while hashing the compressed bytes with the
    /// key's algorithm; verifies, writes `index.bin`, renames the staged directory into
    /// `pkgs/`. Blocking work runs on rayon / `spawn_blocking`; the future only awaits network
    /// chunks.
    pub async fn import_tarball<S>(&self, _key: &PackageKey, _body: S, _opts: ImportOptions) -> Result<PackageEntry>
    where
        S: Stream<Item = io::Result<Bytes>> + Send + Unpin,
    {
        todo!()
    }

    /// The whole compressed tarball is in memory: libdeflate one-shot, then the same path.
    pub fn import_tarball_bytes(&self, _key: &PackageKey, _tgz: &[u8], _opts: ImportOptions) -> Result<PackageEntry> {
        todo!()
    }

    /// A directory (git checkout, `file:` folder for `pack`) imported as a package; the key's
    /// integrity is computed by the caller with [`Store::pack_integrity`].
    pub fn import_directory(&self, _key: &PackageKey, _src: &Path) -> Result<PackageEntry> {
        todo!()
    }

    /// The integrity of a directory as fnpm packs it (sorted entries, deterministic tar).
    pub fn pack_integrity(_dir: &Path) -> Result<Integrity> {
        todo!()
    }

    /// Records `project_dir` in `projects.json` (for prune reference counting).
    pub fn register_project(&self, _project_dir: &Path) -> Result<()> {
        todo!()
    }

    /// Removes `links/` and `pkgs/` entries no registered project references, then blobs
    /// with link count 1.
    pub fn prune(&self, _opts: PruneOptions) -> Result<PruneReport> {
        todo!()
    }

    pub fn status(&self) -> Result<StoreStatus> {
        todo!()
    }
}
