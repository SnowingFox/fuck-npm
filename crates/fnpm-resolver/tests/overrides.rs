//! overrides / resolutions: parsing, selector precedence and their effect on resolution. Ported
//! from bun's `overrides.test.ts` and `nested-overrides.test.ts` with the same fixture packages
//! (`no-deps`, `one-dep`, `one-range-dep`, `one-fixed-dep`, `a-dep`).

mod common;

use common::*;
use fnpm_resolver::overrides::{find, parse_overrides, parse_overrides_with};
use fnpm_resolver::{Error, Mode};
use fnpm_types::{Package, PackageId, Resolution};
use serde_json::{Value, json};

/// The registry of bun's override tests.
fn fixtures() -> Harness {
    let h = Harness::new();
    h.registry
        .add(
            "no-deps",
            &[
                ("1.0.0", no_deps()),
                ("1.0.1", no_deps()),
                ("1.1.0", no_deps()),
                ("2.0.0", no_deps()),
            ],
        )
        .add(
            "one-dep",
            &[("1.0.0", json!({"dependencies": {"no-deps": "1.0.1"}}))],
        )
        .add(
            "one-range-dep",
            &[("1.0.0", json!({"dependencies": {"no-deps": "^1.0.0"}}))],
        )
        .add(
            "one-fixed-dep",
            &[
                ("1.0.0", json!({"dependencies": {"no-deps": "1.0.0"}})),
                ("2.0.0", json!({"dependencies": {"no-deps": "2.0.0"}})),
            ],
        )
        .add(
            "a-dep",
            &[
                ("1.0.1", no_deps()),
                ("1.0.5", no_deps()),
                ("1.0.10", no_deps()),
            ],
        );
    h
}

/// The version of `child` that the dependency `parent` of the root sees (`.` for the root).
fn seen_by(g: &fnpm_types::LockGraph, parent: &str, child: &str) -> String {
    let from = if parent == "." {
        PackageId::ROOT
    } else {
        g.root()
            .dependency(parent)
            .and_then(|e| e.target)
            .unwrap_or_else(|| panic!("root has no {parent}"))
    };
    dep(g, from, child)
        .map(|p| p.version.clone())
        .unwrap_or_else(|| panic!("{parent} has no {child}"))
}

async fn resolve(root: Value) -> fnpm_types::LockGraph {
    let h = fixtures();
    let ws = workspace(root, &[]);
    h.install(&ws).await.graph
}

#[tokio::test]
async fn overrides_affect_your_own_packages_and_all_dependencies() {
    let g = resolve(json!({"dependencies": {"no-deps": "^2", "one-range-dep": "1.0.0"}, "overrides": {"no-deps": "1.0.0"}})).await;
    assert_eq!(seen_by(&g, ".", "no-deps"), "1.0.0");
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "1.0.0");
    assert_eq!(versions(&g, "no-deps"), ["1.0.0"]);
}

#[tokio::test]
async fn overrides_to_npm_specifier() {
    let g = resolve(json!({"dependencies": {"one-range-dep": "1.0.0"}, "overrides": {"no-deps": "npm:a-dep@1.0.1"}})).await;
    let p = dep(
        &g,
        g.root()
            .dependency("one-range-dep")
            .unwrap()
            .target
            .unwrap(),
        "no-deps",
    )
    .unwrap();
    assert_eq!((p.name.as_str(), p.version.as_str()), ("a-dep", "1.0.1"));
}

#[tokio::test]
async fn npm_object_scopes_the_rule_to_the_parent() {
    let g = resolve(json!({
        "dependencies": {"one-dep": "1.0.0", "one-range-dep": "1.0.0"},
        "overrides": {"one-dep": {"no-deps": "2.0.0"}}
    }))
    .await;
    assert_eq!(seen_by(&g, "one-dep", "no-deps"), "2.0.0");
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "1.1.0");
}

#[tokio::test]
async fn dot_overrides_the_parent_itself() {
    let g = resolve(json!({
        "dependencies": {"one-fixed-dep": "^2.0.0"},
        "overrides": {"one-fixed-dep": {".": "1.0.0", "no-deps": "1.1.0"}}
    }))
    .await;
    assert_eq!(
        dep(&g, PackageId::ROOT, "one-fixed-dep").unwrap().version,
        "1.0.0"
    );
    assert_eq!(seen_by(&g, "one-fixed-dep", "no-deps"), "1.1.0");
}

#[tokio::test]
async fn parent_range_matches_the_parents_resolved_version_through_an_alias() {
    let g = resolve(json!({
        "dependencies": {"ofd1": "npm:one-fixed-dep@1.0.0", "ofd2": "npm:one-fixed-dep@2.0.0"},
        "overrides": {"one-fixed-dep@1": {"no-deps": "1.1.0"}}
    }))
    .await;
    assert_eq!(seen_by(&g, "ofd1", "no-deps"), "1.1.0");
    assert_eq!(seen_by(&g, "ofd2", "no-deps"), "2.0.0");
}

#[tokio::test]
async fn ref_values_resolve_against_the_root() {
    let g = resolve(json!({
        "dependencies": {"no-deps": "1.1.0", "one-dep": "1.0.0"},
        "overrides": {"one-dep": {"no-deps": "$no-deps"}}
    }))
    .await;
    assert_eq!(seen_by(&g, "one-dep", "no-deps"), "1.1.0");
    let (rules, warnings) = parse_overrides(
        &json!({"dependencies": {}, "overrides": {"one-dep": {"no-deps": "$nothing"}}}),
    );
    assert!(rules.is_empty());
    assert_eq!(
        warnings,
        ["Could not resolve \"$nothing\": \"nothing\" is not in dependencies"]
    );
}

#[tokio::test]
async fn ref_values_from_workspaces_must_agree() {
    let a = pj(json!({"name": "a", "dependencies": {"x": "^1"}}));
    let b = pj(json!({"name": "b", "dependencies": {"x": "^2"}}));
    let c = pj(json!({"name": "c", "dependencies": {"x": "^1"}}));
    let root = json!({"overrides": {"x": "$x"}});
    let (rules, _) = parse_overrides_with(&root, &[&a, &c], &[]).unwrap();
    assert_eq!(rules[0].value, "^1");
    let (rules, warnings) = parse_overrides_with(&root, &[&a, &b], &[]).unwrap();
    assert!(rules.is_empty());
    assert_eq!(
        warnings,
        ["Could not resolve \"$x\": workspaces declare different versions of \"x\""]
    );
}

#[tokio::test]
async fn every_selector_spelling() {
    for overrides in [
        json!({"overrides": {"one-dep>no-deps": "2.0.0"}}),
        json!({"overrides": {"one-dep": {"no-deps": "2.0.0"}}}),
        json!({"resolutions": {"one-dep/no-deps": "2.0.0"}}),
        json!({"resolutions": {"**/one-dep/no-deps": "2.0.0"}}),
    ] {
        let mut root = json!({"dependencies": {"one-dep": "1.0.0", "one-range-dep": "1.0.0"}});
        for (k, v) in overrides.as_object().unwrap() {
            root[k] = v.clone();
        }
        let g = resolve(root.clone()).await;
        assert_eq!(seen_by(&g, "one-dep", "no-deps"), "2.0.0", "{root}");
        assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "1.1.0", "{root}");
    }
}

#[tokio::test]
async fn pnpm_parent_range_selectors_including_gt() {
    let g = resolve(json!({
        "dependencies": {"ofd1": "npm:one-fixed-dep@1.0.0", "ofd2": "npm:one-fixed-dep@2.0.0"},
        "overrides": {"one-fixed-dep@>=2 <3>no-deps": "1.0.1"}
    }))
    .await;
    assert_eq!(seen_by(&g, "ofd1", "no-deps"), "1.0.0");
    assert_eq!(seen_by(&g, "ofd2", "no-deps"), "1.0.1");
}

#[tokio::test]
async fn precedence_ranged_parent_then_parent_then_flat() {
    let g = resolve(json!({
        "dependencies": {"ofd1": "npm:one-fixed-dep@1.0.0", "ofd2": "npm:one-fixed-dep@2.0.0", "one-range-dep": "1.0.0"},
        "overrides": {"no-deps": "1.0.0", "one-fixed-dep": {"no-deps": "1.0.1"}, "one-fixed-dep@2": {"no-deps": "1.1.0"}}
    }))
    .await;
    assert_eq!(seen_by(&g, "ofd1", "no-deps"), "1.0.1");
    assert_eq!(seen_by(&g, "ofd2", "no-deps"), "1.1.0");
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "1.0.0");
}

#[tokio::test]
async fn precedence_parent_tiers_then_target_range_then_flat() {
    let g = resolve(json!({
        "dependencies": {"one-range-dep": "1.0.0", "one-dep": "1.0.0", "ofd1": "npm:one-fixed-dep@1.0.0", "ofd2": "npm:one-fixed-dep@2.0.0"},
        "overrides": {
            "no-deps": "1.0.0",
            "no-deps@1": "1.1.0",
            "one-range-dep": {"no-deps": "1.0.1", "no-deps@1": "2.0.0"},
            "one-dep": {"no-deps": "2.0.0"}
        }
    }))
    .await;
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "2.0.0");
    assert_eq!(seen_by(&g, "one-dep", "no-deps"), "2.0.0");
    assert_eq!(seen_by(&g, "ofd1", "no-deps"), "1.1.0");
    assert_eq!(seen_by(&g, "ofd2", "no-deps"), "1.0.0");
}

#[tokio::test]
async fn two_matching_ranged_rules_the_text_that_sorts_first_wins() {
    let g = resolve(json!({"dependencies": {"one-range-dep": "1.0.0"}, "overrides": {"no-deps@>=1": "1.0.0", "no-deps@1": "1.0.1"}})).await;
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "1.0.1");
}

#[tokio::test]
async fn a_non_intersecting_range_leaves_the_edge_alone() {
    let g = resolve(json!({
        "dependencies": {"one-range-dep": "1.0.0", "one-dep": "1.0.0"},
        "overrides": {"no-deps@2": "1.0.0", "no-deps@<1.0.1": "2.0.0"}
    }))
    .await;
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "2.0.0");
    assert_eq!(seen_by(&g, "one-dep", "no-deps"), "1.0.1");
    assert_eq!(g.overrides.len(), 2);
}

#[tokio::test]
async fn edges_declared_with_a_dist_tag_never_match_a_ranged_key() {
    let g = resolve(json!({"dependencies": {"no-deps": "latest", "one-range-dep": "1.0.0"}, "overrides": {"no-deps@>=1": "1.0.0"}})).await;
    assert_eq!(seen_by(&g, ".", "no-deps"), "2.0.0");
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "1.0.0");
}

#[tokio::test]
async fn flat_rules_do_not_apply_to_aliases_but_alias_keys_do() {
    let g = resolve(
        json!({"dependencies": {"nd": "npm:no-deps@^1"}, "overrides": {"no-deps": "2.0.0"}}),
    )
    .await;
    assert_eq!(dep(&g, PackageId::ROOT, "nd").unwrap().version, "1.1.0");
    let g = resolve(json!({"dependencies": {"nd": "npm:no-deps@^1"}, "overrides": {"nd@npm:no-deps@^1": "npm:no-deps@1.0.0"}})).await;
    assert_eq!(dep(&g, PackageId::ROOT, "nd").unwrap().version, "1.0.0");
}

#[tokio::test]
async fn overrides_do_not_apply_to_workspace_edges() {
    let h = fixtures();
    let ws = workspace(
        json!({"name": "root", "dependencies": {"pkg1": "workspace:*"}, "overrides": {"pkg1": "file:pkg2"}}),
        &[("packages/pkg1", json!({"name": "pkg1", "version": "1.1.1"}))],
    );
    let g = h.install(&ws).await.graph;
    assert_eq!(
        root_dep(&g, "pkg1").as_deref(),
        Some("workspace:packages/pkg1")
    );
}

#[tokio::test]
async fn a_top_level_override_may_point_at_a_folder() {
    let h = fixtures();
    h.fetcher.folder(
        "/project/local-no-deps",
        json!({"name": "no-deps", "version": "9.0.0"}),
    );
    let ws = workspace(
        json!({"dependencies": {"one-range-dep": "1.0.0"}, "overrides": {"no-deps": "file:./local-no-deps"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let ord = g
        .root()
        .dependency("one-range-dep")
        .unwrap()
        .target
        .unwrap();
    assert_eq!(
        dep(&g, ord, "no-deps").unwrap().resolution,
        Resolution::Folder {
            path: "local-no-deps".into()
        }
    );
}

#[tokio::test]
async fn overrides_beat_resolutions() {
    let g = resolve(json!({"dependencies": {"one-range-dep": "1.0.0"}, "overrides": {"no-deps": "1.0.0"}, "resolutions": {"no-deps": "2.0.0"}})).await;
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "1.0.0");
    let g = resolve(
        json!({"dependencies": {"one-range-dep": "1.0.0"}, "resolutions": {"no-deps": "1.0.1"}}),
    )
    .await;
    assert_eq!(seen_by(&g, "one-range-dep", "no-deps"), "1.0.1");
}

#[tokio::test]
async fn non_object_overrides_fail_the_install() {
    let h = fixtures();
    let ws = workspace(json!({"overrides": ["no-deps"]}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidPackageJson { message, .. } if message.starts_with("\"overrides\" expects a map")),
        "{err}"
    );
}

#[test]
fn parse_warnings_match_bun() {
    let (rules, warnings) = parse_overrides(&json!({"overrides": {
        "//": "comment",
        "": "1.0.0",
        "a": "-",
        "b": "",
        "c": "patch:c@1#./p.patch",
        "d": 1,
        "a>b>c": "1",
        "e": {"f": {"g": "1"}},
        "@scope": "1",
        "h@": "1",
        "ok": "1.0.0"
    }}));
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].selector.to_key(), "ok");
    assert_eq!(
        warnings,
        [
            "Missing overridden package name",
            "Removing \"a\" with \"-\" is not supported",
            "Missing override value",
            "fnpm currently does not support patched package \"overrides\"",
            "Invalid override value for \"d\"",
            "fnpm currently only supports one level of nested \"overrides\"",
            "fnpm currently only supports one level of nested \"overrides\"",
            "invalid package name \"@scope\"",
            "Missing version range after \"h@\"",
        ]
    );
    let (_, warnings) = parse_overrides(&json!({"resolutions": {"x": {"y": "1"}}}));
    assert_eq!(warnings, ["Invalid resolution value for \"x\""]);
    let (_, warnings) = parse_overrides(&json!({"overrides": {"p@not a range!": {"x": "1"}}}));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[test]
fn the_same_rule_in_two_spellings_keeps_the_last() {
    let (rules, _) = parse_overrides(
        &json!({"overrides": {"one-dep": {"no-deps": "1.0.0"}, "one-dep>no-deps": "2.0.0"}}),
    );
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].value, "2.0.0");
}

#[test]
fn pnpm_workspace_overrides_are_replaced_by_package_json() {
    let pnpm = vec![
        ("a".to_owned(), "1.0.0".to_owned()),
        ("b".to_owned(), "2.0.0".to_owned()),
    ];
    let (rules, _) =
        parse_overrides_with(&json!({"overrides": {"a": "3.0.0"}}), &[], &pnpm).unwrap();
    let flat: Vec<(String, String)> = rules
        .iter()
        .map(|r| (r.selector.to_key(), r.value.clone()))
        .collect();
    assert_eq!(
        flat,
        [
            ("a".to_owned(), "3.0.0".to_owned()),
            ("b".to_owned(), "2.0.0".to_owned())
        ]
    );
}

#[test]
fn find_applies_precedence() {
    let (rules, _) = parse_overrides(&json!({"overrides": {
        "x": "1.0.0",
        "x@^1": "1.1.0",
        "p": {"x": "2.0.0"},
        "p@2": {"x": "3.0.0"}
    }}));
    let p1 = Package::new("p", "1.0.0", Resolution::npm("1.0.0", None));
    let p2 = Package::new("p", "2.0.0", Resolution::npm("2.0.0", None));
    let q = Package::new("q", "1.0.0", Resolution::npm("1.0.0", None));
    assert_eq!(find(&rules, Some(&p2), "x", "^1").unwrap().value, "3.0.0");
    assert_eq!(find(&rules, Some(&p1), "x", "^1").unwrap().value, "2.0.0");
    assert_eq!(find(&rules, Some(&q), "x", "^1.2").unwrap().value, "1.1.0");
    assert_eq!(find(&rules, Some(&q), "x", "^5").unwrap().value, "1.0.0");
    assert_eq!(find(&rules, None, "x", "latest").unwrap().value, "1.0.0");
    assert!(find(&rules, Some(&q), "x", "npm:y@^1").is_none());
    assert!(find(&rules, Some(&q), "y", "^1").is_none());
}

#[test]
fn catalog_keys_only_match_that_catalog() {
    let (rules, _) =
        parse_overrides(&json!({"overrides": {"x@catalog:": "1.0.0", "y@catalog:tools": "2.0.0"}}));
    assert_eq!(find(&rules, None, "x", "catalog:").unwrap().value, "1.0.0");
    assert_eq!(
        find(&rules, None, "x", "catalog:default").unwrap().value,
        "1.0.0"
    );
    assert!(find(&rules, None, "x", "^1").is_none());
    assert!(find(&rules, None, "y", "catalog:other").is_none());
    assert_eq!(
        find(&rules, None, "y", "catalog:tools").unwrap().value,
        "2.0.0"
    );
}

#[test]
fn workspace_packages_can_be_parents_by_version() {
    let (rules, _) = parse_overrides(&json!({"overrides": {"app@^2": {"x": "9.0.0"}}}));
    let mut app = Package::new(
        "app",
        "2.1.0",
        Resolution::Workspace {
            path: "packages/app".into(),
        },
    );
    assert_eq!(find(&rules, Some(&app), "x", "^1").unwrap().value, "9.0.0");
    app.version = "1.0.0".into();
    assert!(find(&rules, Some(&app), "x", "^1").is_none());
}
