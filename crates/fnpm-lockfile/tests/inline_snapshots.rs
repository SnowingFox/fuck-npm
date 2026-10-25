//! The snapshot-backed cases of bun's yarn and pnpm migration tests. Each project was
//! extracted verbatim from the test's `tempDir(...)` files into `tests/fixtures/inline/<test
//! file>/<snapshot name>/`, and the migrated graph is compared with bun's `.snap` bun.lock.

mod common;

use std::path::Path;

use common::*;
use fnpm_lockfile::formats::foreign::Migration;
use fnpm_lockfile::formats::{pnpm, yarn_v1};
use serde_json::Value;

fn migrate(dir: &Path, registry: &str) -> Migration {
    let ws = workspace(dir);
    let files = ScopedFiles(dir.to_path_buf());
    let mut o = options(&files);
    o.registry = registry.into();
    if dir.join("pnpm-lock.yaml").exists() {
        pnpm::migrate(&std::fs::read(dir.join("pnpm-lock.yaml")).unwrap(), &ws, &o)
    } else {
        yarn_v1::migrate(&std::fs::read(dir.join("yarn.lock")).unwrap(), &ws, &o)
    }
    .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
}

fn cases(test: &str) -> Vec<(String, std::path::PathBuf)> {
    let root = fixtures().join("inline").join(test);
    let mut v: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    v.sort();
    v
}

fn check(test: &str, registry: &str, check_integrity: bool) -> usize {
    let mut n = 0;
    for (name, dir) in cases(test) {
        let (key, body) = snapshot_text_by_suffix(&format!("{test}.test.ts.snap"), &name);
        let m = migrate(&dir, registry);
        let value = jsonc(&body);
        if value.get("lockfileVersion").is_some() {
            assert_matches_bun_lock_with(&m.graph, &value, registry, check_integrity);
        } else {
            // a package.json snapshot: the migrated root package.json
            let mut root: Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap())
                    .unwrap();
            for e in m.write_back.iter().filter(|e| e.path.is_empty()) {
                e.apply_to(&mut root);
            }
            assert_eq!(root, value, "{key}");
        }
        n += 1;
    }
    n
}

#[test]
fn yarn_lock_migration_snapshots() {
    assert_eq!(
        check("yarn-lock-migration", "https://registry.npmjs.org/", true),
        8
    );
}

#[test]
fn pnpm_lock_migration_snapshots() {
    assert_eq!(
        check("pnpm-lock-migration", "https://registry.npmjs.org/", true),
        4
    );
}

#[test]
fn pnpm_comprehensive_snapshots() {
    assert_eq!(
        check("pnpm-comprehensive", "https://registry.npmjs.org/", true),
        4
    );
}

#[test]
fn pnpm_migration_complete_snapshots() {
    assert_eq!(
        check(
            "pnpm-migration-complete",
            "https://registry.npmjs.org/",
            true
        ),
        13
    );
}
