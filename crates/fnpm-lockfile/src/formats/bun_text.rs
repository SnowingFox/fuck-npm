//! `bun.lock`, bun's text lockfile (a port of bun `install/lockfile/bun.lock.rs`): reading
//! lockfileVersion 0-3 for migration, and writing for `fnpm-lock export --to bun`.
//!
//! The file is JSONC. `packages` is keyed by hoisted install path (`a`, `a/b`,
//! `<workspace name>/c`), and each dependency is bound the way bun binds it: the root's and
//! the workspaces' by name, a package's by walking up its path (`find_resolution`), and a
//! required peer by the highest locked version in its range (the fresh resolver's choice).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::migration::json::{self, Json, Obj};
use fnpm_types::name::{is_safe_install_folder_name, split_name_and_maybe_version};
use fnpm_types::overrides::{PackageSelector, parse_package_segment};
use fnpm_types::spec::is_remote_tarball;
use fnpm_types::{
    Behavior, Bin, ConfigVersion, CpuSet, DepEdge, DependencySpec, Integrity, LibcSet, LockGraph,
    OsSet, OverrideRule, OverrideSelector, Package, PackageId, PatchedDependency,
    PlatformConstraints, Resolution, SpecTag, infer_tag,
};

use crate::migration::{
    self, MigrateOptions, NodeSemver, Semver, is_safe_resolved_tag, url_is_under_registry,
};
use crate::tree::Tree;
use crate::{Error, Result};

/// The newest bun.lock version (`lockfileVersion`) this reader accepts and the writer emits.
pub const CURRENT_VERSION: u32 = 3;

const BUN_LOCK: &str = "bun.lock";

/// The groups in the order bun writes and reads them.
const GROUPS: [(&str, Behavior); 4] = [
    ("dependencies", Behavior::PROD),
    ("devDependencies", Behavior::DEV),
    ("optionalDependencies", Behavior::OPTIONAL),
    ("peerDependencies", Behavior::PEER),
];

/// Parses a bun.lock with the default registry. The graph keeps bun's `configVersion` (V0
/// when the file has none) and records bun.lock's `lockfileVersion` in
/// [`LockGraph::lockfile_version`].
pub fn parse(bytes: &[u8]) -> Result<LockGraph> {
    parse_with(bytes, &MigrateOptions::default()).map(|parsed| parsed.graph)
}

/// The graph and the install path of every `packages` entry.
#[derive(Debug, Clone)]
pub struct Parsed {
    /// The resolved graph.
    pub graph: LockGraph,
    /// `(packages key, package)` in file order.
    pub paths: Vec<(String, PackageId)>,
    /// The file's `lockfileVersion`.
    pub version: u32,
}

/// Parses with registry settings: a tarball URL outside both the configured and the default
/// registry needs an integrity hash from lockfileVersion 2 on.
pub fn parse_with(bytes: &[u8], options: &MigrateOptions) -> Result<Parsed> {
    Parser::run(bytes, options, &NodeSemver)
}

fn err(message: impl Into<String>) -> Error {
    Error::Parse {
        path: PathBuf::from(BUN_LOCK),
        message: message.into(),
    }
}

/// bun `Resolution::from_text_lockfile`: the part after `name@` of a package tuple.
fn resolution_from_text(res: &str, semver: &dyn Semver) -> Option<Resolution> {
    if res.starts_with("root:") {
        return Some(Resolution::Root);
    }
    if let Some(path) = res.strip_prefix("link:") {
        return Some(Resolution::Symlink {
            path: path.to_owned(),
        });
    }
    if let Some(path) = res.strip_prefix("workspace:") {
        return Some(Resolution::Workspace {
            path: path.to_owned(),
        });
    }
    if let Some(path) = res.strip_prefix("file:") {
        return Some(Resolution::Folder {
            path: path.to_owned(),
        });
    }
    match infer_tag(res) {
        SpecTag::Git => {
            let rest = res.strip_prefix("git+").unwrap_or(res);
            let (url, committish) = match rest.rfind('#') {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (rest, ""),
            };
            Some(Resolution::Git {
                url: url.to_owned(),
                committish: committish.to_owned(),
                resolved_commit: String::new(),
                integrity: None,
            })
        }
        SpecTag::Github => {
            let rest = res.strip_prefix("github:").unwrap_or(res);
            let (before, committish) = match rest.rfind('#') {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (rest, ""),
            };
            let (owner, repo) = match before.rfind('/') {
                Some(i) => (&before[..i], &before[i + 1..]),
                None => ("", before),
            };
            Some(Resolution::Github {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                committish: committish.to_owned(),
                resolved_commit: String::new(),
                integrity: None,
            })
        }
        SpecTag::Tarball if is_remote_tarball(res) => Some(Resolution::RemoteTarball {
            url: res.to_owned(),
            integrity: None,
        }),
        SpecTag::Tarball => Some(Resolution::LocalTarball {
            path: res.to_owned(),
            integrity: None,
        }),
        SpecTag::Npm if is_full_version(res) && semver.is_valid(res) => Some(Resolution::Npm {
            version: res.to_owned(),
            tarball_url: None,
            integrity: None,
        }),
        _ => None,
    }
}

/// bun `ConfigVersion::from_expr`: 0 and 1 are themselves, a larger integer is clamped to the
/// newest version, anything else is invalid.
fn config_version(v: f64) -> Option<ConfigVersion> {
    if v == 0.0 {
        Some(ConfigVersion::V0)
    } else if v == 1.0 {
        Some(ConfigVersion::V1)
    } else if v.trunc() == v && v > f64::from(u8::from(ConfigVersion::CURRENT)) {
        Some(ConfigVersion::CURRENT)
    } else {
        None
    }
}

/// bun requires major, minor and patch in a resolved version.
fn is_full_version(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let core = core.strip_prefix('v').unwrap_or(core);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// bun `Repository::resolved_commit`: the part of a `.bun-tag` after its last `-`.
pub(crate) fn resolved_commit(tag: &str) -> &str {
    match tag.rfind('-') {
        Some(i) => &tag[i + 1..],
        None => tag,
    }
}

/// bun's `Negatable` name tables, in the order bun.lock prints them.
const OS_NAMES: [&str; 8] = [
    "aix", "linux", "sunos", "win32", "darwin", "android", "freebsd", "openbsd",
];
const CPU_NAMES: [&str; 11] = [
    "arm", "ppc", "x32", "x64", "ia32", "mips", "s390", "arm64", "ppc64", "s390x", "mipsel",
];

/// `"os": "linux"` or `[ "linux", "darwin", ]` style, from a string or an array.
fn negatable_list<'v>(value: &'v Json<'_>) -> Vec<&'v str> {
    match value {
        Json::Str(s) => vec![s.as_ref()],
        Json::Array(items) => items.iter().filter_map(Json::as_str).collect(),
        _ => Vec::new(),
    }
}

/// bun `Negatable::to_json`.
fn write_negatable(out: &mut String, names: &[&str], has: impl Fn(&str) -> bool, is_none: bool) {
    if is_none {
        out.push_str("\"none\"");
        return;
    }
    let removed = names.iter().filter(|n| !has(n)).count();
    let included = names.len() - removed;
    let print_included = removed > included;
    let one = (print_included && included == 1) || (!print_included && removed == 1);
    if !one {
        out.push_str("[ ");
    }
    for name in names {
        let present = has(name);
        if present && print_included {
            let _ = write!(out, "\"{name}\"");
        } else if !present && !print_included {
            let _ = write!(out, "\"!{name}\"");
        } else {
            continue;
        }
        if one {
            return;
        }
        out.push_str(", ");
    }
    out.push(']');
}

/// JSON string escaping as bun's `format_json_string_utf8`.
pub(crate) fn json_str(out: &mut String, s: &str) {
    out.push('"');
    json_str_inner(out, s);
    out.push('"');
}

fn json_str_inner(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------------------------

/// How a row of a package printed at several paths bound a dependency. Equal or better
/// evidence wins (bun `RowEvidence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Evidence {
    Unbound,
    /// The walk found the root's own dependency; says little about a peer.
    RootDependency,
    /// The walk found the package at a path between this row and the root.
    Walked,
    /// The root's copy, placed there from this row.
    WalkedToRoot,
    /// The walk found a copy outside an optional peer's range.
    Refused,
    /// The copy printed under the package's own path.
    OwnEntry,
    /// The walk found nothing, so the optional peer is unresolved.
    Unprovided,
}

enum ResolveError {
    InvalidPackageKey,
    Unresolvable,
}

/// A `workspaces` row: its path, object, name and version.
type MemberRow<'v> = (&'v str, &'v Obj<'v>, String, Option<String>);

/// A dependency row while parsing.
struct Row {
    edge: DepEdge,
    /// A `catalog:` reference outside the root and the workspaces, left unresolvable.
    stripped: bool,
    /// Listed in `unresolved`: the install that wrote the file did not resolve it.
    unresolved: bool,
}

struct Parser<'a> {
    options: &'a MigrateOptions,
    semver: &'a dyn Semver,
    version: u32,
    graph: LockGraph,
    rows: Vec<Vec<Row>>,
    dedupe: migration::FastMap<(String, String), PackageId>,
}

fn behavior_label(b: Behavior) -> &'static str {
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

fn may_stay_unresolved(b: Behavior) -> bool {
    b.intersects(Behavior::OPTIONAL | Behavior::PEER)
}

fn find_resolution(
    map: &migration::FastMap<String, PackageId>,
    pkg_path: &str,
    dep_name: &str,
    bundled: Option<&HashSet<String>>,
) -> std::result::Result<(PackageId, Evidence), ResolveError> {
    let mut buf = String::with_capacity(pkg_path.len() + 1 + dep_name.len());
    buf.push_str(pkg_path);
    buf.push('/');
    let mut offset = pkg_path.len() + 1;
    let mut at_bundle_root = false;
    let mut valid = true;
    while valid {
        buf.truncate(offset);
        buf.push_str(dep_name);
        if let Some(&id) = map.get(buf.as_str()) {
            let evidence = if offset == pkg_path.len() + 1 {
                Evidence::OwnEntry
            } else if offset == 0 {
                Evidence::WalkedToRoot
            } else {
                Evidence::Walked
            };
            return Ok((id, evidence));
        }
        if offset == 0 || at_bundle_root {
            return Err(ResolveError::Unresolvable);
        }
        let parent = &buf[..offset - 1];
        if let Some(bundled) = bundled {
            at_bundle_root = bundled.contains(parent);
        }
        let Some(slash) = parent.rfind('/') else {
            offset = 0;
            continue;
        };
        let Some(at) = parent.rfind('@') else {
            offset = slash + 1;
            continue;
        };
        if at > slash {
            valid = false;
            continue;
        }
        let Some(next_slash) = parent[..slash].rfind('/') else {
            if at != 0 {
                return Err(ResolveError::InvalidPackageKey);
            }
            offset = 0;
            continue;
        };
        if next_slash > at {
            offset = slash + 1;
            continue;
        }
        if next_slash + 1 != at {
            valid = false;
            continue;
        }
        offset = at;
    }
    Err(ResolveError::InvalidPackageKey)
}

fn parse_bin(info: &Obj<'_>) -> Bin {
    if let Some(bin) = info.get("bin") {
        return bin_from_json(bin);
    }
    if let Some(Json::Str(dir)) = info.get("binDir") {
        return Bin::Dir {
            path: dir.to_string(),
        };
    }
    Bin::None
}

/// bun `Bin::parse_append`: a string is a file, a one-entry object a named file, a larger
/// object a map. Entries whose value is not a string are skipped, like npm.
pub(crate) fn bin_from_json(bin: &Json<'_>) -> Bin {
    match bin {
        Json::Str(s) if !s.is_empty() => Bin::File {
            path: s.to_string(),
        },
        Json::Object(map) => {
            let mut entries: Vec<(String, String)> = map
                .iter()
                .filter_map(|(k, v)| Some((k.to_owned(), v.as_str()?.to_owned())))
                .collect();
            match entries.len() {
                0 => Bin::None,
                1 => {
                    let (name, path) = entries.pop().expect("one entry");
                    Bin::NamedFile { name, path }
                }
                _ => Bin::Map { entries },
            }
        }
        _ => Bin::None,
    }
}

impl<'a> Parser<'a> {
    fn run(bytes: &[u8], options: &'a MigrateOptions, semver: &'a dyn Semver) -> Result<Parsed> {
        let root = json::parse(bytes, true).map_err(|e| err(format!("invalid JSON: {e}")))?;
        let Some(root) = root.as_object() else {
            return Err(err("Expected an object"));
        };
        let version = match root.get("lockfileVersion") {
            None => return Err(err("Missing lockfile version")),
            // Any number with an integral value, as bun reads it (`1.0` and `1e0` are 1).
            Some(Json::Number(n)) => match n.parse::<f64>() {
                Ok(v) if (0.0..=f64::from(u32::MAX)).contains(&v) && v.fract() == 0.0 => v as u32,
                _ => return Err(err("Invalid lockfile version")),
            },
            Some(_) => return Err(err("Invalid lockfile version")),
        };
        if version > CURRENT_VERSION {
            return Err(err(format!(
                "Unsupported lockfile version {version}. This lockfile was likely created by a newer version of Bun. (This reader supports lockfile versions up to {CURRENT_VERSION}.)"
            )));
        }
        let mut graph = LockGraph::new(Package::new("", "", Resolution::Root));
        graph.packages.clear();
        graph.lockfile_version = version;
        graph.config_version = match root.get("configVersion") {
            None => ConfigVersion::V0,
            Some(Json::Number(n)) => match n.parse::<f64>().ok().and_then(config_version) {
                Some(v) => v,
                None => return Err(err("Invalid \"configVersion\". Expected a number")),
            },
            Some(_) => return Err(err("Invalid \"configVersion\". Expected a number")),
        };
        let mut parser = Parser {
            options,
            semver,
            version,
            graph,
            rows: Vec::new(),
            dedupe: migration::FastMap::default(),
        };
        parser.parse_top_level(root)?;
        let paths = parser.parse_packages(root)?;
        let Parser {
            mut graph, rows, ..
        } = parser;
        for (package, rows) in graph.packages.iter_mut().zip(rows) {
            package.dependencies = rows.into_iter().map(|r| r.edge).collect();
        }
        super::npm::finish_graph(&mut graph);
        Ok(Parsed {
            graph,
            paths,
            version,
        })
    }

    fn parse_top_level(&mut self, root: &Obj<'_>) -> Result<()> {
        if let Some(trusted) = root.get("trustedDependencies") {
            let Some(items) = trusted.as_array() else {
                return Err(err("trustedDependencies: Expected an array"));
            };
            let mut set = BTreeSet::new();
            for item in items {
                let Some(name) = item.as_str() else {
                    return Err(err("trustedDependencies: Expected a string"));
                };
                set.insert(name.to_owned());
            }
            self.graph.trusted_dependencies = Some(set);
        }
        if let Some(patched) = root.get("patchedDependencies") {
            let Some(map) = patched.as_object() else {
                return Err(err("patchedDependencies: Expected an object"));
            };
            for (key, path) in map {
                let Some(path) = path.as_str() else {
                    return Err(err("patchedDependencies: Expected a string"));
                };
                self.graph.patched_dependencies.insert(
                    key.to_owned(),
                    PatchedDependency {
                        path: path.to_owned(),
                        hash: None,
                    },
                );
            }
        }
        if let Some(overrides) = root.get("overrides") {
            let Some(map) = overrides.as_object() else {
                return Err(err("overrides: Expected an object"));
            };
            self.graph.overrides = parse_overrides(map)?;
        }
        if let Some(catalog) = root.get("catalog") {
            let Some(map) = catalog.as_object() else {
                return Err(err("catalog: Expected an object"));
            };
            self.graph.catalogs.default = parse_catalog(map, "catalog")?;
        }
        if let Some(catalogs) = root.get("catalogs") {
            let Some(map) = catalogs.as_object() else {
                return Err(err("catalogs: Expected an object"));
            };
            for (name, entries) in map {
                if name.is_empty() {
                    return Err(err("catalogs: Expected a non-empty string"));
                }
                let Some(entries) = entries.as_object() else {
                    return Err(err("catalogs: Expected an object"));
                };
                let parsed = parse_catalog(entries, "catalogs")?;
                self.graph
                    .catalogs
                    .named
                    .entry(name.to_owned())
                    .or_default()
                    .extend(parsed);
            }
        }
        Ok(())
    }

    /// bun `parse_append_dependencies`. `bundled`: the row's path and the bundled paths, for
    /// `packages` entries. `catalogs_apply`: the root and workspaces may use `catalog:`.
    fn parse_dependencies(
        &self,
        obj: &Obj<'_>,
        bundled: Option<(&str, &HashSet<String>)>,
        catalogs_apply: bool,
    ) -> Result<Vec<Row>> {
        let mut optional_peers: HashSet<&str> = HashSet::new();
        let mut unresolved: HashSet<&str> = HashSet::new();
        for (key, is_optional_peers) in [("optionalPeers", true), ("unresolved", false)] {
            let Some(names) = obj.get(key) else { continue };
            let Some(names) = names.as_array() else {
                return Err(err(format!("{key}: Expected an array")));
            };
            for name in names {
                let Some(name) = name.as_str() else {
                    return Err(err(format!("{key}: Expected a string")));
                };
                if is_optional_peers {
                    optional_peers.insert(name);
                } else {
                    unresolved.insert(name);
                }
            }
        }
        let mut rows = Vec::new();
        for (group, group_behavior) in GROUPS {
            let Some(deps) = obj.get(group) else { continue };
            let Some(deps) = deps.as_object() else {
                return Err(err(format!("{group}: Expected an object")));
            };
            for (name, literal) in deps {
                let Some(literal) = literal.as_str() else {
                    return Err(err(format!("{group}.{name}: Expected a string")));
                };
                let tag = migration::spec_tag(name, literal)
                    .ok_or_else(|| err(format!("{group}.{name}: Invalid dependency version")))?;
                let mut behavior = group_behavior;
                if group_behavior == Behavior::PEER && optional_peers.contains(name) {
                    behavior |= Behavior::OPTIONAL;
                }
                let mut stripped = false;
                if !catalogs_apply && tag == SpecTag::Catalog {
                    if behavior.is_peer() {
                        behavior |= Behavior::OPTIONAL;
                    } else {
                        stripped = true;
                    }
                }
                if let Some((pkg_path, bundled)) = bundled
                    && bundled.contains(&format!("{pkg_path}/{name}"))
                {
                    behavior |= Behavior::BUNDLED;
                }
                let is_unresolved = behavior.is_optional() && unresolved.contains(name);
                rows.push(Row {
                    edge: DepEdge {
                        name: name.to_owned(),
                        literal: literal.to_owned(),
                        behavior,
                        target: None,
                    },
                    stripped,
                    unresolved: is_unresolved,
                });
            }
        }
        Ok(rows)
    }

    fn push(&mut self, package: Package, rows: Vec<Row>) -> PackageId {
        let identity = (
            package.name.to_owned(),
            super::npm::resolution_identity(&package.resolution),
        );
        if let Some(&id) = self.dedupe.get(&identity) {
            return id;
        }
        let id = self.graph.push(package);
        self.rows.push(rows);
        self.dedupe.insert(identity, id);
        id
    }

    fn parse_packages(&mut self, root: &Obj<'_>) -> Result<Vec<(String, PackageId)>> {
        let Some(workspaces) = root.get("workspaces").and_then(Json::as_object) else {
            return Err(err("Missing a workspaces object property"));
        };
        let mut root_pkg: Option<&Obj<'_>> = None;
        // The (last) row of each name, in first-seen order.
        let mut member_rows: Vec<MemberRow<'_>> = Vec::new();
        for (path, value) in workspaces {
            let Some(obj) = value.as_object() else {
                return Err(err(format!("workspaces.{path}: Expected an object")));
            };
            if path.is_empty() {
                if root_pkg.is_some() {
                    return Err(err("Duplicate root package"));
                }
                root_pkg = Some(obj);
                continue;
            }
            let Some(name) = obj.get("name").and_then(Json::as_str) else {
                return Err(err(format!(
                    "workspaces.{path}: Expected a string name property"
                )));
            };
            let version = match obj.get("version") {
                None => None,
                Some(Json::Str(v)) => {
                    if !self.semver.is_valid(v) {
                        return Err(err(format!("workspaces.{path}: Invalid semver version")));
                    }
                    Some(v.to_string())
                }
                Some(_) => {
                    return Err(err(format!(
                        "workspaces.{path}: Expected a string version property"
                    )));
                }
            };
            match member_rows.iter_mut().find(|(_, _, n, _)| n == name) {
                Some(slot) => *slot = (path, obj, name.to_owned(), version),
                None => member_rows.push((path, obj, name.to_owned(), version)),
            }
        }
        let Some(root_obj) = root_pkg else {
            return Err(err("Expected root package"));
        };
        let root_name = match root_obj.get("name") {
            None => String::new(),
            Some(Json::Str(s)) => s.to_string(),
            Some(_) => return Err(err("workspaces[\"\"].name: Expected a string")),
        };
        let root_rows = self.parse_dependencies(root_obj, None, true)?;
        let mut root_package = Package::new(root_name, "", Resolution::Root);
        root_package.bin = parse_bin(root_obj);
        let root_id = self.push(root_package, root_rows);
        debug_assert_eq!(root_id, PackageId::ROOT);
        let member_names: Vec<String> = member_rows.iter().map(|m| m.2.clone()).collect();

        let mut pkg_map: migration::FastMap<String, PackageId> = migration::FastMap::default();
        let mut members: Vec<PackageId> = Vec::new();
        // name -> (path, version) of the workspace that owns the name.
        let workspace_by_name: HashMap<&str, (&str, Option<&str>)> = member_rows
            .iter()
            .map(|(p, _, n, v)| (n.as_str(), (*p, v.as_deref())))
            .collect();
        if self.version != 0 {
            for (path, obj, name, version) in &member_rows {
                let rows = self.parse_dependencies(obj, None, true)?;
                let mut package = Package::new(
                    name.to_owned(),
                    version.clone().unwrap_or_default(),
                    Resolution::Workspace {
                        path: (*path).to_owned(),
                    },
                );
                package.bin = parse_bin(obj);
                let id = self.push(package, rows);
                pkg_map.insert(name.to_owned(), id);
                members.push(id);
            }
        }

        let pkg_rows: Vec<(&str, &Json<'_>)> = match root.get("packages") {
            None => Vec::new(),
            Some(Json::Object(map)) => map.iter().collect(),
            Some(_) => return Err(err("packages: Expected an object")),
        };

        // A dependency of a bundled package is only bundled if it is at the top of the
        // bundling package's node_modules, so the bundled paths decide.
        let mut bundled_pkgs: HashSet<String> = HashSet::new();
        for (path, value) in &pkg_rows {
            let Some(items) = value.as_array() else {
                return Err(err(format!("packages.{path}: Expected an array")));
            };
            if let Some(info) = items.iter().find_map(Json::as_object)
                && matches!(info.get("bundled"), Some(Json::Bool(true)))
            {
                bundled_pkgs.insert((*path).to_owned());
            }
        }

        let mut paths: Vec<(String, PackageId)> = Vec::with_capacity(pkg_rows.len());
        'rows: for (path, value) in &pkg_rows {
            let items = value.as_array().expect("checked above");
            let at = |i: usize| -> String { format!("packages.{path}[{i}]") };
            let Some(first) = items.first() else {
                return Err(err(format!("packages.{path}: Missing package info")));
            };
            let Some(res_info) = first.as_str() else {
                return Err(err(format!("{}: Expected a string", at(0))));
            };
            let (name, res_str) = if let Some(rest) = res_info
                .strip_prefix('@')
                .filter(|r| r.starts_with("root:"))
            {
                (String::new(), rest)
            } else {
                match split_name_and_maybe_version(res_info) {
                    (name, Some(res)) if !name.is_empty() => (name.to_owned(), res),
                    _ => return Err(err(format!("{}: Invalid package resolution", at(0)))),
                }
            };
            if !name.is_empty() && !is_safe_install_folder_name(&name) {
                return Err(err(format!("{}: invalid package name", at(0))));
            }
            let Some(mut res) = resolution_from_text(res_str, self.semver) else {
                return Err(err(format!("{}: Unexpected resolution: {res_str}", at(0))));
            };
            let mut i = 1;
            let mut npm_url_needs_integrity = false;
            if let Resolution::Npm { tarball_url, .. } = &mut res {
                let Some(registry) = items.get(i) else {
                    return Err(err(format!("packages.{path}: Missing npm registry")));
                };
                let Some(registry) = registry.as_str() else {
                    return Err(err(format!("{}: Expected a string", at(i))));
                };
                i += 1;
                if !registry.is_empty() {
                    let configured = self.options.registry_for(&name);
                    let under_default =
                        url_is_under_registry(registry, fnpm_config::DEFAULT_REGISTRY);
                    npm_url_needs_integrity =
                        !url_is_under_registry(registry, configured) && !under_default;
                    if !under_default {
                        *tarball_url = Some(registry.to_owned());
                    }
                }
            }

            if self.version != 0
                && let Resolution::Workspace { path: ws_path } = &res
            {
                if let Some(&id) = pkg_map.get(*path) {
                    // Already in the map as a workspace package.
                    paths.push(((*path).to_owned(), id));
                    continue;
                }
                for &member in &members {
                    if matches!(&self.graph.packages[member.index()].resolution, Resolution::Workspace { path: p } if p == ws_path)
                    {
                        pkg_map.insert((*path).to_owned(), member);
                        paths.push(((*path).to_owned(), member));
                        continue 'rows;
                    }
                }
                return Err(err(format!("{}: Unknown workspace: '{ws_path}'", at(0))));
            }

            let mut package = Package::new(name.to_owned(), "", Resolution::Root);
            let mut rows = Vec::new();
            match &res {
                Resolution::Root => {
                    let Some(bin_obj) = items.get(i) else {
                        return Err(err(format!(
                            "packages.{path}: Missing package binaries object"
                        )));
                    };
                    let Some(bin_obj) = bin_obj.as_object() else {
                        return Err(err(format!("{}: Expected an object", at(i))));
                    };
                    i += 1;
                    package.bin = parse_bin(bin_obj);
                }
                _ => {
                    let Some(info) = items.get(i) else {
                        return Err(err(format!("packages.{path}: Missing dependencies object")));
                    };
                    let Some(info) = info.as_object() else {
                        return Err(err(format!("{}: Expected an object", at(i))));
                    };
                    i += 1;
                    let is_workspace = matches!(res, Resolution::Workspace { .. });
                    rows =
                        self.parse_dependencies(info, Some((*path, &bundled_pkgs)), is_workspace)?;
                    package.bin = parse_bin(info);
                    if !is_workspace {
                        package.platform = PlatformConstraints {
                            os: info
                                .get("os")
                                .map_or(OsSet::ALL, |v| OsSet::from_list(&negatable_list(v))),
                            cpu: info
                                .get("cpu")
                                .map_or(CpuSet::ALL, |v| CpuSet::from_list(&negatable_list(v))),
                            libc: info
                                .get("libc")
                                .map_or(LibcSet::NONE, |v| LibcSet::from_list(&negatable_list(v))),
                        };
                        if let Some(Json::Bool(b)) = info.get("hasInstallScript") {
                            package.meta.has_install_script = *b;
                        }
                        if let Some(Json::Str(d)) = info.get("deprecated") {
                            package.meta.deprecated = Some(d.to_string());
                        }
                        if matches!(info.get("bundled"), Some(Json::Bool(true))) {
                            package.meta.bundled = true;
                        }
                    }
                }
            }

            let is_github = matches!(res, Resolution::Github { .. });
            match &mut res {
                Resolution::Npm { version, .. } => {
                    let Some(integrity) = items.get(i) else {
                        return Err(err(format!("packages.{path}: Missing integrity")));
                    };
                    let Some(integrity) = integrity.as_str() else {
                        return Err(err(format!("{}: Expected a string", at(i))));
                    };
                    let integrity = self.parse_integrity(integrity, &at(i))?;
                    if self.version >= 2 && npm_url_needs_integrity && integrity.is_none() {
                        return Err(err(format!(
                            "{}: Missing integrity hash for npm package resolved to a tarball URL outside the configured registry",
                            at(i)
                        )));
                    }
                    package.version = version.clone();
                    if let Some(integrity) = integrity {
                        res.set_integrity(integrity);
                    }
                }
                Resolution::Git {
                    resolved_commit, ..
                }
                | Resolution::Github {
                    resolved_commit, ..
                } => {
                    let Some(tag) = items.get(i) else {
                        return Err(err(format!("packages.{path}: Missing git dependency tag")));
                    };
                    let Some(tag) = tag.as_str() else {
                        return Err(err(format!("{}: Expected a string", at(i))));
                    };
                    // A github tag names the cache folder directly, so it is checked at every
                    // version; a git tag is re-checked at checkout, so only from v2 on.
                    let enforce_safe = is_github || self.version >= 2;
                    if tag.len() > migration::MAX_RESOLVED_TAG_LEN
                        || (enforce_safe && !is_safe_resolved_tag(tag))
                    {
                        return Err(err(format!("{}: Invalid git dependency tag", at(i))));
                    }
                    i += 1;
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
            ) && let Some(integrity) = items.get(i).and_then(Json::as_str)
                && let Some(integrity) = self.parse_integrity(integrity, &at(i))?
            {
                res.set_integrity(integrity);
            }

            let is_root = matches!(res, Resolution::Root);
            package.resolution = res.clone();
            let bin = package.bin.clone();
            let pkg_id = self.push(package, rows);
            // The root's `bin` is recorded nowhere else.
            if pkg_id == PackageId::ROOT && is_root {
                self.graph.packages[0].bin = bin;
            }

            if let Some(&existing) = pkg_map.get(*path) {
                let replaceable = !matches!(res, Resolution::Folder { .. })
                    && members.contains(&existing)
                    && self.graph.packages[existing.index()].name == **path;
                if !replaceable {
                    return Err(err(format!("packages.{path}: Duplicate package path")));
                }
            }
            pkg_map.insert((*path).to_owned(), pkg_id);
            paths.push(((*path).to_owned(), pkg_id));
        }

        self.resolve(
            &pkg_rows,
            &pkg_map,
            &members,
            &member_names,
            &workspace_by_name,
            &bundled_pkgs,
        )?;
        Ok(paths)
    }

    fn parse_integrity(&self, text: &str, at: &str) -> Result<Option<Integrity>> {
        if text.is_empty() {
            return Ok(None);
        }
        match Integrity::parse(text) {
            Ok(integrity) => Ok(Some(integrity)),
            Err(_) if self.version >= 2 => Err(err(format!(
                "{at}: Unsupported or malformed integrity hash"
            ))),
            Err(_) => Ok(None),
        }
    }

    /// The range a peer is matched against by version, and the package name it asks for.
    /// `None` for a dependency that the path walk binds (bun `version_based_peer_range`).
    fn version_based_peer_range(&self, edge: &DepEdge) -> Option<(DependencySpec, String)> {
        let b = edge.behavior;
        if !b.is_peer() || b.is_optional_peer() || b.is_bundled() {
            return None;
        }
        let declared = DependencySpec::parse(&edge.name, &edge.literal).ok()?;
        let is_catalog = declared.tag() == SpecTag::Catalog;
        let range = match &declared {
            DependencySpec::Catalog { catalog } => {
                match self.graph.catalogs.get(catalog, &edge.name) {
                    Some(literal) => DependencySpec::parse(&edge.name, literal).ok()?,
                    None => declared.clone(),
                }
            }
            _ => declared.clone(),
        };
        let name = match &range {
            DependencySpec::Npm { name, .. } | DependencySpec::DistTag { name, .. } => {
                name.to_owned()
            }
            _ => edge.name.to_owned(),
        };
        // Overridden edges keep the path walk; the printed tree has the overridden version.
        let overridable = !b.is_workspace() && !declared.is_alias();
        let override_name = if is_catalog {
            edge.name.as_str()
        } else {
            name.as_str()
        };
        if overridable
            && self
                .graph
                .overrides
                .iter()
                .any(|r| r.selector.target.name == override_name)
        {
            return None;
        }
        Some((range, name))
    }

    fn satisfies(&self, id: PackageId, range: &DependencySpec) -> bool {
        match (&self.graph.packages[id.index()].resolution, range) {
            (Resolution::Npm { version, .. }, DependencySpec::Npm { range, .. }) => {
                let range = if range.is_empty() { "*" } else { range };
                self.semver.satisfies(version, range)
            }
            _ => false,
        }
    }

    /// The highest locked registry version of the peer's name in its range.
    fn peer_by_version(
        &self,
        edge: &DepEdge,
        by_name: &HashMap<&str, Vec<PackageId>>,
    ) -> Option<PackageId> {
        let (range, name) = self.version_based_peer_range(edge)?;
        by_name
            .get(name.as_str())?
            .iter()
            .copied()
            .find(|&id| self.satisfies(id, &range))
    }

    /// A workspace the peer's range matches; the loader links it like `Package::parse` does.
    fn linked_member(
        &self,
        edge: &DepEdge,
        members: &[PackageId],
        workspace_by_name: &HashMap<&str, (&str, Option<&str>)>,
    ) -> Option<PackageId> {
        let (range, name) = self.version_based_peer_range(edge)?;
        let DependencySpec::Npm { range: text, .. } = &range else {
            return None;
        };
        if !self.options.link_workspace_packages {
            return None;
        }
        let (_, version) = workspace_by_name.get(name.as_str())?;
        let star = matches!(text.trim(), "" | "*" | "x" | "X");
        if !star && !version.is_some_and(|v| self.semver.satisfies(v, text)) {
            return None;
        }
        members
            .iter()
            .copied()
            .find(|&id| self.graph.packages[id.index()].name == name)
    }

    fn resolve(
        &mut self,
        pkg_rows: &[(&str, &Json<'_>)],
        pkg_map: &migration::FastMap<String, PackageId>,
        members: &[PackageId],
        member_names: &[String],
        workspace_by_name: &HashMap<&str, (&str, Option<&str>)>,
        bundled_pkgs: &HashSet<String>,
    ) -> Result<()> {
        // Candidates for the version scan, newest first (bun keeps `package_index` sorted).
        let mut by_name: HashMap<&str, Vec<PackageId>> = HashMap::new();
        for (i, p) in self.graph.packages.iter().enumerate() {
            if matches!(p.resolution, Resolution::Npm { .. }) {
                by_name
                    .entry(p.name.as_str())
                    .or_default()
                    .push(PackageId(i as u32));
            }
        }
        for list in by_name.values_mut() {
            list.sort_by(|a, b| {
                let va = self.graph.packages[a.index()]
                    .resolution
                    .npm_version()
                    .unwrap_or_default();
                let vb = self.graph.packages[b.index()]
                    .resolution
                    .npm_version()
                    .unwrap_or_default();
                self.semver.compare(vb, va)
            });
        }
        let own_peer = |this: &Self, edge: &DepEdge| {
            this.linked_member(edge, members, workspace_by_name)
                .or_else(|| this.peer_by_version(edge, &by_name))
        };

        let mut targets: Vec<Vec<Option<PackageId>>> = self
            .rows
            .iter()
            .map(|rows| vec![None; rows.len()])
            .collect();

        // The root's dependencies.
        for (k, row) in self.rows[0].iter().enumerate() {
            let named = pkg_map.get(row.edge.name.as_str()).copied();
            let Some(id) = own_peer(self, &row.edge).or(named) else {
                if may_stay_unresolved(row.edge.behavior) {
                    continue;
                }
                return Err(err(format!(
                    "Failed to resolve root {} dependency '{}'",
                    behavior_label(row.edge.behavior),
                    row.edge.name
                )));
            };
            targets[0][k] = Some(id);
        }

        // Then the workspaces'.
        if self.version != 0 {
            for &member in members {
                let ws_name = self.graph.packages[member.index()].name.to_owned();
                let unlinked = pkg_map.get(ws_name.as_str()) != Some(&member);
                let prefix = if unlinked { "./" } else { "" };
                for (k, row) in self.rows[member.index()].iter().enumerate() {
                    let nested = format!("{prefix}{ws_name}/{}", row.edge.name);
                    let found = own_peer(self, &row.edge).or_else(|| {
                        pkg_map
                            .get(nested.as_str())
                            .or_else(|| pkg_map.get(row.edge.name.as_str()))
                            .copied()
                    });
                    let Some(id) = found else {
                        if unlinked || may_stay_unresolved(row.edge.behavior) {
                            continue;
                        }
                        return Err(err(format!(
                            "Failed to resolve {} dependency '{}' for package '{ws_name}'",
                            behavior_label(row.edge.behavior),
                            row.edge.name
                        )));
                    };
                    targets[member.index()][k] = Some(id);
                }
            }
        }

        // Then each package's, walking up its path.
        let mut bound_by: Vec<Vec<Evidence>> = self
            .rows
            .iter()
            .map(|rows| vec![Evidence::Unbound; rows.len()])
            .collect();
        let mut root_dep_names: HashSet<&str> =
            self.rows[0].iter().map(|r| r.edge.name.as_str()).collect();
        root_dep_names.extend(member_names.iter().map(String::as_str));
        for (path, _) in pkg_rows {
            let Some(&pkg_id) = pkg_map.get(*path) else {
                return Err(err(format!("packages.{path}: Invalid packages object")));
            };
            let package = &self.graph.packages[pkg_id.index()];
            let is_workspace = matches!(package.resolution, Resolution::Workspace { .. });
            if pkg_id == PackageId::ROOT
                || (is_workspace && pkg_map.get(package.name.as_str()) == Some(&pkg_id))
            {
                continue;
            }
            for k in 0..self.rows[pkg_id.index()].len() {
                let row = &self.rows[pkg_id.index()][k];
                if row.stripped {
                    continue;
                }
                let edge = &row.edge;
                let by_version = if is_workspace {
                    own_peer(self, edge)
                } else {
                    self.peer_by_version(edge, &by_name)
                };
                let id = match by_version {
                    Some(id) => id,
                    None => {
                        let bundled = edge.behavior.is_optional_peer().then_some(bundled_pkgs);
                        match find_resolution(pkg_map, path, &edge.name, bundled) {
                            Ok((id, mut evidence)) => {
                                let own_entry = evidence == Evidence::OwnEntry;
                                if evidence == Evidence::WalkedToRoot
                                    && edge.behavior.is_peer()
                                    && root_dep_names.contains(edge.name.as_str())
                                {
                                    evidence = Evidence::RootDependency;
                                } else if edge.behavior.is_optional_peer() && !own_entry {
                                    let range = self.resolved_range(edge);
                                    if !range.is_some_and(|r| self.satisfies(id, &r)) {
                                        evidence = Evidence::Refused;
                                    }
                                }
                                if bound_by[pkg_id.index()][k] > evidence {
                                    continue;
                                }
                                bound_by[pkg_id.index()][k] = evidence;
                                id
                            }
                            Err(ResolveError::InvalidPackageKey) => {
                                return Err(err(format!("packages.{path}: Invalid package path")));
                            }
                            Err(ResolveError::Unresolvable) => {
                                if edge.behavior.is_optional_peer() {
                                    bound_by[pkg_id.index()][k] = Evidence::Unprovided;
                                    targets[pkg_id.index()][k] = None;
                                }
                                if may_stay_unresolved(edge.behavior) {
                                    continue;
                                }
                                return Err(err(format!(
                                    "Failed to resolve {} dependency '{}' for package '{path}'",
                                    behavior_label(edge.behavior),
                                    edge.name
                                )));
                            }
                        }
                    }
                };
                targets[pkg_id.index()][k] = Some(id);
            }
        }

        for (rows, targets) in self.rows.iter_mut().zip(targets) {
            for (row, target) in rows.iter_mut().zip(targets) {
                // The install that wrote bun.lock did not resolve these.
                row.edge.target = if row.unresolved { None } else { target };
            }
        }
        Ok(())
    }

    /// The dependency's range after `catalog:` resolution.
    fn resolved_range(&self, edge: &DepEdge) -> Option<DependencySpec> {
        let declared = DependencySpec::parse(&edge.name, &edge.literal).ok()?;
        match &declared {
            DependencySpec::Catalog { catalog } => {
                match self.graph.catalogs.get(catalog, &edge.name) {
                    Some(literal) => DependencySpec::parse(&edge.name, literal).ok(),
                    None => Some(declared),
                }
            }
            _ => Some(declared),
        }
    }
}

fn parse_catalog(map: &Obj<'_>, section: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (name, version) in map {
        if name.is_empty() {
            return Err(err(format!("{section}: Expected a non-empty string")));
        }
        let Some(version) = version.as_str() else {
            return Err(err(format!("{section}.{name}: Expected a string")));
        };
        if DependencySpec::parse(name, version).is_err() {
            return Err(err(format!("{section}.{name}: Invalid catalog version")));
        }
        out.insert(name.to_owned(), version.to_owned());
    }
    Ok(out)
}

/// The `overrides` section: `"name": "spec"`, or `"selector": { ".": "spec", "child": "spec" }`
/// for scoped rules (lockfileVersion 3).
fn parse_overrides(map: &Obj<'_>) -> Result<Vec<OverrideRule>> {
    let mut rules: Vec<OverrideRule> = Vec::new();
    let mut put = |selector: OverrideSelector, value: &str| -> Result<()> {
        if DependencySpec::parse(&selector.target.name, value).is_err() {
            return Err(err("overrides: Invalid override version"));
        }
        match rules.iter_mut().find(|r| r.selector == selector) {
            Some(existing) => existing.value = value.to_owned(),
            None => rules.push(OverrideRule {
                selector,
                value: value.to_owned(),
            }),
        }
        Ok(())
    };
    for (key, value) in map {
        if key.is_empty() {
            return Err(err("overrides: Expected a non-empty string"));
        }
        if let Some(value) = value.as_str() {
            put(
                OverrideSelector {
                    parent: None,
                    target: PackageSelector {
                        name: key.to_owned(),
                        range: String::new(),
                    },
                },
                value,
            )?;
            continue;
        }
        let Some(group) = value.as_object() else {
            return Err(err("overrides: Expected a string or an object"));
        };
        let parent =
            parse_package_segment(key).map_err(|_| err("overrides: Invalid override key"))?;
        for (child, value) in group {
            let Some(value) = value.as_str() else {
                return Err(err("overrides: Expected a string"));
            };
            if child == "." {
                put(
                    OverrideSelector {
                        parent: None,
                        target: parent.clone(),
                    },
                    value,
                )?;
            } else {
                let target = parse_package_segment(child)
                    .map_err(|_| err("overrides: Invalid override key"))?;
                put(
                    OverrideSelector {
                        parent: Some(parent.clone()),
                        target,
                    },
                    value,
                )?;
            }
        }
    }
    Ok(rules)
}

// ---------------------------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------------------------

/// Serialises to bun.lock, hoisting with every group and platform included.
pub fn serialize(graph: &LockGraph) -> String {
    let tree = crate::tree::hoist(graph, &super::npm::export_hoist_options());
    serialize_with_tree(graph, &tree)
}

/// A tree node with what the writer needs.
struct Placed<'g> {
    key: &'g str,
    name: &'g str,
    package: PackageId,
    /// The edge that put the package there.
    behavior: Behavior,
    depth: usize,
    folder: &'g str,
}

fn placements<'g>(graph: &'g LockGraph, tree: &'g Tree) -> Vec<Placed<'g>> {
    let mut workspace_of: HashMap<u32, PackageId> = HashMap::new();
    for (ws, nodes) in &tree.workspace_roots {
        for &n in nodes {
            workspace_of.insert(n, *ws);
        }
    }
    let mut depths: Vec<usize> = Vec::with_capacity(tree.nodes.len());
    let mut out = Vec::with_capacity(tree.nodes.len());
    for (i, node) in tree.nodes.iter().enumerate() {
        let (owner, depth) = match node.parent {
            Some(p) => (
                tree.nodes.get(p as usize).map(|n| n.package),
                depths.get(p as usize).map_or(1, |d| d + 1),
            ),
            None => match workspace_of.get(&(i as u32)) {
                Some(&ws) if ws != PackageId::ROOT => (Some(ws), 1),
                _ => (Some(PackageId::ROOT), 0),
            },
        };
        depths.push(depth);
        let behavior = owner
            .and_then(|o| graph.get(o))
            .and_then(|p| p.dependency(&node.name))
            .map_or(Behavior::NONE, |e| e.behavior);
        let folder = node
            .path
            .strip_suffix(node.name.as_str())
            .map_or("", |f| f.strip_suffix('/').unwrap_or(f));
        out.push(Placed {
            key: &node.path,
            name: &node.name,
            package: node.package,
            behavior,
            depth,
            folder,
        });
    }
    out.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.folder.cmp(b.folder))
            .then_with(|| a.name.cmp(b.name))
    });
    out
}

fn has_scoped_overrides(graph: &LockGraph) -> bool {
    graph
        .overrides
        .iter()
        .any(|r| r.selector.parent.is_some() || !r.selector.target.range.is_empty())
}

/// bun `Stringifier::version_to_write`. A lockfile loaded from text keeps its version (v0 is
/// floored to v1) unless it has scoped overrides; otherwise (`loaded` = [`CURRENT_VERSION`])
/// the written packages decide: 3 with scoped overrides, else 2, or 1 when one breaks a v2
/// invariant (an off-registry tarball without integrity, an unsafe git tag).
fn version_to_write(graph: &LockGraph, placed: &[Placed<'_>], loaded: u32) -> u32 {
    let has_scoped = has_scoped_overrides(graph);
    if !has_scoped && loaded < 3 {
        return loaded.max(1);
    }
    for p in placed {
        match &graph.packages[p.package.index()].resolution {
            Resolution::Npm {
                tarball_url: Some(url),
                integrity: None,
                ..
            } if !url_is_under_registry(url, fnpm_config::DEFAULT_REGISTRY) => {
                return 1;
            }
            Resolution::Git {
                resolved_commit, ..
            } if !is_safe_resolved_tag(resolved_commit) => return 1,
            _ => {}
        }
    }
    if has_scoped { 3 } else { 2 }
}

struct Writer {
    out: String,
    indent: u32,
}

impl Writer {
    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
    }
    fn inc(&mut self) {
        self.indent += 1;
        self.write_indent();
    }
    fn dec(&mut self) {
        self.indent -= 1;
        self.write_indent();
    }
}

fn bin_single_line(out: &mut String, bin: &Bin) {
    match bin {
        Bin::None => {}
        Bin::File { path } | Bin::Dir { path } => json_str(out, path),
        Bin::NamedFile { name, path } => {
            out.push_str("{ ");
            json_str(out, name);
            out.push_str(": ");
            json_str(out, path);
            out.push_str(" }");
        }
        Bin::Map { entries } => {
            out.push('{');
            for (i, (name, path)) in entries.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                out.push(' ');
                json_str(out, name);
                out.push_str(": ");
                json_str(out, path);
            }
            out.push_str(" }");
        }
    }
}

fn bin_multi_line(w: &mut Writer, bin: &Bin) {
    match bin {
        Bin::None => {}
        Bin::File { path } | Bin::Dir { path } => json_str(&mut w.out, path),
        Bin::NamedFile { name, path } => {
            w.out.push_str("{\n");
            w.indent += 1;
            w.write_indent();
            json_str(&mut w.out, name);
            w.out.push_str(": ");
            json_str(&mut w.out, path);
            w.out.push_str(",\n");
            w.indent -= 1;
            w.write_indent();
            w.out.push('}');
        }
        Bin::Map { entries } => {
            w.out.push('{');
            if entries.is_empty() {
                w.out.push('}');
                return;
            }
            w.out.push('\n');
            w.indent += 1;
            for (name, path) in entries {
                w.write_indent();
                json_str(&mut w.out, name);
                w.out.push_str(": ");
                json_str(&mut w.out, path);
                w.out.push_str(",\n");
            }
            w.indent -= 1;
            w.write_indent();
            w.out.push('}');
        }
    }
}

fn sorted_edges(package: &Package) -> Vec<&DepEdge> {
    let mut edges: Vec<&DepEdge> = package
        .dependencies
        .iter()
        .filter(|e| !migration::is_workspace_only(e.behavior))
        .collect();
    edges.sort_by(|a, b| a.name.cmp(&b.name));
    edges
}

/// Optional dependencies that did not resolve although the tree has a package of that name;
/// written as `unresolved` so that loading does not bind them to it.
fn unresolved_names<'g>(edges: &[&'g DepEdge], placed_names: &HashSet<&str>) -> Vec<&'g str> {
    edges
        .iter()
        .filter(|e| {
            e.target.is_none() && e.behavior.is_optional() && placed_names.contains(e.name.as_str())
        })
        .map(|e| e.name.as_str())
        .collect()
}

fn write_workspace(
    w: &mut Writer,
    graph: &LockGraph,
    id: PackageId,
    path: &str,
    placed_names: &HashSet<&str>,
) {
    let package = &graph.packages[id.index()];
    let mut any = false;
    if id == PackageId::ROOT {
        w.out.push_str("\"\": {");
        if !package.name.is_empty() {
            w.out.push('\n');
            w.inc();
            w.out.push_str("\"name\": ");
            json_str(&mut w.out, &package.name);
            any = true;
        }
    } else {
        json_str(&mut w.out, path);
        w.out.push_str(": {\n");
        w.inc();
        w.out.push_str("\"name\": ");
        json_str(&mut w.out, &package.name);
        if !package.version.is_empty() {
            w.out.push_str(",\n");
            w.write_indent();
            w.out.push_str("\"version\": ");
            json_str(&mut w.out, &package.version);
        }
        if !package.bin.is_none() {
            w.out.push_str(",\n");
            w.write_indent();
            w.out.push_str(if matches!(package.bin, Bin::Dir { .. }) {
                "\"binDir\": "
            } else {
                "\"bin\": "
            });
            bin_multi_line(w, &package.bin);
        }
        any = true;
    }
    let edges = sorted_edges(package);
    let mut optional_peers: Vec<&str> = Vec::new();
    for (group, group_behavior) in GROUPS {
        let mut first = true;
        for edge in &edges {
            if !edge.behavior.intersects(group_behavior) {
                continue;
            }
            if edge.behavior.is_optional_peer() {
                if group_behavior == Behavior::OPTIONAL {
                    continue;
                }
                optional_peers.push(&edge.name);
            }
            if first {
                if any {
                    w.out.push(',');
                }
                w.out.push('\n');
                if any {
                    w.write_indent();
                } else {
                    w.inc();
                }
                let _ = writeln!(w.out, "\"{group}\": {{");
                w.inc();
                any = true;
                first = false;
            } else {
                w.out.push_str(",\n");
                w.write_indent();
            }
            json_str(&mut w.out, &edge.name);
            w.out.push_str(": ");
            json_str(&mut w.out, &edge.literal);
        }
        if !first {
            w.out.push_str(",\n");
            w.dec();
            w.out.push('}');
        }
    }
    let unresolved = unresolved_names(&edges, placed_names);
    for (key, names) in [
        ("optionalPeers", &optional_peers),
        ("unresolved", &unresolved),
    ] {
        if names.is_empty() {
            continue;
        }
        w.out.push_str(",\n");
        w.write_indent();
        let _ = writeln!(w.out, "\"{key}\": [");
        w.indent += 1;
        for name in names.iter() {
            w.write_indent();
            json_str(&mut w.out, name);
            w.out.push_str(",\n");
        }
        w.dec();
        w.out.push(']');
    }
    if any {
        w.out.push_str(",\n");
        w.dec();
    }
    w.out.push_str("},");
}

fn write_info(
    out: &mut String,
    package: &Package,
    placed_behavior: Behavior,
    placed_names: &HashSet<&str>,
) {
    out.push('{');
    let edges = sorted_edges(package);
    let mut any = false;
    let mut optional_peers: Vec<&str> = Vec::new();
    for (group, group_behavior) in GROUPS {
        let mut first = true;
        for edge in &edges {
            if !edge.behavior.intersects(group_behavior) {
                continue;
            }
            if edge.behavior.is_optional_peer() {
                if group_behavior == Behavior::OPTIONAL {
                    continue;
                }
                optional_peers.push(&edge.name);
            }
            if first {
                if any {
                    out.push(',');
                }
                let _ = write!(out, " \"{group}\": {{ ");
                first = false;
                any = true;
            } else {
                out.push_str(", ");
            }
            json_str(out, &edge.name);
            out.push_str(": ");
            json_str(out, &edge.literal);
        }
        if !first {
            out.push_str(" }");
        }
    }
    let unresolved = unresolved_names(&edges, placed_names);
    for (key, names) in [
        ("optionalPeers", &optional_peers),
        ("unresolved", &unresolved),
    ] {
        if names.is_empty() {
            continue;
        }
        let _ = write!(out, ", \"{key}\": [");
        for (i, name) in names.iter().enumerate() {
            if i != 0 {
                out.push(' ');
            }
            json_str(out, name);
            if i != names.len() - 1 {
                out.push(',');
            }
        }
        out.push(']');
    }
    let mut key = |out: &mut String, text: &str| {
        if std::mem::replace(&mut any, true) {
            out.push(',');
        }
        out.push_str(text);
    };
    if placed_behavior.is_bundled() || package.meta.bundled {
        key(out, " \"bundled\": true");
    }
    let platform = &package.platform;
    if platform.os != OsSet::ALL {
        key(out, " \"os\": ");
        write_negatable(
            out,
            &OS_NAMES,
            |n| OsSet::lookup(n).is_some_and(|b| platform.os.contains(b)),
            platform.os == OsSet::NONE,
        );
    }
    if platform.cpu != CpuSet::ALL {
        key(out, " \"cpu\": ");
        write_negatable(
            out,
            &CPU_NAMES,
            |n| CpuSet::lookup(n).is_some_and(|b| platform.cpu.contains(b)),
            platform.cpu == CpuSet::NONE,
        );
    }
    let libc: Vec<&str> = platform.libc.names().collect();
    if libc.len() == 1 {
        key(out, " \"libc\": \"");
        out.push_str(libc[0]);
        out.push('"');
    }
    if package.meta.needs_scripts() {
        key(out, " \"hasInstallScript\": true");
    }
    if !package.bin.is_none() {
        key(
            out,
            if matches!(package.bin, Bin::Dir { .. }) {
                " \"binDir\": "
            } else {
                " \"bin\": "
            },
        );
        bin_single_line(out, &package.bin);
    }
    if any {
        out.push_str(" }");
    } else {
        out.push('}');
    }
}

fn write_tuple(out: &mut String, graph: &LockGraph, p: &Placed<'_>, placed_names: &HashSet<&str>) {
    let package = &graph.packages[p.package.index()];
    let name = &package.name;
    let info = |out: &mut String| write_info(out, package, p.behavior, placed_names);
    let integrity = |out: &mut String, integrity: Option<&Integrity>, always: bool| match integrity
    {
        Some(i) => {
            let _ = write!(out, ", \"{}\"]", i.to_sri());
        }
        None if always => out.push_str(", \"\"]"),
        None => out.push(']'),
    };
    out.push_str("[\"");
    json_str_inner(out, name);
    out.push('@');
    match &package.resolution {
        Resolution::Root => {
            out.push_str("root:\", {");
            if package.bin.is_none() {
                out.push_str("}]");
            } else {
                out.push_str(if matches!(package.bin, Bin::Dir { .. }) {
                    " \"binDir\": "
                } else {
                    " \"bin\": "
                });
                bin_single_line(out, &package.bin);
                out.push_str(" }]");
            }
        }
        Resolution::Folder { path } | Resolution::Symlink { path } => {
            out.push_str(if matches!(package.resolution, Resolution::Folder { .. }) {
                "file:"
            } else {
                "link:"
            });
            json_str_inner(out, path);
            out.push_str("\", ");
            info(out);
            out.push(']');
        }
        Resolution::LocalTarball {
            path: target,
            integrity: i,
        }
        | Resolution::RemoteTarball {
            url: target,
            integrity: i,
        } => {
            json_str_inner(out, target);
            out.push_str("\", ");
            info(out);
            integrity(out, i.as_ref(), false);
        }
        Resolution::Npm {
            version,
            tarball_url,
            integrity: i,
        } => {
            json_str_inner(out, version);
            out.push_str("\", \"");
            if let Some(url) = tarball_url
                .as_deref()
                .filter(|u| !url_is_under_registry(u, fnpm_config::DEFAULT_REGISTRY))
            {
                json_str_inner(out, url);
            }
            out.push_str("\", ");
            info(out);
            integrity(out, i.as_ref(), true);
        }
        Resolution::Workspace { path } => {
            out.push_str("workspace:");
            json_str_inner(out, path);
            out.push_str("\"]");
        }
        Resolution::Git {
            url,
            resolved_commit: tag,
            integrity: i,
            ..
        } => {
            out.push_str("git+");
            if fnpm_types::spec::is_scp_like_path(url) {
                out.push_str("ssh://");
            }
            json_str_inner(out, url);
            out.push('#');
            json_str_inner(out, resolved_commit(tag));
            out.push_str("\", ");
            info(out);
            out.push_str(", ");
            json_str(out, tag);
            integrity(out, i.as_ref(), false);
        }
        Resolution::Github {
            owner,
            repo,
            resolved_commit: tag,
            integrity: i,
            ..
        } => {
            out.push_str("github:");
            if !owner.is_empty() {
                json_str_inner(out, owner);
                out.push('/');
            }
            json_str_inner(out, repo);
            out.push('#');
            json_str_inner(out, resolved_commit(tag));
            out.push_str("\", ");
            info(out);
            out.push_str(", ");
            json_str(out, tag);
            integrity(out, i.as_ref(), false);
        }
    }
}

fn override_key(selector: &PackageSelector) -> String {
    if selector.range.is_empty() {
        selector.name.clone()
    } else {
        format!("{}@{}", selector.name, selector.range)
    }
}

fn write_rule(w: &mut Writer, key: &str, value: &str) {
    w.write_indent();
    json_str(&mut w.out, key);
    w.out.push_str(": ");
    json_str(&mut w.out, value);
    w.out.push_str(",\n");
}

/// bun `write_override_rules`: scoped rules live in selector-keyed objects; a flat rule folds
/// into its name's unranged group as `"."`.
fn write_overrides(w: &mut Writer, rules: &[OverrideRule]) {
    let mut flat: Vec<&OverrideRule> = rules
        .iter()
        .filter(|r| r.selector.parent.is_none() && r.selector.target.range.is_empty())
        .collect();
    flat.sort_by(|a, b| a.selector.target.name.cmp(&b.selector.target.name));
    let mut scoped: Vec<&OverrideRule> = rules
        .iter()
        .filter(|r| r.selector.parent.is_some() || !r.selector.target.range.is_empty())
        .collect();
    if scoped.is_empty() {
        for rule in flat {
            write_rule(w, &rule.selector.target.name, &rule.value);
        }
        return;
    }
    scoped.sort_by(|l, r| {
        l.selector
            .parent
            .is_some()
            .cmp(&r.selector.parent.is_some())
            .then_with(|| match (&l.selector.parent, &r.selector.parent) {
                (Some(a), Some(b)) => a.name.cmp(&b.name).then_with(|| a.range.cmp(&b.range)),
                _ => Ordering::Equal,
            })
            .then_with(|| l.selector.target.name.cmp(&r.selector.target.name))
            .then_with(|| l.selector.target.range.cmp(&r.selector.target.range))
    });
    let solo_end = scoped
        .iter()
        .position(|r| r.selector.parent.is_some())
        .unwrap_or(scoped.len());
    let (mut f, mut s, mut g) = (0usize, 0usize, solo_end);
    while f < flat.len() || s < solo_end || g < scoped.len() {
        let solo = (s < solo_end).then(|| &scoped[s].selector.target);
        let parented = scoped.get(g).and_then(|r| r.selector.parent.as_ref());
        let group: Option<&PackageSelector> = match (solo, parented) {
            (Some(solo), Some(parented)) => Some(
                if (&parented.name, &parented.range) < (&solo.name, &solo.range) {
                    parented
                } else {
                    solo
                },
            ),
            (solo, parented) => solo.or(parented),
        };
        if let Some(flat_rule) = flat.get(f) {
            let flat_first = match group {
                None => true,
                Some(group) => match flat_rule.selector.target.name.cmp(&group.name) {
                    Ordering::Less => true,
                    Ordering::Equal => !group.range.is_empty(),
                    Ordering::Greater => false,
                },
            };
            if flat_first {
                write_rule(w, &flat_rule.selector.target.name, &flat_rule.value);
                f += 1;
                continue;
            }
        }
        let Some(group) = group.cloned() else { break };
        w.write_indent();
        json_str(&mut w.out, &override_key(&group));
        w.out.push_str(": {\n");
        let dot = if group.range.is_empty() {
            match flat.get(f) {
                Some(rule) if rule.selector.target.name == group.name => {
                    f += 1;
                    Some(rule.value.as_str())
                }
                _ => None,
            }
        } else {
            match (s < solo_end).then(|| scoped[s]) {
                Some(rule) if rule.selector.target == group => {
                    s += 1;
                    Some(rule.value.as_str())
                }
                _ => None,
            }
        };
        w.indent += 1;
        if let Some(value) = dot {
            write_rule(w, ".", value);
        }
        while let Some(rule) = scoped.get(g) {
            if rule.selector.parent.as_ref() != Some(&group) {
                break;
            }
            write_rule(w, &override_key(&rule.selector.target), &rule.value);
            g += 1;
        }
        w.indent -= 1;
        w.write_indent();
        w.out.push_str("},\n");
    }
}

fn write_string_map<'m>(w: &mut Writer, entries: impl Iterator<Item = (&'m String, &'m String)>) {
    for (k, v) in entries {
        write_rule(w, k, v);
    }
}

/// Serialises with a given install tree (its node `path`s are the `packages` keys), as a new
/// lockfile.
pub fn serialize_with_tree(graph: &LockGraph, tree: &Tree) -> String {
    serialize_loaded(graph, tree, CURRENT_VERSION)
}

/// Serialises a graph that was read from a bun.lock of version `loaded`, keeping that version
/// the way bun re-saves a lockfile.
pub fn serialize_loaded(graph: &LockGraph, tree: &Tree, loaded: u32) -> String {
    let placed = placements(graph, tree);
    let placed_names: HashSet<&str> = placed.iter().map(|p| p.name).collect();
    let mut w = Writer {
        out: String::with_capacity(256 + placed.len() * 200),
        indent: 0,
    };
    w.out.push_str("{\n");
    w.inc();
    let _ = writeln!(
        w.out,
        "\"lockfileVersion\": {},",
        version_to_write(graph, &placed, loaded)
    );
    w.write_indent();
    let _ = writeln!(
        w.out,
        "\"configVersion\": {},",
        u8::from(graph.config_version)
    );
    w.write_indent();
    w.out.push_str("\"workspaces\": {\n");
    w.inc();
    write_workspace(&mut w, graph, PackageId::ROOT, "", &placed_names);
    for (path, &id) in &graph.workspaces {
        if path.is_empty() || graph.get(id).is_none() {
            continue;
        }
        w.out.push('\n');
        w.write_indent();
        write_workspace(&mut w, graph, id, path, &placed_names);
    }
    w.out.push('\n');
    w.dec();
    w.out.push_str("},\n");

    // Trusted and patched packages that are installed.
    if let Some(trusted) = &graph.trusted_dependencies {
        let mut found: BTreeSet<&str> = BTreeSet::new();
        for p in &placed {
            let package = &graph.packages[p.package.index()];
            let name = if matches!(package.resolution, Resolution::Npm { .. }) {
                package.name.as_str()
            } else {
                p.name
            };
            if trusted.contains(name) {
                found.insert(name);
            }
        }
        if !found.is_empty() {
            w.write_indent();
            w.out.push_str("\"trustedDependencies\": [\n");
            w.indent += 1;
            for name in found {
                w.write_indent();
                json_str(&mut w.out, name);
                w.out.push_str(",\n");
            }
            w.dec();
            w.out.push_str("],\n");
        } else if trusted.is_empty() {
            w.write_indent();
            w.out.push_str("\"trustedDependencies\": [],\n");
        }
    }
    if !graph.patched_dependencies.is_empty() {
        let mut found: BTreeMap<String, &str> = BTreeMap::new();
        for p in &placed {
            let package = &graph.packages[p.package.index()];
            let version = match &package.resolution {
                Resolution::Workspace { .. } | Resolution::Npm { .. } => package.version.clone(),
                other => other.to_string(),
            };
            let key = format!("{}@{version}", package.name);
            if let Some(patch) = graph.patched_dependencies.get(&key) {
                found.insert(key, &patch.path);
            }
        }
        if !found.is_empty() {
            w.write_indent();
            w.out.push_str("\"patchedDependencies\": {\n");
            w.indent += 1;
            for (key, path) in &found {
                write_rule(&mut w, key, path);
            }
            w.dec();
            w.out.push_str("},\n");
        }
    }
    if !graph.overrides.is_empty() {
        w.write_indent();
        w.out.push_str("\"overrides\": {\n");
        w.indent += 1;
        write_overrides(&mut w, &graph.overrides);
        w.dec();
        w.out.push_str("},\n");
    }
    if !graph.catalogs.default.is_empty() {
        w.write_indent();
        w.out.push_str("\"catalog\": {\n");
        w.indent += 1;
        write_string_map(&mut w, graph.catalogs.default.iter());
        w.dec();
        w.out.push_str("},\n");
    }
    if !graph.catalogs.named.is_empty() {
        w.write_indent();
        w.out.push_str("\"catalogs\": {\n");
        w.indent += 1;
        for (name, entries) in &graph.catalogs.named {
            w.write_indent();
            json_str(&mut w.out, name);
            w.out.push_str(": {\n");
            w.indent += 1;
            write_string_map(&mut w, entries.iter());
            w.dec();
            w.out.push_str("},\n");
        }
        w.dec();
        w.out.push_str("},\n");
    }

    w.write_indent();
    w.out.push_str("\"packages\": {");
    let mut first = true;
    for p in &placed {
        if graph.get(p.package).is_none() {
            continue;
        }
        if first {
            first = false;
            w.out.push('\n');
            w.inc();
        } else {
            w.out.push_str(",\n\n");
            w.write_indent();
        }
        json_str(&mut w.out, p.key);
        w.out.push_str(": ");
        write_tuple(&mut w.out, graph, p, &placed_names);
    }
    if !first {
        w.out.push_str(",\n");
        w.dec();
    }
    w.out.push_str("}\n");
    w.dec();
    w.out.push_str("}\n");
    w.out
}
