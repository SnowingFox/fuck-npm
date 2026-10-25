//! Every bun.lock that bun's own test suite snapshots (`test/cli/install/**`), plus the
//! bun.lock files checked into bun's repository, read and written again.

use std::path::{Path, PathBuf};

use crate::ParseOptions;
use crate::text::Dialect;

fn fixtures(dir: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bun-lock")
        .join(dir);
    let mut v: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("{}: {e}", root.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "lock"))
        .collect();
    v.sort();
    v
}

/// Undoes the normalisation bun's tests apply before snapshotting.
fn substitute(s: &str) -> String {
    let integrity =
        fnpm_types::Integrity::compute(fnpm_types::HashAlgorithm::Sha512, b"fixture").to_sri();
    s.replace("<integrity>", &integrity)
        .replace("<port>", "4873")
        .replace("<registry>", "http://localhost:4873")
        .replace("<repo>", "git+https://example.com/repo.git")
        .replace("<sha>", "0123456789abcdef0123456789abcdef01234567")
        .replace("<tarball>", "http://localhost:4873/baz-0.0.3.tgz")
}

/// Lockfiles bun writes after migrating an incomplete yarn.lock fixture. They lack
/// packages that a production dependency needs, so bun's reader rejects them too.
const EXPECTED_LOAD_ERRORS: &[(&str, &str)] = &[
    (
        "yarn-lock-migration-yarn-lock-migration-basic-complex-yarn-lock-with-multiple-de",
        "Failed to resolve prod dependency 'function-bind' for package 'get-intrinsic'",
    ),
    (
        "yarn-lock-migration-yarn-lock-migration-basic-migration-with-realistic-complex-y",
        "Failed to resolve prod dependency '@ampproject/remapping' for package '@babel/core'",
    ),
    (
        "yarn-lock-migration-yarn-lock-migration-basic-yarn-lock-with-resolutions-resolut",
        "Failed to resolve prod dependency '@types/eslint-scope' for package 'webpack'",
    ),
    (
        "yarn-lock-migration-yarn-lock-migration-basic-yarn-lock-with-scoped-packages-and",
        "Failed to resolve prod dependency '@ampproject/remapping' for package '@babel/core'",
    ),
];

fn first_diff(a: &str, b: &str) -> String {
    for (i, (x, y)) in a.lines().zip(b.lines()).enumerate() {
        if x != y {
            return format!("line {}:\n  want: {x}\n  got:  {y}", i + 1);
        }
    }
    format!(
        "length differs: want {} lines, got {} lines",
        a.lines().count(),
        b.lines().count()
    )
}

fn roundtrip(src: &str) -> Result<String, String> {
    let (graph, _) = crate::text_parse(src.as_bytes(), "bun.lock", &ParseOptions::default())
        .map_err(|e| e.to_string())?;
    Ok(super::write::write(&graph, Dialect::Bun).text)
}

#[test]
fn bun_snapshots_round_trip_byte_identically() {
    let mut failures = Vec::new();
    let all = fixtures("snapshots");
    for path in &all {
        let mut src = substitute(&std::fs::read_to_string(path).unwrap());
        // Some tests snapshot the trimmed file.
        if !src.ends_with('\n') {
            src.push('\n');
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if let Some(expected) = EXPECTED_LOAD_ERRORS
            .iter()
            .find(|(f, _)| name.starts_with(f))
            .map(|(_, e)| *e)
        {
            match roundtrip(&src) {
                Err(e) if e.contains(expected) => {}
                other => failures.push(format!(
                    "{name}: expected load error {expected:?}, got {other:?}"
                )),
            }
            continue;
        }
        match roundtrip(&src) {
            Ok(out) if out == src => {}
            Ok(out) => failures.push(format!("{name}: {}", first_diff(&src, &out))),
            Err(e) => failures.push(format!("{name}: parse error: {e}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} snapshots differ:\n{}",
        failures.len(),
        all.len(),
        failures.join("\n")
    );
}

#[test]
fn checked_in_lockfiles_round_trip() {
    let mut failures = Vec::new();
    let all = fixtures("files");
    for path in &all {
        let src = std::fs::read_to_string(path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let once = match roundtrip(&src) {
            Ok(o) => o,
            Err(e) => {
                failures.push(format!("{name}: parse error: {e}"));
                continue;
            }
        };
        // Files from before configVersion existed gain the line bun adds (`0`: an existing
        // project without the field); everything else is byte-identical.
        let expected = if src.contains("\"configVersion\"") {
            src.clone()
        } else {
            let (head, rest) = src.split_at(src.find('\n').map_or(0, |i| i + 1));
            let (v, rest) = rest.split_at(rest.find('\n').map_or(0, |i| i + 1));
            format!("{head}{v}  \"configVersion\": 0,\n{rest}")
        };
        if once != expected {
            failures.push(format!("{name}: {}", first_diff(&expected, &once)));
        }
        match roundtrip(&once) {
            Ok(twice) if twice == once => {}
            Ok(twice) => failures.push(format!(
                "{name}: not a fixpoint: {}",
                first_diff(&once, &twice)
            )),
            Err(e) => failures.push(format!("{name}: reparse error: {e}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} files failed:\n{}",
        failures.len(),
        all.len(),
        failures.join("\n")
    );
}

/// `FNPM_LOCK_IN=<bun.lock> FNPM_LOCK_OUT=<path> cargo test -p fnpm-lockfile --lib rewrite_env -- --ignored`
/// rewrites a lockfile, for comparing with what bun writes.
#[test]
#[ignore]
fn rewrite_env() {
    let (Ok(input), Ok(output)) = (
        std::env::var("FNPM_LOCK_IN"),
        std::env::var("FNPM_LOCK_OUT"),
    ) else {
        return;
    };
    let src = std::fs::read_to_string(input).unwrap();
    std::fs::write(output, roundtrip(&src).unwrap()).unwrap();
}

/// `cargo test --release -p fnpm-lockfile --lib perf_large -- --ignored --nocapture`
#[test]
#[ignore]
fn perf_large() {
    fn best<T>(runs: u32, mut f: impl FnMut() -> T) -> (std::time::Duration, T) {
        let mut best = std::time::Duration::MAX;
        let mut out = None;
        for _ in 0..runs {
            let t = std::time::Instant::now();
            let v = f();
            best = best.min(t.elapsed());
            out = Some(v);
        }
        (best, out.expect("runs > 0"))
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bun-lock/files/bench-large.lock");
    let src = std::fs::read_to_string(path).unwrap();
    let (jsonc, _) = best(20, || crate::jsonc::parse(&src).map(|n| n.pos).unwrap());
    let (parse, graph) = best(20, || {
        crate::text_parse(src.as_bytes(), "bun.lock", &ParseOptions::default())
            .unwrap()
            .0
    });
    let (write, _) = best(20, || super::write::write(&graph, Dialect::Fnpm).text.len());
    let platform = fnpm_types::Platform::current();
    let (hoist, _) = best(20, || {
        crate::tree::hoist(&graph, &crate::tree::HoistOptions::all(platform))
            .nodes
            .len()
    });
    let bytes = crate::sidecar::encode(1, &graph).unwrap();
    let (sidecar, _) = best(20, || {
        crate::sidecar::decode(&bytes, 1).unwrap().packages.len()
    });
    println!(
        "packages={} jsonc={jsonc:?} parse={parse:?} write={write:?} hoist={hoist:?} sidecar_load={sidecar:?} sidecar_bytes={}",
        graph.packages.len(),
        bytes.len()
    );
}
