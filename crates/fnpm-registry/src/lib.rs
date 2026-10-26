//! Registry client: HTTP/2 manifests and tarballs, auth, retries, the on-disk manifest cache,
//! GitHub tarball resolution, `git` remote operations, and the audit/publish/whoami/view
//! endpoints. See `docs/ARCHITECTURE.md#fnpm-registry`.
//!
//! One [`RegistryClient`] per command: it owns a single `reqwest` client (rustls, ALPN h2 with
//! HTTP/1.1 fallback, pooled connections), the request limiter (`network.concurrency`,
//! manifests before tarballs, adaptive after repeated failures), the in-flight dedupe of
//! manifest requests and the memo of parsed manifests.

pub mod audit;
mod auth;
pub mod cache;
mod client;
mod date;
pub mod endpoints;
pub mod git;
mod limiter;
pub mod manifest;
mod npm;
mod proxy;
pub mod publish;
mod retry;
mod semver_ext;
mod tarball;
pub mod tls;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use fnpm_config::{Config, Env};
use fnpm_types::{EventSink, Integrity};
use futures_util::stream::BoxStream;

pub use audit::{AuditPackage, AuditResponse, UnauditedRegistry};
pub use auth::redact_url;
pub use cache::ManifestCache;
pub use endpoints::{JsonResponse, github_tarball_tag};
pub use manifest::{
    Dist, PackageManifest, PickOptions, PickResult, ReleaseAgeExcludes, VersionManifest,
    name_suggests_libc,
};
pub use npm::{ACCEPT_ABBREVIATED, ACCEPT_FULL, MAX_MANIFEST_AGE};
pub use publish::{OtpPrompt, PublishOptions, PublishPackage, Published};
pub use tls::CaStore;

/// Errors of registry operations. URLs in messages never carry credentials.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// bun's `GET <url> - <status>`.
    #[error("GET {url} - {status}")]
    Status { url: String, status: u16 },
    #[error("package \"{0}\" not found")]
    NotFound(String),
    #[error("{url}: {message}")]
    Network { url: String, message: String },
    #[error("--offline: no cached manifest for \"{0}\"")]
    Offline(String),
    #[error("invalid manifest for \"{name}\": {message}")]
    InvalidManifest { name: String, message: String },
    #[error("integrity check failed for {url}: expected {expected}, got {actual}")]
    Integrity {
        url: String,
        expected: String,
        actual: String,
    },
    #[error("git: {0}")]
    Git(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An error status with the registry's `error` message (publish, whoami, audit, view).
    #[error("{status} {url}: {message}")]
    Registry {
        url: String,
        status: u16,
        message: String,
    },
    /// The operation needs credentials (or a one-time password) that are not available.
    #[error("{0}")]
    NeedAuth(String),
    /// A 2xx response whose body is not what the endpoint returns.
    #[error("{url}: {message}")]
    InvalidResponse { url: String, message: String },
    /// The web login of a publish answered with a token that cannot be used.
    #[error("WebLogin: {0}")]
    WebLogin(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// bun's name for a download that got no answer (`ConnectionClosed downloading tarball
    /// x@1.0.0`), read from a [`Error::Network`] message; `None` for other errors.
    pub fn network_error_name(&self) -> Option<&'static str> {
        let Error::Network { message, .. } = self else {
            return None;
        };
        let text = message.to_ascii_lowercase();
        Some(if text.contains("timed out") || text.contains("timeout") {
            "Timeout"
        } else if text.contains("refused") {
            "ConnectionRefused"
        } else if text.contains("closed") || text.contains("reset") || text.contains("incomplete") {
            "ConnectionClosed"
        } else {
            "ConnectionFailed"
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManifestRequest {
    /// Fetch the full packument (needed for `time`, `scripts`, `minimumReleaseAge`, `info`).
    /// The default is the abbreviated (corgi) document.
    pub full: bool,
    /// Ignore cache freshness and revalidate (`fnpm update`, `outdated`).
    pub revalidate: bool,
}

/// A tarball response body. Bytes arrive as the network delivers them; a connection that breaks
/// mid-body is resumed transparently.
pub struct TarballBody {
    /// The request URL (without userinfo).
    pub url: String,
    pub content_length: Option<u64>,
    pub stream: BoxStream<'static, Result<Bytes>>,
}

impl std::fmt::Debug for TarballBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TarballBody")
            .field("url", &self.url)
            .field("content_length", &self.content_length)
            .finish_non_exhaustive()
    }
}

/// Counters for `--verbose` and the benchmark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegistryStats {
    /// Manifest requests sent (not counting retries).
    pub manifest_requests: u64,
    /// Manifests served from the disk cache without a request.
    pub manifest_cache_hits: u64,
    /// 304 responses.
    pub not_modified: u64,
    pub tarball_requests: u64,
    pub bytes_downloaded: u64,
    pub retries: u64,
}

/// Cheap to clone (an `Arc` inside). Requests for the same manifest are deduplicated while in
/// flight; parsed manifests are memoised for the life of the client.
#[derive(Clone)]
pub struct RegistryClient {
    inner: Arc<client::Inner>,
}

impl RegistryClient {
    /// `--offline` / `--prefer-offline` / online.
    pub fn offline_mode(&self) -> fnpm_config::OfflineMode {
        self.inner.offline
    }
}

impl std::fmt::Debug for RegistryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryClient")
            .field("registry", &self.inner.scopes.default.href())
            .finish_non_exhaustive()
    }
}

impl RegistryClient {
    /// Builds the HTTP/2 client from `config.network` (TLS, proxy, concurrency limit) and
    /// opens the manifest cache under `config.cache_dir`. Must be called inside a tokio
    /// runtime. Proxy, TLS and GitHub environment variables come from the process.
    pub fn new(config: Arc<Config>) -> Result<Self> {
        Self::with_env(config, Env::from_process())
    }

    /// [`new`](Self::new) with an explicit environment snapshot (`HTTPS_PROXY`, `NO_PROXY`,
    /// `NODE_EXTRA_CA_CERTS`, `NODE_TLS_REJECT_UNAUTHORIZED`, `SSL_CERT_*`,
    /// `NODE_USE_SYSTEM_CA`, `GITHUB_API_URL`, `GITHUB_TOKEN`, `*_HTTP_RETRY_*`, CI vendors).
    pub fn with_env(config: Arc<Config>, env: Env) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(client::Inner::build(config, env)?),
        })
    }

    /// Where warnings go (retry waits, throttling, unaudited registries, refused credentials).
    pub fn set_event_sink(&self, sink: Arc<dyn EventSink>) {
        *self.inner.events.write() = Some(sink);
    }

    /// The manifest for `name` from memory, then disk cache (if fresh, or if offline /
    /// prefer-offline), then the network with `If-None-Match` / `If-Modified-Since`.
    pub async fn manifest(&self, name: &str, req: ManifestRequest) -> Result<Arc<PackageManifest>> {
        self.inner.manifest(name, req).await
    }

    /// Memory or disk only; never the network. For the speculative path and `--offline`.
    pub fn cached_manifest(&self, name: &str) -> Option<Arc<PackageManifest>> {
        self.inner.cached_manifest(name)
    }

    /// Starts manifest fetches in the background (look-ahead prefetch). Fire and forget;
    /// results land in the memo and are picked up by later `manifest` calls.
    pub fn prefetch<I: IntoIterator<Item = String>>(&self, names: I) {
        for name in names {
            if self.inner.memo.lock().contains_key(&name) {
                continue;
            }
            let inner = self.inner.clone();
            self.inner.runtime.spawn(async move {
                let _ = inner.manifest(&name, ManifestRequest::default()).await;
            });
        }
    }

    /// `GET` a tarball with the right credentials for its host. Integrity is verified by the
    /// consumer (the store hashes while extracting); `expected` is not needed for the request.
    pub async fn tarball(&self, url: &str, expected: Option<&Integrity>) -> Result<TarballBody> {
        let _ = expected;
        self.inner.tarball(url).await
    }

    /// Whole tarball into memory (small packages: one-shot libdeflate path).
    pub async fn tarball_bytes(&self, url: &str) -> Result<Bytes> {
        self.inner.tarball_bytes(url).await
    }

    /// `<registry>/<name>/-/<unscoped>-<version>.tgz` for the package's registry.
    pub fn default_tarball_url(&self, name: &str, version: &str) -> String {
        npm::default_tarball_url(self.inner.scopes.for_package(name).href(), name, version)
    }

    /// The registry URL (credential-free, ending with `/`) that serves `name`.
    pub fn registry_url_for(&self, name: &str) -> &str {
        self.inner.scopes.for_package(name).href()
    }

    /// GitHub: resolves `owner/repo#committish` to a commit SHA via `GITHUB_API_URL`
    /// (default `https://api.github.com`), honouring `GITHUB_TOKEN`.
    pub async fn resolve_github(
        &self,
        owner: &str,
        repo: &str,
        committish: &str,
    ) -> Result<String> {
        self.inner.resolve_github(owner, repo, committish).await
    }

    /// Tarball URL of a GitHub commit (`<api>/repos/<owner>/<repo>/tarball/<sha>`).
    pub fn github_tarball_url(&self, owner: &str, repo: &str, sha: &str) -> String {
        self.inner.github_tarball_url(owner, repo, sha)
    }

    /// Arbitrary JSON endpoint on a registry (audit, publish, whoami, dist-tag, view). Sends the
    /// URL's credentials and `npm-otp`; only GET/HEAD are retried. An empty body is `null`.
    pub async fn request_json(
        &self,
        method: &str,
        url: &str,
        body: Option<serde_json::Value>,
        otp: Option<&str>,
    ) -> Result<serde_json::Value> {
        Ok(self
            .inner
            .request_json_full(method, url, body.as_ref(), otp)
            .await?
            .body)
    }

    /// [`request_json`](Self::request_json) with the status and `npm-notice`.
    pub async fn request_json_full(
        &self,
        method: &str,
        url: &str,
        body: Option<&serde_json::Value>,
        otp: Option<&str>,
    ) -> Result<JsonResponse> {
        self.inner.request_json_full(method, url, body, otp).await
    }

    /// `fnpm pm whoami`: the user of the default registry's credentials, else `GET /-/whoami`.
    /// Returns the name and any `npm-notice`. `auth_type` is `publishConfig.authType`.
    pub async fn whoami(&self, auth_type: Option<&str>) -> Result<(String, Option<String>)> {
        self.inner.whoami(auth_type).await
    }

    /// `fnpm info`: the full packument as JSON.
    pub async fn view(&self, name: &str) -> Result<serde_json::Value> {
        self.inner.view(name).await
    }

    /// `fnpm audit`: bulk advisories for the installed npm packages, grouped by registry.
    pub async fn audit(&self, packages: &[AuditPackage]) -> Result<AuditResponse> {
        self.inner.audit(packages).await
    }

    /// `fnpm publish`: PUT the package, handling the OTP / web-login challenge via `prompt`.
    pub async fn publish(
        &self,
        pkg: &PublishPackage<'_>,
        options: &PublishOptions,
        prompt: &dyn OtpPrompt,
    ) -> Result<Published> {
        self.inner.publish(pkg, options, prompt).await
    }

    /// `--tolerate-republish`: whether the registry already has `name@version`.
    pub async fn version_exists(&self, name: &str, version: &str) -> bool {
        self.inner.version_exists(name, version).await
    }

    /// [`RegistryClient::version_exists`], handing the response's `npm-notice` to `notice`.
    pub async fn version_exists_noticed(
        &self,
        name: &str,
        version: &str,
        notice: &(dyn Fn(&str) + Send + Sync),
    ) -> bool {
        self.inner
            .version_exists_noticed(name, version, notice)
            .await
    }

    /// The scheme, host and path of the registry `name` publishes to, without userinfo (the
    /// configured URL when it does not parse).
    pub fn publish_registry_href(&self, name: &str) -> String {
        let scope = self.inner.scopes.for_package(name);
        match &scope.url {
            Some(url) => {
                let mut url = url.clone();
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.to_string()
            }
            None => scope.configured.clone(),
        }
    }

    /// Waits for background manifest-cache writes (call before the process exits).
    pub async fn flush_cache_writes(&self) {
        let writes = &self.inner.writes;
        loop {
            let notified = writes.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if writes.pending.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// The on-disk manifest cache, unless it is disabled.
    pub fn manifest_cache(&self) -> Option<&ManifestCache> {
        self.inner.cache.as_ref()
    }

    pub fn stats(&self) -> RegistryStats {
        self.inner.stats()
    }

    /// Requests currently holding a network slot, including streamed response bodies.
    pub fn active_requests(&self) -> usize {
        self.inner.limiter.in_flight()
    }

    /// The current request limit (lower than configured after repeated failures).
    pub fn concurrency_limit(&self) -> usize {
        self.inner.limiter.limit()
    }
}
