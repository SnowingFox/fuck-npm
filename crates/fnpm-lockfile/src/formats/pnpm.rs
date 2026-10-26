//! pnpm-lock.yaml: migration of lockfileVersion 7 to 11 (bun `install/pnpm.rs`) and export as
//! lockfileVersion 9.0.
//!
//! Migration reads `importers`, `packages` + `snapshots` (peer suffixes, patch hashes, peer
//! variants), `catalogs`, `overrides` (with selectors), `patchedDependencies` and pnpm 11's
//! registry-qualified versions, and carries pnpm-workspace.yaml / `package.json#pnpm` settings
//! over to package.json the way bun does (workspaces, catalogs, overrides, patchedDependencies,
//! packageExtensions, onlyBuiltDependencies as trustedDependencies).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use fnpm_types::spec::{is_tarball, link_path_for_lockfile};
use fnpm_types::{
    Behavior, ConfigVersion, CpuSet, DepEdge, DependencySpec, Integrity, LibcSet, LockGraph, OsSet,
    OverrideRule, OverrideSelector, Package, PackageId, PatchedDependency, PeerMeta,
    PlatformConstraints, Resolution,
};
use fnpm_workspace::Workspace;
use serde_json::{Map, Value};

use super::foreign::yaml::{self, Node};
use super::foreign::{
    self, DiskFiles, MigrateOptions, Migration, PackageJsonWriteBack, manifest, semver,
};
use crate::{Error, Result};

const FILE: &str = "pnpm-lock.yaml";
const WORKSPACE_FILE: &str = "pnpm-workspace.yaml";

/// Migrates with the process environment and the real filesystem.
pub fn parse(bytes: &[u8], workspace: &Workspace) -> Result<LockGraph> {
    let files = DiskFiles;
    Ok(migrate(bytes, workspace, &MigrateOptions::from_process(&files))?.graph)
}

fn fail(ws: &Workspace, message: impl Into<String>) -> Error {
    foreign::parse_error(ws, FILE, message)
}

fn invalid(ws: &Workspace) -> Error {
    fail(ws, "invalid pnpm-lock.yaml")
}

fn shown(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_debug().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// `(peers suffix start, patch hash start)` of a dep path (pnpm `dependency-path`
/// `indexOfDepPathSuffix`).
fn index_of_dep_path_suffix(path: &str) -> (Option<usize>, Option<usize>) {
    let b = path.as_bytes();
    if b.len() < 2 || b[b.len() - 1] != b')' {
        return (None, None);
    }
    let mut open: i64 = 1;
    let mut i = b.len() - 1;
    while i > 0 {
        i -= 1;
        if b[i] == b'(' {
            open -= 1;
        } else if b[i] == b')' {
            open += 1;
        } else if open == 0 {
            if path[i + 1..].starts_with("(patch_hash=") {
                let peers = path[i + 2..].find('(').map(|j| j + i + 2);
                return (peers, Some(i + 1));
            }
            return (Some(i + 1), None);
        }
    }
    (None, None)
}

/// `name@version(hash)` gives `name@version`; `version(hash)` gives `version`.
pub(crate) fn remove_suffix(path: &str) -> &str {
    let (peers, patch) = index_of_dep_path_suffix(path);
    patch.or(peers).map_or(path, |i| &path[..i])
}

/// pnpm `refToRelative`: is a snapshot reference already a dep path (`name@version`)?
fn reference_is_dep_path(reference: &str) -> bool {
    if reference.starts_with('@') {
        return true;
    }
    let Some(at) = reference.find('@') else {
        return false;
    };
    reference.find([':', '(']).is_none_or(|i| i > at)
}

fn dep_path(name: &str, reference: &str) -> String {
    if reference_is_dep_path(reference) {
        reference.to_owned()
    } else {
        format!("{name}@{reference}")
    }
}

/// `work:1.0.0` gives (`work`, `1.0.0`) for pnpm 11's registry-qualified dep paths.
fn split_registry_qualified_version(res: &str) -> Option<(&str, &str)> {
    let (registry, version) = res.split_once(':')?;
    let b = registry.as_bytes();
    if b.is_empty()
        || !b[0].is_ascii_alphabetic()
        || !b
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
        || !version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        || matches!(
            registry,
            "file" | "link" | "npm" | "runtime" | "workspace" | "catalog" | "git"
        )
    {
        return None;
    }
    Some((registry, version))
}

/// pnpm 11's built-in named registries; `namedRegistries` in pnpm-workspace.yaml overrides them.
const BUILTIN_NAMED_REGISTRIES: [(&str, &str); 2] = [
    ("gh", "https://npm.pkg.github.com/"),
    ("npmjs", "https://registry.npmjs.org/"),
];

/// A tag GitHub's tarball names its folder with must be a plain token.
fn is_safe_resolved_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// bun `Resolution::from_pnpm_lockfile`.
fn resolution_from_reference(res: &str) -> Option<Resolution> {
    if let Some(rest) = res.strip_prefix("https://codeload.github.com/") {
        let (user, rest) = rest.split_once('/')?;
        let (repo, rest) = rest.split_once('/')?;
        let (_, committish) = rest.split_once('/')?;
        if !is_safe_resolved_tag(committish) {
            return None;
        }
        return Some(Resolution::Github {
            owner: user.to_owned(),
            repo: repo.to_owned(),
            committish: committish.to_owned(),
            resolved_commit: committish.to_owned(),
            integrity: None,
        });
    }
    if let Some(path) = res.strip_prefix("file:") {
        if res.ends_with(".tgz") {
            return Some(Resolution::LocalTarball {
                path: path.to_owned(),
                integrity: None,
            });
        }
        return Some(Resolution::Folder {
            path: path.to_owned(),
        });
    }
    match DependencySpec::parse("", res).ok()? {
        DependencySpec::Git(git) => {
            let committish = git.committish.to_raw();
            Some(Resolution::Git {
                url: git.url,
                committish: committish.clone(),
                resolved_commit: committish,
                integrity: None,
            })
        }
        DependencySpec::Github(gh) => {
            let committish = gh.committish.to_raw();
            Some(Resolution::Github {
                owner: gh.owner,
                repo: gh.repo,
                committish: committish.clone(),
                resolved_commit: committish,
                integrity: None,
            })
        }
        DependencySpec::Tarball {
            uri: fnpm_types::TarballUri::Remote(url),
        } => Some(Resolution::RemoteTarball {
            url,
            integrity: None,
        }),
        DependencySpec::Tarball {
            uri: fnpm_types::TarballUri::Local(path),
        } => Some(Resolution::LocalTarball {
            path,
            integrity: None,
        }),
        DependencySpec::Npm { .. } => {
            let v = semver::exact_version(res)?;
            let _ = v;
            Some(Resolution::npm(res, None))
        }
        _ => None,
    }
}

enum ResolutionError {
    Unsupported,
    GitSubdirectory,
}

/// The resolution of a `packages:` entry. A registry package's recorded `tarball:` is the
/// caller's to judge.
fn resolution_from_package_entry(
    res: &str,
    resolution: Option<&Node>,
) -> std::result::Result<Resolution, ResolutionError> {
    let tarball = resolution.and_then(|r| r.get_str("tarball"));
    if let Some(obj) = resolution {
        let ty = obj.get_str("type").unwrap_or_default();
        // `path:` is pnpm's `repo#commit&path:sub/dir` on git and git-hosted tarballs.
        if obj.get_str("path").is_some() {
            return Err(ResolutionError::GitSubdirectory);
        }
        if ty == "git"
            && let Some(repo) = obj.get_str("repo")
        {
            let commit = obj.get_str("commit").unwrap_or_default().to_owned();
            return Ok(Resolution::Git {
                url: repo.strip_prefix("git+").unwrap_or(repo).to_owned(),
                committish: commit.clone(),
                resolved_commit: commit,
                integrity: None,
            });
        }
        if ty == "directory"
            && let Some(dir) = obj.get_str("directory")
        {
            return Ok(Resolution::Folder {
                path: dir.to_owned(),
            });
        }
        if let Some(path) = tarball.and_then(|t| t.strip_prefix("file:")) {
            return Ok(Resolution::LocalTarball {
                path: path.to_owned(),
                integrity: None,
            });
        }
    }
    if res.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return resolution_from_reference(res).ok_or(ResolutionError::Unsupported);
    }
    if let Some(url) = tarball {
        if url.starts_with("https://codeload.github.com/") {
            return resolution_from_reference(url).ok_or(ResolutionError::Unsupported);
        }
        return Ok(Resolution::RemoteTarball {
            url: url.to_owned(),
            integrity: None,
        });
    }
    if let Some(path) = res.strip_prefix("file:")
        && is_tarball(path)
    {
        return Ok(Resolution::LocalTarball {
            path: path.to_owned(),
            integrity: None,
        });
    }
    resolution_from_reference(res).ok_or(ResolutionError::Unsupported)
}

/// `os`/`cpu`/`libc` as a list or a single string.
fn string_list(node: &Node) -> std::result::Result<Vec<&str>, ()> {
    match node {
        Node::Seq(items) => items.iter().map(|i| i.as_str().ok_or(())).collect(),
        other => other.as_str().map(|s| vec![s]).ok_or(()),
    }
}

/// A declared peer of a package or importer.
#[derive(Debug, Clone)]
struct PeerDecl {
    name: String,
    range: String,
    optional: bool,
    seen: bool,
}

impl PeerDecl {
    fn behavior(&self) -> Behavior {
        if self.optional {
            Behavior::PEER | Behavior::OPTIONAL
        } else {
            Behavior::PEER
        }
    }
}

/// With `strict` (a `packages:` entry) anything malformed is an error; in a package.json it is
/// skipped.
fn declared_peers_yaml(obj: &Node) -> std::result::Result<Vec<PeerDecl>, ()> {
    let mut peers: Vec<PeerDecl> = Vec::new();
    let declared = obj.get("peerDependencies");
    let meta = obj.get("peerDependenciesMeta");
    if [declared, meta].iter().flatten().any(|e| !e.is_map()) {
        return Err(());
    }
    for (k, v) in declared.map_or(&[][..], Node::entries) {
        let (Some(name), Some(range)) = (k.as_str(), v.as_str()) else {
            return Err(());
        };
        peers.push(PeerDecl {
            name: name.to_owned(),
            range: range.to_owned(),
            optional: false,
            seen: false,
        });
    }
    for (k, v) in meta.map_or(&[][..], Node::entries) {
        let Some(name) = k.as_str() else {
            return Err(());
        };
        if v.get("optional").and_then(Node::as_bool) != Some(true) {
            continue;
        }
        match peers.iter_mut().find(|p| p.name == name) {
            Some(p) => p.optional = true,
            None => peers.push(PeerDecl {
                name: name.to_owned(),
                range: "*".into(),
                optional: true,
                seen: false,
            }),
        }
    }
    Ok(peers)
}

fn declared_peers_json(manifest: &Value) -> Vec<PeerDecl> {
    let mut peers: Vec<PeerDecl> = manifest::string_entries(manifest, "peerDependencies")
        .into_iter()
        .map(|(n, r)| PeerDecl {
            name: n.to_owned(),
            range: r.to_owned(),
            optional: false,
            seen: false,
        })
        .collect();
    for name in manifest::optional_peer_names(manifest) {
        match peers.iter_mut().find(|p| p.name == name) {
            Some(p) => p.optional = true,
            None => peers.push(PeerDecl {
                name: name.to_owned(),
                range: "*".into(),
                optional: true,
                seen: false,
            }),
        }
    }
    peers
}

const GROUPS: [(&str, Behavior); 3] = [
    ("dependencies", Behavior::PROD),
    ("devDependencies", Behavior::DEV),
    ("optionalDependencies", Behavior::OPTIONAL),
];

fn importer_label(path: &str) -> String {
    if path == "." {
        "the root package".into()
    } else {
        format!("workspace \"{}\"", shown(path))
    }
}

/// `(literal, reference to bind by)` of a snapshot dependency (bun
/// `append_snapshot_dependency_version`): an alias dep path becomes `npm:<name>@<version>`, and
/// a reference that the literal cannot rebuild is kept for binding.
fn snapshot_literal(reference: &str) -> (String, Option<String>) {
    if reference_is_dep_path(reference) {
        if let (name, Some(version)) = fnpm_types::name::split_name_and_maybe_version(reference) {
            let qualified = split_registry_qualified_version(version);
            if qualified.is_none() && !version.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                return (version.to_owned(), Some(reference.to_owned()));
            }
            return match qualified {
                Some((_, v)) => (format!("npm:{name}@{v}"), Some(reference.to_owned())),
                None => (format!("npm:{name}@{version}"), None),
            };
        }
    } else if let Some((_, version)) = split_registry_qualified_version(reference) {
        return (version.to_owned(), Some(reference.to_owned()));
    }
    (reference.to_owned(), None)
}
struct Patch {
    path: String,
    hash: String,
    config_key: String,
}

/// The parsed pnpm-workspace.yaml, if there is one that parses.
fn read_workspace_yaml(
    ws: &Workspace,
    options: &MigrateOptions<'_>,
    warnings: &mut Vec<String>,
) -> Option<(String, Node)> {
    let text = options.files.read(&ws.root_dir.join(WORKSPACE_FILE))?;
    match yaml::parse_last(&text) {
        Ok(node) => Some((text, node)),
        Err(e) => {
            warnings.push(format!("{WORKSPACE_FILE}: {e}"));
            None
        }
    }
}

fn string_pairs(node: &Node) -> Vec<(String, String)> {
    node.entries()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), v.as_str()?.to_owned())))
        .collect()
}

/// One importer's state while edges are bound.
struct Importer {
    /// `.` for the root, else the workspace path
    path: String,
    /// dependency name to its `version` in the importer, without the peer suffix
    versions: HashMap<String, String>,
}

/// The pnpm migration. See the module docs.
pub fn migrate(bytes: &[u8], ws: &Workspace, options: &MigrateOptions<'_>) -> Result<Migration> {
    let data = std::str::from_utf8(bytes).map_err(|_| fail(ws, "pnpm-lock.yaml is not UTF-8"))?;
    let root = yaml::parse_last(data)
        .map_err(|e| fail(ws, format!("Failed to parse pnpm-lock.yaml: {e}")))?;
    if !root.is_map() {
        return Err(fail(
            ws,
            format!(
                "pnpm-lock.yaml root must be an object, got {}",
                root.type_name()
            ),
        ));
    }
    let Some(version_node) = root.get("lockfileVersion") else {
        return Err(fail(ws, "pnpm-lock.yaml missing 'lockfileVersion' field"));
    };
    let version_text = version_node.text().unwrap_or_default();
    let version_num: Option<f64> = match version_node {
        Node::Scalar {
            kind: yaml::Kind::Number,
            text,
        } => text.parse::<f64>().ok().filter(|n| *n >= 0.0),
        Node::Scalar {
            kind: yaml::Kind::Str,
            text,
        } => text
            .split_once('.')
            .map_or(text.as_str(), |(m, _)| m)
            .parse()
            .ok(),
        _ => None,
    };
    let Some(version_num) = version_num else {
        return Err(fail(
            ws,
            format!(
                "pnpm-lock.yaml 'lockfileVersion' must be a number or string, got {}",
                version_node.type_name()
            ),
        ));
    };
    if version_num < 7.0 {
        return Err(fail(
            ws,
            format!(
                "cannot migrate pnpm-lock.yaml: unsupported lockfileVersion {version_text} (run `pnpm install --lockfile-only` to upgrade it)"
            ),
        ));
    }
    let mut warnings = Vec::new();
    if version_num >= 10.0 {
        warnings.push(format!(
            "pnpm-lock.yaml is lockfileVersion {version_text}; migrating it as 9.0"
        ));
    }
    let exclude_links = root
        .get("settings")
        .and_then(|s| s.get("excludeLinksFromLockfile"))
        .and_then(Node::as_bool)
        == Some(true);
    let workspace_yaml = read_workspace_yaml(ws, options, &mut warnings);
    let ws_yaml = workspace_yaml.as_ref().map(|(_, n)| n);

    // -- catalogs and overrides from the lockfile ------------------------------------
    let mut catalogs = fnpm_types::Catalogs::default();
    for (group, entries) in root.get_or_empty("catalogs").entries() {
        let Some(group) = group.as_str() else {
            return Err(invalid(ws));
        };
        if !entries.is_map() {
            continue;
        }
        let target = if group == "default" {
            &mut catalogs.default
        } else {
            catalogs.named.entry(group.to_owned()).or_default()
        };
        for (name, entry) in entries.entries() {
            let (Some(name), Some(spec)) = (name.as_str(), entry.get_str("specifier")) else {
                return Err(fail(
                    ws,
                    format!(
                        "pnpm-lock.yaml catalog '{}' has an invalid entry",
                        shown(group)
                    ),
                ));
            };
            target.insert(name.to_owned(), spec.to_owned());
        }
    }
    let mut overrides: Vec<OverrideRule> = Vec::new();
    for (key, value) in root.get_or_empty("overrides").entries() {
        let Some(key) = key.as_str() else {
            return Err(invalid(ws));
        };
        let Some(value) = value.as_str() else {
            return Err(fail(
                ws,
                format!("pnpm-lock.yaml override '{}' must be a string", shown(key)),
            ));
        };
        // The install warns about unsupported rules when it reads the migrated package.json.
        if value == "-" {
            continue;
        }
        let Ok(Some(selector)) = OverrideSelector::parse(key) else {
            continue;
        };
        if DependencySpec::parse(&selector.target.name, value).is_err() {
            return Err(fail(
                ws,
                format!(
                    "pnpm-lock.yaml override '{}' has an invalid value '{}'",
                    shown(key),
                    shown(value)
                ),
            ));
        }
        overrides.push(OverrideRule {
            selector,
            value: value.to_owned(),
        });
    }

    // patch hash to every configured patchedDependencies key using that patch file
    let mut patches: Vec<Patch> = Vec::new();
    let mut config_patch_paths: Option<Vec<(String, String)>> = None;
    for (key, value) in root.get_or_empty("patchedDependencies").entries() {
        let Some(key) = key.as_str() else {
            return Err(invalid(ws));
        };
        if let Some(hash) = value.as_str() {
            // Current pnpm records only the hash; the path is in the config.
            let config = config_patch_paths.get_or_insert_with(|| {
                let mut paths: Vec<(String, String)> = ws
                    .root
                    .raw
                    .get("pnpm")
                    .map(|p| manifest::string_entries(p, "patchedDependencies"))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
                if let Some(y) = ws_yaml {
                    paths.extend(string_pairs(y.get_or_empty("patchedDependencies")));
                }
                paths
            });
            let bare = fnpm_types::name::split_name_and_maybe_version(key).0;
            let found = config
                .iter()
                .find(|(k, _)| k == key)
                .or_else(|| config.iter().find(|(k, _)| k == bare));
            let Some((config_key, path)) = found else {
                warnings.push(format!(
                    "skipping patch \"{}\" from pnpm-lock.yaml: not in patchedDependencies of package.json or pnpm-workspace.yaml",
                    shown(key)
                ));
                continue;
            };
            patches.push(Patch {
                path: path.clone(),
                hash: hash.to_owned(),
                config_key: config_key.clone(),
            });
        } else {
            let (Some(path), Some(hash)) = (value.get_str("path"), value.get_str("hash")) else {
                return Err(invalid(ws));
            };
            patches.push(Patch {
                path: path.to_owned(),
                hash: hash.to_owned(),
                config_key: key.to_owned(),
            });
        }
    }

    // -- importers ------------------------------------------------------------------
    let Some(importers) = root.get("importers").filter(|i| i.is_map()) else {
        return Err(fail(ws, "pnpm-lock.yaml missing 'importers' field"));
    };
    let mut root_importer: Option<&Node> = None;
    // (path, importer node, manifest, name, version) of named workspace importers
    let mut members: Vec<(String, &Node, Value, String, String)> = Vec::new();
    for (key, value) in importers.entries() {
        let Some(path) = key.as_str() else {
            return Err(invalid(ws));
        };
        if path == "." {
            if root_importer.replace(value).is_some() {
                return Err(invalid(ws));
            }
            continue;
        }
        let manifest = match ws.member_by_path(path) {
            Some(m) => m.manifest.raw.clone(),
            None => {
                let file = ws.root_dir.join(path).join("package.json");
                let Some(text) = options.files.read(&file) else {
                    return Err(fail(
                        ws,
                        format!(
                            "pnpm-lock.yaml lists importer '{}' but '{}/package.json' does not exist",
                            shown(path),
                            shown(path)
                        ),
                    ));
                };
                serde_json::from_str::<Value>(&text).map_err(|_| invalid(ws))?
            }
        };
        // Nameless importers are skipped, like workspace discovery skips them.
        let Some(name) = manifest::get_str(&manifest, "name")
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        let version = manifest::get_str(&manifest, "version")
            .unwrap_or_default()
            .to_owned();
        if members.iter().any(|(p, ..)| p == path) {
            return Err(invalid(ws));
        }
        members.push((
            foreign::normalize_posix(path),
            value,
            manifest,
            name,
            version,
        ));
    }
    let Some(root_importer) = root_importer else {
        return Err(fail(
            ws,
            "pnpm-lock.yaml missing root package entry (importers['.'])",
        ));
    };

    let root_manifest = &ws.root.raw;
    let mut root_pkg = Package::new(
        manifest::get_str(root_manifest, "name").unwrap_or_default(),
        manifest::get_str(root_manifest, "version").unwrap_or_default(),
        Resolution::Root,
    );
    root_pkg.peer_meta = manifest::peer_meta(root_manifest);
    root_pkg.bin = manifest::bin(root_manifest);
    root_pkg.platform = manifest::platform(root_manifest);
    let mut graph = LockGraph::new(root_pkg);
    graph.catalogs = catalogs;
    graph.overrides = overrides;
    let mut index = graph.index();
    let mut importer_state: Vec<Importer> = Vec::new();
    let (edges, versions) = importer_edges(
        ws,
        &graph,
        root_importer,
        root_manifest,
        ".",
        exclude_links,
        &mut warnings,
    )?;
    graph.packages[0].dependencies = edges;
    importer_state.push(Importer {
        path: ".".into(),
        versions,
    });
    // workspace path ("" is the root) and link folder to package
    let mut path_map: HashMap<String, PackageId> = HashMap::new();
    path_map.insert(String::new(), PackageId::ROOT);
    for (path, node, manifest, name, version) in &members {
        let (edges, versions) = importer_edges(
            ws,
            &graph,
            node,
            manifest,
            path,
            exclude_links,
            &mut warnings,
        )?;
        let mut pkg = Package::new(
            name.clone(),
            version.clone(),
            Resolution::Workspace { path: path.clone() },
        );
        pkg.dependencies = edges;
        pkg.peer_meta = manifest::peer_meta(manifest);
        pkg.bin = manifest::bin(manifest);
        let id = foreign::intern(&mut graph, &mut index, pkg);
        if path_map.insert(path.clone(), id).is_some() {
            return Err(invalid(ws));
        }
        importer_state.push(Importer {
            path: path.clone(),
            versions,
        });
    }
    let workspace_end = graph.len();

    // -- link packages: pnpm-lock.yaml has no packages/snapshots entry for them ------------
    for (pkg_index, importer) in importer_state.iter().enumerate() {
        let edges = graph.packages[pkg_index].dependencies.clone();
        for e in edges {
            let spec = e.spec().ok();
            let keep = match &spec {
                Some(DependencySpec::Folder { .. } | DependencySpec::Workspace { .. }) => true,
                Some(DependencySpec::Symlink { path }) => fnpm_types::spec::is_link_path(path),
                _ => false,
            };
            if !keep {
                continue;
            }
            let Some(version) = importer.versions.get(&e.name) else {
                if e.behavior.is_peer() {
                    continue;
                }
                return Err(invalid(ws));
            };
            let Some(link) = remove_suffix(version).strip_prefix("link:") else {
                continue;
            };
            let folder = foreign::join_posix(&importer.path, link);
            if matches!(spec, Some(DependencySpec::Workspace { .. })) {
                if path_map
                    .get(&folder)
                    .is_some_and(|id| id.index() < workspace_end)
                {
                    continue;
                }
                return Err(fail(
                    ws,
                    format!(
                        "Workspace link dependencies to non-existent folders aren't supported yet in pnpm-lock.yaml migration ('{}' of {} links to '{}')",
                        shown(&e.name),
                        importer_label(&importer.path),
                        shown(&folder)
                    ),
                ));
            }
            if path_map.contains_key(&folder) {
                continue;
            }
            let pkg = Package::new(
                e.name.clone(),
                "",
                Resolution::Symlink {
                    path: link_path_for_lockfile(&folder).into_owned(),
                },
            );
            let id = foreign::intern(&mut graph, &mut index, pkg);
            path_map.insert(folder, id);
        }
    }

    // -- packages + snapshots ----------------------------------------------------------
    let packages = root.get("packages");
    let snapshots = root.get("snapshots");
    if packages.is_some() && snapshots.is_none() {
        return Err(fail(
            ws,
            "pnpm-lock.yaml has 'packages' but missing 'snapshots' field",
        ));
    }
    let mut packages_by_key: HashMap<&str, &Node> = HashMap::new();
    for (key, obj) in packages.map_or(&[][..], Node::entries) {
        let Some(key) = key.as_str() else {
            return Err(invalid(ws));
        };
        if !obj.is_map() {
            return Err(invalid(ws));
        }
        // Pruned lockfiles (`turbo prune`) can leave peer-suffixed keys in `packages:`.
        packages_by_key.entry(remove_suffix(key)).or_insert(obj);
    }
    let named_registries: Vec<(String, String)> = {
        let mut list: Vec<(String, String)> = ws_yaml
            .map(|y| string_pairs(y.get_or_empty("namedRegistries")))
            .unwrap_or_default()
            .into_iter()
            .map(|(n, u)| (n, registry_without_auth(&u)))
            .collect();
        for (n, u) in BUILTIN_NAMED_REGISTRIES {
            if !list.iter().any(|(name, _)| name == n) {
                list.push((n.to_owned(), u.to_owned()));
            }
        }
        list
    };
    let mut warned_registries: HashSet<String> = HashSet::new();
    let mut dep_map: HashMap<String, PackageId> = HashMap::new();
    // (package, edge index) to the snapshot reference its dep path is built from
    let mut snapshot_refs: HashMap<(PackageId, usize), String> = HashMap::new();
    let mut unbound_peers: HashSet<PackageId> = HashSet::new();
    // pnpm `patchedDependencies` key to the fnpm keys it patches
    let mut found_patches: Vec<(String, Vec<String>)> = Vec::new();
    let empty = Node::Map(Vec::new());
    for (key, snapshot) in snapshots.map_or(&[][..], Node::entries) {
        let Some(full_key) = key.as_str() else {
            return Err(invalid(ws));
        };
        if !snapshot.is_map() {
            return Err(invalid(ws));
        }
        let (peer_idx, patch_idx) = index_of_dep_path_suffix(full_key);
        let patch_hash = match patch_idx {
            Some(i) => Some(
                full_key[i + "(patch_hash=".len()..]
                    .split_once(')')
                    .ok_or_else(|| invalid(ws))?
                    .0,
            ),
            None => None,
        };
        let key = patch_idx.or(peer_idx).map_or(full_key, |i| &full_key[..i]);
        if let Some(&existing) = dep_map.get(key) {
            if unbound_peers.contains(&existing)
                && bind_peers_from_variant(&graph, existing, snapshot, &mut snapshot_refs)
            {
                unbound_peers.remove(&existing);
            }
            continue;
        }
        let (name, Some(res)) = fnpm_types::name::split_name_and_maybe_version(key) else {
            return Err(invalid(ws));
        };
        if res.starts_with("runtime:") {
            continue;
        }
        let package = match packages_by_key.get(key) {
            Some(obj) => *obj,
            // Like pnpm, only a `file:` directory is rebuilt when its `packages:` entry was pruned.
            None if res
                .strip_prefix("file:")
                .is_some_and(|dir| !is_tarball(dir)) =>
            {
                &empty
            }
            None => continue,
        };
        let (res, registry_name) = match split_registry_qualified_version(res) {
            Some((registry, version)) => (version, Some(registry)),
            None => (res, None),
        };
        let resolution_node = package.get("resolution");
        if resolution_node.is_some_and(|r| !r.is_map()) {
            return Err(invalid(ws));
        }
        let mut resolution = match resolution_from_package_entry(res, resolution_node) {
            Ok(r) => r,
            Err(ResolutionError::Unsupported) => {
                return Err(fail(
                    ws,
                    format!(
                        "pnpm-lock.yaml package '{}' has an unsupported resolution",
                        shown(key)
                    ),
                ));
            }
            Err(ResolutionError::GitSubdirectory) => {
                return Err(fail(
                    ws,
                    format!(
                        "pnpm-lock.yaml package '{}' is a git sub-directory dependency (resolution.path), which fnpm does not support",
                        shown(key)
                    ),
                ));
            }
        };
        // pnpm records injected workspace packages as `name@file:<workspace dir>`.
        if let Resolution::Folder { path } = &resolution
            && let Some(&ws_id) = path_map
                .get(&foreign::normalize_posix(path))
                .filter(|id| id.index() < workspace_end)
        {
            dep_map.insert(key.to_owned(), ws_id);
            continue;
        }
        if let Resolution::Npm {
            version,
            tarball_url,
            ..
        } = &mut resolution
        {
            let scope_registry = options.registry_for(name);
            let registry: String = match registry_name {
                None => scope_registry.to_owned(),
                Some(registry_name) => {
                    let url = named_registries
                        .iter()
                        .find(|(n, _)| n == registry_name)
                        .map(|(_, u)| u.as_str());
                    let is_scope_registry = url.is_some_and(|u| {
                        foreign::url_is_under_registry(u, scope_registry)
                            && foreign::url_is_under_registry(scope_registry, u)
                    });
                    if !is_scope_registry && warned_registries.insert(registry_name.to_owned()) {
                        warnings.push(match url {
                            Some(url) => format!(
                                "fetching pnpm registry \"{}\" packages from {}; add it to fnpm.toml or .npmrc if it needs authentication",
                                shown(registry_name),
                                url
                            ),
                            None => format!(
                                "skipping pnpm registry \"{}\" from pnpm-lock.yaml: not in namedRegistries of pnpm-workspace.yaml (resolving its packages from the configured registry)",
                                shown(registry_name)
                            ),
                        });
                    }
                    url.filter(|_| !is_scope_registry)
                        .unwrap_or(scope_registry)
                        .to_owned()
                }
            };
            // Registries like GitHub Packages serve tarballs off the canonical `/-/` path.
            let recorded = resolution_node
                .and_then(|r| r.get_str("tarball"))
                .filter(|u| foreign::url_is_under_registry(u, &registry));
            let url = recorded.map_or_else(
                || foreign::default_tarball_url(&registry, name, version),
                str::to_owned,
            );
            // Like bun.lock, only npmjs packages are written without their URL.
            *tarball_url =
                (!foreign::url_is_under_registry(&url, foreign::DEFAULT_REGISTRY)).then_some(url);
        }
        if let Some(integrity) = resolution_node.and_then(|r| r.get("integrity")) {
            let integrity = integrity.as_str().ok_or_else(|| invalid(ws))?;
            if let Some(i) = foreign::parse_integrity(integrity) {
                resolution.set_integrity(i);
            }
        }
        let mut pkg = Package::new(
            name,
            package
                .get_str("version")
                .map_or_else(|| npm_version(&resolution), str::to_owned),
            resolution,
        );
        let list = |field: &str| -> Result<Vec<&str>> {
            package
                .get(field)
                .map_or(Ok(Vec::new()), |n| string_list(n).map_err(|()| invalid(ws)))
        };
        let (os, cpu, libc) = (list("os")?, list("cpu")?, list("libc")?);
        pkg.platform = PlatformConstraints {
            os: if os.is_empty() {
                OsSet::ALL
            } else {
                OsSet::from_list(&os)
            },
            cpu: if cpu.is_empty() {
                CpuSet::ALL
            } else {
                CpuSet::from_list(&cpu)
            },
            libc: LibcSet::from_package_list(&libc),
        };
        if let Some(deprecated) = package.get_str("deprecated") {
            pkg.meta.deprecated = Some(deprecated.to_owned());
        }
        let (edges, refs, has_unbound) = package_edges(ws, package, snapshot)?;
        pkg.peer_meta = edges
            .iter()
            .filter(|e| e.behavior.is_optional_peer())
            .map(|e| (e.name.clone(), PeerMeta { optional: true }))
            .collect();
        pkg.dependencies = edges;
        if let Some(hash) = patch_hash
            && let Some(patch) = patches.iter().find(|p| {
                p.hash == hash
                    && fnpm_types::name::split_name_and_maybe_version(&p.config_key).0 == name
            })
        {
            let patched_key = format!("{name}@{}", pkg.resolution);
            graph.patched_dependencies.insert(
                patched_key.clone(),
                PatchedDependency {
                    path: patch.path.clone(),
                    hash: Some(hash.to_owned()),
                },
            );
            match found_patches
                .iter_mut()
                .find(|(k, _)| *k == patch.config_key)
            {
                Some((_, keys)) => keys.push(patched_key),
                None => found_patches.push((patch.config_key.clone(), vec![patched_key])),
            }
        }
        let before = graph.len();
        let id = foreign::intern(&mut graph, &mut index, pkg);
        if graph.len() > before {
            for (ei, reference) in refs {
                snapshot_refs.insert((id, ei), reference);
            }
            if has_unbound {
                unbound_peers.insert(id);
            }
        }
        dep_map.insert(key.to_owned(), id);
    }

    // -- bind: the root's dependencies first, then the workspaces' -------------------------
    for (pkg_index, importer) in importer_state.iter().enumerate() {
        let edges = std::mem::take(&mut graph.packages[pkg_index].dependencies);
        let mut kept = Vec::with_capacity(edges.len());
        for mut e in edges {
            if let Some(peer) = peer_like_bun_lock(&graph, &e) {
                e.target = Some(peer);
                kept.push(e);
                continue;
            }
            let Some(version) = importer.versions.get(&e.name) else {
                if e.behavior.is_peer() {
                    // `--frozen-lockfile` must not resolve anything pnpm-lock.yaml left out; an
                    // unlocked peer is dropped so the install resolves it.
                    if !e.behavior.is_optional_peer()
                        && !options.frozen
                        && graph.by_name(&e.name).next().is_none()
                    {
                        continue;
                    }
                    kept.push(e);
                    continue;
                }
                return Err(fail(
                    ws,
                    if pkg_index == 0 {
                        format!(
                            "pnpm-lock.yaml cannot resolve root dependency '{}' - missing version in importer",
                            shown(&e.name)
                        )
                    } else {
                        format!(
                            "pnpm-lock.yaml cannot resolve workspace dependency '{}' in '{}' - missing version",
                            shown(&e.name),
                            shown(&importer.path)
                        )
                    },
                ));
            };
            let reference = remove_suffix(version.strip_prefix("npm:").unwrap_or(version));
            if let Some(link) = reference.strip_prefix("link:")
                && let Some(&id) = path_map.get(&foreign::join_posix(&importer.path, link))
            {
                e.target = Some(id);
            } else {
                let path = dep_path(&e.name, reference);
                let Some(&id) = dep_map.get(&path) else {
                    return Err(missing_entry(
                        ws,
                        &path,
                        &e.name,
                        &format!("importer '{}'", shown(&importer.path)),
                    ));
                };
                e.target = Some(id);
            }
            if e.target.is_some_and(|t| {
                matches!(
                    graph.packages[t.index()].resolution,
                    Resolution::Workspace { .. }
                )
            }) && !e.behavior.is_peer()
            {
                e.behavior |= Behavior::WORKSPACE;
            }
            kept.push(e);
        }
        graph.packages[pkg_index].dependencies = kept;
    }
    for pkg_index in workspace_end..graph.len() {
        let pid = PackageId(pkg_index as u32);
        for ei in 0..graph.packages[pkg_index].dependencies.len() {
            let e = graph.packages[pkg_index].dependencies[ei].clone();
            if let Some(peer) = peer_like_bun_lock(&graph, &e) {
                graph.packages[pkg_index].dependencies[ei].target = Some(peer);
                continue;
            }
            let reference = remove_suffix(e.literal.strip_prefix("npm:").unwrap_or(&e.literal));
            let path = if let Some(snapshot_ref) = snapshot_refs.get(&(pid, ei)) {
                dep_path(&e.name, snapshot_ref)
            } else if e.behavior.is_peer() {
                continue;
            } else {
                let is_local = matches!(
                    e.spec(),
                    Ok(DependencySpec::Folder { .. }
                        | DependencySpec::Symlink { .. }
                        | DependencySpec::Workspace { .. })
                );
                if is_local
                    && let Some(&id) = path_map.get(&foreign::normalize_posix(
                        reference.strip_prefix("link:").unwrap_or(reference),
                    ))
                {
                    graph.packages[pkg_index].dependencies[ei].target = Some(id);
                    continue;
                }
                dep_path(&e.name, reference)
            };
            let Some(&id) = dep_map.get(&path) else {
                let owner = graph.packages[pkg_index].name.clone();
                return Err(missing_entry(
                    ws,
                    &path,
                    &e.name,
                    &format!("package '{}'", shown(&owner)),
                ));
            };
            graph.packages[pkg_index].dependencies[ei].target = Some(id);
        }
    }

    foreign::clear_non_registry_platform(&mut graph);
    // pnpm records a folder package's `file:` dependencies relative to the lockfile instead.
    for p in &mut graph.packages {
        let Resolution::Folder { path: dir } = &p.resolution else {
            continue;
        };
        let dir = dir.clone();
        for e in &mut p.dependencies {
            if let Ok(DependencySpec::Folder { path }) = e.spec() {
                let relative = foreign::relative_posix(
                    &foreign::normalize_posix(&dir),
                    &foreign::normalize_posix(&path),
                );
                e.literal = format!("file:{}", if relative.is_empty() { "." } else { &relative });
            }
        }
    }

    // -- package.json write-back -------------------------------------------------------
    let mut write_back = Vec::new();
    if let Some(edit) = root_write_back(
        root_manifest,
        workspace_yaml.as_ref(),
        &found_patches,
        &mut warnings,
    ) {
        write_back.push(edit);
    }
    if let Some(y) = ws_yaml {
        put_missing_catalogs(&mut graph, y);
    }
    let effective = foreign::effective_root(root_manifest, &write_back);
    foreign::apply_root_settings(&mut graph, &effective);
    foreign::finish(&mut graph, ConfigVersion::V1);
    Ok(Migration {
        graph,
        write_back,
        warnings,
    })
}

/// The package.json edits for a project that has a pnpm-workspace.yaml but no pnpm-lock.yaml to
/// migrate (bun `migrate_pnpm_workspace_config`, which runs when no lockfile was loaded):
/// pnpm-workspace.yaml's `packages`, catalogs, overrides, patchedDependencies, packageExtensions
/// and onlyBuiltDependencies, and `package.json#pnpm`, copied to the fields fnpm reads. `None`
/// when there is no pnpm-workspace.yaml, when the root already has `workspaces` (it was migrated
/// before), or when nothing changes. Returns the warnings too. Apply only when the lockfile is
/// saved.
pub fn migrate_workspace_config(
    ws: &Workspace,
    options: &MigrateOptions<'_>,
) -> (Option<PackageJsonWriteBack>, Vec<String>) {
    let mut warnings = Vec::new();
    if ws.root.raw.get("workspaces").is_some() {
        return (None, warnings);
    }
    let Some(text) = options.files.read(&ws.root_dir.join(WORKSPACE_FILE)) else {
        return (None, warnings);
    };
    // Like bun, a pnpm-workspace.yaml that does not parse still lets `package.json#pnpm` through.
    let workspace_yaml = match yaml::parse_last(&text) {
        Ok(node) => Some((text, node)),
        Err(e) => {
            warnings.push(format!("{WORKSPACE_FILE}: {e}"));
            None
        }
    };
    let edit = root_write_back(&ws.root.raw, workspace_yaml.as_ref(), &[], &mut warnings);
    (edit, warnings)
}

fn npm_version(resolution: &Resolution) -> String {
    resolution.npm_version().unwrap_or_default().to_owned()
}

fn missing_entry(ws: &Workspace, path: &str, name: &str, parent: &str) -> Error {
    fail(
        ws,
        format!(
            "pnpm-lock.yaml has no package entry '{}' for dependency '{}' of {parent}",
            shown(path),
            shown(name)
        ),
    )
}

/// The URL without `user:password@`.
fn registry_without_auth(href: &str) -> String {
    let Some((scheme, rest)) = href.split_once("://") else {
        return href.to_owned();
    };
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..host_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => href.to_owned(),
    }
}

/// Binds a peer edge the way bun.lock's reader does, so the migrated tree matches the reloaded
/// one (`resolve_peer_dep_version_based`): required, not bundled, not overridden.
fn peer_like_bun_lock(graph: &LockGraph, e: &DepEdge) -> Option<PackageId> {
    if !e.behavior.is_peer()
        || e.behavior.is_optional_peer()
        || e.behavior.is_bundled()
        || foreign::is_overridden(graph, &e.name)
    {
        return None;
    }
    foreign::bind_peer_by_range(graph, &e.name, &e.literal)
}

/// The edges of an importer (bun `parse_append_importer_dependencies`) and the version each
/// dependency name is locked at.
fn importer_edges(
    ws: &Workspace,
    graph: &LockGraph,
    importer: &Node,
    manifest: &Value,
    path: &str,
    exclude_links: bool,
    warnings: &mut Vec<String>,
) -> Result<(Vec<DepEdge>, HashMap<String, String>)> {
    let manifest_peers = declared_peers_json(manifest);
    let mut versions: HashMap<String, String> = HashMap::new();
    let mut edges = Vec::new();
    for (group, behavior) in GROUPS {
        let Some(deps) = importer.get(group) else {
            continue;
        };
        if !deps.is_map() {
            return Err(invalid(ws));
        }
        for (key, value) in deps.entries() {
            let Some(name) = key.as_str() else {
                return Err(invalid(ws));
            };
            let Some(specifier) = value.get("specifier") else {
                return Err(fail(
                    ws,
                    format!(
                        "pnpm-lock.yaml dependency '{}' missing 'specifier' field",
                        shown(name)
                    ),
                ));
            };
            let Some(version) = value.get("version") else {
                return Err(fail(
                    ws,
                    format!(
                        "pnpm-lock.yaml dependency '{}' missing 'version' field",
                        shown(name)
                    ),
                ));
            };
            let version = version.as_str().ok_or_else(|| invalid(ws))?;
            if version.starts_with("runtime:") {
                warnings.push(format!(
                    "skipping \"{}@{}\" from pnpm-lock.yaml: runtime dependencies are not supported",
                    shown(name),
                    shown(version)
                ));
                continue;
            }
            if versions.contains_key(name) {
                continue;
            }
            versions.insert(name.to_owned(), remove_suffix(version).to_owned());
            // pnpm records an importer's auto-installed peers under `dependencies`.
            if manifest_peers.iter().any(|p| p.name == name)
                && manifest.get(group).is_none_or(|g| g.get(name).is_none())
            {
                continue;
            }
            let specifier = specifier.as_str().ok_or_else(|| invalid(ws))?;
            // The dependency keeps its `catalog:` reference; this checks the entry exists.
            if let Some(catalog) = specifier.strip_prefix("catalog:") {
                let catalog = catalog.trim();
                if graph.catalogs.get(catalog, name).is_none() {
                    return Err(fail(
                        ws,
                        format!(
                            "pnpm-lock.yaml catalog '{}' missing entry for dependency '{}'",
                            shown(catalog),
                            shown(name)
                        ),
                    ));
                }
            }
            if DependencySpec::parse(name, specifier).is_err() {
                return Err(invalid(ws));
            }
            edges.push(DepEdge::new(name, specifier, behavior));
        }
    }
    for peer in &manifest_peers {
        if !peer.optional && !versions.contains_key(&peer.name) {
            warnings.push(format!(
                "skipping peer \"{}\" of {}: not in pnpm-lock.yaml, resolving it",
                shown(&peer.name),
                importer_label(path)
            ));
        }
        edges.push(DepEdge::new(&peer.name, &peer.range, peer.behavior()));
    }
    if exclude_links {
        for (group, _) in GROUPS {
            for (name, spec) in manifest::string_entries(manifest, group) {
                if spec.starts_with("link:") && !versions.contains_key(name) {
                    warnings.push(format!(
                        "skipping \"{}\" from {}: excluded from pnpm-lock.yaml by excludeLinksFromLockfile",
                        shown(name),
                        importer_label(path)
                    ));
                }
            }
        }
    }
    Ok((edges, versions))
}

/// `(edges, (edge index, snapshot reference) to bind by, has an unbound peer)`.
type PackageEdges = (Vec<DepEdge>, Vec<(usize, String)>, bool);

/// A package's edges from its `packages:` entry (peers) and snapshot (resolved deps), the
/// snapshot references to bind by, and whether a declared peer has no resolution here.
fn package_edges(ws: &Workspace, package: &Node, snapshot: &Node) -> Result<PackageEdges> {
    let mut peers = declared_peers_yaml(package).map_err(|()| invalid(ws))?;
    let mut edges: Vec<DepEdge> = Vec::new();
    let mut refs_by_name: HashMap<String, String> = HashMap::new();
    // pnpm records resolved required peers under `dependencies` and resolved optional peers
    // under `optionalDependencies`.
    for (group, base) in GROUPS {
        let Some(deps) = snapshot.get(group) else {
            continue;
        };
        if !deps.is_map() {
            return Err(invalid(ws));
        }
        for (key, value) in deps.entries() {
            let Some(name) = key.as_str() else {
                return Err(invalid(ws));
            };
            let reference = remove_suffix(value.as_str().ok_or_else(|| invalid(ws))?);
            if let Some(peer) = peers.iter_mut().find(|p| p.name == name && !p.seen) {
                peer.seen = true;
                if !reference.starts_with("link:") {
                    refs_by_name.insert(name.to_owned(), reference.to_owned());
                    if DependencySpec::parse(name, &peer.range).is_ok() {
                        edges.push(DepEdge::new(name, &peer.range, peer.behavior()));
                        continue;
                    }
                }
            }
            let (literal, reference_to_keep) = snapshot_literal(reference);
            if let Some(r) = reference_to_keep {
                refs_by_name.insert(name.to_owned(), r);
            }
            if DependencySpec::parse(name, &literal).is_err() {
                return Err(invalid(ws));
            }
            edges.push(DepEdge::new(name, literal, base));
        }
    }
    let mut has_unbound = false;
    for peer in peers.iter().filter(|p| !p.seen) {
        if DependencySpec::parse(&peer.name, &peer.range).is_ok() {
            edges.push(DepEdge::new(&peer.name, &peer.range, peer.behavior()));
            has_unbound = true;
        }
    }
    let refs = edges
        .iter()
        .enumerate()
        .filter_map(|(i, e)| refs_by_name.get(&e.name).map(|r| (i, r.clone())))
        .collect();
    Ok((edges, refs, has_unbound))
}

/// pnpm sorts the peer-unmet variant (no suffix) first; later variants carry the resolutions
/// it lacks. Returns whether every peer is bound now.
fn bind_peers_from_variant(
    graph: &LockGraph,
    id: PackageId,
    snapshot: &Node,
    refs: &mut HashMap<(PackageId, usize), String>,
) -> bool {
    let groups = [
        snapshot.get("dependencies"),
        snapshot.get("optionalDependencies"),
    ];
    let mut all_bound = true;
    for (ei, e) in graph.packages[id.index()].dependencies.iter().enumerate() {
        if !e.behavior.is_peer() || refs.contains_key(&(id, ei)) {
            continue;
        }
        let reference = groups
            .iter()
            .flatten()
            .find_map(|g| g.get(&e.name))
            .and_then(Node::as_str)
            .map(remove_suffix)
            .filter(|r| !r.starts_with("link:"));
        match reference {
            Some(r) => {
                refs.insert((id, ei), r.to_owned());
            }
            None => all_bound = false,
        }
    }
    all_bound
}
// -- package.json write-back (bun `update_package_json_after_migration`) -----------------------

fn is_non_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(|o| !o.is_empty())
}

/// `false` if `field` is not an object. With `patches`, a bare `name` key becomes every
/// `name@version` it patched.
fn copy_into_root(
    json: &mut Map<String, Value>,
    field: &str,
    src: &Map<String, Value>,
    patches: Option<&[(String, Vec<String>)]>,
) -> bool {
    let dest = json
        .entry(field.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(dest) = dest.as_object_mut() else {
        return false;
    };
    for (key, value) in src {
        match patches.and_then(|p| p.iter().find(|(k, _)| k == key)) {
            Some((_, keys)) => {
                for k in keys {
                    dest.insert(k.clone(), value.clone());
                }
            }
            None => {
                dest.insert(key.clone(), value.clone());
            }
        }
    }
    true
}

/// The entries of a YAML mapping package.json can represent: scalar keys and values, as
/// written. With `named`, values are mappings of the same.
fn yaml_string_entries(map: &Node, named: bool, skipped: &mut usize) -> Map<String, Value> {
    let text_of = |n: &Node| {
        if n.is_null() {
            None
        } else {
            n.text().map(|t| Value::String(t.to_owned()))
        }
    };
    let mut out = Map::new();
    for (key, value) in map.entries() {
        let key = key.text().filter(|_| !key.is_null());
        let value = match value {
            Node::Map(_) if named => {
                Some(Value::Object(yaml_string_entries(value, false, skipped)))
            }
            _ if named => None,
            _ => text_of(value),
        };
        match (key, value) {
            (Some(k), Some(v)) => {
                out.insert(k.to_owned(), v);
            }
            _ => *skipped += 1,
        }
    }
    out
}

/// `packageExtensions` from pnpm-workspace.yaml: rule to group to name to range, plus
/// optional peers.
fn yaml_extension_entries(map: &Node) -> Map<String, Value> {
    let mut rules = Map::new();
    for (key, value) in map.entries() {
        let Some(key) = key
            .text()
            .filter(|_| value.is_map() && !value.entries().is_empty())
        else {
            continue;
        };
        let mut rule = Map::new();
        for group in ["dependencies", "optionalDependencies", "peerDependencies"] {
            if let Some(ranges) = value.get(group).filter(|g| g.is_map()) {
                rule.insert(
                    group.to_owned(),
                    Value::Object(yaml_string_entries(ranges, false, &mut 0)),
                );
            }
        }
        let mut meta = Map::new();
        for (peer, m) in value.get_or_empty("peerDependenciesMeta").entries() {
            if let (Some(peer), Some(true)) =
                (peer.text(), m.get("optional").and_then(Node::as_bool))
            {
                meta.insert(peer.to_owned(), serde_json::json!({ "optional": true }));
            }
        }
        if !meta.is_empty() {
            rule.insert("peerDependenciesMeta".to_owned(), Value::Object(meta));
        }
        rules.insert(key.to_owned(), Value::Object(rule));
    }
    rules
}

/// Adds `onlyBuiltDependencies` names to `out`. Returns whether the value is a list (an empty
/// list counts: it means no package may run scripts).
fn collect_only_built(names: Option<Vec<Option<String>>>, out: &mut Vec<String>) -> bool {
    let Some(names) = names else {
        return false;
    };
    for name in names.into_iter().flatten() {
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    true
}

fn json_list(v: Option<&Value>) -> Option<Vec<Option<String>>> {
    v?.as_array().map(|items| {
        items
            .iter()
            .map(|i| i.as_str().map(str::to_owned))
            .collect()
    })
}

fn yaml_list(n: Option<&Node>) -> Option<Vec<Option<String>>> {
    n?.as_seq().map(|items| {
        items
            .iter()
            .map(|i| i.as_str().map(str::to_owned))
            .collect()
    })
}

/// The root package.json edits of a pnpm migration: `package.json#pnpm` and
/// pnpm-workspace.yaml settings copied to the fields fnpm reads.
fn root_write_back(
    root: &Value,
    workspace_yaml: Option<&(String, Node)>,
    patches: &[(String, Vec<String>)],
    warnings: &mut Vec<String>,
) -> Option<PackageJsonWriteBack> {
    let original = root.as_object()?;
    let mut json = original.clone();
    let mut changes: Vec<String> = Vec::new();
    let mut trusted: Vec<String> = Vec::new();
    let (mut pnpm_only_built, mut ws_only_built) = (false, false);
    // Copied instead of moved, because pnpm keeps reading this block.
    if let Some(pnpm) = original.get("pnpm").and_then(Value::as_object) {
        for (field, with_patches, change) in [
            ("overrides", false, "copied pnpm.overrides to overrides"),
            (
                "patchedDependencies",
                true,
                "copied pnpm.patchedDependencies to patchedDependencies",
            ),
            (
                "packageExtensions",
                false,
                "copied pnpm.packageExtensions to packageExtensions",
            ),
        ] {
            if let Some(src) = pnpm
                .get(field)
                .filter(|v| is_non_empty_object(v))
                .and_then(Value::as_object)
                && copy_into_root(&mut json, field, src, with_patches.then_some(patches))
            {
                changes.push(change.into());
            }
        }
        pnpm_only_built =
            collect_only_built(json_list(pnpm.get("onlyBuiltDependencies")), &mut trusted);
    }
    let mut workspace_paths: Option<Vec<String>> = None;
    let (mut catalog, mut catalogs, mut ws_overrides, mut ws_patched, mut ws_extensions) =
        (None, None, None, None, None);
    if let Some((_, y)) = workspace_yaml {
        if let Some(packages) = y.get("packages").and_then(Node::as_seq) {
            workspace_paths = Some(
                packages
                    .iter()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect(),
            );
        }
        let mut strings_of = |field: &str| -> Option<Map<String, Value>> {
            let map = y.get(field).filter(|m| m.is_map())?;
            let mut skipped = 0;
            let strings = yaml_string_entries(map, field == "catalogs", &mut skipped);
            if skipped > 0 {
                warnings.push(format!(
                    "skipping non-string entries of \"{field}\" in pnpm-workspace.yaml ({skipped})"
                ));
            }
            Some(strings).filter(|m| !m.is_empty())
        };
        catalog = strings_of("catalog");
        catalogs = strings_of("catalogs");
        ws_overrides = strings_of("overrides");
        ws_patched = strings_of("patchedDependencies");
        if let Some(ext) = y.get("packageExtensions").filter(|m| m.is_map()) {
            ws_extensions = Some(yaml_extension_entries(ext)).filter(|m| !m.is_empty());
        }
        ws_only_built = collect_only_built(yaml_list(y.get("onlyBuiltDependencies")), &mut trusted);
    }
    let paths_array =
        |paths: &[String]| Value::Array(paths.iter().map(|p| Value::String(p.clone())).collect());
    if catalog.is_none() && catalogs.is_none() {
        if let Some(paths) = &workspace_paths {
            json.insert("workspaces".into(), paths_array(paths));
            changes.push("copied pnpm-workspace.yaml to workspaces".into());
        }
    } else {
        // An existing `workspaces` array is replaced by the object form.
        let mut workspaces = json
            .get("workspaces")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(paths) = workspace_paths.as_ref().filter(|p| !p.is_empty()) {
            workspaces.insert("packages".into(), paths_array(paths));
        }
        if let Some(c) = catalog {
            workspaces.insert("catalog".into(), Value::Object(c));
        }
        if let Some(c) = catalogs {
            workspaces.insert("catalogs".into(), Value::Object(c));
        }
        json.insert("workspaces".into(), Value::Object(workspaces));
        changes.push("copied pnpm-workspace.yaml to workspaces".into());
    }
    if let Some(src) = ws_overrides
        && copy_into_root(&mut json, "overrides", &src, None)
    {
        changes.push("copied pnpm-workspace.yaml overrides to overrides".into());
    }
    if let Some(src) = ws_patched
        && copy_into_root(&mut json, "patchedDependencies", &src, Some(patches))
    {
        changes
            .push("copied pnpm-workspace.yaml patchedDependencies to patchedDependencies".into());
    }
    if let Some(src) = ws_extensions
        && copy_into_root(&mut json, "packageExtensions", &src, None)
    {
        changes.push("copied pnpm-workspace.yaml packageExtensions to packageExtensions".into());
    }
    if (pnpm_only_built || ws_only_built) && add_trusted(&mut json, &trusted) {
        if pnpm_only_built {
            changes.push("copied pnpm.onlyBuiltDependencies to trustedDependencies".into());
        }
        if ws_only_built {
            changes.push(
                "copied pnpm-workspace.yaml onlyBuiltDependencies to trustedDependencies".into(),
            );
        }
    }
    if changes.is_empty() {
        return None;
    }
    let fields = json
        .into_iter()
        .filter(|(k, v)| original.get(k) != Some(v))
        .collect();
    Some(PackageJsonWriteBack {
        path: String::new(),
        fields,
        changes,
    })
}

fn add_trusted(json: &mut Map<String, Value>, names: &[String]) -> bool {
    let existing = json.get("trustedDependencies");
    if existing.is_some_and(|e| !e.is_array()) {
        return false;
    }
    let mut items: Vec<Value> = existing
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let existing_len = items.len();
    for name in names {
        if !items.iter().any(|i| i.as_str() == Some(name)) {
            items.push(Value::String(name.clone()));
        }
    }
    if existing.is_some() && items.len() == existing_len {
        return false;
    }
    json.insert("trustedDependencies".into(), Value::Array(items));
    true
}

/// pnpm-lock.yaml has only the catalog entries in use; the rest come from
/// pnpm-workspace.yaml (bun `put_missing_from_pnpm_workspace`).
fn put_missing_catalogs(graph: &mut LockGraph, y: &Node) {
    let text = |n: &Node| {
        if n.is_null() {
            None
        } else {
            n.text().map(str::to_owned)
        }
    };
    for (k, v) in y.get_or_empty("catalog").entries() {
        if let (Some(k), Some(v)) = (k.text(), text(v)) {
            graph.catalogs.default.entry(k.to_owned()).or_insert(v);
        }
    }
    for (group, entries) in y.get_or_empty("catalogs").entries() {
        let Some(group) = group.text() else { continue };
        let target = if group == "default" {
            &mut graph.catalogs.default
        } else {
            graph.catalogs.named.entry(group.to_owned()).or_default()
        };
        for (k, v) in entries.entries() {
            if let (Some(k), Some(v)) = (k.text(), text(v)) {
                target.entry(k.to_owned()).or_insert(v);
            }
        }
    }
}

// -- export (lockfileVersion 9.0) ---------------------------------------------------------------

/// The pnpm `packages:` key of a package, `None` for packages pnpm does not list there
/// (root, workspaces, links).
fn package_key(p: &Package) -> Option<String> {
    let res = match &p.resolution {
        Resolution::Npm { version, .. } => version.clone(),
        Resolution::RemoteTarball { url, .. } => url.clone(),
        Resolution::LocalTarball { path, .. } | Resolution::Folder { path } => {
            format!("file:{path}")
        }
        Resolution::Git {
            url,
            resolved_commit,
            committish,
            ..
        } => {
            format!(
                "git+{url}#{}",
                if resolved_commit.is_empty() {
                    committish
                } else {
                    resolved_commit
                }
            )
        }
        Resolution::Github {
            owner,
            repo,
            resolved_commit,
            ..
        } => format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{resolved_commit}"),
        Resolution::Root | Resolution::Workspace { .. } | Resolution::Symlink { .. } => {
            return None;
        }
    };
    Some(format!("{}@{res}", p.name))
}

/// The `(peer@version)` suffix of a package's snapshot key, from its bound peers.
fn peer_suffix(
    graph: &LockGraph,
    id: PackageId,
    depth: usize,
    cache: &mut HashMap<PackageId, String>,
) -> String {
    if let Some(s) = cache.get(&id) {
        return s.clone();
    }
    if depth > 32 {
        return String::new();
    }
    let mut peers: Vec<(&str, PackageId)> = graph.packages[id.index()]
        .dependencies
        .iter()
        .filter(|e| e.behavior.is_peer())
        .filter_map(|e| Some((e.name.as_str(), e.target?)))
        .filter(|(_, t)| package_key(&graph.packages[t.index()]).is_some() && *t != id)
        .collect();
    peers.sort();
    peers.dedup_by(|a, b| a.0 == b.0);
    let mut out = String::new();
    for (name, target) in peers {
        let reference = reference_to(graph, name, target, "", depth + 1, cache);
        out.push_str(&format!("({name}@{reference})"));
    }
    cache.insert(id, out.clone());
    out
}

/// How a dependency named `alias` refers to `target` from the importer at `from`.
fn reference_to(
    graph: &LockGraph,
    alias: &str,
    target: PackageId,
    from: &str,
    depth: usize,
    cache: &mut HashMap<PackageId, String>,
) -> String {
    let p = &graph.packages[target.index()];
    match &p.resolution {
        Resolution::Root | Resolution::Workspace { .. } | Resolution::Symlink { .. } => {
            let path = match &p.resolution {
                Resolution::Workspace { path } => foreign::normalize_posix(path),
                Resolution::Symlink { path } => foreign::normalize_posix(path),
                _ => String::new(),
            };
            let from = if from == "." { "" } else { from };
            let rel = foreign::relative_posix(from, &path);
            format!("link:{}", if rel.is_empty() { "." } else { &rel })
        }
        _ => {
            let key = package_key(p).unwrap_or_default();
            let suffix = peer_suffix(graph, target, depth, cache);
            if p.name == alias {
                format!("{}{suffix}", &key[p.name.len() + 1..])
            } else {
                format!("{key}{suffix}")
            }
        }
    }
}

fn push_line(out: &mut String, indent: usize, text: &str) {
    for _ in 0..indent {
        out.push(' ');
    }
    out.push_str(text);
    out.push('\n');
}

fn flow_list(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|i| yaml::flow_scalar(i))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// pnpm-lock.yaml lockfileVersion 9.0. Peer bindings become snapshot key suffixes; packages
/// pnpm keeps out of `packages:` (workspaces, links) are `link:` references.
pub fn serialize(graph: &LockGraph) -> Result<String> {
    let mut cache: HashMap<PackageId, String> = HashMap::new();
    let mut out = String::with_capacity(graph.len() * 200 + 256);
    out.push_str("lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\n");

    let has_catalogs = !graph.catalogs.is_empty();
    if has_catalogs {
        out.push_str("\ncatalogs:\n");
        let mut groups: Vec<(&str, &BTreeMap<String, String>)> = Vec::new();
        if !graph.catalogs.default.is_empty() {
            groups.push(("default", &graph.catalogs.default));
        }
        for (name, entries) in &graph.catalogs.named {
            groups.push((name, entries));
        }
        groups.sort_by(|a, b| a.0.cmp(b.0));
        for (group, entries) in groups {
            push_line(&mut out, 2, &format!("{}:", yaml::scalar(group)));
            for (name, specifier) in entries {
                let literal = if group == "default" {
                    "catalog:".to_owned()
                } else {
                    format!("catalog:{group}")
                };
                // the version a workspace resolved the entry to
                let version = graph
                    .workspaces
                    .values()
                    .flat_map(|id| graph.packages[id.index()].dependencies.iter())
                    .find(|e| {
                        e.name == *name
                            && (e.literal == literal
                                || (group == "default" && e.literal == "catalog:default"))
                    })
                    .and_then(|e| e.target)
                    .and_then(|t| {
                        graph.packages[t.index()]
                            .resolution
                            .npm_version()
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| specifier.clone());
                push_line(&mut out, 4, &format!("{}:", yaml::scalar(name)));
                push_line(
                    &mut out,
                    6,
                    &format!("specifier: {}", yaml::scalar(specifier)),
                );
                push_line(&mut out, 6, &format!("version: {}", yaml::scalar(&version)));
            }
        }
    }
    if !graph.overrides.is_empty() {
        out.push_str("\noverrides:\n");
        for rule in &graph.overrides {
            push_line(
                &mut out,
                2,
                &format!(
                    "{}: {}",
                    yaml::scalar(&rule.selector.to_key()),
                    yaml::scalar(&rule.value)
                ),
            );
        }
    }
    if !graph.patched_dependencies.is_empty() {
        out.push_str("\npatchedDependencies:\n");
        for (key, patch) in &graph.patched_dependencies {
            push_line(&mut out, 2, &format!("{}:", yaml::scalar(key)));
            push_line(
                &mut out,
                4,
                &format!(
                    "hash: {}",
                    yaml::scalar(patch.hash.as_deref().unwrap_or_default())
                ),
            );
            push_line(&mut out, 4, &format!("path: {}", yaml::scalar(&patch.path)));
        }
    }

    out.push_str("\nimporters:\n");
    let mut importers: Vec<(String, PackageId)> = graph
        .workspaces
        .iter()
        .map(|(path, id)| {
            (
                if path.is_empty() {
                    ".".to_owned()
                } else {
                    path.clone()
                },
                *id,
            )
        })
        .collect();
    importers.sort_by(|a, b| (a.0 != ".").cmp(&(b.0 != ".")).then_with(|| a.0.cmp(&b.0)));
    for (path, id) in &importers {
        out.push('\n');
        push_line(&mut out, 2, &format!("{}:", yaml::scalar(path)));
        let pkg = &graph.packages[id.index()];
        let mut any = false;
        for (group, wanted) in [
            ("dependencies", Behavior::PROD),
            ("devDependencies", Behavior::DEV),
            ("optionalDependencies", Behavior::OPTIONAL),
        ] {
            let mut edges: Vec<&DepEdge> = pkg
                .dependencies
                .iter()
                .filter(|e| {
                    !e.behavior.is_peer()
                        && e.target.is_some()
                        && e.behavior.group() == wanted.group()
                })
                .collect();
            if edges.is_empty() {
                continue;
            }
            edges.sort_by(|a, b| a.name.cmp(&b.name));
            any = true;
            push_line(&mut out, 4, &format!("{group}:"));
            for e in edges {
                let Some(target) = e.target else { continue };
                let reference = reference_to(graph, &e.name, target, path, 0, &mut cache);
                push_line(&mut out, 6, &format!("{}:", yaml::scalar(&e.name)));
                push_line(
                    &mut out,
                    8,
                    &format!("specifier: {}", yaml::scalar(&e.literal)),
                );
                push_line(
                    &mut out,
                    8,
                    &format!("version: {}", yaml::scalar(&reference)),
                );
            }
        }
        if !any {
            // an importer with nothing locked is an empty mapping
            out.pop();
            out.push_str(" {}\n");
        }
    }

    // packages pnpm lists, by key; the first package of a key wins
    let mut listed: BTreeMap<String, PackageId> = BTreeMap::new();
    for id in graph.ids() {
        if let Some(key) = package_key(&graph.packages[id.index()]) {
            listed.entry(key).or_insert(id);
        }
    }
    if !listed.is_empty() {
        out.push_str("\npackages:\n");
    }
    for (key, id) in &listed {
        let p = &graph.packages[id.index()];
        out.push('\n');
        push_line(&mut out, 2, &format!("{}:", yaml::scalar(key)));
        let integrity = p.resolution.integrity().map(Integrity::to_sri);
        // pnpm records a `tarball:` only off the registry's `/<name>/-/<file>.tgz` path.
        let canonical = |url: &str, version: &str| {
            let file = p.name.rsplit('/').next().unwrap_or(&p.name);
            url.ends_with(&format!("/{}/-/{file}-{version}.tgz", p.name))
        };
        let resolution = match (&p.resolution, &integrity) {
            (
                Resolution::Npm {
                    tarball_url: Some(url),
                    version,
                    ..
                },
                Some(i),
            ) if canonical(url, version) => {
                format!("{{integrity: {}}}", yaml::flow_scalar(i))
            }
            (
                Resolution::Npm {
                    tarball_url: Some(url),
                    version,
                    ..
                },
                None,
            ) if canonical(url, version) => "{}".to_owned(),
            (
                Resolution::Npm {
                    tarball_url: Some(url),
                    ..
                },
                Some(i),
            ) => {
                format!(
                    "{{integrity: {}, tarball: {}}}",
                    yaml::flow_scalar(i),
                    yaml::flow_scalar(url)
                )
            }
            (
                Resolution::Npm {
                    tarball_url: Some(url),
                    ..
                },
                None,
            ) => format!("{{tarball: {}}}", yaml::flow_scalar(url)),
            (Resolution::Npm { .. }, Some(i)) => format!("{{integrity: {}}}", yaml::flow_scalar(i)),
            (Resolution::Npm { .. }, None) => "{}".to_owned(),
            (Resolution::RemoteTarball { url, .. }, i) => tarball_flow(url, i.as_deref()),
            (Resolution::Github { .. }, i) => tarball_flow(&key[p.name.len() + 1..], i.as_deref()),
            (Resolution::LocalTarball { path, .. }, i) => {
                tarball_flow(&format!("file:{path}"), i.as_deref())
            }
            (Resolution::Folder { path }, _) => format!(
                "{{directory: {}, type: directory}}",
                yaml::flow_scalar(path)
            ),
            (
                Resolution::Git {
                    url,
                    resolved_commit,
                    committish,
                    ..
                },
                _,
            ) => format!(
                "{{commit: {}, repo: {}, type: git}}",
                yaml::flow_scalar(if resolved_commit.is_empty() {
                    committish
                } else {
                    resolved_commit
                }),
                yaml::flow_scalar(url)
            ),
            _ => "{}".to_owned(),
        };
        push_line(&mut out, 4, &format!("resolution: {resolution}"));
        if !matches!(p.resolution, Resolution::Npm { .. }) && !p.version.is_empty() {
            push_line(
                &mut out,
                4,
                &format!("version: {}", yaml::scalar(&p.version)),
            );
        }
        let mut peers: Vec<&DepEdge> = p
            .dependencies
            .iter()
            .filter(|e| e.behavior.is_peer())
            .collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        peers.dedup_by(|a, b| a.name == b.name);
        if !peers.is_empty() {
            push_line(&mut out, 4, "peerDependencies:");
            for e in &peers {
                push_line(
                    &mut out,
                    6,
                    &format!("{}: {}", yaml::scalar(&e.name), yaml::scalar(&e.literal)),
                );
            }
            let optional: Vec<&&DepEdge> = peers
                .iter()
                .filter(|e| e.behavior.is_optional_peer())
                .collect();
            if !optional.is_empty() {
                push_line(&mut out, 4, "peerDependenciesMeta:");
                for e in optional {
                    push_line(&mut out, 6, &format!("{}:", yaml::scalar(&e.name)));
                    push_line(&mut out, 8, "optional: true");
                }
            }
        }
        if p.platform.os != OsSet::ALL {
            push_line(
                &mut out,
                4,
                &format!("os: {}", flow_list(&p.platform.os.to_list())),
            );
        }
        if p.platform.cpu != CpuSet::ALL {
            push_line(
                &mut out,
                4,
                &format!("cpu: {}", flow_list(&p.platform.cpu.to_list())),
            );
        }
        if p.platform.libc != LibcSet::NONE {
            push_line(
                &mut out,
                4,
                &format!("libc: {}", flow_list(&p.platform.libc.to_list())),
            );
        }
        if let Some(deprecated) = &p.meta.deprecated {
            push_line(
                &mut out,
                4,
                &format!("deprecated: {}", yaml::scalar(deprecated)),
            );
        }
        if !p.bin.is_none() {
            push_line(&mut out, 4, "hasBin: true");
        }
    }

    if !listed.is_empty() {
        out.push_str("\nsnapshots:\n");
    }
    let mut snapshots: BTreeMap<String, PackageId> = BTreeMap::new();
    for (key, id) in &listed {
        snapshots.insert(
            format!("{key}{}", peer_suffix(graph, *id, 0, &mut cache)),
            *id,
        );
    }
    for (key, id) in &snapshots {
        let p = &graph.packages[id.index()];
        out.push('\n');
        let mut groups: [(&str, Vec<(String, String)>); 2] = [
            ("dependencies", Vec::new()),
            ("optionalDependencies", Vec::new()),
        ];
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for e in &p.dependencies {
            let Some(target) = e.target else { continue };
            if !seen.insert(&e.name) {
                continue;
            }
            let optional = e.behavior.is_optional() || e.behavior.is_optional_peer();
            let reference = reference_to(graph, &e.name, target, "", 0, &mut cache);
            groups[usize::from(optional)]
                .1
                .push((e.name.clone(), reference));
        }
        if groups.iter().all(|(_, g)| g.is_empty()) {
            push_line(&mut out, 2, &format!("{}: {{}}", yaml::scalar(key)));
            continue;
        }
        push_line(&mut out, 2, &format!("{}:", yaml::scalar(key)));
        for (group, mut deps) in groups {
            if deps.is_empty() {
                continue;
            }
            deps.sort();
            push_line(&mut out, 4, &format!("{group}:"));
            for (name, reference) in deps {
                push_line(
                    &mut out,
                    6,
                    &format!("{}: {}", yaml::scalar(&name), yaml::scalar(&reference)),
                );
            }
        }
    }
    Ok(out)
}

fn tarball_flow(url: &str, integrity: Option<&str>) -> String {
    match integrity {
        Some(i) => format!(
            "{{integrity: {}, tarball: {}}}",
            yaml::flow_scalar(i),
            yaml::flow_scalar(url)
        ),
        None => format!("{{tarball: {}}}", yaml::flow_scalar(url)),
    }
}
