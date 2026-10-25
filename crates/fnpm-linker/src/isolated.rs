//! The isolated layout (bun `isolated_install.rs`, `isolated_install/Store.rs`):
//!
//! ```text
//! node_modules/<name>                      -> .fnpm/<key>/node_modules/<name>
//! node_modules/.fnpm/<key>/node_modules/<name>/...   the package (clone/hardlink of the store)
//! node_modules/.fnpm/<key>/node_modules/<dep>        -> ../../<dep key>/node_modules/<dep>
//! node_modules/.fnpm/node_modules/<name>   hidden hoist (install.hoist / hoistPattern)
//! ```
//! With the global virtual store, `.fnpm/<key>` is a symlink to
//! `<store>/links/<key>-<closure hash16>`.

use fnpm_types::{LockGraph, PackageId};

/// One `node_modules/.fnpm/<key>` entry: a package in one peer context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub package: PackageId,
    /// `name@resolution` escaped (`/`, `\`, `:`, `#`, `?` -> `+`), resolution part capped at
    /// 80 bytes (63 + `+` + 16 hex wyhash/xxh3), plus `+<peer hash16>` when peers differ.
    pub key: String,
    /// `(alias, entry index)` for each resolved dependency.
    pub deps: Vec<(String, u32)>,
    /// The closure hash for the global virtual store; `None` when ineligible (trusted,
    /// patched, local, or depends on an ineligible entry).
    pub global_hash: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IsolatedLayout {
    pub entries: Vec<Entry>,
    /// Root and each workspace: `(package id, [(alias, entry index)])` linked into its
    /// `node_modules`.
    pub roots: Vec<(PackageId, Vec<(String, u32)>)>,
    /// Names hoisted into `node_modules/.fnpm/node_modules`.
    pub hidden_hoist: Vec<(String, u32)>,
    /// Names hoisted into the root `node_modules` (`publicHoistPattern`).
    pub public_hoist: Vec<(String, u32)>,
}

pub struct PlanOptions<'a> {
    pub hoist: bool,
    pub hoist_pattern: &'a [String],
    pub public_hoist_pattern: &'a [String],
    pub global_store: bool,
    /// Packages whose scripts will run (never shared through the global store).
    pub trusted: &'a dyn Fn(PackageId) -> bool,
}

pub fn plan(_graph: &LockGraph, _opts: &PlanOptions<'_>) -> IsolatedLayout {
    todo!()
}
