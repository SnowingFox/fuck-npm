//! Registry manifests (packuments): the model, the sonic-rs parser and version picking.
//!
//! Parsing follows bun's `PackageManifest::parse` (`install/npm.rs`): every field is read
//! leniently (a value of the wrong type is ignored rather than failing the document), version
//! keys that are not valid semver are skipped like npm does, and versions are sorted by semver
//! precedence. Picking ports `find_best_version`, `find_best_version_with_filter` and
//! `find_by_dist_tag_with_filter`, including the `minimumReleaseAge` stability window.

use std::collections::BTreeMap;

use fnpm_semver::{Range, Version};
use fnpm_types::{Bin, Integrity, PeerMeta, PlatformConstraints};
use serde::{Deserialize, Serialize};

use crate::semver_ext;
use crate::{Error, Result};

mod raw;

/// The `dist` object of a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dist {
    /// `dist.tarball` as the registry wrote it; empty when the document had none (the default
    /// location `<registry>/<name>/-/<unscoped>-<version>.tgz` applies then).
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
    /// In manifest order, followed by optional peers that only `peerDependenciesMeta` declares
    /// (as `"*"`, like pnpm, yarn and bun).
    pub peer_dependencies: Vec<(String, String)>,
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    /// `bundleDependencies` / `bundledDependencies`. `true` (bundle everything) is expanded to
    /// the names of `dependencies` and `optionalDependencies`.
    pub bundle_dependencies: Vec<String>,
    /// `bin`; `directories.bin` folds into it as [`Bin::Dir`].
    pub bin: Bin,
    pub platform: PlatformConstraints,
    pub dist: Dist,
    pub has_install_script: bool,
    /// `"deprecated": true` is `"true"`; `""` (after `npm undeprecate`) is `None`.
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
    /// Publish times (full manifests only, used by `minimumReleaseAge`), unix ms. Version keys
    /// are canonical version strings. `0` means "no time given" (passes any age gate);
    /// `u64::MAX` means "not a date" (never passes), like npm.
    pub time: BTreeMap<String, u64>,
    pub modified: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// When it was fetched or revalidated (unix ms), for cache freshness.
    pub fetched_at_ms: u64,
    pub max_age_secs: u32,
    pub is_full: bool,
}

/// Options for [`PackageManifest::pick`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PickOptions {
    pub include_prerelease: bool,
    /// Skip versions published less than this many ms before `now_ms`.
    pub minimum_release_age_ms: Option<u64>,
    pub now_ms: u64,
}

/// `minimumReleaseAgeExcludes` in pnpm's grammar: `name` exempts every version of a package,
/// `name@1.2.3 || 1.2.4` only those versions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseAgeExcludes {
    entries: Vec<(String, Option<Version>)>,
}

impl ReleaseAgeExcludes {
    /// Parses the configured entries. A malformed entry (`foo@`, `foo@^1`) is dropped and its
    /// warning returned, never widened to the whole package.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> (Self, Vec<String>) {
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        for entry in entries {
            let entry = entry.as_ref();
            let name_start = usize::from(entry.starts_with('@'));
            let Some(at) = entry[name_start..].find('@') else {
                out.push((entry.to_owned(), None));
                continue;
            };
            let (name, specs) = (&entry[..name_start + at], &entry[name_start + at + 1..]);
            let first = out.len();
            for spec in specs.split("||") {
                match semver_ext::parse_version(spec) {
                    Some(v) => out.push((name.to_owned(), Some(v))),
                    None => {
                        out.truncate(first);
                        warnings.push(format!(
                            "ignoring minimumReleaseAgeExcludes entry {entry:?}: expected a package name or exact versions, e.g. \"{name}@1.2.3\" or \"{name}@1.2.3 || 1.2.4\""
                        ));
                        break;
                    }
                }
            }
        }
        (Self { entries: out }, warnings)
    }

    /// Adds whole packages (`fnpm audit fix` exempts the packages it must move past the gate).
    pub fn with_packages<I: IntoIterator<Item = String>>(mut self, names: I) -> Self {
        self.entries.extend(names.into_iter().map(|n| (n, None)));
        self
    }

    /// True if every version of `name` bypasses the age gate.
    pub fn excludes_package(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, v)| n == name && v.is_none())
    }

    /// True if exactly `name@version` is listed.
    pub fn excludes_version(&self, name: &str, version: &Version) -> bool {
        self.entries.iter().any(|(n, v)| {
            n == name
                && v.as_ref()
                    .is_some_and(|v| semver_ext::cmp(v, version).is_eq())
        })
    }
}

/// The detailed outcome of a pick (bun's `FindVersionResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickResult<'a> {
    Found(&'a VersionManifest),
    /// Found, but a newer matching version was held back by `minimumReleaseAge`.
    FoundWithFilter {
        version: &'a VersionManifest,
        newest_filtered: Option<&'a Version>,
    },
    NotFound,
    /// An exact version was asked for and it is too recent.
    TooRecent,
    /// Every matching version is too recent.
    AllVersionsTooRecent,
}

impl<'a> PickResult<'a> {
    /// The picked version, if any.
    pub fn version(self) -> Option<&'a VersionManifest> {
        match self {
            Self::Found(v) | Self::FoundWithFilter { version: v, .. } => Some(v),
            _ => None,
        }
    }

    /// True if the newest match was filtered out by the age gate (for the resolver's warning).
    pub fn latest_is_filtered(self) -> bool {
        match self {
            Self::FoundWithFilter {
                newest_filtered, ..
            } => newest_filtered.is_some(),
            Self::AllVersionsTooRecent => true,
            _ => false,
        }
    }
}

/// Only the full document has `libc`, so packages whose names suggest a libc-specific build are
/// fetched in full (bun `name_suggests_libc`). `gnu` is not checked: it is also in `signup`.
pub fn name_suggests_libc(name: &str) -> bool {
    name.contains("linux") || name.contains("musl") || name.contains("glibc")
}

impl PackageManifest {
    /// Parses a registry JSON document (abbreviated or full). `name` is the requested name,
    /// which wins over the document's (custom registries may differ). Headers-derived fields
    /// (`etag`, `last_modified`, `fetched_at_ms`, `max_age_secs`, `is_full`) are left at their
    /// defaults for the caller to fill.
    pub fn parse(name: &str, json: &[u8]) -> Result<Self> {
        raw::parse(name, json).map_err(Error::from)
    }

    /// The version that has exactly this precedence.
    pub fn find(&self, version: &Version) -> Option<&VersionManifest> {
        let i = self
            .versions
            .partition_point(|v| semver_ext::cmp(&v.version, version).is_lt());
        self.versions
            .get(i)
            .filter(|v| semver_ext::cmp(&v.version, version).is_eq())
    }

    /// The version a dist-tag points at, if it is published.
    pub fn dist_tag(&self, tag: &str) -> Option<&VersionManifest> {
        let v = semver_ext::parse_version(self.dist_tags.get(tag)?)?;
        self.find(&v)
    }

    /// Publish time of a version: 0 when unknown.
    pub fn publish_time_ms(&self, version: &Version) -> u64 {
        if self.time.is_empty() {
            return 0;
        }
        self.time
            .get(&semver_ext::format(version))
            .copied()
            .unwrap_or(0)
    }

    /// npm's pick: if the `latest` tag satisfies the range, it wins; otherwise the highest
    /// satisfying non-deprecated version, then the highest satisfying at all.
    pub fn pick(&self, range: &Range, opts: PickOptions) -> Option<&VersionManifest> {
        self.pick_detailed(range, opts, None).version()
    }

    /// [`pick`](Self::pick) with the age-gate outcome and version-level excludes.
    pub fn pick_detailed(
        &self,
        range: &Range,
        opts: PickOptions,
        excludes: Option<&ReleaseAgeExcludes>,
    ) -> PickResult<'_> {
        let query = Query::new(range, opts.include_prerelease);
        match self.age_gate(opts, excludes) {
            None => self
                .find_best(&query)
                .map_or(PickResult::NotFound, PickResult::Found),
            Some(gate) => self.find_best_filtered(&query, &gate),
        }
    }

    /// npm's `*`: `latest` (even a prerelease) unless deprecated, else the highest release.
    pub fn pick_any(
        &self,
        opts: PickOptions,
        excludes: Option<&ReleaseAgeExcludes>,
    ) -> PickResult<'_> {
        self.pick_detailed(&Range::any(), opts, excludes)
    }

    /// `latest`, or [`pick_any`](Self::pick_any) when the tag names no published version.
    pub fn pick_latest(
        &self,
        opts: PickOptions,
        excludes: Option<&ReleaseAgeExcludes>,
    ) -> PickResult<'_> {
        if self.dist_tag("latest").is_none() {
            return self.pick_any(opts, excludes);
        }
        self.pick_tag("latest", opts, excludes)
    }

    /// A dist-tag, falling back past versions the age gate holds back (bun
    /// `find_by_dist_tag_with_filter`): the newest older version of the same release line
    /// (same prerelease tag for a prerelease) that is old enough and stable.
    pub fn pick_tag(
        &self,
        tag: &str,
        opts: PickOptions,
        excludes: Option<&ReleaseAgeExcludes>,
    ) -> PickResult<'_> {
        let Some(tagged) = self.dist_tag(tag) else {
            return PickResult::NotFound;
        };
        let Some(gate) = self.age_gate(opts, excludes) else {
            return PickResult::Found(tagged);
        };
        if !gate.blocked(self, tagged) {
            return PickResult::Found(tagged);
        }
        let is_pre = tagged.version.is_prerelease();
        let tagged_line = semver_ext::pre_tag(&tagged.version);
        let mut best: Option<&VersionManifest> = None;
        let mut deprecated: Option<&VersionManifest> = None;
        let mut prev_blocked: Option<u64> = Some(self.publish_time_ms(&tagged.version));
        let filtered = Some(&tagged.version);
        for v in self.versions.iter().rev() {
            if v.version.is_prerelease() != is_pre
                || semver_ext::cmp(&v.version, &tagged.version).is_gt()
            {
                continue;
            }
            if is_pre && semver_ext::pre_tag(&v.version) != tagged_line {
                continue;
            }
            if v.deprecated.is_some() {
                if !gate.blocked(self, v) {
                    deprecated.get_or_insert(v);
                }
                continue;
            }
            if gate.excludes_version(self, v) {
                best = Some(v);
                break;
            }
            let published = self.publish_time_ms(&v.version);
            if gate.too_recent(published) {
                prev_blocked = Some(published);
                continue;
            }
            if let Some(prev) = prev_blocked {
                if (published as f64) < gate.now - (gate.min_age + SEVEN_DAYS_MS) {
                    return PickResult::FoundWithFilter {
                        version: best.unwrap_or(v),
                        newest_filtered: filtered,
                    };
                }
                if prev as f64 - published as f64 >= gate.stability_window() {
                    return PickResult::FoundWithFilter {
                        version: v,
                        newest_filtered: filtered,
                    };
                }
                best.get_or_insert(v);
                prev_blocked = Some(published);
                continue;
            }
            best = Some(v);
            break;
        }
        match best.or(deprecated) {
            Some(version) => PickResult::FoundWithFilter {
                version,
                newest_filtered: filtered,
            },
            None => PickResult::AllVersionsTooRecent,
        }
    }

    /// Like `npm view <name>@<spec>`: an exact dist-tag name wins, then `*`/`latest`, then the
    /// best match for a range. `None` for anything else.
    pub fn find_by_spec(&self, spec: &str) -> Option<&VersionManifest> {
        let spec = if spec.is_empty() { "latest" } else { spec };
        if self.dist_tags.contains_key(spec) {
            return self.dist_tag(spec);
        }
        if spec == "*" || spec == "latest" {
            return self.pick_any(PickOptions::default(), None).version();
        }
        match fnpm_types::spec::infer_tag(spec) {
            fnpm_types::SpecTag::Npm => {
                let range_text = if spec.len() > 1 && spec.starts_with('v') {
                    &spec[1..]
                } else {
                    spec
                };
                if let Some(v) = semver_ext::parse_version(range_text) {
                    return self.find(&v);
                }
                let range = Range::parse(range_text).ok()?;
                self.pick(&range, PickOptions::default())
            }
            _ => None,
        }
    }

    fn age_gate<'e>(
        &self,
        opts: PickOptions,
        excludes: Option<&'e ReleaseAgeExcludes>,
    ) -> Option<AgeGate<'e>> {
        let min_age = opts.minimum_release_age_ms?;
        if excludes.is_some_and(|e| e.excludes_package(&self.name)) {
            return None;
        }
        Some(AgeGate {
            min_age: min_age as f64,
            now: opts.now_ms as f64,
            excludes,
        })
    }

    /// bun `find_best_version`.
    fn find_best<'s>(&'s self, query: &Query<'_>) -> Option<&'s VersionManifest> {
        if let Some(exact) = query.exact {
            return self.find(exact);
        }
        if let Some(latest) = self.dist_tag("latest")
            && latest.deprecated.is_none()
            && query.latest_ok(latest)
        {
            return Some(latest);
        }
        let mut deprecated = None;
        for v in query.candidates(&self.versions) {
            if v.deprecated.is_none() {
                return Some(v);
            }
            deprecated.get_or_insert(v);
        }
        deprecated
    }

    /// bun `find_best_version_with_filter`.
    fn find_best_filtered<'s>(&'s self, query: &Query<'_>, gate: &AgeGate<'_>) -> PickResult<'s> {
        if let Some(exact) = query.exact {
            return match self.find(exact) {
                Some(v) if gate.blocked(self, v) => PickResult::TooRecent,
                Some(v) => PickResult::Found(v),
                None => PickResult::NotFound,
            };
        }
        let mut newest_filtered: Option<&Version> = None;
        if let Some(latest) = self.dist_tag("latest")
            && (query.range.is_any() || query.matches(latest))
        {
            if gate.blocked(self, latest) {
                newest_filtered = Some(&latest.version);
            }
            if newest_filtered.is_none() && latest.deprecated.is_none() && query.latest_ok(latest) {
                return PickResult::Found(latest);
            }
        }
        let mut deprecated: Option<&VersionManifest> = None;
        for pre_pass in query.passes() {
            let list = self
                .versions
                .iter()
                .rev()
                .filter(|v| v.version.is_prerelease() == pre_pass || query.merged());
            if let Some(result) =
                self.search_list(list, query, gate, &mut newest_filtered, &mut deprecated)
            {
                return result;
            }
            if query.merged() {
                break;
            }
        }
        if let Some(version) = deprecated {
            return PickResult::FoundWithFilter {
                version,
                newest_filtered,
            };
        }
        if newest_filtered.is_some() {
            return PickResult::AllVersionsTooRecent;
        }
        PickResult::NotFound
    }

    /// bun `search_version_list`, newest first.
    fn search_list<'s, I: Iterator<Item = &'s VersionManifest>>(
        &'s self,
        list: I,
        query: &Query<'_>,
        gate: &AgeGate<'_>,
        newest_filtered: &mut Option<&'s Version>,
        deprecated: &mut Option<&'s VersionManifest>,
    ) -> Option<PickResult<'s>> {
        let mut prev_blocked: Option<u64> = None;
        let mut best: Option<&VersionManifest> = None;
        for v in list {
            if !query.matches(v) {
                continue;
            }
            if v.deprecated.is_some() {
                if gate.blocked(self, v) {
                    newest_filtered.get_or_insert(&v.version);
                } else {
                    deprecated.get_or_insert(v);
                }
                continue;
            }
            if gate.excludes_version(self, v) {
                best = Some(v);
                break;
            }
            let published = self.publish_time_ms(&v.version);
            if gate.too_recent(published) {
                newest_filtered.get_or_insert(&v.version);
                prev_blocked = Some(published);
            } else if let Some(prev) = prev_blocked {
                if (published as f64) < gate.now - (gate.min_age + SEVEN_DAYS_MS) {
                    best.get_or_insert(v);
                    break;
                }
                if prev as f64 - published as f64 >= gate.stability_window() {
                    best = Some(v);
                    break;
                }
                best.get_or_insert(v);
                prev_blocked = Some(published);
            } else {
                return Some(PickResult::Found(v));
            }
        }
        best.map(|version| match *newest_filtered {
            Some(_) => PickResult::FoundWithFilter {
                version,
                newest_filtered: *newest_filtered,
            },
            None => PickResult::Found(version),
        })
    }
}

const SEVEN_DAYS_MS: f64 = 7.0 * 24.0 * 3600.0 * 1000.0;

struct AgeGate<'e> {
    min_age: f64,
    now: f64,
    excludes: Option<&'e ReleaseAgeExcludes>,
}

impl AgeGate<'_> {
    fn too_recent(&self, published_ms: u64) -> bool {
        published_ms as f64 > self.now - self.min_age
    }

    fn excludes_version(&self, m: &PackageManifest, v: &VersionManifest) -> bool {
        self.excludes
            .is_some_and(|e| e.excludes_version(&m.name, &v.version))
    }

    fn blocked(&self, m: &PackageManifest, v: &VersionManifest) -> bool {
        self.too_recent(m.publish_time_ms(&v.version)) && !self.excludes_version(m, v)
    }

    fn stability_window(&self) -> f64 {
        self.min_age.min(SEVEN_DAYS_MS)
    }
}

/// A range prepared for picking.
struct Query<'r> {
    range: &'r Range,
    include_prerelease: bool,
    has_pre: bool,
    exact: Option<&'r Version>,
    left: Option<&'r Version>,
}

impl<'r> Query<'r> {
    fn new(range: &'r Range, include_prerelease: bool) -> Self {
        Self {
            range,
            include_prerelease,
            has_pre: semver_ext::has_prerelease(range),
            exact: semver_ext::exact(range),
            left: semver_ext::left_version(range),
        }
    }

    fn matches(&self, v: &VersionManifest) -> bool {
        semver_ext::satisfies(self.range, &v.version, self.include_prerelease)
    }

    /// `latest` wins if it matches, and for a prerelease range only if it is the range's own
    /// version ("use latest if semver+tag match range exactly").
    fn latest_ok(&self, latest: &VersionManifest) -> bool {
        (self.range.is_any() || self.matches(latest))
            && (!self.has_pre
                || self
                    .left
                    .is_some_and(|l| semver_ext::cmp(l, &latest.version).is_eq()))
    }

    /// One merged pass with `include_prerelease`; otherwise releases, then prereleases if the
    /// range names one.
    fn merged(&self) -> bool {
        self.include_prerelease
    }

    fn passes(&self) -> impl Iterator<Item = bool> {
        let pre = self.has_pre && !self.include_prerelease;
        [false].into_iter().chain(pre.then_some(true))
    }

    fn candidates<'m>(
        &self,
        versions: &'m [VersionManifest],
    ) -> impl Iterator<Item = &'m VersionManifest> + use<'m, 'r> {
        let (range, include_prerelease, merged) =
            (self.range, self.include_prerelease, self.merged());
        let pre = self.has_pre && !self.include_prerelease;
        [false]
            .into_iter()
            .chain(pre.then_some(true))
            .flat_map(move |pre_pass| {
                versions
                    .iter()
                    .rev()
                    .filter(move |v| merged || v.version.is_prerelease() == pre_pass)
                    .filter(move |v| semver_ext::satisfies(range, &v.version, include_prerelease))
            })
    }
}

impl From<raw::ParseError> for Error {
    fn from(e: raw::ParseError) -> Self {
        Error::InvalidManifest {
            name: e.name,
            message: e.message,
        }
    }
}

#[cfg(test)]
mod tests;
