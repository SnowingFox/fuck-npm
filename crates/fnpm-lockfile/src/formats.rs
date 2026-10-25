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

pub mod bun_text {
    use fnpm_types::LockGraph;

    use crate::Result;

    /// `bun.lock` (lockfileVersion 0-2 text, bun `lockfile/bun.lock.rs`).
    pub fn parse(_bytes: &[u8]) -> Result<LockGraph> {
        todo!()
    }

    pub fn serialize(_graph: &LockGraph) -> String {
        todo!()
    }
}

pub mod bun_binary {
    use fnpm_types::LockGraph;

    use crate::Result;

    /// `bun.lockb` (bun `lockfile/bun.lockb.rs`, `Buffers.rs`). Read-only.
    pub fn parse(_bytes: &[u8]) -> Result<LockGraph> {
        todo!()
    }
}

pub mod npm {
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
}

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
