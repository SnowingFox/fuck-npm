//! Manifest, cache, retry, auth, proxy and tarball behaviour against an in-process registry.
//! Ports the relevant cases of bun's `bun-install-registry.test.ts`, `bun-install-retry.test.ts`,
//! `bun-install-offline.test.ts` and `bun-install-proxy.test.ts`.

mod support;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use fnpm_config::{Auth, Config, Env, ManifestCacheMode, OfflineMode};
use fnpm_registry::{ACCEPT_ABBREVIATED, ACCEPT_FULL, Error, ManifestRequest, RegistryClient};
use futures_util::StreamExt as _;
use support::*;

fn client(config: Config) -> RegistryClient {
    RegistryClient::with_env(Arc::new(config), Env::default()).unwrap()
}

fn client_env(config: Config, env: &[(&str, &str)]) -> RegistryClient {
    RegistryClient::with_env(Arc::new(config), Env::from_pairs(env.iter().copied())).unwrap()
}

#[tokio::test]
async fn fetches_abbreviated_manifest_and_caches_it_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        with_header(
            json(200, packument(&base, "bar", "0.0.2")),
            "etag",
            "\"v1\"",
        )
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    let m = c.manifest("bar", ManifestRequest::default()).await.unwrap();
    assert_eq!(m.name, "bar");
    assert_eq!(m.versions.len(), 1);
    assert_eq!(m.etag.as_deref(), Some("\"v1\""));
    assert_eq!(m.max_age_secs, 300);
    assert!(!m.is_full);
    let req = &server.requests()[0];
    assert_eq!(req.path, "/bar");
    assert_eq!(req.header("accept").as_deref(), Some(ACCEPT_ABBREVIATED));
    assert_eq!(req.header("user-agent").as_deref(), Some("fnpm-test/1.0"));
    assert!(req.header("authorization").is_none());

    // Memoised: no second request.
    let again = c.manifest("bar", ManifestRequest::default()).await.unwrap();
    assert!(Arc::ptr_eq(&m, &again));
    c.flush_cache_writes().await;
    let cache = c.manifest_cache().unwrap();
    assert!(cache.path_for(&server.url(), "bar").exists());

    // A new client (a new run) reads the fresh entry without a request.
    let c2 = client(config(&server.url(), tmp.path()));
    let cached = c2
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(*cached, *m);
    assert_eq!(server.requests().len(), 1);
    assert_eq!(c2.stats().manifest_cache_hits, 1);
    assert_eq!(c2.stats().manifest_requests, 0);
}

#[tokio::test]
async fn revalidates_with_etag_and_reuses_on_304() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, hit| {
        if hit > 1 {
            assert_eq!(s.header("if-none-match").as_deref(), Some("\"abc\""));
            assert!(s.header("if-modified-since").is_none());
            return with_header(status(304), "cache-control", "max-age=60");
        }
        let base = format!("http://{}/", s.header("host").unwrap());
        let r = with_header(
            json(200, packument(&base, "bar", "1.0.0")),
            "etag",
            "\"abc\"",
        );
        let r = with_header(r, "last-modified", "Mon, 01 Jan 2024 00:00:00 GMT");
        with_header(r, "cache-control", "public, max-age=0")
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    let first = c.manifest("bar", ManifestRequest::default()).await.unwrap();
    assert_eq!(first.max_age_secs, 0);
    c.flush_cache_writes().await;

    let c2 = client(config(&server.url(), tmp.path()));
    let second = c2
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(second.versions, first.versions);
    assert_eq!(second.max_age_secs, 60);
    assert!(second.fetched_at_ms >= first.fetched_at_ms);
    assert_eq!(c2.stats().not_modified, 1);
    c2.flush_cache_writes().await;

    // The 304 refreshed the cache entry: a third run uses it without asking.
    let c3 = client(config(&server.url(), tmp.path()));
    c3.manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn falls_back_to_if_modified_since_without_etag() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, hit| {
        if hit > 1 {
            assert_eq!(
                s.header("if-modified-since").as_deref(),
                Some("Mon, 01 Jan 2024 00:00:00 GMT")
            );
            return status(304);
        }
        let base = format!("http://{}/", s.header("host").unwrap());
        let r = with_header(
            json(200, packument(&base, "bar", "1.0.0")),
            "last-modified",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        );
        with_header(r, "cache-control", "no-cache")
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    c.flush_cache_writes().await;
    let c2 = client(config(&server.url(), tmp.path()));
    c2.manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(c2.stats().not_modified, 1);
}

#[tokio::test]
async fn cache_modes_force_and_revalidate() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        with_header(json(200, packument(&base, "bar", "1.0.0")), "etag", "\"e\"")
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    c.flush_cache_writes().await;

    // `revalidate` asks again even though the entry is fresh.
    let c2 = client(config(&server.url(), tmp.path()));
    c2.manifest(
        "bar",
        ManifestRequest {
            full: false,
            revalidate: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(server.requests().len(), 2);
    assert_eq!(
        server.requests()[1].header("if-none-match").as_deref(),
        Some("\"e\"")
    );

    // `--force` ignores cache-control.
    let mut forced = config(&server.url(), tmp.path());
    forced.force = true;
    client(forced)
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 3);

    // BUN_MANIFEST_CACHE=1: cache but always revalidate.
    let mut nocc = config(&server.url(), tmp.path());
    nocc.network.manifest_cache = ManifestCacheMode::NoCacheControl;
    client(nocc)
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 4);

    // --no-cache: no disk cache at all, no conditional headers.
    let mut disabled = config(&server.url(), tmp.path());
    disabled.network.manifest_cache = ManifestCacheMode::Disabled;
    let c5 = client(disabled);
    assert!(c5.manifest_cache().is_none());
    c5.manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 5);
    assert!(server.requests()[4].header("if-none-match").is_none());
}

#[tokio::test]
async fn offline_and_prefer_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        with_header(
            json(200, packument(&base, "bar", "1.0.0")),
            "cache-control",
            "max-age=0",
        )
    })
    .await;
    let mut offline = config(&server.url(), tmp.path());
    offline.network.offline = OfflineMode::Offline;
    let e = client(offline.clone())
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Offline(ref n) if n == "bar"), "{e}");
    assert_eq!(
        server.requests().len(),
        0,
        "--offline never issues a request"
    );

    let c = client(config(&server.url(), tmp.path()));
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    c.flush_cache_writes().await;
    assert_eq!(server.requests().len(), 1);

    // Stale, but --offline and --prefer-offline use any age.
    client(offline.clone())
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    let mut prefer = config(&server.url(), tmp.path());
    prefer.network.offline = OfflineMode::PreferOffline;
    client(prefer.clone())
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 1);
    // --offline even accepts an abbreviated entry for a full request; prefer-offline fetches.
    client(offline.clone())
        .manifest(
            "bar",
            ManifestRequest {
                full: true,
                revalidate: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 1);
    client(prefer)
        .manifest(
            "bar",
            ManifestRequest {
                full: true,
                revalidate: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 2);
    assert_eq!(
        server.requests()[1].header("accept").as_deref(),
        Some(ACCEPT_FULL)
    );

    // A tarball under --offline is never requested.
    let e = client(offline)
        .tarball(&format!("{}bar/-/bar-1.0.0.tgz", server.url()), None)
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Offline(_)));
}

#[tokio::test]
async fn cached_manifest_never_touches_the_network() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        json(200, packument(&base, "bar", "1.0.0"))
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    assert!(c.cached_manifest("bar").is_none());
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    c.flush_cache_writes().await;
    assert!(c.cached_manifest("bar").is_some());
    let c2 = client(config(&server.url(), tmp.path()));
    assert!(c2.cached_manifest("bar").is_some());
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn full_manifests_for_release_age_and_libc_names() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        let name = s.path.trim_start_matches('/').replace("%2f", "/");
        let mut doc = packument(&base, &name, "1.0.0");
        doc["time"] = serde_json::json!({ "1.0.0": "2020-01-01T00:00:00.000Z" });
        json(200, doc)
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    let full = c
        .manifest(
            "bar",
            ManifestRequest {
                full: true,
                revalidate: false,
            },
        )
        .await
        .unwrap();
    assert!(full.is_full);
    assert_eq!(full.time.get("1.0.0"), Some(&1_577_836_800_000));
    assert_eq!(
        server.requests()[0].header("accept").as_deref(),
        Some(ACCEPT_FULL)
    );
    // A full memo entry serves abbreviated requests too.
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    assert_eq!(server.requests().len(), 1);

    let libc = c
        .manifest("@scope/pkg-linux-x64-gnu", ManifestRequest::default())
        .await
        .unwrap();
    assert!(libc.is_full);
    let req = &server.requests()[1];
    assert_eq!(req.path, "/@scope%2fpkg-linux-x64-gnu");
    assert_eq!(req.header("accept").as_deref(), Some(ACCEPT_FULL));
}

#[tokio::test]
async fn full_document_for_libc_falls_back_to_abbreviated() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        if s.header("accept").as_deref() == Some(ACCEPT_FULL) {
            return status(503);
        }
        let base = format!("http://{}/", s.header("host").unwrap());
        json(200, packument(&base, "x-linux-arm64", "1.0.0"))
    })
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.network.retries = 1;
    let m = client(cfg)
        .manifest("x-linux-arm64", ManifestRequest::default())
        .await
        .unwrap();
    assert!(!m.is_full);
    let accepts: Vec<_> = server
        .requests()
        .iter()
        .map(|r| r.header("accept").unwrap())
        .collect();
    assert_eq!(accepts, vec![ACCEPT_FULL, ACCEPT_FULL, ACCEPT_ABBREVIATED]);
}

#[tokio::test]
async fn not_found_and_status_mapping() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| match s.path.as_str() {
        "/missing" => json(404, serde_json::json!({ "error": "Not found" })),
        "/denied" => status(403),
        "/broken" => Response::builder()
            .status(200)
            .body(Body::from("{not json"))
            .unwrap(),
        "/error-doc" => json(200, serde_json::json!({ "error": "you shall not pass" })),
        _ => status(500),
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    let e = c
        .manifest("missing", ManifestRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotFound(ref n) if n == "missing"), "{e}");
    // Remembered for the run.
    assert!(matches!(
        c.manifest("missing", ManifestRequest::default()).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(server.requests_to("/missing").len(), 1);
    assert!(matches!(
        c.manifest("denied", ManifestRequest::default()).await,
        Err(Error::Status { status: 403, .. })
    ));
    assert!(matches!(
        c.manifest("broken", ManifestRequest::default()).await,
        Err(Error::InvalidManifest { .. })
    ));
    let e = c
        .manifest("error-doc", ManifestRequest::default())
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("npm error: you shall not pass"),
        "{e}"
    );
    assert!(c.manifest("", ManifestRequest::default()).await.is_err());
}

#[tokio::test]
async fn concurrent_requests_for_one_manifest_are_deduplicated() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start_with_delay(
        |s, _| {
            let base = format!("http://{}/", s.header("host").unwrap());
            json(200, packument(&base, "bar", "1.0.0"))
        },
        Some(Duration::from_millis(100)),
    )
    .await;
    let c = client(config(&server.url(), tmp.path()));
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let c = c.clone();
            tokio::spawn(async move { c.manifest("bar", ManifestRequest::default()).await })
        })
        .collect();
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap().unwrap());
    }
    assert_eq!(server.requests().len(), 1);
    assert!(results.windows(2).all(|w| Arc::ptr_eq(&w[0], &w[1])));
    // Errors are shared too.
    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let c = c.clone();
            tokio::spawn(async move {
                c.manifest(
                    "@x/y",
                    ManifestRequest {
                        full: true,
                        revalidate: false,
                    },
                )
                .await
                .map(|m| m.name.clone())
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap(), "@x/y");
    }
}

#[tokio::test]
async fn prefetch_fills_the_memo() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        let name = s.path.trim_start_matches('/').to_owned();
        json(200, packument(&base, &name, "1.0.0"))
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    c.prefetch(["a".to_owned(), "b".to_owned(), "c".to_owned()]);
    for _ in 0..200 {
        if server.requests().len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for n in ["a", "b", "c"] {
        c.manifest(n, ManifestRequest::default()).await.unwrap();
    }
    assert_eq!(server.requests().len(), 3);
}

fn flaky(
    fails: usize,
    code: u16,
    retry_after: Option<&'static str>,
) -> impl Fn(&Seen, usize) -> Response + Send + Sync {
    move |s, hit| {
        if hit <= fails {
            let r = status(code);
            return match retry_after {
                Some(v) => with_header(r, "retry-after", v),
                None => r,
            };
        }
        let base = format!("http://{}/", s.header("host").unwrap());
        if s.path.ends_with(".tgz") {
            return Response::builder()
                .status(200)
                .body(Body::from("tarball-bytes"))
                .unwrap();
        }
        json(200, packument(&base, "bar", "0.0.2"))
    }
}

#[tokio::test]
async fn retries_on_500_with_growing_backoff() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(flaky(3, 503, None)).await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.network.retry_backoff_ms = 50;
    let c = client(cfg);
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    let at: Vec<_> = server.requests().iter().map(|r| r.at).collect();
    assert_eq!(at.len(), 4);
    let gaps: Vec<u128> = at.windows(2).map(|w| (w[1] - w[0]).as_millis()).collect();
    // 50, 100 and 200 ms, each less 20% at most.
    assert!(gaps[0] >= 40 && gaps[1] >= 80 && gaps[2] >= 160, "{gaps:?}");
    assert_eq!(c.stats().retries, 3);
}

#[tokio::test]
async fn status_codes_that_are_retried() {
    for (code, requests, ok) in [
        (408, 2, true),
        (429, 2, true),
        (500, 2, true),
        (400, 1, false),
        (401, 1, false),
        (404, 1, false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let server = Server::start(flaky(1, code, None)).await;
        let c = client(config(&server.url(), tmp.path()));
        let r = c.manifest("bar", ManifestRequest::default()).await;
        assert_eq!(
            (code, server.requests().len(), r.is_ok()),
            (code, requests, ok)
        );

        let server = Server::start(flaky(1, code, None)).await;
        let c = client(config(&server.url(), tmp.path()));
        let r = c
            .tarball_bytes(&format!("{}bar/-/bar-0.0.2.tgz", server.url()))
            .await;
        assert_eq!(
            (code, server.requests().len(), r.is_ok()),
            (code, requests, ok),
            "tarball"
        );
    }
}

#[tokio::test]
async fn gives_up_after_the_configured_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(flaky(100, 503, None)).await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.network.retries = 1;
    let e = client(cfg)
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Status { status: 503, .. }), "{e}");
    assert_eq!(server.requests().len(), 2);
    // The retry count env var wins.
    let server = Server::start(flaky(100, 503, None)).await;
    let c = client_env(
        config(&server.url(), tmp.path()),
        &[("BUN_CONFIG_HTTP_RETRY_COUNT", "2")],
    );
    c.manifest("bar", ManifestRequest::default())
        .await
        .unwrap_err();
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn honours_retry_after_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(flaky(1, 429, Some("1"))).await;
    let c = client(config(&server.url(), tmp.path()));
    let warnings = Arc::new(Warnings::default());
    c.set_event_sink(warnings.clone());
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    let at: Vec<_> = server.requests().iter().map(|r| r.at).collect();
    assert!(
        (at[1] - at[0]) >= Duration::from_millis(950),
        "{:?}",
        at[1] - at[0]
    );
    assert_eq!(
        *warnings.0.lock(),
        vec!["registry asked to wait 1s before retrying".to_owned()]
    );
}

#[tokio::test]
async fn retry_sends_the_same_headers() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(flaky(2, 503, None)).await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.registry.auth = Auth::Token {
        token: "retry-token".into(),
    };
    let c = client(cfg);
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    let auths: Vec<_> = server
        .requests()
        .iter()
        .map(|r| r.header("authorization").unwrap())
        .collect();
    assert_eq!(auths, vec!["Bearer retry-token"; 3]);
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.header("npm-auth-type").as_deref() == Some("legacy"))
    );
}

#[tokio::test]
async fn dropped_connections_are_retried_and_throttle_the_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let (addr, connections, task) = dropping_server().await;
    let mut cfg = config(&format!("http://{addr}/"), tmp.path());
    cfg.network.retries = 2;
    cfg.network.concurrency = 8;
    let c = client(cfg);
    let e = c
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Network { .. }), "{e}");
    assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 3);
    // A retried request that failed again: a quarter less (8 -> 6, then 6 -> 5).
    assert!(c.concurrency_limit() < 8);
    task.abort();
}

#[tokio::test]
async fn idle_timeout_is_not_retried_for_blocking_requests_but_is_for_installs() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c2 = count.clone();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            held.push(sock);
        }
    });
    let mut cfg = config(&format!("http://{addr}/"), tmp.path());
    cfg.network.idle_timeout_secs = 1;
    cfg.network.retries = 1;
    let c = client(cfg);
    let warnings = Arc::new(Warnings::default());
    c.set_event_sink(warnings.clone());
    // whoami is blocking: one attempt.
    let mut whoami_cfg = config(&format!("http://{addr}/"), tmp.path());
    whoami_cfg.network.idle_timeout_secs = 1;
    whoami_cfg.registry.auth = Auth::Token { token: "t".into() };
    let w = client(whoami_cfg);
    assert!(w.whoami(None).await.is_err());
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    // A manifest of an install is retried once, and the first timeout warns.
    assert!(c.manifest("bar", ManifestRequest::default()).await.is_err());
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(
        *warnings.0.lock(),
        vec!["registry requests failed, retrying".to_owned()]
    );
    task.abort();
}

#[tokio::test]
async fn scoped_registries_and_their_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let main = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        json(200, packument(&base, "bar", "1.0.0"))
    })
    .await;
    let scoped = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        json(200, packument(&base, "@my/pkg", "1.0.0"))
    })
    .await;
    let mut cfg = config(&main.url(), tmp.path());
    cfg.registry.auth = Auth::Token {
        token: "main".into(),
    };
    cfg.scopes.insert(
        "@my".into(),
        scope(
            &format!("{}npm/", scoped.url()),
            Auth::Basic {
                username: "u".into(),
                password: "p".into(),
            },
        ),
    );
    let c = client(cfg);
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    c.manifest("@my/pkg", ManifestRequest::default())
        .await
        .unwrap();
    // `@myx/...` is not `@my`.
    c.manifest("@myx/pkg", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(scoped.requests().len(), 1);
    assert_eq!(scoped.requests()[0].path, "/npm/@my%2fpkg");
    assert_eq!(
        scoped.requests()[0].header("authorization").as_deref(),
        Some("Basic dTpw")
    );
    let main_reqs = main.requests();
    assert_eq!(main_reqs.len(), 2);
    assert!(
        main_reqs
            .iter()
            .all(|r| r.header("authorization").as_deref() == Some("Bearer main"))
    );
    assert_eq!(
        c.default_tarball_url("@my/pkg", "1.0.0"),
        format!("{}npm/@my/pkg/-/pkg-1.0.0.tgz", scoped.url())
    );
    assert_eq!(
        c.registry_url_for("@my/pkg"),
        format!("{}npm/", scoped.url())
    );
}

#[tokio::test]
async fn embedded_registry_credentials_are_stripped_and_used() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        json(200, packument(&base, "bar", "1.0.0"))
    })
    .await;
    let registry = format!("http://{}/api/npm/:_authToken=SECRET/", server.addr);
    let c = client(config(&registry, tmp.path()));
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    let r = &server.requests()[0];
    assert_eq!(r.path, "/api/npm/bar");
    assert_eq!(r.header("authorization").as_deref(), Some("Bearer SECRET"));

    let userinfo = format!("http://me:pw@{}/", server.addr);
    let c = client(config(&userinfo, tmp.path()));
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    assert_eq!(
        server.requests()[1].header("authorization").as_deref(),
        Some("Basic bWU6cHc=")
    );
    assert_eq!(c.registry_url_for("bar"), server.url());
}

#[tokio::test]
async fn tarball_credentials_are_scoped_to_their_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = Server::start(|_, _| {
        Response::builder()
            .status(200)
            .body(Body::from("r"))
            .unwrap()
    })
    .await;
    let other = Server::start(|_, _| {
        Response::builder()
            .status(200)
            .body(Body::from("o"))
            .unwrap()
    })
    .await;
    let mut cfg = config(&registry.url(), tmp.path());
    cfg.registry.auth = Auth::Token {
        token: "reg".into(),
    };
    let c = client(cfg);
    assert_eq!(
        &c.tarball_bytes(&format!("{}bar/-/bar-1.0.0.tgz", registry.url()))
            .await
            .unwrap()[..],
        b"r"
    );
    assert_eq!(
        registry.requests()[0].header("authorization").as_deref(),
        Some("Bearer reg")
    );
    c.tarball_bytes(&format!("{}bar-1.0.0.tgz", other.url()))
        .await
        .unwrap();
    assert!(
        other.requests()[0].header("authorization").is_none(),
        "never sent to another host"
    );
    // URL userinfo becomes Basic and leaves the URL.
    c.tarball_bytes(&format!("http://user:pa%20ss@{}/x.tgz", other.addr))
        .await
        .unwrap();
    let r = &other.requests()[1];
    assert_eq!(r.path, "/x.tgz");
    assert_eq!(
        r.header("authorization").as_deref(),
        Some("Basic dXNlcjpwYSBzcw==")
    );
    // Configured credentials win over the URL's.
    c.tarball_bytes(&format!("http://user:pw@{}/y.tgz", registry.addr))
        .await
        .unwrap();
    assert_eq!(
        registry.requests()[1].header("authorization").as_deref(),
        Some("Bearer reg")
    );
}

#[tokio::test]
async fn npmrc_url_credentials_only_over_https_for_tarballs() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = Server::start(|_, _| {
        Response::builder()
            .status(200)
            .body(Body::from("r"))
            .unwrap()
    })
    .await;
    let mut cfg = config(&registry.url(), tmp.path());
    cfg.url_auth.push((
        format!("//{}/", registry.addr),
        Auth::Token {
            token: "npmrc".into(),
        },
    ));
    let c = client(cfg);
    // The registry itself (http) gets its own `.npmrc` line for manifests.
    c.manifest("bar", ManifestRequest::default()).await.ok();
    assert_eq!(
        registry.requests()[0].header("authorization").as_deref(),
        Some("Bearer npmrc")
    );
    // A tarball on the registry's origin uses the registry's credentials.
    c.tarball_bytes(&format!("{}bar.tgz", registry.url()))
        .await
        .unwrap();
    assert_eq!(
        registry.requests()[1].header("authorization").as_deref(),
        Some("Bearer npmrc")
    );
}

#[tokio::test]
async fn manifests_go_through_the_proxy_unless_no_proxy() {
    let tmp = tempfile::tempdir().unwrap();
    let proxy = Server::start(|s, _| {
        // A forward proxy sees the absolute URI.
        assert!(s.uri.starts_with("http://registry.invalid/"), "{}", s.uri);
        json(200, packument("http://registry.invalid/", "bar", "1.0.0"))
    })
    .await;
    let c = client_env(
        config("http://registry.invalid/", tmp.path()),
        &[
            ("HTTP_PROXY", &format!("http://u:p@{}", proxy.addr)),
            ("NO_PROXY", "localhost,.example.org"),
        ],
    );
    let m = c.manifest("bar", ManifestRequest::default()).await.unwrap();
    assert_eq!(m.versions.len(), 1);
    let r = &proxy.requests()[0];
    assert_eq!(
        r.header("proxy-authorization").as_deref(),
        Some("Basic dTpw")
    );

    let direct = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        json(200, packument(&base, "bar", "1.0.0"))
    })
    .await;
    let c = client_env(
        config(
            &format!("http://localhost:{}/", direct.addr.port()),
            tmp.path(),
        ),
        &[
            ("http_proxy", &format!("http://{}", proxy.addr)),
            ("no_proxy", "localhost"),
        ],
    );
    c.manifest("bar", ManifestRequest::default()).await.unwrap();
    assert_eq!(direct.requests().len(), 1);
    assert_eq!(proxy.requests().len(), 1);
}

fn tarball_payload() -> Vec<u8> {
    (0..200_000u32).map(|i| (i % 251) as u8).collect()
}

/// Serves the payload, breaking the first `breaks` responses after `cut` bytes. With `ranges`,
/// a `Range: bytes=N-` request gets a 206 with the rest; without, a full 200.
fn breaking_tarball(
    breaks: usize,
    cut: usize,
    ranges: bool,
) -> impl Fn(&Seen, usize) -> Response + Send + Sync {
    let payload = tarball_payload();
    move |s, hit| {
        let start = s
            .header("range")
            .and_then(|r| {
                r.strip_prefix("bytes=")
                    .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
            })
            .filter(|_| ranges)
            .unwrap_or(0);
        let rest = payload[start..].to_vec();
        let total = payload.len();
        let mut builder = Response::builder().header("content-length", rest.len().to_string());
        builder = if start > 0 {
            builder.status(206).header(
                "content-range",
                format!("bytes {start}-{}/{total}", total - 1),
            )
        } else {
            builder.status(200)
        };
        if hit <= breaks {
            let first = bytes::Bytes::from(rest[..cut.min(rest.len())].to_vec());
            // The first part is flushed before the connection breaks.
            let stream = futures_util::stream::unfold(Some(first), |state| async move {
                match state {
                    Some(first) => Some((Ok::<_, std::io::Error>(first), None)),
                    None => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Some((Err(std::io::Error::other("connection reset")), None))
                    }
                }
            });
            return builder.body(Body::from_stream(stream)).unwrap();
        }
        builder.body(Body::from(rest)).unwrap()
    }
}

#[tokio::test]
async fn tarball_streams_and_reports_length() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(breaking_tarball(0, 0, true)).await;
    let c = client(config(&server.url(), tmp.path()));
    let body = c
        .tarball(&format!("{}pkg/-/pkg-1.0.0.tgz", server.url()), None)
        .await
        .unwrap();
    assert_eq!(body.content_length, Some(200_000));
    assert_eq!(body.url, format!("{}pkg/-/pkg-1.0.0.tgz", server.url()));
    let mut got = Vec::new();
    let mut chunks = 0;
    let mut stream = body.stream;
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.unwrap());
        chunks += 1;
    }
    assert!(chunks >= 1);
    assert_eq!(got, tarball_payload());
    assert_eq!(c.stats().tarball_requests, 1);
    assert!(c.stats().bytes_downloaded >= 200_000);
}

#[tokio::test]
async fn tarball_resumes_with_range_after_a_reset() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(breaking_tarball(2, 70_000, true)).await;
    let c = client(config(&server.url(), tmp.path()));
    let bytes = c
        .tarball_bytes(&format!("{}pkg.tgz", server.url()))
        .await
        .unwrap();
    assert_eq!(&bytes[..], &tarball_payload()[..]);
    let ranges: Vec<_> = server
        .requests()
        .iter()
        .map(|r| r.header("range"))
        .collect();
    assert_eq!(
        ranges,
        vec![
            None,
            Some("bytes=70000-".to_owned()),
            Some("bytes=140000-".to_owned())
        ]
    );
}

#[tokio::test]
async fn tarball_resume_without_range_support_skips_delivered_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(breaking_tarball(1, 50_000, false)).await;
    let c = client(config(&server.url(), tmp.path()));
    let bytes = c
        .tarball_bytes(&format!("{}pkg.tgz", server.url()))
        .await
        .unwrap();
    assert_eq!(&bytes[..], &tarball_payload()[..]);
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn tarball_gives_up_mid_body_after_the_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(breaking_tarball(100, 1000, true)).await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.network.retries = 2;
    let c = client(cfg);
    let e = c
        .tarball_bytes(&format!("{}pkg.tgz", server.url()))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Network { .. }), "{e}");
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn tarball_url_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let c = client(config("http://127.0.0.1:9/", tmp.path()));
    let e = c.tarball("file:///etc/passwd", None).await.unwrap_err();
    assert!(
        e.to_string()
            .contains("Expected tarball URL to start with https:// or http://"),
        "{e}"
    );
    let server = Server::start(|_, _| status(404)).await;
    let c = client(config(&server.url(), tmp.path()));
    assert!(matches!(
        c.tarball_bytes(&format!("{}gone.tgz", server.url())).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn network_concurrency_limits_requests_in_flight() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start_with_delay(
        |s, _| {
            let base = format!("http://{}/", s.header("host").unwrap());
            let name = s.path.trim_start_matches('/').to_owned();
            json(200, packument(&base, &name, "1.0.0"))
        },
        Some(Duration::from_millis(30)),
    )
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.network.concurrency = 3;
    let c = client(cfg);
    let tasks: Vec<_> = (0..12)
        .map(|i| {
            let c = c.clone();
            tokio::spawn(async move {
                c.manifest(&format!("p{i}"), ManifestRequest::default())
                    .await
                    .map(|_| ())
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(server.requests().len(), 12);
    let peak = server.peak.load(std::sync::atomic::Ordering::SeqCst);
    assert!((2..=3).contains(&peak), "peak {peak}");
}

#[tokio::test]
async fn manifests_are_sent_before_queued_tarballs() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start_with_delay(
        |s, _| {
            if s.path.ends_with(".tgz") {
                return Response::builder()
                    .status(200)
                    .body(Body::from("t"))
                    .unwrap();
            }
            let base = format!("http://{}/", s.header("host").unwrap());
            json(200, packument(&base, "m", "1.0.0"))
        },
        Some(Duration::from_millis(40)),
    )
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.network.concurrency = 1;
    let c = client(cfg);
    let base = server.url();
    let mut tasks = Vec::new();
    // One tarball takes the only slot; then 3 tarballs and 2 manifests queue up.
    for i in 0..4 {
        let (c, base) = (c.clone(), base.clone());
        tasks.push(tokio::spawn(async move {
            c.tarball_bytes(&format!("{base}t{i}.tgz"))
                .await
                .map(|_| ())
        }));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for name in ["m1", "m2"] {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            c.manifest(name, ManifestRequest::default())
                .await
                .map(|_| ())
        }));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let order: Vec<_> = server.requests().iter().map(|r| r.path.clone()).collect();
    assert_eq!(order[0], "/t0.tgz");
    assert_eq!(&order[1..3], ["/m1", "/m2"], "{order:?}");
}

#[tokio::test]
async fn github_commits_and_tarball_urls() {
    let tmp = tempfile::tempdir().unwrap();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let server = Server::start(move |s, _| {
        assert_eq!(
            s.header("accept").as_deref(),
            Some("application/vnd.github.sha")
        );
        match s.path.as_str() {
            "/repos/oven-sh/bun/commits/main" => {
                assert_eq!(
                    s.header("authorization").as_deref(),
                    Some("Bearer gh-token")
                );
                Response::builder()
                    .status(200)
                    .body(Body::from(format!("{sha}\n")))
                    .unwrap()
            }
            "/repos/oven-sh/bun/commits/HEAD" => Response::builder()
                .status(200)
                .body(Body::from(sha.to_uppercase()))
                .unwrap(),
            _ => status(404),
        }
    })
    .await;
    let api = format!("http://{}", server.addr);
    let c = client_env(
        config("https://registry.npmjs.org/", tmp.path()),
        &[
            ("GITHUB_API_URL", &format!("{api}/")),
            ("GITHUB_TOKEN", "gh-token"),
        ],
    );
    assert_eq!(
        c.resolve_github("oven-sh", "bun", "main").await.unwrap(),
        sha
    );
    assert_eq!(c.resolve_github("oven-sh", "bun", "").await.unwrap(), sha);
    assert!(matches!(
        c.resolve_github("oven-sh", "bun", "nope").await,
        Err(Error::NotFound(_))
    ));
    // A full SHA needs no request.
    let before = server.requests().len();
    assert_eq!(
        c.resolve_github("o", "r", &sha.to_uppercase())
            .await
            .unwrap(),
        sha
    );
    assert_eq!(server.requests().len(), before);
    assert_eq!(
        c.github_tarball_url("o", "r", sha),
        format!("{api}/repos/o/r/tarball/{sha}")
    );
    assert_eq!(
        c.github_tarball_url("o", "", sha),
        format!("{api}/repos/o/tarball/{sha}")
    );
    let default = client(config("https://registry.npmjs.org/", tmp.path()));
    assert_eq!(
        default.github_tarball_url("o", "r", "abc"),
        "https://api.github.com/repos/o/r/tarball/abc"
    );
}

#[tokio::test]
async fn manifests_ask_for_compression_and_are_decoded() {
    use std::io::Write as _;
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        let base = format!("http://{}/", s.header("host").unwrap());
        let body = packument(&base, "zipped", "1.2.3").to_string();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(body.as_bytes()).unwrap();
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(Body::from(gz.finish().unwrap()))
            .unwrap()
    })
    .await;
    let c = client(config(&server.url(), tmp.path()));
    let m = c
        .manifest("zipped", ManifestRequest::default())
        .await
        .unwrap();
    assert_eq!(m.versions.len(), 1);
    assert_eq!(m.dist_tags["latest"], "1.2.3");
    let accept = server.requests()[0].header("accept-encoding").unwrap();
    for enc in ["gzip", "br", "zstd"] {
        assert!(accept.contains(enc), "{accept}");
    }
}

#[tokio::test]
async fn tarballs_are_requested_without_content_encoding() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(breaking_tarball(0, 0, true)).await;
    let c = client(config(&server.url(), tmp.path()));
    let bytes = c
        .tarball_bytes(&format!("{}pkg/-/pkg-1.0.0.tgz", server.url()))
        .await
        .unwrap();
    assert_eq!(bytes.len(), 200_000);
    assert_eq!(
        server.requests()[0].header("accept-encoding").as_deref(),
        Some("identity")
    );
}
