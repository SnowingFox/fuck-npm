//! The no-op fast path: `node_modules/.fnpm/state`.
//!
//! If the lockfile, every workspace package.json, the effective config and the
//! `node_modules` directory are unchanged since the last successful install, `fnpm install`
//! exits after a handful of `stat`s and reads. Runs before any config parsing beyond the
//! cwd lookup.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub const STATE_FILE: &str = "node_modules/.fnpm/state";
pub const STATE_VERSION: u32 = 1;

/// Written last by a successful install; JSON (small, debuggable).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub version: u32,
    pub fnpm_version: String,
    /// xxh3 of fnpm.lock bytes.
    pub lockfile_hash: u64,
    /// `(path relative to root, size, mtime ns, xxh3 of contents)` per package.json.
    pub manifests: Vec<(String, u64, i128, u64)>,
    /// xxh3 over the serialized install-relevant config (linker, backend, omit, platform,
    /// registry URLs, ignoreScripts, production).
    pub config_hash: u64,
    /// `(dev, ino, mtime ns)` of `node_modules` after the install finished.
    pub node_modules: (u64, u64, i128),
    pub linker: String,
    pub packages: u32,
}

pub enum Check {
    /// Nothing changed; report `checked` counts.
    Noop { packages: u32 },
    /// Something changed; carry the parsed state so the install can reuse the hashes.
    Changed(Option<State>),
}

/// Compares `STATE_FILE` with the current files. Package.json contents are only hashed when
/// size or mtime differ.
pub fn check(_project_dir: &Path, _config_hash: u64) -> Check {
    todo!()
}

pub fn write(_project_dir: &Path, _state: &State) -> std::io::Result<()> {
    todo!()
}
