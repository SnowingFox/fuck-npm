//! Peer dependencies: auto-install, providers from above, optional and meta-only peers,
//! agreeing ranges, warnings and dev/peer priority. Ported from the peer cases of
//! `bun-install.test.ts`, `bun-install-peer-deps*` and `test-dev-peer-dependency-priority.test.ts`.

mod common;

use common::*;
use fnpm_types::PackageId;
use serde_json::json;

fn react_registry(h: &Harness) {
    h.registry
        .add(
            "react",
            &[
                ("17.0.2", no_deps()),
                ("18.2.0", no_deps()),
                ("18.3.1", no_deps()),
            ],
        )
        .add(
            "ui",
            &[("1.0.0", json!({"peerDependencies": {"react": ">=17"}}))],
        )
        .add(
            "ui18",
            &[("1.0.0", json!({"peerDependencies": {"react": "^18.0.0"}}))],
        )
        .add(
            "ui-old",
            &[("1.0.0", json!({"peerDependencies": {"react": "^17.0.0"}}))],
        );
}

#[tokio::test]
async fn peers_are_installed_when_nobody_provides_them() {
    let h = Harness::new();
    react_registry(&h);
    let ws = workspace(json!({"dependencies": {"ui": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    let ui = id_of(&g, "ui", "1.0.0");
    assert_eq!(peer(&g, ui, "react").unwrap().version, "18.3.1");
    assert_eq!(versions(&g, "react"), ["18.3.1"]);
}

#[tokio::test]
async fn peers_bind_to_what_the_dependent_provides_even_out_of_range() {
    let h = Harness::new();
    react_registry(&h);
    let ws = workspace(
        json!({"dependencies": {"react": "17.0.2", "ui18": "1.0.0"}}),
        &[],
    );
    let out = h.install(&ws).await;
    let ui = id_of(&out.graph, "ui18", "1.0.0");
    // pnpm semantics: the root's react@17 serves the peer. The install reports the mismatch
    // against the layout it links (fnpm-core `install::peers`), not the resolver.
    assert_eq!(peer(&out.graph, ui, "react").unwrap().version, "17.0.2");
    assert_eq!(versions(&out.graph, "react"), ["17.0.2"]);
    assert!(
        !out.warnings.iter().any(|w| w.starts_with("incorrect peer dependency")),
        "{:?}",
        out.warnings
    );
}

#[tokio::test]
async fn an_installed_version_in_range_is_used() {
    let h = Harness::new();
    react_registry(&h);
    h.registry.add(
        "dep",
        &[("1.0.0", json!({"dependencies": {"react": "18.2.0"}}))],
    );
    let ws = workspace(
        json!({"dependencies": {"dep": "1.0.0", "ui18": "1.0.0"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let ui = id_of(&g, "ui18", "1.0.0");
    assert_eq!(peer(&g, ui, "react").unwrap().version, "18.2.0");
    assert_eq!(versions(&g, "react"), ["18.2.0"]);
}

#[tokio::test]
async fn askers_without_a_provider_agree_on_one_version() {
    let h = Harness::new();
    react_registry(&h);
    let ws = workspace(
        json!({"dependencies": {"ui": "1.0.0", "ui-old": "1.0.0"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    // `>=17` and `^17.0.0` agree on 17.0.2, which both get.
    assert_eq!(versions(&g, "react"), ["17.0.2"]);
    let (ui, old) = (id_of(&g, "ui", "1.0.0"), id_of(&g, "ui-old", "1.0.0"));
    assert_eq!(peer(&g, ui, "react").unwrap().version, "17.0.2");
    assert_eq!(peer(&g, old, "react").unwrap().version, "17.0.2");
}

#[tokio::test]
async fn optional_peers_are_not_installed() {
    let h = Harness::new();
    h.registry
        .add("plugin", &[("1.0.0", json!({"peerDependencies": {"host": "^1", "extra": "^1"}, "peerDependenciesMeta": {"extra": {"optional": true}, "meta-only": {"optional": true}}}))])
        .add("host", &[("1.0.0", no_deps())])
        .add("extra", &[("1.0.0", no_deps())])
        .add("meta-only", &[("1.0.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"plugin": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    assert_eq!(keys(&g), ["host@1.0.0", "plugin@1.0.0"]);
    let plugin = &g.packages[id_of(&g, "plugin", "1.0.0").index()];
    let extra = plugin.dependency("extra").unwrap();
    assert!(extra.behavior.is_optional_peer() && extra.target.is_none());
    // A name only in peerDependenciesMeta becomes an optional `*` peer.
    let meta = plugin.dependency("meta-only").unwrap();
    assert!(meta.behavior.is_optional_peer());
    assert_eq!(meta.literal, "*");
    assert!(h.registry.request_count("extra") == 0 && h.registry.request_count("meta-only") == 0);
}

#[tokio::test]
async fn optional_peers_bind_when_provided() {
    let h = Harness::new();
    h.registry
        .add("plugin", &[("1.0.0", json!({"peerDependencies": {"extra": "^1"}, "peerDependenciesMeta": {"extra": {"optional": true}}}))])
        .add("extra", &[("1.0.0", no_deps()), ("1.5.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"plugin": "1.0.0", "extra": "1.0.0"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let plugin = id_of(&g, "plugin", "1.0.0");
    assert_eq!(peer(&g, plugin, "extra").unwrap().version, "1.0.0");
}

#[tokio::test]
async fn auto_install_peers_off_leaves_them_unresolved_with_a_warning() {
    let mut h = Harness::new();
    h.config.auto_install_peers = false;
    react_registry(&h);
    let ws = workspace(json!({"dependencies": {"ui": "1.0.0"}}), &[]);
    let out = h.install(&ws).await;
    assert_eq!(keys(&out.graph), ["ui@1.0.0"]);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("missing peer dependency \"react@>=17\"")),
        "{:?}",
        out.warnings
    );
    assert_eq!(h.registry.request_count("react"), 0);
}

#[tokio::test]
async fn unmet_peers_only_warn() {
    let h = Harness::new();
    h.registry
        .add("p", &[("1.0.0", json!({"peerDependencies": {"q": "^9"}}))])
        .add("q", &[("1.0.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"p": "1.0.0"}}), &[]);
    let out = h.install(&ws).await;
    assert!(
        out.warnings.iter().any(|w| w
            == "No version matching \"^9\" found for peer dependency \"q\" (but package exists)"),
        "{:?}",
        out.warnings
    );
}

#[tokio::test]
async fn root_dev_dependency_wins_over_its_own_peer() {
    let h = Harness::new();
    h.registry
        .add("no-deps", &[("1.0.0", no_deps()), ("1.1.0", no_deps())]);
    let ws = workspace(
        json!({"devDependencies": {"no-deps": "1.0.0"}, "peerDependencies": {"no-deps": "^1.0.0"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    assert_eq!(versions(&g, "no-deps"), ["1.0.0"]);
    assert_eq!(
        peer(&g, PackageId::ROOT, "no-deps").unwrap().version,
        "1.0.0"
    );
}

#[tokio::test]
async fn workspace_dev_dependency_serves_its_peer_without_registry_requests() {
    let h = Harness::new();
    let ws = workspace(
        json!({"name": "mono", "workspaces": ["packages/*"]}),
        &[
            (
                "packages/lib",
                json!({"name": "lib", "version": "1.0.0", "devDependencies": {"no-deps": "workspace:*"}, "peerDependencies": {"no-deps": "1.0.0"}}),
            ),
            (
                "packages/no-deps",
                json!({"name": "no-deps", "version": "2.0.0"}),
            ),
        ],
    );
    let g = h.install(&ws).await.graph;
    let lib = g.workspaces["packages/lib"];
    assert_eq!(
        peer(&g, lib, "no-deps").unwrap().resolution.to_string(),
        "workspace:packages/no-deps"
    );
    assert!(
        h.registry.requests().is_empty(),
        "{:?}",
        h.registry.requests()
    );
}

#[tokio::test]
async fn peer_of_a_peer_is_resolved_in_a_later_batch() {
    let h = Harness::new();
    h.registry
        .add("a", &[("1.0.0", json!({"peerDependencies": {"b": "^1"}}))])
        .add("b", &[("1.0.0", json!({"peerDependencies": {"c": "^1"}}))])
        .add("c", &[("1.0.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"a": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    assert_eq!(keys(&g), ["a@1.0.0", "b@1.0.0", "c@1.0.0"]);
}

#[tokio::test]
async fn bun_peer_is_optional_for_published_packages() {
    let h = Harness::new();
    h.registry
        .add("x", &[("1.0.0", json!({"peerDependencies": {"bun": "*"}}))]);
    let ws = workspace(json!({"dependencies": {"x": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    assert_eq!(keys(&g), ["x@1.0.0"]);
}

#[tokio::test]
async fn peer_provided_by_a_folder_of_the_root() {
    let h = Harness::new();
    h.fetcher.folder(
        "/project/local-react",
        json!({"name": "react", "version": "0.0.0-local"}),
    );
    react_registry(&h);
    let ws = workspace(
        json!({"dependencies": {"react": "file:./local-react", "ui": "1.0.0"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let reacts: Vec<_> = g
        .packages
        .iter()
        .filter(|p| p.name == "react")
        .map(|p| p.resolution.to_string())
        .collect();
    assert_eq!(reacts, ["file:local-react"]);
    let ui = id_of(&g, "ui", "1.0.0");
    // The root's folder provides the peer (the declarer's dependents come first).
    assert_eq!(
        peer(&g, ui, "react").unwrap().resolution.to_string(),
        "file:local-react"
    );
    assert_eq!(h.registry.request_count("react"), 0);
}
