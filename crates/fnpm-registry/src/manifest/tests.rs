use super::*;
use fnpm_semver::{Comparator, Op};
use fnpm_types::{CpuSet, LibcSet, OsSet};

fn v(s: &str) -> Version {
    semver_ext::parse_version(s).unwrap()
}

fn range(cs: &[(Op, &str)]) -> Range {
    Range {
        sets: vec![
            cs.iter()
                .map(|(op, s)| Comparator {
                    op: *op,
                    version: v(s),
                })
                .collect(),
        ],
    }
}

fn caret(s: &str) -> Range {
    let base = v(s);
    let upper = if base.major > 0 {
        format!("{}.0.0-0", base.major + 1)
    } else if base.minor > 0 {
        format!("0.{}.0-0", base.minor + 1)
    } else {
        format!("0.0.{}-0", base.patch + 1)
    };
    range(&[(Op::Gte, s), (Op::Lt, &upper)])
}

fn exact(s: &str) -> Range {
    range(&[(Op::Eq, s)])
}

fn parse(json: &str) -> PackageManifest {
    PackageManifest::parse("pkg", json.as_bytes()).unwrap()
}

fn versions(m: &PackageManifest) -> Vec<String> {
    m.versions
        .iter()
        .map(|x| semver_ext::format(&x.version))
        .collect()
}

fn picked(r: PickResult<'_>) -> Option<String> {
    r.version().map(|x| semver_ext::format(&x.version))
}

#[test]
fn parses_abbreviated_document() {
    let m = parse(
        r#"{
        "name": "other-name",
        "modified": "2024-05-01T00:00:00.000Z",
        "dist-tags": { "latest": "1.1.0", "beta": "2.0.0-beta.1", "weird": 5 },
        "versions": {
            "1.1.0": {
                "name": "pkg", "version": "1.1.0",
                "dependencies": { "a": "^1.0.0", "b": 7, "c": "2" },
                "optionalDependencies": { "o": "*" },
                "peerDependencies": { "p": ">=1" },
                "peerDependenciesMeta": { "p": { "optional": true }, "q": { "optional": true }, "r": { "optional": false } },
                "bin": { "pkg": "./bin/pkg.js" },
                "engines": { "node": ">=18", "npm": 9 },
                "dist": { "tarball": "https://r/pkg/-/pkg-1.1.0.tgz", "integrity": "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg==", "fileCount": 3, "unpackedSize": 1234 },
                "hasInstallScript": true,
                "license": "MIT"
            },
            "1.0.0": { "dist": { "shasum": "da39a3ee5e6b4b0d3255bfef95601890afd80709" }, "bin": "./cli.js", "deprecated": "use 1.1" },
            "not-a-version": { "dist": {} },
            "2.0.0-beta.1": { "os": ["darwin", "linux"], "cpu": "x64", "libc": ["glibc"], "bundleDependencies": true, "dependencies": { "d": "1" } }
        }
    }"#,
    );
    assert_eq!(m.name, "pkg", "the requested name wins");
    assert_eq!(m.modified.as_deref(), Some("2024-05-01T00:00:00.000Z"));
    assert_eq!(versions(&m), ["1.0.0", "1.1.0", "2.0.0-beta.1"]);
    assert_eq!(m.dist_tags.len(), 2);
    assert_eq!(m.dist_tag("latest").unwrap().version, v("1.1.0"));
    assert_eq!(m.dist_tag("beta").unwrap().version, v("2.0.0-beta.1"));
    assert!(m.dist_tag("weird").is_none());

    let one = m.find(&v("1.1.0")).unwrap();
    assert_eq!(
        one.dependencies,
        vec![("a".into(), "^1.0.0".into()), ("c".into(), "2".into())]
    );
    assert_eq!(one.optional_dependencies, vec![("o".into(), "*".into())]);
    assert_eq!(
        one.peer_dependencies,
        vec![("p".into(), ">=1".into()), ("q".into(), "*".into())]
    );
    assert!(one.peer_dependencies_meta["p"].optional);
    assert!(one.peer_dependencies_meta["q"].optional);
    assert!(!one.peer_dependencies_meta["r"].optional);
    assert_eq!(
        one.bin,
        Bin::NamedFile {
            name: "pkg".into(),
            path: "bin/pkg.js".into()
        }
    );
    assert_eq!(one.engines.len(), 1);
    assert_eq!(one.dist.file_count, Some(3));
    assert_eq!(one.dist.unpacked_size, Some(1234));
    assert_eq!(
        one.dist.integrity.unwrap().algorithm(),
        fnpm_types::HashAlgorithm::Sha512
    );
    assert!(one.has_install_script);
    assert_eq!(one.license.as_deref(), Some("MIT"));
    assert_eq!(one.deprecated, None);

    let zero = m.find(&v("1.0.0")).unwrap();
    assert_eq!(zero.deprecated.as_deref(), Some("use 1.1"));
    assert_eq!(
        zero.dist.integrity.unwrap().algorithm(),
        fnpm_types::HashAlgorithm::Sha1
    );
    assert_eq!(zero.dist.tarball, "");
    assert_eq!(
        zero.bin,
        Bin::File {
            path: "cli.js".into()
        }
    );
    assert!(zero.platform.is_any());

    let beta = m.find(&v("2.0.0-beta.1")).unwrap();
    assert_eq!(beta.platform.os, OsSet::from_list(&["darwin", "linux"]));
    assert_eq!(beta.platform.cpu, CpuSet::from_list(&["x64"]));
    assert_eq!(beta.platform.libc, LibcSet::from_package_list(&["glibc"]));
    assert_eq!(beta.bundle_dependencies, vec!["d".to_owned()]);
}

#[test]
fn lenient_fields() {
    let m = parse(
        r#"{
        "dist-tags": [],
        "versions": {
            "1.0.0": "not an object",
            "1.0.1": { "dependencies": [1, 2], "bin": 5, "dist": "x", "os": 3, "deprecated": "", "peerDependenciesMeta": { "a": true } },
            "1.0.2": { "deprecated": true, "scripts": { "postinstall": "node x" }, "license": { "type": "ISC" }, "directories": { "bin": "./bin" } },
            "1.0.3": { "bin": "", "directories": { "bin": "" }, "scripts": { "install": "" }, "bundledDependencies": ["x", 1] }
        }
    }"#,
    );
    assert_eq!(versions(&m), ["1.0.0", "1.0.1", "1.0.2", "1.0.3"]);
    assert!(m.dist_tags.is_empty());
    let a = m.find(&v("1.0.1")).unwrap();
    assert!(a.dependencies.is_empty());
    assert_eq!(a.bin, Bin::None);
    assert_eq!(a.deprecated, None);
    assert!(a.peer_dependencies_meta.is_empty() || !a.peer_dependencies_meta["a"].optional);
    let b = m.find(&v("1.0.2")).unwrap();
    assert_eq!(b.deprecated.as_deref(), Some("true"));
    assert!(b.has_install_script, "derived from scripts");
    assert_eq!(b.license.as_deref(), Some("ISC"));
    assert_eq!(b.bin, Bin::Dir { path: "bin".into() });
    let c = m.find(&v("1.0.3")).unwrap();
    assert_eq!(c.bin, Bin::None);
    assert!(!c.has_install_script);
    assert_eq!(c.bundle_dependencies, vec!["x".to_owned()]);
}

#[test]
fn document_errors() {
    assert!(matches!(
        PackageManifest::parse("x", br#"{"error":"Not found"}"#),
        Err(Error::InvalidManifest { message, .. }) if message == "npm error: Not found"
    ));
    assert!(PackageManifest::parse("x", br#"{"modified": 5}"#).is_err());
    assert!(PackageManifest::parse("x", b"not json").is_err());
    assert!(
        PackageManifest::parse("x", b"[]").is_err()
            || PackageManifest::parse("x", b"[]")
                .unwrap()
                .versions
                .is_empty()
    );
    let empty = PackageManifest::parse("x", b"{}").unwrap();
    assert!(empty.versions.is_empty());
    assert!(PackageManifest::parse("x", br#"{"error": 1, "versions": {}}"#).is_ok());
}

#[test]
fn sorts_unsorted_documents_stably() {
    let m = parse(
        r#"{"versions": {"2.0.0": {}, "1.0.0-rc.1": {}, "10.0.0": {}, "1.0.0": {}, "1.0.0-alpha": {}, "01.0.0": {"deprecated":"dup"}}}"#,
    );
    assert_eq!(
        versions(&m),
        [
            "1.0.0-alpha",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.0",
            "2.0.0",
            "10.0.0"
        ]
    );
    assert_eq!(
        m.versions[3].deprecated.as_deref(),
        Some("dup"),
        "equal keys keep document order"
    );
}

#[test]
fn publish_times() {
    let m = parse(
        r#"{
        "time": { "created": "2019-01-01T00:00:00.000Z", "1.0.0": "2020-01-01T00:00:00.000Z", "1.0.1": "garbage", "1.0.2": null, "1.0.3": 0, "1.0.4": 12, "v1.0.5": "2021-01-01T00:00:00.000Z" },
        "versions": { "1.0.0": {}, "1.0.1": {}, "1.0.2": {}, "1.0.3": {}, "1.0.4": {}, "v1.0.5": {}, "1.0.6": {} }
    }"#,
    );
    assert_eq!(m.publish_time_ms(&v("1.0.0")), 1_577_836_800_000);
    assert_eq!(m.publish_time_ms(&v("1.0.1")), u64::MAX);
    assert_eq!(m.publish_time_ms(&v("1.0.2")), 0);
    assert_eq!(m.publish_time_ms(&v("1.0.3")), 0);
    assert_eq!(m.publish_time_ms(&v("1.0.4")), u64::MAX);
    assert_eq!(m.publish_time_ms(&v("1.0.5")), 1_609_459_200_000);
    assert_eq!(m.publish_time_ms(&v("1.0.6")), 0);
    assert_eq!(m.time["created"], 1_546_300_800_000);
}

fn npm_like() -> PackageManifest {
    parse(
        r#"{
        "dist-tags": { "latest": "1.2.0", "next": "2.0.0-rc.2" },
        "versions": {
            "0.9.0": {}, "1.0.0": {}, "1.1.0": {}, "1.2.0": {}, "1.3.0": {},
            "1.4.0": { "deprecated": "bad" }, "2.0.0-rc.1": {}, "2.0.0-rc.2": {}
        }
    }"#,
    )
}

#[test]
fn latest_wins_when_it_satisfies() {
    let m = npm_like();
    let o = PickOptions::default();
    assert_eq!(
        picked(m.pick_detailed(&caret("1.0.0"), o, None)).as_deref(),
        Some("1.2.0")
    );
    assert_eq!(
        picked(m.pick_detailed(&Range::any(), o, None)).as_deref(),
        Some("1.2.0")
    );
    assert_eq!(
        picked(m.pick_detailed(&range(&[(Op::Gt, "1.2.0")]), o, None)).as_deref(),
        Some("1.3.0"),
        "deprecated 1.4.0 skipped"
    );
    assert_eq!(
        picked(m.pick_detailed(&range(&[(Op::Gte, "1.4.0")]), o, None)).as_deref(),
        Some("1.4.0"),
        "only deprecated left"
    );
    assert_eq!(
        picked(m.pick_detailed(&exact("1.4.0"), o, None)).as_deref(),
        Some("1.4.0")
    );
    assert_eq!(
        m.pick_detailed(&exact("3.0.0"), o, None),
        PickResult::NotFound
    );
    assert_eq!(
        picked(m.pick_detailed(&caret("0.9.0"), o, None)).as_deref(),
        Some("0.9.0")
    );
    assert!(m.pick(&caret("5.0.0"), o).is_none());
}

#[test]
fn prereleases() {
    let m = npm_like();
    let o = PickOptions::default();
    let rc = range(&[(Op::Gte, "2.0.0-rc.1"), (Op::Lt, "3.0.0-0")]);
    assert_eq!(
        picked(m.pick_detailed(&rc, o, None)).as_deref(),
        Some("2.0.0-rc.2")
    );
    assert_eq!(
        picked(m.pick_detailed(&exact("2.0.0-rc.1"), o, None)).as_deref(),
        Some("2.0.0-rc.1")
    );
    assert!(m.pick(&range(&[(Op::Gte, "2.0.0-0")]), o).is_some());
    // A range without prerelease never picks one.
    assert!(m.pick(&range(&[(Op::Gte, "1.5.0")]), o).is_none());
    let incl = PickOptions {
        include_prerelease: true,
        ..o
    };
    assert_eq!(
        m.pick(&range(&[(Op::Gte, "1.5.0")]), incl)
            .map(|x| semver_ext::format(&x.version))
            .as_deref(),
        Some("2.0.0-rc.2")
    );

    // latest is a prerelease: `*` still takes it (npm), a prerelease range only if it is exactly it.
    let m2 = parse(
        r#"{"dist-tags":{"latest":"2.0.0-rc.1"},"versions":{"1.0.0":{},"2.0.0-rc.1":{},"2.0.0-rc.2":{}}}"#,
    );
    assert_eq!(picked(m2.pick_any(o, None)).as_deref(), Some("2.0.0-rc.1"));
    let rc2 = range(&[(Op::Gte, "2.0.0-rc.1")]);
    assert_eq!(
        picked(m2.pick_detailed(&rc2, o, None)).as_deref(),
        Some("2.0.0-rc.1"),
        "left == latest"
    );
    let rc3 = range(&[(Op::Gte, "2.0.0-rc.0")]);
    assert_eq!(
        picked(m2.pick_detailed(&rc3, o, None)).as_deref(),
        Some("2.0.0-rc.2")
    );
}

#[test]
fn deprecated_latest_is_not_taken() {
    let m = parse(
        r#"{"dist-tags":{"latest":"1.1.0"},"versions":{"1.0.0":{},"1.1.0":{"deprecated":"x"}}}"#,
    );
    assert_eq!(
        picked(m.pick_detailed(&caret("1.0.0"), PickOptions::default(), None)).as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        picked(m.pick_tag("latest", PickOptions::default(), None)).as_deref(),
        Some("1.1.0")
    );
    let missing = parse(r#"{"dist-tags":{"latest":"9.9.9"},"versions":{"1.0.0":{},"1.1.0":{}}}"#);
    assert_eq!(
        picked(missing.pick_latest(PickOptions::default(), None)).as_deref(),
        Some("1.1.0")
    );
    assert_eq!(
        missing.pick_tag("nope", PickOptions::default(), None),
        PickResult::NotFound
    );
}

const DAY: u64 = 24 * 3600 * 1000;
const NOW: u64 = 1_700_000_000_000;

fn with_times(json: &str, ages_days: &[(&str, f64)]) -> PackageManifest {
    let mut m = parse(json);
    for (ver, age) in ages_days {
        m.time
            .insert((*ver).to_owned(), NOW - (age * DAY as f64) as u64);
    }
    m
}

fn gate(days: f64) -> PickOptions {
    PickOptions {
        include_prerelease: false,
        minimum_release_age_ms: Some((days * DAY as f64) as u64),
        now_ms: NOW,
    }
}

#[test]
fn age_gate_basic() {
    let m = with_times(
        r#"{"dist-tags":{"latest":"3.0.0"},"versions":{"1.0.0":{},"2.0.0":{},"2.1.0":{},"3.0.0":{}}}"#,
        &[
            ("1.0.0", 30.0),
            ("2.0.0", 20.0),
            ("2.1.0", 10.0),
            ("3.0.0", 1.0),
        ],
    );
    let r = m.pick_detailed(&Range::any(), gate(5.0), None);
    assert_eq!(picked(r).as_deref(), Some("2.1.0"));
    assert!(r.latest_is_filtered());
    assert_eq!(
        m.pick_detailed(&exact("3.0.0"), gate(5.0), None),
        PickResult::TooRecent
    );
    assert_eq!(
        picked(m.pick_detailed(&exact("2.0.0"), gate(5.0), None)).as_deref(),
        Some("2.0.0")
    );
    assert_eq!(
        m.pick_detailed(&range(&[(Op::Gte, "3.0.0")]), gate(5.0), None),
        PickResult::AllVersionsTooRecent
    );
    // Float days.
    assert_eq!(
        picked(m.pick_detailed(&Range::any(), gate(0.5), None)).as_deref(),
        Some("3.0.0")
    );
    // Excluded package: no gate at all.
    let (ex, w) = ReleaseAgeExcludes::parse(&["pkg"]);
    assert!(w.is_empty());
    assert_eq!(
        picked(m.pick_detailed(&Range::any(), gate(5.0), Some(&ex))).as_deref(),
        Some("3.0.0")
    );
    // Version-level exclude.
    let (ex, _) = ReleaseAgeExcludes::parse(&["pkg@3.0.0 || 9.9.9"]);
    assert_eq!(
        picked(m.pick_detailed(&exact("3.0.0"), gate(5.0), Some(&ex))).as_deref(),
        Some("3.0.0")
    );
    assert_eq!(
        picked(m.pick_detailed(&Range::any(), gate(5.0), Some(&ex))).as_deref(),
        Some("3.0.0")
    );
}

#[test]
fn stability_window_skips_rapid_bugfixes() {
    // bun "detects rapid bugfixes and selects stable version" (1.8 days).
    let m = with_times(
        r#"{"dist-tags":{"latest":"1.0.3"},"versions":{"1.0.0":{},"1.0.1":{},"1.0.2":{},"1.0.3":{}}}"#,
        &[
            ("1.0.0", 8.0),
            ("1.0.1", 2.5),
            ("1.0.2", 1.5),
            ("1.0.3", 0.5),
        ],
    );
    assert_eq!(
        picked(m.pick_detailed(&Range::any(), gate(1.8), None)).as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        picked(m.pick_tag("latest", gate(1.8), None)).as_deref(),
        Some("1.0.0")
    );
}

#[test]
fn gives_up_seven_days_past_the_gate() {
    // bun "gives up after searching 7 days beyond age gate": 5-day gate, 1-day gaps.
    let mut ages = vec![("1.0.8", 1.0), ("1.0.7", 4.0)];
    let names = ["1.0.6", "1.0.5", "1.0.4", "1.0.3", "1.0.2", "1.0.1"];
    for (i, n) in names.iter().enumerate() {
        ages.push((n, 6.0 + i as f64));
    }
    ages.push(("1.0.0", 20.0));
    let json = r#"{"dist-tags":{"latest":"1.0.8"},"versions":{"1.0.0":{},"1.0.1":{},"1.0.2":{},"1.0.3":{},"1.0.4":{},"1.0.5":{},"1.0.6":{},"1.0.7":{},"1.0.8":{}}}"#;
    let m = with_times(json, &ages);
    assert_eq!(
        picked(m.pick_detailed(&Range::any(), gate(5.0), None)).as_deref(),
        Some("1.0.6")
    );

    // bun "with daily releases, gets the latest within minimum age".
    let mut ages = Vec::new();
    let mut names = Vec::new();
    for i in 1..=10 {
        names.push(format!("1.{i}.0"));
    }
    for (i, n) in names.iter().enumerate() {
        ages.push((n.as_str(), (9 - i) as f64));
    }
    let json = format!(
        r#"{{"dist-tags":{{"latest":"1.10.0"}},"versions":{{{}}}}}"#,
        names
            .iter()
            .map(|n| format!("\"{n}\":{{}}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let m = with_times(&json, &ages);
    assert_eq!(
        picked(m.pick_detailed(&Range::any(), gate(3.0), None)).as_deref(),
        Some("1.7.0")
    );

    // bun "stops searching after 7 days beyond minimum age": 1.1.0 is the fallback.
    let m = with_times(
        r#"{"dist-tags":{"latest":"2.0.0"},"versions":{"1.0.0":{},"1.1.0":{},"2.0.0":{}}}"#,
        &[("1.0.0", 20.0), ("1.1.0", 15.0), ("2.0.0", 2.0)],
    );
    assert_eq!(
        picked(m.pick_detailed(&Range::any(), gate(3.0), None)).as_deref(),
        Some("1.1.0")
    );
    assert_eq!(
        picked(m.pick_detailed(&caret("1.0.0"), gate(3.0), None)).as_deref(),
        Some("1.1.0")
    );
}

#[test]
fn prerelease_dist_tags_under_the_gate() {
    let m = with_times(
        r#"{"dist-tags":{"latest":"1.0.0","canary":"2.0.0-canary.5","beta":"2.0.0-beta.1"},
            "versions":{"1.0.0":{},"2.0.0-beta.1":{},"2.0.0-canary.1":{},"2.0.0-canary.2":{},"2.0.0-canary.3":{},"2.0.0-canary.4":{},"2.0.0-canary.5":{}}}"#,
        &[
            ("1.0.0", 30.0),
            ("2.0.0-beta.1", 4.0),
            ("2.0.0-canary.1", 10.0),
            ("2.0.0-canary.2", 8.0),
            ("2.0.0-canary.3", 5.0),
            ("2.0.0-canary.4", 2.0),
            ("2.0.0-canary.5", 0.5),
        ],
    );
    assert_eq!(
        picked(m.pick_tag("canary", gate(3.0), None)).as_deref(),
        Some("2.0.0-canary.3")
    );
    assert_eq!(
        picked(m.pick_tag("latest", gate(10.0), None)).as_deref(),
        Some("1.0.0")
    );
    let all_new = with_times(
        r#"{"dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{}}}"#,
        &[("1.0.0", 0.1)],
    );
    assert_eq!(
        all_new.pick_tag("latest", gate(3.0), None),
        PickResult::AllVersionsTooRecent
    );
}

#[test]
fn excludes_grammar() {
    let (ex, warnings) =
        ReleaseAgeExcludes::parse(&["a", "@s/b", "c@1.0.0", "@s/d@1.0.0 || 2.0.0", "e@", "f@^1"]);
    assert!(ex.excludes_package("a"));
    assert!(ex.excludes_package("@s/b"));
    assert!(!ex.excludes_package("c"));
    assert!(ex.excludes_version("c", &v("1.0.0")));
    assert!(ex.excludes_version("@s/d", &v("2.0.0")));
    assert!(!ex.excludes_version("e", &v("1.0.0")));
    assert!(!ex.excludes_package("e") && !ex.excludes_package("f"));
    assert_eq!(warnings.len(), 2);
    assert!(warnings[0].contains("\"e@\""));
    let ex = ex.with_packages(["z".to_owned()]);
    assert!(ex.excludes_package("z"));
}

#[test]
fn libc_names() {
    assert!(name_suggests_libc("@rollup/rollup-linux-x64-gnu"));
    assert!(name_suggests_libc("@img/sharp-libvips-linuxmusl-x64"));
    assert!(name_suggests_libc("foo-glibc"));
    assert!(!name_suggests_libc("signup-gnu"));
    assert!(!name_suggests_libc("react"));
}

#[test]
fn find_by_spec_like_npm_view() {
    let m = npm_like();
    assert_eq!(
        m.find_by_spec("")
            .map(|x| semver_ext::format(&x.version))
            .as_deref(),
        Some("1.2.0")
    );
    assert_eq!(
        m.find_by_spec("next")
            .map(|x| semver_ext::format(&x.version))
            .as_deref(),
        Some("2.0.0-rc.2")
    );
    assert_eq!(
        m.find_by_spec("1.1.0")
            .map(|x| semver_ext::format(&x.version))
            .as_deref(),
        Some("1.1.0")
    );
    assert_eq!(
        m.find_by_spec("v1.1.0")
            .map(|x| semver_ext::format(&x.version))
            .as_deref(),
        Some("1.1.0")
    );
    assert!(m.find_by_spec("nosuchtag").is_none());
}
