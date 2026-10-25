//! Core resolution: npm ranges, tags, aliases, dedupe, workspaces, local and remote protocols,
//! optional and platform handling, streaming, and look-ahead. Ported from the resolution
//! assertions of bun's `bun-install.test.ts`, `bun-workspaces.test.ts`,
//! `bun-install-link-path.test.ts` and `bun-install-cpu-os.test.ts`.

mod common;

use common::*;
use fnpm_resolver::{Error, Mode, Resolved};
use fnpm_types::{Behavior, DependencyGroup, PackageId, Resolution};
use serde_json::json;

#[tokio::test]
async fn resolves_a_range_to_the_highest_match_and_latest() {
    let h = Harness::new();
    h.registry.add(
        "bar",
        &[
            ("0.0.1", no_deps()),
            ("0.0.2", no_deps()),
            ("0.1.0", no_deps()),
        ],
    );
    h.registry.add(
        "baz",
        &[
            ("1.0.0", no_deps()),
            ("1.2.0", no_deps()),
            ("2.0.0", no_deps()),
        ],
    );
    let ws = workspace(
        json!({"name": "foo", "dependencies": {"bar": "~0.0.1", "baz": "^1"}}),
        &[],
    );
    let out = h.install(&ws).await;
    assert_eq!(keys(&out.graph), ["bar@0.0.2", "baz@1.2.0"]);
    assert!(out.changed);
    out.graph.validate().unwrap();
}

#[tokio::test]
async fn latest_tag_wins_when_it_satisfies_the_range() {
    let h = Harness::new();
    h.registry.add_full(packument(
        "a",
        &[("1.0.0", no_deps()), ("1.5.0", no_deps())],
        &[("latest", "1.0.0")],
        &[],
    ));
    let ws = workspace(json!({"dependencies": {"a": "^1.0.0"}}), &[]);
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "a").as_deref(),
        Some("1.0.0")
    );
}

#[tokio::test]
async fn latest_tag_wins_over_newer_matches_for_caret_and_tilde() {
    let h = Harness::new();
    h.registry.add_full(packument(
        "a",
        &[
            ("1.0.0", no_deps()),
            ("1.2.5", no_deps()),
            ("1.3.0", no_deps()),
            ("1.3.1", no_deps()),
        ],
        &[("latest", "1.3.0")],
        &[],
    ));
    h.registry.add_full(packument(
        "b",
        &[
            ("1.2.1", no_deps()),
            ("1.2.3", no_deps()),
            ("2.0.0", no_deps()),
        ],
        &[("latest", "1.2.1")],
        &[],
    ));
    h.registry.add_full(packument(
        "c",
        &[
            ("1.0.0", no_deps()),
            ("1.4.0", no_deps()),
            ("1.9.0", no_deps()),
        ],
        &[("latest", "1.4.0")],
        &[],
    ));
    // The derived upper bounds (`<2.0.0-0`, `<1.3.0-0`) are not a written prerelease, so like
    // bun (`find_best_version`) and npm-pick-manifest, a satisfying `latest` is preferred.
    let ws = workspace(
        json!({"dependencies": {"a": "^1.2.5", "b": "~1.2.0", "c": "1.x"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    assert_eq!(root_dep(&g, "a").as_deref(), Some("1.3.0"));
    assert_eq!(root_dep(&g, "b").as_deref(), Some("1.2.1"));
    assert_eq!(root_dep(&g, "c").as_deref(), Some("1.4.0"));
}

#[tokio::test]
async fn dist_tags_empty_and_star() {
    let h = Harness::new();
    h.registry.add_full(packument(
        "t",
        &[
            ("1.0.0", no_deps()),
            ("2.0.0-beta.1", no_deps()),
            ("1.1.0", json!({"deprecated": "nope"})),
        ],
        &[("latest", "1.1.0"), ("beta", "2.0.0-beta.1")],
        &[],
    ));
    // `beta` follows the tag; `""` is latest but skips a deprecated latest; `*` picks latest
    // unless deprecated; an explicit `latest` follows the tag even when deprecated.
    let ws = workspace(json!({"dependencies": {"t": "beta"}}), &[]);
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "t").as_deref(),
        Some("2.0.0-beta.1")
    );
    let ws = workspace(json!({"dependencies": {"t": ""}}), &[]);
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "t").as_deref(),
        Some("1.0.0")
    );
    let ws = workspace(json!({"dependencies": {"t": "*"}}), &[]);
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "t").as_deref(),
        Some("1.0.0")
    );
    let ws = workspace(json!({"dependencies": {"t": "latest"}}), &[]);
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "t").as_deref(),
        Some("1.1.0")
    );
}

#[tokio::test]
async fn deprecated_versions_are_picked_only_when_nothing_else_matches() {
    let h = Harness::new();
    h.registry.add_full(packument(
        "d",
        &[
            ("1.0.0", no_deps()),
            ("1.1.0", json!({"deprecated": "bad"})),
            ("2.0.0", json!({"deprecated": "bad"})),
        ],
        &[("latest", "1.0.0")],
        &[],
    ));
    let ws = workspace(
        json!({"dependencies": {"d": "^1.1.0", "e": "npm:d@^2"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    assert_eq!(root_dep(&g, "d").as_deref(), Some("1.1.0"));
    assert_eq!(dep(&g, PackageId::ROOT, "e").unwrap().version, "2.0.0");
    assert_eq!(
        dep(&g, PackageId::ROOT, "d")
            .unwrap()
            .meta
            .deprecated
            .as_deref(),
        Some("bad")
    );
}

#[tokio::test]
async fn prerelease_ranges() {
    let h = Harness::new();
    h.registry.add(
        "p",
        &[
            ("1.0.0", no_deps()),
            ("1.1.0-beta.1", no_deps()),
            ("1.1.0-beta.2", no_deps()),
        ],
    );
    let ws = workspace(json!({"dependencies": {"p": "^1.1.0-beta.1"}}), &[]);
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "p").as_deref(),
        Some("1.1.0-beta.2")
    );
    let ws = workspace(json!({"dependencies": {"p": "^1.0.0"}}), &[]);
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "p").as_deref(),
        Some("1.0.0")
    );
}

#[tokio::test]
async fn no_matching_version_and_missing_tags_are_errors() {
    let h = Harness::new();
    h.registry.add("n", &[("1.0.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"n": "^2"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "No version matching \"^2\" found for specifier \"n\" (but package exists)"
    );
    let ws = workspace(json!({"dependencies": {"n": "nightly"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "Package \"n\" with tag \"nightly\" not found, but package exists"
    );
    let ws = workspace(json!({"dependencies": {"missing": "^1"}}), &[]);
    assert!(
        matches!(h.run(&ws, None, Mode::Normal).await.unwrap_err(), Error::NotFound(n) if n == "missing")
    );
}

#[tokio::test]
async fn transitive_dependencies_share_versions() {
    let h = Harness::new();
    h.registry
        .add("a", &[("1.0.0", json!({"dependencies": {"c": "^1.0.0"}}))])
        .add("b", &[("1.0.0", json!({"dependencies": {"c": "^1.1.0"}}))])
        .add(
            "c",
            &[
                ("1.0.0", no_deps()),
                ("1.1.0", no_deps()),
                ("1.2.0", no_deps()),
            ],
        );
    let ws = workspace(json!({"dependencies": {"a": "1.0.0", "b": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    assert_eq!(versions(&g, "c"), ["1.2.0"]);
    let a = id_of(&g, "a", "1.0.0");
    let b = id_of(&g, "b", "1.0.0");
    assert_eq!(dep(&g, a, "c").unwrap().version, "1.2.0");
    assert_eq!(dep(&g, b, "c").unwrap().version, "1.2.0");
}

#[tokio::test]
async fn an_exact_pin_is_shared_by_ranges_that_accept_it() {
    let h = Harness::new();
    h.registry
        .add("a", &[("1.0.0", json!({"dependencies": {"c": "^1.0.0"}}))])
        .add("c", &[("1.0.0", no_deps()), ("1.5.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"c": "1.0.0", "a": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    // `c@1.0.0` is pinned by the root, so `a`'s `^1.0.0` shares it (bun's exact_pinned rule).
    assert_eq!(versions(&g, "c"), ["1.0.0"]);
}

#[tokio::test]
async fn conflicting_ranges_keep_two_versions() {
    let h = Harness::new();
    h.registry
        .add("a", &[("1.0.0", json!({"dependencies": {"c": "^2"}}))])
        .add("c", &[("1.0.0", no_deps()), ("2.0.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"c": "^1", "a": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    assert_eq!(versions(&g, "c"), ["1.0.0", "2.0.0"]);
}

#[tokio::test]
async fn npm_aliases_keep_the_alias_as_edge_name() {
    let h = Harness::new();
    h.registry
        .add("real", &[("1.0.0", no_deps()), ("2.0.0", no_deps())])
        .add("@s/x", &[("3.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"alias": "npm:real@^1", "real": "^2", "scoped": "npm:@s/x", "self": "npm:^2"}}),
        &[],
    );
    let g = h.run(&ws, None, Mode::Normal).await;
    // `npm:^2` is a range for the dependency's own name, which does not exist.
    assert!(matches!(g, Err(Error::NotFound(n)) if n == "self"));
    let ws = workspace(
        json!({"dependencies": {"alias": "npm:real@^1", "real": "^2", "scoped": "npm:@s/x"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let alias = dep(&g, PackageId::ROOT, "alias").unwrap();
    assert_eq!(
        (alias.name.as_str(), alias.version.as_str()),
        ("real", "1.0.0")
    );
    assert_eq!(root_dep(&g, "real").as_deref(), Some("2.0.0"));
    assert_eq!(dep(&g, PackageId::ROOT, "scoped").unwrap().name, "@s/x");
}

#[tokio::test]
async fn invalid_names_are_rejected() {
    let h = Harness::new();
    let ws = workspace(json!({"dependencies": {"../evil": "^1"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(err.to_string().contains("invalid dependency name"), "{err}");
    let ws = workspace(json!({"dependencies": {"x": "npm:@1.0.0"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(err.to_string().contains("invalid package name"), "{err}");
}

#[tokio::test]
async fn dev_dependencies_only_for_local_packages() {
    let h = Harness::new();
    h.registry
        .add("a", &[("1.0.0", json!({"dependencies": {"b": "1"}}))])
        .add("b", &[("1.0.0", no_deps())])
        .add("dev", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"a": "1.0.0"}, "devDependencies": {"dev": "^1"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let e = g.root().dependency("dev").unwrap();
    assert!(e.behavior.is_dev());
    assert_eq!(keys(&g), ["a@1.0.0", "b@1.0.0", "dev@1.0.0"]);
}

#[tokio::test]
async fn optional_dependency_overrides_dependency_of_the_same_name() {
    let h = Harness::new();
    h.registry
        .add("o", &[("1.0.0", no_deps()), ("2.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"o": "^1"}, "optionalDependencies": {"o": "^2"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let edges: Vec<_> = g
        .root()
        .dependencies
        .iter()
        .filter(|e| e.name == "o")
        .collect();
    assert_eq!(edges.len(), 1);
    assert!(edges[0].behavior.is_optional());
    assert_eq!(root_dep(&g, "o").as_deref(), Some("2.0.0"));
}

#[tokio::test]
async fn optional_failures_are_tolerated() {
    let h = Harness::new();
    h.registry
        .add(
            "a",
            &[(
                "1.0.0",
                json!({"optionalDependencies": {"gone": "^1", "bad": "^9"}}),
            )],
        )
        .add("bad", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"a": "1.0.0"}, "optionalDependencies": {"nope": "^1"}}),
        &[],
    );
    let out = h.install(&ws).await;
    let a = id_of(&out.graph, "a", "1.0.0");
    assert!(
        out.graph.packages[a.index()]
            .dependencies
            .iter()
            .all(|e| e.target.is_none())
    );
    assert!(
        out.graph
            .root()
            .dependency("nope")
            .unwrap()
            .target
            .is_none()
    );
    // Missing packages and versions of optional dependencies are skipped silently.
    assert!(
        out.warnings
            .iter()
            .all(|w| !w.contains("No version matching")),
        "{:?}",
        out.warnings
    );
}

#[tokio::test]
async fn failures_below_an_optional_dependency_are_tolerated() {
    let h = Harness::new();
    h.registry.add(
        "opt",
        &[("1.0.0", json!({"dependencies": {"missing-dep": "^1"}}))],
    );
    let ws = workspace(json!({"optionalDependencies": {"opt": "1.0.0"}}), &[]);
    let out = h.install(&ws).await;
    let opt = out.graph.root().dependency("opt").unwrap().target.unwrap();
    assert!(
        out.graph.packages[opt.index()]
            .dependency("missing-dep")
            .unwrap()
            .target
            .is_none()
    );
    // The same through a required edge is fatal.
    let ws = workspace(json!({"dependencies": {"opt": "1.0.0"}}), &[]);
    assert!(matches!(
        h.run(&ws, None, Mode::Normal).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn network_errors_of_required_dependencies_are_fatal() {
    let h = Harness::new();
    h.registry.broken.lock().push("flaky".into());
    let ws = workspace(json!({"dependencies": {"flaky": "^1"}}), &[]);
    assert!(matches!(
        h.run(&ws, None, Mode::Normal).await,
        Err(Error::Registry(fnpm_registry::Error::Network { .. }))
    ));
    let ws = workspace(json!({"optionalDependencies": {"flaky": "^1"}}), &[]);
    let out = h.install(&ws).await;
    assert!(
        out.warnings.iter().any(|w| w.contains("connection reset")),
        "{:?}",
        out.warnings
    );
}

#[tokio::test]
async fn platform_mismatches_stay_in_the_graph_but_are_not_streamed() {
    let mut h = Harness::new();
    h.config.platform = fnpm_resolver::options::platform_from_cli(
        h.config.platform,
        &["linux"],
        &["x64"],
        &["glibc"],
    )
    .unwrap();
    h.registry
        .add("esbuild", &[("0.21.5", json!({"optionalDependencies": {"@esbuild/linux-x64": "0.21.5", "@esbuild/darwin-arm64": "0.21.5"}}))])
        .add("@esbuild/linux-x64", &[("0.21.5", json!({"os": ["linux"], "cpu": ["x64"]}))])
        .add("@esbuild/darwin-arm64", &[("0.21.5", json!({"os": ["darwin"], "cpu": ["arm64"]}))]);
    let ws = workspace(json!({"dependencies": {"esbuild": "0.21.5"}}), &[]);
    let (out, events) = h
        .run_full(&ws, None, Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    assert_eq!(
        keys(&out.graph),
        [
            "@esbuild/darwin-arm64@0.21.5",
            "@esbuild/linux-x64@0.21.5",
            "esbuild@0.21.5"
        ]
    );
    let streamed: Vec<String> = events
        .iter()
        .map(|Resolved::Package { name, .. }| name.clone())
        .collect();
    assert!(streamed.contains(&"@esbuild/linux-x64".to_owned()));
    assert!(!streamed.contains(&"@esbuild/darwin-arm64".to_owned()));
    let darwin = out
        .graph
        .packages
        .iter()
        .find(|p| p.name == "@esbuild/darwin-arm64")
        .unwrap();
    assert!(darwin.is_disabled(&h.config.platform));
}

#[tokio::test]
async fn cli_platform_overrides_select_other_binaries() {
    let mut h = Harness::new();
    h.config.platform = fnpm_resolver::options::platform_from_cli(
        h.config.platform,
        &["darwin"],
        &["arm64"],
        &[] as &[&str],
    )
    .unwrap();
    h.registry
        .add(
            "bin",
            &[(
                "1.0.0",
                json!({"optionalDependencies": {"bin-linux": "1.0.0", "bin-darwin": "1.0.0"}}),
            )],
        )
        .add("bin-linux", &[("1.0.0", json!({"os": ["linux"]}))])
        .add(
            "bin-darwin",
            &[("1.0.0", json!({"os": ["darwin"], "cpu": ["arm64"]}))],
        );
    let ws = workspace(json!({"dependencies": {"bin": "1.0.0"}}), &[]);
    let (_, events) = h
        .run_full(&ws, None, Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    let streamed: Vec<String> = events
        .iter()
        .map(|Resolved::Package { name, .. }| name.clone())
        .collect();
    assert!(
        streamed.contains(&"bin-darwin".to_owned()) && !streamed.contains(&"bin-linux".to_owned()),
        "{streamed:?}"
    );
}

#[tokio::test]
async fn bundled_dependencies_are_not_fetched_separately() {
    let h = Harness::new();
    h.registry
        .add(
            "host",
            &[(
                "1.0.0",
                json!({"dependencies": {"inner": "^1"}, "bundleDependencies": ["inner"]}),
            )],
        )
        .add("inner", &[("1.0.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"host": "1.0.0"}}), &[]);
    let (out, events) = h
        .run_full(&ws, None, Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    let host = id_of(&out.graph, "host", "1.0.0");
    assert!(
        out.graph.packages[host.index()]
            .dependency("inner")
            .unwrap()
            .behavior
            .is_bundled()
    );
    let inner = out
        .graph
        .packages
        .iter()
        .find(|p| p.name == "inner")
        .unwrap();
    assert!(inner.meta.bundled);
    let streamed: Vec<String> = events
        .iter()
        .map(|Resolved::Package { name, .. }| name.clone())
        .collect();
    assert_eq!(streamed, ["host"]);
}

#[tokio::test]
async fn a_bundled_package_shared_by_a_regular_dependency_is_streamed() {
    // `host` bundles `inner`; `zz` (resolved after host's bundled edge) asks for the same
    // version, so the copy stops being bundle-only and must be fetched on its own.
    let h = Harness::new();
    h.registry
        .add(
            "host",
            &[(
                "1.0.0",
                json!({"dependencies": {"inner": "^1"}, "bundleDependencies": ["inner"]}),
            )],
        )
        .add("zz", &[("1.0.0", json!({"dependencies": {"inner": "^1"}}))])
        .add(
            "inner",
            &[("1.0.0", json!({"dependencies": {"leaf": "1"}}))],
        )
        .add("leaf", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"host": "1.0.0", "zz": "1.0.0"}}),
        &[],
    );
    let (out, events) = h
        .run_full(&ws, None, Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    let inner = id_of(&out.graph, "inner", "1.0.0");
    assert!(!out.graph.packages[inner.index()].meta.bundled);
    let mut streamed: Vec<String> = events
        .iter()
        .map(|Resolved::Package { name, .. }| name.clone())
        .collect();
    streamed.sort();
    assert_eq!(streamed, ["host", "inner", "leaf", "zz"]);
}

#[tokio::test]
async fn streams_each_storable_package_once() {
    let h = Harness::new();
    h.registry
        .add(
            "a",
            &[("1.0.0", json!({"dependencies": {"b": "^1", "c": "^1"}}))],
        )
        .add("b", &[("1.0.0", json!({"dependencies": {"c": "^1"}}))])
        .add("c", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"a": "^1", "b": "^1"}}),
        &[(
            "packages/w",
            json!({"name": "w", "version": "1.0.0", "dependencies": {"c": "^1"}}),
        )],
    );
    let (out, events) = h
        .run_full(&ws, None, Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    let mut streamed: Vec<String> = events
        .iter()
        .map(|Resolved::Package { name, version, .. }| format!("{name}@{version}"))
        .collect();
    streamed.sort();
    assert_eq!(streamed, ["a@1.0.0", "b@1.0.0", "c@1.0.0"]);
    for Resolved::Package {
        id,
        name,
        resolution,
        ..
    } in &events
    {
        let p = &out.graph.packages[id.index()];
        assert_eq!((&p.name, &p.resolution), (name, resolution));
    }
}

#[tokio::test]
async fn omitted_groups_are_not_streamed() {
    let mut h = Harness::new();
    h.config.omit.dev = true;
    h.registry
        .add("dev", &[("1.0.0", no_deps())])
        .add("prod", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"prod": "1.0.0"}, "devDependencies": {"dev": "1.0.0"}}),
        &[],
    );
    let (out, events) = h
        .run_full(&ws, None, Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    assert_eq!(keys(&out.graph), ["dev@1.0.0", "prod@1.0.0"]);
    let streamed: Vec<String> = events
        .iter()
        .map(|Resolved::Package { name, .. }| name.clone())
        .collect();
    assert_eq!(streamed, ["prod"]);
}

#[tokio::test]
async fn custom_tarball_urls_are_kept_default_ones_dropped() {
    let h = Harness::new();
    h.registry.add("a", &[("1.0.0", no_deps())]).add(
        "m",
        &[(
            "1.0.0",
            json!({"tarball": "https://mirror.test/m-1.0.0.tgz"}),
        )],
    );
    let ws = workspace(json!({"dependencies": {"a": "1.0.0", "m": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    match &dep(&g, PackageId::ROOT, "a").unwrap().resolution {
        Resolution::Npm {
            tarball_url,
            integrity,
            ..
        } => assert!(tarball_url.is_none() && integrity.is_some()),
        r => panic!("{r:?}"),
    }
    match &dep(&g, PackageId::ROOT, "m").unwrap().resolution {
        Resolution::Npm { tarball_url, .. } => assert_eq!(
            tarball_url.as_deref(),
            Some("https://mirror.test/m-1.0.0.tgz")
        ),
        r => panic!("{r:?}"),
    }
}

#[tokio::test]
async fn metadata_is_copied_from_the_manifest() {
    let h = Harness::new();
    h.registry.add(
        "tool",
        &[(
            "1.0.0",
            json!({"bin": {"tool": "cli.js"}, "hasInstallScript": true, "os": ["!win32"]}),
        )],
    );
    let ws = workspace(json!({"dependencies": {"tool": "1"}}), &[]);
    let g = h.install(&ws).await.graph;
    let t = dep(&g, PackageId::ROOT, "tool").unwrap();
    assert!(t.meta.has_install_script);
    assert!(!t.bin.is_none());
    assert!(!t.platform.is_any());
}

#[tokio::test]
async fn deterministic_regardless_of_timing() {
    let mut graphs = Vec::new();
    for delay in [0, 3] {
        let h = Harness::with_registry(FakeRegistry::with_delay(delay));
        h.registry
            .add(
                "a",
                &[("1.0.0", json!({"dependencies": {"x": "^1", "y": "^1"}}))],
            )
            .add(
                "b",
                &[("1.0.0", json!({"dependencies": {"x": "^1.1", "z": "^2"}}))],
            )
            .add(
                "x",
                &[
                    ("1.0.0", no_deps()),
                    ("1.1.0", no_deps()),
                    ("1.2.0", no_deps()),
                ],
            )
            .add("y", &[("1.0.0", json!({"dependencies": {"z": "^1"}}))])
            .add("z", &[("1.0.0", no_deps()), ("2.0.0", no_deps())]);
        let ws = workspace(json!({"dependencies": {"a": "1", "b": "1"}}), &[]);
        graphs.push(h.install(&ws).await.graph);
    }
    assert_eq!(graphs[0], graphs[1]);
}

#[tokio::test]
async fn look_ahead_requests_transitive_manifests_concurrently() {
    let h = Harness::with_registry(FakeRegistry::with_delay(20));
    // A chain root -> a -> b -> c -> d and a fan-out root -> f1..f8.
    h.registry
        .add("a", &[("1.0.0", json!({"dependencies": {"b": "^1"}}))])
        .add("b", &[("1.0.0", json!({"dependencies": {"c": "^1"}}))])
        .add("c", &[("1.0.0", json!({"dependencies": {"d": "^1"}}))])
        .add("d", &[("1.0.0", no_deps())]);
    let mut deps = serde_json::Map::new();
    deps.insert("a".into(), json!("^1"));
    for i in 1..=8 {
        let name = format!("f{i}");
        h.registry.add(&name, &[("1.0.0", no_deps())]);
        deps.insert(name, json!("^1"));
    }
    let ws = workspace(json!({"dependencies": deps}), &[]);
    let start = std::time::Instant::now();
    let out = h.install(&ws).await;
    assert_eq!(out.graph.len(), 13);
    assert!(
        h.registry
            .max_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 9
    );
    // Each manifest is requested exactly once.
    for name in ["a", "b", "c", "d", "f1"] {
        assert_eq!(h.registry.request_count(name), 1, "{name}");
    }
    // 4 sequential levels at 20 ms each: the fan-out must not add levels.
    assert!(
        start.elapsed() < std::time::Duration::from_millis(400),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn progress_events_are_emitted() {
    let h = Harness::new();
    h.registry.add("a", &[("1.0.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"a": "1"}}), &[]);
    let sink = fnpm_types::event::VecSink::default();
    let input = fnpm_resolver::ResolveInput {
        workspace: &ws,
        previous: None,
        mode: Mode::Normal,
        requests: vec![],
        removals: vec![],
    };
    h.resolver().resolve(input, None, &sink).await.unwrap();
    let events = sink.0.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, fnpm_types::Event::Resolve { name: Some(n), .. } if n == "a"))
    );
}

#[tokio::test]
async fn requests_add_dependencies_and_report_targets() {
    let h = Harness::new();
    h.registry
        .add("new", &[("1.0.0", no_deps()), ("1.4.0", no_deps())])
        .add("old", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"old": "1"}, "devDependencies": {"new": "^1.0.0"}}),
        &[],
    );
    let (out, _) = h
        .run_full(
            &ws,
            None,
            Mode::Normal,
            vec![request("", "new", "latest", DependencyGroup::Prod)],
            vec![("".into(), "old".into())],
        )
        .await
        .unwrap();
    assert_eq!(out.requests.len(), 1);
    assert_eq!(
        out.graph.packages[out.requests[0].1.index()].version,
        "1.4.0"
    );
    // Moved from devDependencies to dependencies; `old` removed.
    let edges: Vec<_> = out
        .graph
        .root()
        .dependencies
        .iter()
        .map(|e| (e.name.as_str(), e.behavior.group()))
        .collect();
    assert_eq!(edges, [("new", DependencyGroup::Prod)]);
    assert_eq!(keys(&out.graph), ["new@1.4.0"]);
}

#[tokio::test]
async fn a_request_for_an_unknown_workspace_is_an_error() {
    let h = Harness::new();
    let ws = workspace(json!({}), &[]);
    let err = h
        .run_full(
            &ws,
            None,
            Mode::Normal,
            vec![request("nope", "a", "1", DependencyGroup::Prod)],
            vec![],
        )
        .await;
    assert!(matches!(err, Err(Error::WorkspaceNotFound { .. })));
}

#[tokio::test]
async fn behaviors_are_recorded_on_edges() {
    let h = Harness::new();
    h.registry.add("x", &[("1.0.0", no_deps())]);
    let ws = workspace(json!({"optionalDependencies": {"x": "1"}}), &[]);
    let g = h.install(&ws).await.graph;
    assert_eq!(
        g.root().dependency("x").unwrap().behavior,
        Behavior::OPTIONAL
    );
}
