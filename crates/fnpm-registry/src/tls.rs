//! Certificate authorities and client certificates (bun `bun_core/Global.rs::CAStore` and the
//! `ca` / `cafile` handling of `bun-usockets/src/crypto/openssl.c`).
//!
//! - `cafile` replaces the trust store with the certificates in the file; otherwise `ca`
//!   (inline PEM) replaces it, like Node.js.
//! - Without either, the store is chosen by `--use-bundled-ca` (default), `--use-system-ca`
//!   (or `NODE_USE_SYSTEM_CA=1`) or `--use-openssl-ca`. Bundled is Mozilla's list (compiled in,
//!   nothing to parse at startup) plus `SSL_CERT_FILE` / `SSL_CERT_DIR`; system adds the
//!   platform verifier; openssl is only `SSL_CERT_FILE` / `SSL_CERT_DIR` or OpenSSL's default
//!   bundle locations.
//! - `NODE_EXTRA_CA_CERTS` adds to the default store.
//! - `strict-ssl=false` / `NODE_TLS_REJECT_UNAUTHORIZED=0` turns verification off.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fnpm_config::{Env, TlsConfig};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::{Error, Result};

/// Which default certificate authorities to trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaStore {
    /// Mozilla's bundled list (plus `SSL_CERT_FILE` / `SSL_CERT_DIR`).
    #[default]
    Bundled,
    /// The bundled list and the operating system's store.
    System,
    /// Only OpenSSL's certificate file and directory.
    OpenSsl,
}

impl CaStore {
    /// Parses `--ca-store` / `NODE_USE_SYSTEM_CA` style names.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "bundled" | "use-bundled-ca" | "--use-bundled-ca" => Self::Bundled,
            "system" | "use-system-ca" | "--use-system-ca" => Self::System,
            "openssl" | "use-openssl-ca" | "--use-openssl-ca" => Self::OpenSsl,
            _ => return None,
        })
    }

    /// `--use-system-ca`, `--use-openssl-ca`, `--use-bundled-ca`: at most one may be given.
    pub fn from_flags(system: bool, openssl: bool, bundled: bool) -> Result<Option<Self>, String> {
        if usize::from(system) + usize::from(openssl) + usize::from(bundled) > 1 {
            return Err(
                "choose exactly one of --use-system-ca, --use-openssl-ca, or --use-bundled-ca"
                    .to_owned(),
            );
        }
        Ok(if system {
            Some(Self::System)
        } else if openssl {
            Some(Self::OpenSsl)
        } else if bundled {
            Some(Self::Bundled)
        } else {
            None
        })
    }
}

/// The trust decision for one client.
#[derive(Debug)]
pub(crate) enum Trust {
    /// A ready rustls configuration (bundled, openssl, `ca`, `cafile`).
    Rustls(Box<rustls::ClientConfig>),
    /// The platform verifier with extra roots (system store).
    System { extra: Vec<CertificateDer<'static>> },
    /// No verification.
    Insecure,
}

/// bun's HTTP thread reports a CA it cannot load as `HTTPThread: <message>`.
fn tls_error(message: String) -> Error {
    Error::Network {
        url: String::new(),
        message: format!("HTTPThread: {message}"),
    }
}

fn pem_certs(bytes: &[u8], what: &str) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_slice_iter(bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tls_error(format!("the CA {what} is invalid: {e}")))
}

fn read_pem_file(path: &Path, what: &str) -> Result<Vec<CertificateDer<'static>>> {
    let bytes = std::fs::read(path).map_err(|e| {
        tls_error(if e.kind() == std::io::ErrorKind::NotFound {
            format!("could not find {what}: '{}'", path.display())
        } else {
            format!("failed to load {what}: '{}': {e}", path.display())
        })
    })?;
    pem_certs(&bytes, &format!("file '{}'", path.display()))
}

/// Every certificate in the PEM files of a directory (OpenSSL's hashed `c_rehash` layout or any
/// `*.pem` / `*.crt`); unreadable entries are skipped like OpenSSL does.
fn read_pem_dir(dir: &Path) -> Vec<CertificateDer<'static>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if let Ok(bytes) = std::fs::read(entry.path()) {
            out.extend(CertificateDer::pem_slice_iter(&bytes).filter_map(|c| c.ok()));
        }
    }
    out
}

/// OpenSSL's certificate file and directory: `SSL_CERT_FILE` / `SSL_CERT_DIR`, else (when
/// `defaults`) the usual distribution bundle.
fn openssl_certs(env: &Env, defaults: bool) -> Vec<CertificateDer<'static>> {
    let mut out = Vec::new();
    let file = env
        .get("SSL_CERT_FILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let dir = env
        .get("SSL_CERT_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    if let Some(file) = &file
        && let Ok(bytes) = std::fs::read(file)
    {
        out.extend(CertificateDer::pem_slice_iter(&bytes).filter_map(|c| c.ok()));
    }
    if let Some(dir) = &dir {
        for d in std::env::split_paths(dir) {
            out.extend(read_pem_dir(&d));
        }
    }
    if defaults && file.is_none() && dir.is_none() {
        for candidate in [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/ca-bundle.pem",
            "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
            "/etc/ssl/cert.pem",
            "/usr/local/etc/openssl/cert.pem",
        ] {
            if let Ok(bytes) = std::fs::read(candidate) {
                out.extend(CertificateDer::pem_slice_iter(&bytes).filter_map(|c| c.ok()));
                break;
            }
        }
    }
    out
}

fn extra_ca_certs(env: &Env) -> Result<Vec<CertificateDer<'static>>> {
    match env.get("NODE_EXTRA_CA_CERTS").filter(|v| !v.is_empty()) {
        Some(path) => read_pem_file(Path::new(path), "NODE_EXTRA_CA_CERTS file"),
        None => Ok(Vec::new()),
    }
}

/// True unless `strict-ssl=false` or `NODE_TLS_REJECT_UNAUTHORIZED=0`.
pub(crate) fn rejects_unauthorized(config: &TlsConfig, env: &Env) -> bool {
    if env
        .get("NODE_TLS_REJECT_UNAUTHORIZED")
        .is_some_and(|v| v.trim() == "0")
    {
        return false;
    }
    config.reject_unauthorized
}

/// The CA store a configuration selects.
pub(crate) fn ca_store(config: &TlsConfig, env: &Env) -> Result<CaStore> {
    match config.ca_store.as_deref().filter(|s| !s.is_empty()) {
        Some(name) => {
            CaStore::parse(name).ok_or_else(|| tls_error(format!("unknown CA store {name:?}")))
        }
        None if env.truthy("NODE_USE_SYSTEM_CA") == Some(true) => Ok(CaStore::System),
        None => Ok(CaStore::Bundled),
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Client certificate and key from PEM files.
pub(crate) fn load_identity(
    cert: &Path,
    key: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs = read_pem_file(cert, "certfile")?;
    let key_bytes = std::fs::read(key)
        .map_err(|e| tls_error(format!("failed to load keyfile: '{}': {e}", key.display())))?;
    let key = PrivateKeyDer::from_pem_slice(&key_bytes)
        .map_err(|e| tls_error(format!("the keyfile '{}' is invalid: {e}", key.display())))?;
    Ok((certs, key))
}

/// webpki verification, plus what OpenSSL (and so node and bun) also accepts: a server whose
/// certificate is itself one of the configured CA certificates (a self-signed certificate
/// given with `ca` / `cafile`). Only webpki's CA-as-server rejection is bypassed, after
/// its validity checks; certificate purpose, name and handshake signatures are still checked.
#[derive(Debug)]
struct PinnedSelfSigned {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    pinned: Vec<CertificateDer<'static>>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedSelfSigned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(v) => Ok(v),
            Err(e) => {
                let ca_as_server = matches!(
                    &e,
                    rustls::Error::InvalidCertificate(rustls::CertificateError::Other(other))
                        if other.0.downcast_ref::<webpki::Error>()
                            == Some(&webpki::Error::CaUsedAsEndEntity)
                );
                if !ca_as_server
                    || !self
                        .pinned
                        .iter()
                        .any(|c| c.as_ref() == end_entity.as_ref())
                {
                    return Err(e);
                }
                // WebPKI checks EKU after basic constraints, so the rejected CA leaf has
                // not yet been checked for server authentication. Parse that extension
                // separately instead of letting a client-only CA become a server.
                let (_, cert) =
                    x509_parser::parse_x509_certificate(end_entity.as_ref()).map_err(|_| {
                        rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
                    })?;
                let usage = cert.extended_key_usage().map_err(|_| {
                    rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
                })?;
                if usage.is_some_and(|eku| !eku.value.server_auth) {
                    return Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::InvalidPurpose,
                    ));
                }
                let parsed = rustls::server::ParsedCertificate::try_from(end_entity)?;
                rustls::client::verify_server_name(&parsed, server_name)?;
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn rustls_config(
    roots: RootCertStore,
    identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
) -> Result<rustls::ClientConfig> {
    rustls_config_pinned(roots, identity, Vec::new())
}

fn rustls_config_pinned(
    roots: RootCertStore,
    identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
    pinned: Vec<CertificateDer<'static>>,
) -> Result<rustls::ClientConfig> {
    let versions = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_error(format!("TLS setup failed: {e}")))?;
    let builder = if pinned.is_empty() {
        versions.with_root_certificates(roots)
    } else {
        let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            provider(),
        )
        .build()
        .map_err(|e| tls_error(format!("TLS setup failed: {e}")))?;
        versions
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedSelfSigned { inner, pinned }))
    };
    let mut config = match identity {
        Some((certs, key)) => builder
            .with_client_auth_cert(certs, key)
            .map_err(|e| tls_error(format!("invalid client certificate: {e}")))?,
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

fn add_all(
    roots: &mut RootCertStore,
    certs: Vec<CertificateDer<'static>>,
    strict: bool,
    what: &str,
) -> Result<()> {
    if strict {
        for cert in certs {
            roots
                .add(cert)
                .map_err(|e| tls_error(format!("the {what} is invalid: {e}")))?;
        }
    } else {
        roots.add_parsable_certificates(certs);
    }
    Ok(())
}

/// Decides how a client verifies servers.
pub(crate) fn trust(
    config: &TlsConfig,
    env: &Env,
    identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
) -> Result<Trust> {
    if !rejects_unauthorized(config, env) {
        return Ok(Trust::Insecure);
    }
    let mut roots = RootCertStore::empty();
    if let Some(file) = &config.ca_file {
        let certs = read_pem_file(file, "CA file")?;
        if certs.is_empty() {
            return Err(tls_error(format!(
                "the CA file is invalid: '{}'",
                file.display()
            )));
        }
        add_all(&mut roots, certs.clone(), true, "CA file")?;
        return Ok(Trust::Rustls(Box::new(rustls_config_pinned(
            roots, identity, certs,
        )?)));
    }
    if !config.ca.is_empty() {
        let mut pinned = Vec::new();
        for pem in &config.ca {
            let certs = pem_certs(pem.as_bytes(), "")?;
            if certs.is_empty() {
                return Err(tls_error("the CA is invalid: no certificates found".into()));
            }
            add_all(&mut roots, certs.clone(), true, "provided CA")?;
            pinned.extend(certs);
        }
        return Ok(Trust::Rustls(Box::new(rustls_config_pinned(
            roots, identity, pinned,
        )?)));
    }
    let extra = extra_ca_certs(env)?;
    match ca_store(config, env)? {
        CaStore::Bundled => {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            add_all(&mut roots, extra, false, "")?;
            add_all(&mut roots, openssl_certs(env, false), false, "")?;
            Ok(Trust::Rustls(Box::new(rustls_config(roots, identity)?)))
        }
        CaStore::OpenSsl => {
            add_all(&mut roots, extra, false, "")?;
            add_all(&mut roots, openssl_certs(env, true), false, "")?;
            Ok(Trust::Rustls(Box::new(rustls_config(roots, identity)?)))
        }
        CaStore::System => {
            let mut extra = extra;
            extra.extend(openssl_certs(env, false));
            Ok(Trust::System { extra })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_ca_server_keeps_time_and_name_checks() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::pki_types::{ServerName, UnixTime};
        use std::time::Duration;

        let cert =
            CertificateDer::from_pem_slice(include_bytes!("../tests/fixtures/pinned-server.pem"))
                .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            provider(),
        )
        .build()
        .unwrap();
        let verifier = PinnedSelfSigned {
            inner,
            pinned: vec![cert.clone()],
        };
        let localhost = ServerName::try_from("localhost").unwrap();
        let time = |seconds| UnixTime::since_unix_epoch(Duration::from_secs(seconds));
        assert!(
            verifier
                .verify_server_cert(&cert, &[], &localhost, &[], time(1_800_000_000))
                .is_ok()
        );
        for (seconds, expected) in [(0, "NotValidYet"), (6_000_000_000, "Expired")] {
            let err = verifier
                .verify_server_cert(&cert, &[], &localhost, &[], time(seconds))
                .unwrap_err();
            assert!(format!("{err:?}").contains(expected), "{err:?}");
        }
        let wrong_name = ServerName::try_from("wrong.example").unwrap();
        let err = verifier
            .verify_server_cert(&cert, &[], &wrong_name, &[], time(1_800_000_000))
            .unwrap_err();
        assert!(
            matches!(
                err,
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidForNameContext { .. }
                )
            ),
            "{err:?}"
        );
        let unpinned = PinnedSelfSigned {
            pinned: Vec::new(),
            ..verifier
        };
        assert!(
            unpinned
                .verify_server_cert(&cert, &[], &localhost, &[], time(1_800_000_000))
                .is_err()
        );
    }

    #[test]
    fn pinned_client_only_ca_is_not_a_server() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::pki_types::{ServerName, UnixTime};
        use std::time::Duration;

        let cert = CertificateDer::from_pem_slice(include_bytes!(
            "../tests/fixtures/pinned-client-only.pem"
        ))
        .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            provider(),
        )
        .build()
        .unwrap();
        let verifier = PinnedSelfSigned {
            inner,
            pinned: vec![cert.clone()],
        };
        let err = verifier
            .verify_server_cert(
                &cert,
                &[],
                &ServerName::try_from("localhost").unwrap(),
                &[],
                UnixTime::since_unix_epoch(Duration::from_secs(1_800_000_000)),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                rustls::Error::InvalidCertificate(rustls::CertificateError::InvalidPurpose)
            ),
            "{err:?}"
        );
    }

    #[test]
    fn store_selection() {
        let tls = TlsConfig {
            reject_unauthorized: true,
            ..TlsConfig::default()
        };
        assert_eq!(ca_store(&tls, &Env::default()).unwrap(), CaStore::Bundled);
        assert_eq!(
            ca_store(&tls, &Env::from_pairs([("NODE_USE_SYSTEM_CA", "1")])).unwrap(),
            CaStore::System
        );
        let openssl = TlsConfig {
            ca_store: Some("openssl".into()),
            ..tls.clone()
        };
        assert_eq!(
            ca_store(&openssl, &Env::from_pairs([("NODE_USE_SYSTEM_CA", "1")])).unwrap(),
            CaStore::OpenSsl
        );
        let bad = TlsConfig {
            ca_store: Some("nope".into()),
            ..tls.clone()
        };
        assert!(ca_store(&bad, &Env::default()).is_err());
        assert_eq!(CaStore::from_flags(false, false, false), Ok(None));
        assert_eq!(
            CaStore::from_flags(true, false, false),
            Ok(Some(CaStore::System))
        );
        assert!(
            CaStore::from_flags(true, true, false)
                .unwrap_err()
                .contains("choose exactly one")
        );
    }

    #[test]
    fn reject_unauthorized_sources() {
        let tls = TlsConfig {
            reject_unauthorized: true,
            ..TlsConfig::default()
        };
        assert!(rejects_unauthorized(&tls, &Env::default()));
        assert!(!rejects_unauthorized(
            &tls,
            &Env::from_pairs([("NODE_TLS_REJECT_UNAUTHORIZED", "0")])
        ));
        assert!(rejects_unauthorized(
            &tls,
            &Env::from_pairs([("NODE_TLS_REJECT_UNAUTHORIZED", "1")])
        ));
        let off = TlsConfig {
            reject_unauthorized: false,
            ..TlsConfig::default()
        };
        assert!(!rejects_unauthorized(&off, &Env::default()));
        assert!(matches!(
            trust(&off, &Env::default(), None).unwrap(),
            Trust::Insecure
        ));
    }

    #[test]
    fn bundled_store_is_ready_without_files() {
        let tls = TlsConfig {
            reject_unauthorized: true,
            ..TlsConfig::default()
        };
        match trust(&tls, &Env::default(), None).unwrap() {
            Trust::Rustls(c) => {
                assert_eq!(c.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bad_ca_inputs_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = TlsConfig {
            reject_unauthorized: true,
            ca_file: Some(tmp.path().join("nope.pem")),
            ..TlsConfig::default()
        };
        let e = trust(&missing, &Env::default(), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("could not find CA file"), "{e}");
        let empty = tmp.path().join("empty.pem");
        std::fs::write(&empty, "not a cert").unwrap();
        let invalid = TlsConfig {
            reject_unauthorized: true,
            ca_file: Some(empty),
            ..TlsConfig::default()
        };
        assert!(
            trust(&invalid, &Env::default(), None)
                .unwrap_err()
                .to_string()
                .contains("invalid")
        );
        for ca in [
            "invalid",
            "",
            "-----BEGIN CERTIFICATE-----\ninvalid\n-----END CERTIFICATE-----",
        ] {
            let inline = TlsConfig {
                reject_unauthorized: true,
                ca: vec![ca.into()],
                ..TlsConfig::default()
            };
            assert!(
                trust(&inline, &Env::default(), None).is_err(),
                "accepted {ca:?}"
            );
        }
        let env = Env::from_pairs([(
            "NODE_EXTRA_CA_CERTS",
            tmp.path().join("x.pem").to_string_lossy().into_owned(),
        )]);
        let tls = TlsConfig {
            reject_unauthorized: true,
            ..TlsConfig::default()
        };
        assert!(
            trust(&tls, &env, None)
                .unwrap_err()
                .to_string()
                .contains("NODE_EXTRA_CA_CERTS")
        );
    }
}
