//! package-lock.json / npm-shrinkwrap.json converter. Owned by the npm migration work.

use fnpm_types::LockGraph;
use fnpm_workspace::Workspace;

use crate::Result;

/// package-lock.json / npm-shrinkwrap.json v1-v3 (bun `migration/npm_lock.rs`). Workspace
/// entries (`link: true`) map to workspace resolutions.
pub fn parse(_bytes: &[u8], _workspace: &Workspace) -> Result<LockGraph> {
    todo!()
}

/// lockfileVersion 3.
pub fn serialize(_graph: &LockGraph) -> Result<String> {
    todo!()
}
