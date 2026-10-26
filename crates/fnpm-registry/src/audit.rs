//! `fnpm audit`'s bulk advisory requests (bun `runtime/cli/audit_command.rs`).
//!
//! Installed npm packages are grouped by the registry that serves them; each group is sent as a
//! gzipped `POST <registry>/-/npm/v1/security/advisories/bulk` with `{ name: [versions] }`. The
//! responses are merged into one `{ name: [advisory] }` object. A registry other than the default
//! that cannot audit (error status, transport failure, not a list of advisories) is skipped and
//! reported in [`AuditResponse::unaudited`]; the default registry failing is an error.

use std::io::Write as _;

use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use reqwest::Method;
use reqwest::header::HeaderMap;
use serde_json::Value;
use url::Url;

use crate::auth::redact_url;
use crate::client::{Inner, Spec, put};
use crate::limiter::Priority;
use crate::{Error, Result};

/// The installed versions of one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditPackage {
    pub name: String,
    pub versions: Vec<String>,
}

/// A registry that could not be audited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnauditedRegistry {
    /// The registry URL without credentials or trailing slash.
    pub registry: String,
    pub packages: Vec<String>,
    pub reason: String,
}

/// The default registry did not answer with advisories (bun reports this and fails).
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultFailure {
    /// The registry URL without credentials or trailing slash.
    pub registry: String,
    /// The bulk endpoint, without credentials.
    pub url: String,
    pub reason: SkipReason,
    /// What it answered (`--json` relays a body that is JSON but not advisories).
    pub body: Bytes,
}

/// The merged result of every registry.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditResponse {
    /// The merged response text, as the registries sent it (`--json` prints this).
    pub raw: Bytes,
    /// `raw` parsed: `{ package name: [advisory objects] }`.
    pub advisories: Value,
    pub unaudited: Vec<UnauditedRegistry>,
    /// Packages that a registry checked / that were skipped with their registry.
    pub checked: usize,
    pub skipped: usize,
    /// Set when the default registry failed; the other fields are then empty.
    pub default_failure: Option<DefaultFailure>,
}

/// Why a registry's answer does not count as an audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Status(u16),
    Send(String),
    Empty,
    NotJson,
    NotAdvisories,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(s) => write!(f, "{s}"),
            Self::Send(e) => f.write_str(e),
            Self::Empty => f.write_str("empty response"),
            Self::NotJson => f.write_str("non-JSON response"),
            Self::NotAdvisories => f.write_str("not a list of advisories"),
        }
    }
}

/// bun's name for a request that got no response (`ConnectionRefused`, ...).
fn error_name(message: &str) -> &'static str {
    let m = message.to_ascii_lowercase();
    if m.contains("proxyconnectfailed") {
        "ProxyConnectFailed"
    } else if m.contains("connection refused") {
        "ConnectionRefused"
    } else if m.contains("timed out") || m.contains("timeout") {
        "Timeout"
    } else if m.contains("dns") || m.contains("failed to lookup") {
        "DNSError"
    } else if m.contains("certificate") {
        "TLSCertificateError"
    } else if m.contains("connection reset") {
        "ConnectionReset"
    } else {
        "ConnectionClosed"
    }
}

/// `{"name":["1.0.0",...],...}` in the given order.
pub(crate) fn build_body(packages: &[AuditPackage]) -> Vec<u8> {
    let map: serde_json::Map<String, Value> = packages
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                Value::Array(p.versions.iter().cloned().map(Value::String).collect()),
            )
        })
        .collect();
    serde_json::to_vec(&Value::Object(map)).unwrap_or_else(|_| b"{}".to_vec())
}

/// The bulk endpoint answers `{ name: [advisory] }`; anything else is not a clean audit.
pub(crate) fn validate(body: &[u8]) -> std::result::Result<Value, SkipReason> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(SkipReason::Empty);
    }
    let v: Value = serde_json::from_slice(body).map_err(|_| SkipReason::NotJson)?;
    let Value::Object(obj) = &v else {
        return Err(SkipReason::NotAdvisories);
    };
    for advisories in obj.values() {
        let Value::Array(items) = advisories else {
            return Err(SkipReason::NotAdvisories);
        };
        if !items.iter().all(Value::is_object) {
            return Err(SkipReason::NotAdvisories);
        }
    }
    Ok(v)
}

fn is_empty_object(body: &[u8]) -> bool {
    let t = body.trim_ascii();
    t.strip_prefix(b"{")
        .and_then(|r| r.strip_suffix(b"}"))
        .is_some_and(|inner| inner.trim_ascii().is_empty())
}

/// Joins the inner text of non-empty objects (bun `merge_bulk_bodies`).
pub(crate) fn merge_bodies(bodies: &[Bytes]) -> Bytes {
    let non_empty: Vec<&Bytes> = bodies.iter().filter(|b| !is_empty_object(b)).collect();
    match non_empty.as_slice() {
        [] => bodies
            .first()
            .cloned()
            .unwrap_or_else(|| Bytes::from_static(b"{}")),
        [one] => (*one).clone(),
        many => {
            let mut merged = Vec::with_capacity(many.iter().map(|b| b.len()).sum());
            merged.push(b'{');
            for (i, body) in many.iter().enumerate() {
                let t = body.trim_ascii();
                let inner = t
                    .strip_prefix(b"{")
                    .and_then(|r| r.strip_suffix(b"}"))
                    .unwrap_or(t);
                if i > 0 {
                    merged.push(b',');
                }
                merged.extend_from_slice(inner);
            }
            merged.push(b'}');
            Bytes::from(merged)
        }
    }
}

fn gzip(body: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::with_capacity(body.len() / 4 + 64), Compression::new(6));
    enc.write_all(body)?;
    enc.finish()
}

impl Inner {
    async fn audit_one(
        &self,
        base: &Url,
        authorization: Option<&str>,
        body: &[u8],
    ) -> std::result::Result<Bytes, (SkipReason, Bytes)> {
        let no_body = |reason| (reason, Bytes::new());
        let url = Url::parse(&format!(
            "{}/-/npm/v1/security/advisories/bulk",
            base.as_str().trim_end_matches('/')
        ))
        .map_err(|e| no_body(SkipReason::Send(e.to_string())))?;
        let mut headers = HeaderMap::new();
        put(&mut headers, "accept", "application/json");
        put(&mut headers, "content-type", "application/json");
        put(&mut headers, "content-encoding", "gzip");
        if let Some(auth) = authorization {
            put(&mut headers, "authorization", auth);
        }
        let compressed = gzip(body).map_err(|e| no_body(SkipReason::Send(e.to_string())))?;
        // Safe to retry: it changes nothing on the registry.
        let spec = Spec {
            method: Method::POST,
            url,
            headers,
            body: Some(Bytes::from(compressed)),
            priority: Priority::Manifest,
            blocking: true,
            retry: true,
            what: "audit".to_owned(),
        };
        let f = self
            .fetch(&spec)
            .await
            .map_err(|e| no_body(SkipReason::Send(error_name(&e.to_string()).to_owned())))?;
        if f.status >= 400 {
            return Err((SkipReason::Status(f.status), f.body));
        }
        match validate(&f.body) {
            Ok(_) => Ok(f.body),
            Err(reason) => Err((reason, f.body)),
        }
    }

    pub async fn audit(&self, packages: &[AuditPackage]) -> Result<AuditResponse> {
        // Group by registry; the default registry is always asked, even for nothing.
        let mut groups: Vec<(&crate::auth::Scope, Vec<AuditPackage>)> =
            vec![(&self.scopes.default, Vec::new())];
        for p in packages {
            let scope = self.scopes.for_package(&p.name);
            let i = match groups.iter().position(|(s, _)| s.href() == scope.href()) {
                Some(i) => i,
                None => {
                    groups.push((scope, Vec::new()));
                    groups.len() - 1
                }
            };
            match groups[i].1.iter_mut().find(|q| q.name == p.name) {
                Some(existing) => {
                    for v in &p.versions {
                        if !existing.versions.contains(v) {
                            existing.versions.push(v.clone());
                        }
                    }
                }
                None => groups[i].1.push(p.clone()),
            }
        }
        let mut bodies = Vec::new();
        let mut unaudited = Vec::new();
        let (mut checked, mut skipped) = (0, 0);
        for (i, (scope, list)) in groups.iter().enumerate() {
            let is_default = i == 0;
            let registry = redact_url(scope.href().trim_end_matches('/'));
            let result = match &scope.url {
                Some(base) if matches!(base.scheme(), "http" | "https") => {
                    self.audit_one(base, scope.authorization.as_deref(), &build_body(list))
                        .await
                }
                // Not an http(s) registry: a configuration error, fatal like bun.
                _ => {
                    return Err(Error::Network {
                        url: redact_url(&scope.configured),
                        message: "Registry URL must be http:// or https://".to_owned(),
                    });
                }
            };
            match result {
                Ok(body) => {
                    checked += list.len();
                    bodies.push(body);
                }
                Err((reason, body)) if is_default => {
                    let url = format!("{registry}/-/npm/v1/security/advisories/bulk");
                    return Ok(AuditResponse {
                        raw: Bytes::new(),
                        advisories: Value::Object(Default::default()),
                        unaudited: Vec::new(),
                        checked: 0,
                        skipped: 0,
                        default_failure: Some(DefaultFailure {
                            registry,
                            url,
                            reason,
                            body,
                        }),
                    });
                }
                Err((reason, _)) => {
                    skipped += list.len();
                    unaudited.push(UnauditedRegistry {
                        registry,
                        packages: list.iter().map(|p| p.name.clone()).collect(),
                        reason: reason.to_string(),
                    });
                }
            }
        }
        let raw = merge_bodies(&bodies);
        let advisories = serde_json::from_slice(&raw).unwrap_or(Value::Object(Default::default()));
        // The callers report `unaudited` (bun's `print_unaudited`).
        Ok(AuditResponse {
            raw,
            advisories,
            unaudited,
            checked,
            skipped,
            default_failure: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_shape() {
        let body = build_body(&[
            AuditPackage {
                name: "b".into(),
                versions: vec!["1.0.0".into(), "2.0.0".into()],
            },
            AuditPackage {
                name: "a".into(),
                versions: vec!["0.1.0".into()],
            },
        ]);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"b":["1.0.0","2.0.0"],"a":["0.1.0"]}"#
        );
        assert_eq!(build_body(&[]), b"{}");
    }

    #[test]
    fn validation() {
        assert_eq!(validate(b"  "), Err(SkipReason::Empty));
        assert_eq!(validate(b"<html>"), Err(SkipReason::NotJson));
        assert_eq!(validate(b"[]"), Err(SkipReason::NotAdvisories));
        assert_eq!(validate(br#"{"a":{}}"#), Err(SkipReason::NotAdvisories));
        assert_eq!(validate(br#"{"a":[1]}"#), Err(SkipReason::NotAdvisories));
        assert!(validate(br#"{"a":[{"id":1}]}"#).is_ok());
        assert!(validate(b"{}").is_ok());
    }

    #[test]
    fn merging() {
        let a = Bytes::from_static(br#"{"a":[{"id":1}]}"#);
        let b = Bytes::from_static(br#" {"b":[]} "#);
        let empty = Bytes::from_static(b"{ }");
        assert_eq!(merge_bodies(&[]), Bytes::from_static(b"{}"));
        assert_eq!(merge_bodies(std::slice::from_ref(&empty)), empty);
        assert_eq!(merge_bodies(&[empty.clone(), a.clone()]), a);
        let merged = merge_bodies(&[a, empty, b]);
        assert_eq!(&merged[..], br#"{"a":[{"id":1}],"b":[]}"#);
        let v: Value = serde_json::from_slice(&merged).unwrap();
        assert!(v["a"].is_array() && v["b"].is_array());
    }

    #[test]
    fn gzip_round_trip() {
        use std::io::Read as _;
        let data = build_body(&[AuditPackage {
            name: "x".into(),
            versions: vec!["1.0.0".into()],
        }]);
        let gz = gzip(&data).unwrap();
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }
}
