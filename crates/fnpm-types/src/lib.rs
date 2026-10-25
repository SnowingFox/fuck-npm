//! Shared data model for fnpm: package names, dependency specifiers, resolutions, integrity,
//! platform constraints, bins, package metadata and the [`LockGraph`].
//!
//! This crate has no I/O and no async. Every other fnpm crate depends on it.

pub mod behavior;
pub mod bin;
pub mod event;
pub mod graph;
pub mod hosted_git;
pub mod integrity;
mod loose_version;
pub mod meta;
pub mod name;
pub mod overrides;
pub mod platform;
pub mod resolution;
pub mod spec;

pub use behavior::{Behavior, DependencyGroup, PeerMeta};
pub use bin::Bin;
pub use event::{Event, EventSink, InstallSummary, NoopSink};
pub use graph::{
    Catalogs, ConfigVersion, DepEdge, GraphError, GraphIndex, LockGraph, NodeLinker, Package,
    PackageExtension, PackageId, PatchedDependency,
};
pub use hosted_git::{HostProvider, HostedGitError, HostedGitInfo, Representation};
pub use integrity::{HashAlgorithm, Integrity, IntegrityError, IntegrityHasher};
pub use meta::{BundledDependencies, PackageMeta, ScriptFlags};
pub use name::{NameError, PackageName};
pub use overrides::{OverrideRule, OverrideSelector, PackageSelector, SelectorError};
pub use platform::{CpuSet, LibcSet, OsSet, Platform, PlatformConstraints, PlatformError};
pub use resolution::{Resolution, ResolutionTag};
pub use spec::{
    Committish, Dependency, DependencySpec, GitSpec, GithubSpec, SpecError, SpecTag, TarballUri,
    WorkspaceSpec, infer_tag,
};

/// Any error this crate returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(transparent)]
    Spec(#[from] SpecError),
    #[error(transparent)]
    Integrity(#[from] IntegrityError),
    #[error(transparent)]
    HostedGit(#[from] HostedGitError),
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[error(transparent)]
    Selector(#[from] SelectorError),
    #[error(transparent)]
    Graph(#[from] GraphError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `std::thread::available_parallelism()` (at least 1), computed once per process: on Linux
/// every std call reads `/proc/self/cgroup` and the cgroup CPU quota files (about 40 syscalls,
/// 50-130 us), and the install path asks from several crates.
pub fn available_parallelism() -> usize {
    static CPUS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CPUS.get_or_init(|| std::thread::available_parallelism().map_or(1, usize::from))
}
