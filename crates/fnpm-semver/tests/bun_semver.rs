//! The cases of bun `test/cli/install/semver.test.ts`, as recorded in
//! `fixtures/bun-semver-test.jsonl` (see `fixtures/regen/`): every `Bun.semver.satisfies`,
//! `Bun.semver.order` and `semverIntersects` call the test makes, with the value it expects.
//! The two seeded differentials are included with node-semver 7.7.4 answers.

use std::cmp::Ordering;

use fnpm_semver::{Range, Version, order, satisfies};
use serde_json::Value;

const CASES: &str = include_str!("fixtures/bun-semver-test.jsonl");

fn cases() -> impl Iterator<Item = (String, String, String, Value)> {
    CASES.lines().filter(|l| !l.is_empty()).map(|line| {
        let v: Vec<Value> = serde_json::from_str(line).expect("fixture line");
        let s = |i: usize| v[i].as_str().expect("string").to_owned();
        (s(0), s(1), s(2), v[3].clone())
    })
}

fn check(kind: &str) -> usize {
    let mut failures = Vec::new();
    let mut count = 0;
    for (f, a, b, expected) in cases().filter(|c| c.0 == kind) {
        count += 1;
        let got = match f.as_str() {
            "satisfies" => Value::Bool(satisfies(&a, &b)),
            "intersects" => {
                let (l, r) = (Range::parse(&a).unwrap(), Range::parse(&b).unwrap());
                Value::Bool(l.intersects(&r))
            }
            "order" => match order(&a, &b) {
                Ok(Ordering::Less) => Value::from(-1),
                Ok(Ordering::Equal) => Value::from(0),
                Ok(Ordering::Greater) => Value::from(1),
                Err(_) => Value::from("throw"),
            },
            other => panic!("unknown fixture kind {other}"),
        };
        if got != expected {
            failures.push(format!("{f}({a:?}, {b:?}) = {got}, expected {expected}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {count} {kind} cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    count
}

#[test]
fn bun_satisfies_cases() {
    assert!(check("satisfies") > 7000);
}

#[test]
fn bun_order_cases() {
    assert!(check("order") > 100);
}

#[test]
fn bun_intersects_cases() {
    assert!(check("intersects") > 300);
}

/// `expect(unsortedPrereleases.sort(Bun.semver.order)).toMatchSnapshot()`
#[test]
fn bun_prerelease_snapshot() {
    let mut unsorted: Vec<&str> = include_str!("fixtures/bun-prerelease-unsorted.txt")
        .lines()
        .collect();
    let expected: Vec<&str> = include_str!("fixtures/bun-prerelease-order.txt")
        .lines()
        .collect();
    // JS `Array.prototype.sort` is stable, like `sort_by`.
    unsorted.sort_by(|a, b| order(a, b).unwrap());
    assert_eq!(unsorted, expected);
    // Sorting parsed versions by `Ord` gives the same order.
    let mut parsed: Vec<Version> = include_str!("fixtures/bun-prerelease-unsorted.txt")
        .lines()
        .map(|l| Version::parse_loose(l).unwrap())
        .collect();
    parsed.sort();
    let printed: Vec<String> = parsed.iter().map(ToString::to_string).collect();
    assert_eq!(printed, expected);
}

/// The cases bun runs in a subprocess.
#[test]
fn bun_subprocess_cases() {
    let range = vec!["1.0.0"; 300].join(" || ");
    assert!(satisfies("1.0.0", &range));

    let fuzzed = format!("> > > > > > > `{{{}- - - 1e-323-alpha.1", "`${".repeat(34));
    let cases = [
        ("", fuzzed.as_str(), false),
        ("1.0.0", fuzzed.as_str(), true),
        ("", "1 || -", false),
        ("1.0.0", "1 || -", true),
        ("2.0.0", "1 || -", false),
        ("1.0.0", "1 a - b", true),
        ("2.5.0", "^1 || -q ^2", true),
    ];
    for (v, r, want) in cases {
        assert_eq!(satisfies(v, r), want, "{v:?} {r:?}");
    }

    let v_run = "v".repeat(1_000_000);
    let eq_run = "= ".repeat(1_000_000);
    assert!(satisfies("1.0.0", &v_run));
    assert!(satisfies("1.0.0", &eq_run));

    let or_chain = vec!["1"; 250_000].join("||");
    let and_chain = vec![">1"; 250_000].join(" ");
    assert!(!satisfies("2.0.0", &or_chain));
    assert!(satisfies("2.0.0", &format!("{or_chain}||2")));
    assert!(satisfies("2.0.0", &and_chain));
    assert!(!satisfies("1.0.0", &and_chain));
}

/// "failures does not cause weird memory issues"
#[test]
fn bun_repeated_failures() {
    for _ in 0..10_000 {
        assert!(satisfies("1.2.3", "1.2.3"));
        assert!(!satisfies("^2.2.3||lol||!!#4_", "1.2.3"));
        assert!(!satisfies("^1.2.3||lol||!!#4_", "+!+!+!_)31231.2.3"));
        assert!(satisfies("1.2.3", "^1.2.3"));
        assert!(!satisfies("^1.2.3", "1.2.3"));
    }
}

/// "an exact version %s intersects a range when it satisfies it"
#[test]
fn bun_exact_version_intersects_when_it_satisfies() {
    let versions = [
        "0.0.0",
        "0.0.1",
        "0.1.0",
        "0.9.9",
        "1.0.0",
        "1.0.1",
        "1.2.3",
        "1.9.9",
        "2.0.0",
        "2.0.1",
        "3.0.0",
        "1.0.0-0",
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-beta",
        "1.0.0-rc.1",
        "1.2.3-beta.2",
        "2.0.0-0",
        "2.0.0-alpha.0",
        "2.0.0-rc.1",
        "3.0.0-pre",
    ];
    let ranges = [
        "1",
        "1.2",
        "1.x",
        "1.2.x",
        "^1.0.0",
        "^1.2.3",
        "^0.1.0",
        "^0.0.1",
        "~1.2.3",
        "~1.0.0",
        "~0.1.0",
        ">=1.0.0",
        ">1.0.0",
        "<2.0.0",
        "<=2.0.0",
        ">=1.0.0 <2.0.0",
        ">1.0.0 <=2.0.0",
        "1.0.0 - 2.0.0",
        "1.0.0 || 2.0.0",
        "^1 || ^3",
        "<1 || >=2",
        ">=1.0.0-0 <2",
        ">=1.0.0-alpha <1.0.0",
        ">=1.0.0-alpha.1 <1.0.0-beta",
        ">=1.0.0-beta <=2.0.0-rc.1",
        "^1.0.0-alpha",
        "^1.0.0-rc.1",
        "~1.2.3-beta.1",
        "^2.0.0-0",
        ">=2.0.0-alpha.0 <3",
        ">=2.0.0-rc.1",
        "<2.0.0-rc.1",
        ">1",
        ">1 <3",
        "<=1",
        ">0.9 <2.0.0-rc.1",
        "1.0.0-alpha || 2.0.0-rc.1",
        "2.0.0-0",
        "1.0.0-alpha",
    ];
    for v in versions {
        let exact = Range::parse(v).unwrap();
        for r in ranges {
            let range = Range::parse(r).unwrap();
            let want = satisfies(v, r);
            assert_eq!(exact.intersects(&range), want, "{v} meets {r}");
            assert_eq!(range.intersects(&exact), want, "{r} meets {v}");
        }
    }
}
