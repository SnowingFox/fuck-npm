//! Picking versions from a sorted version list: `max_satisfying` and bun `find_best_version`.

use std::ops::Range as Span;

use crate::range::{Op, Range, SatisfyOptions};
use crate::version::Version;

/// The highest version in `versions` that satisfies `range` (node-semver `maxSatisfying`).
/// Works on any order; prefer [`max_satisfying_sorted`] for a sorted slice.
pub fn max_satisfying<'a, I>(
    versions: I,
    range: &Range,
    opts: SatisfyOptions,
) -> Option<&'a Version>
where
    I: IntoIterator<Item = &'a Version>,
{
    versions
        .into_iter()
        .filter(|v| range.satisfies_with(v, opts))
        .max()
}

/// [`max_satisfying`] over `versions` sorted ascending (by [`Ord`] or by precedence).
///
/// An exact range is a binary search; otherwise the slice is narrowed to the range's lower and
/// upper bounds by binary search and scanned from the top, so the common case touches only a
/// handful of versions.
pub fn max_satisfying_sorted<'a>(
    versions: &'a [Version],
    range: &Range,
    opts: SatisfyOptions,
) -> Option<&'a Version> {
    if let Some(exact) = range.as_exact() {
        return find_exact(versions, |v| v, exact);
    }
    let span = candidates(versions, |v| v, range);
    versions[span]
        .iter()
        .rev()
        .find(|v| range.satisfies_with(v, opts))
}

/// bun `PackageManifest::find_best_version` over `items` sorted ascending by version: the
/// version `bun install` picks for `range`.
///
/// 1. An exact range returns that version if it exists, deprecated or not.
/// 2. `latest` wins when it is not deprecated, satisfies the range, and either the range has no
///    prerelease or its first comparator is exactly `latest`.
/// 3. Otherwise the highest satisfying release, then (only when the range names a prerelease)
///    the highest satisfying prerelease.
/// 4. A deprecated version is only returned when every match is deprecated.
pub fn pick_best<'a, T, V, D>(
    items: &'a [T],
    version_of: V,
    range: &Range,
    latest: Option<&'a T>,
    is_deprecated: D,
) -> Option<&'a T>
where
    V: Fn(&T) -> &Version,
    D: Fn(&T) -> bool,
{
    if let Some(exact) = range.as_exact() {
        return find_exact(items, &version_of, exact);
    }
    let has_pre = range.has_prerelease();
    if let Some(latest) = latest {
        let v = version_of(latest);
        let first_is_latest = || {
            let zero = Version::new(0, 0, 0);
            let first = range
                .sets
                .first()
                .and_then(|s| s.first())
                .map_or(&zero, |c| &c.version);
            first == v
        };
        if !is_deprecated(latest) && range.satisfies(v) && (!has_pre || first_is_latest()) {
            return Some(latest);
        }
    }

    let span = candidates(items, &version_of, range);
    let window = &items[span];
    let mut deprecated = None;
    let passes: &[bool] = if has_pre { &[false, true] } else { &[false] };
    for &prereleases in passes {
        for item in window.iter().rev() {
            let v = version_of(item);
            if v.is_prerelease() != prereleases || !range.satisfies(v) {
                continue;
            }
            if !is_deprecated(item) {
                return Some(item);
            }
            deprecated.get_or_insert(item);
        }
    }
    deprecated
}

/// [`pick_best`] for a plain sorted version list with no deprecations.
pub fn best_version<'a>(
    versions: &'a [Version],
    range: &Range,
    latest: Option<&'a Version>,
) -> Option<&'a Version> {
    pick_best(versions, |v| v, range, latest, |_| false)
}

/// The last item whose version has the same precedence as `exact`.
fn find_exact<'a, T, V>(items: &'a [T], version_of: V, exact: &Version) -> Option<&'a T>
where
    V: Fn(&T) -> &Version,
{
    let end = items.partition_point(|it| version_of(it).cmp_precedence(exact).is_le());
    let last = items.get(end.checked_sub(1)?)?;
    version_of(last).eq_precedence(exact).then_some(last)
}

/// The sub-slice of a sorted list that can contain versions satisfying `range`.
fn candidates<T, V>(items: &[T], version_of: V, range: &Range) -> Span<usize>
where
    V: Fn(&T) -> &Version,
{
    // Tiny lists are cheaper to scan than to bisect.
    if items.len() <= 8 {
        return 0..items.len();
    }
    let end = match range.upper_bound() {
        Some((upper, true)) => {
            items.partition_point(|it| version_of(it).cmp_precedence(upper).is_le())
        }
        Some((upper, false)) => {
            items.partition_point(|it| version_of(it).cmp_precedence(upper).is_lt())
        }
        None => items.len(),
    };
    let start = match lower_bound(range) {
        Some(lower) => {
            items[..end].partition_point(|it| version_of(it).cmp_precedence(lower).is_lt())
        }
        None => 0,
    };
    start..end
}

/// The lowest version that might satisfy the range, or `None` when unbounded below.
fn lower_bound(range: &Range) -> Option<&Version> {
    let mut lowest: Option<&Version> = None;
    for set in &range.sets {
        let mut set_lower: Option<&Version> = None;
        for c in set {
            match c.op {
                // `>=0.0.0` also admits prereleases of 0.0.0, which are below it.
                Op::Gte if c.version.is_zero() && !c.version.is_prerelease() => {}
                Op::Gt | Op::Gte | Op::Eq => {
                    if set_lower.is_none_or(|l| c.version.cmp_precedence(l).is_gt()) {
                        set_lower = Some(&c.version);
                    }
                }
                Op::Lt | Op::Lte => {}
            }
        }
        let set_lower = set_lower?;
        if lowest.is_none_or(|l| set_lower.cmp_precedence(l).is_lt()) {
            lowest = Some(set_lower);
        }
    }
    lowest
}
