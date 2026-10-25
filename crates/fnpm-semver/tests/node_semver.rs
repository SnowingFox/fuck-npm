//! node-semver 7.7.4 `test/fixtures/*.js`, exported as JSON. Where bun (which this crate follows)
//! deliberately answers differently, the case is listed in a `BUN_DIFFERS` table with the reason,
//! and the test checks that the difference is exactly that list.

use fnpm_semver::{Op, Range, ReleaseType, SatisfyOptions, Version, order};
use serde_json::Value;

fn fixture(name: &str) -> Vec<Vec<Value>> {
    let text = match name {
        "comparisons" => include_str!("fixtures/node-semver-comparisons.json"),
        "equality" => include_str!("fixtures/node-semver-equality.json"),
        "increments" => include_str!("fixtures/node-semver-increments.json"),
        "invalid-versions" => include_str!("fixtures/node-semver-invalid-versions.json"),
        "valid-versions" => include_str!("fixtures/node-semver-valid-versions.json"),
        "range-include" => include_str!("fixtures/node-semver-range-include.json"),
        "range-exclude" => include_str!("fixtures/node-semver-range-exclude.json"),
        "range-intersection" => include_str!("fixtures/node-semver-range-intersection.json"),
        "range-parse" => include_str!("fixtures/node-semver-range-parse.json"),
        other => panic!("no fixture {other}"),
    };
    serde_json::from_str(text).expect("fixture json")
}

fn s(v: &Value) -> &str {
    v.as_str().expect("string")
}

fn include_prerelease(row: &[Value], at: usize) -> bool {
    row.get(at)
        .and_then(|o| o.get("includePrerelease"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Asserts that the failing cases are exactly `allowed`.
fn assert_only(failures: Vec<String>, allowed: &[&str]) {
    let unexpected: Vec<&String> = failures
        .iter()
        .filter(|f| !allowed.contains(&f.as_str()))
        .collect();
    let fixed: Vec<&&str> = allowed
        .iter()
        .filter(|a| !failures.iter().any(|f| f == *a))
        .collect();
    assert!(
        unexpected.is_empty() && fixed.is_empty(),
        "unexpected failures: {unexpected:#?}\nlisted but passing: {fixed:#?}"
    );
}

#[test]
fn comparisons() {
    for row in fixture("comparisons") {
        let (a, b) = (s(&row[0]), s(&row[1]));
        assert_eq!(order(a, b), Ok(std::cmp::Ordering::Greater), "{a} > {b}");
        assert_eq!(order(b, a), Ok(std::cmp::Ordering::Less), "{b} < {a}");
        let (va, vb) = (
            Version::parse_loose(a).unwrap(),
            Version::parse_loose(b).unwrap(),
        );
        assert!(va > vb && vb < va, "{a} > {b}");
    }
}

#[test]
fn equality() {
    for row in fixture("equality") {
        let (a, b) = (s(&row[0]), s(&row[1]));
        let (va, vb) = (
            Version::parse_loose(a).unwrap(),
            Version::parse_loose(b).unwrap(),
        );
        assert!(va.eq_precedence(&vb), "{a} == {b}");
        assert_eq!(order(a, b), Ok(std::cmp::Ordering::Equal), "{a} == {b}");
    }
}

#[test]
fn valid_versions() {
    for row in fixture("valid-versions") {
        let v = Version::parse(s(&row[0])).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (v.major, v.minor, v.patch),
            (
                row[1].as_u64().unwrap(),
                row[2].as_u64().unwrap(),
                row[3].as_u64().unwrap()
            )
        );
        let pre: Vec<String> = v.pre.iter().map(ToString::to_string).collect();
        let want: Vec<String> = row[4]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.to_string().trim_matches('"').to_owned())
            .collect();
        assert_eq!(pre, want, "{}", s(&row[0]));
        let build: Vec<&str> = row[5].as_array().unwrap().iter().map(s).collect();
        assert_eq!(v.build.as_ref(), build.join("."), "{}", s(&row[0]));
    }
}

#[test]
fn invalid_versions() {
    for row in fixture("invalid-versions") {
        if let Some(text) = row[0].as_str() {
            assert!(Version::parse(text).is_err(), "{text:?} ({})", s(&row[1]));
        }
    }
}

#[test]
fn increments() {
    let mut checked = 0;
    for row in fixture("increments") {
        // `identifierBase` (6th column) and the `release`/`pre` types are not part of npm version.
        if row.len() > 5 {
            continue;
        }
        let release = match s(&row[1]) {
            "major" => ReleaseType::Major,
            "minor" => ReleaseType::Minor,
            "patch" => ReleaseType::Patch,
            "premajor" => ReleaseType::Premajor,
            "preminor" => ReleaseType::Preminor,
            "prepatch" => ReleaseType::Prepatch,
            "prerelease" => ReleaseType::Prerelease,
            _ => continue,
        };
        let preid = row.get(4).and_then(Value::as_str);
        let got = Version::parse_loose(s(&row[0]))
            .ok()
            .and_then(|v| v.try_inc(release, preid).ok());
        let want = row[2].as_str();
        assert_eq!(got.map(|v| v.to_string()).as_deref(), want, "{row:?}");
        checked += 1;
    }
    assert!(checked > 80, "{checked}");
}

fn check_ranges(name: &str, expected: bool) -> Vec<String> {
    let mut failures = Vec::new();
    for row in fixture(name) {
        let (Some(r), Some(v)) = (row[0].as_str(), row[1].as_str()) else {
            continue;
        };
        let incl = include_prerelease(&row, 2);
        let range = if incl {
            Range::parse_including_prerelease(r)
        } else {
            Range::parse(r)
        }
        .unwrap();
        let got = Version::parse_loose(v)
            .map(|v| {
                range.satisfies_with(
                    &v,
                    SatisfyOptions {
                        include_prerelease: incl,
                    },
                )
            })
            .unwrap_or(false);
        if got != expected {
            failures.push(format!(
                "{r} | {v}{}",
                if incl { " | includePrerelease" } else { "" }
            ));
        }
    }
    failures
}

#[test]
fn range_include() {
    // node-semver loose mode reads these where bun does not, and bun says false.
    const BUN_DIFFERS: &[&str] = &[];
    assert_only(check_ranges("range-include", true), BUN_DIFFERS);
}

#[test]
fn range_exclude() {
    const BUN_DIFFERS: &[&str] = &[];
    assert_only(check_ranges("range-exclude", false), BUN_DIFFERS);
}

#[test]
fn range_intersection() {
    const BUN_DIFFERS: &[&str] = &[];
    let mut failures = Vec::new();
    for row in fixture("range-intersection") {
        let (a, b, want) = (s(&row[0]), s(&row[1]), row[2].as_bool().unwrap());
        let (ra, rb) = (Range::parse(a).unwrap(), Range::parse(b).unwrap());
        if ra.intersects(&rb) != want {
            failures.push(format!("{a} | {b}"));
        }
    }
    assert_only(failures, BUN_DIFFERS);
}

/// Each set as sorted comparator strings, without the `>=0.0.0` that node-semver drops.
fn normalized(range: &Range) -> Vec<Vec<String>> {
    let mut sets: Vec<Vec<String>> = range
        .sets
        .iter()
        .map(|set| {
            let mut c: Vec<String> = set
                .iter()
                .filter(|c| !(c.op == Op::Gte && c.version == Version::new(0, 0, 0)))
                .map(ToString::to_string)
                .collect();
            c.sort();
            c.dedup();
            if c.iter().any(|c| c == "<0.0.0-0") {
                c = vec!["<0.0.0-0".to_owned()];
            }
            c
        })
        .collect();
    sets.sort();
    sets.dedup();
    if sets.len() > 1 {
        sets.retain(|set| set.as_slice() != ["<0.0.0-0"]);
    }
    if sets.iter().any(Vec::is_empty) {
        sets = vec![Vec::new()];
    }
    sets
}

#[test]
fn range_parse() {
    const BUN_DIFFERS: &[&str] = &[];
    let mut failures = Vec::new();
    for row in fixture("range-parse") {
        let input = s(&row[0]);
        let incl = include_prerelease(&row, 2);
        let parse = |t: &str| {
            if incl {
                Range::parse_including_prerelease(t)
            } else {
                Range::parse(t)
            }
            .unwrap()
        };
        let got = normalized(&parse(input));
        let want = match row[1].as_str() {
            Some(text) => normalized(&Range::parse(text).unwrap()),
            // node-semver throws: bun reads something anyway.
            None => continue,
        };
        if got != want {
            failures.push(format!("{input} | {got:?} | {}", row[1]));
        }
    }
    assert_only(failures, BUN_DIFFERS);
}
