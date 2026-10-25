//! catalogs, packageExtensions, patchedDependencies, trustedDependencies and
//! minimumReleaseAge. Ported from `catalogs.test.ts`, `bun-install-package-extensions.test.ts`,
//! `minimum-release-age.test.ts` and the patch / trust cases of `bun-install.test.ts`.

mod common;

use common::*;
use fnpm_resolver::options::{
    ReleaseAgeExcludes, parse_minimum_release_age, parse_seconds, platform_from_cli,
    split_minimum_release_age_excludes,
};
use fnpm_resolver::pick::{self, AgeGate, PickError, Picked, Wanted};
use fnpm_resolver::{Error, Mode};
use fnpm_semver::Range;
use fnpm_types::{CpuSet, OsSet, PackageId, Platform};
use serde_json::json;

// ---- catalogs --------------------------------------------------------------------------------

fn catalog_registry(h: &Harness) {
    h.registry
        .add("no-deps", &[("1.0.0", no_deps()), ("2.0.0", no_deps())])
        .add("a-dep", &[("1.0.1", no_deps()), ("1.0.2", no_deps())])
        .add("with-catalog-dep", &[("1.0.0", json!({"dependencies": {"no-deps": "catalog:"}, "peerDependencies": {"a-dep": "catalog:"}}))]);
}

#[tokio::test]
async fn catalogs_from_workspaces_object_and_top_level() {
    let h = Harness::new();
    catalog_registry(&h);
    let ws = workspace(
        json!({"workspaces": {"packages": ["packages/*"], "catalog": {"no-deps": "2.0.0"}, "catalogs": {"old": {"no-deps": "1.0.0"}}}}),
        &[
            (
                "packages/a",
                json!({"name": "a", "dependencies": {"no-deps": "catalog:"}}),
            ),
            (
                "packages/b",
                json!({"name": "b", "dependencies": {"no-deps": "catalog:old"}}),
            ),
            (
                "packages/c",
                json!({"name": "c", "dependencies": {"no-deps": "catalog:default"}}),
            ),
        ],
    );
    let g = h.install(&ws).await.graph;
    assert_eq!(
        dep(&g, g.workspaces["packages/a"], "no-deps")
            .unwrap()
            .version,
        "2.0.0"
    );
    assert_eq!(
        dep(&g, g.workspaces["packages/b"], "no-deps")
            .unwrap()
            .version,
        "1.0.0"
    );
    assert_eq!(
        dep(&g, g.workspaces["packages/c"], "no-deps")
            .unwrap()
            .version,
        "2.0.0"
    );
    assert_eq!(g.catalogs.default["no-deps"], "2.0.0");
    // The edge keeps its declared `catalog:` text.
    assert_eq!(
        g.packages[g.workspaces["packages/a"].index()]
            .dependency("no-deps")
            .unwrap()
            .literal,
        "catalog:"
    );

    let ws = workspace(
        json!({"workspaces": ["packages/*"], "catalog": {"no-deps": "1.0.0"}, "dependencies": {"no-deps": "catalog:"}}),
        &[],
    );
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "no-deps").as_deref(),
        Some("1.0.0")
    );
}

#[tokio::test]
async fn catalogs_need_workspaces_and_entries() {
    let h = Harness::new();
    catalog_registry(&h);
    // Without "workspaces" the root's catalogs are not read.
    let ws = workspace(
        json!({"catalog": {"no-deps": "1.0.0"}, "dependencies": {"no-deps": "catalog:"}}),
        &[],
    );
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(
        matches!(&err, Error::CatalogEntryMissing { catalog, name } if catalog.is_empty() && name == "no-deps"),
        "{err}"
    );
    let ws = workspace(
        json!({"workspaces": [], "catalogs": {"tools": {}}, "dependencies": {"x": "catalog:tools"}}),
        &[],
    );
    assert!(matches!(
        h.run(&ws, None, Mode::Normal).await,
        Err(Error::CatalogEntryMissing { .. })
    ));
}

#[tokio::test]
async fn catalog_references_outside_the_project_are_unresolved() {
    let h = Harness::new();
    catalog_registry(&h);
    let ws = workspace(
        json!({"workspaces": [], "catalog": {"no-deps": "1.0.0"}, "dependencies": {"with-catalog-dep": "1.0.0"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let w = &g.packages[id_of(&g, "with-catalog-dep", "1.0.0").index()];
    // A catalog: dependency of a published package is left unresolved; a catalog: peer becomes
    // an optional `*` peer.
    assert!(w.dependency("no-deps").unwrap().target.is_none());
    let peer_edge = w.dependencies.iter().find(|e| e.name == "a-dep").unwrap();
    assert!(peer_edge.behavior.is_optional_peer());
    assert_eq!(keys(&g), ["with-catalog-dep@1.0.0"]);
}

#[tokio::test]
async fn catalog_defined_twice_is_an_error() {
    let h = Harness::new();
    let ws = workspace(
        json!({"workspaces": {"packages": [], "catalog": {"x": "1"}, "catalogs": {"default": {"x": "2"}}}}),
        &[],
    );
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("\"x\" is defined in both \"catalog\" and \"catalogs.default\""),
        "{err}"
    );
}

#[tokio::test]
async fn overrides_can_point_at_catalogs() {
    let h = Harness::new();
    catalog_registry(&h);
    h.registry.add(
        "one-range-dep",
        &[("1.0.0", json!({"dependencies": {"no-deps": "^2.0.0"}}))],
    );
    let ws = workspace(
        json!({"workspaces": {"packages": [], "catalog": {"no-deps": "1.0.0"}}, "dependencies": {"one-range-dep": "1.0.0"}, "overrides": {"no-deps": "catalog:"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    assert_eq!(versions(&g, "no-deps"), ["1.0.0"]);
}

// ---- packageExtensions ----------------------------------------------------------------------

#[tokio::test]
async fn package_extensions_add_dependencies_and_peers() {
    let h = Harness::new();
    h.registry
        .add("host", &[("1.0.0", no_deps()), ("2.0.0", no_deps())])
        .add("extra", &[("1.0.0", no_deps())])
        .add("opt", &[("1.0.0", no_deps())])
        .add("peer", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({
            "dependencies": {"host": "^1"},
            "packageExtensions": {
                "host@^1": {"dependencies": {"extra": "^1"}, "optionalDependencies": {"opt": "^1"}, "peerDependencies": {"peer": "^1"}},
                "host@^2": {"dependencies": {"never": "^1"}}
            }
        }),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let host = id_of(&g, "host", "1.0.0");
    assert_eq!(dep(&g, host, "extra").unwrap().version, "1.0.0");
    assert_eq!(dep(&g, host, "opt").unwrap().version, "1.0.0");
    assert_eq!(peer(&g, host, "peer").unwrap().version, "1.0.0");
    assert_eq!(g.package_extensions.len(), 2);
}

#[tokio::test]
async fn package_extensions_own_rows_win_and_peers_can_be_made_optional() {
    let h = Harness::new();
    h.registry
        .add(
            "host",
            &[(
                "1.0.0",
                json!({"dependencies": {"own": "^1"}, "peerDependencies": {"p": "^1"}}),
            )],
        )
        .add("own", &[("1.0.0", no_deps()), ("2.0.0", no_deps())])
        .add("p", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({
            "dependencies": {"host": "1.0.0"},
            "packageExtensions": {"host": {"dependencies": {"own": "^2"}, "peerDependenciesMeta": {"p": {"optional": true}}}}
        }),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let host = id_of(&g, "host", "1.0.0");
    assert_eq!(dep(&g, host, "own").unwrap().version, "1.0.0");
    assert!(
        g.packages[host.index()]
            .dependency("p")
            .unwrap()
            .behavior
            .is_optional_peer()
    );
    assert!(!keys(&g).contains(&"p@1.0.0".to_owned()));
}

#[tokio::test]
async fn invalid_package_extensions_are_ignored_with_warnings() {
    let h = Harness::new();
    h.registry.add("host", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({
            "dependencies": {"host": "1.0.0"},
            "packageExtensions": {
                "host": {"dependencies": {"x": "file:./x"}},
                "@scope": {},
                "host@": {},
                "bad": "nope"
            }
        }),
        &[],
    );
    let out = h.install(&ws).await;
    assert_eq!(keys(&out.graph), ["host@1.0.0"]);
    let ignored: Vec<_> = out
        .warnings
        .iter()
        .filter(|w| w.starts_with("Ignored"))
        .collect();
    assert_eq!(ignored.len(), 4, "{:?}", out.warnings);
    assert!(out.warnings.iter().any(|w| w == "Ignored \"host\" in \"packageExtensions\": expected a version, a tag, a git repository or a tarball URL for every dependency"));
    assert!(
        out.warnings
            .iter()
            .any(|w| w == "Ignored \"bad\" in \"packageExtensions\": expected an object")
    );
}

// ---- patchedDependencies / trustedDependencies ----------------------------------------------

#[tokio::test]
async fn patched_dependencies_are_recorded_with_hashes_and_pinned() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("patches")).unwrap();
    std::fs::write(
        dir.path().join("patches/x@1.0.0.patch"),
        "diff --git a/x b/x\n",
    )
    .unwrap();
    let h = Harness::new();
    h.registry
        .add("x", &[("1.0.0", no_deps()), ("1.2.0", no_deps())]);
    let ws = workspace_at(
        dir.path().to_path_buf(),
        json!({"dependencies": {"x": "^1"}, "patchedDependencies": {"x@1.0.0": "patches/x@1.0.0.patch"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    // A patched version stays while the range allows it.
    assert_eq!(root_dep(&g, "x").as_deref(), Some("1.0.0"));
    let patch = &g.patched_dependencies["x@1.0.0"];
    assert_eq!(patch.path, "patches/x@1.0.0.patch");
    assert_eq!(patch.hash.as_ref().map(String::len), Some(64));
}

#[tokio::test]
async fn unchanged_installs_hash_patch_files_as_they_are_now() {
    // fnpm.lock stores only the patch path: the no-op and frozen paths must still hand the
    // linker the hash of the current file, and fail when it is gone.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("patches")).unwrap();
    let file = dir.path().join("patches/x@1.0.0.patch");
    std::fs::write(&file, "one\n").unwrap();
    let h = Harness::new();
    h.registry.add("x", &[("1.0.0", no_deps())]);
    let ws = workspace_at(
        dir.path().to_path_buf(),
        json!({"dependencies": {"x": "1.0.0"}, "patchedDependencies": {"x@1.0.0": "patches/x@1.0.0.patch"}}),
        &[],
    );
    let first = h.install(&ws).await.graph;
    let first_hash = first.patched_dependencies["x@1.0.0"].hash.clone().unwrap();
    let mut locked = first.clone();
    locked.patched_dependencies.get_mut("x@1.0.0").unwrap().hash = None;
    let out = h.run(&ws, Some(&locked), Mode::Normal).await.unwrap();
    assert!(!out.changed);
    assert_eq!(
        out.graph.patched_dependencies["x@1.0.0"].hash.as_deref(),
        Some(first_hash.as_str())
    );
    std::fs::write(&file, "two\n").unwrap();
    for mode in [Mode::Normal, Mode::Frozen] {
        let out = h.run(&ws, Some(&locked), mode).await.unwrap();
        let hash = out.graph.patched_dependencies["x@1.0.0"]
            .hash
            .clone()
            .unwrap();
        assert_ne!(hash, first_hash);
        assert_eq!(hash.len(), 64);
    }
    std::fs::remove_file(&file).unwrap();
    for mode in [Mode::Normal, Mode::Frozen] {
        assert!(matches!(
            h.run(&ws, Some(&locked), mode).await,
            Err(Error::PatchFile { key, .. }) if key == "x@1.0.0"
        ));
    }
}

#[tokio::test]
async fn a_missing_patch_file_fails_before_linking() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let ws = workspace_at(
        dir.path().to_path_buf(),
        json!({"patchedDependencies": {"x@1.0.0": "patches/missing.patch"}}),
        &[],
    );
    assert!(
        matches!(h.run(&ws, None, Mode::Normal).await, Err(Error::PatchFile { key, .. }) if key == "x@1.0.0")
    );
}

#[tokio::test]
async fn trusted_dependencies_pass_through() {
    let h = Harness::new();
    let ws = workspace(
        json!({"trustedDependencies": ["esbuild"]}),
        &[(
            "packages/w",
            json!({"name": "w", "trustedDependencies": ["sharp"]}),
        )],
    );
    let g = h.install(&ws).await.graph;
    let t: Vec<_> = g.trusted_dependencies.unwrap().into_iter().collect();
    assert_eq!(t, ["esbuild", "sharp"]);
    let ws = workspace(json!({"trustedDependencies": []}), &[]);
    assert_eq!(
        h.install(&ws).await.graph.trusted_dependencies,
        Some(Default::default())
    );
    let ws = workspace(json!({}), &[]);
    assert_eq!(h.install(&ws).await.graph.trusted_dependencies, None);
    let ws = workspace(json!({"trustedDependencies": "esbuild"}), &[]);
    assert!(matches!(
        h.run(&ws, None, Mode::Normal).await,
        Err(Error::InvalidPackageJson { .. })
    ));
}

// ---- minimumReleaseAge ----------------------------------------------------------------------

const DAY: u64 = 24 * 60 * 60 * 1000;

#[test]
fn parse_minimum_release_age_values() {
    assert_eq!(parse_seconds("3600"), Some(3600.0));
    assert_eq!(parse_seconds("90m"), Some(5400.0));
    assert_eq!(parse_seconds("3d"), Some(259_200.0));
    assert_eq!(parse_seconds("2 weeks"), Some(1_209_600.0));
    assert_eq!(parse_seconds("1.5h"), Some(5400.0));
    assert_eq!(parse_seconds("-1"), None);
    assert_eq!(parse_seconds("soon"), None);
    assert_eq!(parse_seconds("d"), None);
    assert_eq!(parse_minimum_release_age("0").unwrap(), None);
    assert_eq!(parse_minimum_release_age("1d").unwrap(), Some(DAY));
    let err = parse_minimum_release_age("x").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Expected --minimum-release-age to be a positive number: x"
    );
    assert_eq!(
        split_minimum_release_age_excludes(&["a, b", " c ,,", "@s/d"]),
        ["a", "b", "c", "@s/d"]
    );
}

#[test]
fn release_age_excludes_grammar() {
    let (ex, warnings) = ReleaseAgeExcludes::parse(&[
        "left-pad",
        "@types/node@20.0.0 || 20.1.0",
        "bad@^1",
        "worse@",
    ]);
    assert!(ex.excludes_package("left-pad"));
    assert!(!ex.excludes_package("@types/node"));
    assert!(ex.excludes_version("@types/node", &v("20.1.0")));
    assert!(!ex.excludes_version("@types/node", &v("20.2.0")));
    assert!(!ex.excludes_package("bad") && !ex.excludes_version("bad", &v("1.0.0")));
    assert_eq!(warnings.len(), 2);
    assert!(
        warnings[0].starts_with("ignoring minimumReleaseAgeExcludes entry \"bad@^1\""),
        "{warnings:?}"
    );
}

#[test]
fn cpu_os_libc_overrides() {
    let base = Platform::current();
    let p = platform_from_cli(base, &["linux"], &["arm64", "x64"], &["musl"]).unwrap();
    assert_eq!(p.os, OsSet::from_list(&["linux"]));
    assert_eq!(p.cpu, CpuSet::from_list(&["arm64", "x64"]));
    let p = platform_from_cli(base, &["*"], &[] as &[&str], &[] as &[&str]).unwrap();
    assert_eq!(p.os, OsSet::ALL);
    assert_eq!(p.cpu, base.cpu);
    let p = platform_from_cli(base, &["!win32"], &["none"], &[] as &[&str]).unwrap();
    assert!(!p.os.names().any(|n| n == "win32"));
    assert_eq!(p.cpu, CpuSet::NONE);
    let err = platform_from_cli(base, &["beos"], &[] as &[&str], &[] as &[&str]).unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid os: 'beos'. valid values: *, any, none,"),
        "{err}"
    );
}

fn aged_manifest(now: u64) -> fnpm_registry::PackageManifest {
    // 1.0.0 is old, 1.1.0 is 30 days old, 1.2.0 two days old, 1.3.0 an hour old.
    packument(
        "aged",
        &[
            ("1.0.0", no_deps()),
            ("1.1.0", no_deps()),
            ("1.2.0", no_deps()),
            ("1.3.0", no_deps()),
        ],
        &[],
        &[
            ("1.0.0", now - 400 * DAY),
            ("1.1.0", now - 30 * DAY),
            ("1.2.0", now - 2 * DAY),
            ("1.3.0", now - 3_600_000),
        ],
    )
}

#[test]
fn release_age_gate_picks_older_stable_versions() {
    let now = 1_000 * DAY;
    let m = aged_manifest(now);
    let ex = ReleaseAgeExcludes::default();
    let gate = AgeGate {
        min_age_ms: 3 * DAY,
        now_ms: now,
        excludes: &ex,
    };
    let range = Range::parse("^1").unwrap();
    let picked = pick::find_for_dependency(
        &m,
        Wanted::Range {
            range: &range,
            literal: "^1",
        },
        Some(&gate),
    );
    // 1.3.0 and 1.2.0 are too recent; 1.1.0 was stable for 28 days before 1.2.0: picked.
    assert!(
        matches!(picked, Picked::FoundWithFilter { result, newest_filtered: Some(n) } if result.version == v("1.1.0") && *n == v("1.3.0"))
    );
    assert!(picked.latest_is_filtered());
    // The tag path walks back the same way.
    let picked = pick::find_for_dependency(
        &m,
        Wanted::Tag {
            tag: "latest",
            literal: "latest",
        },
        Some(&gate),
    );
    assert_eq!(picked.ok().unwrap().version, v("1.1.0"));
    // An exact version that is too recent.
    let exact = Range::parse("1.3.0").unwrap();
    assert!(matches!(
        pick::find_for_dependency(
            &m,
            Wanted::Range {
                range: &exact,
                literal: "1.3.0"
            },
            Some(&gate)
        ),
        Picked::Err(PickError::TooRecent)
    ));
    // Everything too recent.
    let gate = AgeGate {
        min_age_ms: 1000 * DAY,
        now_ms: now,
        excludes: &ex,
    };
    assert!(matches!(
        pick::find_for_dependency(
            &m,
            Wanted::Range {
                range: &range,
                literal: "^1"
            },
            Some(&gate)
        ),
        Picked::Err(PickError::AllVersionsTooRecent)
    ));
}

#[test]
fn stability_window_rejects_versions_followed_by_a_quick_fix() {
    let now = 1_000 * DAY;
    let ex = ReleaseAgeExcludes::default();
    let range = Range::parse("^1").unwrap();
    let hour = 3_600_000;
    // Gate 2 days. 1.2.0 (1 day old) is blocked; 1.1.0 passes the gate but was replaced 36 hours
    // later (< the 2-day window), so the walk goes on; 1.0.0 is 84 hours older than 1.1.0: stable.
    let m = packument(
        "q",
        &[
            ("1.0.0", no_deps()),
            ("1.1.0", no_deps()),
            ("1.2.0", no_deps()),
        ],
        &[],
        &[
            ("1.0.0", now - 6 * DAY),
            ("1.1.0", now - 60 * hour),
            ("1.2.0", now - DAY),
        ],
    );
    let gate = AgeGate {
        min_age_ms: 2 * DAY,
        now_ms: now,
        excludes: &ex,
    };
    let picked = pick::find_for_dependency(
        &m,
        Wanted::Range {
            range: &range,
            literal: "^1",
        },
        Some(&gate),
    );
    assert_eq!(picked.ok().unwrap().version, v("1.0.0"));
    // bun only walks back seven days past the gate: a version older than that ends the walk and
    // the newest candidate that passed the gate is kept, unstable or not.
    let m = packument(
        "q",
        &[
            ("1.0.0", no_deps()),
            ("1.1.0", no_deps()),
            ("1.2.0", no_deps()),
        ],
        &[],
        &[
            ("1.0.0", now - 100 * DAY),
            ("1.1.0", now - 5 * DAY),
            ("1.2.0", now - 4 * DAY),
        ],
    );
    let gate = AgeGate {
        min_age_ms: 5 * DAY - 1,
        now_ms: now,
        excludes: &ex,
    };
    let picked = pick::find_for_dependency(
        &m,
        Wanted::Range {
            range: &range,
            literal: "^1",
        },
        Some(&gate),
    );
    assert_eq!(picked.ok().unwrap().version, v("1.1.0"));
}

#[test]
fn excluded_versions_and_packages_skip_the_gate() {
    let now = 1_000 * DAY;
    let m = aged_manifest(now);
    let range = Range::parse("^1").unwrap();
    let (ex, _) = ReleaseAgeExcludes::parse(&["aged@1.2.0"]);
    let gate = AgeGate {
        min_age_ms: 3 * DAY,
        now_ms: now,
        excludes: &ex,
    };
    assert_eq!(
        pick::find_for_dependency(
            &m,
            Wanted::Range {
                range: &range,
                literal: "^1"
            },
            Some(&gate)
        )
        .ok()
        .unwrap()
        .version,
        v("1.2.0")
    );
    let (ex, _) = ReleaseAgeExcludes::parse(&["aged"]);
    let gate = AgeGate {
        min_age_ms: 3 * DAY,
        now_ms: now,
        excludes: &ex,
    };
    assert!(
        matches!(pick::find_for_dependency(&m, Wanted::Range { range: &range, literal: "^1" }, Some(&gate)), Picked::Found(v) if v.version.to_string() == "1.3.0")
    );
}

#[tokio::test]
async fn release_age_errors_and_peers() {
    let now = fnpm_resolver_now();
    let mut h = Harness::new();
    h.config.minimum_release_age_ms = Some(10 * DAY);
    h.registry.add_full(packument(
        "fresh",
        &[("1.0.0", no_deps())],
        &[],
        &[("1.0.0", now - DAY)],
    ));
    h.registry.add_full(packument(
        "needs",
        &[("1.0.0", json!({"peerDependencies": {"fresh": "^1"}}))],
        &[],
        &[("1.0.0", now - 100 * DAY)],
    ));
    let ws = workspace(json!({"dependencies": {"fresh": "^1"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "No version matching \"^1\" found for specifier \"fresh\" (blocked by minimum-release-age: 864000 seconds)"
    );
    let ws = workspace(json!({"dependencies": {"fresh": "latest"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "Package \"fresh\" with tag \"latest\" not found (all versions blocked by minimum-release-age: 864000 seconds)"
    );
    let ws = workspace(json!({"dependencies": {"needs": "1.0.0"}}), &[]);
    let out = h.install(&ws).await;
    assert!(out.warnings.iter().any(|w| w == "No version matching \"^1\" found for peer dependency \"fresh\" (blocked by minimum-release-age: 864000 seconds)"), "{:?}", out.warnings);
    // The gate requests full manifests.
    assert!(h.registry.log.lock().iter().all(|(_, r)| r.full));
}

#[tokio::test]
async fn locked_versions_are_kept_through_the_gate() {
    let now = fnpm_resolver_now();
    let h = Harness::new();
    h.registry.add_full(packument(
        "fresh",
        &[("1.0.0", no_deps())],
        &[],
        &[("1.0.0", now - DAY)],
    ));
    let ws = workspace(json!({"dependencies": {"fresh": "^1"}}), &[]);
    let first = h.install(&ws).await.graph;
    let mut h2 = Harness::new();
    h2.config.minimum_release_age_ms = Some(10 * DAY);
    h2.registry.add_full(packument(
        "fresh",
        &[("1.0.0", no_deps())],
        &[],
        &[("1.0.0", now - DAY)],
    ));
    let ws = workspace(
        json!({"dependencies": {"fresh": "^1"}, "devDependencies": {}}),
        &[],
    );
    let out = h2
        .run(
            &ws,
            Some(&first),
            Mode::Update {
                names: vec![],
                latest: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(root_dep(&out.graph, "fresh").as_deref(), Some("1.0.0"));
    let _ = PackageId::ROOT;
}

fn fnpm_resolver_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
