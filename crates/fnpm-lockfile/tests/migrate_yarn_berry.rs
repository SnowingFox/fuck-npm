//! yarn berry migration, against bun's fixtures and expectations
//! (`test/cli/install/migration/yarn-berry.test.ts`).

mod common;

use std::path::Path;

use common::*;
use fnpm_lockfile::formats::foreign::Migration;
use fnpm_lockfile::formats::yarn_v1;
use fnpm_types::{Behavior, ConfigVersion, LockGraph, Resolution};
use serde_json::{Value, json};

/// The registry bun's tests run against; tarballs there are at the default location.
const REGISTRY: &str = "http://localhost:1234/";

fn run(dir: &Path) -> fnpm_lockfile::Result<Migration> {
    let ws = workspace(dir);
    let files = ScopedFiles(dir.to_path_buf());
    let mut o = options(&files);
    o.registry = REGISTRY.into();
    o.home = Some(dir.to_path_buf());
    let bytes = std::fs::read(dir.join("yarn.lock")).unwrap();
    // berry goes through the yarn.lock entry point, like bun's migration
    yarn_v1::migrate(&bytes, &ws, &o)
}

fn fixture(name: &str) -> Migration {
    run(&fixtures().join("yarn-berry").join(name)).unwrap()
}

fn project(files: &[(&str, String)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    dir
}

/// yarn.lock text from `key -> the lines of that entry` (bun's `yarnLock` helper).
fn yarn_lock(entries: &[(&str, Vec<String>)]) -> String {
    let mut out = String::from("__metadata:\n  version: 8\n  cacheKey: 10c0\n");
    for (key, lines) in entries {
        out.push_str(&format!("\n{}:\n", serde_json::to_string(key).unwrap()));
        for line in lines {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

fn lines(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// `name@version` of every package that is not the root, like bun's `lockedVersions`.
fn locked(g: &LockGraph) -> Vec<String> {
    let mut v: Vec<String> = g.packages.iter().skip(1).map(bun_id).collect();
    v.sort();
    v
}

fn target<'g>(g: &'g LockGraph, owner: usize, name: &str) -> &'g fnpm_types::Package {
    let e = g.packages[owner]
        .dependency(name)
        .unwrap_or_else(|| panic!("no edge {name}"));
    &g.packages[e
        .target
        .unwrap_or_else(|| panic!("{name} unresolved"))
        .index()]
}

fn root_edit(m: &Migration) -> Value {
    let mut root = json!({});
    for e in m.write_back.iter().filter(|e| e.path.is_empty()) {
        e.apply_to(&mut root);
    }
    root
}

#[test]
fn npm_packages_keep_the_versions_yarn_lock_pinned() {
    let m = fixture("basic");
    let g = &m.graph;
    assert_eq!(
        locked(g),
        [
            "@types/is-number@1.0.0",
            "a-dep@1.0.3",
            "no-deps@1.0.0",
            "one-range-dep@1.0.0",
            "peer-deps-fixed@1.0.0",
            "what-bin@1.0.0"
        ]
    );
    assert!(g.trusted_dependencies.is_none());
    assert_eq!(g.config_version, ConfigVersion::V0);
    // yarn's `checksum` is not the tarball's hash; integrity comes from the registry on install
    assert!(
        g.packages
            .iter()
            .all(|p| p.resolution.integrity().is_none())
    );
    assert!(matches!(
        &target(g, 0, "no-deps").resolution,
        Resolution::Npm {
            tarball_url: Some(_),
            ..
        }
    ));
    let peer = target(g, 0, "peer-deps-fixed");
    let peer_edge = peer.dependency("no-deps").unwrap();
    assert!(peer_edge.behavior.is_peer());
    assert_eq!(peer_edge.literal, "^1.0.0");
    assert_eq!(
        peer_edge.target,
        g.root().dependency("no-deps").unwrap().target
    );
    assert_eq!(
        target(g, 0, "what-bin").bin,
        fnpm_types::Bin::NamedFile {
            name: "what-bin".into(),
            path: "what-bin.js".into()
        }
    );
    // what-bin is in dependencies (^1.5.0) and optionalDependencies (^1.0.0): the optional entry wins
    let what = g.root().dependency("what-bin").unwrap();
    assert_eq!(what.literal, "^1.0.0");
    assert_eq!(what.behavior, Behavior::OPTIONAL);
    assert_eq!(
        g.root()
            .dependencies
            .iter()
            .filter(|e| e.name == "what-bin")
            .count(),
        1
    );
    assert!(m.write_back.is_empty());
    g.validate().unwrap();
}

#[test]
fn npm_aliases() {
    let m = fixture("aliases");
    let g = &m.graph;
    assert_eq!(
        locked(g),
        [
            "7-no-deps@1.0.0",
            "no-deps@1.0.0",
            "no-deps@1.0.1",
            "no-deps@2.0.0",
            "one-dep@1.0.0",
            "various-requires@1.0.0"
        ]
    );
    let root = g.root();
    assert_eq!(
        root.dependency("aliased").unwrap().literal,
        "npm:no-deps@^1.0.0"
    );
    assert_eq!(bun_id(target(g, 0, "aliased")), "no-deps@1.0.0");
    // targets starting with `v` or a digit are still aliases, not `v1.2.3` / `1.2.3` ranges
    assert_eq!(
        root.dependency("v-alias").unwrap().literal,
        "npm:various-requires@^1.0.0"
    );
    assert_eq!(bun_id(target(g, 0, "v-alias")), "various-requires@1.0.0");
    assert_eq!(bun_id(target(g, 0, "seven")), "7-no-deps@1.0.0");
    assert_eq!(bun_id(target(g, 0, "no-deps")), "no-deps@2.0.0");
    let one_dep = g
        .root()
        .dependency("one-dep")
        .unwrap()
        .target
        .unwrap()
        .index();
    assert_eq!(bun_id(target(g, one_dep, "no-deps")), "no-deps@1.0.1");
}

#[test]
fn a_package_json_range_written_with_the_npm_protocol() {
    for range in ["npm:^1.0.0", "npm:1.0.0", "npm:1", "npm:*", "npm:latest"] {
        let plain = &range["npm:".len()..];
        let member =
            json!({ "name": "pkg-a", "version": "1.0.0", "devDependencies": { "no-deps": range } });
        let dir = project(&[
            (
                "package.json",
                json!({
                    "name": "berry-npm-protocol",
                    "workspaces": ["packages/*"],
                    "dependencies": { "no-deps": range, "one-range-dep": "^1.0.0" },
                    "resolutions": { "one-range-dep/no-deps": range }
                })
                .to_string(),
            ),
            ("packages/pkg-a/package.json", member.to_string()),
            (
                "yarn.lock",
                yarn_lock(&[
                    (
                        "berry-npm-protocol@workspace:.",
                        lines(&[
                            "resolution: \"berry-npm-protocol@workspace:.\"",
                            "dependencies:",
                            &format!("  no-deps: \"{range}\""),
                            "  one-range-dep: \"npm:^1.0.0\"",
                        ]),
                    ),
                    (
                        &format!("no-deps@{range}"),
                        lines(&["resolution: \"no-deps@npm:1.0.0\""]),
                    ),
                    (
                        "one-range-dep@npm:^1.0.0",
                        lines(&[
                            "resolution: \"one-range-dep@npm:1.0.0\"",
                            "dependencies:",
                            "  no-deps: \"npm:^1.0.0\"",
                        ]),
                    ),
                    (
                        "pkg-a@workspace:packages/pkg-a",
                        lines(&[
                            "resolution: \"pkg-a@workspace:packages/pkg-a\"",
                            "dependencies:",
                            &format!("  no-deps: \"{range}\""),
                        ]),
                    ),
                ]),
            ),
        ]);
        let m = run(dir.path()).unwrap_or_else(|e| panic!("{range}: {e}"));
        assert_eq!(
            locked(&m.graph),
            [
                "no-deps@1.0.0",
                "one-range-dep@1.0.0",
                "pkg-a@workspace:packages/pkg-a"
            ],
            "{range}"
        );
        let root = root_edit(&m);
        assert_eq!(
            root["dependencies"],
            json!({ "no-deps": plain, "one-range-dep": "^1.0.0" }),
            "{range}"
        );
        assert_eq!(
            root["resolutions"],
            json!({ "one-range-dep/no-deps": plain }),
            "{range}"
        );
        let ws_edit = m
            .write_back
            .iter()
            .find(|e| e.path == "packages/pkg-a")
            .unwrap();
        let mut member_after = member.clone();
        ws_edit.apply_to(&mut member_after);
        assert_eq!(member_after["devDependencies"], json!({ "no-deps": plain }));
        assert_eq!(m.graph.root().dependency("no-deps").unwrap().literal, plain);
    }
}

#[test]
fn workspaces_and_workspace_ranges_match_bun_snapshot() {
    let m = fixture("workspaces");
    let lock = snapshot(
        "yarn-berry.test.ts.snap",
        "yarn berry migration workspaces and workspace: ranges 1",
    );
    assert_matches_bun_lock_with(&m.graph, &lock, REGISTRY, false);
    let g = &m.graph;
    let a = g.workspaces["packages/pkg-a"];
    let b = g.workspaces["packages/pkg-b"];
    assert_eq!(g.packages[a.index()].version, "1.2.3");
    assert_eq!(g.root().dependency("pkg-a").unwrap().target, Some(a));
    assert!(
        g.root()
            .dependency("pkg-a")
            .unwrap()
            .behavior
            .is_workspace()
    );
    assert_eq!(
        g.packages[a.index()].dependency("pkg-b").unwrap().target,
        Some(b)
    );
    assert_eq!(
        g.packages[b.index()].dependency("pkg-a").unwrap().target,
        Some(a)
    );
    assert_eq!(
        g.packages[a.index()].bin,
        fnpm_types::Bin::NamedFile {
            name: "pkg-a".into(),
            path: "./bin.js".into()
        }
    );
    // (fnpm-types normalises bin targets; bun.lock writes "./bin.js")
}

#[test]
fn patch_protocol_becomes_patched_dependencies() {
    let m = fixture("patch");
    let root = root_edit(&m);
    assert_eq!(
        root["dependencies"],
        json!({ "no-deps": "1.0.0", "one-dep": "^1.0.0", "optional-native": "^1.0.0" })
    );
    assert_eq!(root["resolutions"], json!({ "one-dep/no-deps": "1.0.1" }));
    assert_eq!(
        root["patchedDependencies"],
        json!({
            "no-deps@1.0.0": ".yarn/patches/no-deps-npm-1.0.0-d5a9b7e1c2.patch",
            "no-deps@1.0.1": ".yarn/patches/no-deps-npm-1.0.1-aa11bb22cc.patch"
        })
    );
    let edit = &m.write_back[0];
    assert_eq!(
        edit.changes,
        [
            "rewrote patch:/portal:/link:/npm: ranges",
            "added patchedDependencies"
        ]
    );
    let g = &m.graph;
    let patched: Vec<(&str, &str)> = g
        .patched_dependencies
        .iter()
        .map(|(k, v)| (k.as_str(), v.path.as_str()))
        .collect();
    assert_eq!(
        patched,
        [
            (
                "no-deps@1.0.0",
                ".yarn/patches/no-deps-npm-1.0.0-d5a9b7e1c2.patch"
            ),
            (
                "no-deps@1.0.1",
                ".yarn/patches/no-deps-npm-1.0.1-aa11bb22cc.patch"
            )
        ]
    );
    // yarn's builtin compat patch folds onto the plain package
    assert_eq!(
        locked(g),
        [
            "native-bar-x64@1.0.0",
            "native-foo-x64@1.0.0",
            "native-foo-x86@1.0.0",
            "native-libc-glibc@1.0.0",
            "native-libc-musl@1.0.0",
            "no-deps@1.0.0",
            "no-deps@1.0.1",
            "one-dep@1.0.0",
            "optional-native@1.0.0"
        ]
    );
    // `conditions` become os/cpu/libc
    let by_id = |id: &str| g.packages.iter().find(|p| bun_id(p) == id).unwrap();
    let x64 = by_id("native-foo-x64@1.0.0");
    assert_eq!(x64.platform.cpu.to_list(), ["x64"]);
    assert_eq!(x64.platform.os, fnpm_types::OsSet::NONE);
    assert_eq!(
        by_id("native-libc-musl@1.0.0").platform.libc.to_list(),
        ["musl"]
    );
    // the resolution `one-dep/no-deps` pins one-dep's no-deps to the patched 1.0.1
    let one_dep = g
        .root()
        .dependency("one-dep")
        .unwrap()
        .target
        .unwrap()
        .index();
    assert_eq!(bun_id(target(g, one_dep, "no-deps")), "no-deps@1.0.1");
    assert_eq!(bun_id(target(g, 0, "no-deps")), "no-deps@1.0.0");
    // the graph's overrides come from the rewritten resolutions
    assert_eq!(g.overrides.len(), 1);
    assert_eq!(g.overrides[0].value, "1.0.1");
}

#[test]
fn resolutions_that_rewrote_a_descriptor_are_followed() {
    let m = fixture("resolutions");
    assert_eq!(
        locked(&m.graph),
        ["a-dep@1.0.5", "no-deps@1.0.0", "one-range-dep@1.0.0"]
    );
    assert_eq!(bun_id(target(&m.graph, 0, "a-dep")), "a-dep@1.0.5");
}

#[test]
fn file_and_portal_dependencies() {
    let m = fixture("protocols");
    assert!(m.warnings.iter().any(|w| {
        w.contains("migrating \"local-portal@portal:./local-portal\" as \"file:local-portal\"")
    }));
    let root = root_edit(&m);
    assert_eq!(
        root["dependencies"],
        json!({ "local-file": "file:./local-file", "local-portal": "file:./local-portal" })
    );
    // a `portal:` target of a resolution is rewritten too
    assert_eq!(
        root["resolutions"],
        json!({ "a-dep": "file:./local-a-dep" })
    );
    let g = &m.graph;
    assert_eq!(
        locked(g),
        [
            "a-dep@file:local-a-dep",
            "local-file@file:local-file",
            "local-portal@file:local-portal",
            "no-deps@1.0.1",
            "sub@file:local-portal/sub"
        ]
    );
    let portal = g
        .root()
        .dependency("local-portal")
        .unwrap()
        .target
        .unwrap()
        .index();
    assert_eq!(
        bun_id(target(g, portal, "sub")),
        "sub@file:local-portal/sub"
    );
    // the resolution binds the portal's a-dep to the root's portal folder
    assert_eq!(bun_id(target(g, portal, "a-dep")), "a-dep@file:local-a-dep");
    // migrating again reads its own `file:` spelling of `portal:`
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixtures().join("yarn-berry/protocols"), dir.path());
    let mut pkg: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("package.json")).unwrap())
            .unwrap();
    for e in &m.write_back {
        e.apply_to(&mut pkg);
    }
    std::fs::write(dir.path().join("package.json"), pkg.to_string()).unwrap();
    let again = run(dir.path()).unwrap();
    assert_eq!(locked(&again.graph), locked(g));
}

fn copy_dir(from: &Path, to: &Path) {
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let dest = to.join(e.file_name());
        if e.path().is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

#[test]
fn an_invalid_berry_lockfile_is_reported() {
    let err = run(&fixtures().join("yarn-berry/malformed"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("yarn.lock entry \"no-deps@npm:^1.0.0\" has no resolution"),
        "{err}"
    );
}

#[test]
fn a_name_at_range_resolution_only_applies_to_that_range() {
    let dir = project(&[
        (
            "package.json",
            json!({ "name": "berry-range-resolution", "dependencies": { "no-deps": "^1.1.0", "one-range-dep": "^1.0.0" }, "resolutions": { "no-deps@npm:^1.0.0": "1.0.0" } }).to_string(),
        ),
        (
            "yarn.lock",
            yarn_lock(&[
                ("berry-range-resolution@workspace:.", lines(&["resolution: \"berry-range-resolution@workspace:.\"", "dependencies:", "  no-deps: \"npm:^1.1.0\"", "  one-range-dep: \"npm:^1.0.0\""])),
                ("no-deps@npm:1.0.0", lines(&["version: 1.0.0", "resolution: \"no-deps@npm:1.0.0\""])),
                ("no-deps@npm:^1.1.0", lines(&["version: 1.1.0", "resolution: \"no-deps@npm:1.1.0\""])),
                ("one-range-dep@npm:^1.0.0", lines(&["version: 1.0.0", "resolution: \"one-range-dep@npm:1.0.0\"", "dependencies:", "  no-deps: \"npm:^1.0.0\""])),
            ]),
        ),
    ]);
    let m = run(dir.path()).unwrap();
    let g = &m.graph;
    assert_eq!(bun_id(target(g, 0, "no-deps")), "no-deps@1.1.0");
    let one = g
        .root()
        .dependency("one-range-dep")
        .unwrap()
        .target
        .unwrap()
        .index();
    assert_eq!(bun_id(target(g, one, "no-deps")), "no-deps@1.0.0");
}

#[test]
fn catalog_ranges_from_yarnrc() {
    let dir = project(&[
        (
            "package.json",
            json!({ "name": "berry-catalog", "workspaces": ["packages/*"], "dependencies": { "no-deps": "catalog:", "a-dep": "catalog:pinned" } }).to_string(),
        ),
        ("packages/pkg-a/package.json", json!({ "name": "pkg-a", "version": "1.0.0", "dependencies": { "no-deps": "catalog:" } }).to_string()),
        (".yarnrc.yml", "nodeLinker: node-modules\n\ncatalog:\n  no-deps: \"npm:^1.0.0\"\n\ncatalogs:\n  pinned:\n    a-dep: ~1.0.2\n".into()),
        (
            "yarn.lock",
            yarn_lock(&[
                ("a-dep@npm:~1.0.2", lines(&["version: 1.0.4", "resolution: \"a-dep@npm:1.0.4\""])),
                ("berry-catalog@workspace:.", lines(&["resolution: \"berry-catalog@workspace:.\"", "dependencies:", "  a-dep: \"catalog:pinned\"", "  no-deps: \"catalog:\""])),
                ("no-deps@npm:^1.0.0", lines(&["version: 1.0.1", "resolution: \"no-deps@npm:1.0.1\""])),
                ("pkg-a@workspace:packages/pkg-a", lines(&["resolution: \"pkg-a@workspace:packages/pkg-a\"", "dependencies:", "  no-deps: \"catalog:\""])),
            ]),
        ),
    ]);
    let m = run(dir.path()).unwrap();
    assert_eq!(
        root_edit(&m)["workspaces"],
        json!({ "packages": ["packages/*"], "catalog": { "no-deps": "^1.0.0" }, "catalogs": { "pinned": { "a-dep": "~1.0.2" } } })
    );
    let g = &m.graph;
    assert_eq!(
        locked(g),
        [
            "a-dep@1.0.4",
            "no-deps@1.0.1",
            "pkg-a@workspace:packages/pkg-a"
        ]
    );
    assert_eq!(g.catalogs.get("", "no-deps"), Some("^1.0.0"));
    assert_eq!(g.catalogs.get("pinned", "a-dep"), Some("~1.0.2"));
    let a = g.workspaces["packages/pkg-a"].index();
    assert_eq!(bun_id(target(g, a, "no-deps")), "no-deps@1.0.1");
}

fn reject(manifest: Value, lock: String) -> String {
    let dir = project(&[("package.json", manifest.to_string()), ("yarn.lock", lock)]);
    run(dir.path()).unwrap_err().to_string()
}

#[test]
fn pins_fnpm_cannot_keep_fail_the_migration() {
    let root = |deps: &[&str]| {
        let mut v = lines(&["resolution: \"berry-reject@workspace:.\"", "dependencies:"]);
        v.extend(deps.iter().map(|s| (*s).to_owned()));
        v
    };
    let no_deps = (
        "no-deps@npm:^1.0.0",
        lines(&["resolution: \"no-deps@npm:1.0.0\""]),
    );
    let cases: Vec<(Value, String, &str)> = vec![
        (
            json!({ "name": "berry-reject", "dependencies": { "no-deps": "^1.0.0", "a-dep": "^1.0.1" } }),
            yarn_lock(&[("berry-reject@workspace:.", root(&["  no-deps: \"npm:^1.0.0\""])), no_deps.clone()]),
            "yarn.lock missing \"a-dep@^1.0.1\", a dependency of \"berry-reject\"",
        ),
        (
            json!({ "name": "berry-reject", "dependencies": { "no-deps": "^1.0.0" }, "resolutions": { "no-deps": "2.0.0" } }),
            yarn_lock(&[("berry-reject@workspace:.", root(&["  no-deps: \"npm:^1.0.0\""])), no_deps.clone()]),
            "yarn.lock missing \"no-deps@^1.0.0\", a dependency of \"berry-reject\"",
        ),
        (
            json!({ "name": "berry-reject", "dependencies": { "generated": "exec:./gen.js" } }),
            yarn_lock(&[
                ("berry-reject@workspace:.", root(&["  generated: \"exec:./gen.js\""])),
                ("generated@exec:./gen.js::locator=berry-reject%40workspace%3A.", lines(&["resolution: \"generated@exec:./gen.js::locator=berry-reject%40workspace%3A.\""])),
            ]),
            "uses a protocol fnpm does not support",
        ),
        (
            json!({ "name": "berry-reject" }),
            "__metadata:\n  version: 3\n".to_owned(),
            "yarn.lock version 3 is not supported",
        ),
        (
            json!({ "name": "berry-reject" }),
            yarn_lock(&[("no-deps@npm:^1.0.0", lines(&["resolution: \"no-deps@npm:1.0.0\""]))]),
            "yarn.lock has no root workspace entry",
        ),
        (
            json!({ "name": "berry-reject" }),
            "__metadata:\n  version: 8\nbase: &b\n  resolution: \"x@npm:1.0.0\"\n\"x@npm:^1\":\n  <<: *b\n".to_owned(),
            "merge key",
        ),
        (
            json!({ "name": "berry-reject", "dependencies": { "no-deps": "^1.0.0" } }),
            "__metadata:\n  version: 8\n\"berry-reject@workspace:.\":\n  resolution: \"berry-reject@workspace:.\"\n  resolution: \"berry-reject@workspace:.\"\n".to_owned(),
            "more than once in one mapping",
        ),
        (
            json!({ "name": "berry-reject", "dependencies": { "a": "github:o/r" } }),
            yarn_lock(&[
                ("berry-reject@workspace:.", root(&["  a: \"github:o/r\""])),
                ("a@github:o/r", lines(&["resolution: \"a@https://github.com/o/r.git#workspace=a&commit=abc\""])),
            ]),
            "is a package inside a git repository",
        ),
    ];
    for (manifest, lock, expected) in cases {
        let err = reject(manifest, lock);
        assert!(err.contains(expected), "{err} should contain {expected}");
    }
}

#[test]
fn a_registry_yarn_uses_must_be_the_configured_one() {
    let lock = yarn_lock(&[
        (
            "berry-reg@workspace:.",
            lines(&[
                "resolution: \"berry-reg@workspace:.\"",
                "dependencies:",
                "  no-deps: \"npm:^1.0.0\"",
            ]),
        ),
        (
            "no-deps@npm:^1.0.0",
            lines(&["resolution: \"no-deps@npm:1.0.0\""]),
        ),
    ]);
    let manifest =
        json!({ "name": "berry-reg", "dependencies": { "no-deps": "^1.0.0" } }).to_string();
    let dir = project(&[
        ("package.json", manifest.clone()),
        ("yarn.lock", lock.clone()),
        (
            ".yarnrc.yml",
            "npmRegistryServer: \"https://other.example\"\n".into(),
        ),
    ]);
    let err = run(dir.path()).unwrap_err().to_string();
    assert!(err.contains("yarn fetches \"no-deps\" from https://other.example and fnpm is configured to fetch it from"), "{err}");
    // the same registry, with credentials, is fine
    let dir = project(&[
        ("package.json", manifest),
        ("yarn.lock", lock),
        (
            ".yarnrc.yml",
            "npmRegistryServer: \"http://user:pw@localhost:1234\"\n".into(),
        ),
    ]);
    let m = run(dir.path()).unwrap();
    assert!(matches!(
        &target(&m.graph, 0, "no-deps").resolution,
        Resolution::Npm {
            tarball_url: Some(_),
            ..
        }
    ));
}

#[test]
fn git_tarball_and_link_references() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let lock = yarn_lock(&[
        (
            "berry-misc@workspace:.",
            lines(&[
                "resolution: \"berry-misc@workspace:.\"",
                "dependencies:",
                "  gh: \"github:o/r\"",
                "  tb: \"https://example.com/tb-1.0.0.tgz\"",
                "  dotted: \"link:./vendor/dotted\"",
            ]),
        ),
        (
            "gh@github:o/r",
            lines(&[
                "version: 1.0.0",
                &format!("resolution: \"gh@https://github.com/o/r.git#commit={sha}\""),
            ]),
        ),
        (
            "tb@https://example.com/tb-1.0.0.tgz",
            lines(&[
                "version: 1.0.0",
                "resolution: \"tb@https://example.com/tb-1.0.0.tgz\"",
            ]),
        ),
        (
            "dotted@link:./vendor/dotted::locator=berry-misc%40workspace%3A.",
            lines(&[
                "resolution: \"dotted@link:./vendor/dotted::locator=berry-misc%40workspace%3A.\"",
            ]),
        ),
    ]);
    let dir = project(&[
        ("package.json", json!({ "name": "berry-misc", "dependencies": { "gh": "github:o/r", "tb": "https://example.com/tb-1.0.0.tgz", "dotted": "link:./vendor/dotted" } }).to_string()),
        ("vendor/dotted/package.json", json!({ "name": "dotted", "version": "1.0.0" }).to_string()),
        ("yarn.lock", lock),
    ]);
    let m = run(dir.path()).unwrap();
    let g = &m.graph;
    assert_eq!(
        target(g, 0, "gh").resolution,
        Resolution::Git {
            url: "https://github.com/o/r.git".into(),
            committish: sha.into(),
            resolved_commit: sha.into(),
            integrity: None
        }
    );
    assert_eq!(
        target(g, 0, "tb").resolution,
        Resolution::RemoteTarball {
            url: "https://example.com/tb-1.0.0.tgz".into(),
            integrity: None
        }
    );
    assert_eq!(
        target(g, 0, "dotted").resolution,
        Resolution::Symlink {
            path: "./vendor/dotted".into()
        }
    );
}

#[test]
fn builtin_and_rc_package_extensions() {
    let lock = yarn_lock(&[
        (
            "berry-ext@workspace:.",
            lines(&[
                "resolution: \"berry-ext@workspace:.\"",
                "dependencies:",
                "  debug: \"npm:^4.1.0\"",
                "  got: \"npm:^10.0.0\"",
            ]),
        ),
        (
            "debug@npm:^4.1.0",
            lines(&["version: 4.1.1", "resolution: \"debug@npm:4.1.1\""]),
        ),
        (
            "got@npm:^10.0.0",
            lines(&[
                "version: 10.7.0",
                "resolution: \"got@npm:10.7.0\"",
                "dependencies:",
                "  extra: \"npm:^1.0.0\"",
            ]),
        ),
        (
            "@types/keyv@npm:^3.1.1",
            lines(&["version: 3.1.4", "resolution: \"@types/keyv@npm:3.1.4\""]),
        ),
        (
            "extra@npm:^1.0.0",
            lines(&["version: 1.0.0", "resolution: \"extra@npm:1.0.0\""]),
        ),
    ]);
    let dir = project(&[
        ("package.json", json!({ "name": "berry-ext", "dependencies": { "debug": "^4.1.0", "got": "^10.0.0" } }).to_string()),
        (".yarnrc.yml", "packageExtensions:\n  \"debug@*\":\n    peerDependencies:\n      extra: \"npm:^1.0.0\"\n".into()),
        ("yarn.lock", lock),
    ]);
    let m = run(dir.path()).unwrap();
    let g = &m.graph;
    let debug = target(g, 0, "debug");
    // builtin: `debug@<4.2.0` marks supports-color an optional peer
    assert!(
        debug
            .dependency("supports-color")
            .unwrap()
            .behavior
            .is_optional_peer()
    );
    // .yarnrc.yml adds a peer
    let extra = debug.dependency("extra").unwrap();
    assert!(extra.behavior.is_peer() && !extra.behavior.is_optional());
    assert_eq!(extra.literal, "^1.0.0");
    // builtin: `got@<11` adds @types/keyv only when yarn.lock locked it (@types/responselike is not)
    let got = target(g, 0, "got");
    assert_eq!(
        bun_id(target(
            g,
            g.packages.iter().position(|p| p.name == "got").unwrap(),
            "@types/keyv"
        )),
        "@types/keyv@3.1.4"
    );
    assert!(got.dependency("@types/responselike").is_none());
    // the rules are copied to package.json
    let root = root_edit(&m);
    assert_eq!(
        root["packageExtensions"],
        json!({ "debug@*": { "peerDependencies": { "extra": "^1.0.0" } } })
    );
    assert!(g.package_extensions.contains_key("debug@*"));
}
