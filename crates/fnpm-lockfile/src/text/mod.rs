//! The bun.lock text format, shared by `fnpm.lock` and `bun.lock` (bun
//! `install/lockfile/bun.lock.rs`).

pub(crate) mod extensions;
pub(crate) mod parse;
pub(crate) mod write;

use fnpm_types::LockGraph;

/// The default registry without its trailing slash, as bun compares tarball URLs with it.
pub(crate) const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// bun's text lockfile versions (`bun.lock.rs` `Version`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub(crate) enum LockfileVersion {
    V0 = 0,
    /// Workspace packages are only listed by path.
    V1 = 1,
    /// Off-registry npm tarballs need an integrity, integrities must be valid, git tags safe.
    V2 = 2,
    /// `overrides` may hold scoped rules.
    V3 = 3,
}

impl LockfileVersion {
    pub const CURRENT: Self = Self::V3;

    pub fn from_int(n: u32) -> Option<Self> {
        Some(match n {
            0 => Self::V0,
            1 => Self::V1,
            2 => Self::V2,
            3 => Self::V3,
            _ => return None,
        })
    }

    /// The version a graph was loaded with; out-of-range values count as current.
    pub fn from_graph(g: &LockGraph) -> Self {
        Self::from_int(g.lockfile_version).unwrap_or(Self::CURRENT)
    }
}

/// `url` points under `registry`: the registry href without its trailing slash is a prefix
/// and the next byte is a path separator.
pub(crate) fn url_is_under_registry(url: &str, registry: &str) -> bool {
    let registry = registry.strip_suffix('/').unwrap_or(registry);
    url.starts_with(registry)
        && (url.len() == registry.len() || url.as_bytes()[registry.len()] == b'/')
}

pub(crate) const MAX_RESOLVED_TAG_LEN: usize = 256;

/// A git `.bun-tag` must be one safe path component (bun `is_safe_resolved_tag`).
pub(crate) fn is_safe_resolved_tag(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_RESOLVED_TAG_LEN
        && !s.starts_with('-')
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_prefix() {
        assert!(url_is_under_registry(
            "https://registry.npmjs.org/a/-/a-1.0.0.tgz",
            "https://registry.npmjs.org/"
        ));
        assert!(!url_is_under_registry(
            "https://registry.npmjs.org.evil.com/a.tgz",
            "https://registry.npmjs.org"
        ));
        assert!(url_is_under_registry(
            "https://registry.npmjs.org",
            DEFAULT_REGISTRY
        ));
    }

    #[test]
    fn resolved_tags() {
        assert!(is_safe_resolved_tag("owner-repo-abc1234"));
        assert!(!is_safe_resolved_tag("-x"));
        assert!(!is_safe_resolved_tag("a/b"));
        assert!(!is_safe_resolved_tag(".."));
        assert!(!is_safe_resolved_tag(""));
        assert!(!is_safe_resolved_tag(&"a".repeat(257)));
    }
}

#[cfg(test)]
mod roundtrip_tests;

#[cfg(test)]
mod parse_tests;

#[cfg(test)]
mod write_tests;
