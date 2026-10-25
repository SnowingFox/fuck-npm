//! trustedDependencies semantics (bun `lockfile.rs` `has_trusted_dependency`,
//! `can_trust_by_name`, `is_folder_inside_declarer`; bun-install-lifecycle-scripts.test.ts
//! "the trusted set", "default trusted dependencies ...").

mod common;

use std::collections::BTreeSet;

use common::*;
use fnpm_scripts::trust::{
    TrustPolicy, can_trust_by_name, canonical_tarball_url, default_trusted, is_default_trusted,
    trusted_from_requests, trusted_name,
};
use fnpm_types::{OverrideRule, OverrideSelector, Package, PackageId, Resolution};

fn set(names: &[&str]) -> Option<BTreeSet<String>> {
    Some(names.iter().map(|s| s.to_string()).collect())
}

#[test]
fn default_list_is_sorted_and_complete() {
    let list = default_trusted();
    let data = include_str!("../data/default-trusted-dependencies.txt");
    let mut expected: Vec<&str> = data.split_whitespace().collect();
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(list, expected.as_slice());
    assert!(list.len() > 300 && list.len() <= 512);
    assert!(list.windows(2).all(|w| w[0] < w[1]));
    for name in list {
        assert!(is_default_trusted(name), "{name}");
    }
}

#[test]
fn default_list_lookup_misses() {
    for name in [
        "",
        "esbuil",
        "esbuildx",
        "left-pad",
        "@esbuild/linux-x64",
        "ESBUILD",
        "react",
        "a",
    ] {
        assert!(!is_default_trusted(name), "{name}");
    }
    assert!(is_default_trusted("esbuild"));
    assert!(is_default_trusted("@anthropic-ai/claude-code"));
    assert!(is_default_trusted("sharp"));
}

#[test]
fn canonical_urls() {
    assert_eq!(
        canonical_tarball_url("https://registry.npmjs.org/", "esbuild", "0.21.5"),
        "https://registry.npmjs.org/esbuild/-/esbuild-0.21.5.tgz"
    );
    assert_eq!(
        canonical_tarball_url("https://r.example/npm", "@s/pkg", "1.0.0-beta.1"),
        "https://r.example/npm/@s/pkg/-/pkg-1.0.0-beta.1.tgz"
    );
}

#[test]
fn root_and_workspaces_always_trusted() {
    let mut g = root("app");
    let ws = g.add_workspace(Package::new(
        "@app/lib",
        "1.0.0",
        Resolution::Workspace {
            path: "packages/lib".into(),
        },
    ));
    g.trusted_dependencies = set(&[]);
    let p = TrustPolicy::from_graph(&g, []);
    assert!(p.is_trusted(&g, PackageId::ROOT));
    assert!(p.is_trusted(&g, ws));
}

#[test]
fn default_list_applies_without_field() {
    let mut g = root("app");
    let esbuild = add(&mut g, with_scripts(npm("esbuild", "0.21.5")));
    let other = add(&mut g, with_scripts(npm("uses-what-bin", "1.0.0")));
    edge(&mut g, PackageId::ROOT, "esbuild", esbuild);
    edge(&mut g, PackageId::ROOT, "uses-what-bin", other);
    let p = TrustPolicy::from_graph(&g, []);
    assert!(p.is_trusted(&g, esbuild));
    assert!(!p.is_trusted(&g, other));
}

#[test]
fn explicit_list_replaces_default() {
    let mut g = root("app");
    let esbuild = add(&mut g, npm("esbuild", "0.21.5"));
    let other = add(&mut g, npm("uses-what-bin", "1.0.0"));
    edge(&mut g, PackageId::ROOT, "esbuild", esbuild);
    edge(&mut g, PackageId::ROOT, "uses-what-bin", other);
    g.trusted_dependencies = set(&["uses-what-bin"]);
    let p = TrustPolicy::from_graph(&g, []);
    assert!(
        !p.is_trusted(&g, esbuild),
        "an explicit list turns the default list off"
    );
    assert!(p.is_trusted(&g, other));
}

#[test]
fn empty_list_trusts_nothing() {
    let mut g = root("app");
    let esbuild = add(&mut g, npm("esbuild", "0.21.5"));
    edge(&mut g, PackageId::ROOT, "esbuild", esbuild);
    g.trusted_dependencies = set(&[]);
    assert!(!TrustPolicy::from_graph(&g, []).is_trusted(&g, esbuild));
}

#[test]
fn default_trust_requires_canonical_tarball_url() {
    let mut g = root("app");
    let canonical = add(
        &mut g,
        npm_url(
            "esbuild",
            "0.21.5",
            "https://registry.npmjs.org/esbuild/-/esbuild-0.21.5.tgz",
        ),
    );
    let empty = add(&mut g, npm_url("sharp", "0.33.0", ""));
    let elsewhere = add(
        &mut g,
        npm_url(
            "node-sass",
            "9.0.0",
            "https://evil.example/node-sass-9.0.0.tgz",
        ),
    );
    let p = TrustPolicy::from_graph(&g, []);
    assert!(p.is_trusted(&g, canonical));
    assert!(p.is_trusted(&g, empty));
    assert!(!p.is_trusted(&g, elsewhere));
    // With a scoped registry configured, its URLs are canonical.
    let mirror = add(
        &mut g,
        npm_url(
            "esbuild",
            "0.20.0",
            "https://mirror.example/esbuild/-/esbuild-0.20.0.tgz",
        ),
    );
    assert!(!p.is_trusted(&g, mirror));
    assert!(p.is_trusted_with_registry(&g, mirror, &|_| "https://mirror.example/"));
    // An explicit list trusts by name wherever the tarball comes from.
    g.trusted_dependencies = set(&["node-sass"]);
    assert!(TrustPolicy::from_graph(&g, []).is_trusted(&g, elsewhere));
}

#[test]
fn npm_alias_is_trusted_by_package_name() {
    // trustedDependencies matches the resolved package name, not the dependency alias.
    let mut g = root("app");
    let pkg = add(&mut g, npm("uses-what-bin", "1.0.0"));
    edge(&mut g, PackageId::ROOT, "my-alias", pkg);
    g.trusted_dependencies = set(&["my-alias"]);
    assert!(!TrustPolicy::from_graph(&g, []).is_trusted(&g, pkg));
    g.trusted_dependencies = set(&["uses-what-bin"]);
    assert!(TrustPolicy::from_graph(&g, []).is_trusted(&g, pkg));
    // An npm: alias of a default-trusted name does not inherit trust.
    let mut g = root("app");
    let evil = add(&mut g, npm("not-esbuild", "1.0.0"));
    edge(&mut g, PackageId::ROOT, "esbuild", evil);
    assert!(!TrustPolicy::from_graph(&g, []).is_trusted(&g, evil));
    assert_eq!(
        trusted_name(
            "esbuild",
            "not-esbuild",
            &g.packages[evil.index()].resolution
        ),
        "not-esbuild"
    );
}

#[test]
fn non_npm_dependencies_need_a_declaration() {
    let mut g = root("app");
    let git = add(
        &mut g,
        Package::new(
            "dep",
            "1.0.0",
            Resolution::Github {
                owner: "o".into(),
                repo: "r".into(),
                committish: "".into(),
                resolved_commit: "abc".into(),
                integrity: None,
            },
        ),
    );
    let host = add(&mut g, npm("host", "1.0.0"));
    edge(&mut g, host, "dep", git);
    edge(&mut g, PackageId::ROOT, "host", host);
    g.trusted_dependencies = set(&["dep", "host"]);
    let p = TrustPolicy::from_graph(&g, []);
    assert!(
        !p.is_trusted(&g, git),
        "only a registry package declares it"
    );
    assert!(!can_trust_by_name(&g, "dep", git));
    edge(&mut g, PackageId::ROOT, "dep", git);
    assert!(p.is_trusted(&g, git));
    assert!(can_trust_by_name(&g, "dep", git));
    assert_eq!(
        trusted_name("dep", "real-name", &g.packages[git.index()].resolution),
        "dep"
    );
    // The default list never applies to non-npm packages.
    let mut g = root("app");
    let file = add(
        &mut g,
        Package::new(
            "esbuild",
            "1.0.0",
            Resolution::Folder {
                path: "vendor/esbuild".into(),
            },
        ),
    );
    edge(&mut g, PackageId::ROOT, "esbuild", file);
    assert!(!TrustPolicy::from_graph(&g, []).is_trusted(&g, file));
    g.trusted_dependencies = set(&["esbuild"]);
    assert!(TrustPolicy::from_graph(&g, []).is_trusted(&g, file));
}

#[test]
fn folder_inside_its_declarer_is_never_trusted() {
    let mut g = root("app");
    let host = add(&mut g, npm("host", "1.0.0"));
    let inner = add(
        &mut g,
        Package::new(
            "inner",
            "1.0.0",
            Resolution::Folder {
                path: "node_modules/host/inner".into(),
            },
        ),
    );
    edge(&mut g, PackageId::ROOT, "host", host);
    edge(&mut g, host, "inner", inner);
    g.trusted_dependencies = set(&["inner", "host"]);
    let p = TrustPolicy::from_graph(&g, ["inner".to_string()]);
    assert!(!p.is_trusted(&g, inner));
    assert!(!fnpm_scripts::trust::is_trusted_folder_package(&g, inner));
    // An override naming it makes the path user authored.
    g.overrides.push(OverrideRule {
        selector: OverrideSelector::parse("inner").unwrap().unwrap(),
        value: "file:./vendor/inner".into(),
    });
    assert!(fnpm_scripts::trust::is_trusted_folder_package(&g, inner));
}

#[test]
fn extra_names_from_trust_flag() {
    let mut g = root("app");
    let pkg = add(&mut g, npm("uses-what-bin", "1.0.0"));
    edge(&mut g, PackageId::ROOT, "uses-what-bin", pkg);
    g.trusted_dependencies = set(&[]);
    assert!(!TrustPolicy::from_graph(&g, []).is_trusted(&g, pkg));
    assert!(TrustPolicy::from_graph(&g, ["uses-what-bin".to_string()]).is_trusted(&g, pkg));
}

#[test]
fn trust_requests_cover_the_subtree_that_names_can_trust() {
    let mut g = root("app");
    let a = add(&mut g, npm("a", "1.0.0"));
    let b = add(&mut g, npm("b", "1.0.0"));
    let c = add(&mut g, npm("c", "1.0.0"));
    let git = add(
        &mut g,
        Package::new(
            "g",
            "1.0.0",
            Resolution::Git {
                url: "u".into(),
                committish: "".into(),
                resolved_commit: "x".into(),
                integrity: None,
            },
        ),
    );
    let below_git = add(&mut g, npm("d", "1.0.0"));
    let unrelated = add(&mut g, npm("e", "1.0.0"));
    edge(&mut g, PackageId::ROOT, "a", a);
    edge(&mut g, PackageId::ROOT, "e", unrelated);
    edge(&mut g, a, "b", b);
    edge(&mut g, b, "c", c);
    edge(&mut g, a, "g", git);
    edge(&mut g, git, "d", below_git);
    edge(&mut g, c, "a", a);
    let s = trusted_from_requests(&g, &["a"]);
    assert!(s.contains(&a) && s.contains(&b) && s.contains(&c));
    assert!(
        !s.contains(&git),
        "a git package below cannot be trusted by name"
    );
    assert!(
        s.contains(&below_git),
        "what it depends on is still visited"
    );
    assert!(!s.contains(&unrelated));
    assert!(trusted_from_requests(&g, &[]).is_empty());
}

#[test]
fn blocked_lists_untrusted_packages_with_scripts() {
    let mut g = root("app");
    g.trusted_dependencies = set(&["trusted"]);
    let t = add(&mut g, with_scripts(npm("trusted", "1.0.0")));
    let u = add(&mut g, with_scripts(npm("untrusted", "2.0.0")));
    let none = add(&mut g, npm("no-scripts", "1.0.0"));
    for (name, id) in [("trusted", t), ("untrusted", u), ("no-scripts", none)] {
        edge(&mut g, PackageId::ROOT, name, id);
    }
    let p = TrustPolicy::from_graph(&g, []);
    let blocked = p.blocked(&g, |_| Some(vec!["postinstall".into()]));
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].name, "untrusted");
    assert_eq!(blocked[0].version, "2.0.0");
    assert_eq!(blocked[0].scripts, ["postinstall"]);
    // `hasInstallScript` is a false positive when the package has nothing to run.
    assert!(p.blocked(&g, |_| Some(vec![])).is_empty());
}
