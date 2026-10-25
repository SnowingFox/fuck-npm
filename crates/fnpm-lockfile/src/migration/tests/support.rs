//! Shared test helpers: a small node-semver, workspaces built without `fnpm-workspace`'s
//! loader, graph normalisation and trees rebuilt from lockfile paths.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fnpm_types::{Behavior, DependencyGroup, LockGraph, PackageId, Resolution};
use fnpm_workspace::{Member, PackageJson, Workspace, WorkspaceSettings};
use serde_json::Value;

use crate::tree::{Tree, TreeNode};

// ------------------------------------------------------------------------------------------
// Workspaces
// ------------------------------------------------------------------------------------------

pub(crate) fn manifest(raw: Value) -> PackageJson {
    let group = |key: &str| -> Vec<(String, String)> {
        raw.get(key)
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    };
    PackageJson {
        name: raw.get("name").and_then(Value::as_str).map(str::to_owned),
        version: raw
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        dependencies: group("dependencies"),
        dev_dependencies: group("devDependencies"),
        optional_dependencies: group("optionalDependencies"),
        peer_dependencies: group("peerDependencies"),
        raw,
        ..PackageJson::default()
    }
}

pub(crate) fn workspace(root: Value, members: &[(&str, Value)]) -> Workspace {
    Workspace {
        root_dir: PathBuf::from("/project"),
        root: manifest(root),
        members: members
            .iter()
            .map(|(path, raw)| Member {
                name: raw
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                version: raw
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                path: (*path).to_owned(),
                dir: PathBuf::from("/project").join(path),
                manifest: manifest(raw.clone()),
            })
            .collect(),
        settings: WorkspaceSettings::default(),
    }
}

pub(crate) fn empty_workspace() -> Workspace {
    workspace(serde_json::json!({}), &[])
}

/// A workspace from a directory on disk, globbing `workspaces` (`dir`, `dir/*`, `dir/**`)
/// the way the fixtures use it.
pub(crate) fn workspace_from_dir(dir: &Path) -> Workspace {
    // The real discovery, in the mode lockfile migration uses (missing members are skipped).
    let opts = fnpm_workspace::LoadOptions {
        missing: fnpm_workspace::MissingWorkspace::Skip,
    };
    Workspace::load_with(dir, &opts)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .0
}

// ------------------------------------------------------------------------------------------
// Graph comparison
// ------------------------------------------------------------------------------------------

/// A comparable view of one package: everything the lockfiles both record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Norm {
    pub integrity: Option<String>,
    pub tarball: Option<String>,
    pub bin: String,
    pub platform: String,
    pub has_install_script: bool,
    /// `(name, literal, group, optional peer, bundled, target key)`.
    pub edges: BTreeSet<(String, String, String, bool, bool, Option<String>)>,
}

pub(crate) fn key(graph: &LockGraph, id: PackageId) -> String {
    let p = &graph.packages[id.index()];
    match &p.resolution {
        Resolution::Root => "<root>".to_owned(),
        r => format!("{}@{r}", p.name),
    }
}

/// Packages reachable from the root and the workspaces over every edge, keyed by
/// `name@resolution`.
pub(crate) fn normalize(graph: &LockGraph) -> BTreeMap<String, Norm> {
    let all =
        Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER | Behavior::WORKSPACE;
    let mut seen = vec![false; graph.packages.len()];
    let mut stack: Vec<PackageId> = graph.workspaces.values().copied().collect();
    while let Some(id) = stack.pop() {
        if std::mem::replace(&mut seen[id.index()], true) {
            continue;
        }
        for e in &graph.packages[id.index()].dependencies {
            if let Some(t) = e.target
                && e.behavior.intersects(all)
            {
                stack.push(t);
            }
        }
    }
    let mut out = BTreeMap::new();
    for (i, p) in graph.packages.iter().enumerate() {
        if !seen[i] {
            continue;
        }
        let edges = p
            .dependencies
            .iter()
            .map(|e| {
                let group = match e.behavior.group() {
                    DependencyGroup::Prod => "prod",
                    DependencyGroup::Dev => "dev",
                    DependencyGroup::Optional => "optional",
                    DependencyGroup::Peer => "peer",
                };
                (
                    e.name.clone(),
                    e.literal.clone(),
                    group.to_owned(),
                    e.behavior.is_optional_peer(),
                    e.behavior.is_bundled(),
                    e.target.map(|t| key(graph, t)),
                )
            })
            .collect();
        let tarball = match &p.resolution {
            Resolution::Npm { tarball_url, .. } => tarball_url.clone(),
            _ => None,
        };
        out.insert(
            key(graph, PackageId(i as u32)),
            Norm {
                integrity: p.resolution.integrity().map(|i| i.to_sri()),
                tarball,
                bin: format!("{:?}", p.bin),
                platform: format!("{:?}", p.platform),
                has_install_script: p.meta.needs_scripts(),
                edges,
            },
        );
    }
    out
}

/// A readable report of the differences between two normalised graphs (empty when equal).
pub(crate) fn diff_norm(a: &BTreeMap<String, Norm>, b: &BTreeMap<String, Norm>) -> Vec<String> {
    let mut out = Vec::new();
    for (k, va) in a {
        match b.get(k) {
            None => out.push(format!("only in ours: {k}")),
            Some(vb) if va != vb => {
                if va.integrity != vb.integrity {
                    out.push(format!(
                        "{k}: integrity {:?} vs {:?}",
                        va.integrity, vb.integrity
                    ));
                }
                if va.tarball != vb.tarball {
                    out.push(format!("{k}: tarball {:?} vs {:?}", va.tarball, vb.tarball));
                }
                if va.bin != vb.bin {
                    out.push(format!("{k}: bin {} vs {}", va.bin, vb.bin));
                }
                if va.platform != vb.platform {
                    out.push(format!("{k}: platform {} vs {}", va.platform, vb.platform));
                }
                if va.has_install_script != vb.has_install_script {
                    out.push(format!(
                        "{k}: hasInstallScript {} vs {}",
                        va.has_install_script, vb.has_install_script
                    ));
                }
                for e in va.edges.difference(&vb.edges) {
                    out.push(format!("{k}: edge only in ours {e:?}"));
                }
                for e in vb.edges.difference(&va.edges) {
                    out.push(format!("{k}: edge only in expected {e:?}"));
                }
            }
            _ => {}
        }
    }
    for k in b.keys() {
        if !a.contains_key(k) {
            out.push(format!("only in expected: {k}"));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------
// Trees from lockfile paths
// ------------------------------------------------------------------------------------------

/// The folder name at the end of a bun.lock path: the last component, or the last two for a
/// scoped name.
fn last_name(path: &str) -> &str {
    let Some(slash) = path.rfind('/') else {
        return path;
    };
    let parent = &path[..slash];
    let start = parent.rfind('/').map_or(0, |i| i + 1);
    if parent[start..].starts_with('@') {
        &path[start..]
    } else {
        &path[slash + 1..]
    }
}

/// The tree a bun.lock describes: each key is a node; its parent is the node at the key
/// without its last name. Keys under `./<workspace>` (an unlinked workspace) are workspace
/// roots.
pub(crate) fn tree_from_bun_paths(graph: &LockGraph, paths: &[(String, PackageId)]) -> Tree {
    let mut tree = Tree::default();
    let mut index: BTreeMap<&str, u32> = BTreeMap::new();
    let mut sorted: Vec<&(String, PackageId)> = paths.iter().collect();
    sorted.sort_by_key(|(p, _)| p.matches('/').count());
    let mut roots: BTreeMap<PackageId, Vec<u32>> = BTreeMap::new();
    for (path, id) in sorted {
        let name = last_name(path);
        let parent_path = path
            .strip_suffix(name)
            .map(|p| p.trim_end_matches('/'))
            .unwrap_or("");
        let parent = index.get(parent_path).copied();
        let n = tree.nodes.len() as u32;
        if parent.is_none() && !parent_path.is_empty() {
            let ws_name = parent_path.trim_start_matches("./");
            if let Some(ws) = graph
                .workspaces
                .values()
                .find(|w| graph.packages[w.index()].name == ws_name)
            {
                roots.entry(*ws).or_default().push(n);
            }
        }
        tree.nodes.push(TreeNode {
            package: *id,
            name: name.to_owned(),
            parent,
            path: path.clone(),
        });
        index.insert(path, n);
    }
    tree.workspace_roots = roots.into_iter().collect();
    tree
}

/// The tree a package-lock.json describes, with bun.lock-style paths (`a/b`,
/// `<workspace name>/c`).
pub(crate) fn tree_from_npm_locations(
    graph: &LockGraph,
    locations: &[(String, PackageId)],
) -> Tree {
    let ws_by_path: BTreeMap<&str, PackageId> = graph
        .workspaces
        .iter()
        .map(|(p, id)| (p.as_str(), *id))
        .collect();
    let mut converted: Vec<(String, PackageId)> = Vec::new();
    for (location, id) in locations {
        let Some(at) = location.find("node_modules/") else {
            continue;
        };
        let base = location[..at].trim_end_matches('/');
        let rest = location[at + "node_modules/".len()..].replace("/node_modules/", "/");
        let path = if base.is_empty() {
            rest
        } else if let Some(ws) = ws_by_path.get(base) {
            format!("{}/{rest}", graph.packages[ws.index()].name)
        } else {
            continue;
        };
        converted.push((path, *id));
    }
    tree_from_bun_paths(graph, &converted)
}

// ------------------------------------------------------------------------------------------
// Fixtures
// ------------------------------------------------------------------------------------------

/// bun's repository, for its test fixtures.
pub(crate) fn bun_dir() -> Option<PathBuf> {
    let dir = match std::env::var_os("FNPM_BUN_DIR") {
        Some(d) => PathBuf::from(d),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../submodule/bun"),
    };
    dir.join("test/cli/install").is_dir().then_some(dir)
}

pub(crate) fn install_tests() -> Option<PathBuf> {
    let dir = bun_dir().map(|d| d.join("test/cli/install"));
    if dir.is_none() {
        eprintln!("bun fixtures not found: set FNPM_BUN_DIR or check out submodule/bun");
    }
    dir
}

/// The snapshots of a bun `.snap` file, by test name.
pub(crate) fn snapshots(path: &Path) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = BTreeMap::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("exports[`") {
        rest = &rest[start + 9..];
        let Some(name_end) = rest.find("`] = `") else {
            break;
        };
        let name = rest[..name_end].to_owned();
        rest = &rest[name_end + 6..];
        let Some(end) = rest.find("\n`;\n") else {
            break;
        };
        let mut body = String::with_capacity(end);
        let mut chars = rest[..end].chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    body.push(next);
                }
            } else {
                body.push(c);
            }
        }
        out.insert(name, body);
        rest = &rest[end..];
    }
    out
}
