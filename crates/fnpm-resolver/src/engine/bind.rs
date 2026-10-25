//! Binding one edge: effective spec (catalogs, overrides), reuse of the previous graph, and the
//! per-protocol resolution (bun `PackageManagerEnqueue.rs` `bind_row`, `asked_version`,
//! `get_or_put_resolved_package`).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use fnpm_registry::PackageManifest;
use fnpm_semver::{Range, Version};
use fnpm_types::name::{is_npm_package_name, is_safe_install_folder_name};
use fnpm_types::spec::{is_link_path, link_path_for_lockfile};
use fnpm_types::{
    DepEdge, DependencySpec, Package, PackageId, Resolution, TarballUri, WorkspaceSpec,
};

use super::{Asked, Completion, Engine, Kind, MState, PState, RState, Row, peers};
use crate::local::{self, LocalKind};
use crate::overrides::Owner;
use crate::pick::{self, AgeGate, PickError, Picked, Wanted};
use crate::source::{FetchRequest, clone_registry_error};
use crate::{Error, Result};

pub(crate) enum Step {
    Done,
    WaitManifest(String),
}

pub(crate) const LINK_PATH_RULE: &str =
    "only the root package.json, a workspace, or a top-level override may link to a path";

/// Joins `rel` onto `base` and normalises `.` / `..` without touching the filesystem.
pub(crate) fn normalize_join(base: &Path, rel: &str) -> PathBuf {
    let rel = if let Some(rest) = rel.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(rel),
        }
    } else {
        PathBuf::from(rel)
    };
    let joined = if rel.is_absolute() {
        rel
    } else {
        base.join(rel)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `abs` relative to `root`, `/`-separated (`.` for the root itself). Paths outside the root use
/// `..`; paths on another drive stay absolute.
pub(crate) fn relative_to(root: &Path, abs: &Path) -> String {
    let root_parts: Vec<_> = root.components().collect();
    let abs_parts: Vec<_> = abs.components().collect();
    let common = root_parts
        .iter()
        .zip(&abs_parts)
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 && abs.is_absolute() {
        return abs.to_string_lossy().replace('\\', "/");
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..root_parts.len() {
        parts.push("..".into());
    }
    for c in &abs_parts[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// bun `bin_target_escapes_package_dir`: absolute, a drive prefix, or `..` above the start.
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
    let mut depth: i32 = 0;
    for part in target.split(['/', '\\']) {
        match part {
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

/// A resolved commit must be a full SHA-1 or SHA-256 object name: it ends up in the lockfile, in
/// download URLs and in checkout paths, so a remote answering with anything else (`../x`, a
/// branch name, an empty line) is an error rather than something to interpolate.
pub(crate) fn checked_commit(sha: String, origin: &str) -> fnpm_registry::Result<String> {
    if matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(sha)
    } else {
        Err(fnpm_registry::Error::Git(format!(
            "{origin}: \"{}\" is not a commit",
            sha.escape_debug()
        )))
    }
}

/// What a candidate lookup can return: a package already in the new graph, or one in the
/// previous graph that is imported when chosen.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Cand {
    New(PackageId),
    Prev(PackageId),
}

impl Engine<'_> {
    pub(crate) fn owner(&self, pkg: PackageId) -> Owner<'_> {
        Owner {
            name: &self.graph.packages[pkg.index()].name,
            version: self.st[pkg.index()].version.as_ref(),
        }
    }

    /// The effective spec of an edge. `Ok(None)`: the edge stays unresolved without an error (a
    /// `catalog:` dependency outside the project).
    pub(crate) fn asked(&mut self, row: Row, edge: &DepEdge) -> Result<Option<Asked>> {
        let declared =
            self.parse_spec(&edge.name, &edge.literal)
                .map_err(|e| Error::InvalidSpec {
                    name: edge.name.clone(),
                    spec: edge.literal.clone(),
                    message: e.to_string(),
                })?;
        let owner_kind = self.st[row.pkg.index()].kind;
        let mut spec = declared.clone();
        let mut literal = edge.literal.clone();
        let mut replaced = false;
        let mut from_rule = false;
        if let DependencySpec::Catalog { .. } = &declared
            && owner_kind != Kind::Project
        {
            if !edge.behavior.is_peer() {
                return Ok(None);
            }
            spec = DependencySpec::Npm {
                name: edge.name.clone(),
                range: "*".into(),
                is_alias: false,
            };
            literal = "*".into();
            replaced = true;
        }
        let mut rule_name = edge.name.clone();
        if !replaced
            && !matches!(declared, DependencySpec::Workspace { .. })
            && !self.overrides.is_empty()
        {
            let owner = self.owner(row.pkg);
            if let Some(rule) = self.overrides.lookup(Some(owner), &edge.name, &declared) {
                let (target_name, value) = (rule.selector.target.name.clone(), rule.value.clone());
                spec = self
                    .parse_spec(&target_name, &value)
                    .map_err(|e| Error::InvalidSpec {
                        name: target_name.clone(),
                        spec: value.clone(),
                        message: e.to_string(),
                    })?;
                literal = value;
                rule_name = target_name;
                replaced = true;
                from_rule = true;
            }
        }
        if let DependencySpec::Catalog { catalog } = &spec {
            let catalog = catalog.clone();
            match crate::catalogs::lookup(&self.graph.catalogs, &catalog, &rule_name)
                .map(str::to_owned)
            {
                Some(entry) => {
                    spec = self
                        .parse_spec(&rule_name, &entry)
                        .map_err(|e| Error::InvalidSpec {
                            name: rule_name.clone(),
                            spec: entry.clone(),
                            message: e.to_string(),
                        })?;
                    literal = entry;
                    replaced = true;
                }
                None => {
                    return Err(Error::CatalogEntryMissing {
                        catalog,
                        name: edge.name.clone(),
                    });
                }
            }
        }
        let name = match &spec {
            DependencySpec::Npm { name, .. } | DependencySpec::DistTag { name, .. } => name.clone(),
            _ => edge.name.clone(),
        };
        Ok(Some(Asked {
            name,
            spec,
            literal,
            replaced,
            from_rule,
        }))
    }

    /// An error on `row`: tolerated (with a warning when it matters) for optional dependencies,
    /// peers and anything only reachable through optional dependencies; fatal otherwise.
    pub(crate) fn fail(&mut self, row: Row, edge: &DepEdge, err: Error) -> Result<Step> {
        let tolerated = edge.behavior.is_optional()
            || edge.behavior.is_peer()
            || self.st[row.pkg.index()].optional_only;
        if !tolerated {
            return Err(err);
        }
        if edge.behavior.is_peer() {
            let literal = &edge.literal;
            let name = &edge.name;
            let message = match &err {
                Error::TooRecent { .. } => {
                    let secs = self.gate.as_ref().map_or(0, |(ms, _)| ms / 1000);
                    format!(
                        "No version matching \"{literal}\" found for peer dependency \"{name}\" (blocked by minimum-release-age: {secs} seconds)"
                    )
                }
                Error::NoMatchingVersion { .. } | Error::DistTagNotFound { .. } => {
                    format!(
                        "No version matching \"{literal}\" found for peer dependency \"{name}\" (but package exists)"
                    )
                }
                other => format!("{other} (peer dependency \"{name}\")"),
            };
            self.warnings.push(message);
        } else if !matches!(
            err,
            Error::NoMatchingVersion { .. }
                | Error::DistTagNotFound { .. }
                | Error::TooRecent { .. }
        ) {
            self.warnings.push(format!(
                "{err}\n  error occurred while resolving optional dependency \"{}\"",
                edge.name
            ));
        }
        Ok(Step::Done)
    }

    pub(crate) fn bind(&mut self, row: Row) -> Result<Step> {
        let edge = self.edge(row).clone();
        if edge.target.is_some() {
            return Ok(Step::Done);
        }
        if edge.behavior.is_optional_peer() && !row.install_peer {
            self.deferred_peers.push(row);
            return Ok(Step::Done);
        }
        let asked = match self.asked(row, &edge) {
            Ok(Some(asked)) => asked,
            Ok(None) => return Ok(Step::Done),
            Err(e) => return self.fail(row, &edge, e),
        };
        if !is_safe_install_folder_name(&edge.name)
            || (asked.spec.tag().is_npm() && !is_npm_package_name(&asked.name))
        {
            let bad = if is_safe_install_folder_name(&edge.name) {
                &asked.name
            } else {
                &edge.name
            };
            let err = Error::InvalidSpec {
                name: edge.name.clone(),
                spec: edge.literal.clone(),
                message: format!("invalid dependency name \"{bad}\""),
            };
            return self.fail(row, &edge, err);
        }
        if edge.behavior.is_peer() && row.install_peer {
            if let Some(target) = self.reuse_previous_peer(row, &edge, &asked) {
                self.set_target(row, target);
                return Ok(Step::Done);
            }
            if edge.behavior.is_optional_peer() {
                return Ok(Step::Done);
            }
        }
        if !edge.behavior.is_peer()
            && let Some(target) = self.reuse_previous(row, &edge, &asked)
        {
            self.set_target(row, target);
            return Ok(Step::Done);
        }
        if edge.behavior.is_peer() && !row.install_peer {
            if let Some(target) = self.linked_workspace(&asked) {
                self.set_target(row, target);
            } else {
                self.deferred_peers.push(row);
            }
            return Ok(Step::Done);
        }
        let result = match asked.spec.clone() {
            DependencySpec::Npm { .. } | DependencySpec::DistTag { .. } => {
                self.resolve_npm(row, &edge, &asked)
            }
            DependencySpec::Workspace { spec } => self.resolve_workspace(row, &edge, &asked, &spec),
            DependencySpec::Folder { path } => self.resolve_folder(row, &edge, &asked, &path),
            DependencySpec::Symlink { path } => self.resolve_link(row, &edge, &asked, &path),
            DependencySpec::Tarball { .. } | DependencySpec::Git(_) | DependencySpec::Github(_) => {
                self.resolve_remote(row, &edge, &asked)
            }
            DependencySpec::Catalog { catalog } => Err(Error::CatalogEntryMissing {
                catalog,
                name: edge.name.clone(),
            }),
        };
        match result {
            Ok(step) => Ok(step),
            Err(e) => self.fail(row, &edge, e),
        }
    }

    // ---------------------------------------------------------------------------------------
    // Update scope
    // ---------------------------------------------------------------------------------------

    pub(crate) fn should_update(&self, row: Row, edge: &DepEdge, asked: &Asked) -> bool {
        if !self.update.active {
            return false;
        }
        if self.update.names.is_empty() {
            return self.st[row.pkg.index()].kind == Kind::Project;
        }
        self.update.names.contains(&edge.name) || self.update.names.contains(&asked.name)
    }

    /// `fnpm update --latest`: a direct npm dependency that keeps its declared spec moves to the
    /// `latest` tag.
    pub(crate) fn picks_latest(&self, row: Row, edge: &DepEdge, asked: &Asked) -> bool {
        self.update.active
            && self.update.latest
            && !asked.replaced
            && asked.spec.tag().is_npm()
            && self.st[row.pkg.index()].kind == Kind::Project
            && (self.update.names.is_empty()
                || self.update.names.contains(&edge.name)
                || self.update.names.contains(&asked.name))
    }

    // ---------------------------------------------------------------------------------------
    // Previous graph
    // ---------------------------------------------------------------------------------------

    /// The effective literal the previous graph's rules gave the same edge, to tell whether a
    /// dist-tag or a non-npm spec still means what it meant.
    fn prev_effective_literal(&self, prev_owner: &Package, edge: &DepEdge) -> Option<String> {
        let prev = self.prev?;
        let declared = DependencySpec::parse(&edge.name, &edge.literal).ok()?;
        let set = &self.prev_overrides;
        let version = super::package_version(prev_owner);
        let owner = Owner {
            name: &prev_owner.name,
            version: version.as_ref(),
        };
        let (mut literal, mut name) = (edge.literal.clone(), edge.name.clone());
        if !matches!(declared, DependencySpec::Workspace { .. })
            && let Some(rule) = set.lookup(Some(owner), &edge.name, &declared)
        {
            literal = rule.value.clone();
            name = rule.selector.target.name.clone();
        }
        if let Ok(DependencySpec::Catalog { catalog }) = DependencySpec::parse(&name, &literal) {
            return crate::catalogs::lookup(&prev.catalogs, &catalog, &name).map(str::to_owned);
        }
        Some(literal)
    }

    /// The previous graph's resolution of the same edge, if it still satisfies the effective
    /// spec (bun keeps lockfile resolutions of unchanged dependencies without any manifest).
    fn reuse_previous(&mut self, row: Row, edge: &DepEdge, asked: &Asked) -> Option<PackageId> {
        let prev = self.prev?;
        if self.requested_rows.contains_key(&(row.pkg, row.edge))
            || self.should_update(row, edge, asked)
        {
            return None;
        }
        let counterpart = self.st[row.pkg.index()].prev?;
        let prev_owner = prev.packages.get(counterpart.index())?;
        let prev_edge = prev_owner
            .dependencies
            .iter()
            .filter(|e| e.name == edge.name && e.behavior.group() == edge.behavior.group())
            .find(|e| e.literal == edge.literal)
            .or_else(|| {
                prev_owner
                    .dependencies
                    .iter()
                    .find(|e| e.name == edge.name && e.behavior.group() == edge.behavior.group())
            })?;
        let target = prev_edge.target?;
        let unchanged = prev_edge.literal == edge.literal
            && self
                .prev_effective_literal(prev_owner, prev_edge)
                .as_deref()
                == Some(asked.literal.as_str());
        if !self.prev_satisfies(target, asked, unchanged) {
            return None;
        }
        self.import_prev(target)
    }

    /// Loaded transitive peer bindings survive unrelated workspace changes, even when a
    /// foreign lockfile selected a version outside the range. A removed or changed project
    /// provider and explicit updates still run the normal peer pass.
    fn reuse_previous_peer(
        &mut self,
        row: Row,
        edge: &DepEdge,
        asked: &Asked,
    ) -> Option<PackageId> {
        if self.update.active
            || self.st[row.pkg.index()].kind == Kind::Project
            || self.changed_peer_providers.contains(&asked.name)
        {
            return None;
        }
        let prev = self.prev?;
        let owner = &prev.packages[self.st[row.pkg.index()].prev?.index()];
        let before = owner.dependencies.iter().find(|e| {
            e.name == edge.name && e.behavior == edge.behavior && e.literal == edge.literal
        })?;
        if self.prev_effective_literal(owner, before).as_deref() != Some(asked.literal.as_str()) {
            return None;
        }
        let target = before.target?;
        let package = &prev.packages[target.index()];
        if package.name != asked.name || !matches!(package.resolution, Resolution::Npm { .. }) {
            return None;
        }
        // A target formerly provided by a non-peer dependency is reusable only when that
        // dependency is still in the current graph. Peer-only locked copies have no such holder.
        if self.prev_non_peer_targets.contains(&target) && !self.imported.contains_key(&target) {
            return None;
        }
        self.import_prev(target)
    }

    /// Whether previous package `id` is what `asked` resolves to.
    fn prev_satisfies(&self, id: PackageId, asked: &Asked, unchanged: bool) -> bool {
        let Some(prev) = self.prev else { return false };
        let p = &prev.packages[id.index()];
        match (&asked.spec, &p.resolution) {
            (DependencySpec::Npm { range, .. }, Resolution::Npm { .. }) => {
                p.name == asked.name
                    && match (Range::parse(range), self.prev_versions[id.index()].as_ref()) {
                        (Ok(r), Some(v)) => r.satisfies(v),
                        _ => unchanged,
                    }
            }
            (DependencySpec::DistTag { .. }, Resolution::Npm { .. }) => unchanged && p.name == asked.name,
            (DependencySpec::Npm { range, .. }, Resolution::Workspace { path }) => {
                self.cfg.link_workspace_packages
                    && self.ws.member_by_path(path).is_some_and(|m| {
                        m.name == asked.name
                            && (range.is_empty()
                                || range == "*"
                                || matches!((Range::parse(range), Version::parse(&m.version)), (Ok(r), Ok(v)) if r.satisfies(&v)))
                    })
            }
            (DependencySpec::DistTag { .. }, Resolution::Workspace { path }) => {
                unchanged && self.ws.member_by_path(path).is_some_and(|m| m.name == asked.name)
            }
            (DependencySpec::Workspace { .. }, Resolution::Workspace { path }) => {
                unchanged && self.ws.member_by_path(path).is_some_and(|m| m.name == p.name)
            }
            (DependencySpec::Workspace { .. }, Resolution::Root) => unchanged,
            (DependencySpec::Tarball { uri: TarballUri::Remote(url) }, Resolution::RemoteTarball { url: u, .. }) => url == u,
            (DependencySpec::Git(spec), Resolution::Git { url, committish, .. }) => {
                &spec.url == url && &spec.committish.to_raw() == committish
            }
            (DependencySpec::Github(spec), Resolution::Github { owner, repo, committish, .. }) => {
                &spec.owner == owner && &spec.repo == repo && &spec.committish.to_raw() == committish
            }
            // Folders, links and local tarballs are read again: their contents can change.
            _ => false,
        }
    }

    /// Copies previous package `id` (with its edges unresolved; they are queued and reuse their
    /// previous targets in turn) into the new graph.
    pub(crate) fn import_prev(&mut self, id: PackageId) -> Option<PackageId> {
        if let Some(&new) = self.imported.get(&id) {
            return Some(new);
        }
        let prev = self.prev?;
        let p = &prev.packages[id.index()];
        match &p.resolution {
            Resolution::Root => return Some(PackageId::ROOT),
            Resolution::Workspace { path } => return self.graph.workspaces.get(path).copied(),
            Resolution::Folder { .. } | Resolution::Symlink { .. } => return None,
            _ => {}
        }
        let version = self.prev_versions[id.index()].clone();
        if let (Resolution::Npm { .. }, Some(v)) = (&p.resolution, &version)
            && !self.extensions_unchanged(&p.name, v)
        {
            return None;
        }
        if let Some(&existing) = self.by_key.get(&p.key()) {
            self.imported.insert(id, existing);
            return Some(existing);
        }
        let mut package = p.clone();
        for e in &mut package.dependencies {
            e.target = None;
        }
        let kind = match package.resolution {
            Resolution::Npm { .. } => Kind::Registry,
            _ => Kind::Remote,
        };
        let mut state = PState::new(kind, version);
        state.prev = Some(id);
        state.loaded = true;
        state.in_bundle = package.meta.bundled;
        let new = self.add_package(package, state);
        self.imported.insert(id, new);
        self.push_rows(new, false);
        Some(new)
    }

    /// The extension rules that match `name@version` are the same as when the previous graph
    /// was resolved, so its edges can be reused.
    fn extensions_unchanged(&self, name: &str, version: &Version) -> bool {
        let Some(prev) = self.prev else { return true };
        let matching = |map: &std::collections::BTreeMap<String, fnpm_types::PackageExtension>| {
            map.iter()
                .filter(|(key, _)| {
                    fnpm_types::overrides::parse_package_segment(key).is_ok_and(|s| {
                        s.name == name
                            && (s.range.is_empty()
                                || Range::parse(&s.range).is_ok_and(|r| {
                                    r.satisfies_with(
                                        version,
                                        fnpm_semver::SatisfyOptions {
                                            include_prerelease: true,
                                        },
                                    )
                                }))
                    })
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>()
        };
        matching(&prev.package_extensions) == matching(&self.graph.package_extensions)
    }

    /// Candidates named `name` from both graphs, highest version first (new before previous on
    /// a tie). Previous packages already imported are listed once, as new.
    pub(crate) fn candidates(&self, name: &str) -> Vec<(Cand, Option<Version>)> {
        let mut out: Vec<(Cand, Option<Version>)> = Vec::new();
        if let Some(ids) = self.by_name.get(name) {
            for &id in ids {
                if matches!(
                    self.graph.packages[id.index()].resolution,
                    Resolution::Npm { .. }
                ) {
                    out.push((Cand::New(id), self.st[id.index()].version.clone()));
                }
            }
        }
        if let Some(ids) = self.prev_by_name.get(name) {
            for &id in ids {
                if self.imported.contains_key(&id) {
                    continue;
                }
                if let Some(prev) = self.prev
                    && self.by_key.contains_key(&prev.packages[id.index()].key())
                {
                    continue;
                }
                out.push((Cand::Prev(id), self.prev_versions[id.index()].clone()));
            }
        }
        out.sort_by(|a, b| match (&a.1, &b.1) {
            (Some(x), Some(y)) => y.cmp(x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        out
    }

    pub(crate) fn cand_loaded(&self, c: Cand) -> bool {
        match c {
            Cand::Prev(_) => true,
            Cand::New(id) => self.st[id.index()].loaded,
        }
    }

    pub(crate) fn cand_pinned(&self, c: Cand) -> bool {
        match c {
            Cand::Prev(_) => false,
            Cand::New(id) => self.st[id.index()].pinned,
        }
    }

    /// Materialises a candidate in the new graph.
    pub(crate) fn take_cand(&mut self, c: Cand) -> Option<PackageId> {
        match c {
            Cand::New(id) => Some(id),
            Cand::Prev(id) => self.import_prev(id),
        }
    }

    /// The workspace member an npm range (or a same-name dist-tag) links to
    /// (`link-workspace-packages`, bun `linked_workspace_path`).
    pub(crate) fn linked_workspace(&self, asked: &Asked) -> Option<PackageId> {
        if !self.cfg.link_workspace_packages {
            return None;
        }
        let DependencySpec::Npm { range, .. } = &asked.spec else {
            return None;
        };
        let member = self.ws.member(&asked.name)?;
        let id = *self.graph.workspaces.get(&member.path)?;
        if range.is_empty() || range.trim() == "*" {
            return Some(id);
        }
        let version = self.st[id.index()].version.as_ref()?;
        Range::parse(range).ok()?.satisfies(version).then_some(id)
    }

    fn member_id_by_name(&self, name: &str) -> Option<PackageId> {
        let member = self.ws.member(name)?;
        self.graph.workspaces.get(&member.path).copied()
    }
}

/// A manifest as far as a row is concerned.
pub(crate) enum Avail {
    Ready(Arc<PackageManifest>),
    Wait,
    Failed(Arc<fnpm_registry::Error>),
}

impl Engine<'_> {
    pub(crate) fn manifest_for(&mut self, name: &str, revalidate: bool) -> Avail {
        if self.manifest_ordered.insert(name.to_owned()) {
            self.manifest_order.push(name.to_owned());
        }
        match self.manifest_state.get(name) {
            Some(MState::Ready(m)) => Avail::Ready(m.clone()),
            Some(MState::Failed(e)) => Avail::Failed(e.clone()),
            Some(MState::Pending) => Avail::Wait,
            None => {
                self.request_manifest(name, revalidate, false);
                match self.manifest_state.get(name) {
                    Some(MState::Pending) => Avail::Wait,
                    _ => Avail::Failed(Arc::new(fnpm_registry::Error::NotFound(name.to_owned()))),
                }
            }
        }
    }

    pub(crate) fn age_gate(&self) -> Option<AgeGate<'_>> {
        self.gate.as_ref().map(|(ms, excludes)| AgeGate {
            min_age_ms: *ms,
            now_ms: self.now_ms,
            excludes,
        })
    }

    /// The highest previous (lockfile) version that `range` accepts.
    fn locked_version(&self, name: &str, range: Option<&Range>) -> Option<Version> {
        let prev = self.prev?;
        let ids = self.prev_by_name.get(name)?;
        ids.iter()
            .filter_map(|id| self.prev_versions[id.index()].clone().map(|v| (id, v)))
            .find(|(id, v)| {
                matches!(prev.packages[id.index()].resolution, Resolution::Npm { .. })
                    && range.is_none_or(|r| r.satisfies(v))
            })
            .map(|(_, v)| v)
    }

    /// `patchedDependencies` versions of `name` that `range` allows, highest first.
    fn patched_versions(&self, name: &str) -> Vec<Version> {
        let mut out: Vec<Version> = self
            .graph
            .patched_dependencies
            .keys()
            .filter_map(|key| {
                let (n, v) = fnpm_types::name::split_name_and_maybe_version(key);
                (n == name)
                    .then(|| v.and_then(|v| Version::parse(v).ok()))
                    .flatten()
            })
            .collect();
        out.sort_by(|a, b| b.cmp(a));
        out
    }

    pub(crate) fn resolve_npm(&mut self, row: Row, edge: &DepEdge, asked: &Asked) -> Result<Step> {
        let name = asked.name.clone();
        let is_peer = edge.behavior.is_peer();
        let latest = self.picks_latest(row, edge, asked);
        let should_update = self.should_update(row, edge, asked);
        let range: Option<Range> = match &asked.spec {
            DependencySpec::Npm { range, .. } => {
                Some(Range::parse(range).map_err(|e| Error::InvalidSpec {
                    name: edge.name.clone(),
                    spec: asked.literal.clone(),
                    message: e.to_string(),
                })?)
            }
            _ => None,
        };
        let (tag, range_text) = match &asked.spec {
            DependencySpec::DistTag { tag, .. } => (Some(tag.clone()), String::new()),
            DependencySpec::Npm { range, .. } => (None, range.clone()),
            _ => (None, String::new()),
        };

        if row.install_peer
            && is_peer
            && !latest
            && let Some(target) = self.settle_peer(row, edge, asked, range.as_ref())
        {
            self.set_target(row, target);
            return Ok(Step::Done);
        }
        if let Some(target) = self.linked_workspace(asked) {
            self.set_target(row, target);
            return Ok(Step::Done);
        }
        if row.install_peer && is_peer {
            if self.is_peer_provided_locally(row, &edge.name) {
                return Ok(Step::Done);
            }
            if let Some(target) = self.alias_of_every_dependent(row, &edge.name, range.as_ref()) {
                self.set_target(row, target);
                return Ok(Step::Done);
            }
            if !self.cfg.auto_install_peers {
                return Ok(Step::Done);
            }
        }

        // A version that bun.lock (the previous graph) already has, or that some dependency asks
        // for exactly, is shared without asking the registry (bun `find_package_id`).
        if !should_update
            && !latest
            && self.patched_versions(&name).is_empty()
            && let Some(r) = &range
        {
            let first = self
                .candidates(&name)
                .into_iter()
                .find(|(_, v)| v.as_ref().is_some_and(|v| r.satisfies(v)));
            if let Some((c, _)) = first
                && (self.cand_loaded(c) || self.cand_pinned(c))
                && let Some(target) = self.take_cand(c)
            {
                self.set_target(row, target);
                return Ok(Step::Done);
            }
        }

        let manifest = match self.manifest_for(&name, should_update) {
            Avail::Ready(m) => m,
            Avail::Wait => return Ok(Step::WaitManifest(name)),
            Avail::Failed(e) => {
                if tag.is_some()
                    && self.cfg.link_workspace_packages
                    && let Some(id) = self.member_id_by_name(&name)
                {
                    self.set_target(row, id);
                    return Ok(Step::Done);
                }
                if matches!(*e, fnpm_registry::Error::NotFound(_)) {
                    return Err(Error::NotFound(name));
                }
                return Err(Error::Registry(clone_registry_error(&e)));
            }
        };
        let gate_data = self.gate.clone();
        let gate = gate_data.as_ref().map(|(ms, excludes)| AgeGate {
            min_age_ms: *ms,
            now_ms: self.now_ms,
            excludes,
        });
        let peer_ranges = if row.install_peer && is_peer && !asked.replaced {
            self.ranges_of_every_asker(row, &edge.name)
        } else {
            Vec::new()
        };
        let picked: Picked<'_> = if latest {
            pick::find_by_dist_tag_with_filter(&manifest, "latest", gate.as_ref())
        } else if let Some(shared) =
            peers::version_every_peer_accepts(&manifest, &peer_ranges, gate.as_ref())
        {
            shared
        } else {
            if row.install_peer && is_peer {
                let found = self
                    .installed_peer_target(row, &edge.name, range.as_ref())
                    .or_else(|| self.installed_version_in_any_range(row, &edge.name, &peer_ranges));
                if let Some(target) = found {
                    self.set_target(row, target);
                    return Ok(Step::Done);
                }
            }
            let wanted = match (&range, &tag) {
                (Some(r), _) => Wanted::Range {
                    range: r,
                    literal: &range_text,
                },
                (None, Some(t)) => Wanted::Tag {
                    tag: t,
                    literal: &asked.literal,
                },
                _ => unreachable!("npm specs are ranges or tags"),
            };
            pick::find_for_dependency(&manifest, wanted, gate.as_ref())
        };
        let mut chosen = match picked {
            Picked::Found(v) | Picked::FoundWithFilter { result: v, .. } => Some(v),
            Picked::Err(err) => {
                if row.install_peer
                    && is_peer
                    && let Some(target) = self.highest_peer_candidate(&edge.name, range.is_some())
                {
                    self.set_target(row, target);
                    return Ok(Step::Done);
                }
                match err {
                    PickError::TooRecent | PickError::AllVersionsTooRecent => {
                        match self
                            .locked_version(&name, range.as_ref())
                            .and_then(|v| manifest.find(&v))
                        {
                            Some(v) => Some(v),
                            None => {
                                let secs = self.gate.as_ref().map_or(0, |(ms, _)| ms / 1000);
                                let message = match &tag {
                                    Some(t) => format!(
                                        "Package \"{name}\" with tag \"{t}\" not found (all versions blocked by minimum-release-age: {secs} seconds)"
                                    ),
                                    None => format!(
                                        "No version matching \"{}\" found for specifier \"{name}\" (blocked by minimum-release-age: {secs} seconds)",
                                        asked.literal
                                    ),
                                };
                                return Err(Error::TooRecent {
                                    name,
                                    spec: asked.literal.clone(),
                                    message,
                                });
                            }
                        }
                    }
                    PickError::NotFound => None,
                }
            }
        };
        if chosen.is_none() {
            // A workspace is only used for a dist-tag if the registry can't satisfy it.
            if tag.is_some()
                && let Some(id) = self
                    .member_id_by_name(&name)
                    .filter(|_| self.cfg.link_workspace_packages)
            {
                self.set_target(row, id);
                return Ok(Step::Done);
            }
            if (latest || tag.as_deref() == Some("latest"))
                && let Some(instead) = pick::find_any_version(&manifest, gate.as_ref()).ok()
            {
                self.warnings.push(format!(
                    "the \"latest\" tag of \"{name}\" names no published version, using {}",
                    instead.version
                ));
                chosen = Some(instead);
            }
        }
        let Some(mut chosen) = chosen else {
            return Err(match tag {
                Some(tag) => Error::DistTagNotFound { name, tag },
                None => Error::NoMatchingVersion {
                    name,
                    spec: asked.literal.clone(),
                },
            });
        };
        if latest {
            // `--latest` never moves below the version in the lockfile, nor below the tag.
            if let Some(locked) = self.locked_version(&name, range.as_ref())
                && chosen.version.cmp(&locked) == std::cmp::Ordering::Less
                && let Some(kept) = manifest.find(&locked)
            {
                chosen = kept;
            }
            if let Some(t) = &tag
                && let Some(tagged) = manifest.dist_tag(t)
                && tagged.version.cmp(&chosen.version) == std::cmp::Ordering::Greater
            {
                chosen = tagged;
            }
        }
        // A patched version stays as long as the range allows it.
        let mut patched = false;
        if !latest && !is_peer && !self.graph.patched_dependencies.is_empty() {
            for v in self.patched_versions(&name) {
                if range.as_ref().is_none_or(|r| r.satisfies(&v))
                    && let Some(found) = manifest.find(&v)
                {
                    if should_update
                        && found.version.cmp(&chosen.version) == std::cmp::Ordering::Less
                    {
                        self.kept_patched.push(format!(
                            "kept {name}@{} (patched, v{} available)",
                            found.version, chosen.version
                        ));
                    }
                    chosen = found;
                    patched = true;
                    break;
                }
            }
        }
        let reusable = !should_update && !patched && !latest;
        let chosen_version = chosen.version.clone();
        for (c, v) in self.candidates(&name) {
            let Some(v) = v else { continue };
            let same = v.cmp(&chosen_version) == std::cmp::Ordering::Equal;
            let dedupe = reusable
                && range.as_ref().is_some_and(|r| r.satisfies(&v))
                && (self.cand_loaded(c) || self.cand_pinned(c) || v.major >= chosen_version.major);
            if (same || dedupe)
                && let Some(target) = self.take_cand(c)
            {
                if let Some(r) = &range
                    && r.as_exact().is_some()
                {
                    self.st[target.index()].pinned = true;
                }
                self.set_target(row, target);
                return Ok(Step::Done);
            }
        }
        let id = self.create_npm_package(row, edge, &name, chosen);
        if let Some(r) = &range
            && r.as_exact().is_some()
        {
            self.st[id.index()].pinned = true;
        }
        Ok(Step::Done)
    }

    /// Adds a registry package for `vm` and queues its edges.
    pub(crate) fn create_npm_package(
        &mut self,
        row: Row,
        edge: &DepEdge,
        name: &str,
        vm: &fnpm_registry::VersionManifest,
    ) -> PackageId {
        let version = vm.version.to_string();
        let default_url = self.manifests.default_tarball_url(name, &version);
        let tarball = (!vm.dist.tarball.is_empty() && vm.dist.tarball != default_url)
            .then(|| vm.dist.tarball.clone());
        let mut package = local::package_from_manifest(name, vm, tarball);
        if !self.extension_rules.is_empty() {
            crate::extensions::apply(
                &self.extension_rules,
                name,
                &vm.version,
                &mut package.dependencies,
            );
        }
        let owner = &self.st[row.pkg.index()];
        let mut state = PState::new(Kind::Registry, Some(vm.version.clone()));
        state.optional_only = owner.optional_only || edge.behavior.is_optional();
        state.in_bundle =
            owner.in_bundle || (edge.behavior.is_bundled() && owner.kind != Kind::Project);
        state.prev = self.prev_by_key.get(&package.key()).copied();
        package.meta.bundled = state.in_bundle;
        let id = self.add_package(package, state);
        self.set_target(row, id);
        self.push_rows(id, false);
        id
    }

    // ---------------------------------------------------------------------------------------
    // workspace:, file:, link:
    // ---------------------------------------------------------------------------------------

    fn resolve_workspace(
        &mut self,
        row: Row,
        edge: &DepEdge,
        _asked: &Asked,
        spec: &WorkspaceSpec,
    ) -> Result<Step> {
        let owner = self.st[row.pkg.index()].clone();
        let not_found = || Error::WorkspaceNotFound {
            name: edge.name.clone(),
        };
        // A registry (or remote) package naming a workspace links the member by name.
        if owner.kind != Kind::Project
            && !owner.local_declarer
            && !self.overrides.has_flat(&edge.name)
        {
            let id = self.member_id_by_name(&edge.name).ok_or_else(not_found)?;
            self.set_target(row, id);
            return Ok(Step::Done);
        }
        let (name, range): (String, Option<String>) = match spec {
            WorkspaceSpec::Path(p) => {
                let dir = owner
                    .dir
                    .clone()
                    .unwrap_or_else(|| self.root_dir().to_path_buf());
                let rel = relative_to(self.root_dir(), &normalize_join(&dir, p));
                if rel == "." {
                    self.set_target(row, PackageId::ROOT);
                    return Ok(Step::Done);
                }
                let id = self
                    .graph
                    .workspaces
                    .get(&rel)
                    .copied()
                    .ok_or_else(not_found)?;
                self.set_target(row, id);
                return Ok(Step::Done);
            }
            WorkspaceSpec::Any | WorkspaceSpec::Caret | WorkspaceSpec::Tilde => {
                (edge.name.clone(), None)
            }
            WorkspaceSpec::Range(r) => {
                let input = r.as_str();
                match input.rfind('@') {
                    Some(at) if at > 0 => {
                        (input[..at].to_owned(), Some(input[at + 1..].to_owned()))
                    }
                    _ => (edge.name.clone(), Some(input.to_owned())),
                }
            }
        };
        let (id, version) = if let Some(member) = self.ws.member(&name) {
            let id = *self
                .graph
                .workspaces
                .get(&member.path)
                .ok_or_else(not_found)?;
            (id, Version::parse(&member.version).ok())
        } else if self.graph.root().name == name && !name.is_empty() {
            (
                PackageId::ROOT,
                Version::parse(&self.graph.root().version).ok(),
            )
        } else {
            return Err(not_found());
        };
        if let Some(range_text) = range {
            let trimmed = range_text.trim();
            let names_no_version = trimmed.is_empty() || matches!(trimmed, "*" | "^" | "~");
            if !names_no_version {
                let ok = match (Range::parse(trimmed), &version) {
                    (Ok(r), Some(v)) => r.satisfies(v),
                    // A tag-only literal (`foo@beta`) links the member whatever its version.
                    (Err(_), _) => true,
                    (Ok(r), None) => r.is_any(),
                };
                if !ok {
                    return Err(Error::InvalidSpec {
                        name: edge.name.clone(),
                        spec: edge.literal.clone(),
                        message: format!(
                            "No matching version for workspace dependency \"{}\". Version: \"{}\"",
                            edge.name, edge.literal
                        ),
                    });
                }
            }
        }
        self.set_target(row, id);
        Ok(Step::Done)
    }

    /// Whether `row` may refer to a path in the project (bun `is_trusted_folder_dependency`).
    fn trusted_path(&self, row: Row, edge: &DepEdge) -> bool {
        let owner = &self.st[row.pkg.index()];
        owner.kind == Kind::Project || owner.local_declarer || self.overrides.has_flat(&edge.name)
    }

    fn resolve_folder(
        &mut self,
        row: Row,
        edge: &DepEdge,
        asked: &Asked,
        path: &str,
    ) -> Result<Step> {
        let owner = self.st[row.pkg.index()].clone();
        let trusted = self.trusted_path(row, edge);
        let from_rule =
            asked.from_rule && !edge.behavior.is_bundled() && (!path_escapes(path) || trusted);
        if from_rule || owner.kind == Kind::Project {
            // The root has one `node_modules/<name>`, and the workspace wins over the folder.
            if row.pkg == PackageId::ROOT
                && let Some(id) = self.member_id_by_name(&edge.name)
            {
                self.set_target(row, id);
                return Ok(Step::Done);
            }
            let base = if from_rule {
                self.root_dir().to_path_buf()
            } else {
                owner
                    .dir
                    .clone()
                    .unwrap_or_else(|| self.root_dir().to_path_buf())
            };
            let abs = normalize_join(&base, path);
            let rel = relative_to(self.root_dir(), &abs);
            let resolution = Resolution::Folder { path: rel.clone() };
            let pj = self
                .fetcher
                .read_package_json(&abs)
                .map_err(Error::Registry)?
                .ok_or_else(|| Error::MissingPackageJson {
                    name: edge.name.clone(),
                    message: format!(
                        "package.json not found for \"file:{path}\" dependency \"{}\"",
                        edge.name
                    ),
                })?;
            let mut package = local::package_from_json(&pj, &edge.name, resolution);
            if let Some(&existing) = self.by_key.get(&package.key()) {
                self.set_target(row, existing);
                return Ok(Step::Done);
            }
            let (edges, warnings) = local::package_json_edges(&pj, LocalKind::Folder)?;
            self.warnings.extend(warnings);
            package.dependencies = edges;
            let mut state = PState::new(Kind::Folder, Version::parse(&package.version).ok());
            state.dir = Some(abs);
            state.local_declarer = true;
            state.optional_only = owner.optional_only || edge.behavior.is_optional();
            state.prev = self.prev_by_key.get(&package.key()).copied();
            let id = self.add_package(package, state);
            self.set_target(row, id);
            self.push_rows(id, false);
            return Ok(Step::Done);
        }
        // A folder declared by a folder of the project: one folder per path, relative to the
        // project; like bun, its own dependencies are not resolved.
        if owner.kind == Kind::Folder
            && let Some(dir) = owner.dir.clone()
        {
            let rel = relative_to(self.root_dir(), &normalize_join(&dir, path));
            let package = Package::new(edge.name.clone(), "", Resolution::Folder { path: rel });
            if let Some(&existing) = self.by_key.get(&package.key()) {
                self.set_target(row, existing);
                return Ok(Step::Done);
            }
            let mut state = PState::new(Kind::Folder, None);
            state.optional_only = owner.optional_only || edge.behavior.is_optional();
            let id = self.add_package(package, state);
            self.set_target(row, id);
            return Ok(Step::Done);
        }
        // A folder of a published package: kept as written, its dependencies are not resolved.
        if !edge.behavior.is_bundled() && path_escapes(path) && !trusted {
            return Err(Error::MissingPackageJson {
                name: edge.name.clone(),
                message: format!(
                    "package.json not found for \"file:{path}\" dependency \"{}\"",
                    edge.name
                ),
            });
        }
        let package = Package::new(
            edge.name.clone(),
            "",
            Resolution::Folder {
                path: path.to_owned(),
            },
        );
        if let Some(&existing) = self.by_key.get(&package.key()) {
            self.set_target(row, existing);
            return Ok(Step::Done);
        }
        let mut state = PState::new(Kind::Folder, None);
        state.optional_only = owner.optional_only || edge.behavior.is_optional();
        state.in_bundle = owner.in_bundle || edge.behavior.is_bundled();
        let id = self.add_package(package, state);
        self.set_target(row, id);
        Ok(Step::Done)
    }

    fn resolve_link(
        &mut self,
        row: Row,
        edge: &DepEdge,
        asked: &Asked,
        path: &str,
    ) -> Result<Step> {
        let owner = self.st[row.pkg.index()].clone();
        let (abs, stored) = if is_link_path(path) {
            if !self.trusted_path(row, edge) && !asked.from_rule {
                let declarer = &self.graph.packages[row.pkg.index()];
                return Err(Error::InvalidSpec {
                    name: edge.name.clone(),
                    spec: edge.literal.clone(),
                    message: format!(
                        "refusing to resolve \"{}@{}\" declared by {}@{}: {LINK_PATH_RULE}",
                        edge.name, edge.literal, declarer.name, declarer.resolution
                    ),
                });
            }
            let base = if asked.from_rule {
                self.root_dir().to_path_buf()
            } else {
                owner
                    .dir
                    .clone()
                    .unwrap_or_else(|| self.root_dir().to_path_buf())
            };
            let abs = normalize_join(&base, path);
            let rel = relative_to(self.root_dir(), &abs);
            if !abs.exists() {
                return Err(Error::MissingPackageJson {
                    name: edge.name.clone(),
                    message: format!(
                        "directory \"{path}\" not found for linked dependency \"{}\"",
                        edge.name
                    ),
                });
            }
            (abs, link_path_for_lockfile(&rel).into_owned())
        } else {
            let abs = self.cfg.global_dir.join("node_modules").join(path);
            if !abs.exists() {
                return Err(Error::MissingPackageJson {
                    name: edge.name.clone(),
                    message: format!(
                        "Package \"{}\" is not linked\n\nTo install a linked package:\n   fnpm link my-pkg-name-from-package-json\n\nTip: the package name is from package.json, which can differ from the folder name.",
                        edge.name
                    ),
                });
            }
            (abs, path.to_owned())
        };
        let resolution = Resolution::Symlink { path: stored };
        let pj = self
            .fetcher
            .read_package_json(&abs)
            .map_err(Error::Registry)?;
        let package = match &pj {
            Some(pj) => local::package_from_json(pj, &edge.name, resolution),
            None => Package::new(edge.name.clone(), "", resolution),
        };
        if let Some(&existing) = self.by_key.get(&package.key()) {
            self.set_target(row, existing);
            return Ok(Step::Done);
        }
        let mut state = PState::new(Kind::Link, Version::parse(&package.version).ok());
        state.dir = Some(abs);
        state.optional_only = owner.optional_only || edge.behavior.is_optional();
        let id = self.add_package(package, state);
        self.set_target(row, id);
        Ok(Step::Done)
    }
}

/// What to fetch for a remote spec.
enum RemoteKind {
    Tarball {
        url: String,
    },
    LocalTarball {
        rel: String,
        abs: PathBuf,
    },
    Git {
        url: String,
        committish: fnpm_types::Committish,
    },
    Github {
        owner: String,
        repo: String,
        committish: fnpm_types::Committish,
    },
}

impl Engine<'_> {
    fn remote_kind(&self, row: Row, edge: &DepEdge, asked: &Asked) -> Result<RemoteKind> {
        Ok(match &asked.spec {
            DependencySpec::Tarball {
                uri: TarballUri::Remote(url),
            } => RemoteKind::Tarball { url: url.clone() },
            DependencySpec::Tarball {
                uri: TarballUri::Local(path),
            } => {
                let owner = &self.st[row.pkg.index()];
                let declarer = &self.graph.packages[row.pkg.index()];
                let root_declares_same = self
                    .graph
                    .root()
                    .dependencies
                    .iter()
                    .any(|e| e.name == edge.name && e.literal == edge.literal);
                let may_name = asked.replaced
                    || owner.kind == Kind::Project
                    || owner.local_declarer
                    || root_declares_same
                    || matches!(declarer.resolution, Resolution::LocalTarball { .. });
                if !may_name {
                    return Err(Error::InvalidSpec {
                        name: edge.name.clone(),
                        spec: edge.literal.clone(),
                        message: format!(
                            "refusing to resolve \"{}@{}\" declared by {}@{}: local tarballs are only allowed in this project's package.json files",
                            edge.name, edge.literal, declarer.name, declarer.resolution
                        ),
                    });
                }
                let base = if asked.replaced || root_declares_same || owner.dir.is_none() {
                    self.root_dir().to_path_buf()
                } else {
                    owner
                        .dir
                        .clone()
                        .unwrap_or_else(|| self.root_dir().to_path_buf())
                };
                let abs = normalize_join(&base, path);
                RemoteKind::LocalTarball {
                    rel: relative_to(self.root_dir(), &abs),
                    abs,
                }
            }
            DependencySpec::Git(spec) => RemoteKind::Git {
                url: spec.url.clone(),
                committish: spec.committish.clone(),
            },
            DependencySpec::Github(spec) => RemoteKind::Github {
                owner: spec.owner.clone(),
                repo: spec.repo.clone(),
                committish: spec.committish.clone(),
            },
            _ => unreachable!("remote specs only"),
        })
    }

    fn remote_key(kind: &RemoteKind) -> String {
        match kind {
            RemoteKind::Tarball { url } => format!("tarball:{url}"),
            RemoteKind::LocalTarball { rel, .. } => format!("file:{rel}"),
            RemoteKind::Git { url, committish } => format!("git:{url}#{}", committish.to_raw()),
            RemoteKind::Github {
                owner,
                repo,
                committish,
            } => format!("github:{owner}/{repo}#{}", committish.to_raw()),
        }
    }

    /// A package of the previous graph with this git / tarball source (bun `locked_repository`).
    fn locked_remote(&self, kind: &RemoteKind) -> Option<PackageId> {
        let prev = self.prev?;
        prev.packages
            .iter()
            .position(|p| match (kind, &p.resolution) {
                (RemoteKind::Tarball { url }, Resolution::RemoteTarball { url: u, .. }) => url == u,
                (
                    RemoteKind::Git { url, committish },
                    Resolution::Git {
                        url: u,
                        committish: c,
                        ..
                    },
                ) => url == u && &committish.to_raw() == c,
                (
                    RemoteKind::Github {
                        owner,
                        repo,
                        committish,
                    },
                    Resolution::Github {
                        owner: o,
                        repo: r,
                        committish: c,
                        ..
                    },
                ) => owner == o && repo == r && &committish.to_raw() == c,
                _ => false,
            })
            .map(|i| PackageId(i as u32))
    }

    /// Git, GitHub and tarball dependencies: fetched in the background; the package takes its
    /// turn once the queue is empty (bun `append_next_remote_package`).
    pub(crate) fn resolve_remote(
        &mut self,
        row: Row,
        edge: &DepEdge,
        asked: &Asked,
    ) -> Result<Step> {
        let kind = self.remote_kind(row, edge, asked)?;
        let key = Self::remote_key(&kind);
        if !self.should_update(row, edge, asked)
            && let Some(prev_id) = self.locked_remote(&kind)
            && let Some(target) = self.import_prev(prev_id)
        {
            self.set_target(row, target);
            return Ok(Step::Done);
        }
        match self.remote.get(&key) {
            Some(RState::Done(id)) => {
                let id = *id;
                self.set_target(row, id);
                return Ok(Step::Done);
            }
            Some(RState::Failed(e)) => return Err(Error::Registry(clone_registry_error(e))),
            Some(_) => {
                self.remote_waiters.entry(key).or_default().push(row);
                return Ok(Step::Done);
            }
            None => {}
        }
        self.remote.insert(key.clone(), RState::Pending);
        self.remote_turns.push_back(key.clone());
        self.remote_waiters
            .entry(key.clone())
            .or_default()
            .push(row);
        let fetcher = self.fetcher.clone();
        let tx = self.done_tx.clone();
        let name = edge.name.clone();
        self.in_flight += 1;
        tokio::spawn(async move {
            let result = async {
                let (resolution, local_path, subdir) = match kind {
                    RemoteKind::Tarball { url } => (
                        Resolution::RemoteTarball {
                            url,
                            integrity: None,
                        },
                        None,
                        None,
                    ),
                    RemoteKind::LocalTarball { rel, abs } => (
                        Resolution::LocalTarball {
                            path: rel,
                            integrity: None,
                        },
                        Some(abs),
                        None,
                    ),
                    RemoteKind::Git { url, committish } => {
                        let sha =
                            checked_commit(fetcher.resolve_git(&url, &committish).await?, &url)?;
                        let subdir = committish.path.clone();
                        (
                            Resolution::Git {
                                url,
                                committish: committish.to_raw(),
                                resolved_commit: sha,
                                integrity: None,
                            },
                            None,
                            subdir,
                        )
                    }
                    RemoteKind::Github {
                        owner,
                        repo,
                        committish,
                    } => {
                        let raw = committish.to_raw();
                        let sha = fetcher
                            .resolve_github(
                                &owner,
                                &repo,
                                committish.reference.as_deref().unwrap_or(""),
                            )
                            .await?;
                        let sha = checked_commit(sha, &format!("github:{owner}/{repo}"))?;
                        (
                            Resolution::Github {
                                owner,
                                repo,
                                committish: raw,
                                resolved_commit: sha,
                                integrity: None,
                            },
                            None,
                            None,
                        )
                    }
                };
                let fetched = fetcher
                    .fetch(FetchRequest {
                        name,
                        resolution: resolution.clone(),
                        local_path,
                        subdir,
                    })
                    .await?;
                Ok(Box::new((resolution, fetched)))
            }
            .await;
            let _ = tx.send(Completion::Remote(key, result));
        });
        Ok(Step::Done)
    }

    /// The turn of a git / tarball package: create it and bind every dependency waiting for it.
    pub(crate) fn remote_turn(&mut self, key: &str) -> Result<()> {
        let waiters = self.remote_waiters.remove(key).unwrap_or_default();
        match self.remote.remove(key) {
            Some(RState::Ready(ready)) => {
                let (mut resolution, fetched) = *ready;
                if let Some(integrity) = fetched.integrity {
                    resolution.set_integrity(integrity);
                }
                let fallback = waiters
                    .first()
                    .map(|w| self.edge(*w).name.clone())
                    .unwrap_or_default();
                let mut package =
                    local::package_from_json(&fetched.manifest, &fallback, resolution);
                let id = match self.by_key.get(&package.key()) {
                    Some(&existing) => existing,
                    None => {
                        let (edges, warnings) =
                            local::package_json_edges(&fetched.manifest, LocalKind::Remote)?;
                        self.warnings.extend(warnings);
                        package.dependencies = edges;
                        let optional_only = waiters.iter().all(|w| {
                            self.edge(*w).behavior.is_optional()
                                || self.st[w.pkg.index()].optional_only
                        });
                        let mut state =
                            PState::new(Kind::Remote, Version::parse(&package.version).ok());
                        state.optional_only = optional_only;
                        state.prev = self.prev_by_key.get(&package.key()).copied();
                        let id = self.add_package(package, state);
                        self.push_rows(id, false);
                        id
                    }
                };
                self.remote.insert(key.to_owned(), RState::Done(id));
                for w in waiters {
                    self.set_target(w, id);
                }
                Ok(())
            }
            Some(RState::Failed(e)) => {
                self.remote
                    .insert(key.to_owned(), RState::Failed(e.clone()));
                for w in waiters {
                    let edge = self.edge(w).clone();
                    // `fnpm update`: a source that cannot be read keeps the package of the lockfile.
                    if self.update.active
                        && let Some(asked) = self.asked(w, &edge).ok().flatten()
                        && let Ok(kind) = self.remote_kind(w, &edge, &asked)
                        && let Some(prev_id) = self.locked_remote(&kind)
                        && let Some(target) = self.import_prev(prev_id)
                    {
                        let p = &self.graph.packages[target.index()];
                        self.warnings.push(format!(
                            "{}@{} was not checked for updates",
                            p.name, p.resolution
                        ));
                        self.set_target(w, target);
                        continue;
                    }
                    self.fail(w, &edge, Error::Registry(clone_registry_error(&e)))?;
                }
                Ok(())
            }
            Some(RState::Done(id)) => {
                self.remote.insert(key.to_owned(), RState::Done(id));
                for w in waiters {
                    self.set_target(w, id);
                }
                Ok(())
            }
            Some(RState::Pending) | None => {
                let e = fnpm_registry::Error::Io(std::io::Error::other(format!(
                    "fetching {key} did not finish"
                )));
                for w in waiters {
                    let edge = self.edge(w).clone();
                    self.fail(w, &edge, Error::Registry(clone_registry_error(&e)))?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers() {
        let root = Path::new("/p");
        assert_eq!(normalize_join(root, "./a/../b"), PathBuf::from("/p/b"));
        assert_eq!(normalize_join(root, "/abs/x"), PathBuf::from("/abs/x"));
        assert_eq!(relative_to(root, Path::new("/p/a/b")), "a/b");
        assert_eq!(relative_to(root, Path::new("/p")), ".");
        assert_eq!(relative_to(Path::new("/p/q"), Path::new("/p/r")), "../r");
        assert!(path_escapes("../x"));
        assert!(path_escapes("/etc"));
        assert!(path_escapes("C:foo"));
        assert!(path_escapes("a/../../b"));
        assert!(!path_escapes("./a/../b"));
        assert!(!path_escapes("lib"));
    }

    #[test]
    fn commits_must_be_object_names() {
        assert!(checked_commit("a".repeat(40), "o").is_ok());
        assert!(checked_commit("B".repeat(64), "o").is_ok());
        for bad in [
            "",
            "main",
            "../../etc",
            &"a".repeat(39),
            &format!("{}/", "a".repeat(39)),
        ] {
            assert!(checked_commit(bad.to_owned(), "o").is_err(), "{bad}");
        }
    }
}
