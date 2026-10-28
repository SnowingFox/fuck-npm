//! Where the resolver gets its data from.
//!
//! [`ManifestSource`] serves registry manifests and [`PackageFetcher`] everything else: git and
//! GitHub commit resolution, the `package.json` of git / tarball packages, and local folders.
//! Production uses [`RegistrySource`] (backed by `fnpm-registry` and `fnpm-store`); tests plug in
//! in-memory fakes so the resolver is exercised without a network.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use fnpm_config::Config;
use fnpm_registry::{ManifestRequest, PackageManifest, RegistryClient};
use fnpm_types::spec::Committish;
use fnpm_types::{HashAlgorithm, Integrity, Resolution};
use fnpm_workspace::PackageJson;
use parking_lot::Mutex;

/// A boxed `Send` future with no borrows, so it can be spawned on the runtime.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Registry manifests. Implemented by [`RegistrySource`] over [`RegistryClient`].
pub trait ManifestSource: Send + Sync {
    /// The manifest for `name`. The future must not borrow `self`; it is spawned so that many
    /// manifests are in flight at once.
    fn manifest(
        &self,
        name: &str,
        request: ManifestRequest,
    ) -> BoxFuture<fnpm_registry::Result<Arc<PackageManifest>>>;

    /// Starts background fetches whose results a later [`ManifestSource::manifest`] call picks
    /// up. The default does nothing.
    fn prefetch(&self, names: Vec<String>) {
        let _ = names;
    }

    /// `<registry>/<name>/-/<unscoped>-<version>.tgz` for the registry that serves `name`. A
    /// tarball at this URL is recorded as the default location (`tarball_url: None`), which keeps
    /// lockfiles portable across mirrors.
    fn default_tarball_url(&self, name: &str, version: &str) -> String;

    /// The URL the manifest of `name` is requested from, for bun's `GET <url> - 404`.
    fn manifest_url(&self, name: &str) -> Option<String> {
        let _ = name;
        None
    }

    /// A `--verbose` line about a resolution decision. The default drops it.
    fn verbose_line(&self, line: String) {
        let _ = line;
    }
}

/// A git / tarball package after it was downloaded (or read from disk).
#[derive(Debug, Clone)]
pub struct FetchedPackage {
    /// Its `package.json`.
    pub manifest: PackageJson,
    /// The integrity of the downloaded bytes, when known.
    pub integrity: Option<Integrity>,
}

/// What to fetch for a git, GitHub or tarball resolution.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// The dependency name (the package name is only known from its `package.json`).
    pub name: String,
    /// The source to fetch.
    pub resolution: Resolution,
    /// For [`Resolution::LocalTarball`]: the absolute path of the file.
    pub local_path: Option<PathBuf>,
    /// For [`Resolution::Git`]: a `#::path:` subdirectory of the repository.
    pub subdir: Option<String>,
}

/// Everything that is not a registry manifest.
pub trait PackageFetcher: Send + Sync {
    /// `git ls-remote` style commit resolution of `url#committish`.
    fn resolve_git(
        &self,
        url: &str,
        committish: &Committish,
    ) -> BoxFuture<fnpm_registry::Result<String>>;

    /// GitHub `owner/repo#committish` to a commit SHA or validated archive root tag.
    fn resolve_github(
        &self,
        owner: &str,
        repo: &str,
        committish: &str,
    ) -> BoxFuture<fnpm_registry::Result<String>>;

    /// Downloads (or reads) a git, GitHub or tarball package and returns its `package.json`.
    /// Implementations may import the content into the store so the fetch phase finds it there.
    fn fetch(&self, request: FetchRequest) -> BoxFuture<fnpm_registry::Result<FetchedPackage>>;

    /// `<dir>/package.json`, or `None` when the directory has none.
    fn read_package_json(&self, dir: &Path) -> fnpm_registry::Result<Option<PackageJson>>;
}

type GithubDownloads = HashMap<(String, String, String), FetchedPackage>;

/// The production sources: manifests from [`RegistryClient`], git through `fnpm_registry::git`,
/// tarballs downloaded and imported into the store so the fetch phase does not download them
/// again.
#[derive(Clone)]
pub struct RegistrySource {
    client: RegistryClient,
    config: Arc<Config>,
    store: Arc<Mutex<Option<fnpm_store::Store>>>,
    github: Arc<Mutex<GithubDownloads>>,
}

impl RegistrySource {
    /// Sources over `client`; the store under `config.store_dir` is opened on first use.
    pub fn new(config: Arc<Config>, client: RegistryClient) -> Self {
        Self {
            client,
            config,
            store: Arc::new(Mutex::new(None)),
            github: Arc::default(),
        }
    }

    fn store(&self) -> Option<fnpm_store::Store> {
        let mut slot = self.store.lock();
        if slot.is_none() {
            *slot = fnpm_store::Store::open(&self.config.store_dir).ok();
        }
        slot.clone()
    }

    /// Reads the package.json out of the tarball, records its integrity and imports it into the
    /// store under its real name and integrity (non-registry keys omit the version).
    /// A failed import is not fatal: the fetch phase downloads the tarball again.
    fn finish_tarball(
        &self,
        origin: &str,
        bytes: &[u8],
        fallback_name: &str,
    ) -> fnpm_registry::Result<FetchedPackage> {
        let text = crate::tarball::package_json_from_tgz(bytes).map_err(|message| {
            fnpm_registry::Error::InvalidManifest {
                name: origin.to_owned(),
                message,
            }
        })?;
        let manifest = parse_package_json(&text, origin)?;
        let integrity = Integrity::compute(HashAlgorithm::Sha512, bytes);
        // Under the name the package gets in the graph (`local::package_from_json`), which the
        // fetch looks it up by.
        let name = manifest
            .name
            .clone()
            .filter(|n| fnpm_types::name::is_safe_lockfile_package_name(n))
            .unwrap_or_else(|| fallback_name.to_owned());
        if let Some(store) = self.store() {
            let key = fnpm_store::PackageKey {
                name,
                version: String::new(),
                integrity,
            };
            let _ = store.import_tarball_bytes(&key, bytes, fnpm_store::ImportOptions::default());
        }
        Ok(FetchedPackage {
            manifest,
            integrity: Some(integrity),
        })
    }
}

impl ManifestSource for RegistrySource {
    fn manifest(
        &self,
        name: &str,
        request: ManifestRequest,
    ) -> BoxFuture<fnpm_registry::Result<Arc<PackageManifest>>> {
        let client = self.client.clone();
        let name = name.to_owned();
        Box::pin(async move { client.manifest(&name, request).await })
    }

    fn prefetch(&self, names: Vec<String>) {
        self.client.prefetch(names);
    }

    fn default_tarball_url(&self, name: &str, version: &str) -> String {
        self.client.default_tarball_url(name, version)
    }

    fn manifest_url(&self, name: &str) -> Option<String> {
        self.client.manifest_url(name)
    }

    fn verbose_line(&self, line: String) {
        self.client.verbose_notice(|| line);
    }
}

fn io_error(message: impl Into<String>) -> fnpm_registry::Error {
    fnpm_registry::Error::Io(std::io::Error::other(message.into()))
}

/// Parses `package.json` text read from a tarball or a checkout.
fn parse_package_json(text: &str, origin: &str) -> fnpm_registry::Result<PackageJson> {
    PackageJson::parse(text).map_err(|e| fnpm_registry::Error::InvalidManifest {
        name: origin.to_owned(),
        message: e.to_string(),
    })
}

impl PackageFetcher for RegistrySource {
    fn resolve_git(
        &self,
        url: &str,
        committish: &Committish,
    ) -> BoxFuture<fnpm_registry::Result<String>> {
        let url = url.to_owned();
        let committish = committish.clone();
        Box::pin(async move { fnpm_registry::git::resolve_commit(&url, &committish).await })
    }

    fn resolve_github(
        &self,
        owner: &str,
        repo: &str,
        committish: &str,
    ) -> BoxFuture<fnpm_registry::Result<String>> {
        let this = self.clone();
        let (owner, repo, committish) = (owner.to_owned(), repo.to_owned(), committish.to_owned());
        Box::pin(async move {
            let url = this.client.github_tarball_url(&owner, &repo, &committish);
            let bytes = this.client.tarball_bytes(&url).await?;
            tokio::task::spawn_blocking(move || {
                // The whole body is read to name the commit after the archive's root directory.
                this.client.verbose_notice(|| {
                    let name = crate::tarball::package_json_from_tgz(&bytes)
                        .ok()
                        .and_then(|text| parse_package_json(&text, &url).ok()?.name)
                        .unwrap_or_else(|| repo.clone());
                    format!(
                        "[{}] Extract {} ({} tgz file)",
                        fnpm_types::text::escape_control_chars(&name),
                        fnpm_types::text::escape_control_chars(
                            url.rsplit('/').next().unwrap_or(&url)
                        ),
                        fnpm_types::text::format_size(bytes.len() as u64)
                    )
                });
                let tag = crate::tarball::github_tag_from_tgz(&bytes).map_err(|message| {
                    fnpm_registry::Error::InvalidManifest {
                        name: url.clone(),
                        message,
                    }
                })?;
                let fallback = fnpm_types::name::fallback_package_name(&repo).to_owned();
                let package = this.finish_tarball(&url, &bytes, &fallback)?;
                this.github
                    .lock()
                    .insert((owner, repo, tag.clone()), package);
                Ok(tag)
            })
            .await
            .map_err(|e| io_error(e.to_string()))?
        })
    }

    fn fetch(&self, request: FetchRequest) -> BoxFuture<fnpm_registry::Result<FetchedPackage>> {
        let this = self.clone();
        Box::pin(async move {
            let bytes = match &request.resolution {
                Resolution::RemoteTarball { url, .. } => {
                    this.client.tarball_bytes(url).await?.to_vec()
                }
                Resolution::Github {
                    owner,
                    repo,
                    resolved_commit,
                    ..
                } => {
                    let key = (owner.clone(), repo.clone(), resolved_commit.clone());
                    if let Some(package) = this.github.lock().get(&key).cloned() {
                        return Ok(package);
                    }
                    let url = this.client.github_tarball_url(owner, repo, resolved_commit);
                    this.client.tarball_bytes(&url).await?.to_vec()
                }
                Resolution::LocalTarball { path, .. } => {
                    let file = request
                        .local_path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(path));
                    tokio::fs::read(&file).await?
                }
                Resolution::Git {
                    url,
                    resolved_commit,
                    ..
                } => {
                    return this.fetch_git(&request, url, resolved_commit).await;
                }
                other => return Err(io_error(format!("cannot fetch a {other} resolution"))),
            };
            let fallback = crate::local::unnamed_package_name(&request.resolution);
            tokio::task::spawn_blocking(move || {
                this.finish_tarball(&request.name, &bytes, &fallback)
            })
            .await
            .map_err(|e| io_error(e.to_string()))?
        })
    }

    fn read_package_json(&self, dir: &Path) -> fnpm_registry::Result<Option<PackageJson>> {
        read_package_json_at(dir)
    }
}

impl RegistrySource {
    async fn fetch_git(
        &self,
        request: &FetchRequest,
        url: &str,
        sha: &str,
    ) -> fnpm_registry::Result<FetchedPackage> {
        static CHECKOUTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // One directory per checkout: two dependencies on the same commit (`#main` and `#<sha>`)
        // are fetched concurrently and must not delete each other's work.
        let n = CHECKOUTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest = self
            .config
            .cache_dir
            .join("tmp")
            .join(format!("git-{sha}-{}-{n}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dest).await;
        fnpm_registry::git::checkout(
            &self.config.cache_dir,
            url,
            sha,
            request.subdir.as_deref(),
            &dest,
        )
        .await?;
        let result = async {
            let text = tokio::fs::read_to_string(dest.join("package.json")).await?;
            let manifest = parse_package_json(&text, &request.name)?;
            // Hashing and importing the checkout is blocking filesystem work.
            let (this, dir) = (self.clone(), dest.clone());
            tokio::task::spawn_blocking(move || {
                let integrity = fnpm_store::Store::pack_integrity(&dir).ok();
                if let (Some(store), Some(name), Some(integrity)) =
                    (this.store(), manifest.name.clone(), integrity)
                {
                    let key = fnpm_store::PackageKey {
                        name,
                        version: String::new(),
                        integrity,
                    };
                    let _ = store.import_directory(&key, &dir);
                }
                FetchedPackage {
                    manifest,
                    integrity,
                }
            })
            .await
            .map_err(|e| io_error(e.to_string()))
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&dest).await;
        result
    }
}

/// Reads `<dir>/package.json` with [`PackageJson::parse`]; a missing file is `Ok(None)`.
pub fn read_package_json_at(dir: &Path) -> fnpm_registry::Result<Option<PackageJson>> {
    let path = dir.join("package.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_package_json(&text, &path.to_string_lossy()).map(Some),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        // bun names the errno: `error: ENAMETOOLONG`.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidFilename => Err(fnpm_registry::Error::Io(
            std::io::Error::new(e.kind(), "ENAMETOOLONG"),
        )),
        Err(e) => Err(fnpm_registry::Error::Io(e)),
    }
}

/// Clones a registry error (they hold no shared state) so one failure can be reported to every
/// dependency that waited on the same request.
pub(crate) fn clone_registry_error(e: &fnpm_registry::Error) -> fnpm_registry::Error {
    use fnpm_registry::Error as E;
    match e {
        E::Status { url, status } => E::Status {
            url: url.clone(),
            status: *status,
        },
        E::NotFound(n) => E::NotFound(n.clone()),
        E::Network { url, message } => E::Network {
            url: url.clone(),
            message: message.clone(),
        },
        E::Offline(n) => E::Offline(n.clone()),
        E::InvalidManifest { name, message } => E::InvalidManifest {
            name: name.clone(),
            message: message.clone(),
        },
        E::Integrity {
            url,
            expected,
            actual,
        } => E::Integrity {
            url: url.clone(),
            expected: expected.clone(),
            actual: actual.clone(),
        },
        E::Git(m) => E::Git(m.clone()),
        E::Io(io) => E::Io(std::io::Error::new(io.kind(), io.to_string())),
        E::Registry {
            url,
            status,
            message,
        } => E::Registry {
            url: url.clone(),
            status: *status,
            message: message.clone(),
        },
        E::NeedAuth(m) => E::NeedAuth(m.clone()),
        E::WebLogin(m) => E::WebLogin(m.clone()),
        E::InvalidResponse { url, message } => E::InvalidResponse {
            url: url.clone(),
            message: message.clone(),
        },
    }
}
