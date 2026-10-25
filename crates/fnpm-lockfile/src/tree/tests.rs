use fnpm_types::{
    Behavior, CpuSet, DepEdge, HashAlgorithm, Integrity, LockGraph, OsSet, Package, PackageId,
    Platform, Resolution,
};

use super::*;

fn npm(g: &mut LockGraph, name: &str, version: &str) -> PackageId {
    let i = Integrity::compute(
        HashAlgorithm::Sha512,
        format!("{name}@{version}").as_bytes(),
    );
    g.push(Package::new(
        name,
        version,
        Resolution::npm(version, Some(i)),
    ))
}

fn edge(
    g: &mut LockGraph,
    from: PackageId,
    name: &str,
    literal: &str,
    b: Behavior,
    to: Option<PackageId>,
) {
    let mut e = DepEdge::new(name, literal, b);
    e.target = to;
    g.packages[from.index()].dependencies.push(e);
}

fn root() -> LockGraph {
    LockGraph::new(Package::new("app", "", Resolution::Root))
}

fn paths(t: &Tree) -> Vec<(String, String)> {
    t.nodes
        .iter()
        .map(|n| (n.path.clone(), n.package.0.to_string()))
        .collect()
}

fn platform() -> Platform {
    Platform {
        os: OsSet::from_list(&["linux"]),
        cpu: CpuSet::from_list(&["x64"]),
        libc: fnpm_types::LibcSet::NONE,
    }
}

fn all() -> HoistOptions {
    HoistOptions::all(platform())
}

fn keys(t: &Tree) -> Vec<&str> {
    t.nodes.iter().map(|n| n.path.as_str()).collect()
}

#[test]
fn conflicting_versions_nest() {
    let mut g = root();
    let a1 = npm(&mut g, "a", "1.0.0");
    let a2 = npm(&mut g, "a", "2.0.0");
    let b = npm(&mut g, "b", "1.0.0");
    edge(
        &mut g,
        PackageId::ROOT,
        "a",
        "^1.0.0",
        Behavior::PROD,
        Some(a1),
    );
    edge(
        &mut g,
        PackageId::ROOT,
        "b",
        "^1.0.0",
        Behavior::PROD,
        Some(b),
    );
    edge(&mut g, b, "a", "^2.0.0", Behavior::PROD, Some(a2));
    let t = hoist(&g, &all());
    assert_eq!(keys(&t), ["a", "b", "b/a"]);
    assert_eq!(t.nodes[2].package, a2);
    assert_eq!(t.nodes[2].parent, Some(1));
    assert_eq!(t.nodes[0].parent, None);
}

#[test]
fn shared_versions_dedupe_and_transitives_hoist() {
    let mut g = root();
    let a = npm(&mut g, "a", "1.0.0");
    let b = npm(&mut g, "b", "1.0.0");
    let c = npm(&mut g, "c", "1.0.0");
    edge(&mut g, PackageId::ROOT, "a", "^1", Behavior::PROD, Some(a));
    edge(&mut g, PackageId::ROOT, "b", "^1", Behavior::PROD, Some(b));
    edge(&mut g, b, "a", "^1", Behavior::PROD, Some(a));
    edge(&mut g, b, "c", "^1", Behavior::PROD, Some(c));
    assert_eq!(keys(&hoist(&g, &all())), ["a", "b", "c"]);
}

#[test]
fn aliases_use_the_alias_as_folder_name() {
    let mut g = root();
    let a = npm(&mut g, "real", "1.0.0");
    edge(
        &mut g,
        PackageId::ROOT,
        "alias",
        "npm:real@^1",
        Behavior::PROD,
        Some(a),
    );
    let t = hoist(&g, &all());
    assert_eq!(keys(&t), ["alias"]);
    assert_eq!(t.nodes[0].name, "alias");
}

#[test]
fn workspaces_get_links_and_their_own_node_modules() {
    let mut g = root();
    let lib = g.add_workspace(Package::new(
        "lib",
        "1.0.0",
        Resolution::Workspace {
            path: "packages/lib".into(),
        },
    ));
    let a1 = npm(&mut g, "a", "1.0.0");
    let a2 = npm(&mut g, "a", "2.0.0");
    edge(&mut g, PackageId::ROOT, "a", "^1", Behavior::PROD, Some(a1));
    edge(&mut g, lib, "a", "^2", Behavior::PROD, Some(a2));
    let t = hoist(&g, &all());
    assert_eq!(keys(&t), ["a", "lib", "lib/a"]);
    assert_eq!(t.workspace_roots, vec![(lib, vec![2])]);
    // `--filter` to the root only: the workspace is not linked.
    let t = hoist(
        &g,
        &HoistOptions {
            workspaces: Some(vec![PackageId::ROOT]),
            ..all()
        },
    );
    assert_eq!(keys(&t), ["a"]);
    // `--filter lib`: the root's own dependencies are skipped, the workspace's are kept.
    let t = hoist(
        &g,
        &HoistOptions {
            workspaces: Some(vec![lib]),
            ..all()
        },
    );
    assert_eq!(keys(&t), ["lib", "lib/a"]);
}

#[test]
fn omitted_groups_and_platform_are_not_installed() {
    let mut g = root();
    let d = npm(&mut g, "dev-only", "1.0.0");
    let o = npm(&mut g, "native", "1.0.0");
    g.packages[o.index()].platform.os = OsSet::from_list(&["darwin"]);
    let p = npm(&mut g, "prod", "1.0.0");
    let pd = npm(&mut g, "prod-dev", "1.0.0");
    edge(
        &mut g,
        PackageId::ROOT,
        "dev-only",
        "1",
        Behavior::DEV,
        Some(d),
    );
    edge(
        &mut g,
        PackageId::ROOT,
        "native",
        "1",
        Behavior::OPTIONAL,
        Some(o),
    );
    edge(
        &mut g,
        PackageId::ROOT,
        "prod",
        "1",
        Behavior::PROD,
        Some(p),
    );
    // A registry package's dev dependencies are never installed.
    edge(&mut g, p, "prod-dev", "1", Behavior::DEV, Some(pd));
    assert_eq!(keys(&hoist(&g, &all())), ["dev-only", "prod"]);
    let prod = HoistOptions {
        include: Behavior::PROD | Behavior::OPTIONAL | Behavior::PEER,
        ..all()
    };
    assert_eq!(keys(&hoist(&g, &prod)), ["prod"]);
    let mac = HoistOptions {
        platform: Platform {
            os: OsSet::from_list(&["darwin"]),
            ..platform()
        },
        ..all()
    };
    assert_eq!(keys(&hoist(&g, &mac)), ["dev-only", "native", "prod"]);
}

#[test]
fn a_peer_is_served_by_the_dependents_sibling() {
    let mut g = root();
    let r17 = npm(&mut g, "react", "17.0.2");
    let r18 = npm(&mut g, "react", "18.2.0");
    let plugin = npm(&mut g, "plugin", "1.0.0");
    let host = npm(&mut g, "host", "1.0.0");
    edge(
        &mut g,
        PackageId::ROOT,
        "react",
        "^18",
        Behavior::PROD,
        Some(r18),
    );
    edge(
        &mut g,
        PackageId::ROOT,
        "host",
        "1",
        Behavior::PROD,
        Some(host),
    );
    edge(&mut g, host, "react", "^17", Behavior::PROD, Some(r17));
    edge(&mut g, host, "plugin", "1", Behavior::PROD, Some(plugin));
    edge(&mut g, plugin, "react", "^17", Behavior::PEER, Some(r17));
    let t = hoist(&g, &all());
    // plugin would see react 18 from the root, so it stays next to host's react 17.
    assert_eq!(keys(&t), ["host", "react", "host/plugin", "host/react"]);
}

#[test]
fn bundled_dependencies_are_not_installed_separately() {
    let mut g = root();
    let a = npm(&mut g, "a", "1.0.0");
    let inner = npm(&mut g, "inner", "1.0.0");
    edge(&mut g, PackageId::ROOT, "a", "1", Behavior::PROD, Some(a));
    edge(
        &mut g,
        a,
        "inner",
        "1",
        Behavior::PROD | Behavior::BUNDLED,
        Some(inner),
    );
    assert_eq!(keys(&hoist(&g, &all())), ["a"]);
}

#[test]
fn unresolved_edges_are_skipped() {
    let mut g = root();
    edge(
        &mut g,
        PackageId::ROOT,
        "missing",
        "1",
        Behavior::OPTIONAL,
        None,
    );
    let t = hoist(&g, &all());
    assert!(t.nodes.is_empty());
}

#[test]
fn endless_nesting_is_reported() {
    let mut g = root();
    let a1 = npm(&mut g, "a", "1.0.0");
    let a2 = npm(&mut g, "a", "2.0.0");
    let b1 = npm(&mut g, "b", "1.0.0");
    let b2 = npm(&mut g, "b", "2.0.0");
    edge(&mut g, PackageId::ROOT, "a", "1", Behavior::PROD, Some(a1));
    edge(&mut g, a1, "b", "1", Behavior::PROD, Some(b1));
    edge(&mut g, b1, "a", "2", Behavior::PROD, Some(a2));
    edge(&mut g, a2, "b", "2", Behavior::PROD, Some(b2));
    edge(&mut g, b2, "a", "1", Behavior::PROD, Some(a1));
    let err = try_hoist(&g, &all()).unwrap_err();
    assert!(
        err.cycle.contains("a@1.0.0") && err.cycle.contains("b@2.0.0"),
        "{}",
        err.cycle
    );
    assert!(!err.partial.nodes.is_empty());
    // `hoist` returns the partial tree instead.
    assert_eq!(hoist(&g, &all()), err.partial);
}

#[test]
fn hoisting_does_not_depend_on_arena_order() {
    let mut g = root();
    let a1 = npm(&mut g, "a", "1.0.0");
    let a2 = npm(&mut g, "a", "2.0.0");
    let b = npm(&mut g, "b", "1.0.0");
    let c = npm(&mut g, "c", "1.0.0");
    edge(&mut g, PackageId::ROOT, "c", "1", Behavior::PROD, Some(c));
    edge(&mut g, PackageId::ROOT, "b", "1", Behavior::PROD, Some(b));
    edge(&mut g, c, "a", "2", Behavior::PROD, Some(a2));
    edge(&mut g, b, "a", "1", Behavior::PROD, Some(a1));
    let before = paths(&hoist(&g, &all()));
    // Same graph with packages pushed in another order and edges reversed.
    let mut h = root();
    let c2 = npm(&mut h, "c", "1.0.0");
    let b2 = npm(&mut h, "b", "1.0.0");
    let a22 = npm(&mut h, "a", "2.0.0");
    let a12 = npm(&mut h, "a", "1.0.0");
    edge(&mut h, b2, "a", "1", Behavior::PROD, Some(a12));
    edge(&mut h, PackageId::ROOT, "b", "1", Behavior::PROD, Some(b2));
    edge(&mut h, c2, "a", "2", Behavior::PROD, Some(a22));
    edge(&mut h, PackageId::ROOT, "c", "1", Behavior::PROD, Some(c2));
    let after: Vec<String> = hoist(&h, &all())
        .nodes
        .iter()
        .map(|n| format!("{}={}", n.path, h.packages[n.package.index()].key()))
        .collect();
    let before: Vec<String> = before
        .iter()
        .map(|(p, id)| format!("{p}={}", g.packages[id.parse::<usize>().unwrap()].key()))
        .collect();
    assert_eq!(before, after);
}

#[test]
fn resolve_paths_walks_up_like_node() {
    let mut p: Vec<(String, PackageId)> = [
        ("a", 1),
        ("b", 2),
        ("b/a", 3),
        ("@s/x", 4),
        ("@s/x/a", 5),
        ("@s/x/@t/y", 6),
        ("@s/x/@t/y/z", 7),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), PackageId(v)))
    .collect();
    p.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(resolve_paths(&p, "", "a"), Some(PackageId(1)));
    assert_eq!(resolve_paths(&p, "b", "a"), Some(PackageId(3)));
    assert_eq!(resolve_paths(&p, "b/a", "a"), Some(PackageId(3)));
    assert_eq!(resolve_paths(&p, "b/a", "b"), Some(PackageId(2)));
    assert_eq!(resolve_paths(&p, "@s/x", "a"), Some(PackageId(5)));
    assert_eq!(resolve_paths(&p, "@s/x/@t/y", "a"), Some(PackageId(5)));
    assert_eq!(resolve_paths(&p, "@s/x/@t/y/z", "b"), Some(PackageId(2)));
    assert_eq!(resolve_paths(&p, "@s/x/@t/y", "z"), Some(PackageId(7)));
    assert_eq!(resolve_paths(&p, "b", "missing"), None);
}

#[test]
fn parent_paths() {
    assert_eq!(parent_path("a"), Some(""));
    assert_eq!(parent_path("a/b"), Some("a"));
    assert_eq!(parent_path("@s/a"), Some(""));
    assert_eq!(parent_path("x/@s/a"), Some("x"));
    assert_eq!(parent_path("@s/a/b"), Some("@s/a"));
    assert_eq!(parent_path("x/b@1"), None);
    assert_eq!(parent_path("./ws/a"), Some("./ws"));
}
