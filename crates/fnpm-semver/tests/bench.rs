//! Micro benchmarks without criterion. Run with
//! `cargo test --release -p fnpm-semver --test bench -- --ignored --nocapture`
//! (`FNPM_BENCH=<substring>` runs a subset).

use std::hint::black_box;
use std::time::Instant;

use fnpm_semver::{
    Range, SatisfyOptions, Version, best_version, max_satisfying, max_satisfying_sorted,
};

// The allocator fnpm ships with (`fnpm-napi`).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Times `f`; set `FNPM_BENCH=<substring>` to run only matching benchmarks.
fn time<R>(label: &str, iters: u32, mut f: impl FnMut() -> R) -> f64 {
    if std::env::var("FNPM_BENCH").is_ok_and(|filter| !label.contains(&filter)) {
        return 0.0;
    }
    for _ in 0..iters / 10 {
        black_box(f());
    }
    let start = Instant::now();
    for _ in 0..iters {
        black_box(f());
    }
    let ns = start.elapsed().as_nanos() as f64 / f64::from(iters);
    println!("{label:<52} {ns:>10.1} ns/op");
    ns
}

/// 1000 versions shaped like a busy package: 10 majors x 10 minors x 9 patches, plus a
/// prerelease train before each minor.
fn thousand_versions() -> Vec<Version> {
    let mut out = Vec::new();
    for major in 0..10 {
        for minor in 0..10 {
            out.push(Version::parse_loose(&format!("{major}.{minor}.0-rc.1")).unwrap());
            for patch in 0..9 {
                out.push(Version::new(major, minor, patch));
            }
        }
    }
    out.sort();
    assert_eq!(out.len(), 1000);
    out
}

#[test]
#[ignore = "benchmark"]
fn bench_semver() {
    let versions = thousand_versions();
    let opts = SatisfyOptions::default();

    // Reference points for how fast this machine is.
    time("calibration: \"12345\".parse::<u64>()", 10_000_000, || {
        black_box("12345").parse::<u64>()
    });
    time("calibration: Box::new(1u64)", 10_000_000, || {
        Box::new(black_box(1u64))
    });
    time("Version::parse_loose(\"1.2.3\")", 2_000_000, || {
        Version::parse_loose(black_box("1.2.3"))
    });
    time(
        "Version::parse_loose(\"1.2.3-beta.4+b\")",
        2_000_000,
        || Version::parse_loose(black_box("1.2.3-beta.4+b")),
    );
    time("Range::parse(\"^1.2.3\")", 2_000_000, || {
        Range::parse(black_box("^1.2.3"))
    });
    time(
        "Range::parse(\">=1.2.7 <1.3.0 || ^2 || 3.x\")",
        1_000_000,
        || Range::parse(black_box(">=1.2.7 <1.3.0 || ^2 || 3.x")),
    );

    let caret = Range::parse("^1.2.3").unwrap();
    let v = Version::new(1, 5, 0);
    time("Range::satisfies (^1.2.3, 1.5.0)", 10_000_000, || {
        black_box(&caret).satisfies(black_box(&v))
    });
    let a = Version::new(1, 5, 0);
    let b = Version::parse_loose("1.5.0-rc.1").unwrap();
    time("Version::cmp_precedence", 10_000_000, || {
        black_box(&a).cmp_precedence(black_box(&b))
    });

    for (label, range) in [
        ("^5.0.0", "^5.0.0"),
        ("~3.4.0", "~3.4.0"),
        ("* (top hit)", "*"),
        (">=20 (no match)", ">=20"),
        ("exact 7.7.7", "7.7.7"),
        ("^1 || ^4", "^1 || ^4"),
        ("<0.5.0 (scan from low end)", "<0.5.0"),
    ] {
        let r = Range::parse(range).unwrap();
        let sorted = max_satisfying_sorted(&versions, &r, opts);
        assert_eq!(sorted, max_satisfying(&versions, &r, opts), "{range}");
        time(
            &format!("max_satisfying_sorted x1000 {label}"),
            1_000_000,
            || max_satisfying_sorted(black_box(&versions), black_box(&r), opts),
        );
        time(
            &format!("max_satisfying (full scan) x1000 {label}"),
            20_000,
            || max_satisfying(black_box(&versions), black_box(&r), opts),
        );
    }
    let latest = versions.iter().find(|v| **v == Version::new(9, 9, 8));
    let r = Range::parse("^9.1.0").unwrap();
    time("best_version x1000 ^9.1.0 (latest hit)", 1_000_000, || {
        best_version(black_box(&versions), &r, latest)
    });
    let r = Range::parse("^4.1.0").unwrap();
    time("best_version x1000 ^4.1.0 (latest miss)", 1_000_000, || {
        best_version(black_box(&versions), &r, latest)
    });
}
