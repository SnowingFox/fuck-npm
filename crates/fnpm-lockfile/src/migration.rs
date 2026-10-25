//! Migration from foreign lockfiles (bun `install/migration.rs`).
//!
//! [`migrate_dir`] finds `npm-shrinkwrap.json`, `package-lock.json`, `yarn.lock` and
//! `pnpm-lock.yaml` in that order and converts the first one that migrates; a lockfile that
//! fails is reported and the next one is tried. The helpers here are shared by the format
//! modules: registry URL classification, root `overrides` / `resolutions`,
//! `trustedDependencies` and `patchedDependencies` from the manifests, and the graph
//! post-processing every migration ends with.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fnpm_config::{Config, DEFAULT_REGISTRY};
use fnpm_types::overrides::parse_package_segment;
use fnpm_types::{
    Behavior, ConfigVersion, DependencyGroup, LockGraph, OverrideRule, OverrideSelector, PackageId,
    PatchedDependency, PlatformConstraints, Resolution,
};
use fnpm_workspace::Workspace;
use serde_json::Value;

use crate::{Error, Format, Result, formats};

pub(crate) mod json;
#[cfg(test)]
mod tests;

/// Settings a migration reads from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateOptions {
    /// The default registry, ending with `/`.
    pub registry: String,
    /// `@scope` to registry URL.
    pub scopes: BTreeMap<String, String>,
    /// `--frozen-lockfile`: a root or workspace peer without a lockfile entry is kept as an
    /// unresolved edge instead of being left for the resolver.
    pub frozen_lockfile: bool,
    /// `linkWorkspacePackages`: a peer whose range a workspace's version satisfies links it.
    pub link_workspace_packages: bool,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            registry: DEFAULT_REGISTRY.to_owned(),
            scopes: BTreeMap::new(),
            frozen_lockfile: false,
            link_workspace_packages: true,
        }
    }
}

impl MigrateOptions {
    /// The options of a loaded configuration.
    pub fn from_config(config: &Config) -> Self {
        Self {
            registry: config.registry.url.clone(),
            scopes: config
                .scopes
                .iter()
                .map(|(k, v)| (k.clone(), v.url.clone()))
                .collect(),
            frozen_lockfile: config.frozen_lockfile,
            link_workspace_packages: config.link_workspace_packages,
        }
    }

    /// The registry `name` is fetched from (`@scope:registry`, else the default).
    pub fn registry_for(&self, name: &str) -> &str {
        name.strip_prefix('@')
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(scope, _)| self.scopes.get(&format!("@{scope}")))
            .map_or(self.registry.as_str(), String::as_str)
    }
}

/// What a migration had to say. bun prints these as it goes; fnpm returns them so the CLI
/// decides how to render them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Printed unless the log level is silent.
    pub warnings: Vec<String>,
    /// Printed with `--verbose` only.
    pub verbose: Vec<String>,
    /// `note:` lines that follow the warnings.
    pub notes: Vec<String>,
    /// Registry packages whose integrity the lockfile did not record and whose tarball is
    /// therefore fetched from the configured registry. The install fills their integrity from
    /// the registry manifest (bun `fetch_necessary_package_metadata_after_yarn_or_pnpm_migration`)
    /// and fails if the registry has none for the version.
    pub missing_integrity: Vec<PackageId>,
}

impl Report {
    pub(crate) fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    pub(crate) fn verbose(&mut self, message: impl Into<String>) {
        self.verbose.push(message.into());
    }

    fn absorb(&mut self, other: Report) {
        self.warnings.extend(other.warnings);
        self.verbose.extend(other.verbose);
        self.notes.extend(other.notes);
        self.missing_integrity.extend(other.missing_integrity);
    }
}

/// A successful migration.
#[derive(Debug, Clone)]
pub struct Migrated {
    /// The migrated graph, finished with the manifests' trust and patches.
    pub graph: LockGraph,
    /// The format of the lockfile that was migrated.
    pub format: Format,
    /// The lockfile that was migrated.
    pub path: PathBuf,
    /// Warnings and notes of every lockfile that was tried.
    pub report: Report,
}

/// The foreign lockfiles bun migrates from, in the order it tries them.
pub const MIGRATION_ORDER: [&str; 4] = [
    "npm-shrinkwrap.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
];

/// `true` for a yarn berry lockfile (it has a `__metadata` entry).
pub fn is_yarn_berry(bytes: &[u8]) -> bool {
    memchr::memmem::find(bytes, b"__metadata:").is_some()
}

/// Migrates the highest-priority foreign lockfile in `dir` (bun
/// `detect_and_load_other_lockfile`). Lockfiles that cannot be read are skipped. When one
/// fails to migrate and another one exists, a warning is recorded and the next one is tried;
/// the error of the last one is returned. `Ok(None)` when there is none.
///
/// The migrated graph has `trustedDependencies` and `patchedDependencies` from the manifests
/// (bun `finish_migration`); the foreign file is left in place.
pub fn migrate_dir(
    dir: &Path,
    workspace: &Workspace,
    options: &MigrateOptions,
) -> Result<Option<Migrated>> {
    let found: Vec<(&str, PathBuf, Vec<u8>)> = MIGRATION_ORDER
        .iter()
        .filter_map(|name| {
            let path = dir.join(name);
            std::fs::read(&path).ok().map(|bytes| (*name, path, bytes))
        })
        .collect();
    let mut report = Report::default();
    for (i, (name, path, bytes)) in found.iter().enumerate() {
        let next = found.get(i + 1).map(|f| f.0);
        let attempt = match *name {
            "yarn.lock" if is_yarn_berry(bytes) => formats::yarn_berry::parse(bytes, workspace)
                .map(|g| (g, Format::YarnBerry, Report::default())),
            "yarn.lock" => formats::yarn_v1::parse(bytes, workspace)
                .map(|g| (g, Format::YarnV1, Report::default())),
            "pnpm-lock.yaml" => {
                formats::pnpm::parse(bytes, workspace).map(|g| (g, Format::Pnpm, Report::default()))
            }
            _ => formats::npm::migrate(bytes, workspace, options, name, next)
                .map(|(g, r)| (g, Format::Npm, r)),
        };
        match attempt {
            Ok((mut graph, format, attempt_report)) => {
                report.absorb(attempt_report);
                finish_migration(&mut graph, workspace);
                return Ok(Some(Migrated {
                    graph,
                    format,
                    path: path.clone(),
                    report,
                }));
            }
            Err(err) => {
                let Some(next) = next else {
                    return Err(err);
                };
                if is_unsupported_version(&err) {
                    report.warn(format!("{}, trying {next}", short_error(&err)));
                } else {
                    report.warn(format!(
                        "cannot migrate {name} ({}), trying {next}",
                        short_error(&err)
                    ));
                }
            }
        }
    }
    Ok(None)
}

fn is_unsupported_version(err: &Error) -> bool {
    matches!(err, Error::Parse { message, .. } if message.contains("unsupported lockfileVersion"))
}

fn short_error(err: &Error) -> String {
    match err {
        Error::Parse { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// `true` if `url` points at a resource under `registry`: the registry href without its
/// trailing slash must be a prefix, followed by the end or a `/` (bun `url_is_under_registry`).
pub fn url_is_under_registry(url: &str, registry: &str) -> bool {
    let registry = registry.trim_end_matches('/');
    url.starts_with(registry)
        && (url.len() == registry.len() || url.as_bytes()[registry.len()] == b'/')
}

/// `<registry>/<name>/-/<unscoped>-<version>.tgz` (bun `ExtractTarball::build_url`).
pub fn default_tarball_url(registry: &str, name: &str, version: &str) -> String {
    let registry = registry.trim_end_matches('/');
    let unscoped = match name.strip_prefix('@').and_then(|rest| rest.split_once('/')) {
        Some((_, unscoped)) => unscoped,
        None => name,
    };
    format!("{registry}/{name}/-/{unscoped}-{version}.tgz")
}

/// npm's `name-from-folder`: the basename, keeping an `@scope` parent component.
pub(crate) fn package_name_from_path(path: &str) -> &str {
    let Some(slash) = path.rfind('/') else {
        return path;
    };
    let parent = &path[..slash];
    let parent_start = parent.rfind('/').map_or(0, |i| i + 1);
    if parent[parent_start..].starts_with('@') {
        &path[parent_start..]
    } else {
        &path[slash + 1..]
    }
}

/// The longest `.bun-tag` bun accepts.
pub(crate) const MAX_RESOLVED_TAG_LEN: usize = 256;

/// bun `is_safe_resolved_tag`: a git `.bun-tag` that is a safe path component.
pub(crate) fn is_safe_resolved_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= MAX_RESOLVED_TAG_LEN
        && !tag.starts_with('-')
        && tag != "."
        && tag != ".."
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// bun `bin_target_escapes_package_dir`: a drive prefix, an absolute path, a NUL, or a `..`
/// that climbs above the start.
pub(crate) fn path_escapes(target: &str) -> bool {
    if target
        .split(['/', '\\'])
        .next()
        .is_some_and(|first| first.contains(':'))
    {
        return true;
    }
    if target.starts_with('/') || target.starts_with('\\') || target.contains('\0') {
        return true;
    }
    let mut depth: isize = 0;
    for component in target.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

/// What every migration does last (bun `finish_migration`): `trustedDependencies` from the
/// root and workspace manifests, and the root's `patchedDependencies`. Neither is recorded by
/// other package managers' lockfiles.
pub fn finish_migration(graph: &mut LockGraph, workspace: &Workspace) {
    // A converter that recorded trust itself (pnpm's `onlyBuiltDependencies`) keeps it, joined
    // with the manifests'.
    graph.trusted_dependencies = match (
        graph.trusted_dependencies.take(),
        trusted_dependencies_from_manifests(workspace),
    ) {
        (Some(mut recorded), Some(declared)) => {
            recorded.extend(declared);
            Some(recorded)
        }
        (recorded, declared) => declared.or(recorded),
    };
    for (key, patch) in patched_dependencies(&workspace.root.raw) {
        graph.patched_dependencies.insert(key, patch);
    }
}

/// The union of `trustedDependencies` of the root and every workspace. `None` when no manifest
/// has the field (bun `read_trusted_dependencies_from_manifests`).
pub fn trusted_dependencies_from_manifests(workspace: &Workspace) -> Option<BTreeSet<String>> {
    let mut trusted: Option<BTreeSet<String>> = None;
    let manifests = std::iter::once(&workspace.root.raw)
        .chain(workspace.members.iter().map(|m| &m.manifest.raw));
    for raw in manifests {
        let Some(list) = raw.get("trustedDependencies").and_then(Value::as_array) else {
            continue;
        };
        let set = trusted.get_or_insert_with(BTreeSet::new);
        set.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    trusted
}

/// The root package.json `patchedDependencies` (`"name@version": "patches/x.patch"`).
pub fn patched_dependencies(root: &Value) -> Vec<(String, PatchedDependency)> {
    let Some(map) = root.get("patchedDependencies").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(key, path)| {
            let path = path.as_str()?;
            Some((
                key.clone(),
                PatchedDependency {
                    path: path.to_owned(),
                    hash: None,
                },
            ))
        })
        .collect()
}

/// A fresh resolve only records `os`/`cpu`/`libc` for the root and registry packages; folder,
/// tarball, git and workspace packages install unconditionally, so a migrated graph must not
/// constrain them (bun `clear_non_registry_platform_constraints`).
pub fn clear_non_registry_platform_constraints(graph: &mut LockGraph) {
    for package in &mut graph.packages {
        if !matches!(
            package.resolution,
            Resolution::Root | Resolution::Npm { .. }
        ) {
            package.platform = PlatformConstraints::ANY;
        }
    }
}

/// The root's `overrides` (npm) or, without them, `resolutions` (yarn), parsed the way an
/// install does (bun `OverrideMap::parse_append`). `$name` values take the root's declared
/// spec for `name`, or the one every workspace agrees on. Invalid rules are skipped with a
/// warning in `report`.
pub fn root_overrides(
    graph: &LockGraph,
    workspace: &Workspace,
    report: &mut Report,
) -> Vec<OverrideRule> {
    let root = &workspace.root.raw;
    let (field, rules) = match (root.get("overrides"), root.get("resolutions")) {
        (Some(rules), _) => ("overrides", rules),
        (None, Some(rules)) => ("resolutions", rules),
        (None, None) => return Vec::new(),
    };
    let Some(rules) = rules.as_object() else {
        report.warn(format!("\"{field}\" must be an object"));
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, value) in rules {
        if key.is_empty() {
            report.warn(format!("Missing overridden package name in \"{field}\""));
            continue;
        }
        if key.starts_with("//") {
            continue;
        }
        if let Some(value) = value.as_str() {
            match OverrideSelector::parse(key) {
                Ok(Some(selector)) => {
                    push_rule(&mut out, graph, workspace, report, field, selector, value)
                }
                Ok(None) => {}
                Err(err) => report.warn(format!("Invalid override \"{key}\": {err}")),
            }
            continue;
        }
        let Some(children) = value.as_object().filter(|_| field == "overrides") else {
            report.warn(format!(
                "Invalid {} value for \"{key}\"",
                &field[..field.len() - 1]
            ));
            continue;
        };
        let parent = match parse_package_segment(key) {
            Ok(parent) => parent,
            Err(err) => {
                report.warn(format!("Invalid override \"{key}\": {err}"));
                continue;
            }
        };
        for (child_key, child_value) in children {
            if child_key.starts_with("//") {
                continue;
            }
            if child_value.is_object() {
                report.warn(format!("Invalid override \"{child_key}\": only one level of nested overrides is supported"));
                continue;
            }
            let Some(child_value) = child_value.as_str() else {
                report.warn(format!("Invalid override value for \"{child_key}\""));
                continue;
            };
            if child_key == "." {
                let selector = OverrideSelector {
                    parent: None,
                    target: parent.clone(),
                };
                push_rule(
                    &mut out,
                    graph,
                    workspace,
                    report,
                    field,
                    selector,
                    child_value,
                );
                continue;
            }
            match OverrideSelector::parse(child_key) {
                Ok(Some(OverrideSelector { parent: None, target })) => {
                    let selector = OverrideSelector { parent: Some(parent.clone()), target };
                    push_rule(&mut out, graph, workspace, report, field, selector, child_value);
                }
                Ok(Some(_)) => report.warn(format!(
                    "Invalid override \"{child_key}\": only one level of nested overrides is supported"
                )),
                Ok(None) => {}
                Err(err) => report.warn(format!("Invalid override \"{child_key}\": {err}")),
            }
        }
    }
    out
}

fn push_rule(
    out: &mut Vec<OverrideRule>,
    graph: &LockGraph,
    workspace: &Workspace,
    report: &mut Report,
    field: &str,
    selector: OverrideSelector,
    value: &str,
) {
    let label = &field[..field.len() - 1];
    if value.starts_with("patch:") {
        report.warn(format!(
            "Bun currently does not support patched package \"{field}\""
        ));
        return;
    }
    if value.is_empty() {
        report.warn(format!("Missing {label} value"));
        return;
    }
    if value == "-" {
        report.warn(format!(
            "Removing \"{}\" with \"-\" is not supported",
            selector.target.name
        ));
        return;
    }
    let value = match value.strip_prefix('$') {
        None => value.to_owned(),
        Some(reference) => match reference_literal(graph, workspace, reference) {
            Ok(Some(literal)) => literal,
            Ok(None) => {
                report.warn(format!(
                    "Could not resolve \"{value}\": \"{reference}\" is not in dependencies"
                ));
                return;
            }
            Err(()) => {
                report.warn(format!(
                    "Could not resolve \"{value}\": workspaces declare different versions of \"{reference}\""
                ));
                return;
            }
        },
    };
    out.push(OverrideRule { selector, value });
}

/// The spec for `name` that a `$name` override value refers to: the root's own declaration,
/// else the one all workspaces that declare it agree on (`Err` when they disagree).
fn reference_literal(
    graph: &LockGraph,
    workspace: &Workspace,
    name: &str,
) -> std::result::Result<Option<String>, ()> {
    if let Some(edge) = graph
        .root()
        .dependencies
        .iter()
        .find(|e| e.name == name && !is_workspace_only(e.behavior))
    {
        return Ok(Some(edge.literal.clone()));
    }
    for group in DependencyGroup::ALL {
        if let Some((_, spec)) = workspace.root.group(group).iter().find(|(n, _)| n == name) {
            return Ok(Some(spec.clone()));
        }
    }
    let mut found: Option<&str> = None;
    for member in &workspace.members {
        for group in DependencyGroup::ALL {
            if let Some((_, spec)) = member.manifest.group(group).iter().find(|(n, _)| n == name) {
                match found {
                    Some(prev) if prev != spec => return Err(()),
                    _ => found = Some(spec),
                }
            }
        }
    }
    Ok(found.map(str::to_owned))
}

/// The synthetic root-to-workspace edge (only the `WORKSPACE` bit).
pub(crate) fn is_workspace_only(behavior: Behavior) -> bool {
    behavior.is_workspace()
        && !behavior
            .intersects(Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER)
}

/// `config_version` for a migrated graph (bun `choose_config_version`): pnpm migrations get
/// V1, everything else V0.
pub fn migrated_config_version(format: Format) -> ConfigVersion {
    if format == Format::Pnpm {
        ConfigVersion::V1
    } else {
        ConfigVersion::V0
    }
}

/// The URL of a git resolution as bun prints it after `git+`: an scp-like `host:path` gets
/// `ssh://` in front.
pub(crate) fn git_display_url(repo: &str) -> String {
    if fnpm_types::spec::is_scp_like_path(repo) {
        format!("ssh://{repo}")
    } else {
        repo.to_owned()
    }
}

/// The semver operations lockfile readers need, behind a trait so readers can be tested
/// without the real implementation.
pub(crate) trait Semver {
    /// `version` satisfies the npm `range`. Unparsable input satisfies nothing.
    fn satisfies(&self, version: &str, range: &str) -> bool;
    /// Version order; unparsable versions sort before parsable ones, then by text.
    fn compare(&self, a: &str, b: &str) -> std::cmp::Ordering;
    /// `version` is a valid semver version.
    fn is_valid(&self, version: &str) -> bool;
}

/// [`Semver`] backed by `fnpm-semver`.
pub(crate) struct NodeSemver;

impl Semver for NodeSemver {
    fn satisfies(&self, version: &str, range: &str) -> bool {
        let (Ok(version), Ok(range)) = (
            fnpm_semver::Version::parse(version),
            fnpm_semver::Range::parse(range),
        ) else {
            return false;
        };
        range.satisfies(&version)
    }

    fn compare(&self, a: &str, b: &str) -> std::cmp::Ordering {
        match (
            fnpm_semver::Version::parse(a),
            fnpm_semver::Version::parse(b),
        ) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
            (Err(_), Err(_)) => a.cmp(b),
        }
    }

    fn is_valid(&self, version: &str) -> bool {
        fnpm_semver::Version::parse(version).is_ok()
    }
}

/// The `workspaces` patterns of a manifest: an array, or `{ "packages": [...] }`.
pub(crate) fn workspace_patterns(value: &Value) -> Option<Vec<String>> {
    let list = match value {
        Value::Array(list) => list,
        Value::Object(obj) => obj.get("packages")?.as_array()?,
        _ => return None,
    };
    Some(
        list.iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

/// A workspace member a lockfile's `workspaces` patterns select: `(path, name, version)`.
pub(crate) type MemberRow = (String, String, String);

/// The members that `patterns` (from a lockfile's root entry) select, the way bun's
/// `WorkspaceMap::process_names_array` reads them: each matching directory with a
/// package.json that has a `name`; others are skipped. When the patterns are the root
/// package.json's own, the already-loaded workspace members are used; otherwise the
/// directories under the workspace root are globbed.
pub(crate) fn lockfile_workspace_members(
    patterns: &[String],
    workspace: &Workspace,
) -> Vec<MemberRow> {
    let declared = workspace
        .root
        .raw
        .get("workspaces")
        .and_then(workspace_patterns);
    if declared.as_deref() == Some(patterns) {
        return workspace
            .members
            .iter()
            .filter(|m| !m.name.is_empty())
            .map(|m| (m.path.clone(), m.name.clone(), m.version.clone()))
            .collect();
    }
    glob_members(&workspace.root_dir, patterns)
}

fn glob_members(root: &Path, patterns: &[String]) -> Vec<MemberRow> {
    let normalize = |p: &str| p.trim_start_matches("./").trim_end_matches('/').to_owned();
    let mut include = globset::GlobSetBuilder::new();
    let mut exclude = globset::GlobSetBuilder::new();
    let mut prefixes: BTreeSet<String> = BTreeSet::new();
    for pattern in patterns {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(p) => (true, normalize(p)),
            None => (false, normalize(pattern)),
        };
        let Ok(glob) = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
        else {
            continue;
        };
        if negated {
            exclude.add(glob);
            continue;
        }
        include.add(glob);
        let prefix: Vec<&str> = pattern
            .split('/')
            .take_while(|c| !c.contains(['*', '?', '[', '{']))
            .collect();
        prefixes.insert(prefix.join("/"));
    }
    let (Ok(include), Ok(exclude)) = (include.build(), exclude.build()) else {
        return Vec::new();
    };
    let mut found: BTreeMap<String, MemberRow> = BTreeMap::new();
    let mut stack: Vec<String> = prefixes.into_iter().collect();
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        if !rel.is_empty()
            && include.is_match(&rel)
            && !exclude.is_match(&rel)
            && !found.contains_key(&rel)
        {
            let manifest = std::fs::read(dir.join("package.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            if let Some(manifest) = manifest
                && let Some(name) = manifest
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
            {
                let version = manifest
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                found.insert(rel.clone(), (rel.clone(), name.to_owned(), version));
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "node_modules"
                || name.starts_with('.')
                || !entry.file_type().is_ok_and(|t| t.is_dir())
            {
                continue;
            }
            stack.push(if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            });
        }
    }
    found.into_values().collect()
}

/// The tag of a dependency specifier, or `None` when it does not parse (bun
/// `Dependency::parse` failing). Only the kinds whose parse can fail are fully parsed, so the
/// common registry ranges allocate nothing.
pub(crate) fn spec_tag(name: &str, literal: &str) -> Option<fnpm_types::SpecTag> {
    use fnpm_types::SpecTag;
    let trimmed = fnpm_types::spec::trim_literal(literal);
    let tag = fnpm_types::infer_tag(trimmed);
    match tag {
        SpecTag::Github | SpecTag::Tarball | SpecTag::Catalog | SpecTag::Folder => {
            fnpm_types::DependencySpec::parse_with_tag(name, trimmed, tag)
                .ok()
                .map(|_| tag)
        }
        _ => Some(tag),
    }
}

/// A hash map with xxh3 instead of SipHash: lockfile keys are not attacker-chosen in a way
/// that matters for a one-shot parse, and hashing every path and name is the hot part.
pub(crate) type FastMap<K, V> =
    std::collections::HashMap<K, V, xxhash_rust::xxh3::Xxh3DefaultBuilder>;
