//! yarn berry converter. Owned by the yarn/pnpm migration work.

use fnpm_types::LockGraph;
use fnpm_workspace::Workspace;

use crate::Result;

/// bun `yarn_berry.rs` (+ builtin package extensions). Read-only.
pub fn parse(_bytes: &[u8], _workspace: &Workspace) -> Result<LockGraph> {
    todo!()
}
