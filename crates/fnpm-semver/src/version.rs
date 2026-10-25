//! [`Version`] and [`Identifier`]: parsing, precedence, formatting and increments.
//!
//! Port of bun `semver/Version.rs` plus `runtime/cli/pm_version_command.rs` (`inc`).

use std::cmp::Ordering;
use std::fmt;

use crate::scan::{self, Wildcard};
use crate::{Error, Result};

/// The largest number node-semver accepts in a version (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// A prerelease identifier.
///
/// `Numeric` is only produced for canonical numbers (`0`, or digits without a leading zero that
/// fit in `u64`), so formatting round-trips. A digit string like `01` is kept as `AlphaNumeric`
/// but still compares numerically, as in node-semver and bun.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Identifier {
    /// A numeric identifier such as `1` in `1.0.0-rc.1`.
    Numeric(u64),
    /// Any other identifier such as `rc` in `1.0.0-rc.1`.
    AlphaNumeric(Box<str>),
}

impl Identifier {
    /// Builds an identifier from its text, choosing `Numeric` for canonical numbers.
    pub fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        if !bytes.is_empty()
            && (bytes[0] != b'0' || bytes.len() == 1)
            && let Some(n) = scan::parse_u64(bytes)
        {
            return Identifier::Numeric(n);
        }
        Identifier::AlphaNumeric(text.into())
    }

    /// The numeric value used for precedence: `Some` for a numeric identifier and for an
    /// all-digit alphanumeric one (`01`) that fits in `u64`.
    #[inline]
    fn number(&self) -> Option<u64> {
        match self {
            Identifier::Numeric(n) => Some(*n),
            Identifier::AlphaNumeric(s) => match s.as_bytes().first() {
                Some(b'0'..=b'9') => scan::parse_u64(s.as_bytes()),
                _ => None,
            },
        }
    }

    #[inline]
    fn as_text(&self) -> Option<&str> {
        match self {
            Identifier::Numeric(_) => None,
            Identifier::AlphaNumeric(s) => Some(s),
        }
    }

    /// Semver precedence: numbers compare numerically and sort before strings, which compare
    /// by bytes.
    #[inline]
    pub fn cmp_precedence(&self, other: &Self) -> Ordering {
        if let (Identifier::Numeric(a), Identifier::Numeric(b)) = (self, other) {
            return a.cmp(b);
        }
        match (self.number(), other.number()) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => {
                let a = self.as_text().unwrap_or_default();
                let b = other.as_text().unwrap_or_default();
                a.as_bytes().cmp(b.as_bytes())
            }
        }
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Identifier::Numeric(n) => write!(f, "{n}"),
            Identifier::AlphaNumeric(s) => f.write_str(s),
        }
    }
}

/// Compares two prerelease lists by semver precedence (an empty list is *not* special here:
/// callers handle "no prerelease sorts after any prerelease").
#[inline]
pub(crate) fn cmp_pre(a: &[Identifier], b: &[Identifier]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        match x.cmp_precedence(y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// A full semver version.
///
/// [`Ord`] is bun's `Version::order`: semver precedence first, then build metadata by bytes
/// (so sorting is total and agrees with `==`). Use [`Version::cmp_precedence`] for node-semver
/// `compare`, which ignores build metadata; ranges always use precedence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    /// Major number.
    pub major: u64,
    /// Minor number.
    pub minor: u64,
    /// Patch number.
    pub patch: u64,
    /// Prerelease identifiers (`rc.1` is `[rc, 1]`); empty for a release.
    pub pre: Vec<Identifier>,
    /// Build metadata without the `+`; empty when absent.
    pub build: Box<str>,
}

/// Which release of a version a pinned spec allows to change, for `bun update` / `outdated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedVersion {
    /// `^`-like: minor and patch may change.
    Major,
    /// `~`-like: patch may change.
    Minor,
    /// Exact.
    Patch,
}

/// The most significant part that differs between two versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangedVersion {
    /// Major numbers differ.
    Major,
    /// Minor numbers differ.
    Minor,
    /// Patch numbers differ.
    Patch,
    /// Prereleases differ.
    Pre,
    /// Build metadata differs.
    Build,
}

/// `pm version` / `npm version` increments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseType {
    /// `1.2.3` -> `2.0.0`; `2.0.0-rc.1` -> `2.0.0`.
    Major,
    /// `1.2.3` -> `1.3.0`; `1.3.0-rc.1` -> `1.3.0`.
    Minor,
    /// `1.2.3` -> `1.2.4`; `1.2.4-rc.1` -> `1.2.4`.
    Patch,
    /// `1.2.3` -> `2.0.0-0`.
    Premajor,
    /// `1.2.3` -> `1.3.0-0`.
    Preminor,
    /// `1.2.3` -> `1.2.4-0`.
    Prepatch,
    /// `1.2.3` -> `1.2.4-0`; `1.2.4-rc.1` -> `1.2.4-rc.2`.
    Prerelease,
}

/// Converts scanned prerelease text into identifiers.
pub(crate) fn identifiers(pre: &[u8]) -> Vec<Identifier> {
    if pre.is_empty() {
        return Vec::new();
    }
    pre.split(|c| *c == b'.')
        .map(|part| Identifier::new(ascii(part)))
        .collect()
}

/// Scanned tags only contain `[0-9A-Za-z.+-]`, so they are always valid UTF-8.
#[inline]
pub(crate) fn ascii(bytes: &[u8]) -> &str {
    if bytes.is_empty() {
        return "";
    }
    std::str::from_utf8(bytes).unwrap_or_default()
}

impl Version {
    /// `major.minor.patch` with no prerelease or build metadata.
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: Vec::new(),
            build: Box::default(),
        }
    }

    /// Builds a version from scanned parts; missing numbers are 0.
    pub(crate) fn from_partial(p: &scan::Partial<'_>) -> Self {
        Self {
            major: p.major.unwrap_or(0),
            minor: p.minor.unwrap_or(0),
            patch: p.patch.unwrap_or(0),
            pre: identifiers(p.tag.pre),
            build: ascii(p.tag.build).into(),
        }
    }

    /// Scans `s` as one complete version, apart from surrounding whitespace (bun
    /// `Version::parse_exact`).
    fn scan_exact(s: &str) -> Option<scan::Scan<'_>> {
        let trimmed = scan::trim_ws(s.as_bytes());
        let scanned = scan::scan_version(trimmed);
        (scanned.valid && scanned.wildcard == Wildcard::None && scanned.len == trimmed.len())
            .then_some(scanned)
    }

    /// Parses a version the way npm validates a published version: surrounding whitespace, an
    /// optional `=` and an optional `v`, three numbers without leading zeros, and a `-`-introduced prerelease
    /// whose numeric identifiers have no leading zeros; at most 256 bytes and numbers up to
    /// `Number.MAX_SAFE_INTEGER`, like node-semver. `1.0.0rc1` and `01.2.3` are rejected; use
    /// [`Version::parse_loose`] for those.
    pub fn parse(s: &str) -> Result<Self> {
        const MAX_LENGTH: usize = 256;
        let invalid = || Error::InvalidVersion(s.to_owned());
        if s.len() > MAX_LENGTH {
            return Err(invalid());
        }
        let scanned = Self::scan_exact(s).ok_or_else(invalid)?;
        if scanned.implicit_pre {
            return Err(invalid());
        }
        // At most one `=` and one `v` before the numbers.
        let trimmed = scan::trim_ws(s.as_bytes());
        let rest = trimmed
            .strip_prefix(b"=")
            .map_or(trimmed, |r| &r[scan::leading_ws(r)..]);
        let rest = rest.strip_prefix(b"v").unwrap_or(rest);
        if !rest.first().is_some_and(u8::is_ascii_digit) {
            return Err(invalid());
        }
        let version = Self::from_partial(&scanned.version);
        if version.major.max(version.minor).max(version.patch) > MAX_SAFE_INTEGER {
            return Err(invalid());
        }
        let core = rest
            .split(|c| matches!(c, b'-' | b'+'))
            .next()
            .unwrap_or_default();
        let leading_zero = |part: &[u8]| part.len() > 1 && part[0] == b'0';
        if core.split(|c| *c == b'.').any(leading_zero) {
            return Err(invalid());
        }
        let tag = scanned.version.tag;
        let pre_ok = tag.pre.is_empty()
            || tag.pre.split(|c| *c == b'.').all(|id| {
                !id.is_empty() && !(id.iter().all(u8::is_ascii_digit) && leading_zero(id))
            });
        let build_ok =
            tag.build.is_empty() || tag.build.split(|c| *c == b'.').all(|id| !id.is_empty());
        if !pre_ok || !build_ok {
            return Err(invalid());
        }
        Ok(version)
    }

    /// Parses a version the way bun reads versions from manifests and `Bun.semver`: also
    /// `1.0.0rc.1`, `1.2.3beta`, leading zeros, and any amount of leading `v`, `=` and
    /// whitespace. Numbers may be up to `u64::MAX`.
    pub fn parse_loose(s: &str) -> Result<Self> {
        Self::scan_exact(s)
            .map(|scanned| Self::from_partial(&scanned.version))
            .ok_or_else(|| Error::InvalidVersion(s.to_owned()))
    }

    /// bun `Version::parse(..).version.max()`: the version at the start of `s`, with missing
    /// parts as `u64::MAX`. Used by [`crate::order`].
    pub(crate) fn scan_max(s: &str) -> Result<Self> {
        let scanned = scan::scan_version(s.as_bytes());
        if !scanned.valid {
            return Err(Error::InvalidVersion(s.to_owned()));
        }
        let p = &scanned.version;
        Ok(Self {
            major: p.major.unwrap_or(u64::MAX),
            minor: p.minor.unwrap_or(u64::MAX),
            patch: p.patch.unwrap_or(u64::MAX),
            pre: identifiers(p.tag.pre),
            build: ascii(p.tag.build).into(),
        })
    }

    /// node-semver `semver.coerce(text)`: the first thing in `text` that looks like a version,
    /// up to its third number. `None` when nothing does, or a number has a leading zero or is
    /// above `Number.MAX_SAFE_INTEGER`.
    pub fn coerce(text: &str) -> Option<Self> {
        const LONGEST: usize = 16;
        fn digits(text: &[u8]) -> usize {
            text.iter().take_while(|it| it.is_ascii_digit()).count()
        }
        fn part(text: &[u8]) -> Option<(&[u8], &[u8])> {
            let text = text.strip_prefix(b".")?;
            let length = digits(text);
            (1..=LONGEST)
                .contains(&length)
                .then(|| text.split_at(length))
        }
        fn number(digits: &[u8]) -> Option<u64> {
            if matches!(digits, [b'0', _, ..]) {
                return None;
            }
            scan::parse_u64(digits).filter(|it| *it <= MAX_SAFE_INTEGER)
        }
        let mut rest = text.as_bytes();
        loop {
            let start = rest.iter().take_while(|it| !it.is_ascii_digit()).count();
            let from = &rest[start..];
            let (major, after) = from.split_at(digits(from));
            if major.is_empty() {
                return None;
            }
            if major.len() > LONGEST {
                rest = after;
                continue;
            }
            let minor = part(after);
            let patch = minor.and_then(|it| part(it.1));
            return Some(Self::new(
                number(major)?,
                minor.map_or(Some(0), |it| number(it.0))?,
                patch.map_or(Some(0), |it| number(it.0))?,
            ));
        }
    }

    /// Whether the version has a prerelease.
    #[inline]
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// `0.0.0` (ignoring the tag).
    #[inline]
    pub(crate) fn is_zero(&self) -> bool {
        self.major == 0 && self.minor == 0 && self.patch == 0
    }

    /// Same `major.minor.patch`.
    #[inline]
    pub(crate) fn same_core(&self, other: &Self) -> bool {
        self.major == other.major && self.minor == other.minor && self.patch == other.patch
    }

    /// node-semver `compare`: semver precedence, ignoring build metadata. Allocation-free.
    #[inline]
    pub fn cmp_precedence(&self, other: &Self) -> Ordering {
        let core =
            (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        if core != Ordering::Equal {
            return core;
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => cmp_pre(&self.pre, &other.pre),
        }
    }

    /// node-semver `eq`: same precedence (build metadata ignored).
    #[inline]
    pub fn eq_precedence(&self, other: &Self) -> bool {
        self.cmp_precedence(other) == Ordering::Equal
    }

    /// `pm version` / `npm version` increments (node-semver `inc`). An invalid `preid` is
    /// ignored and numbers saturate at `u64::MAX`; use [`Version::try_inc`] to get errors.
    pub fn inc(&self, release: ReleaseType, preid: Option<&str>) -> Self {
        self.try_inc(release, preid)
            .or_else(|_| self.try_inc(release, None))
            .unwrap_or_else(|_| self.saturating_inc(release))
    }

    /// Like [`Version::inc`], but fails like bun's `pm version`: when a number is above
    /// `Number.MAX_SAFE_INTEGER`, a prerelease identifier is empty or has a character semver
    /// does not allow, or `preid` is invalid (`01` included).
    pub fn try_inc(&self, release: ReleaseType, preid: Option<&str>) -> Result<Self> {
        let invalid_version = || Error::InvalidVersion(self.to_string());
        if self.major.max(self.minor).max(self.patch) > MAX_SAFE_INTEGER {
            return Err(invalid_version());
        }
        let mut pre: Vec<String> = Vec::with_capacity(self.pre.len() + 1);
        for id in &self.pre {
            let text = id.to_string();
            pre.push(normalize_identifier(&text).ok_or_else(invalid_version)?);
        }
        let (mut major, mut minor, mut patch) = (self.major, self.minor, self.patch);
        let is_release = pre.is_empty();
        match release {
            ReleaseType::Major | ReleaseType::Premajor => {
                if release == ReleaseType::Premajor || is_release || minor != 0 || patch != 0 {
                    major += 1;
                }
                (minor, patch) = (0, 0);
            }
            ReleaseType::Minor | ReleaseType::Preminor => {
                if release == ReleaseType::Preminor || is_release || patch != 0 {
                    minor += 1;
                }
                patch = 0;
            }
            ReleaseType::Patch | ReleaseType::Prerelease if !is_release => {}
            ReleaseType::Patch | ReleaseType::Prerelease | ReleaseType::Prepatch => patch += 1,
        }
        if release != ReleaseType::Prerelease {
            pre.clear();
        }
        if !matches!(
            release,
            ReleaseType::Major | ReleaseType::Minor | ReleaseType::Patch
        ) {
            match pre
                .iter_mut()
                .rev()
                .find_map(|id| Some((safe_number(id)?, id)))
            {
                Some((n, id)) => *id = (n + 1).to_string(),
                None => pre.push("0".to_owned()),
            }
            if let Some(preid) = preid.filter(|p| !p.is_empty()) {
                let invalid_preid = || Error::InvalidVersion(preid.to_owned());
                for part in preid.split('.') {
                    normalize_identifier(part).ok_or_else(invalid_preid)?;
                    if part.len() > 1
                        && part.starts_with('0')
                        && part.bytes().all(|c| c.is_ascii_digit())
                    {
                        return Err(invalid_preid());
                    }
                }
                let first = pre[0].as_str();
                let is_same_name = match (safe_number(first), safe_number(preid)) {
                    (Some(a), Some(b)) => a == b,
                    _ => first == preid,
                };
                let second_is_number = pre
                    .get(1)
                    .is_some_and(|s| s.bytes().all(|c| c.is_ascii_digit()));
                if !is_same_name || !second_is_number {
                    pre = vec![preid.to_owned(), "0".to_owned()];
                }
            }
        }
        Ok(Self {
            major,
            minor,
            patch,
            pre: pre
                .iter()
                .flat_map(|p| p.split('.'))
                .map(Identifier::new)
                .collect(),
            build: Box::default(),
        })
    }

    fn saturating_inc(&self, release: ReleaseType) -> Self {
        let mut out = Self::new(self.major, self.minor, self.patch);
        match release {
            ReleaseType::Major | ReleaseType::Premajor => {
                out.major = out.major.saturating_add(1);
                (out.minor, out.patch) = (0, 0);
            }
            ReleaseType::Minor | ReleaseType::Preminor => {
                out.minor = out.minor.saturating_add(1);
                out.patch = 0;
            }
            _ => out.patch = out.patch.saturating_add(1),
        }
        if !matches!(
            release,
            ReleaseType::Major | ReleaseType::Minor | ReleaseType::Patch
        ) {
            out.pre.push(Identifier::Numeric(0));
        }
        out
    }

    /// bun `Version::which_version_is_different`: the most significant part that differs.
    pub fn diff(&self, other: &Self) -> Option<ChangedVersion> {
        if self.major != other.major {
            return Some(ChangedVersion::Major);
        }
        if self.minor != other.minor {
            return Some(ChangedVersion::Minor);
        }
        if self.patch != other.patch {
            return Some(ChangedVersion::Patch);
        }
        if self.is_prerelease() != other.is_prerelease() {
            return Some(ChangedVersion::Pre);
        }
        if !self.is_prerelease() {
            return None;
        }
        if cmp_pre(&self.pre, &other.pre) != Ordering::Equal {
            return Some(ChangedVersion::Pre);
        }
        (self.build != other.build).then_some(ChangedVersion::Build)
    }

    /// bun `Version::which_version_is_pinned` (a modified pnpm `whichVersionIsPinned`): how much
    /// of the version a spec from `package.json` pins, without a full range parse. Invalid input
    /// pins the major.
    pub fn which_version_is_pinned(input: &str) -> PinnedVersion {
        which_version_is_pinned(scan::trim_ws(input.as_bytes()))
    }

    /// bun `Version::is_tagged_version_only`: `[A-Za-z][A-Za-z0-9]*` after trimming.
    pub fn is_tagged_version_only(input: &str) -> bool {
        let v = scan::trim_ws(input.as_bytes());
        !v.is_empty() && v[0].is_ascii_alphabetic() && v[1..].iter().all(u8::is_ascii_alphanumeric)
    }
}

/// bun pm `identifiers`: `None` if empty or not `[0-9A-Za-z-]+`; numbers below
/// `MAX_SAFE_INTEGER` are normalized (`01` -> `1`).
fn normalize_identifier(id: &str) -> Option<String> {
    let valid = !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
    valid.then(|| safe_number(id).map_or_else(|| id.to_owned(), |n| n.to_string()))
}

fn safe_number(id: &str) -> Option<u64> {
    scan::parse_u64(id.as_bytes()).filter(|n| *n < MAX_SAFE_INTEGER)
}

fn which_version_is_pinned(version: &[u8]) -> PinnedVersion {
    let mut i;
    let pinned = 'pinned: {
        for (j, &c) in version.iter().enumerate() {
            if scan::is_ws(c) || c == b'v' || c == b'=' {
                continue;
            }
            i = j;
            match c {
                b'~' | b'^' => {
                    i += 1;
                    for (k, &d) in version.iter().enumerate().skip(i) {
                        if !scan::is_ws(d) {
                            i = k;
                            break 'pinned if c == b'~' {
                                PinnedVersion::Minor
                            } else {
                                PinnedVersion::Major
                            };
                        }
                    }
                    return PinnedVersion::Major;
                }
                b'0'..=b'9' => break 'pinned PinnedVersion::Patch,
                _ => return PinnedVersion::Major,
            }
        }
        return PinnedVersion::Major;
    };

    let len = version.len();
    let read_number = |i: &mut usize| -> Option<Option<u8>> {
        if *i >= len || !version[*i].is_ascii_digit() {
            return None;
        }
        while *i < len && version[*i].is_ascii_digit() {
            *i += 1;
        }
        Some(version.get(*i).copied())
    };
    // major
    match read_number(&mut i) {
        None => return PinnedVersion::Major,
        Some(None) => {
            return if pinned == PinnedVersion::Patch {
                PinnedVersion::Major
            } else {
                pinned
            };
        }
        Some(Some(b'.')) => {}
        Some(Some(_)) => return PinnedVersion::Major,
    }
    i += 1;
    // minor
    match read_number(&mut i) {
        None => return PinnedVersion::Major,
        Some(None) => {
            return if pinned == PinnedVersion::Patch {
                PinnedVersion::Minor
            } else {
                pinned
            };
        }
        Some(Some(b'.')) => {}
        Some(Some(_)) => return PinnedVersion::Major,
    }
    i += 1;
    // patch
    let d = match read_number(&mut i) {
        None => return PinnedVersion::Major,
        Some(None) => return pinned,
        Some(Some(d)) => d,
    };
    let tag_char =
        |c: u8| matches!(c, b'-' | b'+' | b'.' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9');
    if !tag_char(d) {
        return PinnedVersion::Major;
    }
    i += 1;
    while i < len {
        if !tag_char(version[i]) {
            return PinnedVersion::Major;
        }
        i += 1;
    }
    pinned
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_precedence(other)
            .then_with(|| self.build.as_bytes().cmp(other.build.as_bytes()))
            .then_with(|| {
                // Same precedence but different spelling (`01` vs `1`): keep `Ord` consistent
                // with `Eq`.
                if self.pre == other.pre {
                    Ordering::Equal
                } else {
                    self.pre
                        .iter()
                        .map(ToString::to_string)
                        .cmp(other.pre.iter().map(ToString::to_string))
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            fmt::Display::fmt(id, f)?;
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build)?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Version {
    type Err = Error;
    /// [`Version::parse_loose`], the form bun accepts everywhere.
    fn from_str(s: &str) -> Result<Self> {
        Self::parse_loose(s)
    }
}
