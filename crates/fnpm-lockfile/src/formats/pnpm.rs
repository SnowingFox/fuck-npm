//! pnpm-lock.yaml converter. Owned by the yarn/pnpm migration work.

use fnpm_types::LockGraph;
use fnpm_workspace::Workspace;

use crate::Result;

/// pnpm-lock.yaml v5.4 / v6 / v9 (bun `pnpm.rs`), including catalogs, overrides,
/// patchedDependencies and onlyBuiltDependencies from the lockfile settings.
pub fn parse(_bytes: &[u8], _workspace: &Workspace) -> Result<LockGraph> {
    todo!()
}

/// lockfileVersion 9.0.
pub fn serialize(_graph: &LockGraph) -> Result<String> {
    todo!()
}
