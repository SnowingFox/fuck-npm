//! Shared helpers for the migration tests: the project's `Workspace`, a
//! filesystem view that stays inside the fixture, and readers for bun's `.snap` files.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fnpm_lockfile::formats::foreign::{MigrateOptions, ProjectFiles};
use fnpm_types::{LockGraph, Package, PackageId, Resolution};
use fnpm_workspace::{PackageJson, Workspace};
use serde_json::Value;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Reads only files inside the fixture, so `~/.yarnrc.yml` and parent folders never leak in.
pub struct ScopedFiles(pub PathBuf);

impl ProjectFiles for ScopedFiles {
    fn read(&self, path: &Path) -> Option<String> {
        if !path.starts_with(&self.0) {
            return None;
        }
        std::fs::read_to_string(path).ok()
    }
}

/// In-memory files: path relative to the fixture root to contents.
pub struct MemFiles {
    pub root: PathBuf,
    pub files: BTreeMap<String, String>,
}

impl ProjectFiles for MemFiles {
    fn read(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        self.files.get(rel.to_str()?).cloned()
    }
}

pub fn manifest(raw: Value) -> PackageJson {
    PackageJson {
        name: raw.get("name").and_then(Value::as_str).map(str::to_owned),
        version: raw
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        raw,
        ..Default::default()
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn patterns(root: &Value, dir: &Path) -> Vec<String> {
    let from_json = match root.get("workspaces") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        Some(Value::Object(o)) => o
            .get("packages")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if !from_json.is_empty() {
        return from_json;
    }
    // pnpm-workspace.yaml `packages:` (a list of plain strings in every fixture)
    let Ok(text) = std::fs::read_to_string(dir.join("pnpm-workspace.yaml")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        if line.starts_with("packages:") {
            in_packages = true;
            continue;
        }
        if in_packages {
            match line.trim_start().strip_prefix("- ") {
                Some(item) => out.push(item.trim().trim_matches(['\'', '"']).to_owned()),
                None if line.starts_with(' ') || line.is_empty() => {}
                None => in_packages = false,
            }
        }
    }
    out
}

/// The project's workspace, loaded by fnpm-workspace the way lockfile migration loads it.
pub fn workspace(dir: &Path) -> Workspace {
    // The real discovery, in the mode lockfile migration uses (missing members are skipped).
    let opts = fnpm_workspace::LoadOptions {
        missing: fnpm_workspace::MissingWorkspace::Skip,
    };
    // A project whose workspace config does not load (the broken pnpm-workspace.yaml cases) is
    // migrated with its root package.json alone, as the migration does.
    Workspace::load_with(dir, &opts)
        .map(|(ws, _)| ws)
        .unwrap_or_else(|_| Workspace {
            root_dir: dir.to_path_buf(),
            root: manifest(
                read_json(&dir.join("package.json")).unwrap_or_else(|| serde_json::json!({})),
            ),
            members: Vec::new(),
            settings: Default::default(),
        })
}

pub fn options(files: &dyn ProjectFiles) -> MigrateOptions<'_> {
    let mut o = MigrateOptions::new(files);
    o.home = None;
    o
}

/// The bun.lock text of `exports[\`<name>\`]` in a `.snap` file, as JSON.
pub fn snapshot(file: &str, name: &str) -> Value {
    let text = std::fs::read_to_string(fixtures().join("snapshots").join(file)).unwrap();
    let marker = format!("exports[`{name}`] = `\n\"");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("no snapshot {name}"))
        + marker.len();
    let end = start + text[start..].find("\"\n`;").unwrap();
    jsonc(&text[start..end])
}

/// The `exports[...]` whose key ends with `: <suffix> 1`, as `(key, raw text)`.
pub fn snapshot_text_by_suffix(file: &str, suffix: &str) -> (String, String) {
    let text = std::fs::read_to_string(fixtures().join("snapshots").join(file)).unwrap();
    let tail = format!(": {suffix} 1`] = `\n");
    let at = text
        .find(&tail)
        .unwrap_or_else(|| panic!("no snapshot {suffix} in {file}"));
    let key_start = text[..at].rfind("exports[`").unwrap() + "exports[`".len();
    let body_start = at + tail.len();
    let end = body_start + text[body_start..].find("\n`;").unwrap();
    let mut body = &text[body_start..end];
    body = body.strip_prefix('"').unwrap_or(body);
    body = body.strip_suffix('"').unwrap_or(body);
    (
        text[key_start..at + 2 + suffix.len()].to_owned(),
        body.to_owned(),
    )
}

/// Parses bun.lock JSONC (trailing commas).
pub fn jsonc(text: &str) -> Value {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

/// bun.lock's `name@resolution` for a graph package.
pub fn bun_id(p: &Package) -> String {
    let res = match &p.resolution {
        Resolution::Npm { version, .. } => version.clone(),
        Resolution::Github {
            owner,
            repo,
            committish,
            ..
        } => format!("github:{owner}/{repo}#{committish}"),
        Resolution::Git {
            url,
            resolved_commit,
            committish,
            ..
        } => {
            // bun prints an scp-like repository as `ssh://user@host:path`
            let ssh = if fnpm_types::spec::is_scp_like_path(url) {
                "ssh://"
            } else {
                ""
            };
            format!(
                "git+{ssh}{url}#{}",
                if resolved_commit.is_empty() {
                    committish
                } else {
                    resolved_commit
                }
            )
        }
        Resolution::RemoteTarball { url, .. } => url.clone(),
        Resolution::LocalTarball { path, .. } => path.clone(),
        Resolution::Folder { path } => format!("file:{path}"),
        Resolution::Symlink { path } => format!("link:{}", path.trim_start_matches("./")),
        Resolution::Workspace { path } => format!("workspace:{path}"),
        Resolution::Root => "root:".into(),
    };
    format!("{}@{res}", p.name)
}

/// The npm tuple `[id, url, info, integrity]` or the info/integrity of other tuples.
pub struct LockPackage {
    pub id: String,
    pub url: Option<String>,
    pub info: Value,
    pub integrity: Option<String>,
}

pub fn lock_packages(lock: &Value) -> BTreeMap<String, LockPackage> {
    let mut out = BTreeMap::new();
    for (path, tuple) in lock["packages"].as_object().unwrap() {
        let t = tuple.as_array().unwrap();
        let id = t[0].as_str().unwrap().to_owned();
        let pkg = if t.len() >= 3 && t[1].is_string() && t[2].is_object() {
            let integrity = t
                .get(3)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            LockPackage {
                id,
                url: t[1].as_str().map(str::to_owned),
                info: t[2].clone(),
                integrity,
            }
        } else {
            let info = t.get(1).cloned().unwrap_or(Value::Null);
            let integrity = t
                .last()
                .and_then(Value::as_str)
                .filter(|s| s.contains('-') && (s.starts_with("sha")))
                .map(str::to_owned);
            LockPackage {
                id,
                url: None,
                info,
                integrity,
            }
        };
        out.insert(path.clone(), pkg);
    }
    out
}

/// node's lookup from a bun.lock path: own node_modules, then each ancestor's, then the root.
fn lookup<'a>(
    paths: &'a BTreeMap<String, LockPackage>,
    from: &str,
    name: &str,
) -> Option<&'a LockPackage> {
    let mut base = from.to_owned();
    loop {
        let key = if base.is_empty() {
            name.to_owned()
        } else {
            format!("{base}/{name}")
        };
        if let Some(p) = paths.get(&key) {
            return Some(p);
        }
        if base.is_empty() {
            return None;
        }
        // drop the last package name (scoped names have two segments)
        let mut parts: Vec<&str> = base.split('/').collect();
        let last_scoped = parts.len() >= 2 && parts[parts.len() - 2].starts_with('@');
        parts.truncate(parts.len() - if last_scoped { 2 } else { 1 });
        base = parts.join("/");
    }
}

fn info_deps(info: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for group in ["dependencies", "optionalDependencies", "peerDependencies"] {
        if let Some(Value::Object(m)) = info.get(group) {
            for (k, v) in m {
                out.insert(k.clone(), v.as_str().unwrap_or_default().to_owned());
            }
        }
    }
    out
}

fn graph_deps(p: &Package) -> BTreeMap<String, String> {
    p.dependencies
        .iter()
        .filter(|e| !e.behavior.is_dev())
        .map(|e| (e.name.clone(), e.literal.clone()))
        .collect()
}

/// Asserts the graph holds exactly the packages of a bun.lock (by `name@resolution`), with the
/// same integrity, tarball location (`registry` is the default one), dependency ranges, and
/// that every resolved edge points where node's lookup over bun.lock's paths finds it.
pub fn assert_matches_bun_lock(graph: &LockGraph, lock: &Value, registry: &str) {
    assert_matches_bun_lock_with(graph, lock, registry, true);
}

/// [`assert_matches_bun_lock`]; without `integrity`, registry integrity is not compared (yarn
/// berry's `checksum` is not a tarball hash, so bun fills it from the registry after migrating).
pub fn assert_matches_bun_lock_with(
    graph: &LockGraph,
    lock: &Value,
    _registry: &str,
    check_integrity: bool,
) {
    graph.validate().unwrap();
    let paths = lock_packages(lock);
    let lock_ids: BTreeSet<&str> = paths
        .values()
        .filter(|p| !p.id.contains("@workspace:"))
        .map(|p| p.id.as_str())
        .collect();
    // bun.lock holds the packages of the install tree: those reachable from a workspace
    let reachable = graph.reachable(fnpm_types::Behavior(0xff), None);
    let graph_ids: BTreeSet<String> = graph
        .packages
        .iter()
        .enumerate()
        .filter(|(i, _)| reachable[*i])
        .map(|(_, p)| p)
        .filter(|p| {
            !matches!(
                p.resolution,
                Resolution::Root | Resolution::Workspace { .. }
            )
        })
        .map(bun_id)
        .collect();
    let graph_ids_ref: BTreeSet<&str> = graph_ids.iter().map(String::as_str).collect();
    assert_eq!(graph_ids_ref, lock_ids, "package sets differ");

    for lp in paths.values() {
        let Some(p) = graph.packages.iter().find(|p| bun_id(p) == lp.id) else {
            continue;
        };
        if let Resolution::Npm {
            tarball_url,
            integrity,
            ..
        } = &p.resolution
        {
            // bun.lock writes `""` only for npmjs; any other registry's URL is recorded.
            let expected_url = lp.url.clone().filter(|u| !u.is_empty());
            assert_eq!(tarball_url, &expected_url, "{}: tarball url", lp.id);
            if check_integrity {
                assert_eq!(
                    integrity.map(|i| i.to_sri()),
                    lp.integrity,
                    "{}: integrity",
                    lp.id
                );
            }
            assert_eq!(
                graph_deps(p),
                info_deps(&lp.info),
                "{}: dependencies",
                lp.id
            );
        } else if let (true, Some(sri)) = (check_integrity, &lp.integrity) {
            assert_eq!(
                p.resolution.integrity().map(|i| i.to_sri()).as_ref(),
                Some(sri),
                "{}: integrity",
                lp.id
            );
        }
    }

    // edges resolve where node's lookup over bun.lock's install paths finds them
    let ids: Vec<String> = graph.packages.iter().map(bun_id).collect();
    for (path, lp) in &paths {
        let Some(owner) = graph.packages.iter().position(|p| bun_id(p) == lp.id) else {
            continue;
        };
        if matches!(
            graph.packages[owner].resolution,
            Resolution::Workspace { .. }
        ) {
            continue;
        }
        check_edges(graph, &ids, &paths, owner, path);
    }
    let ws_name_paths: Vec<(PackageId, String)> = graph
        .workspaces
        .values()
        .map(|id| {
            (
                *id,
                if id.0 == 0 {
                    String::new()
                } else {
                    graph.packages[id.index()].name.clone()
                },
            )
        })
        .collect();
    for (id, path) in ws_name_paths {
        check_edges(graph, &ids, &paths, id.index(), &path);
    }

    // the root's declared dependencies
    if let Some(root) = lock["workspaces"].get("") {
        let mut expected = BTreeMap::new();
        for group in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ] {
            if let Some(Value::Object(m)) = root.get(group) {
                for (k, v) in m {
                    expected.insert(k.clone(), v.as_str().unwrap().to_owned());
                }
            }
        }
        let got: BTreeMap<String, String> = graph
            .root()
            .dependencies
            .iter()
            .map(|e| (e.name.clone(), e.literal.clone()))
            .collect();
        assert_eq!(got, expected, "root dependencies");
    }
}

fn default_url(registry: &str, name: &str, version: &str) -> String {
    let unscoped = name.rsplit_once('/').map_or(name, |(_, n)| n);
    format!(
        "{}/{name}/-/{unscoped}-{version}.tgz",
        registry.trim_end_matches('/')
    )
}

fn check_edges(
    graph: &LockGraph,
    ids: &[String],
    paths: &BTreeMap<String, LockPackage>,
    owner: usize,
    path: &str,
) {
    for e in &graph.packages[owner].dependencies {
        let Some(t) = e.target else { continue };
        if e.behavior.is_peer()
            || matches!(
                graph.packages[t.index()].resolution,
                Resolution::Workspace { .. } | Resolution::Root
            )
        {
            continue;
        }
        let found = lookup(paths, path, &e.name).map(|p| p.id.as_str());
        assert_eq!(
            found,
            Some(ids[t.index()].as_str()),
            "edge {} of {} at {path:?}",
            e.name,
            ids[owner]
        );
    }
}
