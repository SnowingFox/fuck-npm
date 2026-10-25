//! The shared HTTP machinery: one `reqwest` client (HTTP/2 over rustls, ALPN h2 with an HTTP/1.1
//! fallback, connection reuse per origin), the request limiter, retries with backoff, and the
//! warnings bun prints about them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use fnpm_config::{Config, Env, LogLevel, ManifestCacheMode, OfflineMode};
use fnpm_types::{Event, EventSink};
use parking_lot::{Mutex, RwLock};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use tokio::sync::Notify;
use url::Url;

use crate::auth::{Scopes, redact_url};
use crate::cache::ManifestCache;
use crate::limiter::{Limiter, Priority};
use crate::manifest::PackageManifest;
use crate::proxy::ProxySettings;
use crate::retry::{self, Outcome};
use crate::tls::{self, Trust};
use crate::{Error, RegistryStats, Result};

/// A manifest kept for the life of the client.
#[derive(Clone)]
pub(crate) struct MemoEntry {
    pub manifest: Arc<PackageManifest>,
    /// True if the registry answered for it in this run (not just the disk cache).
    pub from_network: bool,
}

pub(crate) type SharedResult = std::result::Result<Arc<PackageManifest>, Arc<Error>>;

/// One in-flight manifest request that concurrent callers share.
pub(crate) type InFlight = Arc<tokio::sync::OnceCell<SharedResult>>;

#[derive(Default)]
pub(crate) struct Stats {
    pub manifest_requests: AtomicU64,
    pub manifest_cache_hits: AtomicU64,
    pub not_modified: AtomicU64,
    pub tarball_requests: AtomicU64,
    pub bytes_downloaded: AtomicU64,
    pub retries: AtomicU64,
}

/// Background cache writes still running.
#[derive(Default)]
pub(crate) struct Writes {
    pub pending: AtomicUsize,
    pub done: Notify,
}

#[derive(Default)]
struct Announced {
    /// "registry requests failed, retrying" (after a timeout) is said once per client.
    timeout: bool,
    /// The highest attempt whose `Retry-After` wait was announced.
    asked: u16,
}

pub(crate) struct Inner {
    pub http: reqwest::Client,
    pub scopes: Scopes,
    pub limiter: Arc<Limiter>,
    pub cache: Option<ManifestCache>,
    pub memo: Mutex<HashMap<String, MemoEntry>>,
    pub missing: Mutex<HashSet<String>>,
    pub inflight: Mutex<HashMap<(String, bool), InFlight>>,
    pub stats: Stats,
    pub writes: Arc<Writes>,
    pub runtime: tokio::runtime::Handle,
    pub github_api: String,
    pub github_token: Option<String>,
    pub ci_name: Option<&'static str>,
    pub user_agent: String,
    pub events: RwLock<Option<Arc<dyn EventSink>>>,
    announced: Mutex<Announced>,
    pub verbose: bool,
    pub silent: bool,
    pub retries: u16,
    pub backoff_ms: u64,
    pub offline: OfflineMode,
    pub cache_mode: ManifestCacheMode,
}

/// One logical request. Retries resend exactly these headers and body.
pub(crate) struct Spec {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    pub priority: Priority,
    /// The caller waits on this one request: no retry after a timeout or a 501.
    pub blocking: bool,
    /// False for requests the registry must not get twice (`publish`).
    pub retry: bool,
    /// For `--verbose` retry lines: `package manifest bar`, `tarball bar-1.0.0.tgz`.
    pub what: String,
}

/// A complete response.
pub(crate) struct Fetched {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Fetched {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// `user-agent` default: `fnpm/<version> npm/? node/<version> <os> <arch>`.
pub(crate) fn default_user_agent() -> String {
    format!(
        "fnpm/{} npm/? node/v22.0.0 {} {}",
        env!("CARGO_PKG_VERSION"),
        os_name(),
        arch_name()
    )
}

pub(crate) fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

pub(crate) fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// The CI vendor, as npm's `ci-info` names it (bun `ci_info::detect_ci_name`).
pub(crate) fn detect_ci_name(env: &Env) -> Option<&'static str> {
    if env.get("CI").is_some_and(|v| v == "false") {
        return None;
    }
    let set = |k: &str| env.get(k).is_some();
    let eq = |k: &str, v: &str| env.get(k) == Some(v);
    let vendors: [(&'static str, bool); 52] = [
        ("agola-ci", set("AGOLA_GIT_REF")),
        ("appcircle", set("AC_APPCIRCLE")),
        ("appveyor", set("APPVEYOR")),
        ("aws-codebuild", set("CODEBUILD_BUILD_ARN")),
        ("azure-pipelines", set("TF_BUILD")),
        ("bamboo", set("bamboo_planKey")),
        ("bitbucket-pipelines", set("BITBUCKET_COMMIT")),
        ("bitrise", set("BITRISE_IO")),
        ("buddy", set("BUDDY_WORKSPACE_ID")),
        ("buildkite", set("BUILDKITE")),
        ("circleci", set("CIRCLECI")),
        ("cirrus-ci", set("CIRRUS_CI")),
        ("cloudflare-pages", set("CF_PAGES")),
        ("cloudflare-workers", set("WORKERS_CI")),
        ("codefresh", set("CF_BUILD_ID")),
        ("codemagic", set("CM_BUILD_ID")),
        ("codeship", eq("CI_NAME", "codeship")),
        ("drone", set("DRONE")),
        ("dsari", set("DSARI")),
        ("earthly", set("EARTHLY_CI")),
        ("expo-application-services", set("EAS_BUILD")),
        ("gerrit", set("GERRIT_PROJECT")),
        ("gitea-actions", set("GITEA_ACTIONS")),
        ("github-actions", set("GITHUB_ACTIONS")),
        ("gitlab-ci", set("GITLAB_CI")),
        ("gocd", set("GO_PIPELINE_LABEL")),
        ("google-cloud-build", set("BUILDER_OUTPUT")),
        ("harness-ci", set("HARNESS_BUILD_ID")),
        (
            "heroku",
            env.get("NODE")
                .is_some_and(|v| v.contains("/app/.heroku/node/bin/node")),
        ),
        ("hudson", set("HUDSON_URL")),
        ("jenkins", set("JENKINS_URL") && set("BUILD_ID")),
        ("layerci", set("LAYERCI")),
        ("magnum-ci", set("MAGNUM")),
        ("netlify-ci", set("NETLIFY")),
        ("nevercode", set("NEVERCODE")),
        ("prow", set("PROW_JOB_ID")),
        ("releasehub", set("RELEASE_BUILD_ID")),
        ("render", set("RENDER")),
        ("sail-ci", set("SAILCI")),
        ("screwdriver", set("SCREWDRIVER")),
        ("semaphore", set("SEMAPHORE")),
        ("sourcehut", eq("CI_NAME", "sourcehut")),
        ("strider-cd", set("STRIDER")),
        ("taskcluster", set("TASK_ID") && set("RUN_ID")),
        ("teamcity", set("TEAMCITY_VERSION")),
        ("travis-ci", set("TRAVIS")),
        ("vela", set("VELA")),
        ("vercel", set("NOW_BUILDER") || set("VERCEL")),
        ("visual-studio-app-center", set("APPCENTER_BUILD_ID")),
        ("woodpecker", eq("CI", "woodpecker")),
        ("xcode-cloud", set("CI_XCODE_PROJECT")),
        ("xcode-server", set("XCS")),
    ];
    vendors
        .into_iter()
        .find(|(_, hit)| *hit)
        .map(|(name, _)| name)
}

/// Why a request produced no response, as text (the error's source chain).
pub(crate) fn describe(e: &reqwest::Error) -> String {
    let mut parts = Vec::new();
    let mut source: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = source {
        let text = err.to_string();
        if !parts.iter().any(|p: &String| p.contains(&text)) {
            parts.push(text);
        }
        source = err.source();
    }
    // reqwest's own message repeats the URL; secrets in it are masked.
    redact_url(&parts.join(": "))
}

pub(crate) fn classify(e: &reqwest::Error) -> Outcome {
    if e.is_timeout() {
        return Outcome::Timeout;
    }
    if e.is_redirect() || e.is_builder() {
        return Outcome::Final;
    }
    let text = describe(e).to_ascii_lowercase();
    if text.contains("certificate")
        || text.contains("unknownissuer")
        || text.contains("invalid peer")
    {
        return Outcome::Final;
    }
    if text.contains("timed out") {
        return Outcome::Timeout;
    }
    Outcome::NoResponse
}

fn header_name(name: &'static str) -> HeaderName {
    HeaderName::from_static(name)
}

/// Inserts a header whose value comes from configuration; values that are not valid header text
/// are skipped (credentials with CR/LF/NUL were already refused).
pub(crate) fn put(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(header_name(name), v);
    }
}

impl Inner {
    pub fn build(config: Arc<Config>, env: Env) -> Result<Self> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::Io(std::io::Error::other(
                "RegistryClient::new must be called inside a tokio runtime",
            ))
        })?;
        let net = &config.network;
        let scopes = Scopes::from_config(&config);
        let proxy = ProxySettings::resolve(&net.proxy, &env);
        let identity = match &scopes.default.identity {
            Some((cert, key)) => Some(tls::load_identity(cert, key)?),
            None => None,
        };
        let trust = tls::trust(&net.tls, &env, identity)?;

        let user_agent = if net.user_agent.trim().is_empty() {
            default_user_agent()
        } else {
            net.user_agent.clone()
        };
        let idle = Duration::from_secs(net.idle_timeout_secs.max(1));
        let mut builder = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            .read_timeout(idle)
            .connect_timeout(idle.min(Duration::from_secs(60)))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_while_idle(false)
            .no_proxy();
        builder = match trust {
            Trust::Rustls(cfg) => builder.tls_backend_preconfigured(*cfg),
            Trust::System { extra } => {
                let certs: Vec<reqwest::Certificate> = extra
                    .iter()
                    .filter_map(|c| reqwest::Certificate::from_der(c.as_ref()).ok())
                    .collect();
                builder.tls_certs_merge(certs)
            }
            Trust::Insecure => builder.tls_danger_accept_invalid_certs(true),
        };
        if proxy.http.is_some() || proxy.https.is_some() {
            let settings = Arc::new(proxy.clone());
            builder = builder.proxy(reqwest::Proxy::custom(move |url: &reqwest::Url| {
                let target = settings.for_url(url)?;
                let mut proxy_url = target.url.clone();
                if let Some((user, pass)) = &target.basic_auth {
                    let _ = proxy_url.set_username(user);
                    let _ = proxy_url.set_password(Some(pass));
                }
                Some(proxy_url)
            }));
        }
        let http = builder.build().map_err(|e| Error::Network {
            url: String::new(),
            message: describe(&e),
        })?;

        let cache = match net.manifest_cache {
            ManifestCacheMode::Disabled => None,
            _ => Some(ManifestCache::for_cache_dir(&config.cache_dir)),
        };
        let github_api = env
            .get("GITHUB_API_URL")
            .filter(|u| !u.is_empty())
            .unwrap_or("https://api.github.com")
            .trim_end_matches('/')
            .to_owned();
        let github_token = env
            .get("GITHUB_TOKEN")
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        let ci_name = detect_ci_name(&env);
        let retries = env
            .get("FNPM_CONFIG_HTTP_RETRY_COUNT")
            .or_else(|| env.get("BUN_CONFIG_HTTP_RETRY_COUNT"))
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(net.retries);
        let backoff_ms = env
            .get("FNPM_CONFIG_HTTP_RETRY_BACKOFF")
            .or_else(|| env.get("BUN_CONFIG_HTTP_RETRY_BACKOFF"))
            .map_or(Some(net.retry_backoff_ms), |v| v.trim().parse::<u64>().ok())
            .unwrap_or(50);

        let inner = Self {
            limiter: Limiter::new(usize::from(net.concurrency.max(1))),
            offline: net.offline,
            cache_mode: if config.force && net.manifest_cache == ManifestCacheMode::CacheControl {
                ManifestCacheMode::NoCacheControl
            } else {
                net.manifest_cache
            },
            verbose: config.log_level == LogLevel::Verbose,
            silent: config.log_level == LogLevel::Silent,
            http,
            scopes,
            cache,
            memo: Mutex::default(),
            missing: Mutex::default(),
            inflight: Mutex::default(),
            stats: Stats::default(),
            writes: Arc::default(),
            runtime,
            github_api,
            github_token,
            ci_name,
            user_agent,
            events: RwLock::new(None),
            announced: Mutex::default(),
            retries,
            backoff_ms,
        };
        for w in inner.scopes.warnings.iter().chain(&proxy.invalid) {
            inner.warn(w.clone());
        }
        Ok(inner)
    }

    pub fn warn(&self, message: String) {
        if self.silent {
            return;
        }
        if let Some(sink) = self.events.read().as_ref() {
            sink.emit(Event::warn(message, Some("registry")));
        }
    }

    pub fn stats(&self) -> RegistryStats {
        let s = &self.stats;
        RegistryStats {
            manifest_requests: s.manifest_requests.load(Ordering::Relaxed),
            manifest_cache_hits: s.manifest_cache_hits.load(Ordering::Relaxed),
            not_modified: s.not_modified.load(Ordering::Relaxed),
            tarball_requests: s.tarball_requests.load(Ordering::Relaxed),
            bytes_downloaded: s.bytes_downloaded.load(Ordering::Relaxed),
            retries: s.retries.load(Ordering::Relaxed),
        }
    }

    /// Decides whether and how long to wait before the next attempt, and says what bun says.
    /// Returns `None` when the request should not be tried again.
    pub fn plan_retry(
        &self,
        spec: &Spec,
        outcome: Outcome,
        attempt: u16,
        retry_after: Option<&str>,
        err: &str,
    ) -> Option<Duration> {
        if !spec.retry || !retry::should_retry(outcome, spec.blocking) || attempt >= self.retries {
            return None;
        }
        // A retried request failed again without a response.
        if outcome == Outcome::NoResponse
            && attempt > 0
            && let Some(limit) = self.limiter.throttle()
            && self.verbose
        {
            self.warn(format!(
                "request failed twice, limiting to {limit} concurrent requests"
            ));
        }
        let next = attempt + 1;
        let (wait, asked) =
            retry::wait_before(self.backoff_ms, next, retry_after, retry::fast_random());
        let line = {
            let mut a = self.announced.lock();
            if asked && next > a.asked {
                a.asked = next;
                Some(format!(
                    "registry asked to wait {}s before retrying",
                    wait.as_secs()
                ))
            } else if !asked && !a.timeout && outcome == Outcome::Timeout {
                a.timeout = true;
                Some("registry requests failed, retrying".to_owned())
            } else {
                None
            }
        };
        if let Some(line) = line {
            self.warn(line);
        } else if self.verbose {
            self.warn(format!(
                "{err} downloading {}. Retry {next}/{} in {}ms...",
                spec.what,
                self.retries,
                wait.as_millis()
            ));
        }
        self.stats.retries.fetch_add(1, Ordering::Relaxed);
        Some(wait)
    }

    fn request(
        &self,
        spec: &Spec,
        extra: Option<(&'static str, String)>,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(spec.method.clone(), spec.url.clone())
            .headers(spec.headers.clone());
        if let Some((name, value)) = extra {
            req = req.header(name, value);
        }
        if let Some(body) = &spec.body {
            req = req.body(body.clone());
        }
        req
    }

    /// Sends `spec` with retries and reads the whole body. A response with a status that is not
    /// retried (or after the last retry) is returned as is; only transport failures are errors.
    pub async fn fetch(&self, spec: &Spec) -> Result<Fetched> {
        let mut attempt: u16 = 0;
        loop {
            let permit = self.limiter.acquire(spec.priority).await;
            let result = match self.request(spec, None).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers = resp.headers().clone();
                    match read_body(resp, MAX_BODY).await {
                        Ok(None) => {
                            drop(permit);
                            return Err(Error::InvalidResponse {
                                url: redact_url(spec.url.as_str()),
                                message: format!(
                                    "response body is larger than {} MiB",
                                    MAX_BODY >> 20
                                ),
                            });
                        }
                        Ok(Some(body)) => {
                            self.stats
                                .bytes_downloaded
                                .fetch_add(body.len() as u64, Ordering::Relaxed);
                            Ok(Fetched {
                                status,
                                headers,
                                body,
                            })
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            drop(permit);
            let (outcome, retry_after, err_name) = match &result {
                Ok(f) => (
                    Outcome::Status(f.status),
                    f.header("retry-after").map(str::to_owned),
                    status_name(f.status),
                ),
                Err(e) => (classify(e), None, error_name(e)),
            };
            if let Outcome::Status(s) = outcome
                && !retry::should_retry(outcome, spec.blocking)
            {
                if s < 500 {
                    self.limiter.record_success();
                }
                return result.map_err(|_| unreachable_error());
            }
            match self.plan_retry(spec, outcome, attempt, retry_after.as_deref(), &err_name) {
                Some(wait) => {
                    attempt += 1;
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                }
                None => {
                    return match result {
                        Ok(f) => Ok(f),
                        Err(e) => Err(Error::Network {
                            url: redact_url(spec.url.as_str()),
                            message: describe(&e),
                        }),
                    };
                }
            }
        }
    }
}

/// The largest (decoded) response body read into memory. The biggest full packuments on npm are
/// a few hundred MiB; this only stops a hostile or broken server (or a decompression bomb, now
/// that manifests are fetched compressed) from exhausting memory.
const MAX_BODY: usize = 1 << 30;

/// The whole body, or `None` once it grows past `limit` bytes.
async fn read_body(
    mut resp: reqwest::Response,
    limit: usize,
) -> std::result::Result<Option<Bytes>, reqwest::Error> {
    let mut first: Option<Bytes> = None;
    let mut buf: Option<bytes::BytesMut> = None;
    let mut len = 0usize;
    while let Some(chunk) = resp.chunk().await? {
        len += chunk.len();
        if len > limit {
            return Ok(None);
        }
        // One chunk (the common case for small bodies) is returned without a copy.
        match (&mut first, &mut buf) {
            (None, None) => first = Some(chunk),
            (Some(f), None) => {
                let mut b = bytes::BytesMut::with_capacity((len * 2).min(limit));
                b.extend_from_slice(f);
                b.extend_from_slice(&chunk);
                first = None;
                buf = Some(b);
            }
            (_, Some(b)) => b.extend_from_slice(&chunk),
        }
    }
    Ok(Some(match buf {
        Some(b) => b.freeze(),
        None => first.unwrap_or_default(),
    }))
}

fn unreachable_error() -> Error {
    Error::Network {
        url: String::new(),
        message: "unreachable".to_owned(),
    }
}

/// bun's names for failed downloads in `--verbose` retry lines.
pub(crate) fn status_name(status: u16) -> String {
    match status {
        429 => "TooManyRequests".to_owned(),
        500..=599 => "HTTPError".to_owned(),
        s => StatusCode::from_u16(s)
            .map_or_else(|_| format!("HTTP {s}"), |c| format!("HTTP {}", c.as_u16())),
    }
}

pub(crate) fn error_name(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "Timeout".to_owned()
    } else if e.is_connect() {
        let text = describe(e).to_ascii_lowercase();
        if text.contains("refused") {
            "ConnectionRefused".to_owned()
        } else {
            "ConnectionFailed".to_owned()
        }
    } else if e.is_body() || e.is_decode() {
        "ConnectionClosed".to_owned()
    } else if e.is_redirect() {
        "TooManyRedirects".to_owned()
    } else {
        "ConnectionClosed".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bodies_are_read_whole_and_capped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = [0u8; 1024];
                    let _ = sock.read(&mut req).await;
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n")
                        .await;
                    for _ in 0..4 {
                        let _ = sock.write_all(b"400\r\n").await;
                        let _ = sock.write_all(&[b'x'; 0x400]).await;
                        let _ = sock.write_all(b"\r\n").await;
                        let _ = sock.flush().await;
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                });
            }
        });
        let http = reqwest::Client::new();
        let get = || http.get(format!("http://{addr}/")).send();
        let whole = read_body(get().await.unwrap(), 4096)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(whole.len(), 4096);
        assert!(whole.iter().all(|&b| b == b'x'));
        assert!(
            read_body(get().await.unwrap(), 4095)
                .await
                .unwrap()
                .is_none()
        );
    }
}
