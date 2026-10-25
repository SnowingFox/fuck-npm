//! Comparing a lockfile with package.json (`--frozen-lockfile`, `fnpm-lock verify`) and with
//! another lockfile (`fnpm-lock diff`).

use std::collections::{BTreeMap, BTreeSet};

use fnpm_types::{Behavior, DependencyGroup, LockGraph, OverrideSelector, Package, Resolution};
use fnpm_workspace::{PackageJson, Workspace};
use serde_json::Value;

use crate::hash::resolution_text;

/// What changed between two lockfiles, by package name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockDiff {
    /// `name@resolution` of packages only in the second graph.
    pub added: Vec<String>,
    /// `name@resolution` of packages only in the first graph.
    pub removed: Vec<String>,
    /// `(name, from, to)` for a name with one package on each side.
    pub changed: Vec<(String, String, String)>,
}

impl LockDiff {
    /// Nothing changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

fn versions(g: &LockGraph) -> BTreeMap<&str, BTreeSet<String>> {
    let mut m: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for p in g.packages.iter().skip(1) {
        if matches!(
            p.resolution,
            Resolution::Root | Resolution::Workspace { .. }
        ) {
            continue;
        }
        m.entry(p.name.as_str())
            .or_default()
            .insert(resolution_text(&p.resolution));
    }
    m
}

/// `fnpm-lock diff`: packages added, removed and changed from `a` to `b`, sorted by name.
pub fn diff(a: &LockGraph, b: &LockGraph) -> LockDiff {
    let (va, vb) = (versions(a), versions(b));
    let mut d = LockDiff::default();
    let names: BTreeSet<&str> = va.keys().chain(vb.keys()).copied().collect();
    let empty = BTreeSet::new();
    for name in names {
        let (x, y) = (
            va.get(name).unwrap_or(&empty),
            vb.get(name).unwrap_or(&empty),
        );
        if x == y {
            continue;
        }
        if x.len() == 1 && y.len() == 1 {
            let (from, to) = (
                x.iter().next().cloned().unwrap_or_default(),
                y.iter().next().cloned().unwrap_or_default(),
            );
            d.changed.push((name.to_owned(), from, to));
            continue;
        }
        d.removed
            .extend(x.difference(y).map(|v| format!("{name}@{v}")));
        d.added
            .extend(y.difference(x).map(|v| format!("{name}@{v}")));
    }
    d
}

/// `(group, name)` to specifier, with npm's precedence (an optional dependency replaces the
/// production one of the same name).
fn declared_from_json(pj: &PackageJson) -> BTreeMap<(DependencyGroup, String), String> {
    let mut m = BTreeMap::new();
    let optional: BTreeSet<&str> = pj
        .optional_dependencies
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    for g in DependencyGroup::ALL {
        for (name, spec) in pj.group(g) {
            if g == DependencyGroup::Prod && optional.contains(name.as_str()) {
                continue;
            }
            m.insert((g, name.clone()), spec.clone());
        }
    }
    m
}

fn declared_from_graph(p: &Package) -> BTreeMap<(DependencyGroup, String), String> {
    let groups = Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER;
    p.dependencies
        .iter()
        .filter(|e| e.behavior.intersects(groups))
        .map(|e| ((e.behavior.group(), e.name.clone()), e.literal.clone()))
        .collect()
}

fn optional_peers_json(pj: &PackageJson) -> BTreeSet<String> {
    let peers: BTreeSet<&str> = pj
        .peer_dependencies
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    pj.peer_dependencies_meta
        .iter()
        .filter(|(n, m)| m.optional && peers.contains(n.as_str()))
        .map(|(n, _)| n.clone())
        .collect()
}

fn optional_peers_graph(p: &Package) -> BTreeSet<String> {
    p.dependencies
        .iter()
        .filter(|e| e.behavior.is_optional_peer())
        .map(|e| e.name.clone())
        .collect()
}

/// The first difference between declared and locked dependencies of one package, in bun's
/// `ChangedDependency` wording.
fn dependency_change(pj: &PackageJson, p: &Package, workspace: &str) -> Option<String> {
    let want = declared_from_json(pj);
    let have = declared_from_graph(p);
    let suffix = if workspace.is_empty() {
        String::new()
    } else {
        format!(" in {workspace}")
    };
    let names: BTreeSet<&str> = want
        .keys()
        .chain(have.keys())
        .map(|(_, n)| n.as_str())
        .collect();
    for name in names {
        let w: Vec<(&DependencyGroup, &String)> = want
            .iter()
            .filter(|((_, n), _)| n == name)
            .map(|((g, _), v)| (g, v))
            .collect();
        let h: Vec<(&DependencyGroup, &String)> = have
            .iter()
            .filter(|((_, n), _)| n == name)
            .map(|((g, _), v)| (g, v))
            .collect();
        if w == h {
            continue;
        }
        return Some(match (w.is_empty(), h.is_empty()) {
            (false, true) => format!("{name} was added{suffix}"),
            (true, false) => format!("{name} was removed{suffix}"),
            _ => {
                let wg: Vec<_> = w.iter().map(|(g, _)| **g).collect();
                let hg: Vec<_> = h.iter().map(|(g, _)| **g).collect();
                if wg != hg {
                    format!("{name} is in different dependency groups{suffix}")
                } else {
                    let (now, before) = w
                        .iter()
                        .zip(&h)
                        .find(|(a, b)| a.1 != b.1)
                        .map_or(("", ""), |(a, b)| (a.1.as_str(), b.1.as_str()));
                    format!("{name} changed from {before} to {now}{suffix}")
                }
            }
        });
    }
    let (wo, ho) = (optional_peers_json(pj), optional_peers_graph(p));
    if let Some(name) = wo.symmetric_difference(&ho).next() {
        return Some(format!("{name} changed in peerDependenciesMeta{suffix}"));
    }
    None
}

/// npm `overrides` (nested one level), else yarn `resolutions`, plus `pnpm-workspace.yaml`
/// overrides, as normalised `(key, value)` pairs.
fn declared_overrides(ws: &Workspace) -> (BTreeSet<(String, String)>, &'static str) {
    let mut out = BTreeSet::new();
    let root_dep = |name: &str| -> Option<String> {
        DependencyGroup::ALL.iter().find_map(|g| {
            ws.root
                .group(*g)
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        })
    };
    let mut add = |key: &str, value: &str| {
        let value = match value.strip_prefix('$') {
            Some(name) => root_dep(name).unwrap_or_else(|| value.to_owned()),
            None => value.to_owned(),
        };
        if let Ok(Some(sel)) = OverrideSelector::parse(key) {
            out.insert((sel.to_key(), value));
        }
    };
    let (field, label) = match ws.root.field("overrides") {
        Some(v @ Value::Object(_)) => (Some(v), "overrides"),
        _ => (ws.root.field("resolutions"), "resolutions"),
    };
    if let Some(Value::Object(map)) = field {
        for (k, v) in map {
            match v {
                Value::String(s) => add(k, s),
                Value::Object(children) => {
                    for (ck, cv) in children {
                        let Value::String(cv) = cv else { continue };
                        if ck == "." {
                            add(k, cv);
                        } else {
                            add(&format!("{k}>{ck}"), cv);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    for (k, v) in &ws.settings.overrides {
        add(k, v);
    }
    (out, label)
}

fn locked_overrides(g: &LockGraph) -> BTreeSet<(String, String)> {
    g.overrides
        .iter()
        .map(|r| (r.selector.to_key(), r.value.clone()))
        .collect()
}

fn json_string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    match v {
        Some(Value::Object(m)) => m
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

fn declared_catalogs(ws: &Workspace) -> fnpm_types::Catalogs {
    let mut c = ws.settings.catalogs.clone();
    let sources = [
        ws.root.field("workspaces").filter(|w| w.is_object()),
        Some(&ws.root.raw),
    ];
    for src in sources.into_iter().flatten() {
        for (k, v) in json_string_map(src.get("catalog")) {
            c.default.entry(k).or_insert(v);
        }
        if let Some(Value::Object(named)) = src.get("catalogs") {
            for (name, entries) in named {
                let group = c.named.entry(name.clone()).or_default();
                for (k, v) in json_string_map(Some(entries)) {
                    group.entry(k).or_insert(v);
                }
            }
        }
    }
    c
}

/// Why a lockfile is out of date with the workspace's package.json files (frozen check,
/// `fnpm-lock verify`). Empty means in sync. Each entry is a sentence like bun's
/// `--frozen-lockfile` note: `overrides in package.json changed since fnpm.lock was saved`,
/// `dependencies in package.json changed since fnpm.lock was saved: react changed from ^17
/// to ^18 in packages/app`.
pub fn out_of_sync(graph: &LockGraph, workspace: &Workspace) -> Vec<String> {
    const SINCE: &str = "in package.json changed since fnpm.lock was saved";
    let mut out = Vec::new();
    let (declared, label) = declared_overrides(workspace);
    if declared != locked_overrides(graph) {
        out.push(format!("{label} {SINCE}"));
    }
    if declared_catalogs(workspace) != graph.catalogs {
        out.push(format!("the catalog {SINCE}"));
    }
    let patched: BTreeMap<String, String> =
        json_string_map(workspace.root.field("patchedDependencies"));
    let locked_patched: BTreeMap<String, String> = graph
        .patched_dependencies
        .iter()
        .map(|(k, v)| (k.clone(), v.path.clone()))
        .collect();
    if patched != locked_patched {
        out.push(format!("patchedDependencies {SINCE}"));
    }

    let locked_paths: BTreeSet<&str> = graph
        .workspaces
        .keys()
        .filter(|p| !p.is_empty())
        .map(String::as_str)
        .collect();
    let declared_paths: BTreeSet<&str> =
        workspace.members.iter().map(|m| m.path.as_str()).collect();
    for p in declared_paths.difference(&locked_paths) {
        out.push(format!("workspaces {SINCE}: {p} was added"));
    }
    for p in locked_paths.difference(&declared_paths) {
        out.push(format!("workspaces {SINCE}: {p} was removed"));
    }

    if let Some(change) = dependency_change(&workspace.root, graph.root(), "") {
        out.push(format!("dependencies {SINCE}: {change}"));
    }
    for m in &workspace.members {
        let Some(id) = graph.workspaces.get(&m.path) else {
            continue;
        };
        let Some(p) = graph.get(*id) else { continue };
        if p.name != m.name {
            out.push(format!(
                "workspaces {SINCE}: {} was renamed to {}",
                m.path, m.name
            ));
            continue;
        }
        if let Some(change) = dependency_change(&m.manifest, p, &m.path) {
            out.push(format!("dependencies {SINCE}: {change}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnpm_types::{DepEdge, PeerMeta};
    use fnpm_workspace::{Member, WorkspaceSettings};
    use std::path::PathBuf;

    fn pj(deps: &[(&str, &str)], dev: &[(&str, &str)]) -> PackageJson {
        PackageJson {
            dependencies: deps
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            dev_dependencies: dev
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            raw: serde_json::json!({}),
            ..Default::default()
        }
    }

    fn graph() -> LockGraph {
        let mut g = LockGraph::new(Package::new("app", "", Resolution::Root));
        g.root_mut().dependencies = vec![
            DepEdge::new("a", "^1.0.0", Behavior::PROD),
            DepEdge::new("d", "^2", Behavior::DEV),
        ];
        let mut lib = Package::new(
            "lib",
            "1.0.0",
            Resolution::Workspace {
                path: "packages/lib".into(),
            },
        );
        lib.dependencies = vec![DepEdge::new("p", "^3", Behavior::PEER | Behavior::OPTIONAL)];
        lib.peer_meta
            .insert("p".into(), PeerMeta { optional: true });
        g.add_workspace(lib);
        g
    }

    fn ws(root: PackageJson, lib: PackageJson) -> Workspace {
        Workspace {
            root_dir: PathBuf::from("/p"),
            root,
            members: vec![Member {
                name: "lib".into(),
                version: "1.0.0".into(),
                path: "packages/lib".into(),
                dir: PathBuf::from("/p/packages/lib"),
                manifest: lib,
            }],
            settings: WorkspaceSettings::default(),
        }
    }

    fn lib_pj() -> PackageJson {
        let mut l = pj(&[], &[]);
        l.peer_dependencies = vec![("p".into(), "^3".into())];
        l.peer_dependencies_meta
            .insert("p".into(), PeerMeta { optional: true });
        l
    }

    #[test]
    fn in_sync() {
        let w = ws(pj(&[("a", "^1.0.0")], &[("d", "^2")]), lib_pj());
        assert_eq!(out_of_sync(&graph(), &w), Vec::<String>::new());
    }

    #[test]
    fn reports_changes_like_bun() {
        let w = ws(pj(&[("a", "^1.1.0")], &[("d", "^2")]), lib_pj());
        assert_eq!(
            out_of_sync(&graph(), &w),
            [
                "dependencies in package.json changed since fnpm.lock was saved: a changed from ^1.0.0 to ^1.1.0"
            ]
        );
        let w = ws(pj(&[("a", "^1.0.0"), ("b", "1")], &[("d", "^2")]), lib_pj());
        assert!(out_of_sync(&graph(), &w)[0].ends_with("b was added"));
        let w = ws(pj(&[], &[("a", "^1.0.0"), ("d", "^2")]), lib_pj());
        assert!(out_of_sync(&graph(), &w)[0].ends_with("a is in different dependency groups"));
        let mut lib = lib_pj();
        lib.peer_dependencies_meta.clear();
        let w = ws(pj(&[("a", "^1.0.0")], &[("d", "^2")]), lib);
        assert!(
            out_of_sync(&graph(), &w)[0]
                .ends_with("p changed in peerDependenciesMeta in packages/lib")
        );
        let mut root = pj(&[("a", "^1.0.0")], &[("d", "^2")]);
        root.raw =
            serde_json::json!({ "overrides": { "x": "1.0.0" }, "catalog": { "react": "^18" } });
        let w = ws(root, lib_pj());
        let r = out_of_sync(&graph(), &w);
        assert_eq!(
            r[0],
            "overrides in package.json changed since fnpm.lock was saved"
        );
        assert_eq!(
            r[1],
            "the catalog in package.json changed since fnpm.lock was saved"
        );
    }

    #[test]
    fn workspaces_added_and_removed() {
        let mut w = ws(pj(&[("a", "^1.0.0")], &[("d", "^2")]), lib_pj());
        w.members[0].path = "packages/other".into();
        let r = out_of_sync(&graph(), &w);
        assert!(r.iter().any(|s| s.ends_with("packages/other was added")));
        assert!(r.iter().any(|s| s.ends_with("packages/lib was removed")));
    }

    #[test]
    fn diffs_graphs() {
        let mut a = LockGraph::new(Package::new("app", "", Resolution::Root));
        a.push(Package::new("x", "1.0.0", Resolution::npm("1.0.0", None)));
        a.push(Package::new("y", "1.0.0", Resolution::npm("1.0.0", None)));
        let mut b = LockGraph::new(Package::new("app", "", Resolution::Root));
        b.push(Package::new("x", "2.0.0", Resolution::npm("2.0.0", None)));
        b.push(Package::new("z", "1.0.0", Resolution::npm("1.0.0", None)));
        let d = diff(&a, &b);
        assert_eq!(d.changed, [("x".into(), "1.0.0".into(), "2.0.0".into())]);
        assert_eq!(d.added, ["z@1.0.0"]);
        assert_eq!(d.removed, ["y@1.0.0"]);
        assert!(diff(&a, &a).is_empty());
    }
}
