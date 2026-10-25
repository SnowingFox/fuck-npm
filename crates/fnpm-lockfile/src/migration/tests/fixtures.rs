//! bun's package-lock.json migration fixtures (`test/cli/install/migration/npm-arborist`):
//! each lockfile is migrated and compared with the bun.lock bun wrote for it, parsed by
//! [`crate::formats::bun_text`], and its warnings with bun's.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use super::support::{
    TestSemver, diff_norm, install_tests, normalize, snapshots, tree_from_bun_paths,
    tree_from_npm_locations, workspace_from_dir,
};
use crate::formats::{bun_text, npm};
use crate::migration::MigrateOptions;

/// The registry bun's migration tests configure (`OFFLINE_REGISTRY`).
fn offline() -> MigrateOptions {
    MigrateOptions {
        registry: "http://localhost:1/".to_owned(),
        ..MigrateOptions::default()
    }
}

struct Expected {
    exit_code: i32,
    warnings: BTreeSet<String>,
    lock: Option<String>,
}

fn expected(body: &str) -> Expected {
    let body = body.trim().trim_start_matches('"').trim_end_matches('"');
    let mut exit_code = -1;
    let mut warnings = BTreeSet::new();
    let mut lock = None;
    let lines: Vec<&str> = body.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some(code) = line.strip_prefix("bun pm migrate exit code: ") {
            exit_code = code.trim().parse().unwrap_or(-1);
        } else if let Some(w) = line.strip_prefix("warn: ") {
            warnings.insert(w.to_owned());
        } else if line.starts_with("bun install --frozen-lockfile exit code") {
            let rest = lines[i + 1..].join("\n");
            lock = Some(rest.trim().to_owned() + "\n");
            break;
        }
    }
    Expected {
        exit_code,
        warnings,
        lock,
    }
}

fn arborist_fixtures(dir: &Path) -> Vec<(String, Option<String>, String)> {
    let list: Value =
        serde_json::from_slice(&std::fs::read(dir.join("fixtures.json")).expect("fixtures.json"))
            .expect("json");
    list.as_array()
        .expect("array")
        .iter()
        .map(|f| {
            (
                f["name"].as_str().expect("name").to_owned(),
                f.get("root").and_then(Value::as_str).map(str::to_owned),
                f.get("lockfile")
                    .and_then(Value::as_str)
                    .unwrap_or("package-lock.json")
                    .to_owned(),
            )
        })
        .collect()
}

#[test]
fn npm_arborist_fixtures_match_bun() {
    let Some(install) = install_tests() else {
        return;
    };
    let arborist = install.join("migration/npm-arborist");
    let snaps = snapshots(&install.join("migration/__snapshots__/npm-arborist.test.ts.snap"));
    let options = offline();
    let mut failures = Vec::new();
    let mut checked = 0;
    for (name, root, lockfile) in arborist_fixtures(&arborist) {
        let dir = match &root {
            Some(r) => arborist.join(&name).join(r),
            None => arborist.join(&name),
        };
        let Some(snap) = snaps.get(&format!(
            "package-lock.json migration fixes arborist fixtures {name} 1"
        )) else {
            failures.push(format!("{name}: no snapshot"));
            continue;
        };
        let exp = expected(snap);
        let bytes = std::fs::read(dir.join(&lockfile)).expect("lockfile");
        let ws = workspace_from_dir(&dir);
        let result = npm::migrate(&bytes, &ws, &options, &lockfile, None);
        checked += 1;
        match (result, exp.exit_code) {
            (Err(e), 0) => failures.push(format!("{name}: migration failed: {e}")),
            (Err(_), _) => {}
            (Ok(_), code) if code != 0 => {
                failures.push(format!("{name}: migrated, bun exited {code}"))
            }
            (Ok((graph, report)), _) => {
                let ours: BTreeSet<String> = report
                    .warnings
                    .iter()
                    .chain(&report.verbose)
                    .cloned()
                    .collect();
                if ours != exp.warnings {
                    failures.push(format!(
                        "{name}: warnings differ\n  ours:     {:?}\n  expected: {:?}",
                        ours.difference(&exp.warnings).collect::<Vec<_>>(),
                        exp.warnings.difference(&ours).collect::<Vec<_>>()
                    ));
                }
                let Some(lock) = exp.lock else {
                    failures.push(format!("{name}: no bun.lock in snapshot"));
                    continue;
                };
                let parsed =
                    match bun_text::parse_with_semver(lock.as_bytes(), &options, &TestSemver) {
                        Ok(p) => p,
                        Err(e) => {
                            failures.push(format!("{name}: expected bun.lock does not parse: {e}"));
                            continue;
                        }
                    };
                let diff = diff_norm(&normalize(&graph), &normalize(&parsed.graph));
                if !diff.is_empty() {
                    failures.push(format!("{name}:\n    {}", diff.join("\n    ")));
                }
                // Our bun.lock writer, given bun's tree, reproduces bun's file.
                let tree = tree_from_bun_paths(&parsed.graph, &parsed.paths);
                let written = bun_text::serialize_loaded(&parsed.graph, &tree, parsed.version);
                if written != lock {
                    failures.push(format!(
                        "{name}: bun.lock round trip differs:\n{}",
                        first_difference(&lock, &written)
                    ));
                }
            }
        }
    }
    assert!(checked >= 50, "only {checked} fixtures");
    assert!(
        failures.is_empty(),
        "{} of {checked} fixtures differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// npm export: migrating, exporting with npm's own tree and migrating again keeps the graph.
/// A package that npm installed at several paths with different nested dependencies is
/// collapsed into one package by the migration (as in bun), so its tree is not reproducible;
/// for those lockfiles (and ones with entries outside the project) only the package set is
/// compared.
#[test]
fn npm_arborist_fixtures_round_trip_through_export() {
    let Some(install) = install_tests() else {
        return;
    };
    let arborist = install.join("migration/npm-arborist");
    let options = offline();
    let mut failures = Vec::new();
    for (name, root, lockfile) in arborist_fixtures(&arborist) {
        let dir = match &root {
            Some(r) => arborist.join(&name).join(r),
            None => arborist.join(&name),
        };
        let bytes = std::fs::read(dir.join(&lockfile)).expect("lockfile");
        let ws = workspace_from_dir(&dir);
        let Ok((graph, _, locations)) =
            npm::migrate_with_locations(&bytes, &ws, &options, &lockfile, None)
        else {
            continue;
        };
        let tree = tree_from_npm_locations(&graph, &locations);
        let exported =
            npm::serialize_with_tree(&graph, &tree, fnpm_config::DEFAULT_REGISTRY).expect("export");
        let mut seen = std::collections::HashSet::new();
        let lossy = locations.iter().any(|(k, id)| {
            k.starts_with("../") || (k.contains("node_modules/") && !seen.insert(*id))
        });
        match npm::migrate(
            exported.as_bytes(),
            &ws,
            &options,
            "package-lock.json",
            None,
        ) {
            Ok((again, _)) if lossy => {
                let ours: BTreeSet<String> = normalize(&again).into_keys().collect();
                let expected: BTreeSet<String> = graph
                    .packages
                    .iter()
                    .enumerate()
                    .map(|(i, _)| super::support::key(&graph, fnpm_types::PackageId(i as u32)))
                    .collect();
                let missing: Vec<_> = ours.difference(&expected).collect();
                if !missing.is_empty() {
                    failures.push(format!(
                        "{name}: exported packages not in the graph: {missing:?}"
                    ));
                }
            }
            Ok((again, _)) => {
                let diff = diff_norm(&normalize(&again), &normalize(&graph));
                if !diff.is_empty() {
                    failures.push(format!("{name}:\n    {}", diff.join("\n    ")));
                }
            }
            Err(e) => failures.push(format!("{name}: exported lockfile does not migrate: {e}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

pub(crate) fn first_difference(expected: &str, actual: &str) -> String {
    for (i, (a, b)) in expected.lines().zip(actual.lines()).enumerate() {
        if a != b {
            return format!("  line {}:\n  expected: {a}\n  actual:   {b}", i + 1);
        }
    }
    format!(
        "  lengths differ: {} vs {} lines",
        expected.lines().count(),
        actual.lines().count()
    )
}

/// The bun.lock files that bun's other tests write for migrated projects parse and are
/// reproduced byte for byte by the writer.
#[test]
fn bun_lock_snapshots_round_trip() {
    let Some(install) = install_tests() else {
        return;
    };
    let mut failures = Vec::new();
    let mut checked = 0;
    let mut incomplete = 0;
    for snap_file in [
        "migration/__snapshots__/yarn-lock-migration.test.ts.snap",
        "migration/__snapshots__/pnpm-lock-migration.test.ts.snap",
        "migration/__snapshots__/pnpm-migration.test.ts.snap",
        "migration/__snapshots__/yarn-berry.test.ts.snap",
        "__snapshots__/bun-lock.test.ts.snap",
        "__snapshots__/bun-workspaces.test.ts.snap",
        "__snapshots__/bun-install-registry.test.ts.snap",
    ] {
        for (name, body) in snapshots(&install.join(snap_file)) {
            let body = body.trim().trim_start_matches('"').trim_end_matches('"');
            let Some(start) = body.find("{\n  \"lockfileVersion\"") else {
                continue;
            };
            let text = &body[start..];
            let Some(end) = text
                .find("\n}\n")
                .map(|e| e + 3)
                .or_else(|| text.ends_with("\n}").then_some(text.len()))
            else {
                continue;
            };
            let mut lock = text[..end].to_owned();
            if !lock.ends_with('\n') {
                lock.push('\n');
            }
            checked += 1;
            let parsed = match bun_text::parse_with_semver(
                lock.as_bytes(),
                &MigrateOptions::default(),
                &TestSemver,
            ) {
                Ok(p) => p,
                // Some yarn migration snapshots list a dependency without its package (bun's
                // own loader rejects them the same way); they are only checked to fail.
                Err(e) if e.to_string().contains("Failed to resolve prod dependency") => {
                    incomplete += 1;
                    continue;
                }
                Err(e) => {
                    failures.push(format!("{snap_file} {name}: does not parse: {e}"));
                    continue;
                }
            };
            let tree = tree_from_bun_paths(&parsed.graph, &parsed.paths);
            let written = bun_text::serialize_loaded(&parsed.graph, &tree, parsed.version);
            if written != lock {
                failures.push(format!(
                    "{snap_file} {name}:\n{}",
                    first_difference(&lock, &written)
                ));
            }
        }
    }
    eprintln!("checked {checked} bun.lock snapshots, {incomplete} incomplete");
    assert!(incomplete <= 4, "{incomplete} snapshots do not parse");
    assert!(checked > 20, "only {checked} snapshots");
    assert!(
        failures.is_empty(),
        "{} of {checked} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A 3,059-entry lockfileVersion 3 monorepo (`migration/contoso-test`).
#[test]
fn contoso_monorepo_migrates() {
    let Some(install) = install_tests() else {
        return;
    };
    let dir = install.join("migration/contoso-test");
    let bytes = std::fs::read(dir.join("package-lock.json")).expect("lockfile");
    let ws = workspace_from_dir(&dir);
    let (graph, report) = npm::migrate(
        &bytes,
        &ws,
        &MigrateOptions::default(),
        "package-lock.json",
        None,
    )
    .unwrap();
    graph.validate().unwrap();
    assert!(
        graph.workspaces.len() > 1,
        "{:?}",
        graph.workspaces.keys().collect::<Vec<_>>()
    );
    for path in graph.workspaces.keys().filter(|p| !p.is_empty()) {
        assert!(dir.join(path).join("package.json").is_file(), "{path}");
    }
    assert!(graph.packages.len() > 1000, "{}", graph.packages.len());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    // Every registry package is pinned with integrity, except the copies inside a bundle.
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    let bundled: BTreeSet<String> = doc["packages"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| v["inBundle"] == true)
        .map(|(k, v)| {
            format!(
                "{}@{}",
                crate::migration::package_name_from_path(k),
                v["version"].as_str().unwrap_or_default()
            )
        })
        .collect();
    let missing: Vec<_> = graph
        .packages
        .iter()
        .filter(|p| {
            matches!(p.resolution, fnpm_types::Resolution::Npm { .. })
                && p.resolution.integrity().is_none()
                && !bundled.contains(&p.key())
        })
        .map(|p| p.key())
        .collect();
    assert!(missing.is_empty(), "{missing:?}");
}

/// `cargo test --release -- --ignored migration_speed --nocapture`.
#[test]
#[ignore = "timing"]
fn migration_speed() {
    let Some(install) = install_tests() else {
        return;
    };
    let dir = install.join("migration/contoso-test");
    let bytes = std::fs::read(dir.join("package-lock.json")).expect("lockfile");
    let ws = workspace_from_dir(&dir);
    let options = MigrateOptions::default();
    let n = 20;
    let start = std::time::Instant::now();
    let mut graph = None;
    for _ in 0..n {
        graph = Some(
            npm::migrate(&bytes, &ws, &options, "package-lock.json", None)
                .unwrap()
                .0,
        );
    }
    let graph = graph.unwrap();
    eprintln!(
        "package-lock.json ({} KiB, {} packages): {:.2} ms",
        bytes.len() / 1024,
        graph.packages.len(),
        start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
    );
    let snaps = snapshots(&install.join("migration/__snapshots__/npm-arborist.test.ts.snap"));
    let lock = expected(&snaps["package-lock.json migration fixes arborist fixtures audit-omit 1"])
        .lock
        .unwrap();
    let start = std::time::Instant::now();
    let mut parsed = None;
    for _ in 0..n {
        parsed = Some(
            bun_text::parse_with_semver(lock.as_bytes(), &options, &super::support::TestSemver)
                .unwrap(),
        );
    }
    let parsed = parsed.unwrap();
    eprintln!(
        "bun.lock ({} KiB, {} packages): {:.2} ms",
        lock.len() / 1024,
        parsed.graph.packages.len(),
        start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
    );
    let tree = tree_from_bun_paths(&parsed.graph, &parsed.paths);
    let start = std::time::Instant::now();
    for _ in 0..n {
        std::hint::black_box(bun_text::serialize_loaded(
            &parsed.graph,
            &tree,
            parsed.version,
        ));
    }
    eprintln!(
        "bun.lock write: {:.2} ms",
        start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
    );
    if let Ok(lockb) = std::fs::read(install.join("bun.lockb.bin-linking")) {
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(crate::formats::bun_binary::parse(&lockb).unwrap());
        }
        eprintln!(
            "bun.lockb ({} KiB): {:.3} ms",
            lockb.len() / 1024,
            start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
        );
    }
}
