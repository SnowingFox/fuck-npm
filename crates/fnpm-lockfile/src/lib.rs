//! Lockfiles: `fnpm.lock` read/write, the binary sidecar, the hoisted tree algorithm, and
//! converters from/to bun.lock, bun.lockb, package-lock.json / npm-shrinkwrap.json (v1-v3),
//! yarn.lock v1, yarn berry and pnpm-lock.yaml (v5.4-v9).
//! See `docs/ARCHITECTURE.md#fnpm-lockfile`.

pub mod formats;
pub mod migration;
pub mod sidecar;
pub mod tree;

use std::path::{Path, PathBuf};

use fnpm_types::LockGraph;
use fnpm_workspace::Workspace;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{format:?} lockfiles cannot be written")]
    Unsupported { format: Format },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub const FNPM_LOCK: &str = "fnpm.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Fnpm,
    BunText,
    BunBinary,
    /// package-lock.json or npm-shrinkwrap.json.
    Npm,
    YarnV1,
    YarnBerry,
    Pnpm,
}

impl Format {
    /// File names in load priority order (bun `migration.rs`).
    pub const PRIORITY: [(Format, &'static str); 8] = [
        (Format::Fnpm, "fnpm.lock"),
        (Format::BunText, "bun.lock"),
        (Format::BunBinary, "bun.lockb"),
        (Format::Npm, "npm-shrinkwrap.json"),
        (Format::Npm, "package-lock.json"),
        (Format::YarnV1, "yarn.lock"),
        (Format::YarnBerry, "yarn.lock"),
        (Format::Pnpm, "pnpm-lock.yaml"),
    ];
}

/// The result of [`load`].
#[derive(Debug, Clone)]
pub struct Loaded {
    pub graph: LockGraph,
    pub format: Format,
    pub path: PathBuf,
    /// Not fnpm.lock: the graph came from another tool and the file is left in place.
    pub migrated: bool,
    /// xxh3-64 of the file bytes (sidecar and state-file key).
    pub hash: u64,
}

/// Finds and reads the highest-priority lockfile in `dir`. Uses the sidecar when its key
/// matches the file hash. `yarn.lock` is sniffed for v1 vs berry (`__metadata`).
pub fn load(_dir: &Path, _workspace: &Workspace) -> Result<Option<Loaded>> {
    todo!()
}

/// Parses a lockfile of a known format. Converters keep resolved versions exactly; data a
/// format lacks (bin, os/cpu, hasInstallScript) is left default and filled from the store's
/// package.json during install, with no manifest fetch.
pub fn parse(format: Format, bytes: &[u8], workspace: &Workspace) -> Result<LockGraph> {
    match format {
        Format::Fnpm => formats::fnpm::parse(bytes),
        Format::BunText => formats::bun_text::parse(bytes),
        Format::BunBinary => formats::bun_binary::parse(bytes),
        Format::Npm => formats::npm::parse(bytes, workspace),
        Format::YarnV1 => formats::yarn_v1::parse(bytes, workspace),
        Format::YarnBerry => formats::yarn_berry::parse(bytes, workspace),
        Format::Pnpm => formats::pnpm::parse(bytes, workspace),
    }
}

/// Serialises to a format (`fnpm-lock export`). bun.lockb and yarn berry are read-only.
pub fn serialize(format: Format, graph: &LockGraph) -> Result<Vec<u8>> {
    match format {
        Format::Fnpm => Ok(formats::fnpm::serialize(graph).into_bytes()),
        Format::BunText => Ok(formats::bun_text::serialize(graph).into_bytes()),
        Format::Npm => formats::npm::serialize(graph).map(String::into_bytes),
        Format::YarnV1 => Ok(formats::yarn_v1::serialize(graph).into_bytes()),
        Format::Pnpm => formats::pnpm::serialize(graph).map(String::into_bytes),
        Format::BunBinary | Format::YarnBerry => Err(Error::Unsupported { format }),
    }
}

/// Writes `fnpm.lock` atomically (temp + rename) and refreshes the sidecar. Returns the new
/// file hash. Skips the write when the bytes are unchanged.
pub fn save(_dir: &Path, _graph: &LockGraph) -> Result<u64> {
    todo!()
}

/// Why a lockfile is out of date with the workspace's package.json files (frozen check,
/// `fnpm-lock verify`). Empty means in sync.
pub fn out_of_sync(_graph: &LockGraph, _workspace: &Workspace) -> Vec<String> {
    todo!()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// `(name, from, to)`
    pub changed: Vec<(String, String, String)>,
}

/// `fnpm-lock diff`.
pub fn diff(_a: &LockGraph, _b: &LockGraph) -> LockDiff {
    todo!()
}
