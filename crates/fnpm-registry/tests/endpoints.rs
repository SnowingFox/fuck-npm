//! audit, publish (OTP and web login), whoami, view and generic JSON requests. Ports cases of
//! bun's `bun-audit.test.ts`, `bun-publish.test.ts` and `bun-install-proxy.test.ts`.

mod support;

use std::io::Read as _;
use std::sync::Arc;

use fnpm_config::{Auth, Env};
use fnpm_registry::{
    AuditPackage, Error, OtpPrompt, PublishOptions, PublishPackage, RegistryClient, SkipReason,
};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json as j};
use support::*;

fn client(config: fnpm_config::Config) -> RegistryClient {
    RegistryClient::with_env(Arc::new(config), Env::default()).unwrap()
}

fn gunzip(body: &[u8]) -> Value {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .read_to_end(&mut out)
        .unwrap();
    serde_json::from_slice(&out).unwrap()
}

#[tokio::test]
async fn audit_groups_by_registry_and_merges() {
    let tmp = tempfile::tempdir().unwrap();
    let main = Server::start(|s, _| {
        assert_eq!(s.path, "/-/npm/v1/security/advisories/bulk");
        assert_eq!(s.header("content-encoding").as_deref(), Some("gzip"));
        assert_eq!(
            s.header("content-type").as_deref(),
            Some("application/json")
        );
        assert_eq!(
            gunzip(&s.body),
            j!({ "a": ["1.0.0", "1.1.0"], "b": ["2.0.0"] })
        );
        json(200, j!({ "a": [{ "id": 1, "severity": "high" }] }))
    })
    .await;
    let scoped = Server::start(|s, _| {
        assert_eq!(s.path, "/npm/-/npm/v1/security/advisories/bulk");
        assert_eq!(s.header("authorization").as_deref(), Some("Bearer scoped"));
        assert_eq!(gunzip(&s.body), j!({ "@s/x": ["0.1.0"] }));
        json(200, j!({ "@s/x": [{ "id": 2 }] }))
    })
    .await;
    let mut cfg = config(&main.url(), tmp.path());
    cfg.scopes.insert(
        "@s".into(),
        scope(
            &format!("{}npm/", scoped.url()),
            Auth::Token {
                token: "scoped".into(),
            },
        ),
    );
    let c = client(cfg);
    let pkgs = vec![
        AuditPackage {
            name: "a".into(),
            versions: vec!["1.0.0".into()],
        },
        AuditPackage {
            name: "@s/x".into(),
            versions: vec!["0.1.0".into()],
        },
        AuditPackage {
            name: "b".into(),
            versions: vec!["2.0.0".into()],
        },
        AuditPackage {
            name: "a".into(),
            versions: vec!["1.1.0".into(), "1.0.0".into()],
        },
    ];
    let r = c.audit(&pkgs).await.unwrap();
    assert_eq!(
        r.advisories,
        j!({ "a": [{ "id": 1, "severity": "high" }], "@s/x": [{ "id": 2 }] })
    );
    assert_eq!((r.checked, r.skipped), (3, 0));
    assert!(r.unaudited.is_empty());
}

#[tokio::test]
async fn audit_skips_a_failing_scoped_registry_but_not_the_default() {
    let tmp = tempfile::tempdir().unwrap();
    let main = Server::start(|_, _| json(200, j!({}))).await;
    let broken = Server::start(|_, _| status(404)).await;
    let mut cfg = config(&main.url(), tmp.path());
    cfg.scopes
        .insert("@s".into(), scope(&broken.url(), Auth::None));
    let c = client(cfg);
    let warnings = Arc::new(Warnings::default());
    c.set_event_sink(warnings.clone());
    let r = c
        .audit(&[AuditPackage {
            name: "@s/x".into(),
            versions: vec!["1.0.0".into()],
        }])
        .await
        .unwrap();
    assert_eq!(r.unaudited.len(), 1);
    assert_eq!(r.unaudited[0].packages, vec!["@s/x".to_owned()]);
    assert_eq!(r.unaudited[0].reason, "404");
    assert_eq!(r.unaudited[0].registry, broken.url().trim_end_matches('/'));
    assert_eq!((r.checked, r.skipped), (0, 1));
    assert_eq!(r.advisories, j!({}));
    // The commands report the skipped registries themselves.
    assert_eq!(warnings.0.lock().len(), 0);

    let bad_default = Server::start(|_, _| json(200, j!([]))).await;
    let c = client(config(&bad_default.url(), tmp.path()));
    let failure = c.audit(&[]).await.unwrap().default_failure.unwrap();
    assert_eq!(failure.reason, SkipReason::NotAdvisories);
    assert_eq!(&failure.body[..], b"[]");
    let down = Server::start(|_, _| status(500)).await;
    let mut cfg = config(&down.url(), tmp.path());
    cfg.network.retries = 1;
    let failure = client(cfg)
        .audit(&[])
        .await
        .unwrap()
        .default_failure
        .unwrap();
    assert_eq!(failure.reason, SkipReason::Status(500));
    // Safe to retry: the POST was sent twice.
    assert_eq!(down.requests().len(), 2);
}

#[tokio::test]
async fn whoami_from_config_or_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        assert_eq!(s.path, "/-/whoami");
        assert_eq!(s.header("npm-command").as_deref(), Some("whoami"));
        assert_eq!(s.header("npm-auth-type").as_deref(), Some("web"));
        assert!(
            s.header("user-agent")
                .unwrap()
                .starts_with("fnpm-test/1.0 ")
        );
        match s.header("authorization").as_deref() {
            Some("Bearer good") => with_header(
                json(200, j!({ "username": "alice" })),
                "npm-notice",
                "hello",
            ),
            _ => json(401, j!({ "error": "bad token" })),
        }
    })
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.registry.auth = Auth::Token {
        token: "good".into(),
    };
    assert_eq!(
        client(cfg.clone()).whoami(None).await.unwrap(),
        ("alice".to_owned(), Some("hello".to_owned()))
    );
    cfg.registry.auth = Auth::Token {
        token: "bad".into(),
    };
    let e = client(cfg.clone()).whoami(None).await.unwrap_err();
    assert!(
        matches!(e, Error::Registry { status: 401, ref message, .. } if message == "bad token"),
        "{e}"
    );
    cfg.registry.auth = Auth::None;
    assert!(matches!(
        client(cfg.clone()).whoami(None).await,
        Err(Error::NeedAuth(_))
    ));
    let before = server.requests().len();
    cfg.registry.auth = Auth::Basic {
        username: "bob".into(),
        password: "pw".into(),
    };
    assert_eq!(client(cfg.clone()).whoami(None).await.unwrap().0, "bob");
    let with_user = config(&format!("http://carol@{}/", server.addr), tmp.path());
    assert_eq!(client(with_user).whoami(None).await.unwrap().0, "carol");
    assert_eq!(
        server.requests().len(),
        before,
        "no request when the config names the user"
    );
}

#[tokio::test]
async fn view_and_request_json() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| match (s.method.as_str(), s.path.as_str()) {
        ("GET", "/@s%2fp") => {
            assert_eq!(s.header("accept").as_deref(), Some("application/json, */*"));
            json(200, j!({ "name": "@s/p", "readme": "hi" }))
        }
        ("PUT", "/-/package/x/dist-tags/beta") => {
            assert_eq!(s.header("npm-otp").as_deref(), Some("123456"));
            assert_eq!(s.header("npm-auth-type").as_deref(), Some("legacy"));
            assert_eq!(s.header("authorization").as_deref(), Some("Bearer t"));
            assert_eq!(
                serde_json::from_slice::<Value>(&s.body).unwrap(),
                j!("1.0.0")
            );
            json(201, j!({ "ok": true }))
        }
        ("DELETE", _) => json(500, j!({ "error": "boom" })),
        _ => status(404),
    })
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.registry.auth = Auth::Token { token: "t".into() };
    let c = client(cfg);
    assert_eq!(c.view("@s/p").await.unwrap()["readme"], "hi");
    assert!(matches!(c.view("nope").await, Err(Error::NotFound(_))));
    let url = format!("{}-/package/x/dist-tags/beta", server.url());
    let r = c
        .request_json("put", &url, Some(j!("1.0.0")), Some("123456"))
        .await
        .unwrap();
    assert_eq!(r, j!({ "ok": true }));
    // Not retried, and the message comes through.
    let e = c
        .request_json("DELETE", &url, None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::Registry { status: 500, ref message, .. } if message == "boom"),
        "{e}"
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|r| r.method == "DELETE")
            .count(),
        1
    );
}

struct Prompt {
    otp: Option<String>,
    web: Mutex<Vec<String>>,
}

impl OtpPrompt for Prompt {
    fn web_login(&self, auth_url: &str) {
        self.web.lock().push(auth_url.to_owned());
    }
    fn read_otp(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move { self.otp.clone() })
    }
}

fn package() -> PublishPackage<'static> {
    PublishPackage {
        name: "@s/p",
        version: "1.0.0",
        manifest_json: r#"{"name":"@s/p","version":"1.0.0"}"#,
        tarball: b"tgz",
    }
}

#[tokio::test]
async fn publish_puts_the_body_with_npm_headers() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| {
        assert_eq!(s.method, "PUT");
        assert_eq!(s.path, "/@s%2fp");
        assert_eq!(
            s.header("content-type").as_deref(),
            Some("application/json")
        );
        assert_eq!(s.header("npm-command").as_deref(), Some("publish"));
        assert_eq!(s.header("npm-auth-type").as_deref(), Some("web"));
        assert!(s.header("user-agent").unwrap().contains("workspaces/false"));
        let body: Value = serde_json::from_slice(&s.body).unwrap();
        assert_eq!(body["dist-tags"], j!({ "next": "1.0.0" }));
        assert_eq!(body["_attachments"]["@s/p-1.0.0.tgz"]["data"], "dGd6");
        with_header(json(200, j!({ "ok": true })), "npm-notice", "published")
    })
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.registry.auth = Auth::Token { token: "t".into() };
    let c = client(cfg.clone());
    let prompt = Prompt {
        otp: None,
        web: Mutex::default(),
    };
    let options = PublishOptions {
        tag: Some("next".into()),
        ..Default::default()
    };
    let r = c.publish(&package(), &options, &prompt).await.unwrap();
    assert_eq!(r.notice.as_deref(), Some("published"));

    cfg.registry.auth = Auth::None;
    let e = client(cfg)
        .publish(&package(), &options, &prompt)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("missing authentication"), "{e}");
}

#[tokio::test]
async fn publish_asks_for_an_otp_after_401() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| match s.header("npm-otp").as_deref() {
        None => with_header(json(401, j!({ "error": "otp required" })), "www-authenticate", "OTP"),
        Some("654321") => {
            assert_eq!(s.header("npm-auth-type").as_deref(), Some("legacy"));
            json(200, j!({}))
        }
        Some(_) => json(401, j!({ "error": "You must provide a one-time pass. Upgrade your client to npm@latest in order to use 2FA." })),
    })
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.registry.auth = Auth::Token { token: "t".into() };
    let c = client(cfg);
    let good = Prompt {
        otp: Some("654321".into()),
        web: Mutex::default(),
    };
    c.publish(&package(), &PublishOptions::default(), &good)
        .await
        .unwrap();
    assert_eq!(
        server.requests().len(),
        2,
        "never retried, sent again only with the OTP"
    );
    let bad = Prompt {
        otp: Some("000000".into()),
        web: Mutex::default(),
    };
    let e = c
        .publish(&package(), &PublishOptions::default(), &bad)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("Received invalid OTP"), "{e}");
    // An OTP given up front is sent with the first request.
    let options = PublishOptions {
        otp: Some("654321".into()),
        ..Default::default()
    };
    c.publish(&package(), &options, &bad).await.unwrap();
}

#[tokio::test]
async fn publish_web_login_polls_done_url() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, hit| {
        let base = format!("http://{}", s.header("host").unwrap());
        match (s.method.as_str(), s.path.as_str()) {
            ("PUT", _) if s.header("npm-otp").is_none() => with_header(
                json(
                    401,
                    j!({ "authUrl": format!("{base}/auth"), "doneUrl": format!("{base}/done") }),
                ),
                "www-authenticate",
                "OTP",
            ),
            ("PUT", _) => {
                assert_eq!(s.header("npm-otp").as_deref(), Some("web-token"));
                json(200, j!({}))
            }
            ("GET", "/done") if hit == 1 => with_header(status(202), "retry-after", "0"),
            ("GET", "/done") => json(200, j!({ "token": "web-token" })),
            _ => status(404),
        }
    })
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.registry.auth = Auth::Token { token: "t".into() };
    let prompt = Prompt {
        otp: None,
        web: Mutex::default(),
    };
    client(cfg)
        .publish(&package(), &PublishOptions::default(), &prompt)
        .await
        .unwrap();
    assert_eq!(
        *prompt.web.lock(),
        vec![format!("http://{}/auth", server.addr)]
    );
    assert_eq!(server.requests_to("/done").len(), 2);
}

#[tokio::test]
async fn publish_refuses_ip_restricted_logins_and_foreign_done_urls() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|s, _| match s.path.as_str() {
        "/ip" => with_header(json(401, j!({})), "www-authenticate", "Bearer, ipaddress"),
        _ => json(401, j!({ "error": "one-time pass", "authUrl": "https://evil.example/a", "doneUrl": "https://evil.example/done" })),
    })
    .await;
    let mut cfg = config(&server.url(), tmp.path());
    cfg.registry.auth = Auth::Token { token: "t".into() };
    let c = client(cfg);
    let prompt = Prompt {
        otp: None,
        web: Mutex::default(),
    };
    let ip = PublishPackage {
        name: "ip",
        ..package()
    };
    let e = c
        .publish(&ip, &PublishOptions::default(), &prompt)
        .await
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("login is not allowed from your IP address"),
        "{e}"
    );
    // The done URL is not on the registry: no web login, the prompt is asked (and declines).
    let e = c
        .publish(&package(), &PublishOptions::default(), &prompt)
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NeedAuth(_)), "{e}");
    assert!(prompt.web.lock().is_empty());
}

#[tokio::test]
async fn tolerate_republish_checks_the_version() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start(|_, _| json(200, j!({ "versions": { "1.0.0": {} } }))).await;
    let c = client(config(&server.url(), tmp.path()));
    assert!(c.version_exists("@s/p", "1.0.0+build").await);
    assert!(!c.version_exists("@s/p", "2.0.0").await);
    assert_eq!(server.requests()[0].path, "/@s%2fp");
}
