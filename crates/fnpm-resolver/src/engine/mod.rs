//! The resolve driver.
//!
//! Dependencies are resolved strictly in queue order (bun `advance`): the queue stops at the
//! first row whose manifest has not arrived, while every row queued behind it has already asked
//! for its manifest and each arriving manifest speculatively requests what its likely version
//! depends on (`request_ahead` / `look_ahead` / `follow_likely_version`). That keeps results
//! independent of network timing while keeping the network saturated. Git, GitHub and tarball
//! packages never block the queue: they are fetched in the background and take their turn, in
//! order, once the queue is empty. Peers wait until everything else is resolved and are then
//! resolved in batches (bun's peer pass).

mod bind;
mod finish;
mod lookahead;
mod peers;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fnpm_config::Config;
use fnpm_registry::{ManifestRequest, PackageManifest};
use fnpm_semver::Version;
use fnpm_types::{
    Behavior, DepEdge, DependencyGroup, DependencySpec, Event, EventSink, LockGraph, Package,
    PackageId, PatchedDependency, Resolution, SpecError,
};
use fnpm_workspace::{PackageJson, Workspace};
use tokio::sync::mpsc;

use crate::extensions::ExtensionRule;
use crate::local::{self, LocalKind};
use crate::options::ReleaseAgeExcludes;
use crate::overrides::OverrideSet;
use crate::source::{FetchedPackage, ManifestSource, PackageFetcher};
use crate::{Error, Mode, Request, Resolved, Result};

/// One dependency edge waiting in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Row {
    pub pkg: PackageId,
    pub edge: u32,
    /// Resolved in the peer pass (peers are installed rather than deferred).
    pub install_peer: bool,
}

/// What kind of package a graph entry is, for the rules that depend on the declarer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Root or workspace member.
    Project,
    Folder,
    Link,
    Registry,
    /// Git, GitHub or tarball.
    Remote,
}

#[derive(Debug, Clone)]
pub(crate) struct PState {
    /// npm version (or workspace version) parsed once.
    pub version: Option<Version>,
    /// The same package in the previous graph.
    pub prev: Option<PackageId>,
    /// Came from the previous graph (bun: `id < loaded_package_count`).
    pub loaded: bool,
    /// Some dependency asks for exactly this version.
    pub pinned: bool,
    pub kind: Kind,
    /// Absolute directory for project and folder packages.
    pub dir: Option<PathBuf>,
    /// Folder packages declared by the project (or a top-level override) read their own
    /// `file:` / `link:` dependencies like a project package.
    pub local_declarer: bool,
    /// Only reached through optional dependencies so far: failures below it are tolerated.
    pub optional_only: bool,
    /// Ships inside another package's tarball.
    pub in_bundle: bool,
    pub needed: bool,
    pub emitted: bool,
}

pub(crate) enum MState {
    Pending,
    Ready(Arc<PackageManifest>),
    Failed(Arc<fnpm_registry::Error>),
}

pub(crate) enum RState {
    Pending,
    Ready(Box<(Resolution, FetchedPackage)>),
    Failed(Arc<fnpm_registry::Error>),
    /// The package was created.
    Done(PackageId),
}

pub(crate) enum Completion {
    Manifest(String, fnpm_registry::Result<Arc<PackageManifest>>),
    Remote(
        String,
        fnpm_registry::Result<Box<(Resolution, FetchedPackage)>>,
    ),
}

/// The effective request of an edge after catalogs and overrides.
#[derive(Debug, Clone)]
pub(crate) struct Asked {
    /// The registry name for npm specs; the edge name otherwise.
    pub name: String,
    pub spec: DependencySpec,
    /// The literal that was parsed into `spec`.
    pub literal: String,
    /// An override or catalog replaced the declared spec.
    pub replaced: bool,
    /// The replacement came from a root-level override rule.
    pub from_rule: bool,
    /// A project's `catalog:` reference was replaced by its catalog entry.
    pub from_catalog: bool,
}

/// Update scope of `fnpm update`.
#[derive(Debug, Clone, Default)]
pub(crate) struct UpdatePlan {
    pub active: bool,
    pub names: HashSet<String>,
    pub latest: bool,
}

pub(crate) struct Engine<'a> {
    pub cfg: &'a Config,
    pub patch_hashes: &'a std::collections::BTreeMap<String, String>,
    pub ws: &'a Workspace,
    pub prev: Option<&'a LockGraph>,
    pub manifests: Arc<dyn ManifestSource>,
    pub fetcher: Arc<dyn PackageFetcher>,
    pub events: &'a dyn EventSink,
    pub tx: Option<mpsc::UnboundedSender<Resolved>>,

    pub graph: LockGraph,
    pub st: Vec<PState>,
    /// name -> package ids, highest npm version first.
    pub by_name: HashMap<String, Vec<PackageId>>,
    /// `name@resolution` -> id.
    pub by_key: HashMap<String, PackageId>,
    /// Previous graph: name -> ids (highest first) and key -> id.
    pub prev_by_name: HashMap<String, Vec<PackageId>>,
    pub prev_by_key: HashMap<String, PackageId>,
    pub prev_versions: Vec<Option<Version>>,
    pub prev_non_peer_targets: HashSet<PackageId>,
    pub changed_peer_providers: HashSet<String>,
    pub imported: HashMap<PackageId, PackageId>,

    pub queue: VecDeque<Row>,
    pub deferred_peers: Vec<Row>,
    pub in_peer_pass: bool,
    pub remote_turns: VecDeque<String>,
    pub remote: HashMap<String, RState>,
    pub remote_waiters: HashMap<String, Vec<Row>>,
    pub manifest_state: HashMap<String, MState>,
    pub done_tx: mpsc::UnboundedSender<Completion>,
    pub done_rx: mpsc::UnboundedReceiver<Completion>,
    pub in_flight: usize,

    pub lookahead_seen: HashSet<String>,
    pub lookahead_waiting: HashMap<String, Vec<(String, String, bool)>>,
    pub manifest_order: Vec<String>,
    pub manifest_ordered: HashSet<String>,

    pub overrides: OverrideSet,
    /// The previous graph's rules, to tell whether an edge still means what it meant.
    pub prev_overrides: OverrideSet,
    pub extension_rules: Vec<ExtensionRule>,
    pub gate: Option<(u64, ReleaseAgeExcludes)>,
    pub now_ms: u64,
    pub update: UpdatePlan,
    pub requested_rows: HashMap<(PackageId, u32), usize>,
    pub requests: Vec<Request>,
    pub request_targets: Vec<Option<PackageId>>,
    pub kept_patched: Vec<String>,
    /// `(package, deprecated warning)` of the registry packages resolved from a manifest by
    /// this run; [`Engine::finish`] reports what the project asks for.
    pub deprecated: Vec<(PackageId, String)>,
    pub warnings: Vec<String>,
    /// An omitted local dependency was left unresolved: the lockfile must not be saved.
    pub unsaved: bool,
    pub peer_askers: Option<peers::Askers>,
    /// Reverse edges (`target -> [(owner, edge is a peer)]`), built when the peer pass starts and
    /// kept up to date by [`Engine::set_target`] / [`Engine::add_package`] from then on.
    pub parents: Option<Vec<Vec<(PackageId, bool)>>>,
    pub progress: u32,
}

pub(crate) struct EngineInput<'a> {
    pub cfg: &'a Config,
    pub patch_hashes: &'a std::collections::BTreeMap<String, String>,
    pub ws: &'a Workspace,
    pub prev: Option<&'a LockGraph>,
    pub mode: Mode,
    pub requests: Vec<Request>,
    pub removals: Vec<(String, String)>,
    pub manifests: Arc<dyn ManifestSource>,
    pub fetcher: Arc<dyn PackageFetcher>,
    pub events: &'a dyn EventSink,
    pub tx: Option<mpsc::UnboundedSender<Resolved>>,
    pub hints: Vec<String>,
}

/// Output of a full resolve (before `ResolveOutput` is assembled).
pub(crate) struct EngineOutput {
    pub graph: LockGraph,
    pub requests: Vec<(Request, PackageId)>,
    pub warnings: Vec<String>,
    pub manifest_order: Vec<String>,
    pub unsaved: bool,
}

/// SHA-256 (hex) of a `patchedDependencies` file, part of the patched package's store key. A
/// missing or unreadable file is an error: the install must fail before anything is linked.
pub(crate) fn patch_hash(root_dir: &Path, key: &str, path: &str) -> Result<String> {
    match std::fs::read(root_dir.join(path)) {
        Ok(bytes) => {
            Ok(fnpm_types::Integrity::compute(fnpm_types::HashAlgorithm::Sha256, &bytes).to_hex())
        }
        Err(e) => Err(Error::PatchFile {
            key: key.to_owned(),
            path: path.to_owned(),
            message: format!("Couldn't find patch file: {e}"),
        }),
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// npm-like version of a package: its npm resolution version, or a workspace's version.
pub(crate) fn package_version(p: &Package) -> Option<Version> {
    match &p.resolution {
        Resolution::Npm { version, .. } => Version::parse(version).ok(),
        Resolution::Workspace { .. } => Version::parse(&p.version).ok(),
        _ => None,
    }
}

/// Inserts `id` into a highest-version-first list.
fn insert_sorted(
    list: &mut Vec<PackageId>,
    id: PackageId,
    versions: &dyn Fn(PackageId) -> Option<Version>,
) {
    let v = versions(id);
    let at = list
        .iter()
        .position(|&other| match (&v, versions(other)) {
            (Some(a), Some(b)) => a.cmp(&b) == std::cmp::Ordering::Greater,
            (Some(_), None) => true,
            _ => false,
        })
        .unwrap_or(list.len());
    list.insert(at, id);
}

impl<'a> Engine<'a> {
    pub(crate) fn new(input: EngineInput<'a>) -> Result<Self> {
        let EngineInput {
            cfg,
            patch_hashes,
            ws,
            prev,
            mode,
            requests,
            removals,
            manifests,
            fetcher,
            events,
            tx,
            hints,
        } = input;
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let update = match &mode {
            Mode::Update { names, latest } => UpdatePlan {
                active: true,
                names: names.iter().cloned().collect(),
                latest: *latest,
            },
            _ => UpdatePlan::default(),
        };
        let gate = cfg.minimum_release_age_ms.filter(|ms| *ms > 0).map(|ms| {
            let (excludes, _) = ReleaseAgeExcludes::parse(&cfg.minimum_release_age_excludes);
            (ms, excludes)
        });
        let mut engine = Engine {
            cfg,
            patch_hashes,
            ws,
            prev,
            manifests,
            fetcher,
            events,
            tx,
            graph: LockGraph::new(Package::new("", "", Resolution::Root)),
            st: Vec::new(),
            by_name: HashMap::new(),
            by_key: HashMap::new(),
            prev_by_name: HashMap::new(),
            prev_by_key: HashMap::new(),
            prev_versions: Vec::new(),
            prev_non_peer_targets: HashSet::new(),
            changed_peer_providers: HashSet::new(),
            imported: HashMap::new(),
            queue: VecDeque::new(),
            deferred_peers: Vec::new(),
            in_peer_pass: false,
            remote_turns: VecDeque::new(),
            remote: HashMap::new(),
            remote_waiters: HashMap::new(),
            manifest_state: HashMap::new(),
            done_tx,
            done_rx,
            in_flight: 0,
            lookahead_seen: HashSet::new(),
            lookahead_waiting: HashMap::new(),
            manifest_order: Vec::new(),
            manifest_ordered: HashSet::new(),
            overrides: OverrideSet::new(&[]),
            prev_overrides: OverrideSet::new(&[]),
            extension_rules: Vec::new(),
            gate,
            now_ms: now_ms(),
            update,
            requested_rows: HashMap::new(),
            requests,
            request_targets: Vec::new(),
            kept_patched: Vec::new(),
            deprecated: Vec::new(),
            warnings: Vec::new(),
            unsaved: false,
            peer_askers: None,
            parents: None,
            progress: 0,
        };
        if engine.gate.is_some() {
            let (_, warnings) = ReleaseAgeExcludes::parse(&cfg.minimum_release_age_excludes);
            engine.warnings.extend(warnings);
        }
        engine.index_previous();
        engine.setup_root_fields()?;
        if engine.prev.is_none() {
            for name in hints {
                engine.request_manifest(&name, false, true);
            }
        }
        engine.add_project_packages(&removals)?;
        Ok(engine)
    }

    fn index_previous(&mut self) {
        let Some(prev) = self.prev else { return };
        self.prev_overrides = OverrideSet::new(&prev.overrides);
        self.prev_versions = prev.packages.iter().map(package_version).collect();
        self.prev_non_peer_targets = prev
            .packages
            .iter()
            .flat_map(|p| {
                p.dependencies
                    .iter()
                    .filter(|e| !e.behavior.is_peer())
                    .filter_map(|e| e.target)
            })
            .collect();
        for (i, p) in prev.packages.iter().enumerate() {
            let id = PackageId(i as u32);
            self.prev_by_key.entry(p.key()).or_insert(id);
            if matches!(p.resolution, Resolution::Npm { .. }) {
                let versions = &self.prev_versions;
                let list = self.prev_by_name.entry(p.name.clone()).or_default();
                insert_sorted(list, id, &|x: PackageId| versions[x.index()].clone());
            }
        }
    }

    /// overrides / resolutions, catalogs, patchedDependencies, trustedDependencies and
    /// packageExtensions from the root (and workspace) package.json files.
    fn setup_root_fields(&mut self) -> Result<()> {
        let ws = self.ws;
        let root = &ws.root.raw;
        let members: Vec<&PackageJson> = ws.members.iter().map(|m| &m.manifest).collect();
        let (rules, warnings) =
            crate::overrides::parse_overrides_with(root, &members, &ws.settings.overrides)?;
        self.warnings.extend(warnings);
        self.overrides = OverrideSet::new(&rules);
        self.graph.overrides = rules;
        self.graph.catalogs = crate::catalogs::parse_catalogs(root, &ws.settings.catalogs)?;
        let (ext_rules, recorded, warnings) = crate::extensions::parse_package_extensions(root);
        self.warnings.extend(warnings);
        self.extension_rules = ext_rules;
        self.graph.package_extensions = recorded;
        self.graph.patched_dependencies = self.parse_patched(root)?;
        for member in &ws.members {
            if member.manifest.raw.get("patchedDependencies").is_some() {
                let text = "ignoring \"patchedDependencies\" in workspace package, move it to the root package.json";
                if !self.warnings.iter().any(|w| w == text) {
                    self.warnings.push(text.into());
                }
            }
        }
        self.graph.trusted_dependencies = crate::frozen::trusted(self.ws)?;
        if let Some(prev) = self.prev {
            self.graph.node_linker = prev.node_linker;
            self.graph.config_version = prev.config_version;
        }
        Ok(())
    }

    fn parse_patched(
        &mut self,
        root: &serde_json::Value,
    ) -> Result<std::collections::BTreeMap<String, PatchedDependency>> {
        let mut out = std::collections::BTreeMap::new();
        let Some(field) = root.get("patchedDependencies") else {
            return Ok(out);
        };
        let Some(object) = field.as_object() else {
            return Err(Error::InvalidPackageJson {
                path: "package.json".into(),
                message:
                    "\"patchedDependencies\" expects a map of \"name@version\" to patch file paths"
                        .into(),
            });
        };
        for (key, value) in object {
            let Some(path) = value.as_str() else { continue };
            let (name, version) = fnpm_types::name::split_name_and_maybe_version(key);
            if version.is_none_or(str::is_empty) || name.is_empty() {
                self.warnings.push(format!(
                    "patchedDependencies keys need a version: {key}@<version>"
                ));
                continue;
            }
            let hash = match self.patch_hashes.get(key) {
                Some(hash) => hash.clone(),
                None => patch_hash(&self.ws.root_dir, key, path)?,
            };
            out.insert(
                key.clone(),
                PatchedDependency {
                    path: path.to_owned(),
                    hash: Some(hash),
                },
            );
        }
        Ok(out)
    }

    /// The root and workspace packages with their declared edges, `fnpm add` requests and
    /// `fnpm remove` removals applied.
    fn add_project_packages(&mut self, removals: &[(String, String)]) -> Result<()> {
        let root_dir = self.ws.root_dir.clone();
        let fallback = root_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut root = local::package_from_json(&self.ws.root, &fallback, Resolution::Root);
        let (edges, warnings) = local::package_json_edges(&self.ws.root, LocalKind::Root)?;
        self.warnings.extend(warnings);
        root.dependencies = edges;
        let root_version = Version::parse(&root.version).ok();
        self.graph = LockGraph {
            overrides: std::mem::take(&mut self.graph.overrides),
            catalogs: std::mem::take(&mut self.graph.catalogs),
            patched_dependencies: std::mem::take(&mut self.graph.patched_dependencies),
            trusted_dependencies: self.graph.trusted_dependencies.take(),
            package_extensions: std::mem::take(&mut self.graph.package_extensions),
            node_linker: self.graph.node_linker,
            config_version: self.graph.config_version,
            ..LockGraph::new(root)
        };
        let prev_root = self.prev.map(|_| PackageId::ROOT);
        self.st
            .push(PState::project(root_version, prev_root, root_dir.clone()));
        self.by_key.insert(self.graph.root().key(), PackageId::ROOT);
        for member in &self.ws.members {
            let resolution = Resolution::Workspace {
                path: member.path.clone(),
            };
            let mut package = local::package_from_json(&member.manifest, &member.name, resolution);
            package.name = member.name.clone();
            package.version = member.version.clone();
            let (edges, warnings) =
                local::package_json_edges(&member.manifest, LocalKind::Workspace)?;
            self.warnings.extend(warnings);
            package.dependencies = edges;
            let version = Version::parse(&member.version).ok();
            let prev_id = self
                .prev
                .and_then(|p| p.workspaces.get(&member.path).copied());
            let id = self.graph.add_workspace(package);
            self.by_key
                .insert(self.graph.packages[id.index()].key(), id);
            self.by_name
                .entry(member.name.clone())
                .or_default()
                .push(id);
            self.st
                .push(PState::project(version, prev_id, member.dir.clone()));
        }
        // `fnpm remove`.
        for (workspace, name) in removals {
            if let Some(&id) = self.graph.workspaces.get(workspace) {
                self.graph.packages[id.index()]
                    .dependencies
                    .retain(|e| &e.name != name);
            }
        }
        // `fnpm add` / `fnpm update <pkg>@<spec>`.
        let requests = std::mem::take(&mut self.requests);
        for request in &requests {
            let Some(&id) = self.graph.workspaces.get(&request.workspace) else {
                return Err(Error::WorkspaceNotFound {
                    name: request.workspace.clone(),
                });
            };
            let pkg = &mut self.graph.packages[id.index()];
            let behavior = request.group.behavior();
            // bun's move-to-group rule: the request replaces every non-peer entry of the name.
            pkg.dependencies.retain(|e| {
                e.name != request.name
                    || (e.behavior.is_peer() && request.group != DependencyGroup::Peer)
            });
            pkg.dependencies.push(DepEdge::new(
                request.name.clone(),
                request.spec.clone(),
                behavior,
            ));
            pkg.dependencies.sort_by(|a, b| {
                local::behavior_order(a.behavior, b.behavior).then_with(|| a.name.cmp(&b.name))
            });
        }
        for (i, request) in requests.iter().enumerate() {
            if let Some(&id) = self.graph.workspaces.get(&request.workspace) {
                let behavior = request.group.behavior();
                if let Some(at) = self.graph.packages[id.index()]
                    .dependencies
                    .iter()
                    .position(|e| e.name == request.name && e.behavior == behavior)
                {
                    self.requested_rows.insert((id, at as u32), i);
                }
            }
        }
        self.request_targets = vec![None; requests.len()];
        self.requests = requests;
        if let Some(prev) = self.prev {
            for (path, &id) in &self.graph.workspaces {
                let current = &self.graph.packages[id.index()];
                let previous = prev
                    .workspaces
                    .get(path)
                    .map(|id| &prev.packages[id.index()]);
                let before: HashMap<_, _> = previous
                    .into_iter()
                    .flat_map(|p| &p.dependencies)
                    .filter(|e| !e.behavior.is_peer())
                    .map(|e| (&e.name, &e.literal))
                    .collect();
                let now: HashMap<_, _> = current
                    .dependencies
                    .iter()
                    .filter(|e| !e.behavior.is_peer())
                    .map(|e| (&e.name, &e.literal))
                    .collect();
                for name in before.keys().chain(now.keys()) {
                    if before.get(name) != now.get(name) {
                        self.changed_peer_providers.insert((*name).clone());
                    }
                }
            }
            for path in prev
                .workspaces
                .keys()
                .filter(|p| !self.graph.workspaces.contains_key(*p))
            {
                let p = &prev.packages[prev.workspaces[path].index()];
                self.changed_peer_providers.extend(
                    p.dependencies
                        .iter()
                        .filter(|e| !e.behavior.is_peer())
                        .map(|e| e.name.clone()),
                );
            }
        }
        let ids: Vec<PackageId> = self.graph.workspaces.values().copied().collect();
        let mut ordered = vec![PackageId::ROOT];
        ordered.extend(ids.into_iter().filter(|id| *id != PackageId::ROOT));
        for id in ordered {
            self.mark_needed(id);
            self.push_rows(id, false);
        }
        Ok(())
    }

    /// Parses the spec of `name: literal` (cheap: no allocation for the common range forms beyond the owned strings).
    pub(crate) fn parse_spec(
        &self,
        name: &str,
        literal: &str,
    ) -> std::result::Result<DependencySpec, SpecError> {
        DependencySpec::parse(name, literal)
    }

    pub(crate) fn edge(&self, row: Row) -> &DepEdge {
        &self.graph.packages[row.pkg.index()].dependencies[row.edge as usize]
    }

    pub(crate) fn root_dir(&self) -> &Path {
        &self.ws.root_dir
    }

    /// Queues the edges of a package in order; peers wait for the peer pass.
    pub(crate) fn push_rows(&mut self, pkg: PackageId, install_peer: bool) {
        let n = self.graph.packages[pkg.index()].dependencies.len();
        for i in 0..n {
            let row = Row {
                pkg,
                edge: i as u32,
                install_peer,
            };
            self.queue.push_back(row);
            self.request_row_ahead(row);
        }
    }

    pub(crate) fn emit_progress(&mut self, name: &str) {
        self.progress += 1;
        if self.events.enabled() {
            let resolved = self.graph.len() as u32;
            self.events.emit(Event::Resolve {
                resolved,
                total: resolved + self.queue.len() as u32,
                name: Some(name.to_owned()),
            });
        }
    }

    /// Adds a package to the graph and the indexes.
    pub(crate) fn add_package(&mut self, package: Package, state: PState) -> PackageId {
        let key = package.key();
        let name = package.name.clone();
        let is_npm = matches!(package.resolution, Resolution::Npm { .. });
        let id = self.graph.push(package);
        self.st.push(state);
        if let Some(parents) = &mut self.parents {
            parents.push(Vec::new());
        }
        self.by_key.insert(key, id);
        if is_npm {
            let st = &self.st;
            let list = self.by_name.entry(name.clone()).or_default();
            insert_sorted(list, id, &|x: PackageId| st[x.index()].version.clone());
        } else {
            self.by_name.entry(name.clone()).or_default().push(id);
        }
        self.emit_progress(&name);
        id
    }

    /// Records the edge's target and propagates "needed" (fetch) state.
    pub(crate) fn set_target(&mut self, row: Row, target: PackageId) {
        let edge = &mut self.graph.packages[row.pkg.index()].dependencies[row.edge as usize];
        edge.target = Some(target);
        if self.st[target.index()].kind == Kind::Project && !edge.behavior.is_peer() {
            edge.behavior |= Behavior::WORKSPACE;
        }
        let behavior = edge.behavior;
        if let Some(parents) = &mut self.parents {
            parents[target.index()].push((row.pkg, behavior.is_peer()));
        }
        if let Some(&i) = self.requested_rows.get(&(row.pkg, row.edge)) {
            self.request_targets[i] = Some(target);
        }
        if !behavior.is_optional() && !self.st[row.pkg.index()].optional_only {
            self.mark_required(target);
        }
        if self.st[row.pkg.index()].needed && self.follows(row.pkg, behavior, target) {
            self.mark_needed(target);
        }
    }

    /// Whether the fetch phase needs `target` because of this edge.
    fn follows(&self, owner: PackageId, behavior: Behavior, target: PackageId) -> bool {
        let omit = &self.cfg.omit;
        if (omit.dev && behavior.is_dev())
            || (omit.optional && behavior.is_optional())
            || (omit.peer && behavior.is_peer())
        {
            return false;
        }
        if behavior.is_bundled() && self.st[owner.index()].kind != Kind::Project {
            return false;
        }
        let p = &self.graph.packages[target.index()];
        p.platform
            .matches_for_edge(&self.cfg.platform, behavior.is_optional())
    }

    /// The reverse edges of the current graph.
    pub(crate) fn build_parents(&self) -> Vec<Vec<(PackageId, bool)>> {
        let mut parents: Vec<Vec<(PackageId, bool)>> = vec![Vec::new(); self.graph.len()];
        for (i, p) in self.graph.packages.iter().enumerate() {
            for e in &p.dependencies {
                if let Some(t) = e.target {
                    parents[t.index()].push((PackageId(i as u32), e.behavior.is_peer()));
                }
            }
        }
        parents
    }

    /// Builds the reverse-edge index once; [`Engine::set_target`] maintains it afterwards.
    pub(crate) fn ensure_parents(&mut self) {
        if self.parents.is_none() {
            self.parents = Some(self.build_parents());
        }
    }

    /// `id` is reachable through required edges: failures below it are fatal again.
    fn mark_required(&mut self, id: PackageId) {
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            if !std::mem::replace(&mut self.st[id.index()].optional_only, false) {
                continue;
            }
            for e in &self.graph.packages[id.index()].dependencies {
                if let Some(t) = e.target
                    && !e.behavior.is_optional()
                    && self.st[t.index()].optional_only
                {
                    stack.push(t);
                }
            }
        }
    }

    pub(crate) fn mark_needed(&mut self, id: PackageId) {
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            if self.st[id.index()].needed {
                continue;
            }
            self.st[id.index()].needed = true;
            let p = &self.graph.packages[id.index()];
            if !self.st[id.index()].emitted
                && p.resolution.is_storable()
                && !self.st[id.index()].in_bundle
            {
                self.st[id.index()].emitted = true;
                if let Some(tx) = &self.tx {
                    let _ = tx.send(Resolved::of(id, p));
                }
            }
            let edges: Vec<(Behavior, PackageId)> = p
                .dependencies
                .iter()
                .filter_map(|e| e.target.map(|t| (e.behavior, t)))
                .collect();
            for (behavior, target) in edges {
                if !self.st[target.index()].needed && self.follows(id, behavior, target) {
                    stack.push(target);
                }
            }
        }
    }

    /// Requests a manifest unless it is known or in flight. `speculative` requests (look-ahead,
    /// hints) do not count for the saved request order.
    pub(crate) fn request_manifest(&mut self, name: &str, revalidate: bool, speculative: bool) {
        if !speculative && self.manifest_ordered.insert(name.to_owned()) {
            self.manifest_order.push(name.to_owned());
        }
        if self.manifest_state.contains_key(name) {
            return;
        }
        if !fnpm_types::name::is_safe_install_folder_name(name)
            && !fnpm_types::name::is_npm_package_name(name)
        {
            return;
        }
        self.manifest_state.insert(name.to_owned(), MState::Pending);
        self.lookahead_seen.insert(name.to_owned());
        let request = ManifestRequest {
            full: self.gate.is_some(),
            revalidate,
        };
        let fut = self.manifests.manifest(name, request);
        let tx = self.done_tx.clone();
        let name = name.to_owned();
        self.in_flight += 1;
        tokio::spawn(async move {
            let result = fut.await;
            let _ = tx.send(Completion::Manifest(name, result));
        });
    }

    /// Waits for one background completion and records it.
    pub(crate) async fn wait_one(&mut self) -> bool {
        if self.in_flight == 0 {
            return false;
        }
        let Some(done) = self.done_rx.recv().await else {
            return false;
        };
        self.in_flight -= 1;
        self.complete(done);
        while let Ok(done) = self.done_rx.try_recv() {
            self.in_flight -= 1;
            self.complete(done);
        }
        true
    }

    fn complete(&mut self, done: Completion) {
        match done {
            Completion::Manifest(name, result) => {
                let state = match result {
                    Ok(m) => MState::Ready(m),
                    Err(e) => MState::Failed(Arc::new(e)),
                };
                let ready = matches!(state, MState::Ready(_));
                self.manifest_state.insert(name.clone(), state);
                if ready {
                    self.on_manifest(&name);
                }
            }
            Completion::Remote(key, result) => {
                let state = match result {
                    Ok(r) => RState::Ready(r),
                    Err(e) => RState::Failed(Arc::new(e)),
                };
                self.remote.insert(key, state);
            }
        }
    }

    pub(crate) async fn run(mut self) -> Result<EngineOutput> {
        self.drive().await?;
        // Peer warnings, collapsing and pruning are CPU work over the whole graph: keep them off
        // the async workers when the runtime allows it.
        let multi_thread = tokio::runtime::Handle::try_current()
            .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
        if multi_thread && self.graph.len() > 256 {
            tokio::task::block_in_place(|| self.finish())
        } else {
            self.finish()
        }
    }

    async fn drive(&mut self) -> Result<()> {
        loop {
            while let Some(&row) = self.queue.front() {
                match self.bind(row)? {
                    bind::Step::Done => {
                        self.queue.pop_front();
                    }
                    bind::Step::WaitManifest(name) => {
                        while matches!(self.manifest_state.get(&name), Some(MState::Pending)) {
                            if !self.wait_one().await {
                                break;
                            }
                        }
                        if matches!(self.manifest_state.get(&name), Some(MState::Pending) | None) {
                            // Nothing is in flight for it any more; record a failure so the row ends.
                            self.manifest_state.insert(
                                name.clone(),
                                MState::Failed(Arc::new(fnpm_registry::Error::NotFound(
                                    name.clone(),
                                ))),
                            );
                        }
                    }
                }
            }
            if let Some(key) = self.remote_turns.pop_front() {
                while matches!(self.remote.get(&key), Some(RState::Pending)) {
                    if !self.wait_one().await {
                        break;
                    }
                }
                self.remote_turn(&key)?;
                continue;
            }
            if !self.deferred_peers.is_empty() {
                self.start_peer_batch();
                continue;
            }
            break;
        }
        Ok(())
    }
}

impl PState {
    pub(crate) fn project(version: Option<Version>, prev: Option<PackageId>, dir: PathBuf) -> Self {
        Self {
            version,
            prev,
            loaded: false,
            pinned: false,
            kind: Kind::Project,
            dir: Some(dir),
            local_declarer: true,
            optional_only: false,
            in_bundle: false,
            needed: false,
            emitted: false,
        }
    }

    pub(crate) fn new(kind: Kind, version: Option<Version>) -> Self {
        Self {
            version,
            prev: None,
            loaded: false,
            pinned: false,
            kind,
            dir: None,
            local_declarer: false,
            optional_only: true,
            in_bundle: false,
            needed: false,
            emitted: false,
        }
    }
}
