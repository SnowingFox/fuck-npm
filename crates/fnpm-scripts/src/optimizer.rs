//! Postinstall optimizer (bun `postinstall_optimizer.rs`).
//!
//! `nativeDependencies` (default: esbuild, @anthropic-ai/claude-code): skip the trusted
//! package's scripts when one of its optional dependencies with explicit os+cpu matches the
//! target, and point its bins into that platform package. `ignoreScripts` (default: sharp >=
//! 0.33.0): never run its scripts. A present array in the root package.json replaces the
//! default.

use fnpm_types::{LockGraph, PackageId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run,
    Skip,
    /// Skip scripts and redirect bins to the platform package.
    NativeBinlink { platform_package: PackageId },
}

pub struct Optimizer {
    pub native_dependencies: Vec<String>,
    pub ignore_scripts: Vec<String>,
}

impl Optimizer {
    /// From the root package.json `nativeDependencies` / `ignoreScripts` (defaults when
    /// absent). `BUN_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER` /
    /// `..._DISABLE_IGNORE_SCRIPTS` (and `FNPM_` equivalents) disable each half.
    pub fn from_root(_root: &serde_json::Value, _env: &fnpm_config::Env) -> Self {
        todo!()
    }

    pub fn decide(&self, _graph: &LockGraph, _id: PackageId, _platform: &fnpm_types::Platform) -> Decision {
        todo!()
    }
}
