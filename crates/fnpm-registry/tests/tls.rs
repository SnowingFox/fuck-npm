//! Certificate handling against a local TLS registry (bun `bun-install-retry.test.ts` "does not
//! retry a certificate it refuses", and the `ca` / `cafile` / `NODE_EXTRA_CA_CERTS` /
//! `NODE_TLS_REJECT_UNAUTHORIZED` behaviours).

mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fnpm_config::Env;
use fnpm_registry::{Error, ManifestRequest, RegistryClient};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// An HTTPS/1.1 server that answers every request with a one-version packument.
async fn tls_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(fixture("server.pem"))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("server.key")).unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(sock).await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body = serde_json::json!({
                    "name": "bar",
                    "dist-tags": { "latest": "1.0.0" },
                    "versions": { "1.0.0": { "dist": { "tarball": "https://localhost/bar.tgz" } } }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(resp.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (addr, connections)
}

fn client(config: fnpm_config::Config, env: &[(&str, &str)]) -> RegistryClient {
    RegistryClient::with_env(Arc::new(config), Env::from_pairs(env.iter().copied())).unwrap()
}

#[tokio::test]
async fn an_unknown_certificate_is_refused_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (addr, connections) = tls_server().await;
    let c = client(
        support::config(&format!("https://localhost:{}/", addr.port()), tmp.path()),
        &[],
    );
    let e = c
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Network { .. }), "{e}");
    assert!(e.to_string().to_lowercase().contains("certificate"), "{e}");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "the next handshake would see the same certificate"
    );
}

#[tokio::test]
async fn inline_ca_and_cafile_replace_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let (addr, _) = tls_server().await;
    let url = format!("https://localhost:{}/", addr.port());
    let mut cfg = support::config(&url, tmp.path());
    cfg.network.tls.ca = vec![std::fs::read_to_string(fixture("ca.pem")).unwrap()];
    assert_eq!(
        client(cfg, &[])
            .manifest("bar", ManifestRequest::default())
            .await
            .unwrap()
            .versions
            .len(),
        1
    );

    let mut cfg = support::config(&url, tmp.path());
    cfg.network.tls.ca_file = Some(fixture("ca.pem"));
    client(cfg, &[])
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();

    // `127.0.0.1` is in the certificate too.
    let mut cfg = support::config(&format!("https://127.0.0.1:{}/", addr.port()), tmp.path());
    cfg.network.tls.ca_file = Some(fixture("ca.pem"));
    client(cfg, &[])
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn extra_ca_certs_add_to_the_bundled_store() {
    let tmp = tempfile::tempdir().unwrap();
    let (addr, _) = tls_server().await;
    let cfg = support::config(&format!("https://localhost:{}/", addr.port()), tmp.path());
    let ca = fixture("ca.pem").to_string_lossy().into_owned();
    client(cfg.clone(), &[("NODE_EXTRA_CA_CERTS", &ca)])
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    // The openssl store with SSL_CERT_FILE only trusts that file.
    let mut openssl = cfg.clone();
    openssl.network.tls.ca_store = Some("openssl".into());
    client(openssl, &[("SSL_CERT_FILE", &ca)])
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn verification_can_be_turned_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (addr, _) = tls_server().await;
    let cfg = support::config(&format!("https://localhost:{}/", addr.port()), tmp.path());
    client(cfg.clone(), &[("NODE_TLS_REJECT_UNAUTHORIZED", "0")])
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
    let mut strict_off = cfg;
    strict_off.network.tls.reject_unauthorized = false;
    client(strict_off, &[])
        .manifest("bar", ManifestRequest::default())
        .await
        .unwrap();
}
