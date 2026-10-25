//! Behaviour of the public API beyond the recorded bun / node-semver tables.

use std::cmp::Ordering;

use fnpm_semver::{
    ChangedVersion, Comparator, Error, Identifier, Op, PinnedVersion, Range, ReleaseType,
    SatisfyOptions, Version, best_version, max_satisfying, max_satisfying_sorted, pick_best,
    satisfies,
};

fn v(s: &str) -> Version {
    Version::parse_loose(s).unwrap_or_else(|e| panic!("{e}"))
}

fn r(s: &str) -> Range {
    Range::parse(s).unwrap()
}

fn vs(list: &[&str]) -> Vec<Version> {
    let mut out: Vec<Version> = list.iter().map(|s| v(s)).collect();
    out.sort();
    out
}

const INCLUDE: SatisfyOptions = SatisfyOptions {
    include_prerelease: true,
};

#[test]
fn version_fields_and_display() {
    let x = v(" v1.2.3-rc.1.x-y+build.5 ");
    assert_eq!((x.major, x.minor, x.patch), (1, 2, 3));
    assert_eq!(
        x.pre,
        vec![
            Identifier::AlphaNumeric("rc".into()),
            Identifier::Numeric(1),
            Identifier::AlphaNumeric("x-y".into())
        ]
    );
    assert_eq!(&*x.build, "build.5");
    assert_eq!(x.to_string(), "1.2.3-rc.1.x-y+build.5");
    assert!(x.is_prerelease());
    assert_eq!(Version::new(1, 2, 3).to_string(), "1.2.3");
    assert_eq!("=1.2.3".parse::<Version>().unwrap(), Version::new(1, 2, 3));
    // A non-canonical number keeps its spelling but compares numerically.
    let padded = v("1.0.0-01");
    assert_eq!(padded.pre, vec![Identifier::AlphaNumeric("01".into())]);
    assert_eq!(padded.to_string(), "1.0.0-01");
    assert!(padded.eq_precedence(&v("1.0.0-1")));
    assert_ne!(padded, v("1.0.0-1"));
    assert_ne!(padded.cmp(&v("1.0.0-1")), Ordering::Equal);
}

#[test]
fn strict_and_loose_parsing() {
    for ok in [
        "1.2.3",
        "v1.2.3",
        "=1.2.3",
        " 1.2.3 ",
        "1.2.3-0",
        "1.2.3-rc.1+b.2",
        "1.2.3+build-1",
    ] {
        assert!(Version::parse(ok).is_ok(), "{ok}");
    }
    for loose_only in [
        "1.0.0rc1",
        "1.2.3beta",
        "01.2.3",
        "1.02.3",
        "1.2.3-01",
        "18446744073709551615.0.0",
        "vv=1.2.3",
        "v=1.2.3",
        "v 1.2.3",
    ] {
        assert!(Version::parse(loose_only).is_err(), "{loose_only}");
        assert!(Version::parse_loose(loose_only).is_ok(), "{loose_only}");
    }
    for bad in [
        "",
        "1",
        "1.2",
        "1.2.x",
        "^1.2.3",
        "1.2.3.4",
        "1.2.3 junk",
        "18446744073709551616.0.0",
        "1.2.3,",
    ] {
        assert_eq!(
            Version::parse_loose(bad),
            Err(Error::InvalidVersion(bad.to_owned())),
            "{bad}"
        );
    }
    assert_eq!(v("1.0.0rc.1").to_string(), "1.0.0-rc.1");
    assert_eq!(v("1.0.0rc.1+build").to_string(), "1.0.0-rc.1+build");
}

#[test]
fn precedence_and_ord() {
    let order = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
    ];
    for w in order.windows(2) {
        assert_eq!(
            v(w[0]).cmp_precedence(&v(w[1])),
            Ordering::Less,
            "{} < {}",
            w[0],
            w[1]
        );
        assert!(v(w[0]) < v(w[1]));
    }
    // Build metadata is ignored by precedence and breaks ties in `Ord`.
    assert!(v("1.0.0+a").eq_precedence(&v("1.0.0+b")));
    assert!(v("1.0.0+a") < v("1.0.0+b"));
    assert!(v("1.0.0") < v("1.0.0+a"));
    // Numbers that overflow u64 compare as text, after every number.
    assert!(v("1.0.0-18446744073709551615") < v("1.0.0-18446744073709551616"));
    assert!(v("1.0.0-99999999999999999999") > v("1.0.0-1"));
}

#[test]
fn coerce() {
    // bun Version.rs `coerce` test: node-semver 6.3.1 and 7.8.5 answers.
    let cases: &[(&str, Option<[u64; 3]>)] = &[
        ("0", Some([0, 0, 0])),
        ("v1", Some([1, 0, 0])),
        ("x 16.3 y", Some([16, 3, 0])),
        ("16.3.0-alpha", Some([16, 3, 0])),
        ("1.2.3.4", Some([1, 2, 3])),
        ("1.2.3\n4", Some([1, 2, 3])),
        ("a1b2", Some([1, 0, 0])),
        ("1.", Some([1, 0, 0])),
        ("1..2", Some([1, 0, 0])),
        (".1", Some([1, 0, 0])),
        ("1.2.33333333333333333", Some([1, 2, 0])),
        ("1.22222222222222222.3", Some([1, 0, 0])),
        ("12345678901234567.1", Some([1, 0, 0])),
        ("99999999999999999.2.3 4.5", Some([2, 3, 0])),
        (
            "1.2.3456789012345678.9",
            Some([1, 2, 3_456_789_012_345_678]),
        ),
        ("9007199254740991", Some([9_007_199_254_740_991, 0, 0])),
        ("9007199254740992", None),
        ("1.9007199254740992", None),
        ("01.2.3", None),
        ("1.02.3", None),
        ("1.2.03", None),
        ("00", None),
        ("", None),
        ("abc", None),
        ("12345678901234567", None),
    ];
    for (text, want) in cases {
        let have = Version::coerce(text).map(|it| [it.major, it.minor, it.patch]);
        assert_eq!(have, *want, "{text:?}");
    }
}

/// bun `test/cli/install/bun-pm-version.test.ts`: `semver.inc(version, increment, preid)` of
/// node-semver 7.8.1.
#[test]
fn increments_like_bun_pm_version() {
    let cases = [
        ("1.0.0-rc.1", "major", "1.0.0"),
        ("1.2.0-rc.1", "major", "2.0.0"),
        ("1.2.0-rc.1", "minor", "1.2.0"),
        ("1.2.3-rc.1", "minor", "1.3.0"),
        ("1.2.3-beta.1", "patch", "1.2.3"),
        ("1.2.3", "major", "2.0.0"),
        ("1.2.3", "minor", "1.3.0"),
        ("1.2.3", "patch", "1.2.4"),
        ("1.2.3", "premajor", "2.0.0-0"),
        ("1.2.3", "preminor", "1.3.0-0"),
        ("1.2.3", "prepatch", "1.2.4-0"),
        ("1.2.3", "prerelease", "1.2.4-0"),
        ("1.2.3", "premajor beta", "2.0.0-beta.0"),
        ("1.2.3", "preminor beta", "1.3.0-beta.0"),
        ("1.2.3", "prepatch beta", "1.2.4-beta.0"),
        ("1.2.3", "prerelease beta", "1.2.4-beta.0"),
        ("1.2.3-beta.1", "premajor", "2.0.0-0"),
        ("1.2.3-beta.1", "preminor", "1.3.0-0"),
        ("1.2.3-beta.1", "prepatch", "1.2.4-0"),
        ("1.2.3-beta.1", "prepatch rc", "1.2.4-rc.0"),
        ("1.2.3-beta.1", "prerelease", "1.2.3-beta.2"),
        ("1.2.3-beta.1", "prerelease beta", "1.2.3-beta.2"),
        ("1.2.3-beta.1", "prerelease rc", "1.2.3-rc.0"),
        ("1.0.0-4294967295", "prerelease rc", "1.0.0-rc.0"),
        ("1.2.3-0", "prerelease beta", "1.2.3-beta.0"),
        ("1.2.3-0", "prerelease", "1.2.3-1"),
        ("1.2.3-beta", "prerelease", "1.2.3-beta.0"),
        ("1.2.3-beta", "prerelease beta", "1.2.3-beta.0"),
        ("1.0.0-beta.x", "prerelease beta", "1.0.0-beta.0"),
        ("1.0.0-5", "prerelease 5", "1.0.0-5.0"),
        ("1.0.0", "prerelease 0", "1.0.1-0.0"),
        ("1.0.0", "prerelease 01a", "1.0.1-01a.0"),
        ("1.0.0", "prerelease 0-1", "1.0.1-0-1.0"),
        ("1.2.3", "prerelease a.b", "1.2.4-a.b.0"),
        ("1.0.0-x.7.z.92", "prerelease", "1.0.0-x.7.z.93"),
        ("1.0.0-alpha.1.beta", "prerelease", "1.0.0-alpha.2.beta"),
        ("1.0.0-alpha.beta", "prerelease", "1.0.0-alpha.beta.0"),
        ("1.0.0-beta.1.2", "prerelease beta", "1.0.0-beta.1.3"),
        ("1.2.3+build.5", "patch", "1.2.4"),
        ("1.2.3-beta.1+build.5", "prerelease", "1.2.3-beta.2"),
        ("v1.2.3", "patch", "1.2.4"),
        (
            "1.0.0-beta.4294967295",
            "prerelease",
            "1.0.0-beta.4294967296",
        ),
        ("1.0.0-4294967295", "prerelease", "1.0.0-4294967296"),
        (
            "1.0.0-canary.20250101123456",
            "prerelease",
            "1.0.0-canary.20250101123457",
        ),
        ("1.0.0-20250101123456", "prerelease", "1.0.0-20250101123457"),
        (
            "1.0.0-beta.9007199254740990",
            "prerelease",
            "1.0.0-beta.9007199254740991",
        ),
        (
            "1.0.0-beta.9007199254740991",
            "prerelease",
            "1.0.0-beta.9007199254740991.0",
        ),
        (
            "1.0.0-beta.18446744073709551614",
            "prerelease",
            "1.0.0-beta.18446744073709551614.0",
        ),
        (
            "1.0.0-18446744073709551615",
            "prerelease",
            "1.0.0-18446744073709551615.0",
        ),
        (
            "1.0.9007199254740991-0",
            "prerelease",
            "1.0.9007199254740991-1",
        ),
        // `pm version` previews
        ("1.0.3-alpha.1", "patch", "1.0.3"),
        ("1.0.3-alpha.1", "prerelease beta", "1.0.3-beta.0"),
        ("1.0.3-alpha.1", "prepatch beta", "1.0.4-beta.0"),
        ("1.0.3-1", "prerelease abcd", "1.0.3-abcd.0"),
        ("1.0.0-alpha.0", "prepatch", "1.0.1-0"),
        ("1.0.0-alpha.0", "premajor", "2.0.0-0"),
    ];
    for (version, increment, want) in cases {
        let mut words = increment.split(' ');
        let release = release(words.next().unwrap());
        let preid = words.next();
        let got = v(version)
            .try_inc(release, preid)
            .unwrap_or_else(|e| panic!("{version} {increment}: {e}"));
        assert_eq!(got.to_string(), want, "{version} + {increment}");
        assert_eq!(v(version).inc(release, preid).to_string(), want);
    }
}

fn release(s: &str) -> ReleaseType {
    match s {
        "major" => ReleaseType::Major,
        "minor" => ReleaseType::Minor,
        "patch" => ReleaseType::Patch,
        "premajor" => ReleaseType::Premajor,
        "preminor" => ReleaseType::Preminor,
        "prepatch" => ReleaseType::Prepatch,
        "prerelease" => ReleaseType::Prerelease,
        other => panic!("{other}"),
    }
}

#[test]
fn invalid_increments() {
    for preid in ["be ta", "beta!", "a..b", ".a", "01", "00", "beta.007"] {
        assert!(
            v("1.0.0")
                .try_inc(ReleaseType::Prerelease, Some(preid))
                .is_err(),
            "{preid:?}"
        );
        // `inc` ignores the bad preid.
        assert_eq!(
            v("1.0.0")
                .inc(ReleaseType::Prerelease, Some(preid))
                .to_string(),
            "1.0.1-0"
        );
    }
    // Above Number.MAX_SAFE_INTEGER, like `bun pm version`.
    assert!(
        v("1.0.9007199254740992")
            .try_inc(ReleaseType::Patch, None)
            .is_err()
    );
    assert!(
        v("1.0.0-a..b")
            .try_inc(ReleaseType::Prerelease, None)
            .is_err()
    );
    assert_eq!(
        v("18446744073709551615.0.0")
            .inc(ReleaseType::Major, None)
            .to_string(),
        "18446744073709551615.0.0"
    );
    // An empty preid is no preid.
    assert_eq!(
        v("1.2.3")
            .try_inc(ReleaseType::Prerelease, Some(""))
            .unwrap()
            .to_string(),
        "1.2.4-0"
    );
}

#[test]
fn diff() {
    let d = |a: &str, b: &str| v(a).diff(&v(b));
    assert_eq!(d("1.0.0", "2.0.0"), Some(ChangedVersion::Major));
    assert_eq!(d("1.0.0", "1.1.0"), Some(ChangedVersion::Minor));
    assert_eq!(d("1.0.0", "1.0.1"), Some(ChangedVersion::Patch));
    assert_eq!(d("1.0.0", "1.0.0-rc.1"), Some(ChangedVersion::Pre));
    assert_eq!(d("1.0.0-rc.1", "1.0.0-rc.2"), Some(ChangedVersion::Pre));
    assert_eq!(
        d("1.0.0-rc.1+a", "1.0.0-rc.1+b"),
        Some(ChangedVersion::Build)
    );
    assert_eq!(d("1.0.0-rc.1+a", "1.0.0-rc.1+a"), None);
    // bun only looks at build metadata of prereleases.
    assert_eq!(d("1.0.0+a", "1.0.0+b"), None);
}

#[test]
fn which_version_is_pinned() {
    let cases = [
        ("1.2.3", PinnedVersion::Patch),
        ("v1.2.3", PinnedVersion::Patch),
        ("=1.2.3", PinnedVersion::Patch),
        (" 1.2.3 ", PinnedVersion::Patch),
        ("1.2.3-beta.1+b", PinnedVersion::Patch),
        ("1.2", PinnedVersion::Minor),
        ("1", PinnedVersion::Major),
        ("^1.2.3", PinnedVersion::Major),
        ("^ 1.2.3", PinnedVersion::Major),
        ("~1.2.3", PinnedVersion::Minor),
        ("~ 1.2.3", PinnedVersion::Minor),
        ("~1", PinnedVersion::Minor),
        ("~1.2", PinnedVersion::Minor),
        ("~v1.2.3", PinnedVersion::Major),
        ("~", PinnedVersion::Major),
        (">=1.2.3", PinnedVersion::Major),
        ("1.2.3 || 2", PinnedVersion::Major),
        ("1.2.3 - 2", PinnedVersion::Major),
        ("1.x", PinnedVersion::Major),
        ("1.2.x", PinnedVersion::Major),
        ("", PinnedVersion::Major),
        ("latest", PinnedVersion::Major),
    ];
    for (input, want) in cases {
        assert_eq!(Version::which_version_is_pinned(input), want, "{input:?}");
    }
    assert!(Version::is_tagged_version_only(" latest "));
    assert!(Version::is_tagged_version_only("next2"));
    assert!(!Version::is_tagged_version_only("1.0.0"));
    assert!(!Version::is_tagged_version_only("v12-lts"));
    assert!(!Version::is_tagged_version_only(""));
}

#[test]
fn range_desugaring() {
    let cases = [
        ("^1.2.3", ">=1.2.3 <2.0.0-0"),
        ("^0.2.3", ">=0.2.3 <0.3.0-0"),
        ("^0.0.3", ">=0.0.3 <0.0.4-0"),
        ("^0.0", ">=0.0.0 <0.1.0-0"),
        ("^0.0.x", ">=0.0.0 <0.1.0-0"),
        ("^1.2.3-beta.2", ">=1.2.3-beta.2 <2.0.0-0"),
        ("~1", ">=1.0.0 <2.0.0-0"),
        ("~1.2", ">=1.2.0 <1.3.0-0"),
        ("~>1.2.3", ">=1.2.3 <1.3.0-0"),
        ("1.2", "<1.3.0-0 >=1.2.0"),
        ("1.x", "<2.0.0-0 >=1.0.0"),
        ("*", ">=0.0.0"),
        ("", "*"),
        ("latest", "*"),
        ("1.2.3", "=1.2.3"),
        ("=v1.2.3", "=1.2.3"),
        ("1 - 2", ">=1.0.0 <3.0.0-0"),
        ("1.2.3 - 2.3.4", ">=1.2.3 <=2.3.4"),
        ("1.2.3 - 2.3", ">=1.2.3 <2.4.0-0"),
        ("1.0.0 - x", ">=1.0.0"),
        (">1", ">=2.0.0"),
        (">1.2", ">=1.3.0"),
        ("<1.2", "<1.2.0-0"),
        ("<=1.2", "<1.3.0-0"),
        (">=1.2", ">=1.2.0"),
        ("<x", "<0.0.0-0"),
        ("< =1", "<2.0.0-0"),
        ("1 || 2", "<2.0.0-0 >=1.0.0 || <3.0.0-0 >=2.0.0"),
        ("^99999999999999999999", "<0.0.0-0"),
    ];
    for (input, want) in cases {
        assert_eq!(r(input).to_string(), want, "{input:?}");
        // The printed form parses back to the same range (an empty set prints as `*`).
        if !r(input).is_any() {
            assert_eq!(r(&r(input).to_string()), r(input), "{input:?}");
        }
    }
}

#[test]
fn include_prerelease_desugaring() {
    // bun SemverQuery.rs `parse_including_prerelease` and the cases bun comments out of
    // node-semver range-include/range-exclude.
    let inc = |range: &str, version: &str| {
        Range::parse_including_prerelease(range)
            .unwrap()
            .satisfies_with(&v(version), INCLUDE)
    };
    for (range, version) in [
        ("2.x", "2.0.0-pre.0"),
        ("2.x", "2.1.0-pre.0"),
        ("1.1.x", "1.1.0-a"),
        ("1.1.x", "1.1.1-a"),
        ("*", "1.0.0-rc1"),
        ("^1.0.0-0", "1.0.1-rc1"),
        ("^1.0.0-rc2", "1.0.1-rc1"),
        ("^1.0.0", "1.0.1-rc1"),
        ("^1.0.0", "1.1.0-rc1"),
        ("1 - 2", "2.0.0-pre"),
        ("1 - 2", "1.0.0-pre"),
        ("1.0 - 2", "1.0.0-pre"),
        ("=0.7.x", "0.7.0-asdf"),
        (">=0.7.x", "0.7.0-asdf"),
        ("<=0.7.x", "0.7.0-asdf"),
        (">=1.0.0 <=1.1.0", "1.1.0-pre"),
    ] {
        assert!(inc(range, version), "{range} includes {version}");
        assert!(
            !r(range).satisfies(&v(version)),
            "{range} excludes {version} without the option"
        );
    }
    for (range, version) in [
        ("1.1.x", "1.2.0-a"),
        ("1.1.x", "1.0.0-a"),
        ("1.x", "0.0.0-a"),
        ("1.x", "2.0.0-a"),
        (">=1.0.0 <1.1.0", "1.1.0"),
    ] {
        assert!(!inc(range, version), "{range} excludes {version}");
    }
    assert_eq!(
        Range::parse_including_prerelease("1.x")
            .unwrap()
            .to_string(),
        "<2.0.0-0 >=1.0.0-0"
    );
    assert_eq!(
        Range::parse_including_prerelease(">1").unwrap().to_string(),
        ">=2.0.0-0"
    );
    assert_eq!(
        Range::parse_including_prerelease("1.0.0 - 2.0.0")
            .unwrap()
            .to_string(),
        ">=1.0.0-0 <2.0.1-0"
    );
    // On a normally parsed range the option only drops the prerelease rule.
    assert!(r("^1.0.0").satisfies_with(&v("1.5.0-rc.1"), INCLUDE));
    assert!(!r("^1.0.0").satisfies(&v("1.5.0-rc.1")));
}

#[test]
fn parse_info_like_bun_has_skipped() {
    // bun SemverQuery.rs `has_skipped`.
    let cases = [
        ("", false),
        ("*", false),
        (">=16", false),
        (">= 16.x", false),
        ("^18 || >=20", false),
        ("16 - 18", false),
        ("v16", false),
        (">=v16.0.0", false),
        ("~>16", false),
        ("1.2.3-rc.1+build", false),
        ("16+build", false),
        ("foo-bar", true),
        ("latest", true),
        ("lts/*", true),
        (">=14 && <17", true),
        (">=16, <18", true),
        (">=16 npm", true),
        ("16.0.0.0", true),
        ("16-18", true),
        (">=", true),
        (">=1.2.3<2.0.0", true),
        ("-18", true),
        ("-x", true),
        ("16 || -18", true),
        ("16 || - 18.0.0", true),
        ("01.2.3", false),
        ("1.0.0rc1", false),
        ("^16 | ^18", false),
    ];
    for (text, expected) in cases {
        let (_, info) = Range::parse_with_info(text);
        assert_eq!(info.has_skipped, expected, "{text:?}");
        assert_eq!(Range::parse_strict(text).is_err(), expected, "{text:?}");
    }
    let (_, info) = Range::parse_with_info("^1.2.3-beta+b || 2");
    assert!(info.has_prerelease && info.has_build && !info.has_skipped);
    let (_, info) = Range::parse_with_info("^1.2.3 || 2.x");
    assert!(!info.has_prerelease && !info.has_build);
    assert_eq!(
        Range::parse_strict("latest"),
        Err(Error::InvalidRange("latest".to_owned()))
    );
}

#[test]
fn has_prerelease_matches_parse_info() {
    for text in [
        "^1.2.3",
        "^1.2.3-beta",
        "~1.2.3-0",
        ">=1.0.0-rc.1 <2",
        "1.x",
        "1 - 2.0.0-rc.1",
        "1.2.3-beta || 2",
        "<1.2",
        ">=1.2.3-0",
        "1.0.0 - 2",
        "*",
        "",
    ] {
        let (range, info) = Range::parse_with_info(text);
        assert_eq!(range.has_prerelease(), info.has_prerelease, "{text:?}");
    }
}

#[test]
fn range_queries() {
    assert!(Range::any().is_any() && Range::any().has_no_comparators());
    assert!(r("").has_no_comparators() && r("latest").has_no_comparators());
    assert!(!r("*").has_no_comparators() && r("*").satisfies(&v("1.2.3")));
    assert!(r("1 || ^latest").is_any() && !r("1 || latest").is_any());
    assert_eq!(r("=1.2.3").as_exact(), Some(&Version::new(1, 2, 3)));
    assert_eq!(r("1.2.3-rc.1").as_exact(), Some(&v("1.2.3-rc.1")));
    assert_eq!(r("1.2"), r("1.2.x"));
    assert!(r("^1").as_exact().is_none() && r("1 || 2").as_exact().is_none());
    // A prerelease never satisfies `*` without the option.
    assert!(!r("*").satisfies(&v("1.0.0-rc.1")) && !r("").satisfies(&v("1.0.0-rc.1")));
    assert!(r("*").satisfies_with(&v("1.0.0-rc.1"), INCLUDE));
    assert!(satisfies("1.2.3", "latest") && !satisfies("1.2.3", "^2") && !satisfies("1.2.3é", "*"));
    assert!(!satisfies("1.0.0", "^1.0.0é"));
    let c = Comparator::new(Op::Gte, Version::new(0, 0, 0));
    assert!(c.matches(&v("0.0.0-a")));
    assert_eq!(c.to_string(), ">=0.0.0");
    assert_eq!(Comparator::new(Op::Eq, v("1.2.3+b")).to_string(), "1.2.3+b");
}

#[test]
fn min_version() {
    // node-semver test/ranges/min-version.js
    let cases: &[(&str, Option<&str>)] = &[
        ("*", Some("0.0.0")),
        ("* || >=2", Some("0.0.0")),
        (">=2 || *", Some("0.0.0")),
        (">2 || *", Some("0.0.0")),
        ("1.0.0", Some("1.0.0")),
        ("1.0", Some("1.0.0")),
        ("1.0.x", Some("1.0.0")),
        ("1.0.*", Some("1.0.0")),
        ("1", Some("1.0.0")),
        ("1.x.x", Some("1.0.0")),
        ("1.*.x", Some("1.0.0")),
        ("1.x.*", Some("1.0.0")),
        ("1.x", Some("1.0.0")),
        ("1.*", Some("1.0.0")),
        ("=1.0.0", Some("1.0.0")),
        ("~1.1.1", Some("1.1.1")),
        ("~1.1.1-beta", Some("1.1.1-beta")),
        ("~1.1.1 || >=2", Some("1.1.1")),
        ("^1.1.1", Some("1.1.1")),
        ("^1.1.1-beta", Some("1.1.1-beta")),
        ("^1.1.1 || >=2", Some("1.1.1")),
        ("^2.16.2 ^2.16", Some("2.16.2")),
        ("1.1.1 - 1.8.0", Some("1.1.1")),
        ("1.1 - 1.8.0", Some("1.1.0")),
        ("<2", Some("0.0.0")),
        ("<0.0.0-beta", Some("0.0.0-0")),
        ("<0.0.1-beta", Some("0.0.0")),
        ("<2 || >4", Some("0.0.0")),
        (">4 || <2", Some("0.0.0")),
        ("<=2 || >=4", Some("0.0.0")),
        (">=4 || <=2", Some("0.0.0")),
        ("<0.0.0-beta >0.0.0-alpha", Some("0.0.0-alpha.0")),
        (">0.0.0-alpha <0.0.0-beta", Some("0.0.0-alpha.0")),
        (">=1.1.1 <2 || >=2.2.2 <2", Some("1.1.1")),
        (">=2.2.2 <2 || >=1.1.1 <2", Some("1.1.1")),
        (">1.0.0", Some("1.0.1")),
        (">1.0.0-0", Some("1.0.0-0.0")),
        (">1.0.0-beta", Some("1.0.0-beta.0")),
        (">2 || >1.0.0", Some("1.0.1")),
        (">2 || >1.0.0-0", Some("1.0.0-0.0")),
        (">2 || >1.0.0-beta", Some("1.0.0-beta.0")),
        (">4 <3", None),
        ("<x", None),
    ];
    for (range, want) in cases {
        assert_eq!(
            r(range).min_version().map(|v| v.to_string()).as_deref(),
            *want,
            "{range}"
        );
    }
}

#[test]
fn max_satisfying_cases() {
    // node-semver test/ranges/max-satisfying.js
    let cases: &[(&[&str], &str, &str)] = &[
        (&["1.2.3", "1.2.4"], "1.2", "1.2.4"),
        (&["1.2.4", "1.2.3"], "1.2", "1.2.4"),
        (&["1.2.3", "1.2.4", "1.2.5", "1.2.6"], "~1.2.3", "1.2.6"),
        (
            &[
                "1.1.0", "1.2.0", "1.2.1", "1.3.0", "2.0.0b1", "2.0.0b2", "2.0.0b3", "2.0.0",
                "2.1.0",
            ],
            "~2.0.0",
            "2.0.0",
        ),
    ];
    for (list, range, want) in cases {
        let unsorted: Vec<Version> = list.iter().map(|s| v(s)).collect();
        let sorted = vs(list);
        let opts = SatisfyOptions::default();
        assert_eq!(
            max_satisfying(&unsorted, &r(range), opts),
            Some(&v(want)),
            "{range}"
        );
        assert_eq!(
            max_satisfying_sorted(&sorted, &r(range), opts),
            Some(&v(want)),
            "{range}"
        );
    }
    let none: Vec<Version> = Vec::new();
    assert_eq!(
        max_satisfying_sorted(&none, &r("*"), SatisfyOptions::default()),
        None
    );
    assert_eq!(
        max_satisfying(&none, &r("*"), SatisfyOptions::default()),
        None
    );
}

/// `max_satisfying_sorted` (which bisects) agrees with the plain scan on many shapes.
#[test]
fn max_satisfying_sorted_agrees_with_scan() {
    let mut list = Vec::new();
    for major in 0..4u64 {
        for minor in 0..4u64 {
            for patch in 0..3u64 {
                list.push(Version::new(major, minor, patch));
                list.push(Version {
                    pre: vec![
                        Identifier::AlphaNumeric("rc".into()),
                        Identifier::Numeric(patch),
                    ],
                    ..Version::new(major, minor, patch)
                });
            }
        }
    }
    list.push(v("0.0.0-0"));
    list.sort();
    let ranges = [
        "*",
        "^1.2.0",
        "~2.1",
        "1.x",
        ">=2.3.1",
        ">3",
        "<1.1",
        "<=0.0.0",
        "1.1.1",
        "=3.3.2",
        "1.2.3",
        "^1.2.0-rc.1",
        ">=2.0.0-rc.0 <2.0.0",
        "1 - 2.1",
        "0.0.0-0",
        ">=0.0.0-0",
        ">=0.0.0",
        "<x",
        "^0.0.1 || ^3",
        "~1.1.1-rc.1 || 0.x",
        "2.2.2-rc.2",
        "<2.0.0-rc.1",
        ">1.3.2 <2.0.1-rc.1",
    ];
    for range in ranges {
        let range = r(range);
        for opts in [SatisfyOptions::default(), INCLUDE] {
            let scanned = max_satisfying(&list, &range, opts);
            assert_eq!(
                max_satisfying_sorted(&list, &range, opts),
                scanned,
                "{range} {opts:?}"
            );
        }
    }
}

#[derive(Debug, PartialEq)]
struct Item {
    version: Version,
    deprecated: bool,
}

fn items(list: &[(&str, bool)]) -> Vec<Item> {
    let mut out: Vec<Item> = list
        .iter()
        .map(|(s, d)| Item {
            version: v(s),
            deprecated: *d,
        })
        .collect();
    out.sort_by(|a, b| a.version.cmp(&b.version));
    out
}

fn pick(list: &[Item], range: &str, latest: Option<&str>) -> Option<String> {
    let latest = latest.map(|l| {
        list.iter()
            .find(|i| i.version == v(l))
            .expect("latest in list")
    });
    pick_best(list, |i| &i.version, &r(range), latest, |i| i.deprecated)
        .map(|i| i.version.to_string())
}

/// bun `PackageManifest::find_best_version`.
#[test]
fn pick_best_like_bun() {
    let list = items(&[
        ("1.0.0", false),
        ("1.1.0", false),
        ("1.2.0", false),
        ("1.3.0-rc.1", false),
        ("2.0.0", false),
        ("2.1.0-beta.1", false),
        ("2.1.0-beta.2", false),
        ("3.0.0", true),
        ("3.0.1", true),
    ]);
    let latest = Some("1.1.0");
    // `latest` wins when it satisfies.
    assert_eq!(pick(&list, "^1.0.0", latest).as_deref(), Some("1.1.0"));
    assert_eq!(pick(&list, "*", latest).as_deref(), Some("1.1.0"));
    assert_eq!(pick(&list, "", latest).as_deref(), Some("1.1.0"));
    // Otherwise the highest release.
    assert_eq!(pick(&list, "^1.2.0", latest).as_deref(), Some("1.2.0"));
    assert_eq!(pick(&list, ">=1", None).as_deref(), Some("2.0.0"));
    // With a prerelease in the range, `latest` must be the first comparator itself.
    assert_eq!(pick(&list, "^1.0.0-rc.0", latest).as_deref(), Some("1.2.0"));
    assert_eq!(
        pick(&list, ">=1.1.0 || 1.3.0-rc.1", latest).as_deref(),
        Some("1.1.0")
    );
    // Releases first, then prereleases.
    assert_eq!(
        pick(&list, "^2.1.0-beta.1", latest).as_deref(),
        Some("2.1.0-beta.2")
    );
    assert_eq!(
        pick(&list, ">=1.0.0 <1.3.0-rc.5 || 1.3.0-rc.1", None).as_deref(),
        Some("1.2.0")
    );
    assert_eq!(
        pick(&list, "1.3.0-rc.1 || 2.1.0-beta.2", None).as_deref(),
        Some("2.1.0-beta.2")
    );
    // A prerelease is not picked for a range without one.
    assert_eq!(pick(&list, ">=2.0.1 <3", latest), None);
    // Deprecated versions only when every match is deprecated; exact versions regardless.
    assert_eq!(pick(&list, "^3", None).as_deref(), Some("3.0.1"));
    assert_eq!(pick(&list, ">=2", None).as_deref(), Some("2.0.0"));
    assert_eq!(pick(&list, "3.0.0", None).as_deref(), Some("3.0.0"));
    assert_eq!(
        pick(&list, "3.0.0", Some("3.0.1")).as_deref(),
        Some("3.0.0")
    );
    assert_eq!(pick(&list, "^3", Some("3.0.1")).as_deref(), Some("3.0.1"));
    assert_eq!(pick(&list, "4.0.0", latest), None);
    assert_eq!(pick(&list, "^4", latest), None);

    let plain = vs(&["1.0.0", "1.5.0", "2.0.0"]);
    assert_eq!(
        best_version(&plain, &r("^1"), Some(&plain[0])),
        Some(&plain[0])
    );
    assert_eq!(best_version(&plain, &r("^1"), None), Some(&plain[1]));
}

#[test]
fn intersects_and_subset() {
    assert!(r("^1.2.0").intersects(&r("~1.2.5")));
    assert!(!r("^1").intersects(&r("^2")));
    // bun intersects.rs `is_subset_of`, npm `subset` answers except where noted.
    let cases = [
        (">=16.0.0", ">=16.0.0", true),
        (">=16.0.0", ">=16.0.1", false),
        (">=16.0.1", ">=16.0.0", true),
        (">16.0.0", ">=16.0.0", true),
        (">=16.0.0", ">16.0.0", false),
        ("16.3.1", ">=16.0.0", true),
        ("16.3.1", "^16.4.0", false),
        ("16.3.1", "16.3.1", true),
        ("^16.3.1", "16.3.1", false),
        ("^16", "^16.0.0", true),
        ("^16.3", "^16", true),
        ("^16", "^16.3", false),
        ("~16.3", "^16.3.0", true),
        ("16.x", ">=16 <17", true),
        ("16", "16.x", true),
        ("^18 || ^20", ">=18", true),
        ("^18 || ^20", "^18.0.0 || ^20.0.0 || >=22.0.0", true),
        ("^18 || ^20 || ^21", "^18.0.0 || ^20.0.0 || >=22.0.0", false),
        (">=18", "^18.0.0 || >=19.0.0", false),
        (">=1", "1 || >=2", false),
        (">=18 <20", ">=18", true),
        (">=18", ">=18 <20", false),
        ("<6.0.0", ">=0.10.0", false),
        ("<6.0.0", "<6.0.0", true),
        ("<6.0.0", "<=6.0.0", true),
        ("<=6.0.0", "<6.0.0", false),
        (">16", ">=17.0.0", true),
        (">=17.0.0", ">16", true),
        (">16.3", ">=16.4.0", true),
        ("<=16", "<17.0.0", true),
        ("<17.0.0", "<=16", true),
        ("<=16.3", "<16.4.0", true),
        ("<=16", "<=16.99.0", false),
        ("<2.0.0", "<2.0.0-0", true),
        (">=2.0.0-a <2.0.0", ">=2.0.0-a <2.0.0-0", false),
        (">16 <17.0.0", "1.0.0", true),
        ("*", "*", true),
        ("", "*", true),
        ("*", ">=0.0.0", true),
        (">=1", "*", true),
        ("*", ">=1", false),
        ("<5", "*", true),
        ("<5", ">=0.0.0 <6", true),
        ("16 - 18", ">=16 <19", true),
        ("16 - 18.2.1", "<=18.2.1", true),
        ("16 - 18.2.1", "<18.2.1", false),
        (">2 <1", ">=16", true),
        (">=16 || >2 <1", ">=16", true),
        (">=16", "<0", false),
        ("<0", "<0", true),
        (">=20.19.0 <21.0.0", "^20.10.0", true),
        (">=20.19.0 <21", "^20.10.0", true),
        (">=20.19.0 <21.0.0-0", "^20.10.0", true),
        ("^20.10.0", ">=20.10.0 <21.0.0", true),
        (">=22.0.0-rc.1", ">=20.0.0", false),
        (">=22.0.0-rc.1", ">=22.0.0-alpha", true),
        (">=22.0.0-alpha", ">=22.0.0-rc.1", false),
        (">=1.0.0 <2.0.0-rc", "^1.0.0", false),
        (">=1.0.0 <2.0.0-rc", ">=1.0.0 <2.0.0-rc.1", true),
        ("1.2.3-beta", ">=1.2.3-alpha", true),
        ("1.2.3-beta", ">=1.2.3-alpha <2.0.0", true),
        ("1.2.3-beta", ">=1.2.0", false),
        (
            ">=1.2.3-a.very.long.prerelease.2",
            ">=1.2.3-a.very.long.prerelease.1",
            true,
        ),
        (
            ">=1.2.3-a.very.long.prerelease.1",
            ">=1.2.3-a.very.long.prerelease.2",
            false,
        ),
    ];
    for (sub, dom, expected) in cases {
        assert_eq!(r(sub).is_subset_of(&r(dom)), expected, "{sub:?} in {dom:?}");
    }
}

#[test]
fn serde_round_trip() {
    let version = v("1.2.3-rc.1+b");
    let json = serde_json::to_string(&version).unwrap();
    assert_eq!(json, "\"1.2.3-rc.1+b\"");
    assert_eq!(serde_json::from_str::<Version>(&json).unwrap(), version);
    assert!(serde_json::from_str::<Version>("\"nope\"").is_err());
    let range = r("^1.2.3 || 2.x");
    let json = serde_json::to_string(&range).unwrap();
    assert_eq!(serde_json::from_str::<Range>(&json).unwrap(), range);
}

#[test]
fn errors_display() {
    assert_eq!(
        Error::InvalidVersion("x".into()).to_string(),
        "invalid version \"x\""
    );
    assert_eq!(
        Error::InvalidRange("x".into()).to_string(),
        "invalid range \"x\""
    );
}
