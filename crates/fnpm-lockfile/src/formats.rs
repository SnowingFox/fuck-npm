//! One module per lockfile format. Every module exposes `parse` and, where writable,
//! `serialize`, both pure functions over bytes / strings.

pub mod fnpm {
    use fnpm_types::LockGraph;

    use crate::Result;

    /// `fnpm.lock`: bun.lock's text format (JSONC, trailing commas, 2-space indent) with
    /// fnpm's version stamp. See ARCHITECTURE.md for the full schema and example.
    pub fn parse(_bytes: &[u8]) -> Result<LockGraph> {
        todo!()
    }

    /// Deterministic: same graph, same bytes.
    pub fn serialize(_graph: &LockGraph) -> String {
        todo!()
    }
}

/// `bun.lock` (lockfileVersion 0-3 text, bun `lockfile/bun.lock.rs`).
pub mod bun_text;

/// `bun.lockb` (bun `lockfile/bun.lockb.rs`, `Buffers.rs`). Read-only.
pub mod bun_binary;

/// package-lock.json / npm-shrinkwrap.json (bun `migration/npm_lock.rs`).
pub mod npm;

pub mod yarn_v1 {
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
}

pub mod yarn_berry {
    use fnpm_types::LockGraph;
    use fnpm_workspace::Workspace;

    use crate::Result;

    /// bun `yarn_berry.rs` (+ builtin package extensions). Read-only.
    pub fn parse(_bytes: &[u8], _workspace: &Workspace) -> Result<LockGraph> {
        todo!()
    }
}

pub mod pnpm {
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
}
