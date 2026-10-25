//! `node_modules/.fnpm/lock.bin`: an rkyv image of the [`LockGraph`] keyed by the lockfile's
//! xxh3 hash, so a warm install skips JSONC parsing.
//!
//! The archived type is a private mirror of `LockGraph` (string table + flat arrays) owned by
//! this module; `fnpm-types` stays serde-only.

use std::path::Path;

use fnpm_types::LockGraph;

use crate::Result;

pub const SIDECAR: &str = "node_modules/.fnpm/lock.bin";

/// `Some` only when the stored key equals `lock_hash` and the format version matches.
pub fn load(_project_dir: &Path, _lock_hash: u64) -> Option<LockGraph> {
    todo!()
}

pub fn save(_project_dir: &Path, _lock_hash: u64, _graph: &LockGraph) -> Result<()> {
    todo!()
}
