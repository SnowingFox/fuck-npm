//! Who may run lifecycle scripts (bun `lockfile.rs:3620-3860`, `PackageInstaller.rs`).

use std::collections::BTreeSet;

use fnpm_types::{LockGraph, PackageId};

/// Built-in list: `bun/src/install/default-trusted-dependencies.txt`, copied to
/// `crates/fnpm-scripts/data/default-trusted-dependencies.txt` and `include_str!`ed.
pub fn default_trusted() -> &'static [&'static str] {
    todo!()
}

pub struct TrustPolicy {
    /// `None`: the default list applies (npm resolutions from the default registry only).
    pub explicit: Option<BTreeSet<String>>,
    /// `--trust` / `fnpm pm trust` names for this run.
    pub extra: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub package: PackageId,
    pub name: String,
    pub version: String,
    pub scripts: Vec<String>,
}

impl TrustPolicy {
    pub fn from_graph(graph: &LockGraph, extra: impl IntoIterator<Item = String>) -> Self {
        Self { explicit: graph.trusted_dependencies.clone(), extra: extra.into_iter().collect() }
    }

    /// Root and workspaces always run their own scripts. Others: by real name for npm
    /// resolutions; by alias for non-npm resolutions only when the root or a workspace
    /// declares that exact resolution; `file:` folders inside their declarer are never trusted.
    pub fn is_trusted(&self, _graph: &LockGraph, _id: PackageId) -> bool {
        todo!()
    }
}
