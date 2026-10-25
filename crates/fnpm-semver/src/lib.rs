//! npm-compatible semver versions and ranges.
//!
//! Port of `bun/src/semver/` (`Version.rs`, `SemverRange.rs`, `SemverQuery.rs`,
//! `intersects.rs`) with node-semver semantics: loose parsing (`v1`, `=1.2.3`, `1.0.0rc.1`),
//! x-ranges, hyphen ranges, `||`, `^`/`~` desugaring and the prerelease rule (a prerelease only
//! satisfies a comparator set that has a prerelease on the same `major.minor.patch`).
//! See `docs/ARCHITECTURE.md#fnpm-semver`.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid version {0:?}")]
    InvalidVersion(String),
    #[error("invalid range {0:?}")]
    InvalidRange(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A prerelease identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Identifier {
    Numeric(u64),
    AlphaNumeric(Box<str>),
}

/// A full semver version. Ordering is semver precedence; build metadata is ignored by
/// `cmp` but kept for display.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<Identifier>,
    pub build: Box<str>,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self { major, minor, patch, pre: Vec::new(), build: Box::default() }
    }

    /// Strict-ish npm parse: optional leading `v`/`=` and whitespace, three parts required.
    pub fn parse(_s: &str) -> Result<Self> {
        todo!("port bun Version::parse + parse_exact")
    }

    /// npm `loose` mode: also `1.0.0rc.1`, `1.2.3beta`.
    pub fn parse_loose(_s: &str) -> Result<Self> {
        todo!()
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// `pm version` / `npm version` increments.
    pub fn inc(&self, _release: ReleaseType, _preid: Option<&str>) -> Self {
        todo!("port node-semver inc")
    }
}

impl Ord for Version {
    fn cmp(&self, _other: &Self) -> Ordering {
        todo!("semver precedence")
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        todo!()
    }
}

impl std::str::FromStr for Version {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseType {
    Major,
    Minor,
    Patch,
    Premajor,
    Preminor,
    Prepatch,
    Prerelease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Comparator {
    pub op: Op,
    pub version: Version,
}

impl Comparator {
    pub fn matches(&self, _v: &Version) -> bool {
        todo!()
    }
}

/// A range: `||`-separated comparator sets, each an AND of comparators. `*` / `""` is the
/// single empty set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Range {
    pub sets: Vec<Vec<Comparator>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SatisfyOptions {
    /// npm `includePrerelease`.
    pub include_prerelease: bool,
}

impl Range {
    /// Parses any npm range: `^1.2`, `~1`, `1.x`, `>=1 <2`, `1 - 2`, `1 || 2`, `*`, `""`,
    /// `latest` is not a range (dist tags are handled by the caller).
    pub fn parse(_s: &str) -> Result<Self> {
        todo!("port bun SemverQuery::parse")
    }

    pub fn any() -> Self {
        Self { sets: vec![Vec::new()] }
    }

    pub fn is_any(&self) -> bool {
        self.sets.iter().any(Vec::is_empty)
    }

    /// `Some` when the range is one exact version (`1.2.3`, `=1.2.3`): the resolver's fast path.
    pub fn as_exact(&self) -> Option<&Version> {
        match self.sets.as_slice() {
            [set] => match set.as_slice() {
                [Comparator { op: Op::Eq, version }] => Some(version),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn satisfies(&self, v: &Version) -> bool {
        self.satisfies_with(v, SatisfyOptions::default())
    }

    pub fn satisfies_with(&self, _v: &Version, _opts: SatisfyOptions) -> bool {
        todo!()
    }

    /// node-semver `intersects` (bun `intersects.rs`), used by override and peer matching.
    pub fn intersects(&self, _other: &Range) -> bool {
        todo!()
    }

    /// The lowest version that can satisfy the range (`minVersion`).
    pub fn min_version(&self) -> Option<Version> {
        todo!()
    }
}

impl fmt::Display for Range {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        todo!("normalized form")
    }
}

impl std::str::FromStr for Range {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// The highest version in `versions` that satisfies `range`.
pub fn max_satisfying<'a, I>(versions: I, range: &Range, opts: SatisfyOptions) -> Option<&'a Version>
where
    I: IntoIterator<Item = &'a Version>,
{
    versions.into_iter().filter(|v| range.satisfies_with(v, opts)).max()
}

/// Shorthand: parse both and test. Invalid input is `false`.
pub fn satisfies(version: &str, range: &str) -> bool {
    match (Version::parse_loose(version), Range::parse(range)) {
        (Ok(v), Ok(r)) => r.satisfies(&v),
        _ => false,
    }
}
