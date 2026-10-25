//! Registry manifests (packuments), parsed with sonic-rs.

use std::collections::BTreeMap;

use fnpm_semver::{Range, SatisfyOptions, Version};
use fnpm_types::{Bin, Integrity, PeerMeta, PlatformConstraints};
use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dist {
    pub tarball: String,
    /// `integrity`, else `shasum` as SHA-1.
    pub integrity: Option<Integrity>,
    pub file_count: Option<u32>,
    pub unpacked_size: Option<u64>,
}

/// One version of a package, as much as an install needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionManifest {
    pub version: Version,
    /// In manifest order.
    pub dependencies: Vec<(String, String)>,
    pub optional_dependencies: Vec<(String, String)>,
    pub peer_dependencies: Vec<(String, String)>,
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    pub bundle_dependencies: Vec<String>,
    pub bin: Bin,
    /// `directories.bin` folds into `bin` as [`Bin::Dir`].
    pub platform: PlatformConstraints,
    pub dist: Dist,
    pub has_install_script: bool,
    pub deprecated: Option<String>,
    pub engines: BTreeMap<String, String>,
    /// Only in full manifests.
    pub scripts: BTreeMap<String, String>,
    pub license: Option<String>,
}

/// A package's versions, sorted ascending by semver precedence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManifest {
    pub name: String,
    pub dist_tags: BTreeMap<String, String>,
    pub versions: Vec<VersionManifest>,
    /// Publish times (full manifests only, used by `minimumReleaseAge`). Unix ms.
    pub time: BTreeMap<String, u64>,
    pub modified: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// When it was fetched or revalidated (unix ms), for cache freshness.
    pub fetched_at_ms: u64,
    pub max_age_secs: u32,
    pub is_full: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PickOptions {
    pub include_prerelease: bool,
    /// Skip versions published less than this many ms before `now_ms`.
    pub minimum_release_age_ms: Option<u64>,
    pub now_ms: u64,
}

impl PackageManifest {
    /// Parses a registry JSON document (abbreviated or full).
    pub fn parse(_name: &str, _json: &[u8]) -> Result<Self> {
        todo!("sonic-rs; only materialise fields listed above")
    }

    pub fn find(&self, version: &Version) -> Option<&VersionManifest> {
        self.versions.binary_search_by(|v| v.version.cmp(version)).ok().map(|i| &self.versions[i])
    }

    pub fn dist_tag(&self, tag: &str) -> Option<&VersionManifest> {
        let v: Version = self.dist_tags.get(tag)?.parse().ok()?;
        self.find(&v)
    }

    /// npm's pick: if the `latest` tag satisfies the range, it wins; otherwise the highest
    /// satisfying non-deprecated version, then the highest satisfying at all.
    pub fn pick(&self, _range: &Range, _opts: PickOptions) -> Option<&VersionManifest> {
        let _ = SatisfyOptions::default();
        todo!("port bun npm.rs find_best_version")
    }
}
