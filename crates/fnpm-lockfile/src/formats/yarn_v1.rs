//! yarn.lock v1 converter. Owned by the yarn/pnpm migration work; the writer can use
//! [`crate::print::yarn_v1`] (bun's `printer/Yarn.rs` port).

use fnpm_types::LockGraph;
use fnpm_workspace::Workspace;

use crate::Result;

/// bun `yarn.rs`.
pub fn parse(_bytes: &[u8], _workspace: &Workspace) -> Result<LockGraph> {
    todo!()
}

pub fn serialize(_graph: &LockGraph) -> String {
    todo!()
}
