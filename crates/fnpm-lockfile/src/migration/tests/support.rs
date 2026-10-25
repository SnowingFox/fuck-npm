//! Shared test helpers: a small node-semver, workspaces built without `fnpm-workspace`'s
//! loader, graph normalisation and trees rebuilt from lockfile paths.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fnpm_types::{Behavior, DependencyGroup, LockGraph, PackageId, Resolution};
use fnpm_workspace::{Member, PackageJson, Workspace, WorkspaceSettings};
use serde_json::Value;

use crate::migration::Semver;
use crate::tree::{Tree, TreeNode};

// ------------------------------------------------------------------------------------------
// A test-only node-semver (the real one is `fnpm-semver`).
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V {
    nums: [u64; 3],
    pre: Vec<String>,
}

impl V {
    pub(crate) fn parse(s: &str) -> Option<V> {
        let s = s.trim();
        let s = s.strip_prefix('=').unwrap_or(s).trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, p.split('.').map(str::to_owned).collect()),
            None => (s, Vec::new()),
        };
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let mut nums = [0u64; 3];
        for (n, p) in nums.iter_mut().zip(parts) {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *n = p.parse().ok()?;
        }
        Some(V { nums, pre })
    }
}

impl PartialOrd for V {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for V {
    fn cmp(&self, other: &Self) -> Ordering {
        self.nums
            .cmp(&other.nums)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let o = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

type Cmp = (Ordering, bool, V); // (direction, inclusive, version): v <dir> version

/// A partial version `1`, `1.2`, `1.x`, `*`: the given numbers.
fn partial(s: &str) -> Option<(Vec<u64>, Vec<String>)> {
    let s = s.trim().trim_start_matches('=').trim_start_matches('v');
    let s = s.split('+').next().unwrap_or("");
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, p.split('.').map(str::to_owned).collect()),
        None => (s, Vec::new()),
    };
    let mut nums = Vec::new();
    for p in core.split('.') {
        if matches!(p, "x" | "X" | "*" | "") {
            break;
        }
        nums.push(p.parse().ok()?);
    }
    Some((nums, pre))
}

fn v(nums: [u64; 3], pre: Vec<String>) -> V {
    V { nums, pre }
}

fn zero_pre() -> Vec<String> {
    vec!["0".to_owned()]
}

fn comparators(set: &str) -> Option<Vec<Cmp>> {
    let set = set.trim();
    if let Some((lo, hi)) = set.split_once(" - ") {
        let (l, lpre) = partial(lo)?;
        let (h, hpre) = partial(hi)?;
        let mut out = vec![];
        let mut ln = [0; 3];
        for (i, n) in l.iter().enumerate() {
            ln[i] = *n;
        }
        out.push((Ordering::Greater, true, v(ln, lpre)));
        match h.len() {
            3 => out.push((Ordering::Less, true, v([h[0], h[1], h[2]], hpre))),
            2 => out.push((Ordering::Less, false, v([h[0], h[1] + 1, 0], zero_pre()))),
            1 => out.push((Ordering::Less, false, v([h[0] + 1, 0, 0], zero_pre()))),
            _ => {}
        }
        return Some(out);
    }
    let mut tokens: Vec<String> = Vec::new();
    let mut pending_op = String::new();
    for t in set.split_whitespace() {
        if matches!(t, ">" | ">=" | "<" | "<=" | "=" | "^" | "~") {
            pending_op = t.to_owned();
            continue;
        }
        tokens.push(format!("{pending_op}{t}"));
        pending_op.clear();
    }
    let mut out = Vec::new();
    for t in tokens {
        let (op, rest) = ["<=", ">=", "<", ">", "~>", "~", "^", "="]
            .iter()
            .find_map(|op| t.strip_prefix(op).map(|r| (*op, r)))
            .unwrap_or(("", t.as_str()));
        let (n, pre) = partial(rest)?;
        let full = |n: &[u64]| {
            [
                n.first().copied().unwrap_or(0),
                n.get(1).copied().unwrap_or(0),
                n.get(2).copied().unwrap_or(0),
            ]
        };
        match op {
            "^" => {
                let f = full(&n);
                out.push((Ordering::Greater, true, v(f, pre)));
                let upper = if f[0] > 0 || n.len() == 1 {
                    [f[0] + 1, 0, 0]
                } else if f[1] > 0 || n.len() == 2 {
                    [0, f[1] + 1, 0]
                } else {
                    [0, 0, f[2] + 1]
                };
                if !n.is_empty() {
                    out.push((Ordering::Less, false, v(upper, zero_pre())));
                }
            }
            "~" | "~>" => {
                let f = full(&n);
                out.push((Ordering::Greater, true, v(f, pre)));
                let upper = if n.len() <= 1 {
                    [f[0] + 1, 0, 0]
                } else {
                    [f[0], f[1] + 1, 0]
                };
                if !n.is_empty() {
                    out.push((Ordering::Less, false, v(upper, zero_pre())));
                }
            }
            ">" | ">=" | "<" | "<=" => {
                let incl = op.ends_with('=');
                let dir = if op.starts_with('>') {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
                if n.len() == 3 {
                    out.push((dir, incl, v(full(&n), pre)));
                } else if n.is_empty() {
                    if dir == Ordering::Less {
                        out.push((Ordering::Less, false, v([0, 0, 0], zero_pre())));
                    }
                } else {
                    let f = full(&n);
                    let bumped = if n.len() == 1 {
                        [f[0] + 1, 0, 0]
                    } else {
                        [f[0], f[1] + 1, 0]
                    };
                    match (dir, incl) {
                        (Ordering::Greater, true) => {
                            out.push((Ordering::Greater, true, v(f, vec![])))
                        }
                        (Ordering::Greater, false) => {
                            out.push((Ordering::Greater, true, v(bumped, vec![])))
                        }
                        (_, true) => out.push((Ordering::Less, false, v(bumped, zero_pre()))),
                        (_, false) => out.push((Ordering::Less, false, v(f, zero_pre()))),
                    }
                }
            }
            _ => {
                if n.len() == 3 {
                    let f = full(&n);
                    out.push((Ordering::Greater, true, v(f, pre.clone())));
                    out.push((Ordering::Less, true, v(f, pre)));
                } else if !n.is_empty() {
                    let f = full(&n);
                    let upper = if n.len() == 1 {
                        [f[0] + 1, 0, 0]
                    } else {
                        [f[0], f[1] + 1, 0]
                    };
                    out.push((Ordering::Greater, true, v(f, vec![])));
                    out.push((Ordering::Less, false, v(upper, zero_pre())));
                }
            }
        }
    }
    Some(out)
}

fn set_matches(version: &V, set: &[Cmp]) -> bool {
    let ok = set.iter().all(|(dir, incl, c)| {
        let o = version.cmp(c);
        o == *dir || (*incl && o == Ordering::Equal)
    });
    if !ok {
        return false;
    }
    if version.pre.is_empty() {
        return true;
    }
    set.iter()
        .any(|(_, _, c)| !c.pre.is_empty() && c.pre != zero_pre() && c.nums == version.nums)
}

pub(crate) struct TestSemver;

impl Semver for TestSemver {
    fn satisfies(&self, version: &str, range: &str) -> bool {
        let Some(version) = V::parse(version) else {
            return false;
        };
        range
            .split("||")
            .any(|set| comparators(set).is_some_and(|c| set_matches(&version, &c)))
    }

    fn compare(&self, a: &str, b: &str) -> Ordering {
        match (V::parse(a), V::parse(b)) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => a.cmp(b),
        }
    }

    fn is_valid(&self, version: &str) -> bool {
        V::parse(version).is_some()
    }
}

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

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A workspace from a directory on disk, globbing `workspaces` (`dir`, `dir/*`, `dir/**`)
/// the way the fixtures use it.
pub(crate) fn workspace_from_dir(dir: &Path) -> Workspace {
    let root = read_json(&dir.join("package.json")).unwrap_or_else(|| serde_json::json!({}));
    let patterns: Vec<String> = match root.get("workspaces") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::Object(o)) => o
            .get("packages")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut paths: BTreeSet<String> = BTreeSet::new();
    fn walk(base: &Path, rel: &str, recursive: bool, out: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(base.join(rel)) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "node_modules" || name.starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if entry.path().join("package.json").is_file() {
                out.insert(child.clone());
            }
            if recursive {
                walk(base, &child, true, out);
            }
        }
    }
    for pattern in patterns {
        if let Some(prefix) = pattern.strip_suffix("/**") {
            walk(dir, prefix, true, &mut paths);
        } else if let Some(prefix) = pattern.strip_suffix("/*") {
            walk(dir, prefix, false, &mut paths);
        } else if dir.join(&pattern).join("package.json").is_file() {
            paths.insert(pattern.trim_end_matches('/').to_owned());
        }
    }
    let members: Vec<(String, Value)> = paths
        .into_iter()
        .filter_map(|p| Some((p.clone(), read_json(&dir.join(&p).join("package.json"))?)))
        .collect();
    let refs: Vec<(&str, Value)> = members
        .iter()
        .map(|(p, v)| (p.as_str(), v.clone()))
        .collect();
    let mut ws = workspace(root, &refs);
    ws.root_dir = dir.to_owned();
    ws
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
