//! Range intersection and subset tests. Port of bun `semver/intersects.rs`.

use std::cmp::Ordering;

use crate::range::{Comparator, Op, Range};
use crate::version::{Identifier, Version};

#[derive(Clone, Copy)]
struct Bound<'a> {
    version: &'a Version,
    inclusive: bool,
}

/// A bound whose version may be rewritten by [`Interval::canonical`].
#[derive(Clone)]
struct OwnedBound {
    version: Version,
    inclusive: bool,
}

#[derive(Clone, Copy, Default)]
struct Interval<'a> {
    lower: Option<Bound<'a>>,
    upper: Option<Bound<'a>>,
}

impl<'a> Interval<'a> {
    fn raise(&mut self, b: Bound<'a>) {
        let replace = match self.lower {
            None => true,
            Some(cur) => match b.version.cmp_precedence(cur.version) {
                Ordering::Greater => true,
                Ordering::Equal => !b.inclusive,
                Ordering::Less => false,
            },
        };
        if replace {
            self.lower = Some(b);
        }
    }

    fn cap(&mut self, b: Bound<'a>) {
        let replace = match self.upper {
            None => true,
            Some(cur) => match b.version.cmp_precedence(cur.version) {
                Ordering::Less => true,
                Ordering::Equal => !b.inclusive,
                Ordering::Greater => false,
            },
        };
        if replace {
            self.upper = Some(b);
        }
    }

    fn narrow(&mut self, c: &'a Comparator) {
        let bound = |inclusive| Bound {
            version: &c.version,
            inclusive,
        };
        match c.op {
            Op::Eq => {
                self.raise(bound(true));
                self.cap(bound(true));
            }
            Op::Gt => self.raise(bound(false)),
            Op::Gte => self.raise(bound(true)),
            Op::Lt => self.cap(bound(false)),
            Op::Lte => self.cap(bound(true)),
        }
    }

    fn of(set: &'a [Comparator]) -> Self {
        let mut i = Interval::default();
        i.and(set);
        i
    }

    fn and(&mut self, set: &'a [Comparator]) {
        for c in set {
            self.narrow(c);
        }
    }

    fn is_non_empty(&self) -> bool {
        let Some(u) = self.upper else {
            return true;
        };
        is_non_empty(
            self.lower.map(|l| (l.version, l.inclusive)),
            (u.version, u.inclusive),
        )
    }

    /// The same set of versions with each bound written one canonical way: `>1.MAX.MAX`
    /// becomes `>=2.0.0`, `<=1.MAX.MAX` and `<2.0.0` become `<2.0.0-0`, and `>=0.0.0` is no
    /// bound.
    fn canonical(&self) -> Canonical {
        let mut lower = self.lower.map(|b| OwnedBound {
            version: b.version.clone(),
            inclusive: b.inclusive,
        });
        let mut upper = self.upper.map(|b| OwnedBound {
            version: b.version.clone(),
            inclusive: b.inclusive,
        });
        if let Some(l) = &mut lower
            && !l.inclusive
            && is_last_patch(&l.version)
            && let Some(next) = after_last_patch(&l.version)
        {
            l.version = next;
            l.inclusive = true;
        }
        if lower
            .as_ref()
            .is_some_and(|l| l.inclusive && l.version.is_zero() && !l.version.is_prerelease())
        {
            lower = None;
        }
        if let Some(u) = upper.take() {
            upper = if u.inclusive && is_last_patch(&u.version) {
                after_last_patch(&u.version).map(|version| OwnedBound {
                    version,
                    inclusive: false,
                })
            } else {
                Some(u)
            };
        }
        // `^1` and `<=1` end at `<2.0.0-0`. Only below a prerelease of `2.0.0` has `<2.0.0` more in it.
        if let Some(u) = &mut upper {
            let lower_is_pre_of_upper = lower
                .as_ref()
                .is_some_and(|l| l.version.is_prerelease() && l.version.same_core(&u.version));
            if !u.inclusive && !u.version.is_prerelease() && !lower_is_pre_of_upper {
                u.version.pre = vec![Identifier::Numeric(0)];
            }
        }
        Canonical { lower, upper }
    }
}

fn is_non_empty(lower: Option<(&Version, bool)>, upper: (&Version, bool)) -> bool {
    // `0.0.0-0` is the lowest version, so a missing lower bound starts there.
    let lowest = Comparator::null_set().version;
    let (l, l_inclusive) = lower.unwrap_or((&lowest, true));
    match l.cmp_precedence(upper.0) {
        Ordering::Less => true,
        Ordering::Equal => l_inclusive && upper.1,
        Ordering::Greater => false,
    }
}

struct Canonical {
    lower: Option<OwnedBound>,
    upper: Option<OwnedBound>,
}

impl Canonical {
    fn is_non_empty(&self) -> bool {
        let Some(u) = &self.upper else {
            return true;
        };
        is_non_empty(
            self.lower.as_ref().map(|l| (&l.version, l.inclusive)),
            (&u.version, u.inclusive),
        )
    }

    /// Everything between the bounds of `self` is also between the bounds of `other`.
    fn is_within(&self, other: &Canonical) -> bool {
        let lower = match (&self.lower, &other.lower) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(a), Some(b)) => match a.version.cmp_precedence(&b.version) {
                Ordering::Greater => true,
                Ordering::Equal => b.inclusive || !a.inclusive,
                Ordering::Less => false,
            },
        };
        let upper = match (&self.upper, &other.upper) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(a), Some(b)) => match a.version.cmp_precedence(&b.version) {
                Ordering::Less => true,
                Ordering::Equal => b.inclusive || !a.inclusive,
                Ordering::Greater => false,
            },
        };
        lower && upper
    }

    /// The only prereleases that satisfy a set are those of a comparator that names one:
    /// whether `set` has such a comparator for each bound of `self` that is a prerelease.
    fn prereleases_can_satisfy(&self, set: &[Comparator]) -> bool {
        let lower = self.lower.as_ref().filter(|it| it.version.is_prerelease());
        // No version is below `1.2.3-0`.
        let upper = self.upper.as_ref().filter(|it| {
            it.version.is_prerelease()
                && (it.inclusive || !matches!(it.version.pre.as_slice(), [Identifier::Numeric(0)]))
        });
        lower.into_iter().chain(upper).all(|bound| {
            set.iter()
                .any(|c| c.version.is_prerelease() && c.version.same_core(&bound.version))
        })
    }
}

fn is_last_patch(v: &Version) -> bool {
    v.patch == u64::MAX && !v.is_prerelease()
}

/// The version after `major.minor.MAX`, if there is one.
fn after_last_patch(v: &Version) -> Option<Version> {
    Some(match v.minor.checked_add(1) {
        Some(minor) => Version::new(v.major, minor, 0),
        None => Version::new(v.major.checked_add(1)?, 0, 0),
    })
}

fn exact_version(set: &[Comparator]) -> Option<&Version> {
    match set {
        [
            Comparator {
                op: Op::Eq,
                version,
            },
        ] => Some(version),
        _ => None,
    }
}

/// `*` (a lone `>=0.0.0`). `>=0.0.0-0` is not `*`.
fn is_star(set: &[Comparator]) -> bool {
    matches!(set, [Comparator { op: Op::Gte, version }] if version.is_zero() && !version.is_prerelease())
}

impl Range {
    /// `satisfies`, except that `*` admits a prerelease too.
    fn admits(&self, v: &Version) -> bool {
        self.satisfies(v) || (v.is_prerelease() && self.sets.iter().any(|s| is_star(s)))
    }

    fn admits_an_exact_version_of(&self, other: &Range) -> bool {
        other
            .sets
            .iter()
            .filter_map(|s| exact_version(s))
            .any(|v| self.admits(v))
    }

    /// Whether some version satisfies both ranges (bun `Group::intersects`, used for override
    /// selectors and peer checks). An exact version is tested with `satisfies` (where `*` also
    /// admits a prerelease); two ranges only compare their bounds, so the prerelease rule is
    /// not modelled between them.
    pub fn intersects(&self, other: &Range) -> bool {
        if self.admits_an_exact_version_of(other) || other.admits_an_exact_version_of(self) {
            return true;
        }
        let is_range = |set: &&Vec<Comparator>| exact_version(set).is_none();
        for a in self.sets.iter().filter(is_range) {
            let base = Interval::of(a);
            if !base.is_non_empty() {
                continue;
            }
            for b in other.sets.iter().filter(is_range) {
                let mut i = base;
                i.and(b);
                if i.is_non_empty() {
                    return true;
                }
            }
        }
        false
    }

    /// Whether every version that satisfies `self` also satisfies `other` (bun
    /// `Group::is_subset_of`). Like npm `subset`, each `||` alternative of `self` has to be
    /// within a single alternative of `other`, so `>=1` is not a subset of `1 || >=2`.
    pub fn is_subset_of(&self, other: &Range) -> bool {
        let doms: Vec<(Canonical, &[Comparator])> = other
            .sets
            .iter()
            .map(|set| (Interval::of(set).canonical(), set.as_slice()))
            .collect();
        'alternatives: for set in &self.sets {
            let sub = Interval::of(set).canonical();
            if !sub.is_non_empty() {
                continue;
            }
            for (dom, dom_set) in &doms {
                if sub.is_within(dom) && sub.prereleases_can_satisfy(dom_set) {
                    continue 'alternatives;
                }
            }
            return false;
        }
        true
    }
}
