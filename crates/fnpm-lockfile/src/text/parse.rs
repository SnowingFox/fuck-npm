//! The bun.lock / fnpm.lock reader (port of bun `bun.lock.rs`
//! `parse_into_binary_lockfile`), producing a [`LockGraph`] whose edges are bound exactly
//! as bun binds them, then re-hoisted like bun's `Lockfile::resolve`.

use std::collections::BTreeSet;

use crate::fxhash::{FxHashMap as HashMap, FxHashSet as HashSet, map};

use fnpm_types::name::{is_safe_install_folder_name, split_name_and_maybe_version};
use fnpm_types::overrides::parse_package_segment;
use fnpm_types::spec::is_remote_tarball;
use fnpm_types::{
    Behavior, Bin, ConfigVersion, CpuSet, DepEdge, Integrity, LibcSet, LockGraph, OsSet,
    OverrideRule, OverrideSelector, Package, PackageId, PackageSelector, PatchedDependency,
    PeerMeta, PlatformConstraints, Resolution, SpecTag, infer_tag,
};

use super::{LockfileVersion, url_is_under_registry};
use crate::flat::{Flat, INVALID, VTag};
use crate::jsonc::{self, Node, Row};
use crate::semver;
use crate::tree;

/// A parse failure at a byte offset of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub pos: u32,
    pub message: String,
}

impl ParseError {
    fn at(pos: u32, message: impl Into<String>) -> Self {
        Self {
            pos,
            message: message.into(),
        }
    }
}

type PResult<T> = Result<T, ParseError>;

/// What the reader needs from the install's configuration.
#[derive(Clone, Copy, Default)]
pub(crate) struct ParseOptions<'a> {
    /// The registry URL serving a package name (scopes), for the lockfileVersion 2 check of
    /// off-registry tarballs. `None`: the default registry.
    pub registry_for: Option<&'a dyn Fn(&str) -> String>,
}

/// A parsed lockfile and the warnings bun would print while loading it.
pub(crate) struct Parsed {
    pub graph: LockGraph,
    pub warnings: Vec<ParseError>,
}

const GROUPS: [(&str, Behavior); 4] = [
    ("dependencies", Behavior::PROD),
    ("devDependencies", Behavior::DEV),
    ("optionalDependencies", Behavior::OPTIONAL),
    ("peerDependencies", Behavior::PEER),
];

fn objects<'n, 'a>(n: &'n Node<'a>) -> &'n [Row<'a>] {
    n.as_object().unwrap_or(&[])
}

struct DepsOut {
    edges: Vec<DepEdge>,
    /// Names of optional edges listed under `unresolved`.
    unresolved: Vec<String>,
    peer_meta: Vec<String>,
}

/// bun `parse_append_dependencies`: the four groups plus `optionalPeers` / `unresolved`.
fn parse_dependencies(obj: &Node<'_>, bundled: Option<(&str, &HashSet<&str>)>) -> PResult<DepsOut> {
    let mut optional_peers: HashSet<&str> = HashSet::default();
    let mut unresolved_names: Vec<&str> = Vec::new();
    for (key, is_optional_peers) in [("optionalPeers", true), ("unresolved", false)] {
        let Some(names) = obj.get(key) else { continue };
        let Some(items) = names.as_array() else {
            return Err(ParseError::at(names.pos, "Expected an array"));
        };
        for item in items {
            let Some(n) = item.as_str() else {
                return Err(ParseError::at(item.pos, "Expected a string"));
            };
            if is_optional_peers {
                optional_peers.insert(n);
            } else {
                unresolved_names.push(n);
            }
        }
    }
    let mut out = DepsOut {
        edges: Vec::new(),
        unresolved: Vec::new(),
        peer_meta: Vec::new(),
    };
    for (group, behavior) in GROUPS {
        let Some(deps) = obj.get(group) else { continue };
        let Some(rows) = deps.as_object() else {
            return Err(ParseError::at(deps.pos, "Expected an object"));
        };
        for row in rows {
            let Some(version) = row.value.as_str() else {
                return Err(ParseError::at(row.value.pos, "Expected a string"));
            };
            let name: &str = &row.key;
            if !valid_dependency(name, version) {
                return Err(ParseError::at(row.value.pos, "Invalid dependency version"));
            }
            let mut b = behavior;
            if behavior == Behavior::PEER && optional_peers.contains(name) {
                b |= Behavior::OPTIONAL;
                out.peer_meta.push(name.to_owned());
            }
            if let Some((path, set)) = bundled
                && !set.is_empty()
            {
                let loc = format!("{path}/{name}");
                if set.contains(loc.as_str()) {
                    b |= Behavior::BUNDLED;
                }
            }
            if b.is_optional() && unresolved_names.contains(&name) {
                out.unresolved.push(name.to_owned());
            }
            out.edges.push(DepEdge::new(name, version, b));
        }
    }
    Ok(out)
}

/// bun `dependency::parse` succeeds (only folder, tarball, git and GitHub specifiers can fail).
fn valid_dependency(name: &str, literal: &str) -> bool {
    match infer_tag(fnpm_types::spec::trim_literal(literal)) {
        SpecTag::Npm
        | SpecTag::DistTag
        | SpecTag::Workspace
        | SpecTag::Catalog
        | SpecTag::Symlink => true,
        _ => fnpm_types::DependencySpec::parse(name, literal).is_ok(),
    }
}

/// bun `OverrideMap::lockfile_range` + `is_selector_range`: a selector's range must be an
/// npm range (a target's may also be `catalog:` or an `npm:` alias).
fn valid_selector_range(name: &str, range: &str, is_target: bool) -> bool {
    if range.is_empty() {
        return true;
    }
    let spec = crate::flat::Spec::parse(range);
    let _ = name;
    match spec.tag {
        crate::flat::VTag::Npm => is_target || !spec.is_alias,
        crate::flat::VTag::Catalog => is_target,
        _ => false,
    }
}

fn parse_bin(info: &Node<'_>) -> Bin {
    if let Some(bin) = info.get("bin") {
        if let Some(rows) = bin.as_object() {
            let mut entries: Vec<(String, String)> = rows
                .iter()
                .filter_map(|r| Some((r.key.to_string(), r.value.as_str()?.to_owned())))
                .collect();
            return match entries.len() {
                0 => Bin::None,
                1 => {
                    let (name, path) = entries.pop().expect("one entry");
                    Bin::NamedFile { name, path }
                }
                _ => Bin::Map { entries },
            };
        }
        return match bin.as_str() {
            Some(s) if !s.is_empty() => Bin::File { path: s.to_owned() },
            _ => Bin::None,
        };
    }
    if let Some(dir) = info.get("binDir").and_then(Node::as_str)
        && !dir.is_empty()
    {
        return Bin::Dir {
            path: dir.to_owned(),
        };
    }
    Bin::None
}

fn str_list(n: &Node<'_>) -> Vec<String> {
    match (&n.value, n.as_array()) {
        (jsonc::Value::String(s), _) => vec![s.to_string()],
        (_, Some(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// bun `parse_integrity`: a supported hash, or empty. Anything else is an error at v2+ and
/// a warning before.
fn parse_integrity(
    s: &str,
    v: LockfileVersion,
    pos: u32,
    warnings: &mut Vec<ParseError>,
) -> PResult<Option<Integrity>> {
    if s.is_empty() {
        return Ok(None);
    }
    match Integrity::parse(s) {
        Ok(i) => Ok(Some(i)),
        Err(_) if v >= LockfileVersion::V2 => Err(ParseError::at(
            pos,
            "Unsupported or malformed integrity hash",
        )),
        Err(_) => {
            warnings.push(ParseError::at(
                pos,
                "Unsupported or malformed integrity hash; ignoring",
            ));
            Ok(None)
        }
    }
}

/// bun `Resolution::from_text_lockfile`.
fn resolution_from_text(s: &str) -> Option<Resolution> {
    if s.starts_with("root:") {
        return Some(Resolution::Root);
    }
    if let Some(p) = s.strip_prefix("link:") {
        return Some(Resolution::Symlink { path: p.to_owned() });
    }
    if let Some(p) = s.strip_prefix("workspace:") {
        return Some(Resolution::Workspace { path: p.to_owned() });
    }
    if let Some(p) = s.strip_prefix("file:") {
        return Some(Resolution::Folder { path: p.to_owned() });
    }
    match infer_tag(s) {
        SpecTag::Git => {
            let rest = s.strip_prefix("git+").unwrap_or(s);
            let (url, committish) = rest.rsplit_once('#').unwrap_or((rest, ""));
            Some(Resolution::Git {
                url: url.to_owned(),
                committish: committish.to_owned(),
                resolved_commit: String::new(),
                integrity: None,
            })
        }
        SpecTag::Github => {
            let rest = s.strip_prefix("github:").unwrap_or(s);
            let (before, committish) = rest.rsplit_once('#').map_or((rest, ""), |(b, c)| (b, c));
            let (owner, repo) = before.rsplit_once('/').unwrap_or(("", before));
            Some(Resolution::Github {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                committish: committish.to_owned(),
                resolved_commit: String::new(),
                integrity: None,
            })
        }
        SpecTag::Tarball => Some(if is_remote_tarball(s) {
            Resolution::RemoteTarball {
                url: s.to_owned(),
                integrity: None,
            }
        } else {
            Resolution::LocalTarball {
                path: s.to_owned(),
                integrity: None,
            }
        }),
        SpecTag::Npm => {
            // bun keeps the parsed version, so `=1.0.0` and `v1.0.0` are written `1.0.0`.
            let v = canonical_version(s);
            semver::is_version(v).then(|| Resolution::npm(v, None))
        }
        _ => None,
    }
}

/// A resolved version without what bun's `Version::parse` skips: leading whitespace, `v`
/// and `=`, and trailing whitespace.
fn canonical_version(s: &str) -> &str {
    s.trim_start_matches(|c: char| {
        matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0B' | '\x0C' | 'v' | '=')
    })
    .trim_end_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C'])
}

/// bun's `RowEvidence`: how good a binding found by the path walk is. Larger is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Evidence {
    Unbound,
    RootDependency,
    Walked,
    WalkedToRoot,
    Refused,
    OwnEntry,
    Unprovided,
}

enum Walk {
    Found(u32, Evidence),
    Unresolvable,
    InvalidKey,
}

/// bun `PkgMap::find_resolution`.
fn find_resolution(
    pkg_map: &HashMap<&str, u32>,
    pkg_path: &str,
    dep_name: &str,
    bundled: Option<&HashSet<&str>>,
) -> Walk {
    let mut base: &str = pkg_path;
    let mut first = true;
    let mut at_bundle_root = false;
    let mut key = String::with_capacity(pkg_path.len() + dep_name.len() + 1);
    loop {
        key.clear();
        key.push_str(base);
        let top = !first && base.is_empty();
        if !top {
            key.push('/');
        }
        key.push_str(dep_name);
        if let Some(&id) = pkg_map.get(key.as_str()) {
            let e = if first {
                Evidence::OwnEntry
            } else if top {
                Evidence::WalkedToRoot
            } else {
                Evidence::Walked
            };
            return Walk::Found(id, e);
        }
        if top || at_bundle_root {
            return Walk::Unresolvable;
        }
        if let Some(set) = bundled {
            at_bundle_root = set.contains(base);
        }
        match tree::parent_path(base) {
            Some(p) => base = p,
            None => return Walk::InvalidKey,
        }
        first = false;
    }
}

fn behavior_word(b: Behavior) -> &'static str {
    if b.contains(Behavior::DEV) {
        "dev"
    } else if b.contains(Behavior::OPTIONAL) {
        "optional"
    } else if b.contains(Behavior::PEER) {
        "peer"
    } else if b.contains(Behavior::WORKSPACE) {
        "workspace"
    } else {
        "prod"
    }
}

fn escape_control(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                c
            }
        })
        .collect()
}

/// Edges a fresh install may itself leave unresolved (bun `may_stay_unresolved`).
fn may_stay_unresolved(b: Behavior) -> bool {
    b.intersects(Behavior::OPTIONAL | Behavior::PEER)
}

fn sort_edges(p: &mut Package) {
    p.dependencies.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.behavior.group().cmp(&b.behavior.group()))
    });
}

/// A package being read, with the rows of its dependency object that need fix-ups.
struct Pending {
    unresolved: Vec<String>,
}

/// Parses bun.lock text.
pub(crate) fn parse(src: &str, opts: ParseOptions<'_>) -> PResult<Parsed> {
    let root = jsonc::parse(src).map_err(|e| ParseError::at(e.pos, e.message))?;
    if !root.is_object() {
        return Err(ParseError::at(root.pos, "Expected an object"));
    }
    let mut warnings = Vec::new();

    let Some(v) = root.get("lockfileVersion") else {
        return Err(ParseError::at(root.pos, "Missing lockfile version"));
    };
    let n = match v.value {
        jsonc::Value::Number(n) if (0.0..=f64::from(u32::MAX)).contains(&n) && n.fract() == 0.0 => {
            n as u32
        }
        _ => return Err(ParseError::at(v.pos, "Invalid lockfile version")),
    };
    let Some(version) = LockfileVersion::from_int(n) else {
        return Err(ParseError::at(
            v.pos,
            format!(
                "Unsupported lockfile version {n}. This lockfile was likely created by a newer version of fnpm. (This is fnpm v{}, which supports lockfile versions up to {}.)",
                env!("CARGO_PKG_VERSION"),
                LockfileVersion::CURRENT as u32
            ),
        ));
    };

    let config_version = match root.get("configVersion") {
        None => ConfigVersion::V0,
        Some(c) => match c.value {
            // Versions above the current one clamp to it (bun `ConfigVersion::from_expr`).
            jsonc::Value::Number(x) if x.fract() == 0.0 && x >= 1.0 => ConfigVersion::CURRENT,
            jsonc::Value::Number(x) if x.fract() == 0.0 && x >= 0.0 => ConfigVersion::V0,
            _ => {
                return Err(ParseError::at(
                    c.pos,
                    "Invalid \"configVersion\". Expected a number",
                ));
            }
        },
    };

    let mut trusted: Option<BTreeSet<String>> = None;
    if let Some(t) = root.get("trustedDependencies") {
        let Some(items) = t.as_array() else {
            return Err(ParseError::at(t.pos, "Expected an array"));
        };
        let mut set = BTreeSet::new();
        for i in items {
            let Some(s) = i.as_str() else {
                return Err(ParseError::at(i.pos, "Expected a string"));
            };
            set.insert(s.to_owned());
        }
        trusted = Some(set);
    }

    let mut patched = std::collections::BTreeMap::new();
    if let Some(p) = root.get("patchedDependencies") {
        let Some(rows) = p.as_object() else {
            return Err(ParseError::at(p.pos, "Expected an object"));
        };
        for r in rows {
            let Some(path) = r.value.as_str() else {
                return Err(ParseError::at(r.value.pos, "Expected a string"));
            };
            patched.insert(
                r.key.to_string(),
                PatchedDependency {
                    path: path.to_owned(),
                    hash: None,
                },
            );
        }
    }

    let mut ext_records: Vec<super::extensions::ExtRecord> = Vec::new();
    if let Some(e) = root.get("packageExtensions") {
        for r in objects(e) {
            let list = |k: &str| {
                r.value
                    .get(k)
                    .and_then(Node::as_array)
                    .map_or_else(Vec::new, |a| {
                        a.iter()
                            .filter_map(|i| i.as_str().map(str::to_owned))
                            .collect()
                    })
            };
            ext_records.push((
                r.key.to_string(),
                list("dependencies"),
                list("peerDependencies"),
                list("optionalPeers"),
            ));
        }
    }

    let mut overrides: Vec<OverrideRule> = Vec::new();
    if let Some(o) = root.get("overrides") {
        let Some(rows) = o.as_object() else {
            return Err(ParseError::at(o.pos, "Expected an object"));
        };
        for r in rows {
            if r.key.is_empty() {
                return Err(ParseError::at(r.key_pos, "Expected a non-empty string"));
            }
            if let Some(value) = r.value.as_str() {
                if !valid_dependency(&r.key, value) {
                    return Err(ParseError::at(r.value.pos, "Invalid override version"));
                }
                overrides.push(OverrideRule {
                    selector: OverrideSelector {
                        parent: None,
                        target: PackageSelector {
                            name: r.key.to_string(),
                            range: String::new(),
                        },
                    },
                    value: value.to_owned(),
                });
                continue;
            }
            let Some(children) = r.value.as_object() else {
                return Err(ParseError::at(
                    r.value.pos,
                    "Expected a string or an object",
                ));
            };
            let Ok(parent) = parse_package_segment(&r.key) else {
                return Err(ParseError::at(r.key_pos, "Invalid override key"));
            };
            for c in children {
                let Some(value) = c.value.as_str() else {
                    return Err(ParseError::at(c.value.pos, "Expected a string"));
                };
                let selector = if c.key == "." {
                    OverrideSelector {
                        parent: None,
                        target: parent.clone(),
                    }
                } else {
                    let Ok(target) = parse_package_segment(&c.key) else {
                        return Err(ParseError::at(c.key_pos, "Invalid override key"));
                    };
                    OverrideSelector {
                        parent: Some(parent.clone()),
                        target,
                    }
                };
                let target_ok =
                    valid_selector_range(&selector.target.name, &selector.target.range, true);
                let parent_ok = selector
                    .parent
                    .as_ref()
                    .is_none_or(|p| valid_selector_range(&p.name, &p.range, false));
                if !parent_ok || !target_ok || !valid_dependency(&selector.target.name, value) {
                    return Err(ParseError::at(c.value.pos, "Invalid override version"));
                }
                overrides.push(OverrideRule {
                    selector,
                    value: value.to_owned(),
                });
            }
        }
    }

    let mut catalogs = fnpm_types::Catalogs::default();
    let parse_catalog =
        |rows: &[Row<'_>], out: &mut std::collections::BTreeMap<String, String>| -> PResult<()> {
            for r in rows {
                if r.key.is_empty() {
                    return Err(ParseError::at(r.key_pos, "Expected a non-empty string"));
                }
                let Some(v) = r.value.as_str() else {
                    return Err(ParseError::at(r.value.pos, "Expected a string"));
                };
                if !valid_dependency(&r.key, v) {
                    return Err(ParseError::at(r.value.pos, "Invalid catalog version"));
                }
                if out.insert(r.key.to_string(), v.to_owned()).is_some() {
                    return Err(ParseError::at(r.key_pos, "Duplicate catalog entry"));
                }
            }
            Ok(())
        };
    if let Some(c) = root.get("catalog") {
        let Some(rows) = c.as_object() else {
            return Err(ParseError::at(c.pos, "Expected an object"));
        };
        parse_catalog(rows, &mut catalogs.default)?;
    }
    if let Some(c) = root.get("catalogs") {
        let Some(rows) = c.as_object() else {
            return Err(ParseError::at(c.pos, "Expected an object"));
        };
        for r in rows {
            if r.key.is_empty() {
                return Err(ParseError::at(r.key_pos, "Expected a non-empty string"));
            }
            let Some(entries) = r.value.as_object() else {
                return Err(ParseError::at(r.value.pos, "Expected an object"));
            };
            let group = catalogs.named.entry(r.key.to_string()).or_default();
            parse_catalog(entries, group)?;
        }
    }

    // ── workspaces ──
    let Some(ws) = root.get("workspaces").filter(|w| w.is_object()) else {
        return Err(ParseError::at(
            root.pos,
            "Missing a workspaces object property",
        ));
    };
    let mut root_obj: Option<&Node<'_>> = None;
    // The last row of each name, in first-seen order.
    let mut member_rows: Vec<&Row<'_>> = Vec::new();
    let mut member_index: HashMap<&str, usize> = HashMap::default();
    let mut seen_paths: HashSet<&str> = HashSet::default();
    for r in objects(ws) {
        if !seen_paths.insert(&r.key) {
            warnings.push(ParseError::at(
                r.key_pos,
                format!(
                    "Duplicate key \"{}\" in object literal",
                    escape_control(&r.key)
                ),
            ));
        }
        if !r.value.is_object() {
            return Err(ParseError::at(r.value.pos, "Expected an object"));
        }
        if r.key.is_empty() {
            if root_obj.is_some() {
                return Err(ParseError::at(r.key_pos, "Duplicate root package"));
            }
            root_obj = Some(&r.value);
            continue;
        }
        let Some(name) = r.value.get("name").and_then(Node::as_str) else {
            return Err(ParseError::at(
                r.value.pos,
                "Expected a string name property",
            ));
        };
        match member_index.get(name) {
            Some(&i) => member_rows[i] = r,
            None => {
                member_index.insert(name, member_rows.len());
                member_rows.push(r);
            }
        }
        if let Some(v) = r.value.get("version") {
            let Some(s) = v.as_str() else {
                return Err(ParseError::at(v.pos, "Expected a string version property"));
            };
            if !semver::is_version(s) {
                return Err(ParseError::at(v.pos, "Invalid semver version"));
            }
        }
    }
    let Some(root_obj) = root_obj else {
        return Err(ParseError::at(ws.pos, "Expected root package"));
    };
    let root_name = match root_obj.get("name") {
        Some(n) => match n.as_str() {
            Some(s) => s.to_owned(),
            None => return Err(ParseError::at(n.pos, "Expected a string")),
        },
        None => String::new(),
    };

    let mut graph = LockGraph::new(Package::new(root_name, "", Resolution::Root));
    graph.lockfile_version = version as u32;
    graph.config_version = config_version;
    graph.trusted_dependencies = trusted;
    graph.patched_dependencies = patched;
    graph.overrides = overrides;
    graph.catalogs = catalogs;
    let mut pending: Vec<Pending> = Vec::new();
    {
        let d = parse_dependencies(root_obj, None)?;
        let root = graph.root_mut();
        for n in d.peer_meta {
            root.peer_meta.insert(n, PeerMeta { optional: true });
        }
        root.dependencies = d.edges;
        pending.push(Pending {
            unresolved: d.unresolved,
        });
    }

    let mut by_key: HashMap<(String, String), u32> = map(1024);
    by_key.insert((graph.root().name.clone(), Resolution::Root.to_string()), 0);
    let mut pkg_map: HashMap<&str, u32> = map(1024);
    let mut members: Vec<u32> = Vec::new();

    if version != LockfileVersion::V0 {
        for r in &member_rows {
            let name = r
                .value
                .get("name")
                .and_then(Node::as_str)
                .unwrap_or_default();
            let ver = r
                .value
                .get("version")
                .and_then(Node::as_str)
                .unwrap_or_default();
            let mut p = Package::new(
                name,
                ver,
                Resolution::Workspace {
                    path: r.key.to_string(),
                },
            );
            let d = parse_dependencies(&r.value, None)?;
            p.dependencies = d.edges;
            for n in d.peer_meta {
                p.peer_meta.insert(n, PeerMeta { optional: true });
            }
            p.bin = parse_bin(&r.value);
            let key = (p.name.clone(), p.resolution.to_string());
            let id = match by_key.get(&key) {
                Some(&id) => id,
                None => {
                    let id = graph.add_workspace(p).0;
                    by_key.insert(key, id);
                    pending.push(Pending {
                        unresolved: d.unresolved,
                    });
                    id
                }
            };
            pkg_map.insert(name, id);
            members.push(id);
        }
    }

    // ── packages ──
    let rows: &[Row<'_>] = match root.get("packages") {
        None => &[],
        Some(p) => match p.as_object() {
            Some(rows) => rows,
            None => return Err(ParseError::at(p.pos, "Expected an object")),
        },
    };
    let mut bundled_pkgs: HashSet<&str> = HashSet::default();
    for r in rows {
        let Some(items) = r.value.as_array() else {
            return Err(ParseError::at(r.value.pos, "Expected an array"));
        };
        if let Some(info) = items.iter().find(|i| i.is_object())
            && info.get("bundled").and_then(Node::as_bool) == Some(true)
        {
            bundled_pkgs.insert(&r.key);
        }
    }
    let registry_for = |name: &str| {
        opts.registry_for
            .map_or_else(|| super::DEFAULT_REGISTRY.to_owned(), |f| f(name))
    };

    // (path, package id) in file order.
    let mut placements: Vec<(&str, u32, u32)> = Vec::with_capacity(rows.len());
    'rows: for r in rows {
        let path: &str = &r.key;
        let items = r.value.as_array().unwrap_or(&[]);
        if items.is_empty() {
            return Err(ParseError::at(r.value.pos, "Missing package info"));
        }
        let mut i = 0usize;
        let Some(res_info) = items[0].as_str() else {
            return Err(ParseError::at(items[0].pos, "Expected a string"));
        };
        i += 1;
        let (name, res_str) = if let Some(rest) = res_info
            .strip_prefix('@')
            .filter(|r| r.starts_with("root:"))
        {
            ("", rest)
        } else {
            match split_name_and_maybe_version(res_info) {
                (n, Some(v)) => (n, v),
                _ => return Err(ParseError::at(items[0].pos, "Invalid package resolution")),
            }
        };
        if !name.is_empty() && !is_safe_install_folder_name(name) {
            return Err(ParseError::at(items[0].pos, "invalid package name"));
        }
        let Some(mut res) = resolution_from_text(res_str) else {
            return Err(ParseError::at(
                items[0].pos,
                format!("Unexpected resolution: {}", escape_control(res_str)),
            ));
        };

        let mut npm_url_needs_integrity = false;
        if let Resolution::Npm { tarball_url, .. } = &mut res {
            let Some(reg) = items.get(i) else {
                return Err(ParseError::at(r.value.pos, "Missing npm registry"));
            };
            i += 1;
            let Some(reg) = reg.as_str() else {
                return Err(ParseError::at(reg.pos, "Expected a string"));
            };
            if !reg.is_empty() {
                let configured = registry_for(name);
                npm_url_needs_integrity = !url_is_under_registry(reg, &configured)
                    && !url_is_under_registry(reg, super::DEFAULT_REGISTRY);
                *tarball_url = Some(reg.to_owned());
            }
        }

        if version != LockfileVersion::V0
            && let Resolution::Workspace { path: ws_path } = &res
        {
            if pkg_map.contains_key(path) {
                continue;
            }
            for &m in &members {
                if matches!(&graph.packages[m as usize].resolution, Resolution::Workspace { path: p } if p == ws_path)
                {
                    pkg_map.insert(path, m);
                    placements.push((path, m, r.key_pos));
                    continue 'rows;
                }
            }
            return Err(ParseError::at(
                items[0].pos,
                format!("Unknown workspace: '{}'", escape_control(ws_path)),
            ));
        }

        let mut pkg = Package::new(name, res.npm_version().unwrap_or(""), Resolution::Root);
        let mut unresolved = Vec::new();
        let is_ws = matches!(res, Resolution::Workspace { .. });
        match &res {
            Resolution::Root => {
                let Some(bin_obj) = items.get(i) else {
                    return Err(ParseError::at(
                        r.value.pos,
                        "Missing package binaries object",
                    ));
                };
                i += 1;
                if !bin_obj.is_object() {
                    return Err(ParseError::at(bin_obj.pos, "Expected an object"));
                }
                pkg.bin = parse_bin(bin_obj);
            }
            _ if is_ws && version != LockfileVersion::V0 => {}
            _ => {
                let Some(info) = items.get(i) else {
                    return Err(ParseError::at(r.value.pos, "Missing dependencies object"));
                };
                i += 1;
                if !info.is_object() {
                    return Err(ParseError::at(info.pos, "Expected an object"));
                }
                let d = parse_dependencies(info, Some((path, &bundled_pkgs)))?;
                pkg.dependencies = d.edges;
                unresolved = d.unresolved;
                for n in d.peer_meta {
                    pkg.peer_meta.insert(n, PeerMeta { optional: true });
                }
                pkg.bin = parse_bin(info);
                if !is_ws {
                    let os = info
                        .get("os")
                        .map(|n| OsSet::from_list(&str_list(n)))
                        .unwrap_or(OsSet::ALL);
                    let cpu = info
                        .get("cpu")
                        .map(|n| CpuSet::from_list(&str_list(n)))
                        .unwrap_or(CpuSet::ALL);
                    let libc = info
                        .get("libc")
                        .map(|n| LibcSet::from_package_list(&str_list(n)))
                        .unwrap_or(LibcSet::NONE);
                    pkg.platform = PlatformConstraints { os, cpu, libc };
                    if info.get("hasInstallScript").and_then(Node::as_bool) == Some(true) {
                        pkg.meta.has_install_script = true;
                    }
                    if let Some(dep) = info.get("deprecated").and_then(Node::as_str) {
                        pkg.meta.deprecated = Some(dep.to_owned());
                    }
                    if info.get("bundled").and_then(Node::as_bool) == Some(true) {
                        pkg.meta.bundled = true;
                    }
                }
            }
        }

        match &mut res {
            Resolution::Npm { integrity, .. } => {
                let Some(item) = items.get(i) else {
                    return Err(ParseError::at(r.value.pos, "Missing integrity"));
                };
                let Some(s) = item.as_str() else {
                    return Err(ParseError::at(item.pos, "Expected a string"));
                };
                *integrity = parse_integrity(s, version, item.pos, &mut warnings)?;
                if version >= LockfileVersion::V2 && npm_url_needs_integrity && integrity.is_none()
                {
                    return Err(ParseError::at(
                        item.pos,
                        "Missing integrity hash for npm package resolved to a tarball URL outside the configured registry",
                    ));
                }
            }
            Resolution::Git {
                resolved_commit, ..
            }
            | Resolution::Github {
                resolved_commit, ..
            } => {
                let is_github = matches!(res_str_kind(res_str), SpecTag::Github);
                let Some(item) = items.get(i) else {
                    return Err(ParseError::at(r.value.pos, "Missing git dependency tag"));
                };
                i += 1;
                let Some(tag) = item.as_str() else {
                    return Err(ParseError::at(item.pos, "Expected a string"));
                };
                let enforce = is_github || version >= LockfileVersion::V2;
                if tag.len() > super::MAX_RESOLVED_TAG_LEN
                    || (enforce && !super::is_safe_resolved_tag(tag))
                {
                    return Err(ParseError::at(item.pos, "Invalid git dependency tag"));
                }
                *resolved_commit = tag.to_owned();
            }
            _ => {}
        }
        if matches!(
            res,
            Resolution::LocalTarball { .. }
                | Resolution::RemoteTarball { .. }
                | Resolution::Git { .. }
                | Resolution::Github { .. }
        ) && let Some(item) = items.get(i)
            && let Some(s) = item.as_str()
        {
            let integ = parse_integrity(s, version, item.pos, &mut warnings)?;
            if let Some(integ) = integ {
                res.set_integrity(integ);
            }
        }

        pkg.resolution = res;
        let key = (pkg.name.clone(), pkg.resolution.to_string());
        let is_root_res = matches!(pkg.resolution, Resolution::Root);
        let id = match by_key.get(&key) {
            Some(&id) => {
                if id == 0 && is_root_res {
                    graph.packages[0].bin = pkg.bin;
                }
                id
            }
            None => {
                let is_ws = matches!(pkg.resolution, Resolution::Workspace { .. });
                let ws_path = match &pkg.resolution {
                    Resolution::Workspace { path } => Some(path.clone()),
                    _ => None,
                };
                let id = graph.push(pkg).0;
                if is_ws && let Some(p) = ws_path {
                    graph.workspaces.insert(p, PackageId(id));
                }
                by_key.insert(key, id);
                pending.push(Pending { unresolved });
                id
            }
        };
        let is_folder = matches!(
            graph.packages[id as usize].resolution,
            Resolution::Folder { .. }
        );
        if let Some(&existing) = pkg_map.get(path) {
            let replaceable = !is_folder
                && members.contains(&existing)
                && graph.packages[existing as usize].name == path;
            if !replaceable {
                return Err(ParseError::at(r.key_pos, "Duplicate package path"));
            }
        }
        pkg_map.insert(path, id);
        placements.push((path, id, r.key_pos));
    }

    for p in &mut graph.packages {
        sort_edges(p);
    }
    if version == LockfileVersion::V0 {
        // A v0 lockfile creates its workspace packages from `packages` rows. bun's reader then
        // never binds their dependencies (its member list stays empty); bind them like v1's.
        members = graph
            .workspaces
            .iter()
            .filter(|(p, _)| !p.is_empty())
            .map(|(_, id)| id.0)
            .collect();
    }
    bind(
        graph,
        &pkg_map,
        &placements,
        &members,
        &bundled_pkgs,
        &ext_records,
        &pending,
        warnings,
    )
}

fn res_str_kind(s: &str) -> SpecTag {
    infer_tag(s)
}

/// Binds every edge the way bun's reader does, then hoists like `Lockfile::resolve`.
#[allow(clippy::too_many_arguments)]
fn bind(
    mut graph: LockGraph,
    pkg_map: &HashMap<&str, u32>,
    placements: &[(&str, u32, u32)],
    members: &[u32],
    bundled: &HashSet<&str>,
    ext_records: &[super::extensions::ExtRecord],
    pending: &[Pending],
    warnings: Vec<ParseError>,
) -> PResult<Parsed> {
    super::extensions::rules_from_records(&mut graph, ext_records);
    let targets: Vec<((u32, u32), u32)> = {
        let mut flat = Flat::new(&graph);
        bind_flat(&mut flat, pkg_map, placements, members, bundled)?;
        for (pkg, pd) in pending.iter().enumerate() {
            if pd.unresolved.is_empty() || pkg >= flat.lists.len() {
                continue;
            }
            for d in flat.list(pkg as u32) {
                let dep = &flat.deps[d as usize];
                if dep.behavior.is_optional() && pd.unresolved.iter().any(|n| n == dep.name) {
                    flat.resolutions[d as usize] = INVALID;
                }
            }
        }
        flat.tag_workspace_links();
        let _ = tree::resolve(&mut flat);
        flat.deps
            .iter()
            .zip(&flat.resolutions)
            .filter_map(|(d, &r)| d.edge.map(|e| (e, r)))
            .collect()
    };
    for ((p, e), r) in targets {
        graph.packages[p as usize].dependencies[e as usize].target =
            (r != INVALID).then_some(PackageId(r));
    }
    Ok(Parsed { graph, warnings })
}

fn bind_flat(
    flat: &mut Flat<'_>,
    pkg_map: &HashMap<&str, u32>,
    placements: &[(&str, u32, u32)],
    members: &[u32],
    bundled: &HashSet<&str>,
) -> PResult<()> {
    let n = flat.graph.packages.len() as u32;
    let member_named = |flat: &Flat<'_>, name: u32| {
        members
            .iter()
            .copied()
            .find(|&m| m < n && flat.pkg_name_id[m as usize] == name)
    };
    let linked_member = |flat: &Flat<'_>, d: u32| -> Option<u32> {
        let name = flat.version_based_peer(d)?;
        let dep = &flat.deps[d as usize];
        if dep.resolved.tag != VTag::Npm {
            return None;
        }
        flat.linked_workspace(name, dep.resolved.range)?;
        member_named(flat, name)
    };
    let root_peer = |flat: &Flat<'_>, d: u32| {
        flat.version_based_peer(d)?;
        (pkg_map.get(flat.deps[d as usize].name) == Some(&0)).then_some(0)
    };
    let own_peer = |flat: &Flat<'_>, d: u32| {
        root_peer(flat, d)
            .or_else(|| linked_member(flat, d))
            .or_else(|| flat.peer_by_version(d, |_| true))
    };

    // Root dependencies.
    for d in flat.list(0) {
        let dep = &flat.deps[d as usize];
        if dep.edge.is_none() {
            continue;
        }
        let named = pkg_map.get(dep.name).copied();
        let bound = if dep.behavior.is_workspace() && !named.is_some_and(|id| members.contains(&id))
        {
            member_named(flat, dep.name_id).or(named)
        } else {
            own_peer(flat, d).or(named)
        };
        match bound {
            Some(id) => flat.resolutions[d as usize] = id,
            None if may_stay_unresolved(dep.behavior) => {}
            None => {
                return Err(ParseError::at(
                    0,
                    format!(
                        "Failed to resolve root {} dependency '{}'",
                        behavior_word(dep.behavior),
                        escape_control(dep.name)
                    ),
                ));
            }
        }
    }

    // Workspace dependencies.
    for &m in members {
        let ws_name = flat.graph.packages[m as usize].name.as_str();
        let unlinked = pkg_map.get(ws_name) != Some(&m);
        let prefix = if unlinked { "./" } else { "" };
        for d in flat.list(m) {
            let dep = &flat.deps[d as usize];
            let local = format!("{prefix}{ws_name}/{}", dep.name);
            let bound = own_peer(flat, d).or_else(|| {
                pkg_map
                    .get(local.as_str())
                    .or_else(|| pkg_map.get(dep.name))
                    .copied()
            });
            match bound {
                Some(id) => flat.resolutions[d as usize] = id,
                None if unlinked || may_stay_unresolved(dep.behavior) => {}
                None => {
                    return Err(ParseError::at(
                        0,
                        format!(
                            "Failed to resolve {} dependency '{}' for package '{}'",
                            behavior_word(dep.behavior),
                            escape_control(dep.name),
                            escape_control(ws_name)
                        ),
                    ));
                }
            }
        }
    }

    // Every other package, once per path it is printed at.
    let mut bound_by = vec![Evidence::Unbound; flat.deps.len()];
    let root_dep_names: HashSet<u32> = flat
        .list(0)
        .map(|d| flat.deps[d as usize].name_id)
        .collect();
    for &(path, id, key_pos) in placements {
        let p = &flat.graph.packages[id as usize];
        let is_ws = matches!(p.resolution, Resolution::Workspace { .. });
        if id == 0 || (is_ws && pkg_map.get(p.name.as_str()) == Some(&id)) {
            continue;
        }
        for d in flat.list(id) {
            let dep = &flat.deps[d as usize];
            if dep.version.tag == VTag::Uninitialized {
                continue;
            }
            let by_version = if is_ws {
                own_peer(flat, d)
            } else {
                root_peer(flat, d).or_else(|| flat.peer_by_version(d, |_| true))
            };
            let res_id = match by_version {
                Some(r) => r,
                None => {
                    let walk_bound = dep.behavior.is_optional_peer().then_some(bundled);
                    match find_resolution(pkg_map, path, dep.name, walk_bound) {
                        Walk::Found(r, mut ev) => {
                            let own = ev == Evidence::OwnEntry;
                            if ev == Evidence::WalkedToRoot
                                && dep.behavior.is_peer()
                                && root_dep_names.contains(&dep.name_id)
                            {
                                ev = Evidence::RootDependency;
                            } else if dep.behavior.is_optional_peer()
                                && !own
                                && !flat.satisfies(r, &dep.resolved)
                            {
                                ev = Evidence::Refused;
                            }
                            if bound_by[d as usize] > ev {
                                continue;
                            }
                            bound_by[d as usize] = ev;
                            r
                        }
                        Walk::InvalidKey => {
                            return Err(ParseError::at(key_pos, "Invalid package path"));
                        }
                        Walk::Unresolvable => {
                            if dep.behavior.is_optional_peer() {
                                bound_by[d as usize] = Evidence::Unprovided;
                                flat.resolutions[d as usize] = INVALID;
                            }
                            if may_stay_unresolved(dep.behavior) {
                                continue;
                            }
                            return Err(ParseError::at(
                                key_pos,
                                format!(
                                    "Failed to resolve {} dependency '{}' for package '{}'",
                                    behavior_word(dep.behavior),
                                    escape_control(dep.name),
                                    escape_control(path)
                                ),
                            ));
                        }
                    }
                }
            };
            flat.resolutions[d as usize] = res_id;
        }
    }
    Ok(())
}
