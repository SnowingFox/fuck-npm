//! What the yarn v1, yarn berry and pnpm converters share: the [`Migration`] result (a graph
//! plus the package.json edits bun makes when it migrates), the [`MigrateOptions`] they read
//! the environment through, and helpers for building the graph.
//!
//! A migration is a pure function of the lockfile bytes, the loaded [`Workspace`] and the
//! files it reads through [`ProjectFiles`] (`.yarnrc.yml`, `pnpm-workspace.yaml`, package.json
//! of `file:` folders), so it can be tested without touching the disk.

pub(crate) mod manifest;
pub(crate) mod semver;
pub(crate) mod yaml;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fnpm_types::{
    Bin, ConfigVersion, GraphIndex, HashAlgorithm, Integrity, LockGraph, OverrideRule,
    OverrideSelector, Package, PackageExtension, PackageId, PatchedDependency, PeerMeta,
    PlatformConstraints, Resolution,
};
use fnpm_workspace::{Member, Workspace};
use serde_json::Value;

use crate::Error;

/// The npm registry, which yarn's `registry.yarnpkg.com` mirrors.
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/";

/// The result of migrating a foreign lockfile.
#[derive(Debug, Clone)]
pub struct Migration {
    /// The migrated graph, with resolved versions, tarball locations and integrity kept.
    pub graph: LockGraph,
    /// package.json edits that keep the next install reading the same thing yarn/pnpm did
    /// (bun `package_json_write_back`). Apply them only when the lockfile is saved.
    pub write_back: Vec<PackageJsonWriteBack>,
    /// Non-fatal problems, worded like bun's warnings.
    pub warnings: Vec<String>,
}

/// Edits to one package.json.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageJsonWriteBack {
    /// The workspace path of the package.json, `""` for the root.
    pub path: String,
    /// Top-level fields to set, in order, each to its complete new value. Fields not listed
    /// are left as they are. Apply with `PackageJsonEditor::set_path` to keep formatting.
    pub fields: Vec<(String, Value)>,
    /// What changed, as bun reports it (`copied pnpm.overrides to overrides`).
    pub changes: Vec<String>,
}

impl PackageJsonWriteBack {
    /// Applies the edits to a parsed package.json. New fields are appended.
    pub fn apply_to(&self, manifest: &mut Value) {
        if let Value::Object(map) = manifest {
            for (key, value) in &self.fields {
                map.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Reads project files for a migration.
pub trait ProjectFiles: Send + Sync {
    /// The UTF-8 contents of `path`, or `None` if it does not exist or cannot be read.
    fn read(&self, path: &Path) -> Option<String>;
}

/// [`ProjectFiles`] on the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiskFiles;

impl ProjectFiles for DiskFiles {
    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

/// The environment a migration reads.
pub struct MigrateOptions<'a> {
    /// Where `.yarnrc.yml`, pnpm-workspace.yaml and local package.json files are read from.
    pub files: &'a dyn ProjectFiles,
    /// The configured default registry (`install.registry`).
    pub registry: String,
    /// `@scope` (without `@`) to registry URL.
    pub scope_registries: BTreeMap<String, String>,
    /// `install.linkWorkspacePackages`: a range a workspace's version satisfies links to it.
    pub link_workspace_packages: bool,
    /// `--frozen-lockfile`: nothing the lockfile left out may be resolved.
    pub frozen: bool,
    /// Environment variables (only `YARN_*` are read).
    pub env: BTreeMap<String, String>,
    /// The home directory, for `~/.yarnrc.yml`.
    pub home: Option<PathBuf>,
}

impl std::fmt::Debug for MigrateOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrateOptions")
            .field("registry", &self.registry)
            .field("scope_registries", &self.scope_registries)
            .field("link_workspace_packages", &self.link_workspace_packages)
            .field("frozen", &self.frozen)
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl<'a> MigrateOptions<'a> {
    /// Defaults: the npm registry, workspace linking on, not frozen, no environment.
    pub fn new(files: &'a dyn ProjectFiles) -> Self {
        Self {
            files,
            registry: DEFAULT_REGISTRY.to_owned(),
            scope_registries: BTreeMap::new(),
            link_workspace_packages: true,
            frozen: false,
            env: BTreeMap::new(),
            home: None,
        }
    }

    /// [`MigrateOptions::new`] with `YARN_*` variables and `HOME` from the process.
    pub fn from_process(files: &'a dyn ProjectFiles) -> Self {
        let mut options = Self::new(files);
        // `vars()` panics on a variable that is not UTF-8 (a HOME with a Latin-1 name).
        options.env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .filter(|(k, _)| k.starts_with("YARN_"))
            .collect();
        options.home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        options
    }

    /// The registry a package is fetched from (its scope's, else the default).
    pub fn registry_for(&self, name: &str) -> &str {
        if let Some(scope) = name
            .strip_prefix('@')
            .and_then(|n| n.split_once('/'))
            .map(|(s, _)| s)
            && let Some(url) = self.scope_registries.get(scope)
        {
            return url;
        }
        &self.registry
    }

    pub(crate) fn env_var(&self, key: &str) -> Option<&str> {
        self.env
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

pub(crate) fn parse_error(ws: &Workspace, file: &str, message: impl Into<String>) -> Error {
    Error::Parse {
        path: ws.root_dir.join(file),
        message: message.into(),
    }
}

/// `true` if `url` is `registry` or below it (`registry.example.com.evil.com` is not).
pub(crate) fn url_is_under_registry(url: &str, registry: &str) -> bool {
    let registry = registry.trim_end_matches('/');
    url.starts_with(registry)
        && (url.len() == registry.len() || url.as_bytes()[registry.len()] == b'/')
}

/// `<registry>/<name>/-/<unscoped>-<version>.tgz`.
pub(crate) fn default_tarball_url(registry: &str, name: &str, version: &str) -> String {
    let registry = registry.trim_end_matches('/');
    let unscoped = name.rsplit_once('/').map_or(name, |(_, n)| n);
    format!("{registry}/{name}/-/{unscoped}-{version}.tgz")
}

/// Joins `/`-separated paths and resolves `.` and `..`. An absolute `path` is returned as is.
/// The result has no leading `./`; `..` that climb above `base` are kept.
pub(crate) fn join_posix(base: &str, path: &str) -> String {
    if path.starts_with('/') {
        return normalize_posix(path);
    }
    if base.is_empty() || base == "." {
        return normalize_posix(path);
    }
    normalize_posix(&format!("{base}/{path}"))
}

/// Resolves `.`, `..` and repeated `/` in a relative or absolute `/`-separated path.
pub(crate) fn normalize_posix(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if absolute => {}
                _ => parts.push(".."),
            },
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// The relative `/`-separated path from directory `from` to `to` (both normalized, relative
/// to the same root). Empty when they are the same.
pub(crate) fn relative_posix(from: &str, to: &str) -> String {
    let from: Vec<&str> = from
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let to: Vec<&str> = to
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    parts.join("/")
}

/// The root package of `ws` with its declared dependencies (unresolved).
pub(crate) fn root_package(ws: &Workspace) -> Package {
    let raw = &ws.root.raw;
    let mut pkg = Package::new(
        manifest::get_str(raw, "name").unwrap_or_default(),
        manifest::get_str(raw, "version").unwrap_or_default(),
        Resolution::Root,
    );
    pkg.dependencies = manifest::manifest_edges(raw);
    pkg.peer_meta = manifest::peer_meta(raw);
    pkg.bin = manifest::bin(raw);
    pkg.platform = manifest::platform(raw);
    pkg
}

/// A workspace member's package with its declared dependencies (unresolved).
pub(crate) fn workspace_package(member: &Member) -> Package {
    let raw = &member.manifest.raw;
    let mut pkg = Package::new(
        member.name.clone(),
        member.version.clone(),
        Resolution::Workspace {
            path: member.path.clone(),
        },
    );
    pkg.dependencies = manifest::manifest_edges(raw);
    pkg.peer_meta = manifest::peer_meta(raw);
    pkg.bin = manifest::bin(raw);
    pkg
}

/// Appends `pkg` unless a package with the same name and resolution exists (bun
/// `append_package_dedupe`). Returns the id either way.
pub(crate) fn intern(graph: &mut LockGraph, index: &mut GraphIndex, pkg: Package) -> PackageId {
    if let Some(id) = index.get(&pkg.name, &pkg.resolution) {
        return id;
    }
    let (name, resolution) = (pkg.name.clone(), pkg.resolution.clone());
    let id = if matches!(pkg.resolution, Resolution::Workspace { .. }) {
        graph.add_workspace(pkg)
    } else {
        graph.push(pkg)
    };
    index.insert(&name, &resolution, id);
    id
}

/// A fresh resolve only records `os`/`cpu`/`libc` for the root and registry packages, so a
/// migrated graph must not constrain folders, tarballs, git packages or workspaces (bun
/// `clear_non_registry_platform_constraints`).
pub(crate) fn clear_non_registry_platform(graph: &mut LockGraph) {
    for p in &mut graph.packages {
        if !matches!(p.resolution, Resolution::Root | Resolution::Npm { .. }) {
            p.platform = PlatformConstraints::ANY;
        }
    }
}

/// The real package name and range a peer edge asks for (`npm:real@^1` gives `real`, `^1`).
pub(crate) fn peer_target(name: &str, literal: &str) -> (String, String) {
    let plain = fnpm_types::spec::without_npm_protocol(literal);
    if let Some(alias) = plain.strip_prefix("npm:") {
        let (real, range) = fnpm_types::spec::split_npm_alias(alias);
        return (real.to_owned(), range.unwrap_or("*").to_owned());
    }
    (name.to_owned(), plain.to_owned())
}

/// The version a package satisfies a range with: its npm version, or a local package's own
/// package.json version.
fn candidate_version(p: &Package) -> Option<semver::Version> {
    match &p.resolution {
        Resolution::Npm { version, .. } => semver::exact_version(version),
        Resolution::Root | Resolution::Workspace { .. } => semver::exact_version(&p.version),
        _ => None,
    }
}

/// An override rule targets `name`.
pub(crate) fn is_overridden(graph: &LockGraph, name: &str) -> bool {
    graph
        .overrides
        .iter()
        .any(|r| r.selector.target.name == name)
}

/// Binds a peer the way loading bun.lock does (`resolve_peer_dep_by_range`): the highest
/// version of that (real) name in the graph that satisfies the range, after `catalog:`. `None`
/// when nothing does or the range is not semver.
pub(crate) fn bind_peer_by_range(
    graph: &LockGraph,
    name: &str,
    literal: &str,
) -> Option<PackageId> {
    let literal = resolve_catalog(graph, name, literal).unwrap_or(literal);
    let (real, range) = peer_target(name, literal);
    let range = semver::Range::parse(&range)?;
    best_by_range(graph, &real, &range)
}

/// The highest-versioned package named `name` whose version satisfies `range`.
pub(crate) fn best_by_range(
    graph: &LockGraph,
    name: &str,
    range: &semver::Range,
) -> Option<PackageId> {
    let mut best: Option<(semver::Version, PackageId)> = None;
    for id in graph.by_name(name) {
        let Some(v) = candidate_version(&graph.packages[id.index()]) else {
            continue;
        };
        if range.satisfies(&v, false) && best.as_ref().is_none_or(|(b, _)| v > *b) {
            best = Some((v, id));
        }
    }
    best.map(|(_, id)| id)
}

/// The highest-versioned package named `name`, for a peer nothing satisfies.
pub(crate) fn newest(graph: &LockGraph, name: &str) -> Option<PackageId> {
    let mut best: Option<(semver::Version, PackageId)> = None;
    let mut first = None;
    for id in graph.by_name(name) {
        first.get_or_insert(id);
        if let Some(v) = candidate_version(&graph.packages[id.index()])
            && best.as_ref().is_none_or(|(b, _)| v > *b)
        {
            best = Some((v, id));
        }
    }
    best.map(|(_, id)| id).or(first)
}

/// The range `catalog:<name>` stands for.
pub(crate) fn resolve_catalog<'g>(
    graph: &'g LockGraph,
    name: &str,
    literal: &str,
) -> Option<&'g str> {
    let catalog = literal.strip_prefix("catalog:")?.trim();
    graph.catalogs.get(catalog, name)
}

/// The workspace an npm range links to (bun `linked_workspace_path`): the member of that name
/// when the range is `*` or its version satisfies the range.
pub(crate) fn linked_workspace(
    graph: &LockGraph,
    link: bool,
    name: &str,
    literal: &str,
) -> Option<PackageId> {
    if !link {
        return None;
    }
    let (real, range) = peer_target(name, literal);
    let range = semver::Range::parse(&range)?;
    let id = graph
        .workspaces
        .iter()
        .filter(|(path, _)| !path.is_empty())
        .map(|(_, id)| *id)
        .find(|id| graph.packages[id.index()].name == real)?;
    if range.is_star() {
        return Some(id);
    }
    let v = semver::exact_version(&graph.packages[id.index()].version)?;
    range.satisfies(&v, false).then_some(id)
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    match value {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

fn read_catalogs(owner: &Value, graph: &mut LockGraph) -> bool {
    let mut found = false;
    if let Some(Value::Object(_)) = owner.get("catalog") {
        for (k, v) in string_map(owner.get("catalog")) {
            graph.catalogs.default.entry(k).or_insert(v);
        }
        found = true;
    }
    if let Some(Value::Object(groups)) = owner.get("catalogs") {
        for (group, entries) in groups {
            let entries = string_map(Some(entries));
            let target = if group == "default" {
                &mut graph.catalogs.default
            } else {
                graph.catalogs.named.entry(group.clone()).or_default()
            };
            for (k, v) in entries {
                target.entry(k).or_insert(v);
            }
        }
        found = true;
    }
    found
}

/// npm `overrides` (one level of nesting, `.` for the parent itself, `$name` references), else
/// yarn `resolutions`, in declaration order (bun `OverrideMap::parse`).
pub(crate) fn root_overrides(root: &Value) -> Vec<OverrideRule> {
    let mut rules = Vec::new();
    let reference = |value: &str| -> String {
        let Some(name) = value.strip_prefix('$') else {
            return value.to_owned();
        };
        manifest::GROUPS
            .iter()
            .filter(|(g, _)| *g != "peerDependencies")
            .find_map(|(g, _)| {
                manifest::string_entries(root, g)
                    .into_iter()
                    .find(|(n, _)| *n == name)
            })
            .map_or_else(|| value.to_owned(), |(_, spec)| spec.to_owned())
    };
    let mut push = |key: &str, value: &str, parent: Option<&OverrideSelector>| {
        if value == "-" {
            return;
        }
        let Ok(Some(mut selector)) = OverrideSelector::parse(key) else {
            return;
        };
        if let Some(parent) = parent {
            if selector.parent.is_some() {
                return;
            }
            selector.parent = Some(parent.target.clone());
        }
        rules.push(OverrideRule {
            selector,
            value: reference(value),
        });
    };
    if let Some(Value::Object(overrides)) = root.get("overrides") {
        for (key, value) in overrides {
            if key.starts_with("//") {
                continue;
            }
            match value {
                Value::String(v) => push(key, v, None),
                Value::Object(nested) => {
                    let Ok(Some(parent)) = OverrideSelector::parse(key) else {
                        continue;
                    };
                    if let Some(Value::String(own)) = nested.get(".") {
                        push(key, own, None);
                    }
                    for (child, v) in nested {
                        if child == "." || child.starts_with("//") {
                            continue;
                        }
                        if let Value::String(v) = v {
                            push(child, v, Some(&parent));
                        }
                    }
                }
                _ => {}
            }
        }
        return rules;
    }
    for (key, value) in manifest::string_entries(root, "resolutions") {
        push(key, value, None);
    }
    rules
}

/// Fills the graph's root-level settings from the root package.json as it will be after the
/// write-back: overrides/resolutions (unless the lockfile recorded its own), catalogs, trustedDependencies, patchedDependencies and
/// packageExtensions (bun reads these when it parses the migrated root).
pub(crate) fn apply_root_settings(graph: &mut LockGraph, root: &Value) {
    // A lockfile's own rules (pnpm `overrides:`) are what it was resolved with.
    if graph.overrides.is_empty() {
        graph.overrides = root_overrides(root);
    }
    let in_workspaces = root
        .get("workspaces")
        .filter(|w| w.is_object())
        .is_some_and(|w| read_catalogs(w, graph));
    if !in_workspaces {
        read_catalogs(root, graph);
    }
    if let Some(Value::Array(items)) = root.get("trustedDependencies") {
        graph.trusted_dependencies = Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>(),
        );
    }
    for (key, path) in string_map(root.get("patchedDependencies")) {
        graph
            .patched_dependencies
            .entry(key)
            .or_insert(PatchedDependency { path, hash: None });
    }
    if let Some(Value::Object(rules)) = root.get("packageExtensions") {
        for (key, rule) in rules {
            let mut ext = PackageExtension {
                dependencies: string_map(rule.get("dependencies")),
                optional_dependencies: string_map(rule.get("optionalDependencies")),
                peer_dependencies: string_map(rule.get("peerDependencies")),
                ..Default::default()
            };
            for name in manifest::optional_peer_names(rule) {
                ext.peer_dependencies_meta
                    .insert(name.to_owned(), PeerMeta { optional: true });
            }
            graph.package_extensions.insert(key.clone(), ext);
        }
    }
}

/// `root` with the root write-back applied.
pub(crate) fn effective_root(root: &Value, write_back: &[PackageJsonWriteBack]) -> Value {
    let mut root = root.clone();
    for edit in write_back.iter().filter(|e| e.path.is_empty()) {
        edit.apply_to(&mut root);
    }
    root
}

/// Last steps every converter shares: platform constraints of non-registry packages cleared,
/// dependencies sorted, and the config version bun's `choose_config_version` gives migrations.
pub(crate) fn finish(graph: &mut LockGraph, config_version: ConfigVersion) {
    clear_non_registry_platform(graph);
    graph.sort_dependencies();
    graph.config_version = config_version;
}

/// bun's `Integrity::parse`: the strongest supported entry; a digest shorter than its
/// algorithm's is zero-padded (bun keeps such placeholder hashes, and so does the lockfile it
/// writes), which `fnpm_types::Integrity::parse` rejects.
pub(crate) fn parse_integrity(sri: &str) -> Option<Integrity> {
    sri.split_ascii_whitespace()
        .filter_map(|entry| {
            let (algo, rest) = entry.split_once('-')?;
            let (algorithm, len) = match algo {
                "sha1" => (HashAlgorithm::Sha1, 20),
                "sha256" => (HashAlgorithm::Sha256, 32),
                "sha384" => (HashAlgorithm::Sha384, 48),
                "sha512" => (HashAlgorithm::Sha512, 64),
                _ => return None,
            };
            let b64 = rest
                .split_once('?')
                .map_or(rest, |(v, _)| v)
                .trim_end_matches('=');
            let mut digest = decode_base64(b64)?;
            if digest.len() > len {
                return None;
            }
            digest.resize(len, 0);
            Integrity::from_digest(algorithm, &digest).ok()
        })
        .max_by_key(|i| i.algorithm())
}

/// Standard-alphabet base64 without padding. `None` on another character, an impossible
/// length, or non-zero bits after the last byte (like bun's decoder).
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
}

/// The bin of a `file:` folder or `link:` package, read from its package.json (yarn.lock and
/// pnpm-lock.yaml do not record bins of local packages).
pub(crate) fn local_bin(ws: &Workspace, options: &MigrateOptions<'_>, folder: &str) -> Bin {
    let path = ws.root_dir.join(folder).join("package.json");
    options
        .files
        .read(&path)
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map_or(Bin::None, |m| manifest::bin(&m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lenient_integrity() {
        let full = "sha512-3X6cn4+UJdXJuLPu11v8i/fGLe2PdI6v1yKTELam04lY5esCAFdG/qQts6N6rLrL6g1YRq+MKBAwxbmUQk355A==";
        assert_eq!(parse_integrity(full).unwrap().to_sri(), full);
        assert_eq!(
            parse_integrity("sha512-clsx==").unwrap().to_sri(),
            "sha512-clsxAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
        );
        assert!(parse_integrity("sha512-!!").is_none());
        // five characters, or bits left over after the last byte, are not base64
        assert!(parse_integrity("sha512-chalk==").is_none());
        assert!(parse_integrity("sha512-reactcanary==").is_none());
        assert!(parse_integrity("md5-abc").is_none());
        assert!(parse_integrity("sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").is_none());
        assert_eq!(
            parse_integrity("sha1-W4o6d2Xf4AEmHd6RVYnngvjJTR4= sha512-xA==")
                .unwrap()
                .algorithm(),
            HashAlgorithm::Sha512
        );
    }

    #[test]
    fn paths() {
        assert_eq!(join_posix("", "./a/b/"), "a/b");
        assert_eq!(join_posix("packages/x", "../y"), "packages/y");
        assert_eq!(join_posix("a", "../../b"), "../b");
        assert_eq!(join_posix("a", "/abs/p"), "/abs/p");
        assert_eq!(normalize_posix("./x/./y//z/.."), "x/y");
        assert_eq!(relative_posix("vendor/a", "vendor/b"), "../b");
        assert_eq!(relative_posix("a", "a"), "");
        assert_eq!(relative_posix("", "a/b"), "a/b");
    }

    #[test]
    fn registry_urls() {
        assert!(url_is_under_registry(
            "https://r.example/a/-/a-1.tgz",
            "https://r.example/"
        ));
        assert!(!url_is_under_registry(
            "https://r.example.evil/a",
            "https://r.example"
        ));
        assert_eq!(
            default_tarball_url("https://registry.npmjs.org/", "@s/n", "1.0.0"),
            "https://registry.npmjs.org/@s/n/-/n-1.0.0.tgz"
        );
        let files = DiskFiles;
        let mut o = MigrateOptions::new(&files);
        o.scope_registries
            .insert("s".into(), "https://s.example/".into());
        assert_eq!(o.registry_for("@s/x"), "https://s.example/");
        assert_eq!(o.registry_for("x"), DEFAULT_REGISTRY);
    }

    #[test]
    fn overrides_and_settings() {
        let root = json!({
            "dependencies": { "foo": "^1.2.3" },
            "overrides": { "a": "1.0.0", "b": { ".": "2.0.0", "c": "3.0.0" }, "d": "$foo", "//": "x" },
            "resolutions": { "ignored": "1" },
            "workspaces": { "packages": ["p/*"], "catalog": { "r": "^18" }, "catalogs": { "x": { "r": "^17" } } },
            "trustedDependencies": ["esbuild"],
            "patchedDependencies": { "a@1.0.0": "patches/a.patch" },
            "packageExtensions": { "e@1": { "dependencies": { "f": "1" }, "peerDependenciesMeta": { "g": { "optional": true } } } }
        });
        let mut g = LockGraph::new(Package::new("r", "", Resolution::Root));
        apply_root_settings(&mut g, &root);
        let keys: Vec<(String, String)> = g
            .overrides
            .iter()
            .map(|r| (r.selector.to_key(), r.value.clone()))
            .collect();
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[0].1, "1.0.0");
        assert_eq!(keys[2].1, "3.0.0");
        assert!(g.overrides[2].selector.parent.is_some());
        assert_eq!(keys[3].1, "^1.2.3");
        assert_eq!(g.catalogs.get("", "r"), Some("^18"));
        assert_eq!(g.catalogs.get("x", "r"), Some("^17"));
        assert!(g.trusted_dependencies.as_ref().unwrap().contains("esbuild"));
        assert_eq!(g.patched_dependencies["a@1.0.0"].path, "patches/a.patch");
        assert!(g.package_extensions["e@1"].peer_dependencies_meta["g"].optional);

        let yarn = json!({ "resolutions": { "z/y": "1", "a": "2" } });
        let rules = root_overrides(&yarn);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].selector.target.name, "y");
    }

    #[test]
    fn peer_binding() {
        let mut g = LockGraph::new(Package::new("r", "", Resolution::Root));
        let a1 = g.push(Package::new("a", "1.0.0", Resolution::npm("1.0.0", None)));
        let a2 = g.push(Package::new("a", "2.0.0", Resolution::npm("2.0.0", None)));
        assert_eq!(bind_peer_by_range(&g, "a", "^1"), Some(a1));
        assert_eq!(bind_peer_by_range(&g, "a", "*"), Some(a2));
        assert_eq!(bind_peer_by_range(&g, "a", "^3"), None);
        assert_eq!(bind_peer_by_range(&g, "x", "npm:a@^2"), Some(a2));
        assert_eq!(newest(&g, "a"), Some(a2));
    }
}
