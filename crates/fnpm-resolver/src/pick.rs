//! Version picking: which published version a range or dist-tag resolves to.
//!
//! A port of bun `install/npm.rs` (`find_for_dependency`, `find_best_version`,
//! `find_by_dist_tag_with_filter`, `search_version_list`) including the `minimumReleaseAge`
//! gate with its stability window and `minimumReleaseAgeExcludes`.

use std::cmp::Ordering;

use fnpm_registry::{PackageManifest, VersionManifest};
use fnpm_semver::{Range, Version};

use crate::options::ReleaseAgeExcludes;

const SEVEN_DAYS_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// The `minimumReleaseAge` gate for one resolve.
#[derive(Debug, Clone, Copy)]
pub struct AgeGate<'a> {
    /// Versions published less than this long ago are skipped.
    pub min_age_ms: u64,
    /// The clock, captured once per resolve so every pick agrees.
    pub now_ms: u64,
    /// `minimumReleaseAgeExcludes`.
    pub excludes: &'a ReleaseAgeExcludes,
}

/// Why no version was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickError {
    /// No published version satisfies the range / the tag does not exist.
    NotFound,
    /// The exact version asked for is newer than the gate allows.
    TooRecent,
    /// Versions satisfy the range, but every one is newer than the gate allows.
    AllVersionsTooRecent,
}

/// The outcome of a pick.
#[derive(Debug, Clone, Copy)]
pub enum Picked<'m> {
    /// The version, chosen without the gate getting in the way.
    Found(&'m VersionManifest),
    /// Found, but the gate skipped newer candidates (`newest_filtered` is the newest skipped).
    FoundWithFilter {
        /// The chosen version.
        result: &'m VersionManifest,
        /// The newest version the gate skipped.
        newest_filtered: Option<&'m Version>,
    },
    /// Nothing was picked.
    Err(PickError),
}

impl<'m> Picked<'m> {
    /// The chosen version, if any.
    pub fn ok(self) -> Option<&'m VersionManifest> {
        match self {
            Self::Found(v) | Self::FoundWithFilter { result: v, .. } => Some(v),
            Self::Err(_) => None,
        }
    }

    /// The gate hid a newer version than the one picked (`fnpm outdated` marks these).
    pub fn latest_is_filtered(&self) -> bool {
        match self {
            Self::FoundWithFilter {
                newest_filtered, ..
            } => newest_filtered.is_some(),
            Self::Err(e) => *e == PickError::AllVersionsTooRecent,
            Self::Found(_) => false,
        }
    }
}

/// What a dependency asks the registry for.
#[derive(Debug, Clone, Copy)]
pub enum Wanted<'a> {
    /// A semver range. `literal` is the range text as written (after `npm:` / alias removal).
    Range {
        /// The parsed range.
        range: &'a Range,
        /// Its text.
        literal: &'a str,
    },
    /// A dist-tag. `literal` is the declared text (`""` for an implicit `latest`).
    Tag {
        /// The tag (`latest` for an empty spec).
        tag: &'a str,
        /// The declared text.
        literal: &'a str,
    },
}

fn is_deprecated(v: &VersionManifest) -> bool {
    v.deprecated.as_deref().is_some_and(|d| !d.is_empty())
}

fn publish_ms(m: &PackageManifest, v: &VersionManifest) -> u64 {
    m.time.get(&v.version.to_string()).copied().unwrap_or(0)
}

fn too_recent(m: &PackageManifest, v: &VersionManifest, gate: &AgeGate<'_>) -> bool {
    publish_ms(m, v) > gate.now_ms.saturating_sub(gate.min_age_ms)
}

fn blocked(m: &PackageManifest, v: &VersionManifest, gate: &AgeGate<'_>) -> bool {
    too_recent(m, v, gate) && !gate.excludes.excludes_version(&m.name, &v.version)
}

fn excluded_entirely(m: &PackageManifest, gate: Option<&AgeGate<'_>>) -> bool {
    gate.is_some_and(|g| g.excludes.excludes_package(&m.name))
}

/// Precedence comparison ignoring build metadata.
fn same_version(a: &Version, b: &Version) -> bool {
    a.cmp(b) == Ordering::Equal
}

fn has_prerelease(range: &Range) -> bool {
    range
        .sets
        .iter()
        .flatten()
        .any(|c| c.version.is_prerelease())
}

fn left_version(range: &Range) -> Option<&Version> {
    range
        .sets
        .first()
        .and_then(|s| s.first())
        .map(|c| &c.version)
}

fn is_empty_group(range: Option<&Range>) -> bool {
    range.is_none_or(|r| r.sets.iter().all(Vec::is_empty))
}

fn satisfies(range: Option<&Range>, v: &Version) -> bool {
    match range {
        None => true,
        Some(r) => r.satisfies(v),
    }
}

/// npm's `*` goes by the text: `x` and `>=0.0.0` are ranges like any other.
pub fn is_any_version_range(literal: &str) -> bool {
    literal == "*" || literal.ends_with("@*")
}

/// The releases (`pre == false`) or prereleases of `m`, newest first (`versions` is sorted
/// ascending).
fn newest_first(m: &PackageManifest, pre: bool) -> impl Iterator<Item = &VersionManifest> {
    m.versions
        .iter()
        .rev()
        .filter(move |v| v.version.is_prerelease() == pre)
}

/// Releases are searched, then prereleases if the range has a prerelease comparator.
fn list_kinds(with_prereleases: bool) -> &'static [bool] {
    if with_prereleases {
        &[false, true]
    } else {
        &[false]
    }
}

/// bun `find_for_dependency`.
pub fn find_for_dependency<'m>(
    m: &'m PackageManifest,
    wanted: Wanted<'_>,
    gate: Option<&AgeGate<'_>>,
) -> Picked<'m> {
    match wanted {
        Wanted::Range { range, literal } => {
            if is_any_version_range(literal) {
                return find_any_version(m, gate);
            }
            find_best_version_with_filter(m, Some(range), gate)
        }
        Wanted::Tag { tag, literal } => {
            // Nobody wrote `latest` explicitly, so like npm, don't follow it to a deprecated version.
            if literal.is_empty() && m.dist_tag("latest").is_some_and(is_deprecated) {
                return find_any_version(m, gate);
            }
            find_by_dist_tag_with_filter(m, tag, gate)
        }
    }
}

/// npm's `*`: `latest` (even a prerelease) unless deprecated, else the highest release.
pub fn find_any_version<'m>(m: &'m PackageManifest, gate: Option<&AgeGate<'_>>) -> Picked<'m> {
    find_best_version_with_filter(m, None, gate)
}

/// `latest`, or [`find_any_version`] when that tag names no published version.
pub fn find_latest<'m>(m: &'m PackageManifest, gate: Option<&AgeGate<'_>>) -> Picked<'m> {
    if m.dist_tag("latest").is_none() {
        return find_any_version(m, gate);
    }
    find_by_dist_tag_with_filter(m, "latest", gate)
}

/// bun `find_by_dist_tag_with_filter`: the tag's version, or with the gate the newest older
/// version of the same release line that passes it.
pub fn find_by_dist_tag_with_filter<'m>(
    m: &'m PackageManifest,
    tag: &str,
    gate: Option<&AgeGate<'_>>,
) -> Picked<'m> {
    let Some(dist) = m.dist_tag(tag) else {
        return Picked::Err(PickError::NotFound);
    };
    let Some(gate) = gate.filter(|_| !excluded_entirely(m, gate)) else {
        return Picked::Found(dist);
    };
    if !blocked(m, dist, gate) {
        return Picked::Found(dist);
    }
    let stability_window = gate.min_age_ms.min(SEVEN_DAYS_MS);
    let latest = &dist.version;
    let is_pre = latest.is_prerelease();
    let first_pre = |v: &Version| v.pre.first().map(|i| format!("{i:?}"));
    let latest_tag = first_pre(latest);

    let mut best: Option<&VersionManifest> = None;
    let mut deprecated: Option<&VersionManifest> = None;
    let mut prev_blocked: Option<&VersionManifest> = Some(dist);
    let candidates = m
        .versions
        .iter()
        .filter(|v| v.version.is_prerelease() == is_pre);
    for v in candidates.rev() {
        if v.version.cmp(latest) == Ordering::Greater {
            continue;
        }
        if is_pre && first_pre(&v.version) != latest_tag {
            continue;
        }
        if is_deprecated(v) {
            if !blocked(m, v, gate) {
                deprecated.get_or_insert(v);
            }
            continue;
        }
        if gate.excludes.excludes_version(&m.name, &v.version) {
            best = Some(v);
            break;
        }
        if too_recent(m, v, gate) {
            prev_blocked = Some(v);
            continue;
        }
        if let Some(prev) = prev_blocked {
            if publish_ms(m, v)
                < gate
                    .now_ms
                    .saturating_sub(gate.min_age_ms.saturating_add(SEVEN_DAYS_MS))
            {
                return Picked::FoundWithFilter {
                    result: best.unwrap_or(v),
                    newest_filtered: Some(latest),
                };
            }
            if publish_ms(m, prev).saturating_sub(publish_ms(m, v)) >= stability_window {
                return Picked::FoundWithFilter {
                    result: v,
                    newest_filtered: Some(latest),
                };
            }
            best.get_or_insert(v);
            prev_blocked = Some(v);
            continue;
        }
        best = Some(v);
        break;
    }
    match best.or(deprecated) {
        Some(result) => Picked::FoundWithFilter {
            result,
            newest_filtered: Some(latest),
        },
        None => Picked::Err(PickError::AllVersionsTooRecent),
    }
}

/// bun `find_best_version` (no gate): an exact version directly; else `latest` if it satisfies
/// and is not deprecated (for a prerelease range only if it is the range's own version); else
/// the highest satisfying non-deprecated release, then prerelease; else the highest deprecated.
pub fn find_best_version<'m>(
    m: &'m PackageManifest,
    range: Option<&Range>,
) -> Option<&'m VersionManifest> {
    if let Some(exact) = range.and_then(Range::as_exact) {
        return m.find(exact);
    }
    let pre = range.is_some_and(has_prerelease);
    if let Some(latest) = m.dist_tag("latest")
        && !is_deprecated(latest)
        && (is_empty_group(range) || satisfies(range, &latest.version))
        && (!pre
            || range
                .and_then(left_version)
                .is_some_and(|l| same_version(l, &latest.version)))
    {
        return Some(latest);
    }
    let mut deprecated = None;
    for &kind in list_kinds(pre) {
        for v in newest_first(m, kind) {
            if satisfies(range, &v.version) {
                if !is_deprecated(v) {
                    return Some(v);
                }
                deprecated.get_or_insert(v);
            }
        }
    }
    deprecated
}

/// bun `find_best_version_with_filter`: [`find_best_version`] with the release-age gate.
pub fn find_best_version_with_filter<'m>(
    m: &'m PackageManifest,
    range: Option<&Range>,
    gate: Option<&AgeGate<'_>>,
) -> Picked<'m> {
    let Some(gate) = gate.filter(|_| !excluded_entirely(m, gate)) else {
        return find_best_version(m, range).map_or(Picked::Err(PickError::NotFound), Picked::Found);
    };
    if let Some(exact) = range.and_then(Range::as_exact) {
        return match m.find(exact) {
            Some(v) if blocked(m, v, gate) => Picked::Err(PickError::TooRecent),
            Some(v) => Picked::Found(v),
            None => Picked::Err(PickError::NotFound),
        };
    }
    let pre = range.is_some_and(has_prerelease);
    let mut newest_filtered: Option<&'m Version> = None;
    if let Some(latest) = m.dist_tag("latest")
        && (is_empty_group(range) || satisfies(range, &latest.version))
    {
        if blocked(m, latest, gate) {
            newest_filtered = Some(&latest.version);
        }
        if newest_filtered.is_none()
            && !is_deprecated(latest)
            && (!pre
                || range
                    .and_then(left_version)
                    .is_some_and(|l| same_version(l, &latest.version)))
        {
            return Picked::Found(latest);
        }
    }
    let mut deprecated: Option<&'m VersionManifest> = None;
    for &kind in list_kinds(pre) {
        if let Some(result) = search_version_list(
            m,
            newest_first(m, kind),
            range,
            gate,
            &mut newest_filtered,
            &mut deprecated,
        ) {
            return result;
        }
    }
    if let Some(result) = deprecated {
        return Picked::FoundWithFilter {
            result,
            newest_filtered,
        };
    }
    if newest_filtered.is_some() {
        return Picked::Err(PickError::AllVersionsTooRecent);
    }
    Picked::Err(PickError::NotFound)
}

fn search_version_list<'m>(
    m: &'m PackageManifest,
    list: impl Iterator<Item = &'m VersionManifest>,
    range: Option<&Range>,
    gate: &AgeGate<'_>,
    newest_filtered: &mut Option<&'m Version>,
    deprecated: &mut Option<&'m VersionManifest>,
) -> Option<Picked<'m>> {
    let stability_window = gate.min_age_ms.min(SEVEN_DAYS_MS);
    let mut prev_blocked: Option<&VersionManifest> = None;
    let mut best: Option<&'m VersionManifest> = None;
    for v in list {
        if !satisfies(range, &v.version) {
            continue;
        }
        if is_deprecated(v) {
            if blocked(m, v, gate) {
                newest_filtered.get_or_insert(&v.version);
            } else {
                deprecated.get_or_insert(v);
            }
            continue;
        }
        // Listed versions count as stable: they skip the gate and replace an unstable fallback.
        if gate.excludes.excludes_version(&m.name, &v.version) {
            best = Some(v);
            break;
        }
        if too_recent(m, v, gate) {
            newest_filtered.get_or_insert(&v.version);
            prev_blocked = Some(v);
        } else if let Some(prev) = prev_blocked {
            // Only look back up to seven days past the gate.
            if publish_ms(m, v)
                < gate
                    .now_ms
                    .saturating_sub(gate.min_age_ms.saturating_add(SEVEN_DAYS_MS))
            {
                best.get_or_insert(v);
                break;
            }
            if publish_ms(m, prev).saturating_sub(publish_ms(m, v)) >= stability_window {
                best = Some(v);
                break;
            }
            best.get_or_insert(v);
            prev_blocked = Some(v);
        } else {
            return Some(Picked::Found(v));
        }
    }
    best.map(|result| match newest_filtered {
        Some(_) => Picked::FoundWithFilter {
            result,
            newest_filtered: *newest_filtered,
        },
        None => Picked::Found(result),
    })
}
