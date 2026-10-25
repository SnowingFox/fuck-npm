//! npm-compatible semver versions and ranges.
//!
//! Port of `bun/src/semver/` (`Version.rs`, `SemverRange.rs`, `SemverQuery.rs`,
//! `intersects.rs`) with node-semver semantics: loose parsing (`v1`, `=1.2.3`, `1.0.0rc.1`),
//! x-ranges, hyphen ranges, `||`, `^`/`~` desugaring and the prerelease rule (a prerelease only
//! satisfies a comparator set that has a prerelease on the same `major.minor.patch`).
//! See `docs/ARCHITECTURE.md#fnpm-semver`.
//!
//! Parsing scans the input without allocating; only prerelease identifiers, build metadata
//! and the comparator vectors of a [`Range`] are allocated. [`Range::satisfies`] and
//! [`Version`] comparisons never allocate.

mod intersect;
mod pick;
mod range;
mod scan;
mod serde_impl;
mod version;

pub use pick::{best_version, max_satisfying, max_satisfying_sorted, pick_best};
pub use range::{Comparator, Op, ParseInfo, Range, SatisfyOptions};
pub use version::{ChangedVersion, Identifier, PinnedVersion, ReleaseType, Version};

/// Errors from parsing versions and ranges.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The text is not a version (or an increment argument is invalid).
    #[error("invalid version {0:?}")]
    InvalidVersion(String),
    /// The text is not a range npm accepts ([`Range::parse_strict`]).
    #[error("invalid range {0:?}")]
    InvalidRange(String),
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `Bun.semver.satisfies(version, range)`: parse both and test. A version that is not exactly
/// one version (see [`Version::parse_loose`]) or non-ASCII input is `false`; a range of only
/// unknown words (`latest`) matches every release, as in bun.
pub fn satisfies(version: &str, range: &str) -> bool {
    if !version.is_ascii() || !range.is_ascii() {
        return false;
    }
    match (Version::parse_loose(version), Range::parse(range)) {
        (Ok(v), Ok(r)) => r.satisfies(&v),
        _ => false,
    }
}

/// `Bun.semver.order(a, b)`: compares two versions by precedence, ignoring build metadata.
///
/// Like bun, a partial version or wildcard is accepted and its missing parts are `u64::MAX`
/// (`1.x` sorts after every `1.y.z`), text after the version is ignored, and non-ASCII input
/// compares equal. Fails when either side is not a version (`^1.0.0`, `2x`, `1.0.<above u64>`).
pub fn order(a: &str, b: &str) -> Result<std::cmp::Ordering> {
    if !a.is_ascii() || !b.is_ascii() {
        return Ok(std::cmp::Ordering::Equal);
    }
    Ok(Version::scan_max(a)?.cmp_precedence(&Version::scan_max(b)?))
}
