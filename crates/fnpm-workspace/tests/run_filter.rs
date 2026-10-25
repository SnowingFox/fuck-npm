//! `fnpm run --filter` / `--workspaces` package discovery, selection and order, ported from
//! bun's `test/cli/run/filter-workspace.test.ts`.

mod common;

use std::path::Path;

use common::{pkg, tree};
use fnpm_workspace::{Error, RunPackages};
use serde_json::json;

fn cwd_root() -> tempfile::TempDir {
    let scripts = |s: &str| json!({ "scripts": { "present": format!("echo {s}") } });
    tree(&[
        ("package.json", &json!({ "name": "ws", "scripts": { "present": "echo rootscript" }, "workspaces": ["packages/*"] }).to_string()),
        ("packages/pkga/package.json", &pkg("pkga", scripts("scripta"))),
        ("packages/scoped/package.json", &pkg("@scoped/scoped", scripts("scriptd"))),
        ("packages/pkgb/package.json", &pkg("pkgb", scripts("scriptb"))),
        ("packages/dirname/package.json", &pkg("pkgc", scripts("scriptc"))),
        ("packages/malformed1/package.json", &json!({ "scripts": { "present": "echo malformed1" } }).to_string()),
        ("packages/malformed2/package.json", "asdfsadfas"),
        ("packages/missing/foo", "bar"),
    ])
}

/// Edges: web -> api -> shared -> pkg-a; pkg-b and the root are isolated.
fn graph_root() -> tempfile::TempDir {
    tree(&[
        ("package.json", &json!({ "name": "ws", "workspaces": ["packages/*"], "scripts": { "present": "echo out-root" } }).to_string()),
        ("packages/web/package.json", &pkg("web", json!({ "dependencies": { "api": "workspace:*" } }))),
        ("packages/api/package.json", &pkg("api", json!({ "devDependencies": { "shared": "workspace:*" } }))),
        ("packages/shared/package.json", &pkg("shared", json!({ "dependencies": { "pkg-a": "*" } }))),
        ("packages/pkg-a/package.json", &pkg("pkg-a", json!({}))),
        ("packages/pkg-b/package.json", &pkg("pkg-b", json!({}))),
    ])
}

fn run(cwd: &Path, filters: &[&str]) -> Vec<String> {
    let r = RunPackages::discover(cwd, filters, false).unwrap();
    names(&r)
}

fn names(r: &RunPackages) -> Vec<String> {
    let mut v: Vec<String> = r
        .packages
        .iter()
        .map(|p| p.display_name(&r.root_dir))
        .collect();
    v.sort();
    v
}

#[test]
fn resolve_names_from_every_directory() {
    let dir = cwd_root();
    let r = dir.path();
    let dirs = [
        r.to_path_buf(),
        r.join("packages"),
        r.join("packages/pkga"),
        r.join("packages/pkgb"),
        r.join("packages/dirname"),
        r.join("packages/scoped"),
    ];
    let all = ["@scoped/scoped", "pkga", "pkgb", "pkgc"];
    for d in &dirs {
        for name in all {
            assert_eq!(run(d, &[name]), [name], "{name} from {}", d.display());
        }
        assert_eq!(run(d, &["*"]), all, "* from {}", d.display());
        assert_eq!(run(d, &all), all);
    }
}

#[test]
fn path_globs_include_nameless_packages() {
    let dir = cwd_root();
    let r = dir.path();
    let with_nameless = [
        "@scoped/scoped",
        "packages/malformed1",
        "pkga",
        "pkgb",
        "pkgc",
    ];
    assert_eq!(run(r, &["./packages/*"]), with_nameless);
    assert_eq!(run(r, &["./**"]), with_nameless);
    assert_eq!(run(r, &["./packages/pkg*"]), ["pkga", "pkgb"]);
    assert_eq!(run(r, &["./packages/malformed1"]), ["packages/malformed1"]);
    // '*' still skips a package.json without a name
    assert_eq!(run(r, &["*"]), ["@scoped/scoped", "pkga", "pkgb", "pkgc"]);
    // a negation-only filter set still skips a package.json without a name
    assert_eq!(run(r, &["!pkga"]), ["@scoped/scoped", "pkgb", "pkgc"]);
}

#[test]
fn malformed_package_json_is_skipped_with_a_warning() {
    let dir = cwd_root();
    let r = RunPackages::discover(dir.path(), &["*"], false).unwrap();
    assert_eq!(r.warnings.len(), 1);
    let w = &r.warnings[0];
    assert!(w.starts_with("Failed to read \""), "{w}");
    assert!(
        w.contains("malformed2/package.json") && w.ends_with("skipping this workspace package"),
        "{w}"
    );
}

#[test]
fn no_match_and_unmatched_patterns() {
    let dir = cwd_root();
    let r = RunPackages::discover(dir.path(), &["nosuchpkg"], false).unwrap();
    assert!(r.packages.is_empty());
    assert_eq!(
        RunPackages::nothing_selected_message(&["nosuchpkg"], false),
        "No workspace packages matched the filter \"nosuchpkg\""
    );
    assert_eq!(
        RunPackages::nothing_selected_message::<&str>(&[], true),
        "No workspace packages found"
    );
    let r = RunPackages::discover(dir.path(), &["pkga", "typo"], false).unwrap();
    assert_eq!(names(&r), ["pkga"]);
    assert_eq!(r.unmatched, ["typo"]);
}

#[test]
fn script_not_found_messages() {
    let dir = cwd_root();
    let one = RunPackages::discover(dir.path(), &["pkga"], false).unwrap();
    assert_eq!(
        one.script_not_found_message("notpresent", &["pkga"], false),
        "Script \"notpresent\" not found in package \"pkga\""
    );
    let many = RunPackages::discover(dir.path(), &["*"], false).unwrap();
    assert_eq!(
        many.script_not_found_message("x", &["*"], false),
        "Script \"x\" not found in 4 packages matching \"*\""
    );
    let ws = RunPackages::discover::<&str>(dir.path(), &[], true).unwrap();
    assert_eq!(
        ws.script_not_found_message("x", &[] as &[&str], true),
        "Script \"x\" not found in 4 workspace packages"
    );
    let nameless = RunPackages::discover(dir.path(), &["./packages/malformed1"], false).unwrap();
    assert_eq!(
        nameless.script_not_found_message("x", &["./packages/malformed1"], false),
        "Script \"x\" not found in package \"packages/malformed1\""
    );
}

#[test]
fn selectors() {
    let dir = graph_root();
    let r = dir.path();
    assert_eq!(run(r, &["api..."]), ["api", "pkg-a", "shared"]);
    assert_eq!(run(r, &["api^..."]), ["pkg-a", "shared"]);
    assert_eq!(run(r, &["...api"]), ["api", "web"]);
    assert_eq!(run(r, &["...^api"]), ["web"]);
    assert_eq!(
        run(r, &["...{./packages/pkg-a}"]),
        ["api", "pkg-a", "shared", "web"]
    );
    // the root's own scripts are never selected
    assert_eq!(
        run(r, &["{packages}"]),
        ["api", "pkg-a", "pkg-b", "shared", "web"]
    );
    assert_eq!(
        run(&r.join("packages"), &["{.}"]),
        ["api", "pkg-a", "pkg-b", "shared", "web"]
    );
    assert_eq!(run(r, &["{.}"]), ["api", "pkg-a", "pkg-b", "shared", "web"]);
    assert_eq!(run(r, &["*", "!web"]), ["api", "pkg-a", "pkg-b", "shared"]);
    assert_eq!(run(r, &["!web"]), ["api", "pkg-a", "pkg-b", "shared"]);
    assert_eq!(run(r, &["!web", "!pkg-b"]), ["api", "pkg-a", "shared"]);
    assert_eq!(
        run(r, &["*", "!./packages/web"]),
        ["api", "pkg-a", "pkg-b", "shared"]
    );
    assert_eq!(
        run(r, &["!{./packages/web}"]),
        ["api", "pkg-a", "pkg-b", "shared"]
    );
    assert_eq!(run(r, &["*", "!...api"]), ["pkg-a", "pkg-b", "shared"]);
    let e = RunPackages::discover(r, &["..."], false).unwrap_err();
    assert!(matches!(e, Error::BadFilter { .. }));
    assert!(
        e.to_string()
            .contains("is missing a workspace name or path")
    );
}

#[test]
fn relations_follow_optional_but_not_peer_dependencies() {
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "ws", "workspaces": ["packages/*"] }).to_string(),
        ),
        (
            "packages/app/package.json",
            &pkg(
                "app",
                json!({ "optionalDependencies": { "optlib": "workspace:*" }, "peerDependencies": { "peerlib": "workspace:*" } }),
            ),
        ),
        ("packages/optlib/package.json", &pkg("optlib", json!({}))),
        ("packages/peerlib/package.json", &pkg("peerlib", json!({}))),
    ]);
    assert_eq!(run(dir.path(), &["app..."]), ["app", "optlib"]);
}

/// The `"workspaces"` `!` fixture.
fn negated_fixture(workspaces: serde_json::Value) -> tempfile::TempDir {
    tree(&[
        (
            "package.json",
            &json!({ "name": "ws", "workspaces": workspaces }).to_string(),
        ),
        (
            "packages/app/package.json",
            &pkg("app", json!({ "dependencies": { "lib": "workspace:*" } })),
        ),
        ("packages/lib/package.json", &pkg("lib", json!({}))),
        (
            "packages/legacy/package.json",
            &pkg(
                "legacy",
                json!({ "dependencies": { "lib": "workspace:*" } }),
            ),
        ),
        (
            "packages/legacy/nested/package.json",
            &pkg("legacy-nested", json!({})),
        ),
        ("other/legacy/package.json", &pkg("other-legacy", json!({}))),
        ("other/lib/package.json", &pkg("other-lib", json!({}))),
    ])
}

fn rel_dirs(r: &RunPackages) -> Vec<String> {
    let mut v: Vec<String> = r
        .packages
        .iter()
        .map(|p| {
            p.dir
                .strip_prefix(&r.root_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    v.sort();
    v
}

#[test]
fn negated_workspaces_entries() {
    let dir = negated_fixture(json!(["packages/*", "!packages/legacy"]));
    let r = dir.path();
    for filters in [
        vec!["*"],
        vec!["./packages/*"],
        vec!["{packages}"],
        vec!["...lib"],
    ] {
        let got = RunPackages::discover(r, &filters, false).unwrap();
        assert_eq!(
            rel_dirs(&got),
            ["packages/app", "packages/lib"],
            "{filters:?}"
        );
    }
    let got = RunPackages::discover::<&str>(r, &[], true).unwrap();
    assert_eq!(rel_dirs(&got), ["packages/app", "packages/lib"]);
    let got = RunPackages::discover(r, &["l*"], false).unwrap();
    assert_eq!(rel_dirs(&got), ["packages/lib"]);
    // the excluded package is not selectable by name
    let got = RunPackages::discover(r, &["legacy"], false).unwrap();
    assert!(got.packages.is_empty());
}

#[test]
fn run_and_install_find_the_same_members() {
    let configs: Vec<(serde_json::Value, Vec<&str>)> = vec![
        (
            json!({ "packages": ["packages/*", "!packages/legacy"] }),
            vec!["packages/app", "packages/lib"],
        ),
        (
            json!(["packages/*", "!**/legacy"]),
            vec!["packages/app", "packages/lib"],
        ),
        (
            json!(["packages/*", "!packages/{app,legacy}"]),
            vec!["packages/lib"],
        ),
        (
            json!(["packages/**", "!packages/legacy"]),
            vec!["packages/app", "packages/legacy/nested", "packages/lib"],
        ),
        (
            json!(["./packages/*", "!./packages/legacy"]),
            vec!["packages/app", "packages/lib"],
        ),
        (
            json!(["packages/*", "!././packages/legacy//"]),
            vec!["packages/app", "packages/lib"],
        ),
        (
            json!(["!./packages/legacy/", "packages/*"]),
            vec!["packages/app", "packages/lib"],
        ),
        (
            json!(["packages/*", "!packages/legacy", "packages/leg*"]),
            vec!["packages/app", "packages/lib"],
        ),
        (
            json!(["packages/**", "!packages/legacy/**"]),
            vec!["packages/app", "packages/lib"],
        ),
        (
            json!(["packages/*", "!packages/leg*/**"]),
            vec!["packages/app", "packages/lib"],
        ),
        (
            json!(["packages/lib", "!packages/lib"]),
            vec!["packages/lib"],
        ),
    ];
    for (workspaces, members) in configs {
        let dir = negated_fixture(workspaces.clone());
        let got = RunPackages::discover::<&str>(dir.path(), &[], true).unwrap();
        assert_eq!(rel_dirs(&got), members, "run {workspaces}");
        let ws = fnpm_workspace::Workspace::load(dir.path()).unwrap();
        let paths: Vec<&str> = ws.members.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, members, "install {workspaces}");
    }
}

#[test]
fn without_a_workspace_root_every_package_json_is_a_candidate() {
    // "respect dependency order": no root package.json, dep1 depends on dep0
    let dir = tree(&[
        (
            "dep1/package.json",
            &pkg("dep1", json!({ "dependencies": { "dep0": "*" } })),
        ),
        ("dep0/package.json", &pkg("dep0", json!({}))),
        ("dep0/node_modules/x/package.json", &pkg("x", json!({}))),
        (".hidden/package.json", &pkg("hidden", json!({}))),
    ]);
    let r = RunPackages::discover(dir.path(), &["*"], false).unwrap();
    // bun's run walk matches dot-directories
    assert_eq!(names(&r), ["dep0", "dep1", "hidden"]);
    let order = RunPackages::discover(dir.path(), &["dep*"], false).unwrap();
    let (levels, cycles) = order.topo_levels();
    assert!(cycles.is_empty());
    let level_names: Vec<Vec<&str>> = levels
        .iter()
        .map(|l| l.iter().map(|&i| order.packages[i].name.as_str()).collect())
        .collect();
    assert_eq!(level_names, [vec!["dep0"], vec!["dep1"]]);
    // "dependency order is kept inside a relation selection"
    let rel = RunPackages::discover(dir.path(), &["dep1..."], false).unwrap();
    assert_eq!(names(&rel), ["dep0", "dep1"]);
}

#[test]
fn cycles_drop_the_order() {
    // "detect cycle of length > 2": dep2's package.json is named dep1 as well
    let dir = tree(&[
        (
            "dep0/package.json",
            &pkg("dep0", json!({ "dependencies": { "dep1": "*" } })),
        ),
        (
            "dep1/package.json",
            &pkg("dep1", json!({ "dependencies": { "dep2": "*" } })),
        ),
        (
            "dep2/package.json",
            &pkg("dep1", json!({ "dependencies": { "dep0": "*" } })),
        ),
    ]);
    let r = RunPackages::discover(dir.path(), &["*"], false).unwrap();
    assert_eq!(r.packages.len(), 3);
    let (levels, cycles) = r.topo_levels();
    assert_eq!(levels, vec![vec![0, 1, 2]]);
    assert!(!cycles.is_empty());
}

#[test]
fn workspaces_flag_skips_the_root() {
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "ws", "workspaces": [".", "packages/*"] }).to_string(),
        ),
        ("packages/a/package.json", &pkg("a", json!({}))),
    ]);
    assert_eq!(
        names(&RunPackages::discover::<&str>(dir.path(), &[], true).unwrap()),
        ["a"]
    );
    // with `.` listed, `*` selects the root package too
    assert_eq!(run(dir.path(), &["*"]), ["a", "ws"]);
}

#[test]
fn non_string_workspaces_items_fail() {
    let dir = tree(&[("package.json", r#"{"workspaces": ["a", 1]}"#)]);
    let e = RunPackages::discover(dir.path(), &["*"], false)
        .unwrap_err()
        .to_string();
    assert!(
        e.ends_with("Failed to parse \"workspaces\" property: all items must be strings"),
        "{e}"
    );
}

#[cfg(unix)]
#[test]
fn self_referential_symlink_does_not_loop() {
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "ws", "workspaces": ["packages/**"] }).to_string(),
        ),
        ("packages/pkga/package.json", &pkg("pkga", json!({}))),
        ("packages/cyc/package.json", &pkg("cyc", json!({}))),
    ]);
    std::os::unix::fs::symlink(
        dir.path().join("packages/cyc"),
        dir.path().join("packages/cyc/loop"),
    )
    .unwrap();
    let r = RunPackages::discover(dir.path(), &["*"], false).unwrap();
    // `pkga` once; `cyc` at packages/cyc and once more through its own `loop` alias
    assert_eq!(names(&r), ["cyc", "cyc", "pkga"]);
}

#[cfg(unix)]
#[test]
fn shared_directory_root_is_ignored_with_a_warning() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tree(&[
        (
            "shared/package.json",
            &json!({ "name": "evil", "workspaces": ["**"] }).to_string(),
        ),
        ("shared/proj/package.json", &pkg("proj", json!({}))),
        ("shared/proj/sub/package.json", &pkg("sub", json!({}))),
        ("shared/other/package.json", &pkg("other", json!({}))),
    ]);
    let shared = dir.path().join("shared");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    let r = RunPackages::discover(&shared.join("proj"), &["*"], false).unwrap();
    assert_eq!(r.root_dir, shared.join("proj"));
    assert_eq!(names(&r), ["proj", "sub"]);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("directory is writable by other users")),
        "{:?}",
        r.warnings
    );
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    let r = RunPackages::discover(&shared.join("proj"), &["*"], false).unwrap();
    assert_eq!(r.root_dir, shared);
}

#[test]
fn packages_with_odd_dependency_shapes_are_still_runnable() {
    // bun's runtime package.json reader ignores non-object groups and non-string versions.
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "ws", "workspaces": ["packages/*"] }).to_string(),
        ),
        (
            "packages/a/package.json",
            &pkg(
                "a",
                json!({ "dependencies": { "b": "workspace:*", "x": 1 }, "devDependencies": ["y"] }),
            ),
        ),
        ("packages/b/package.json", &pkg("b", json!({}))),
    ]);
    let r = RunPackages::discover(dir.path(), &["a..."], false).unwrap();
    assert_eq!(names(&r), ["a", "b"]);
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
}
