//! Allocation-free version precedence and range evaluation over `fnpm_semver`'s data types.
//!
//! Manifest parsing and version picking run for every version of every packument, so they use
//! these helpers directly: a version key is parsed once into [`Version`], compared in place, and a
//! [`Range`] is evaluated comparator by comparator with node-semver's prerelease rule.

use std::cmp::Ordering;
use std::fmt::Write as _;

use fnpm_semver::{Comparator, Identifier, Op, Range, Version};

/// Parses a version key from a registry document (`1.2.3`, `1.2.3-beta.1+build`, `v1.2.3`,
/// `=1.2.3`). Leading zeros are accepted, as bun does; partial versions are not.
pub fn parse_version(input: &str) -> Option<Version> {
    let mut s = input.trim();
    s = s.strip_prefix('=').unwrap_or(s).trim_start();
    s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let (core, build) = match s.split_once('+') {
        Some((core, build)) => (core, Some(build)),
        None => (s, None),
    };
    let (numbers, pre) = match core.split_once('-') {
        Some((numbers, pre)) => (numbers, Some(pre)),
        None => (core, None),
    };
    let mut parts = numbers.split('.');
    let major = parse_number(parts.next()?)?;
    let minor = parse_number(parts.next()?)?;
    let patch = parse_number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let mut version = Version::new(major, minor, patch);
    if let Some(pre) = pre {
        for ident in pre.split('.') {
            if !is_identifier(ident) {
                return None;
            }
            version.pre.push(match ident.parse::<u64>() {
                Ok(n) if ident.bytes().all(|b| b.is_ascii_digit()) => Identifier::Numeric(n),
                _ => Identifier::AlphaNumeric(ident.into()),
            });
        }
    }
    if let Some(build) = build {
        if !build.split('.').all(is_identifier) {
            return None;
        }
        version.build = build.into();
    }
    Some(version)
}

fn parse_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Semver precedence (build metadata ignored).
pub fn cmp(a: &Version, b: &Version) -> Ordering {
    a.major
        .cmp(&b.major)
        .then(a.minor.cmp(&b.minor))
        .then(a.patch.cmp(&b.patch))
        .then_with(|| cmp_pre(&a.pre, &b.pre))
}

fn cmp_pre(a: &[Identifier], b: &[Identifier]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    for (x, y) in a.iter().zip(b) {
        let order = match (x, y) {
            (Identifier::Numeric(x), Identifier::Numeric(y)) => x.cmp(y),
            (Identifier::Numeric(_), Identifier::AlphaNumeric(_)) => Ordering::Less,
            (Identifier::AlphaNumeric(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::AlphaNumeric(x), Identifier::AlphaNumeric(y)) => {
                x.as_bytes().cmp(y.as_bytes())
            }
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    a.len().cmp(&b.len())
}

/// Same `major.minor.patch`.
fn same_tuple(a: &Version, b: &Version) -> bool {
    a.major == b.major && a.minor == b.minor && a.patch == b.patch
}

fn comparator_matches(c: &Comparator, v: &Version) -> bool {
    let order = cmp(v, &c.version);
    match c.op {
        Op::Eq => order == Ordering::Equal,
        Op::Lt => order == Ordering::Less,
        Op::Lte => order != Ordering::Greater,
        Op::Gt => order == Ordering::Greater,
        Op::Gte => order != Ordering::Less,
    }
}

/// node-semver `satisfies`: some comparator set matches, and a prerelease only matches a set that
/// names a prerelease of the same `major.minor.patch` (unless `include_prerelease`).
pub fn satisfies(range: &Range, v: &Version, include_prerelease: bool) -> bool {
    range.sets.iter().any(|set| {
        if !set.iter().all(|c| comparator_matches(c, v)) {
            return false;
        }
        if v.pre.is_empty() || include_prerelease {
            return true;
        }
        set.iter()
            .any(|c| !c.version.pre.is_empty() && same_tuple(&c.version, v))
    })
}

/// The `-0` of a desugared upper bound (`^1.2.3` is `>=1.2.3 <2.0.0-0`), which the user did
/// not write.
fn is_desugared_bound(c: &Comparator) -> bool {
    c.op == Op::Lt && matches!(c.version.pre.as_slice(), [Identifier::Numeric(0)])
}

/// True if the range names a prerelease (bun's `Query::Flags::PRE`); desugared `<X.Y.Z-0`
/// upper bounds do not count.
pub fn has_prerelease(range: &Range) -> bool {
    range
        .sets
        .iter()
        .flatten()
        .any(|c| !c.version.pre.is_empty() && !is_desugared_bound(c))
}

/// The version of the first comparator (bun's `group.head.head.range.left`).
pub fn left_version(range: &Range) -> Option<&Version> {
    range
        .sets
        .first()
        .and_then(|set| set.first())
        .map(|c| &c.version)
}

/// `Some` for a range that is one exact version.
pub fn exact(range: &Range) -> Option<&Version> {
    match range.sets.as_slice() {
        [set] => match set.as_slice() {
            [
                Comparator {
                    op: Op::Eq,
                    version,
                },
            ] => Some(version),
            _ => None,
        },
        _ => None,
    }
}

/// The canonical text of a version: `1.2.3`, `1.2.3-beta.1`, `1.2.3-rc+build`.
pub fn format(v: &Version) -> String {
    let mut out = String::with_capacity(16);
    write_version(&mut out, v);
    out
}

/// Appends [`format`]'s text to `out`.
pub fn write_version(out: &mut String, v: &Version) {
    let _ = write!(out, "{}.{}.{}", v.major, v.minor, v.patch);
    for (i, ident) in v.pre.iter().enumerate() {
        out.push(if i == 0 { '-' } else { '.' });
        match ident {
            Identifier::Numeric(n) => {
                let _ = write!(out, "{n}");
            }
            Identifier::AlphaNumeric(s) => out.push_str(s),
        }
    }
    if !v.build.is_empty() {
        out.push('+');
        out.push_str(&v.build);
    }
}

/// The prerelease tag before its first dot (`beta` of `1.0.0-beta.2`), as text.
pub fn pre_tag(v: &Version) -> Option<String> {
    v.pre.first().map(|ident| match ident {
        Identifier::Numeric(n) => n.to_string(),
        Identifier::AlphaNumeric(s) => s.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        parse_version(s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    fn c(op: Op, s: &str) -> Comparator {
        Comparator { op, version: v(s) }
    }

    #[test]
    fn parses_registry_keys() {
        assert_eq!(v("1.2.3"), Version::new(1, 2, 3));
        assert_eq!(v(" v1.2.3 "), Version::new(1, 2, 3));
        assert_eq!(v("=1.2.3"), Version::new(1, 2, 3));
        assert_eq!(v("01.0.0"), Version::new(1, 0, 0));
        let pre = v("1.0.0-beta.11+exp.sha.5114f85");
        assert_eq!(
            pre.pre,
            vec![
                Identifier::AlphaNumeric("beta".into()),
                Identifier::Numeric(11)
            ]
        );
        assert_eq!(&*pre.build, "exp.sha.5114f85");
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "a.b.c",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3+",
            "1.2.3-ä",
        ] {
            assert!(parse_version(bad).is_none(), "{bad:?} should not parse");
        }
        assert!(parse_version("99999999999999999999.0.0").is_none());
    }

    #[test]
    fn precedence_follows_semver_spec() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "2.0.0",
        ];
        for pair in ordered.windows(2) {
            assert_eq!(
                cmp(&v(pair[0]), &v(pair[1])),
                Ordering::Less,
                "{} < {}",
                pair[0],
                pair[1]
            );
            assert_eq!(cmp(&v(pair[1]), &v(pair[0])), Ordering::Greater);
        }
        assert_eq!(cmp(&v("1.0.0+a"), &v("1.0.0+b")), Ordering::Equal);
    }

    #[test]
    fn satisfies_with_prerelease_rule() {
        let range = Range {
            sets: vec![vec![c(Op::Gte, "1.2.3-beta.1"), c(Op::Lt, "2.0.0-0")]],
        };
        assert!(satisfies(&range, &v("1.2.3-beta.2"), false));
        assert!(satisfies(&range, &v("1.5.0"), false));
        assert!(!satisfies(&range, &v("1.5.0-beta"), false));
        assert!(satisfies(&range, &v("1.5.0-beta"), true));
        assert!(!satisfies(&range, &v("2.0.0"), false));

        let any = Range { sets: vec![vec![]] };
        assert!(satisfies(&any, &v("0.0.1"), false));
        assert!(!satisfies(&any, &v("1.0.0-rc.1"), false));
        assert!(satisfies(&any, &v("1.0.0-rc.1"), true));

        let either = Range {
            sets: vec![vec![c(Op::Eq, "1.0.0")], vec![c(Op::Gt, "3.0.0")]],
        };
        assert!(satisfies(&either, &v("1.0.0"), false));
        assert!(satisfies(&either, &v("3.0.1"), false));
        assert!(!satisfies(&either, &v("2.0.0"), false));
        assert!(has_prerelease(&range));
        assert!(!has_prerelease(&either));
        let caret = Range {
            sets: vec![vec![c(Op::Gte, "1.2.3"), c(Op::Lt, "2.0.0-0")]],
        };
        assert!(!has_prerelease(&caret));
        assert_eq!(
            exact(&Range {
                sets: vec![vec![c(Op::Eq, "1.0.0")]]
            }),
            Some(&v("1.0.0"))
        );
        assert_eq!(exact(&either), None);
    }

    #[test]
    fn formats_canonically() {
        for s in [
            "1.2.3",
            "1.2.3-beta.1",
            "0.0.0-0",
            "1.0.0-rc.1+build.5",
            "1.0.0+x",
        ] {
            assert_eq!(format(&v(s)), s);
        }
        assert_eq!(format(&v("v01.2.3")), "1.2.3");
        assert_eq!(pre_tag(&v("1.0.0-beta.2")).as_deref(), Some("beta"));
        assert_eq!(pre_tag(&v("1.0.0")), None);
    }
}
