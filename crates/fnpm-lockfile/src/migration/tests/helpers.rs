use serde_json::json;

use super::support::workspace;
use crate::formats::bun_text::{json_str, resolved_commit};
use crate::migration::{
    MigrateOptions, Report, Semver, default_tarball_url, is_safe_resolved_tag, is_yarn_berry,
    package_name_from_path, path_escapes, root_overrides, trusted_dependencies_from_manifests,
    url_is_under_registry,
};
use fnpm_types::{LockGraph, Package, Resolution};

#[test]
fn registry_urls() {
    assert!(url_is_under_registry(
        "https://registry.npmjs.org/a/-/a-1.0.0.tgz",
        "https://registry.npmjs.org/"
    ));
    assert!(url_is_under_registry(
        "https://registry.npmjs.org",
        "https://registry.npmjs.org/"
    ));
    assert!(!url_is_under_registry(
        "https://registry.npmjs.org.evil.com/a.tgz",
        "https://registry.npmjs.org"
    ));
    assert!(!url_is_under_registry(
        "https://other.example/a.tgz",
        "https://registry.npmjs.org/"
    ));
    assert_eq!(
        default_tarball_url("https://r.example/", "@s/pkg", "1.2.3-beta.1"),
        "https://r.example/@s/pkg/-/pkg-1.2.3-beta.1.tgz"
    );
    assert_eq!(
        default_tarball_url("https://r.example", "left-pad", "1.3.0"),
        "https://r.example/left-pad/-/left-pad-1.3.0.tgz"
    );
    let mut o = MigrateOptions::default();
    o.scopes
        .insert("@corp".into(), "https://corp.example/".into());
    assert_eq!(o.registry_for("@corp/x"), "https://corp.example/");
    assert_eq!(o.registry_for("@other/x"), "https://registry.npmjs.org/");
    assert_eq!(o.registry_for("plain"), "https://registry.npmjs.org/");
}

#[test]
fn names_tags_and_paths() {
    assert_eq!(package_name_from_path("node_modules/a"), "a");
    assert_eq!(package_name_from_path("node_modules/@s/a"), "@s/a");
    assert_eq!(
        package_name_from_path("node_modules/a/node_modules/@s/b"),
        "@s/b"
    );
    assert_eq!(package_name_from_path("packages/lib"), "lib");
    assert_eq!(package_name_from_path("lib"), "lib");
    assert!(is_safe_resolved_tag(
        "3754568bfd43a841d2d72d7fb54598635aea8fa4"
    ));
    assert!(is_safe_resolved_tag("isaacs-abbrev-js-3f9802e"));
    for bad in ["", "-rf", ".", "..", "a/b", "a b", &"x".repeat(257)] {
        assert!(!is_safe_resolved_tag(bad), "{bad}");
    }
    assert_eq!(resolved_commit("isaacs-abbrev-js-3f9802e"), "3f9802e");
    assert_eq!(resolved_commit("abc"), "abc");
    for escaping in ["../x", "/abs", "C:foo", "a/../../b", "x\0y", "\\\\server"] {
        assert!(path_escapes(escaping), "{escaping}");
    }
    for inside in ["a", "a/b", "a/../b", "./a"] {
        assert!(!path_escapes(inside), "{inside}");
    }
    assert!(is_yarn_berry(b"__metadata:\n  version: 6\n"));
    assert!(!is_yarn_berry(b"# yarn lockfile v1\n"));
}

#[test]
fn json_strings() {
    let mut s = String::new();
    json_str(&mut s, "a\"b\\c\nd\u{1}");
    assert_eq!(s, r#""a\"b\\c\nd\u0001""#);
}

#[test]
fn test_semver_sanity() {
    let s = crate::migration::NodeSemver;
    assert!(s.satisfies("1.2.3", "^1.0.0"));
    assert!(!s.satisfies("2.0.0", "^1.0.0"));
    assert!(s.satisfies("0.1.5", "^0.1.2"));
    assert!(!s.satisfies("0.2.0", "^0.1.2"));
    assert!(s.satisfies("1.4.0", "~1.4"));
    assert!(s.satisfies("3.0.0", "^3.0.0 || ^4.0.0"));
    assert!(s.satisfies("1.0.0", "*"));
    assert!(!s.satisfies("1.0.0-beta", "*"));
    assert!(s.satisfies("1.0.0-beta.2", ">=1.0.0-beta.1"));
    assert!(s.satisfies("1.5.0", "1.2 - 2"));
    assert!(s.satisfies("2.9.9", ">=2 <3"));
    assert_eq!(s.compare("1.10.0", "1.9.0"), std::cmp::Ordering::Greater);
}

#[test]
fn overrides_from_package_json() {
    let ws = workspace(
        json!({
            "dependencies": { "react": "^18.2.0" },
            "overrides": {
                "// comment": "ignored",
                "foo": "1.0.0",
                "bar@^2": "2.1.0",
                "baz": { ".": "3.0.0", "qux": "4.0.0", "deep": { "x": "1" } },
                "ref": "$react",
                "missing": "$nope",
                "patched": "patch:x",
                "dash": "-",
                "": "x"
            },
            "resolutions": { "ignored": "1.0.0" }
        }),
        &[],
    );
    let mut graph = LockGraph::new(Package::new("app", "", Resolution::Root));
    graph.root_mut().dependencies.push(fnpm_types::DepEdge::new(
        "react",
        "^18.2.0",
        fnpm_types::Behavior::PROD,
    ));
    let mut report = Report::default();
    let rules = root_overrides(&graph, &ws, &mut report);
    let keys: Vec<(String, &str)> = rules
        .iter()
        .map(|r| (r.selector.to_key(), r.value.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            ("foo".to_owned(), "1.0.0"),
            ("bar@^2".to_owned(), "2.1.0"),
            ("baz".to_owned(), "3.0.0"),
            ("baz>qux".to_owned(), "4.0.0"),
            ("ref".to_owned(), "^18.2.0"),
        ]
    );
    assert_eq!(report.warnings.len(), 5, "{:?}", report.warnings);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("\"$nope\": \"nope\" is not in dependencies"))
    );

    // yarn `resolutions` when there are no `overrides`.
    let ws = workspace(
        json!({ "resolutions": { "a/b": "1.0.0", "**/c": "2.0.0" } }),
        &[],
    );
    let rules = root_overrides(&graph, &ws, &mut Report::default());
    let keys: Vec<String> = rules.iter().map(|r| r.selector.to_key()).collect();
    assert_eq!(keys, ["a>b", "c"]);
}

#[test]
fn trusted_dependencies_union() {
    let ws = workspace(json!({ "name": "root" }), &[("a", json!({ "name": "a" }))]);
    assert_eq!(trusted_dependencies_from_manifests(&ws), None);
    let ws = workspace(
        json!({ "trustedDependencies": [] }),
        &[("a", json!({ "name": "a" }))],
    );
    assert_eq!(
        trusted_dependencies_from_manifests(&ws),
        Some(Default::default())
    );
    let ws = workspace(
        json!({ "trustedDependencies": ["esbuild"] }),
        &[(
            "a",
            json!({ "name": "a", "trustedDependencies": ["sharp", "esbuild"] }),
        )],
    );
    assert_eq!(
        trusted_dependencies_from_manifests(&ws),
        Some(["esbuild".to_owned(), "sharp".to_owned()].into())
    );
}
