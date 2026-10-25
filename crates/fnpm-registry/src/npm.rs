//! Manifest requests (bun `NetworkTask::manifest_request` and `npm::registry::get_package_metadata`)
//! and the memory/disk/network lookup order.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use fnpm_config::{ManifestCacheMode, OfflineMode};
use reqwest::Method;
use reqwest::header::HeaderMap;
use url::Url;

use crate::auth::{Scope, redact_url};
use crate::client::{Fetched, Inner, MemoEntry, SharedResult, Spec, put};
use crate::date::now_ms;
use crate::limiter::Priority;
use crate::manifest::{PackageManifest, name_suggests_libc};
use crate::semver_ext;
use crate::{Error, ManifestRequest, Result};

/// The Accept header for abbreviated ("corgi") documents. Less restrictive than npm's, see
/// <https://github.com/oven-sh/bun/issues/341>.
pub const ACCEPT_ABBREVIATED: &str =
    "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*";
/// The Accept header for full documents.
pub const ACCEPT_FULL: &str = "application/json, */*";

/// How long a cached manifest stays fresh at most, and without `Cache-Control` (seconds).
pub const MAX_MANIFEST_AGE: u32 = 300;

/// Above this size, the JSON is parsed on the rayon pool instead of the tokio worker.
const PARSE_OFF_THREAD: usize = 64 * 1024;

/// The `max-age` a response allows, at most [`MAX_MANIFEST_AGE`]; `no-cache` / `no-store` is 0.
pub(crate) fn max_age(cache_control: Option<&str>) -> u32 {
    let mut max_age = u64::from(MAX_MANIFEST_AGE);
    for directive in cache_control.unwrap_or("").split(',') {
        let d = directive.trim();
        if d.eq_ignore_ascii_case("no-cache") || d.eq_ignore_ascii_case("no-store") {
            max_age = 0;
        } else if d.len() >= 8 && d[..8].eq_ignore_ascii_case("max-age=") {
            max_age = max_age.min(d[8..].trim().parse::<u64>().unwrap_or(0));
        }
    }
    max_age as u32
}

/// Fresh: younger than its max-age, and not written while the clock was set in the future (an
/// expiry more than [`MAX_MANIFEST_AGE`] ahead of now).
pub(crate) fn is_fresh(m: &PackageManifest, now: u64) -> bool {
    let expiry = m
        .fetched_at_ms
        .saturating_add(u64::from(m.max_age_secs) * 1000);
    now < expiry && expiry <= now.saturating_add(u64::from(MAX_MANIFEST_AGE) * 1000)
}

/// `<registry>/<name>` with `/` in a scoped name encoded as `%2f` (AWS CodeArtifact needs it).
/// The joined URL must stay on the registry: same origin and under its path.
pub(crate) fn manifest_url(scope: &Scope, name: &str) -> Result<Url> {
    let invalid = |message: String| Error::Network {
        url: redact_url(&scope.configured),
        message,
    };
    let Some(registry) = &scope.url else {
        return Err(invalid(format!(
            "Registry URL must be http:// or https://\nReceived: {:?}",
            redact_url(&scope.configured)
        )));
    };
    if !matches!(registry.scheme(), "http" | "https") {
        return Err(invalid(format!(
            "Registry URL must be http:// or https://\nReceived: {:?}",
            redact_url(registry.as_str())
        )));
    }
    let mut base = registry.clone();
    base.set_query(None);
    base.set_fragment(None);
    if !base.path().ends_with('/') {
        let p = format!("{}/", base.path());
        base.set_path(&p);
    }
    let encoded = name.replace('/', "%2f");
    let joined = base.join(&encoded).map_err(|_| {
        invalid(format!(
            "Failed to join registry {:?} and package {name:?} URLs",
            redact_url(base.as_str())
        ))
    })?;
    let dir_end = base.path().rfind('/').map_or(0, |i| i + 1);
    let on_registry = joined.scheme() == base.scheme()
        && joined.host_str() == base.host_str()
        && joined.port_or_known_default() == base.port_or_known_default()
        && joined.path().starts_with(&base.path()[..dir_end])
        && joined.path().len() > base.path().len();
    if !on_registry {
        return Err(invalid(format!(
            "invalid package name {name:?}: manifest URL {:?} is not on registry {:?}",
            redact_url(joined.as_str()),
            redact_url(base.as_str())
        )));
    }
    Ok(joined)
}

/// `<registry>/<name>/-/<unscoped>-<version>.tgz` (bun `extract_tarball::build_url`).
pub(crate) fn default_tarball_url(registry_href: &str, name: &str, version: &str) -> String {
    let registry = registry_href.trim_end_matches('/');
    let unscoped = match name.strip_prefix('@') {
        Some(rest) => rest.split_once('/').map_or(name, |(_, n)| n),
        None => name,
    };
    let version = semver_ext::parse_version(version)
        .map_or_else(|| version.to_owned(), |v| semver_ext::format(&v));
    format!("{registry}/{name}/-/{unscoped}-{version}.tgz")
}

fn manifest_headers(scope: &Scope, cached: Option<&PackageManifest>, full: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(auth) = &scope.authorization {
        put(&mut headers, "authorization", auth);
        put(&mut headers, "npm-auth-type", "legacy");
    }
    if let Some(m) = cached {
        match (&m.etag, &m.last_modified) {
            (Some(etag), _) if !etag.is_empty() => put(&mut headers, "if-none-match", etag),
            (_, Some(lm)) if !lm.is_empty() => put(&mut headers, "if-modified-since", lm),
            _ => {}
        }
    }
    put(
        &mut headers,
        "accept",
        if full {
            ACCEPT_FULL
        } else {
            ACCEPT_ABBREVIATED
        },
    );
    headers
}

async fn parse_body(name: &str, body: Bytes) -> Result<PackageManifest> {
    if body.len() < PARSE_OFF_THREAD {
        return PackageManifest::parse(name, &body);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let owned = name.to_owned();
    rayon::spawn(move || {
        let _ = tx.send(PackageManifest::parse(&owned, &body));
    });
    rx.await.unwrap_or_else(|_| {
        Err(Error::InvalidManifest {
            name: name.to_owned(),
            message: "parser thread exited".to_owned(),
        })
    })
}

impl Error {
    /// A copy for every caller that shared one in-flight request.
    pub(crate) fn duplicate(&self) -> Error {
        match self {
            Error::Status { url, status } => Error::Status {
                url: url.clone(),
                status: *status,
            },
            Error::NotFound(s) => Error::NotFound(s.clone()),
            Error::Network { url, message } => Error::Network {
                url: url.clone(),
                message: message.clone(),
            },
            Error::Offline(s) => Error::Offline(s.clone()),
            Error::InvalidManifest { name, message } => Error::InvalidManifest {
                name: name.clone(),
                message: message.clone(),
            },
            Error::Integrity {
                url,
                expected,
                actual,
            } => Error::Integrity {
                url: url.clone(),
                expected: expected.clone(),
                actual: actual.clone(),
            },
            Error::Git(s) => Error::Git(s.clone()),
            Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), e.to_string())),
            Error::Registry {
                url,
                status,
                message,
            } => Error::Registry {
                url: url.clone(),
                status: *status,
                message: message.clone(),
            },
            Error::NeedAuth(s) => Error::NeedAuth(s.clone()),
            Error::InvalidResponse { url, message } => Error::InvalidResponse {
                url: url.clone(),
                message: message.clone(),
            },
        }
    }

    /// True for failures another attempt could fix: no response, or 5xx/429/408 after the
    /// retries ran out.
    fn is_transient(&self) -> bool {
        match self {
            Error::Network { .. } => true,
            Error::Status { status, .. } => *status > 499 || *status == 429 || *status == 408,
            _ => false,
        }
    }
}

impl Inner {
    fn remember(&self, name: &str, manifest: &Arc<PackageManifest>, from_network: bool) {
        self.memo.lock().insert(
            name.to_owned(),
            MemoEntry {
                manifest: manifest.clone(),
                from_network,
            },
        );
    }

    /// Memory, then disk (any age). Never the network.
    pub fn cached_manifest(&self, name: &str) -> Option<Arc<PackageManifest>> {
        if let Some(e) = self.memo.lock().get(name) {
            return Some(e.manifest.clone());
        }
        let cache = self.cache.as_ref()?;
        let scope = self.scopes.for_package(name);
        let m = cache.load(scope.href(), name)?;
        self.remember(name, &m, false);
        Some(m)
    }

    pub async fn manifest(
        self: &Arc<Self>,
        name: &str,
        req: ManifestRequest,
    ) -> Result<Arc<PackageManifest>> {
        if name.is_empty() {
            return Err(Error::InvalidManifest {
                name: String::new(),
                message: "empty package name".to_owned(),
            });
        }
        if let Some(e) = self.memo.lock().get(name)
            && (e.manifest.is_full || !req.full)
            && (!req.revalidate || e.from_network)
        {
            return Ok(e.manifest.clone());
        }
        if self.missing.lock().contains(name) {
            return Err(Error::NotFound(name.to_owned()));
        }
        let fetch_full = req.full || name_suggests_libc(name);
        let key = (name.to_owned(), fetch_full);
        let cell = self.inflight.lock().entry(key.clone()).or_default().clone();
        let this = self.clone();
        let result: SharedResult = cell
            .get_or_init(|| async move {
                this.load_manifest(name, fetch_full, req)
                    .await
                    .map_err(Arc::new)
            })
            .await
            .clone();
        {
            let mut inflight = self.inflight.lock();
            if inflight.get(&key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
                inflight.remove(&key);
            }
        }
        result.map_err(|e| e.duplicate())
    }

    async fn load_disk(&self, scope_href: &str, name: &str) -> Option<Arc<PackageManifest>> {
        let cache = self.cache.clone()?;
        let (href, owned) = (scope_href.to_owned(), name.to_owned());
        tokio::task::spawn_blocking(move || cache.load(&href, &owned))
            .await
            .unwrap_or_default()
    }

    /// Writes the cache entry on a blocking thread; never awaited on the critical path.
    pub fn store_async(&self, scope_href: &str, manifest: Arc<PackageManifest>) {
        let Some(cache) = self.cache.clone() else {
            return;
        };
        let writes = self.writes.clone();
        writes.pending.fetch_add(1, Ordering::AcqRel);
        let href = scope_href.to_owned();
        self.runtime.spawn_blocking(move || {
            // A cache that cannot be written only costs a refetch next time.
            let _ = cache.store(&href, &manifest);
            if writes.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                writes.done.notify_waiters();
            }
        });
    }

    async fn load_manifest(
        self: &Arc<Self>,
        name: &str,
        fetch_full: bool,
        req: ManifestRequest,
    ) -> Result<Arc<PackageManifest>> {
        let scope = self.scopes.for_package(name).clone();
        let href = scope.href().to_owned();
        let cached = self.load_disk(&href, name).await;

        if self.offline == OfflineMode::Offline {
            return match cached {
                Some(m) => {
                    self.stats
                        .manifest_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                    self.remember(name, &m, false);
                    Ok(m)
                }
                None => Err(Error::Offline(name.to_owned())),
            };
        }
        // Moved, not shared: a 304 then updates the decoded manifest in place instead of
        // deep-copying every version.
        let (usable, other) = match cached {
            Some(c) if c.is_full || !fetch_full => (Some(c), None),
            c => (None, c),
        };
        if let Some(m) = &usable {
            let fresh = self.cache_mode == ManifestCacheMode::CacheControl
                && !req.revalidate
                && is_fresh(m, now_ms());
            if self.offline == OfflineMode::PreferOffline || fresh {
                self.stats
                    .manifest_cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                self.remember(name, m, false);
                return Ok(m.clone());
            }
        }
        match self
            .request_manifest(&scope, name, usable, fetch_full)
            .await
        {
            Ok(m) => Ok(m),
            // The full document was only wanted for `libc`: one more try with the abbreviated one.
            // (`usable` was a full document then, so the abbreviated copy can only be `other`.)
            Err(e) if fetch_full && !req.full && e.is_transient() => {
                let abbreviated = other.filter(|c| !c.is_full);
                self.request_manifest(&scope, name, abbreviated, false)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    async fn request_manifest(
        self: &Arc<Self>,
        scope: &Scope,
        name: &str,
        cached: Option<Arc<PackageManifest>>,
        full: bool,
    ) -> Result<Arc<PackageManifest>> {
        let url = manifest_url(scope, name)?;
        let spec = Spec {
            method: Method::GET,
            headers: manifest_headers(scope, cached.as_deref(), full),
            url,
            body: None,
            priority: Priority::Manifest,
            blocking: false,
            retry: true,
            what: format!("package manifest {name}"),
        };
        self.stats.manifest_requests.fetch_add(1, Ordering::Relaxed);
        let fetched = self.fetch(&spec).await?;
        let href = scope.href();
        self.manifest_response(href, name, &spec.url, fetched, cached, full)
            .await
    }

    async fn manifest_response(
        &self,
        href: &str,
        name: &str,
        url: &Url,
        f: Fetched,
        cached: Option<Arc<PackageManifest>>,
        full: bool,
    ) -> Result<Arc<PackageManifest>> {
        let now = now_ms();
        match f.status {
            304 => {
                let Some(old) = cached else {
                    return Err(Error::InvalidResponse {
                        url: redact_url(url.as_str()),
                        message: "unexpected 304 Not Modified without a cached manifest".to_owned(),
                    });
                };
                self.stats.not_modified.fetch_add(1, Ordering::Relaxed);
                let mut m = Arc::unwrap_or_clone(old);
                m.fetched_at_ms = now;
                m.max_age_secs = max_age(f.header("cache-control"));
                let m = Arc::new(m);
                self.store_async(href, m.clone());
                self.remember(name, &m, true);
                Ok(m)
            }
            200..=299 => {
                let mut m = parse_body(name, f.body.clone()).await?;
                m.etag = f
                    .header("etag")
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned);
                m.last_modified = f
                    .header("last-modified")
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned);
                m.fetched_at_ms = now;
                m.max_age_secs = max_age(f.header("cache-control"));
                m.is_full = full;
                let m = Arc::new(m);
                self.store_async(href, m.clone());
                self.remember(name, &m, true);
                Ok(m)
            }
            404 => {
                self.missing.lock().insert(name.to_owned());
                Err(Error::NotFound(name.to_owned()))
            }
            status => Err(Error::Status {
                url: redact_url(url.as_str()),
                status,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnpm_config::Auth;

    fn scope(url: &str) -> Scope {
        Scope {
            url: Url::parse(url).ok(),
            configured: url.to_owned(),
            authorization: None,
            user: None,
            own_key: None,
            identity: None,
        }
    }

    #[test]
    fn cache_control() {
        assert_eq!(max_age(None), 300);
        assert_eq!(max_age(Some("public, max-age=60")), 60);
        assert_eq!(max_age(Some("max-age=86400")), 300);
        assert_eq!(max_age(Some("MAX-AGE=10, no-cache")), 0);
        assert_eq!(max_age(Some("no-store")), 0);
        assert_eq!(max_age(Some("max-age=soon")), 0);
    }

    #[test]
    fn freshness_and_clock_skew() {
        let mut m = PackageManifest::parse("x", b"{}").unwrap();
        m.fetched_at_ms = 1_000_000;
        m.max_age_secs = 300;
        assert!(is_fresh(&m, 1_000_000));
        assert!(is_fresh(&m, 1_299_999));
        assert!(!is_fresh(&m, 1_300_000));
        // Written with the clock an hour ahead: expired.
        m.fetched_at_ms = 1_000_000 + 3_600_000;
        assert!(!is_fresh(&m, 1_000_000));
        m.max_age_secs = 0;
        m.fetched_at_ms = 1_000_000;
        assert!(!is_fresh(&m, 1_000_000));
    }

    #[test]
    fn manifest_urls() {
        let s = scope("https://registry.npmjs.org/");
        assert_eq!(
            manifest_url(&s, "react").unwrap().as_str(),
            "https://registry.npmjs.org/react"
        );
        assert_eq!(
            manifest_url(&s, "@types/node").unwrap().as_str(),
            "https://registry.npmjs.org/@types%2fnode"
        );
        let s = scope("https://npm.example/api/npm/registry?x=1");
        assert_eq!(
            manifest_url(&s, "a").unwrap().as_str(),
            "https://npm.example/api/npm/registry/a"
        );
        let s = scope("http://127.0.0.1:4873");
        assert_eq!(
            manifest_url(&s, "pkg").unwrap().as_str(),
            "http://127.0.0.1:4873/pkg"
        );
        for bad in [".", "..", "%2e%2e", ""] {
            let s = scope("https://r.example/npm/");
            assert!(manifest_url(&s, bad).is_err(), "{bad}");
        }
        assert!(manifest_url(&scope("file:///tmp/registry/"), "a").is_err());
        assert!(manifest_url(&scope("not a url"), "a").is_err());
    }

    #[test]
    fn default_tarball_urls() {
        assert_eq!(
            default_tarball_url("https://r/", "lodash", "4.17.21"),
            "https://r/lodash/-/lodash-4.17.21.tgz"
        );
        assert_eq!(
            default_tarball_url("https://r", "@types/node", "20.1.0"),
            "https://r/@types/node/-/node-20.1.0.tgz"
        );
        assert_eq!(
            default_tarball_url("https://r/", "x", "1.0.0-beta.1+build"),
            "https://r/x/-/x-1.0.0-beta.1+build.tgz"
        );
    }

    #[test]
    fn headers_for_manifest_requests() {
        let mut s = scope("https://r/");
        s.authorization = crate::auth::authorization_of(&Auth::Token { token: "t".into() });
        let mut m = PackageManifest::parse("x", b"{}").unwrap();
        m.etag = Some("\"e\"".into());
        m.last_modified = Some("Mon, 01 Jan 2024 00:00:00 GMT".into());
        let h = manifest_headers(&s, Some(&m), false);
        assert_eq!(h["accept"], ACCEPT_ABBREVIATED);
        assert_eq!(h["authorization"], "Bearer t");
        assert_eq!(h["npm-auth-type"], "legacy");
        assert_eq!(h["if-none-match"], "\"e\"");
        assert!(h.get("if-modified-since").is_none());
        m.etag = None;
        let h = manifest_headers(&scope("https://r/"), Some(&m), true);
        assert_eq!(h["accept"], ACCEPT_FULL);
        assert_eq!(h["if-modified-since"], "Mon, 01 Jan 2024 00:00:00 GMT");
        assert!(h.get("authorization").is_none());
    }
}
