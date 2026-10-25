//! `bun.lockb` reader (migration). Owned by the bun/npm migration work.

use fnpm_types::LockGraph;

use crate::Result;

/// `bun.lockb` (bun `lockfile/bun.lockb.rs`, `Buffers.rs`). Read-only.
pub fn parse(_bytes: &[u8]) -> Result<LockGraph> {
    todo!()
}
