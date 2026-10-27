//! In-memory fakes and fixture builders for resolver tests: a registry, a git / tarball
//! fetcher, package.json and workspace builders, and a default config.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fnpm_config::{
    Auth, Config, LinkerSetting, LockfileConfig, LogLevel, ManifestCacheMode, NetworkConfig,
    OfflineMode, Omit, ProxyConfig, RegistryScope, TlsConfig,
};
use fnpm_registry::{Dist, ManifestRequest, PackageManifest, VersionManifest};
use fnpm_resolver::{
    BoxFuture, FetchRequest, FetchedPackage, ManifestSource, Mode, PackageFetcher, Request,
    ResolveInput, ResolveOutput, Resolved, Resolver,
};
use fnpm_semver::Version;
use fnpm_types::spec::Committish;
use fnpm_types::{
    Bin, DependencyGroup, HashAlgorithm, Integrity, LockGraph, Package, PackageId, PeerMeta,
    Platform, PlatformConstraints, Resolution, event::VecSink,
};
use fnpm_workspace::{Member, PackageJson, Workspace, WorkspaceSettings};
use parking_lot::Mutex;
use serde_json::{Value, json};

pub const REGISTRY: &str = "https://registry.test/";

pub fn v(s: &str) -> Version {
    Version::parse(s).unwrap_or_else(|e| panic!("bad version {s}: {e}"))
}

fn pairs(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn peer_meta(value: Option<&Value>) -> BTreeMap<String, PeerMeta> {
    value
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        PeerMeta {
                            optional: v.get("optional").and_then(Value::as_bool).unwrap_or(false),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn platform(value: &Value) -> PlatformConstraints {
    PlatformConstraints::from_package_json(
        &strings(value.get("os")),
        &strings(value.get("cpu")),
        &strings(value.get("libc")),
    )
}

fn bin(value: &Value, name: &str) -> Bin {
    match value.get("bin") {
        Some(Value::String(path)) => Bin::NamedFile {
            name: name.rsplit('/').next().unwrap_or(name).to_owned(),
            path: path.clone(),
        },
        Some(Value::Object(map)) => Bin::from_map(
            map.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())),
        ),
        _ => Bin::None,
    }
}

/// A `PackageJson` from JSON, filled the way `PackageJson::parse` does.
pub fn pj(value: Value) -> PackageJson {
    PackageJson {
        name: value.get("name").and_then(Value::as_str).map(str::to_owned),
        version: value
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        dependencies: pairs(value.get("dependencies")),
        dev_dependencies: pairs(value.get("devDependencies")),
        optional_dependencies: pairs(value.get("optionalDependencies")),
        peer_dependencies: pairs(value.get("peerDependencies")),
        peer_dependencies_meta: peer_meta(value.get("peerDependenciesMeta")),
        bundle_dependencies: strings(
            value
                .get("bundleDependencies")
                .or_else(|| value.get("bundledDependencies")),
        ),
        workspaces: match value.get("workspaces") {
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            ),
            Some(Value::Object(o)) => Some(strings(o.get("packages"))),
            _ => None,
        },
        bin: bin(
            &value,
            value.get("name").and_then(Value::as_str).unwrap_or(""),
        ),
        scripts: value
            .get("scripts")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
        platform: platform(&value),
        engines: BTreeMap::new(),
        raw: value,
    }
}

/// A project: the root package.json plus members `(path, package.json)`.
pub fn workspace(root: Value, members: &[(&str, Value)]) -> Workspace {
    workspace_at(PathBuf::from("/project"), root, members)
}

pub fn workspace_at(root_dir: PathBuf, root: Value, members: &[(&str, Value)]) -> Workspace {
    let mut list: Vec<Member> = members
        .iter()
        .map(|(path, value)| {
            let manifest = pj(value.clone());
            Member {
                name: manifest
                    .name
                    .clone()
                    .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_owned()),
                version: manifest.version.clone().unwrap_or_default(),
                path: (*path).to_owned(),
                dir: root_dir.join(path),
                manifest,
            }
        })
        .collect();
    list.sort_by(|a, b| a.path.cmp(&b.path));
    Workspace {
        root_dir,
        root: pj(root),
        members: list,
        settings: WorkspaceSettings::default(),
    }
}

pub fn config() -> Config {
    let dir = PathBuf::from("/tmp/fnpm-resolver-tests");
    Config {
        cwd: PathBuf::from("/project"),
        project_dir: PathBuf::from("/project"),
        home_dir: dir.join("home"),
        store_dir: dir.join("store"),
        cache_dir: dir.join("cache"),
        global_dir: dir.join("global"),
        global_bin_dir: dir.join("bin"),
        registry: RegistryScope {
            url: REGISTRY.into(),
            auth: Auth::None,
            email: None,
            cert_file: None,
            key_file: None,
        },
        scopes: BTreeMap::new(),
        url_auth: Vec::new(),
        network: NetworkConfig {
            concurrency: 64,
            retries: 0,
            retry_backoff_ms: 0,
            idle_timeout_secs: 1,
            offline: OfflineMode::Online,
            manifest_cache: ManifestCacheMode::Disabled,
            proxy: ProxyConfig::default(),
            tls: TlsConfig::default(),
            user_agent: "fnpm-test".into(),
        },
        linker: LinkerSetting::Auto,
        backend: None,
        global_store: false,
        hoist: true,
        hoist_pattern: Vec::new(),
        public_hoist_pattern: Vec::new(),
        link_workspace_packages: true,
        frozen_lockfile: false,
        production: false,
        dry_run: false,
        force: false,
        exact: false,
        ignore_scripts: false,
        verify_integrity: true,
        save_text_lockfile: None,
        lockfile: LockfileConfig {
            save: true,
            print_yarn: false,
            path: None,
            save_path: None,
            force_save: false,
        },
        omit: Omit::default(),
        auto_install_peers: true,
        concurrent_scripts: 4,
        minimum_release_age_ms: None,
        minimum_release_age_excludes: Vec::new(),
        platform: Platform::current(),
        security_scanner: None,
        log_level: LogLevel::Default,
        ci: false,
        // Fields the test does not care about keep the crate's defaults.
        ..Config::defaults(Path::new("/project"))
    }
}

fn integrity_for(name: &str, version: &str) -> Integrity {
    Integrity::compute(
        HashAlgorithm::Sha512,
        format!("{name}@{version}").as_bytes(),
    )
}

pub fn default_tarball(name: &str, version: &str) -> String {
    let unscoped = name.rsplit('/').next().unwrap_or(name);
    format!("{REGISTRY}{name}/-/{unscoped}-{version}.tgz")
}

/// A version manifest from JSON (`dependencies`, `optionalDependencies`, `peerDependencies`,
/// `peerDependenciesMeta`, `bundleDependencies`, `os`/`cpu`/`libc`, `deprecated`,
/// `hasInstallScript`, `bin`, `tarball`).
pub fn version_manifest(name: &str, version: &str, value: &Value) -> VersionManifest {
    VersionManifest {
        version: v(version),
        dependencies: pairs(value.get("dependencies")),
        optional_dependencies: pairs(value.get("optionalDependencies")),
        peer_dependencies: pairs(value.get("peerDependencies")),
        peer_dependencies_meta: peer_meta(value.get("peerDependenciesMeta")),
        bundle_dependencies: strings(value.get("bundleDependencies")),
        bin: bin(value, name),
        platform: platform(value),
        dist: Dist {
            tarball: value
                .get("tarball")
                .and_then(Value::as_str)
                .map_or_else(|| default_tarball(name, version), str::to_owned),
            integrity: Some(integrity_for(name, version)),
            file_count: None,
            unpacked_size: None,
        },
        has_install_script: value
            .get("hasInstallScript")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        deprecated: value
            .get("deprecated")
            .and_then(Value::as_str)
            .map(str::to_owned),
        engines: BTreeMap::new(),
        scripts: BTreeMap::new(),
        license: None,
    }
}

/// A packument: `versions` maps version -> version JSON; `latest` defaults to the highest
/// release; `time` maps version -> publish ms.
pub fn packument(
    name: &str,
    versions: &[(&str, Value)],
    tags: &[(&str, &str)],
    time: &[(&str, u64)],
) -> PackageManifest {
    let mut list: Vec<VersionManifest> = versions
        .iter()
        .map(|(ver, val)| version_manifest(name, ver, val))
        .collect();
    list.sort_by(|a, b| a.version.cmp(&b.version));
    let mut dist_tags: BTreeMap<String, String> = tags
        .iter()
        .map(|(t, ver)| ((*t).to_owned(), (*ver).to_owned()))
        .collect();
    if !dist_tags.contains_key("latest")
        && let Some(highest) = list.iter().rev().find(|vm| !vm.version.is_prerelease())
    {
        dist_tags.insert("latest".into(), highest.version.to_string());
    }
    PackageManifest {
        name: name.into(),
        dist_tags,
        versions: list,
        time: time
            .iter()
            .map(|(ver, ms)| ((*ver).to_owned(), *ms))
            .collect(),
        modified: None,
        etag: None,
        last_modified: None,
        fetched_at_ms: 0,
        max_age_secs: 300,
        is_full: true,
    }
}

/// The fake registry.
#[derive(Default)]
pub struct FakeRegistry {
    pub manifests: Mutex<HashMap<String, Arc<PackageManifest>>>,
    pub log: Mutex<Vec<(String, ManifestRequest)>>,
    pub in_flight: Arc<AtomicUsize>,
    pub max_in_flight: Arc<AtomicUsize>,
    pub delay_ms: u64,
    /// Names whose request fails with a network error.
    pub broken: Mutex<Vec<String>>,
}

impl FakeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_delay(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            ..Self::default()
        }
    }

    /// Adds a package with `versions` (version, JSON) and the default `latest`.
    pub fn add(&self, name: &str, versions: &[(&str, Value)]) -> &Self {
        self.add_full(packument(name, versions, &[], &[]))
    }

    pub fn add_full(&self, manifest: PackageManifest) -> &Self {
        self.manifests
            .lock()
            .insert(manifest.name.clone(), Arc::new(manifest));
        self
    }

    pub fn requests(&self) -> Vec<String> {
        self.log.lock().iter().map(|(n, _)| n.clone()).collect()
    }

    pub fn request_count(&self, name: &str) -> usize {
        self.log.lock().iter().filter(|(n, _)| n == name).count()
    }
}

impl ManifestSource for FakeRegistry {
    fn manifest(
        &self,
        name: &str,
        request: ManifestRequest,
    ) -> BoxFuture<fnpm_registry::Result<Arc<PackageManifest>>> {
        self.log.lock().push((name.to_owned(), request));
        let result = if self.broken.lock().iter().any(|b| b == name) {
            Err(fnpm_registry::Error::Network {
                url: format!("{REGISTRY}{name}"),
                message: "connection reset".into(),
            })
        } else {
            self.manifests
                .lock()
                .get(name)
                .cloned()
                .ok_or_else(|| fnpm_registry::Error::NotFound(name.to_owned()))
        };
        let delay = self.delay_ms;
        let (in_flight, max) = (self.in_flight.clone(), self.max_in_flight.clone());
        Box::pin(async move {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            max.fetch_max(now, Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }

    fn default_tarball_url(&self, name: &str, version: &str) -> String {
        default_tarball(name, version)
    }
}

/// The fake git / tarball / folder source.
#[derive(Default)]
pub struct FakeFetcher {
    /// `url#committish-ref` (ref `""` for HEAD) -> sha.
    pub commits: Mutex<HashMap<String, String>>,
    /// Key -> package.json, where the key is a tarball URL, `git:<url>#<sha>`,
    /// `github:<owner>/<repo>#<sha>` or the absolute path of a local tarball.
    pub packages: Mutex<HashMap<String, Value>>,
    /// Absolute folder -> package.json.
    pub folders: Mutex<HashMap<PathBuf, Value>>,
    pub fetches: Mutex<Vec<String>>,
}

impl FakeFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn commit(&self, url: &str, reference: &str, sha: &str) -> &Self {
        self.commits
            .lock()
            .insert(format!("{url}#{reference}"), sha.into());
        self
    }

    pub fn package(&self, key: &str, json: Value) -> &Self {
        self.packages.lock().insert(key.into(), json);
        self
    }

    pub fn folder(&self, dir: impl Into<PathBuf>, json: Value) -> &Self {
        self.folders.lock().insert(dir.into(), json);
        self
    }
}

impl PackageFetcher for FakeFetcher {
    fn resolve_git(
        &self,
        url: &str,
        committish: &Committish,
    ) -> BoxFuture<fnpm_registry::Result<String>> {
        let key = format!("{url}#{}", committish.reference.clone().unwrap_or_default());
        let found = self
            .commits
            .lock()
            .get(&key)
            .cloned()
            .ok_or_else(|| fnpm_registry::Error::Git(format!("no ref {key}")));
        Box::pin(async move { found })
    }

    fn resolve_github(
        &self,
        owner: &str,
        repo: &str,
        committish: &str,
    ) -> BoxFuture<fnpm_registry::Result<String>> {
        let key = format!("github:{owner}/{repo}#{committish}");
        let found = self
            .commits
            .lock()
            .get(&key)
            .cloned()
            .ok_or_else(|| fnpm_registry::Error::NotFound(key.clone()));
        Box::pin(async move { found })
    }

    fn fetch(&self, request: FetchRequest) -> BoxFuture<fnpm_registry::Result<FetchedPackage>> {
        let key = match &request.resolution {
            Resolution::RemoteTarball { url, .. } => url.clone(),
            Resolution::Git {
                url,
                resolved_commit,
                ..
            } => format!("git:{url}#{resolved_commit}"),
            Resolution::Github {
                owner,
                repo,
                resolved_commit,
                ..
            } => format!("github:{owner}/{repo}#{resolved_commit}"),
            Resolution::LocalTarball { .. } => request
                .local_path
                .clone()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            other => other.to_string(),
        };
        self.fetches.lock().push(key.clone());
        let found = self.packages.lock().get(&key).cloned();
        Box::pin(async move {
            let json = found.ok_or_else(|| fnpm_registry::Error::NotFound(key.clone()))?;
            Ok(FetchedPackage {
                manifest: pj(json),
                integrity: Some(Integrity::compute(HashAlgorithm::Sha512, key.as_bytes())),
            })
        })
    }

    fn read_package_json(&self, dir: &Path) -> fnpm_registry::Result<Option<PackageJson>> {
        Ok(self.folders.lock().get(dir).cloned().map(pj))
    }
}

/// A resolver plus its fakes.
pub struct Harness {
    pub registry: Arc<FakeRegistry>,
    pub fetcher: Arc<FakeFetcher>,
    pub config: Config,
    /// Holds the cache directory (prefetch hints) for the harness's lifetime.
    pub cache: tempfile::TempDir,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_registry(FakeRegistry::new())
    }

    pub fn with_registry(registry: FakeRegistry) -> Self {
        let cache = tempfile::tempdir().expect("tempdir");
        let mut config = config();
        config.cache_dir = cache.path().to_path_buf();
        Self {
            registry: Arc::new(registry),
            fetcher: Arc::new(FakeFetcher::new()),
            config,
            cache,
        }
    }

    pub fn resolver(&self) -> Resolver {
        Resolver::with_sources(
            Arc::new(self.config.clone()),
            self.registry.clone(),
            self.fetcher.clone(),
        )
    }

    pub async fn run(
        &self,
        ws: &Workspace,
        previous: Option<&LockGraph>,
        mode: Mode,
    ) -> fnpm_resolver::Result<ResolveOutput> {
        self.run_full(ws, previous, mode, Vec::new(), Vec::new())
            .await
            .map(|(o, _)| o)
    }

    pub async fn run_full(
        &self,
        ws: &Workspace,
        previous: Option<&LockGraph>,
        mode: Mode,
        requests: Vec<Request>,
        removals: Vec<(String, String)>,
    ) -> fnpm_resolver::Result<(ResolveOutput, Vec<Resolved>)> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = VecSink::default();
        let out = self
            .resolver()
            .resolve(
                ResolveInput {
                    workspace: ws,
                    previous,
                    mode,
                    requests,
                    removals,
                },
                Some(tx),
                &sink,
            )
            .await
            // The tests look at the failed row; the report of every failed row is the
            // engine's (`Error::Unresolved`), tested where it is built.
            .map_err(|e| match e {
                fnpm_resolver::Error::Unresolved { first, .. } => *first,
                e => e,
            })?;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        Ok((out, events))
    }

    pub async fn install(&self, ws: &Workspace) -> ResolveOutput {
        self.run(ws, None, Mode::Normal)
            .await
            .unwrap_or_else(|e| panic!("resolve failed: {e}"))
    }
}

pub fn request(workspace: &str, name: &str, spec: &str, group: DependencyGroup) -> Request {
    Request {
        workspace: workspace.into(),
        name: name.into(),
        spec: spec.into(),
        group,
    }
}

// ---- graph inspection -----------------------------------------------------------------------

/// `name@resolution` of every package, sorted (the root excluded).
pub fn keys(g: &LockGraph) -> Vec<String> {
    let mut k: Vec<String> = g.packages.iter().skip(1).map(Package::key).collect();
    k.sort();
    k
}

/// The package the edge `name` of `from` points at.
pub fn dep<'g>(g: &'g LockGraph, from: PackageId, name: &str) -> Option<&'g Package> {
    let p = &g.packages[from.index()];
    p.dependencies
        .iter()
        .find(|e| e.name == name && !e.behavior.is_peer())
        .and_then(|e| e.target)
        .map(|t| &g.packages[t.index()])
}

pub fn peer<'g>(g: &'g LockGraph, from: PackageId, name: &str) -> Option<&'g Package> {
    let p = &g.packages[from.index()];
    p.dependencies
        .iter()
        .find(|e| e.name == name && e.behavior.is_peer())
        .and_then(|e| e.target)
        .map(|t| &g.packages[t.index()])
}

/// Version of the root's dependency `name`.
pub fn root_dep(g: &LockGraph, name: &str) -> Option<String> {
    dep(g, PackageId::ROOT, name).map(|p| p.resolution.to_string())
}

/// The id of `name@version`.
pub fn id_of(g: &LockGraph, name: &str, version: &str) -> PackageId {
    g.packages
        .iter()
        .position(|p| p.name == name && p.version == version)
        .map(|i| PackageId(i as u32))
        .unwrap_or_else(|| panic!("{name}@{version} not in graph: {:?}", keys(g)))
}

/// Versions of `name` in the graph, ascending.
pub fn versions(g: &LockGraph, name: &str) -> Vec<String> {
    let mut list: Vec<Version> = g
        .packages
        .iter()
        .filter(|p| p.name == name)
        .filter_map(|p| Version::parse(&p.version).ok())
        .collect();
    list.sort();
    list.iter().map(ToString::to_string).collect()
}

pub fn no_deps() -> Value {
    json!({})
}
