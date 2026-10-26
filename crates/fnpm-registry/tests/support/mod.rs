//! A programmable in-process registry (axum) and a `Config` builder that does not depend on
//! `fnpm_config::Config::load`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use fnpm_config::{
    Auth, Config, LinkerSetting, LockfileConfig, LogLevel, ManifestCacheMode, NetworkConfig,
    OfflineMode, Omit, ProxyConfig, RegistryScope, TlsConfig,
};
use fnpm_types::Platform;
use parking_lot::Mutex;

/// One request the server saw.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: Method,
    /// Path and query (or the absolute URI a proxy sees).
    pub uri: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub at: Instant,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }
}

type Handler = dyn Fn(&Seen, usize) -> Response + Send + Sync;

pub struct Server {
    pub addr: SocketAddr,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    pub in_flight: Arc<AtomicUsize>,
    pub peak: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    /// `handler(request, hit)`: `hit` is 1 for the first request to that path.
    pub async fn start<F>(handler: F) -> Self
    where
        F: Fn(&Seen, usize) -> Response + Send + Sync + 'static,
    {
        Self::start_with_delay(handler, None).await
    }

    /// Like [`start`], holding every response for `delay` (to observe concurrency).
    pub async fn start_with_delay<F>(handler: F, delay: Option<std::time::Duration>) -> Self
    where
        F: Fn(&Seen, usize) -> Response + Send + Sync + 'static,
    {
        let handler: Arc<Handler> = Arc::new(handler);
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let hits: Arc<Mutex<BTreeMap<String, usize>>> = Arc::default();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let app = {
            let (seen, hits, in_flight, peak) =
                (seen.clone(), hits.clone(), in_flight.clone(), peak.clone());
            axum::Router::new().fallback(move |req: Request| {
                let (handler, seen, hits, in_flight, peak) = (
                    handler.clone(),
                    seen.clone(),
                    hits.clone(),
                    in_flight.clone(),
                    peak.clone(),
                );
                async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    let (parts, body) = req.into_parts();
                    let body = axum::body::to_bytes(body, usize::MAX)
                        .await
                        .map(|b| b.to_vec())
                        .unwrap_or_default();
                    let s = Seen {
                        method: parts.method.clone(),
                        uri: parts.uri.to_string(),
                        path: parts.uri.path().to_owned(),
                        headers: parts.headers.clone(),
                        body,
                        at: Instant::now(),
                    };
                    let hit = {
                        let mut h = hits.lock();
                        let n = h.entry(s.path.clone()).or_insert(0);
                        *n += 1;
                        *n
                    };
                    seen.lock().push(s.clone());
                    if let Some(d) = delay {
                        tokio::time::sleep(d).await;
                    }
                    let resp = handler(&s, hit);
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    resp
                }
            })
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            seen,
            in_flight,
            peak,
            task,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }

    pub fn requests_to(&self, path: &str) -> Vec<Seen> {
        self.seen
            .lock()
            .iter()
            .filter(|s| s.path == path)
            .cloned()
            .collect()
    }
}

/// A TCP server that accepts connections and closes them without answering.
pub async fn dropping_server() -> (SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            c.fetch_add(1, Ordering::SeqCst);
            drop(sock);
        }
    });
    (addr, count, task)
}

pub fn json(status: u16, body: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub fn status(status: u16) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .body(Body::from("busy"))
        .unwrap()
}

pub fn with_header(mut r: Response, name: &'static str, value: &str) -> Response {
    r.headers_mut().insert(name, value.parse().unwrap());
    r
}

/// A one-version packument whose tarball is served by the same registry.
pub fn packument(base: &str, name: &str, version: &str) -> serde_json::Value {
    let unscoped = name.rsplit('/').next().unwrap();
    serde_json::json!({
        "name": name,
        "dist-tags": { "latest": version },
        "versions": {
            version: {
                "name": name,
                "version": version,
                "dist": { "tarball": format!("{base}{name}/-/{unscoped}-{version}.tgz") }
            }
        }
    })
}

pub fn scope(url: &str, auth: Auth) -> RegistryScope {
    RegistryScope {
        url: url.to_owned(),
        auth,
        email: None,
        cert_file: None,
        key_file: None,
    }
}

pub fn config(registry: &str, cache_dir: &Path) -> Config {
    Config {
        cwd: cache_dir.to_path_buf(),
        project_dir: cache_dir.to_path_buf(),
        home_dir: cache_dir.to_path_buf(),
        store_dir: cache_dir.join("store"),
        cache_dir: cache_dir.join("cache"),
        global_dir: cache_dir.join("global"),
        global_bin_dir: cache_dir.join("bin"),
        registry: scope(registry, Auth::None),
        scopes: BTreeMap::new(),
        url_auth: Vec::new(),
        network: NetworkConfig {
            concurrency: 64,
            retries: 5,
            retry_backoff_ms: 1,
            idle_timeout_secs: 300,
            offline: OfflineMode::Online,
            manifest_cache: ManifestCacheMode::CacheControl,
            proxy: ProxyConfig::default(),
            tls: TlsConfig {
                reject_unauthorized: true,
                ..TlsConfig::default()
            },
            user_agent: "fnpm-test/1.0".to_owned(),
        },
        linker: LinkerSetting::Auto,
        backend: None,
        global_store: true,
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
        ..Config::defaults(cache_dir)
    }
}

/// An event sink that records warnings.
#[derive(Default)]
pub struct Warnings(pub Mutex<Vec<String>>);

impl fnpm_types::EventSink for Warnings {
    fn emit(&self, event: fnpm_types::Event) {
        if let fnpm_types::Event::Warn { message, .. } = event {
            self.0.lock().push(message);
        }
    }
}
