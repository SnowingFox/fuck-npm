//! `package-lock.json` / `npm-shrinkwrap.json` (a port of bun `install/migration.rs`
//! `migrate_npm_lockfile` and `install/migration/npm_lock.rs`), and the lockfileVersion 3
//! writer for `fnpm-lock export --to npm`.
//!
//! Reading follows bun exactly: only lockfileVersion 2-4 is migrated (v1 lockfiles only have
//! the legacy `dependencies` tree, which bun does not read either); each `packages` entry is
//! keyed by its install path and every dependency is bound by node's lookup from that path
//! (`find_target`). `link: true` entries point at their target (workspaces and `file:`
//! folders), `bundleDependencies` mark edges bundled, `os` / `cpu` / `libc` become platform
//! constraints, and `resolved` URLs become registry, git, GitHub, tarball or folder
//! resolutions.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use fnpm_types::{
    Behavior, Bin, CpuSet, DepEdge, DependencySpec, HostProvider, HostedGitInfo, Integrity,
    LibcSet, LockGraph, OsSet, Package, PackageId, PeerMeta, PlatformConstraints, Resolution,
    SpecTag, infer_tag,
};
use fnpm_workspace::Workspace;
use serde_json::{Map, Value};

use crate::migration::json::{self, Json, Obj};

use crate::migration::{
    self, MigrateOptions, Report, default_tarball_url, is_safe_resolved_tag,
    package_name_from_path, path_escapes, url_is_under_registry,
};
use crate::{Error, Result};

/// The default name used in messages.
const PACKAGE_LOCK: &str = "package-lock.json";

/// Migrates a package-lock.json with default options and finishes it like an install does
/// (trusted and patched dependencies from the manifests). Workspace entries (`link: true`)
/// map to workspace resolutions.
pub fn parse(bytes: &[u8], workspace: &Workspace) -> Result<LockGraph> {
    let (mut graph, _) = migrate(
        bytes,
        workspace,
        &MigrateOptions::default(),
        PACKAGE_LOCK,
        None,
    )?;
    migration::finish_migration(&mut graph, workspace);
    Ok(graph)
}

/// Migrates `bytes`, the contents of `lockfile_name` (`package-lock.json` or
/// `npm-shrinkwrap.json`). `next` is the lockfile tried after this one, for the
/// unsupported-version message.
pub fn migrate(
    bytes: &[u8],
    workspace: &Workspace,
    options: &MigrateOptions,
    lockfile_name: &str,
    next: Option<&str>,
) -> Result<(LockGraph, Report)> {
    migrate_with_locations(bytes, workspace, options, lockfile_name, next).map(|(g, r, _)| (g, r))
}

/// `(packages key, package)` pairs.
pub type Locations = Vec<(String, PackageId)>;

/// [`migrate`], plus the package each `packages` key was migrated to (links map to their
/// target). Entries that were skipped are left out.
pub fn migrate_with_locations(
    bytes: &[u8],
    workspace: &Workspace,
    options: &MigrateOptions,
    lockfile_name: &str,
    next: Option<&str>,
) -> Result<(LockGraph, Report, Locations)> {
    let invalid = |detail: &str| Error::Parse {
        path: PathBuf::from(lockfile_name),
        message: format!("InvalidNPMLockfile: {detail}"),
    };
    let json = json::parse(bytes, false).map_err(|e| Error::Parse {
        path: PathBuf::from(lockfile_name),
        message: format!("InvalidNPMLockfile: {e}"),
    })?;
    let Some(root) = json.as_object() else {
        return Err(invalid("expected an object"));
    };
    match root.get("lockfileVersion") {
        Some(Json::Number(n)) => {
            let v = n.parse::<f64>().unwrap_or(f64::NAN);
            if !(2.0..=4.0).contains(&v) {
                let _ = next;
                return Err(Error::Parse {
                    path: PathBuf::from(lockfile_name),
                    message: format!(
                        "cannot migrate {lockfile_name}: unsupported lockfileVersion {}",
                        v as i64
                    ),
                });
            }
        }
        _ => return Err(invalid("missing lockfileVersion")),
    }

    let entries: Vec<(&str, &Obj<'_>)> = match root.get("packages").and_then(Json::as_object) {
        Some(packages) => {
            let mut entries = Vec::with_capacity(packages.len());
            for (key, value) in packages {
                let Some(obj) = value.as_object() else {
                    return Err(invalid(&format!("entry \"{key}\" is not an object")));
                };
                entries.push((key, obj));
            }
            entries
        }
        None => Vec::new(),
    };
    // The first key must be "", the project itself.
    let Some(&(_, root_entry)) = entries.first().filter(|(key, _)| key.is_empty()) else {
        return Err(invalid(
            "the first entry of \"packages\" must be the root (\"\")",
        ));
    };

    let workspaces = match root_entry.get("workspaces") {
        None => None,
        Some(value) => {
            let valid = match value {
                Json::Array(_) => true,
                Json::Object(obj) => matches!(obj.get("packages"), Some(Json::Array(_))),
                _ => false,
            };
            if !valid {
                return Err(invalid(
                    "\"workspaces\" must be an array or an object with a \"packages\" array",
                ));
            }
            let patterns = json_workspace_patterns(value);
            Some(
                migration::lockfile_workspace_members(&patterns, workspace)
                    .into_iter()
                    .map(|(path, name, version)| (path, (name, version)))
                    .collect::<Vec<_>>(),
            )
        }
    };

    let mut migrator = Migrator::new(entries, workspaces, options, lockfile_name)?;
    migrator.run()?;
    let locations = migrator.locations();
    let Migrator {
        mut graph,
        mut report,
        ..
    } = migrator;

    migration::clear_non_registry_platform_constraints(&mut graph);
    graph.overrides = migration::root_overrides(&graph, workspace, &mut report);
    finish_graph(&mut graph);
    graph.config_version = fnpm_types::ConfigVersion::V0;
    Ok((graph, report, locations))
}

/// The post-processing every migrated graph gets: edges to workspaces are tagged
/// `WORKSPACE` (bun `tag_workspace_links`), the workspace map is rebuilt from the workspace
/// packages, packages only reached through bundled edges are marked bundled, and edges are
/// sorted.
pub(crate) fn finish_graph(graph: &mut LockGraph) {
    let is_workspace: Vec<bool> = graph
        .packages
        .iter()
        .map(|p| matches!(p.resolution, Resolution::Workspace { .. }))
        .collect();
    let mut bundled_only: Vec<Option<bool>> = vec![None; graph.packages.len()];
    for package in &mut graph.packages {
        for edge in &mut package.dependencies {
            let Some(target) = edge.target else { continue };
            if is_workspace[target.index()] {
                edge.behavior |= Behavior::WORKSPACE;
            }
            let slot = &mut bundled_only[target.index()];
            *slot = Some(slot.unwrap_or(true) && edge.behavior.is_bundled());
        }
    }
    for (package, only) in graph.packages.iter_mut().zip(bundled_only) {
        if only == Some(true) {
            package.meta.bundled = true;
        }
    }
    let mut workspaces = BTreeMap::new();
    workspaces.insert(String::new(), PackageId::ROOT);
    for (i, package) in graph.packages.iter().enumerate() {
        if let Resolution::Workspace { path } = &package.resolution {
            workspaces
                .entry(path.clone())
                .or_insert(PackageId(i as u32));
        }
    }
    graph.workspaces = workspaces;
    graph.sort_dependencies();
}

/// The `workspaces` patterns of the lockfile's root entry.
fn json_workspace_patterns(value: &Json<'_>) -> Vec<String> {
    let list = match value {
        Json::Array(list) => list.as_slice(),
        Json::Object(obj) => obj
            .get("packages")
            .and_then(Json::as_array)
            .unwrap_or_default(),
        _ => &[],
    };
    list.iter()
        .filter_map(Json::as_str)
        .map(str::to_owned)
        .collect()
}

/// bun's `Bundle`.
enum Bundle {
    None,
    All,
    Names(Vec<String>),
}

impl Bundle {
    fn contains(&self, name: &str) -> bool {
        match self {
            Bundle::None => false,
            Bundle::All => true,
            Bundle::Names(names) => names.iter().any(|n| n == name),
        }
    }
}

/// A folder inside a dependent that is installed from the cache, read relative to the package.
#[derive(Clone, Copy)]
struct FolderInDependent<'k> {
    path: &'k str,
    /// The fallback name, like in a fresh resolve; npm writes these entries without a name.
    alias: &'k str,
}

const GROUPS: [(&str, Behavior); 4] = [
    ("dependencies", Behavior::PROD),
    ("devDependencies", Behavior::DEV),
    ("optionalDependencies", Behavior::OPTIONAL),
    ("peerDependencies", Behavior::PEER),
];

/// An edge while a package is linked. `workspace_member` rows are the root's links to its
/// workspaces, which only take part in duplicate detection (the graph keeps them in
/// `LockGraph::workspaces`).
struct PendingEdge {
    edge: DepEdge,
    workspace_member: bool,
}

struct Migrator<'a> {
    entries: Vec<(&'a str, &'a Obj<'a>)>,
    workspaces: Option<Vec<(String, (String, String))>>,
    /// Workspace path to its position in `workspaces`.
    workspace_index: migration::FastMap<String, usize>,
    index: migration::FastMap<&'a str, u32>,
    link_entries: Vec<bool>,
    shadowed: Vec<bool>,
    skipped_external: Vec<bool>,
    /// Targets the root or a workspace depends on directly.
    local_declared: Vec<bool>,
    entry_ids: Vec<Option<PackageId>>,
    queue: Vec<(u32, Option<PackageId>)>,
    graph: LockGraph,
    /// `(name, resolution identity)` to package, bun's `get_package_id`.
    dedupe: migration::FastMap<(String, String), PackageId>,
    /// Entries without integrity whose tarball is under neither the configured registry nor
    /// the default one.
    unverifiable: Vec<u32>,
    /// The packages those entries were migrated to, including folders inside a dependent
    /// (bun finds those by their empty URL).
    unverifiable_ids: Vec<PackageId>,
    patched: Vec<String>,
    options: &'a MigrateOptions,
    lockfile_name: &'a str,
    report: Report,
    /// Reused by `find_target`.
    probe: std::cell::Cell<String>,
}

fn flag_is_true(obj: &Obj<'_>, key: &str) -> bool {
    matches!(obj.get(key), Some(Json::Bool(true)))
}

fn parent_dir(dir: &str) -> &str {
    if let Some(i) = dir.rfind("node_modules/") {
        let enclosing = &dir[..i];
        return enclosing.strip_suffix('/').unwrap_or(enclosing);
    }
    match dir.rfind('/') {
        Some(i) => &dir[..i],
        None => "",
    }
}

/// The part of `path` below `dir`, if it is strictly inside it.
fn path_inside<'k>(dir: &str, path: &'k str) -> Option<&'k str> {
    path.strip_prefix(dir)?
        .strip_prefix('/')
        .filter(|rest| !rest.is_empty() && !path_escapes(rest))
}

/// The identity bun's `Resolution::eql` compares: the version for registry packages, the
/// printed resolution otherwise.
pub(crate) fn resolution_identity(resolution: &Resolution) -> String {
    resolution.to_string()
}

/// npm copies `os`, `cpu` and `libc` from package.json as they are, so each can be a single
/// string. Absent `os`/`cpu` allow everything; absent `libc` is "not declared".
fn platform_list<'v>(value: Option<&'v Json<'_>>) -> std::result::Result<Option<Vec<&'v str>>, ()> {
    match value {
        None => Ok(None),
        Some(Json::Str(s)) => Ok(Some(vec![s.as_ref()])),
        Some(Json::Array(items)) => items
            .iter()
            .map(|i| i.as_str().ok_or(()))
            .collect::<std::result::Result<_, _>>()
            .map(Some),
        Some(_) => Err(()),
    }
}

fn normalize_version(version: &str) -> String {
    let v = version.trim();
    let v = v.strip_prefix('=').unwrap_or(v);
    let v = v.strip_prefix('v').unwrap_or(v);
    v.trim().to_owned()
}

impl<'a> Migrator<'a> {
    fn new(
        entries: Vec<(&'a str, &'a Obj<'a>)>,
        workspaces: Option<Vec<(String, (String, String))>>,
        options: &'a MigrateOptions,
        lockfile_name: &'a str,
    ) -> Result<Self> {
        let n = entries.len();
        let mut workspace_index: migration::FastMap<String, usize> = Default::default();
        for (i, (path, _)) in workspaces.iter().flatten().enumerate() {
            workspace_index.entry(path.clone()).or_insert(i);
        }
        let mut this = Self {
            entries,
            workspaces,
            workspace_index,
            index: migration::FastMap::with_capacity_and_hasher(n, Default::default()),
            link_entries: vec![false; n],
            shadowed: vec![false; n],
            skipped_external: vec![false; n],
            local_declared: vec![false; n],
            entry_ids: vec![None; n],
            queue: Vec::new(),
            graph: LockGraph::new(Package::new("", "", Resolution::Root)),
            dedupe: migration::FastMap::with_capacity_and_hasher(n, Default::default()),
            unverifiable: Vec::new(),
            unverifiable_ids: Vec::new(),
            patched: Vec::new(),
            options,
            lockfile_name,
            report: Report::default(),
            probe: std::cell::Cell::new(String::new()),
        };
        this.graph.packages.clear();
        this.build_index()?;
        Ok(this)
    }

    fn invalid(&self, detail: impl std::fmt::Display) -> Error {
        Error::Parse {
            path: PathBuf::from(self.lockfile_name),
            message: format!("InvalidNPMLockfile: {detail}"),
        }
    }

    fn workspace_entry(&self, key: &str) -> Option<&(String, String)> {
        let i = *self.workspace_index.get(key)?;
        self.workspaces.as_ref()?.get(i).map(|(_, v)| v)
    }

    fn build_index(&mut self) -> Result<()> {
        for (j, (key, pkg)) in self.entries.iter().enumerate() {
            if pkg.contains_key("link") {
                self.link_entries[j] = true;
                self.index.insert(key, j as u32);
                continue;
            }
            if flag_is_true(pkg, "extraneous") {
                continue;
            }
            self.index.insert(key, j as u32);
        }
        if self.link_entries[0] || self.index.get("").copied() != Some(0) {
            return Err(self.invalid("the root entry is missing or a link"));
        }
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        let root = self.build_package(0, false, Some(SpecTag::Npm), None)?;
        debug_assert_eq!(root, PackageId::ROOT);
        let mut cursor = 0;
        while cursor < self.queue.len() {
            let (j, id) = self.queue[cursor];
            cursor += 1;
            match id {
                None => self.shadow_dependencies(j),
                Some(id) => self.link_package(j, id)?,
            }
        }
        self.warn_unreachable();

        let mut missing: Vec<PackageId> = self
            .unverifiable_ids
            .iter()
            .copied()
            .filter(|id| {
                self.graph.packages[id.index()]
                    .resolution
                    .integrity()
                    .is_none()
            })
            .collect();
        missing.sort();
        missing.dedup();
        self.report.missing_integrity = missing;
        if let Some((&first, more)) = self.unverifiable.split_first() {
            let url = self.entries[first as usize]
                .1
                .get("resolved")
                .and_then(Json::as_str)
                .unwrap_or_default();
            let more = if more.is_empty() {
                String::new()
            } else {
                format!(", and {} more", more.len())
            };
            self.report.warn(format!(
                "{}: missing integrity, downloading from configured registry instead of {url}{more}",
                self.entries[first as usize].0
            ));
        }
        if !self.patched.is_empty() {
            self.report.warn(format!(
                "skipping npm patches for {} from {}",
                self.patched.join(", "),
                self.lockfile_name
            ));
            self.report
                .notes
                .push("recreate them with fnpm patch <pkg>".to_owned());
        }
        Ok(())
    }

    fn locations(&self) -> Vec<(String, PackageId)> {
        (0..self.entries.len())
            .filter_map(|j| {
                let id = if self.link_entries[j] {
                    self.link_target(j as u32)
                        .and_then(|t| self.entry_ids[t as usize])
                } else {
                    self.entry_ids[j]
                }?;
                Some((self.entries[j].0.to_owned(), id))
            })
            .collect()
    }

    fn build_or_get(&mut self, j: u32, via_link: bool, hint: Option<SpecTag>) -> Result<PackageId> {
        if let Some(id) = self.entry_ids[j as usize] {
            return Ok(id);
        }
        self.build_package(j, via_link, hint, None)
    }

    fn build_package(
        &mut self,
        j: u32,
        via_link: bool,
        hint: Option<SpecTag>,
        folder: Option<FolderInDependent<'a>>,
    ) -> Result<PackageId> {
        let (key, pkg) = self.entries[j as usize];
        let workspace_entry = if j == 0 {
            None
        } else {
            self.workspace_entry(key).cloned()
        };
        let name: String = if let Some((ws_name, _)) = &workspace_entry {
            ws_name.clone()
        } else if let Some(set_name) = pkg.get("name").and_then(Json::as_str) {
            set_name.to_owned()
        } else if let Some(folder) = folder {
            folder.alias.to_owned()
        } else {
            package_name_from_path(key).to_owned()
        };

        if matches!(pkg.get("patched"), Some(p) if *p != Json::Bool(false)) {
            self.patched.push(format!("\"{name}\""));
        }

        let os = platform_list(pkg.get("os"))
            .map_err(|_| self.invalid(format!("\"{key}\": invalid os")))?;
        let cpu = platform_list(pkg.get("cpu"))
            .map_err(|_| self.invalid(format!("\"{key}\": invalid cpu")))?;
        let libc = platform_list(pkg.get("libc"))
            .map_err(|_| self.invalid(format!("\"{key}\": invalid libc")))?;
        let platform = PlatformConstraints {
            os: os.map_or(OsSet::ALL, |l| OsSet::from_list(&l)),
            cpu: cpu.map_or(CpuSet::ALL, |l| CpuSet::from_list(&l)),
            libc: libc.map_or(LibcSet::NONE, |l| LibcSet::from_list(&l)),
        };
        let has_install_script = match pkg.get("hasInstallScript") {
            None => false,
            Some(Json::Bool(b)) => *b,
            Some(_) => {
                return Err(self.invalid(format!("\"{key}\": hasInstallScript must be a boolean")));
            }
        };
        let integrity = match pkg.get("integrity") {
            None => None,
            Some(Json::Str(s)) => Integrity::parse(s).ok(),
            Some(_) => return Err(self.invalid(format!("\"{key}\": integrity must be a string"))),
        };

        let mut resolution = self.entry_resolution(
            j,
            pkg,
            key,
            &name,
            workspace_entry.is_some(),
            via_link,
            hint,
            folder.map(|f| f.path),
        )?;
        if let Some(integrity) = integrity {
            resolution.set_integrity(integrity);
        }
        let mut unverifiable = false;
        if let Resolution::Npm { tarball_url, .. } = &mut resolution
            && let Some(url) = tarball_url.as_deref()
        {
            let configured = self.options.registry_for(&name);
            let under_default = url_is_under_registry(url, fnpm_config::DEFAULT_REGISTRY);
            if !url_is_under_registry(url, configured) && !under_default && integrity.is_none() {
                // A tarball from that host could not be verified; the configured registry's
                // copy is downloaded instead.
                *tarball_url = None;
                self.unverifiable.push(j);
                unverifiable = true;
            } else if under_default {
                *tarball_url = None;
            }
        }

        let identity = (name.clone(), resolution_identity(&resolution));
        if j != 0
            && let Some(&existing) = self.dedupe.get(&identity)
        {
            // A bundled copy carries no `resolved`/`integrity`; a registry copy of the same
            // version does.
            if matches!(resolution, Resolution::Npm { .. }) && integrity.is_some() {
                let existing_pkg = &mut self.graph.packages[existing.index()];
                if existing_pkg.resolution.integrity().is_none() {
                    existing_pkg.resolution = resolution;
                }
            }
            if folder.is_none() {
                self.entry_ids[j as usize] = Some(existing);
            }
            if unverifiable {
                self.unverifiable_ids.push(existing);
            }
            self.shadow(j);
            return Ok(existing);
        }

        let version = match (&resolution, &workspace_entry) {
            (Resolution::Npm { version, .. }, _) => version.clone(),
            (_, Some((_, ws_version))) => ws_version.clone(),
            _ => pkg
                .get("version")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        let mut package = Package::new(name.clone(), version, resolution);
        package.bin = self.parse_bin(key, pkg, &name)?;
        package.platform = platform;
        package.meta.has_install_script = has_install_script;
        package.meta.deprecated = pkg
            .get("deprecated")
            .and_then(Json::as_str)
            .map(str::to_owned);
        package.meta.license = pkg.get("license").and_then(Json::as_str).map(str::to_owned);
        if let Some(engines) = pkg.get("engines").and_then(Json::as_object) {
            package.meta.engines = engines
                .iter()
                .filter_map(|(k, v)| Some((k.to_owned(), v.as_str()?.to_owned())))
                .collect();
        }
        if let Some(meta) = pkg.get("peerDependenciesMeta").and_then(Json::as_object) {
            for (peer, m) in meta {
                let optional = m.as_object().is_some_and(|m| flag_is_true(m, "optional"));
                package
                    .peer_meta
                    .insert(peer.to_owned(), PeerMeta { optional });
            }
        }
        let id = self.graph.push(package);
        if unverifiable {
            self.unverifiable_ids.push(id);
        }
        self.dedupe.insert(identity, id);
        self.queue.push((j, Some(id)));
        // `build_or_get` keeps handing other dependents the build of the entry's own key.
        match folder {
            None => self.entry_ids[j as usize] = Some(id),
            Some(_) => self.shadowed[j as usize] = true,
        }
        Ok(id)
    }

    fn parse_bin(&self, key: &str, pkg: &Obj<'_>, name: &str) -> Result<Bin> {
        let Some(bin) = pkg.get("bin") else {
            return Ok(Bin::None);
        };
        let Some(bin) = bin.as_object() else {
            return Err(self.invalid(format!("\"{key}\": bin must be an object")));
        };
        let mut entries = Vec::with_capacity(bin.len());
        for (bin_name, target) in bin {
            let Some(target) = target.as_str() else {
                return Err(self.invalid(format!("\"{key}\": bin \"{bin_name}\" must be a string")));
            };
            entries.push((bin_name.to_owned(), target.to_owned()));
        }
        Ok(match entries.len() {
            0 => Bin::None,
            1 => {
                let (bin_name, path) = entries.pop().expect("one entry");
                if bin_name == name {
                    Bin::File { path }
                } else {
                    Bin::NamedFile {
                        name: bin_name,
                        path,
                    }
                }
            }
            _ => Bin::Map { entries },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn entry_resolution(
        &mut self,
        j: u32,
        pkg: &Obj<'_>,
        key: &str,
        name: &str,
        is_workspace: bool,
        via_link: bool,
        hint: Option<SpecTag>,
        path_in_dependent: Option<&str>,
    ) -> Result<Resolution> {
        if j == 0 {
            return Ok(Resolution::Root);
        }
        if is_workspace {
            return Ok(Resolution::Workspace {
                path: key.to_owned(),
            });
        }
        if let Some(resolved) = pkg.get("resolved") {
            let Some(r) = resolved.as_str() else {
                return Err(self.invalid(format!("\"{key}\": resolved must be a string")));
            };
            let tag = infer_tag(r);
            if matches!(tag, SpecTag::Git | SpecTag::Github) {
                return self.git_resolution(key, r, tag, name, hint);
            }
            if let Some(local) = r.strip_prefix("file:") {
                return Ok(Resolution::LocalTarball {
                    path: local.to_owned(),
                    integrity: None,
                });
            }
            let version = if hint == Some(SpecTag::Tarball) {
                None
            } else {
                match pkg.get("version") {
                    None => None,
                    Some(Json::Str(v)) => Some(v.as_ref()),
                    Some(_) => {
                        return Err(self.invalid(format!("\"{key}\": version must be a string")));
                    }
                }
            };
            return Ok(match version {
                None => Resolution::RemoteTarball {
                    url: r.to_owned(),
                    integrity: None,
                },
                Some(version) => Resolution::Npm {
                    version: normalize_version(version),
                    tarball_url: Some(r.to_owned()),
                    integrity: None,
                },
            });
        }
        if !via_link
            && key.contains("node_modules/")
            && !name.is_empty()
            && let Some(version) = pkg.get("version").and_then(Json::as_str)
        {
            if name.starts_with('@') && name.find('/').is_none_or(|slash| slash >= name.len() - 1) {
                return Err(self.invalid(format!("\"{key}\": invalid scoped name \"{name}\"")));
            }
            let version = normalize_version(version);
            let url = default_tarball_url(self.options.registry_for(name), name, &version);
            return Ok(Resolution::Npm {
                version,
                tarball_url: Some(url),
                integrity: None,
            });
        }
        Ok(Resolution::Folder {
            path: path_in_dependent.unwrap_or(key).to_owned(),
        })
    }

    fn git_resolution(
        &self,
        key: &str,
        r: &str,
        tag: SpecTag,
        name: &str,
        hint: Option<SpecTag>,
    ) -> Result<Resolution> {
        let use_github = tag == SpecTag::Github
            || (hint == Some(SpecTag::Github)
                && matches!(HostedGitInfo::from_url(r), Ok(Some(info)) if info.provider == HostProvider::Github));
        let github = if use_github {
            match DependencySpec::parse_with_tag(name, r, SpecTag::Github) {
                Ok(DependencySpec::Github(spec)) => Some(spec),
                _ => None,
            }
        } else {
            None
        };
        let committish = match &github {
            Some(spec) => spec.committish.to_raw(),
            None => {
                let rest = r.strip_prefix("git+").unwrap_or(r);
                rest.rfind('#')
                    .map(|i| rest[i + 1..].to_owned())
                    .unwrap_or_default()
            }
        };
        if committish.is_empty() || !is_safe_resolved_tag(&committish) {
            return Err(self.invalid(format!(
                "\"{key}\": git resolution \"{r}\" has no safe commit"
            )));
        }
        Ok(match github {
            Some(spec) => Resolution::Github {
                owner: spec.owner,
                repo: spec.repo,
                committish: committish.clone(),
                resolved_commit: committish,
                integrity: None,
            },
            None => {
                let rest = r.strip_prefix("git+").unwrap_or(r);
                let repo = rest.rfind('#').map_or(rest, |i| &rest[..i]);
                Resolution::Git {
                    url: migration::git_display_url(repo),
                    committish: committish.clone(),
                    resolved_commit: committish,
                    integrity: None,
                }
            }
        })
    }

    fn parse_bundle(&self, key: &str, pkg: &Obj<'_>) -> Result<Bundle> {
        let Some(value) = pkg
            .get("bundleDependencies")
            .or_else(|| pkg.get("bundledDependencies"))
        else {
            return Ok(Bundle::None);
        };
        match value {
            Json::Bool(true) => Ok(Bundle::All),
            Json::Bool(false) => Ok(Bundle::None),
            Json::Array(items) => {
                let mut names = Vec::with_capacity(items.len());
                for item in items {
                    let Some(name) = item.as_str() else {
                        return Err(self
                            .invalid(format!("\"{key}\": bundleDependencies must list strings")));
                    };
                    names.push(name.to_owned());
                }
                Ok(Bundle::Names(names))
            }
            _ => Err(self.invalid(format!(
                "\"{key}\": bundleDependencies must be an array or a boolean"
            ))),
        }
    }

    fn link_package(&mut self, j: u32, id: PackageId) -> Result<()> {
        let (key, pkg): (&'a str, &'a Obj<'a>) = self.entries[j as usize];
        let res = self.graph.packages[id.index()].resolution.clone();
        let is_local = matches!(res, Resolution::Root | Resolution::Workspace { .. });
        let bundle = if is_local {
            Bundle::None
        } else {
            self.parse_bundle(key, pkg)?
        };
        let replace_optional_dups = matches!(res, Resolution::Root | Resolution::Npm { .. });
        let skip_peer_dups = matches!(res, Resolution::Npm { .. });
        let installed_from_cache = matches!(
            res,
            Resolution::Npm { .. }
                | Resolution::LocalTarball { .. }
                | Resolution::RemoteTarball { .. }
                | Resolution::Git { .. }
                | Resolution::Github { .. }
        );
        let is_folder = matches!(res, Resolution::Folder { .. });

        let declared: usize = GROUPS
            .iter()
            .filter_map(|(group, _)| pkg.get(group).and_then(Json::as_object))
            .map(|deps| deps.len())
            .sum();
        let mut edges: Vec<PendingEdge> = Vec::with_capacity(declared);
        if j == 0 {
            self.link_workspaces(&mut edges)?;
        }
        let peer_meta = pkg.get("peerDependenciesMeta").and_then(Json::as_object);

        for (group, group_behavior) in GROUPS {
            let Some(deps) = pkg.get(group) else { continue };
            let Some(deps): Option<&'a Obj<'a>> = deps.as_object() else {
                return Err(self.invalid(format!("\"{key}\": {group} must be an object")));
            };
            let is_peer_group = group_behavior == Behavior::PEER;
            let is_optional_group = group_behavior == Behavior::OPTIONAL;

            for (name, spec) in deps {
                let Some(spec) = spec.as_str() else {
                    return Err(self.invalid(format!("\"{key}\": {group}.{name} must be a string")));
                };
                let version_tag = match migration::spec_tag(name, spec) {
                    Some(tag) if tag != SpecTag::Catalog => tag,
                    _ => {
                        self.report.warn(format!(
                            "skipping \"{name}@{spec}\" from {}: unsupported version specifier",
                            self.lockfile_name
                        ));
                        continue;
                    }
                };

                let mut behavior = group_behavior;
                if is_peer_group
                    && peer_meta
                        .and_then(|m| m.get(name))
                        .and_then(Json::as_object)
                        .is_some_and(|m| flag_is_true(m, "optional"))
                {
                    behavior |= Behavior::OPTIONAL;
                }
                if bundle.contains(name) {
                    behavior |= Behavior::BUNDLED;
                }

                let duplicate_of = if (is_peer_group && skip_peer_dups)
                    || (is_optional_group && replace_optional_dups)
                {
                    edges.iter().position(|e| e.edge.name == *name)
                } else {
                    None
                };
                if is_peer_group && duplicate_of.is_some() {
                    continue;
                }

                // Trust a `file:` spec only in a `file:` package the root or a workspace
                // declares, not in a folder a registry package ships.
                let declares_folder =
                    is_folder && version_tag == SpecTag::Folder && self.local_declared[j as usize];
                let mut found = self.find_target(key, name);
                if let Some((t, false)) = found
                    && self.is_copied_folder(t)
                {
                    self.skip_copied_folder(t, name);
                    continue;
                }
                if let Some((t, _)) = found
                    && is_local
                {
                    self.local_declared[t as usize] = true;
                }
                let has_entry = found.is_some();
                let mut folder_in_dependent: Option<FolderInDependent<'a>> = None;
                if let Some((t, through_link)) = found
                    && !is_local
                    && !declares_folder
                {
                    if through_link && installed_from_cache {
                        let target_key = self.entries[t as usize].0;
                        folder_in_dependent = path_inside(key, target_key)
                            .map(|path| FolderInDependent { path, alias: name });
                    }
                    if folder_in_dependent.is_none() && self.is_external_folder(t, through_link) {
                        self.skip_external(t, name);
                        found = None;
                    }
                }
                let target = match found {
                    Some((t, through_link)) => Some(match folder_in_dependent {
                        Some(folder) => {
                            self.build_package(t, through_link, Some(version_tag), Some(folder))?
                        }
                        None => self.build_or_get(t, through_link, Some(version_tag))?,
                    }),
                    // Left out, so the install resolves it like a newly added dependency.
                    // `--frozen-lockfile` resolves nothing new, so it keeps the edge.
                    None if is_local
                        && behavior.is_peer()
                        && !behavior.is_optional_peer()
                        && !self.options.frozen_lockfile =>
                    {
                        continue;
                    }
                    None if behavior.is_peer() || behavior.contains(Behavior::OPTIONAL) => None,
                    None => {
                        if !has_entry {
                            let from = if key.is_empty() { "." } else { key };
                            self.report.verbose(format!(
                                "skipping \"{name}@{spec}\" from {}: no entry for this dependency of \"{from}\"",
                                self.lockfile_name
                            ));
                        }
                        continue;
                    }
                };

                let edge = DepEdge {
                    name: name.to_owned(),
                    literal: spec.to_owned(),
                    behavior,
                    target,
                };
                match duplicate_of {
                    Some(k) => {
                        edges[k] = PendingEdge {
                            edge,
                            workspace_member: false,
                        }
                    }
                    None => edges.push(PendingEdge {
                        edge,
                        workspace_member: false,
                    }),
                }
            }
        }

        self.graph.packages[id.index()].dependencies = edges
            .into_iter()
            .filter(|e| !e.workspace_member)
            .map(|e| e.edge)
            .collect();
        Ok(())
    }

    fn link_workspaces(&mut self, edges: &mut Vec<PendingEdge>) -> Result<()> {
        let Some(workspaces) = self.workspaces.clone() else {
            return Ok(());
        };
        for (path, (ws_name, _)) in workspaces {
            let target = self
                .index
                .get(path.as_str())
                .copied()
                .filter(|&t| !self.link_entries[t as usize]);
            let Some(t) = target else {
                self.report.verbose(format!(
                    "workspace \"{ws_name}\" ({path}) not in {}, resolving from package.json",
                    self.lockfile_name
                ));
                continue;
            };
            let wid = self.build_or_get(t, false, Some(SpecTag::Workspace))?;
            edges.push(PendingEdge {
                edge: DepEdge {
                    name: ws_name,
                    literal: format!("workspace:{path}"),
                    behavior: Behavior::WORKSPACE,
                    target: Some(wid),
                },
                workspace_member: true,
            });
        }
        Ok(())
    }

    fn find_target(&self, from_key: &str, name: &str) -> Option<(u32, bool)> {
        let mut dir = from_key;
        let mut probe = self.probe.take();
        let found = loop {
            probe.clear();
            if !dir.is_empty() {
                probe.push_str(dir);
                probe.push('/');
            }
            probe.push_str("node_modules/");
            probe.push_str(name);
            if let Some(&t) = self.index.get(probe.as_str()) {
                break if self.link_entries[t as usize] {
                    self.link_target(t).map(|x| (x, true))
                } else {
                    Some((t, false))
                };
            }
            if dir.is_empty() {
                break None;
            }
            dir = parent_dir(dir);
        };
        self.probe.set(probe);
        found
    }

    fn link_target(&self, t: u32) -> Option<u32> {
        let resolved = self.entries[t as usize]
            .1
            .get("resolved")
            .and_then(Json::as_str)?;
        self.index
            .get(resolved)
            .copied()
            .filter(|&x| !self.link_entries[x as usize])
    }

    fn is_external_folder(&self, t: u32, through_link: bool) -> bool {
        let (key, pkg) = self.entries[t as usize];
        if !path_escapes(key) {
            return false;
        }
        if let Some(id) = self.entry_ids[t as usize] {
            return matches!(
                self.graph.packages[id.index()].resolution,
                Resolution::Folder { .. }
            );
        }
        if pkg.contains_key("resolved") || self.workspace_entry(key).is_some() {
            return false;
        }
        through_link || !pkg.contains_key("version") || !key.contains("node_modules/")
    }

    fn skip_external(&mut self, t: u32, name: &str) {
        if self.skipped_external[t as usize] {
            return;
        }
        self.skipped_external[t as usize] = true;
        self.report.warn(format!(
            "skipping \"{name}\" from {}: transitive folder dependency \"{}\" is outside the project",
            self.lockfile_name, self.entries[t as usize].0
        ));
        self.shadow(t);
    }

    /// An entry from `npm install --install-links`: an ordinary entry whose `resolved` is
    /// `file:<dir>`.
    fn is_copied_folder(&self, t: u32) -> bool {
        self.entries[t as usize]
            .1
            .get("resolved")
            .and_then(Json::as_str)
            .is_some_and(|r| r.starts_with("file:") && infer_tag(r) == SpecTag::Folder)
    }

    /// Drops the dependency, so that the install resolves it from the `file:` specifier of
    /// the package that declares it.
    fn skip_copied_folder(&mut self, t: u32, name: &str) {
        if !self.shadowed[t as usize] {
            self.report.verbose(format!(
                "skipping \"{name}\" from {}: folder copied by npm (--install-links)",
                self.lockfile_name
            ));
        }
        self.shadow(t);
    }

    fn shadow(&mut self, j: u32) {
        if self.shadowed[j as usize] {
            return;
        }
        self.shadowed[j as usize] = true;
        self.queue.push((j, None));
    }

    fn shadow_dependencies(&mut self, j: u32) {
        let (key, pkg) = self.entries[j as usize];
        for (group, _) in GROUPS {
            let Some(deps) = pkg.get(group).and_then(Json::as_object) else {
                continue;
            };
            for name in deps.keys() {
                if let Some((t, _)) = self.find_target(key, name)
                    && self.entry_ids[t as usize].is_none()
                {
                    self.shadow(t);
                }
            }
        }
    }

    fn warn_unreachable(&mut self) {
        let mut count = 0usize;
        let mut list = String::new();
        for (j, (key, pkg)) in self.entries.iter().enumerate() {
            if self.link_entries[j]
                || self.entry_ids[j].is_some()
                || self.shadowed[j]
                || flag_is_true(pkg, "extraneous")
            {
                continue;
            }
            count += 1;
            if count <= 5 {
                if count > 1 {
                    list.push_str(", ");
                }
                list.push('"');
                list.push_str(&fnpm_types::text::escape_control_chars(key));
                list.push('"');
            }
        }
        if count == 0 {
            return;
        }
        if count > 5 {
            list.push_str(&format!(" and {} more", count - 5));
        }
        let noun = if count == 1 { "entry" } else { "entries" };
        self.report.verbose(format!(
            "skipping {count} unused {} {noun}: {list}",
            self.lockfile_name
        ));
    }
}

// ---------------------------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------------------------

/// Serialises to package-lock.json lockfileVersion 3, hoisting the graph with every
/// dependency group included (npm's own lockfile lists dev and optional packages).
pub fn serialize(graph: &LockGraph) -> Result<String> {
    let tree = crate::tree::hoist(graph, &export_hoist_options());
    serialize_with_tree(graph, &tree, fnpm_config::DEFAULT_REGISTRY)
}

/// The hoisting a lockfile export uses: every group and every platform, all workspaces.
pub(crate) fn export_hoist_options() -> crate::tree::HoistOptions {
    crate::tree::HoistOptions {
        include: Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER,
        platform: fnpm_types::Platform {
            os: OsSet::ALL,
            cpu: CpuSet::ALL,
            libc: LibcSet::ALL,
        },
        workspaces: None,
        self_contained: Vec::new(),
    }
}

/// Serialises with a given install tree. `registry` is the registry written into `resolved`
/// for packages at the default tarball location.
pub fn serialize_with_tree(
    graph: &LockGraph,
    tree: &crate::tree::Tree,
    registry: &str,
) -> Result<String> {
    let flags = DepFlags::compute(graph);
    let locations = npm_locations(graph, tree);
    let root = graph.root();

    let mut packages = Map::new();
    let mut root_entry = Map::new();
    if !root.name.is_empty() {
        root_entry.insert("name".into(), root.name.clone().into());
    }
    if !root.version.is_empty() {
        root_entry.insert("version".into(), root.version.clone().into());
    }
    let members: Vec<Value> = graph
        .workspaces
        .keys()
        .filter(|p| !p.is_empty())
        .map(|p| Value::from(p.clone()))
        .collect();
    if !members.is_empty() {
        root_entry.insert("workspaces".into(), Value::Array(members));
    }
    write_local_info(&mut root_entry, root);
    packages.insert(String::new(), Value::Object(root_entry));

    // Local packages (workspaces, folders, links) are written once at their own path; the
    // tree entry that installs them is a link.
    let mut local_written: Vec<bool> = vec![false; graph.packages.len()];
    for (path, &id) in &graph.workspaces {
        if path.is_empty() {
            continue;
        }
        write_local_target(&mut packages, graph, id, path, &mut local_written);
    }

    for (node, location) in tree.nodes.iter().zip(&locations) {
        if packages.contains_key(location.as_str()) {
            continue;
        }
        let Some(package) = graph.get(node.package) else {
            continue;
        };
        let mut entry = Map::new();
        match &package.resolution {
            Resolution::Root => {
                entry.insert("resolved".into(), "".into());
                entry.insert("link".into(), true.into());
            }
            Resolution::Workspace { path }
            | Resolution::Folder { path }
            | Resolution::Symlink { path } => {
                entry.insert("resolved".into(), path.clone().into());
                entry.insert("link".into(), true.into());
                packages.insert(location.clone(), Value::Object(entry));
                write_local_target(&mut packages, graph, node.package, path, &mut local_written);
                continue;
            }
            _ => {
                if package.name != node.name {
                    entry.insert("name".into(), package.name.clone().into());
                }
                write_remote_info(&mut entry, package, registry);
                flags.write(&mut entry, node.package);
                write_registry_info(&mut entry, package, graph);
            }
        }
        packages.insert(location.clone(), Value::Object(entry));
    }

    let mut doc = Map::new();
    if !root.name.is_empty() {
        doc.insert("name".into(), root.name.clone().into());
    }
    if !root.version.is_empty() {
        doc.insert("version".into(), root.version.clone().into());
    }
    doc.insert("lockfileVersion".into(), 3.into());
    doc.insert("requires".into(), true.into());
    doc.insert("packages".into(), Value::Object(packages));
    let mut out = serde_json::to_string_pretty(&Value::Object(doc)).map_err(|e| Error::Parse {
        path: PathBuf::from(PACKAGE_LOCK),
        message: e.to_string(),
    })?;
    out.push('\n');
    Ok(out)
}

fn write_local_target(
    packages: &mut Map<String, Value>,
    graph: &LockGraph,
    id: PackageId,
    path: &str,
    written: &mut [bool],
) {
    if written[id.index()] || packages.contains_key(path) {
        return;
    }
    written[id.index()] = true;
    let Some(package) = graph.get(id) else { return };
    let mut entry = Map::new();
    if !package.name.is_empty() {
        entry.insert("name".into(), package.name.clone().into());
    }
    if !package.version.is_empty() {
        entry.insert("version".into(), package.version.clone().into());
    }
    write_local_info(&mut entry, package);
    packages.insert(path.to_owned(), Value::Object(entry));
}

fn group_object(package: &Package, group: Behavior) -> Option<Value> {
    let mut map = Map::new();
    for edge in &package.dependencies {
        if migration::is_workspace_only(edge.behavior) {
            continue;
        }
        let in_group = match group {
            Behavior::OPTIONAL => edge.behavior.group() == fnpm_types::DependencyGroup::Optional,
            Behavior::PEER => edge.behavior.is_peer(),
            Behavior::DEV => edge.behavior.group() == fnpm_types::DependencyGroup::Dev,
            _ => edge.behavior.group() == fnpm_types::DependencyGroup::Prod,
        };
        if in_group {
            map.insert(edge.name.clone(), edge.literal.clone().into());
        }
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

fn write_groups(entry: &mut Map<String, Value>, package: &Package, include_dev: bool) {
    for (key, group) in [
        ("dependencies", Behavior::PROD),
        ("devDependencies", Behavior::DEV),
        ("optionalDependencies", Behavior::OPTIONAL),
        ("peerDependencies", Behavior::PEER),
    ] {
        if group == Behavior::DEV && !include_dev {
            continue;
        }
        if let Some(obj) = group_object(package, group) {
            entry.insert(key.into(), obj);
        }
    }
    let optional_peers: Map<String, Value> = package
        .dependencies
        .iter()
        .filter(|e| e.behavior.is_optional_peer())
        .map(|e| (e.name.clone(), serde_json::json!({ "optional": true })))
        .collect();
    if !optional_peers.is_empty() {
        entry.insert("peerDependenciesMeta".into(), Value::Object(optional_peers));
    }
}

fn write_bin(entry: &mut Map<String, Value>, package: &Package) {
    let bins: Map<String, Value> = package
        .bin
        .entries(&package.name)
        .into_iter()
        .map(|(n, p)| (n.to_owned(), Value::from(p)))
        .collect();
    if !bins.is_empty() {
        entry.insert("bin".into(), Value::Object(bins));
    }
}

fn write_local_info(entry: &mut Map<String, Value>, package: &Package) {
    write_groups(entry, package, true);
    write_bin(entry, package);
    if !package.meta.engines.is_empty() {
        entry.insert(
            "engines".into(),
            serde_json::to_value(&package.meta.engines).unwrap_or_default(),
        );
    }
}

fn write_remote_info(entry: &mut Map<String, Value>, package: &Package, registry: &str) {
    if !package.version.is_empty() {
        entry.insert("version".into(), package.version.clone().into());
    }
    let resolved = match &package.resolution {
        Resolution::Npm {
            version,
            tarball_url,
            ..
        } => Some(
            tarball_url
                .clone()
                .unwrap_or_else(|| default_tarball_url(registry, &package.name, version)),
        ),
        Resolution::Git {
            url,
            resolved_commit,
            ..
        } => Some(format!("git+{url}#{resolved_commit}")),
        Resolution::Github {
            owner,
            repo,
            resolved_commit,
            ..
        } => Some(format!(
            "git+ssh://git@github.com/{owner}/{repo}.git#{}",
            // A graph read from bun.lock(b) holds bun's `owner-repo-commit` tag.
            super::bun_text::resolved_commit(resolved_commit)
        )),
        Resolution::RemoteTarball { url, .. } => Some(url.clone()),
        Resolution::LocalTarball { path, .. } => Some(format!("file:{path}")),
        _ => None,
    };
    if let Some(resolved) = resolved {
        entry.insert("resolved".into(), resolved.into());
    }
    if let Some(integrity) = package.resolution.integrity() {
        entry.insert("integrity".into(), integrity.to_sri().into());
    }
}

fn write_registry_info(entry: &mut Map<String, Value>, package: &Package, graph: &LockGraph) {
    let _ = graph;
    if package.meta.bundled {
        entry.insert("inBundle".into(), true.into());
    }
    if package.meta.needs_scripts() {
        entry.insert("hasInstallScript".into(), true.into());
    }
    if let Some(license) = &package.meta.license {
        entry.insert("license".into(), license.clone().into());
    }
    write_groups(entry, package, false);
    let bundled: Vec<Value> = package
        .dependencies
        .iter()
        .filter(|e| e.behavior.is_bundled())
        .map(|e| Value::from(e.name.clone()))
        .collect();
    if !bundled.is_empty() {
        entry.insert("bundleDependencies".into(), Value::Array(bundled));
    }
    write_bin(entry, package);
    if !package.meta.engines.is_empty() {
        entry.insert(
            "engines".into(),
            serde_json::to_value(&package.meta.engines).unwrap_or_default(),
        );
    }
    if package.platform.os != OsSet::ALL {
        entry.insert("os".into(), package.platform.os.to_list().into());
    }
    if package.platform.cpu != CpuSet::ALL {
        entry.insert("cpu".into(), package.platform.cpu.to_list().into());
    }
    if package.platform.libc != LibcSet::NONE {
        entry.insert("libc".into(), package.platform.libc.to_list().into());
    }
    if let Some(deprecated) = &package.meta.deprecated {
        entry.insert("deprecated".into(), deprecated.clone().into());
    }
}

/// npm's `dev` / `optional` / `devOptional` / `peer` flags: a package gets a flag when every
/// path to it from the root and the workspaces goes through an edge of that kind.
struct DepFlags {
    dev: Vec<bool>,
    optional: Vec<bool>,
    dev_optional: Vec<bool>,
    peer: Vec<bool>,
}

impl DepFlags {
    fn compute(graph: &LockGraph) -> Self {
        let reach = |excluded: Behavior| -> Vec<bool> {
            let mut seen = vec![false; graph.packages.len()];
            let mut stack: Vec<PackageId> = graph.workspaces.values().copied().collect();
            while let Some(id) = stack.pop() {
                if std::mem::replace(&mut seen[id.index()], true) {
                    continue;
                }
                for edge in &graph.packages[id.index()].dependencies {
                    let Some(target) = edge.target else { continue };
                    if edge.behavior.intersects(excluded) || seen[target.index()] {
                        continue;
                    }
                    stack.push(target);
                }
            }
            seen
        };
        let not = |v: Vec<bool>| v.into_iter().map(|b| !b).collect::<Vec<_>>();
        Self {
            dev: not(reach(Behavior::DEV)),
            optional: not(reach(Behavior::OPTIONAL)),
            dev_optional: not(reach(Behavior::DEV | Behavior::OPTIONAL)),
            peer: not(reach(Behavior::PEER)),
        }
    }

    fn write(&self, entry: &mut Map<String, Value>, id: PackageId) {
        let i = id.index();
        if self.dev[i] {
            entry.insert("dev".into(), true.into());
        }
        if self.optional[i] {
            entry.insert("optional".into(), true.into());
        }
        if self.dev_optional[i] && !self.dev[i] && !self.optional[i] {
            entry.insert("devOptional".into(), true.into());
        }
        if self.peer[i] {
            entry.insert("peer".into(), true.into());
        }
    }
}

/// The package-lock.json key of every tree node: `node_modules/a/node_modules/b`, with the
/// `node_modules` of a workspace (or linked folder) under its own path.
pub(crate) fn npm_locations(graph: &LockGraph, tree: &crate::tree::Tree) -> Vec<String> {
    let mut workspace_of: HashMap<u32, &str> = HashMap::new();
    for (ws, nodes) in &tree.workspace_roots {
        if let Some(Resolution::Workspace { path }) = graph.get(*ws).map(|p| &p.resolution) {
            for &n in nodes {
                workspace_of.insert(n, path.as_str());
            }
        }
    }
    let mut out: Vec<String> = Vec::with_capacity(tree.nodes.len());
    for (i, node) in tree.nodes.iter().enumerate() {
        let location = match node.parent {
            None => match workspace_of.get(&(i as u32)) {
                Some(path) if !path.is_empty() => format!("{path}/node_modules/{}", node.name),
                _ => format!("node_modules/{}", node.name),
            },
            Some(parent) => {
                let parent_node = &tree.nodes[parent as usize];
                let base = match graph.get(parent_node.package).map(|p| &p.resolution) {
                    Some(
                        Resolution::Workspace { path }
                        | Resolution::Folder { path }
                        | Resolution::Symlink { path },
                    ) => path.clone(),
                    _ => out
                        .get(parent as usize)
                        .cloned()
                        .unwrap_or_else(|| format!("node_modules/{}", parent_node.name)),
                };
                format!("{base}/node_modules/{}", node.name)
            }
        };
        out.push(location);
    }
    out
}
