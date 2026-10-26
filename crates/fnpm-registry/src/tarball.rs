//! Tarball downloads (bun `NetworkTask::for_tarball`), streamed to the consumer.
//!
//! Credentials follow npm: the `.npmrc` line of the tarball's own URL (https only, and only for a
//! URL that normalises stably), else the registry's when the tarball is on its origin, else the
//! URL's userinfo as `Basic`. The userinfo is always removed from the URL.
//!
//! Failures before the body starts are retried like any request. If the connection breaks
//! mid-body, the download resumes with `Range: bytes=<received>-` (or, when the server ignores
//! ranges, skips the bytes already delivered), so the consumer sees one uninterrupted stream.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt as _;
use reqwest::Method;
use reqwest::header::HeaderMap;
use url::Url;

use crate::auth::{redact_url, split_userinfo};
use crate::client::{Inner, Spec, classify, describe, error_name, put, status_name};
use crate::limiter::{Permit, Priority};
use crate::retry::{self, Outcome};
use crate::{Error, Result, TarballBody};

/// True if the path has a `.`/`..` segment once `%2e`, `%2f`, `%5c` and `;params` are read the
/// way Tomcat and Jetty read them (bun `hides_dot_segment`).
pub(crate) fn hides_dot_segment(path: &str) -> bool {
    let b = path.as_bytes();
    let (mut start, mut i, mut found) = (0, 0, false);
    let mut check = |segment: &[u8]| {
        let end = segment
            .iter()
            .position(|&c| c == b';')
            .unwrap_or(segment.len());
        let name = &segment[..end];
        found |= [&b"."[..], b"..", b"%2e", b"%2e.", b".%2e", b"%2e%2e"]
            .iter()
            .any(|d| name.eq_ignore_ascii_case(d));
    };
    while i < b.len() {
        let encoded_sep = b[i] == b'%'
            && i + 2 < b.len()
            && matches!(&b[i + 1..i + 3], [b'2', b'f' | b'F'] | [b'5', b'c' | b'C']);
        if b[i] == b'/' || encoded_sep {
            check(&b[start..i]);
            i += if encoded_sep { 3 } else { 1 };
            start = i;
        } else {
            i += 1;
        }
    }
    check(&b[start..]);
    found
}

/// Parses a `dist.tarball` URL: `(url without userinfo, userinfo, normalized)`.
pub(crate) fn prepare_url(raw: &str) -> Result<(Url, Option<String>, bool)> {
    let trimmed = raw.trim();
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err(Error::Network {
            url: redact_url(raw),
            message: format!(
                "Expected tarball URL to start with https:// or http://, got {:?}",
                redact_url(raw)
            ),
        });
    }
    let (userinfo, without) = split_userinfo(trimmed);
    let stable = !without.bytes().any(|b| b <= 0x20 || b == 0x7f);
    let url = Url::parse(&without).map_err(|e| Error::Network {
        url: redact_url(raw),
        message: format!("invalid tarball URL: {e}"),
    })?;
    let query_free = url.path();
    let normalized = stable && !hides_dot_segment(query_free);
    Ok((url, userinfo, normalized))
}

struct Download {
    inner: Arc<Inner>,
    spec: Spec,
    resp: Option<reqwest::Response>,
    permit: Option<Permit>,
    total: Option<u64>,
    received: u64,
    skip: u64,
    attempt: u16,
    done: bool,
}

enum Started {
    Body(reqwest::Response, Permit),
    Failed(Outcome, Option<String>, String, Option<Error>),
}

impl Inner {
    fn tarball_spec(&self, url: &str) -> Result<Spec> {
        if self.offline == fnpm_config::OfflineMode::Offline {
            return Err(Error::Offline(redact_url(url)));
        }
        let (parsed, userinfo, normalized) = prepare_url(url)?;
        let mut headers = HeaderMap::new();
        let auth = self
            .scopes
            .tarball_authorization(&parsed, userinfo.as_deref(), normalized);
        if let Some(auth) = auth {
            put(&mut headers, "authorization", &auth);
        }
        // A `.tgz` is already compressed, and a resumed download counts raw body bytes, so the
        // body must not be content-encoded (manifests do ask for gzip/br/zstd).
        put(&mut headers, "accept-encoding", "identity");
        let what = format!(
            "tarball {}",
            parsed
                .path_segments()
                .and_then(|mut s| s.next_back())
                .unwrap_or("")
        );
        Ok(Spec {
            method: Method::GET,
            url: parsed,
            headers,
            body: None,
            priority: Priority::Tarball,
            blocking: false,
            retry: true,
            what,
        })
    }

    async fn start(&self, spec: &Spec, from: u64) -> Started {
        let permit = self.limiter.acquire(spec.priority).await;
        let mut req = self
            .http
            .request(spec.method.clone(), spec.url.clone())
            .headers(spec.headers.clone());
        if from > 0 {
            req = req.header("range", format!("bytes={from}-"));
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (200..300).contains(&status) {
                    return Started::Body(resp, permit);
                }
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                drop(permit);
                // bun: `GET <url> - <status>`, a 404 included.
                let err = Error::Status {
                    url: redact_url(spec.url.as_str()),
                    status,
                };
                Started::Failed(
                    Outcome::Status(status),
                    retry_after,
                    status_name(status),
                    Some(err),
                )
            }
            Err(e) => {
                drop(permit);
                let err = Error::Network {
                    url: redact_url(spec.url.as_str()),
                    message: describe(&e),
                };
                Started::Failed(classify(&e), None, error_name(&e), Some(err))
            }
        }
    }

    pub async fn tarball(self: &Arc<Self>, url: &str) -> Result<TarballBody> {
        let spec = self.tarball_spec(url)?;
        self.stats.tarball_requests.fetch_add(1, Ordering::Relaxed);
        let mut attempt = 0u16;
        let (resp, permit) = loop {
            match self.start(&spec, 0).await {
                Started::Body(resp, permit) => break (resp, permit),
                Started::Failed(outcome, retry_after, name, err) => {
                    match self.plan_retry(&spec, outcome, attempt, retry_after.as_deref(), &name) {
                        Some(wait) => {
                            attempt += 1;
                            if !wait.is_zero() {
                                tokio::time::sleep(wait).await;
                            }
                        }
                        None => {
                            return Err(err.unwrap_or_else(|| Error::Status {
                                url: redact_url(url),
                                status: 0,
                            }));
                        }
                    }
                }
            }
        };
        let total = resp.content_length();
        let url_out = spec.url.to_string();
        let state = Download {
            inner: self.clone(),
            spec,
            resp: Some(resp),
            permit: Some(permit),
            total,
            received: 0,
            skip: 0,
            attempt,
            done: false,
        };
        let stream = futures_util::stream::unfold(state, |mut st| async move {
            if st.done {
                return None;
            }
            loop {
                let failure = match st.resp.as_mut() {
                    Some(resp) => match resp.chunk().await {
                        Ok(Some(mut chunk)) => {
                            if st.skip > 0 {
                                let n = (st.skip as usize).min(chunk.len());
                                chunk = chunk.slice(n..);
                                st.skip -= n as u64;
                                if chunk.is_empty() {
                                    continue;
                                }
                            }
                            st.received += chunk.len() as u64;
                            st.inner
                                .stats
                                .bytes_downloaded
                                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                            return Some((Ok(chunk), st));
                        }
                        Ok(None) if st.total.is_none_or(|t| st.received >= t) => {
                            st.done = true;
                            st.resp = None;
                            st.permit = None;
                            st.inner.limiter.record_success();
                            return None;
                        }
                        Ok(None) => (
                            Outcome::NoResponse,
                            "ConnectionClosed".to_owned(),
                            format!(
                                "connection closed after {} of {} bytes",
                                st.received,
                                st.total.unwrap_or(0)
                            ),
                        ),
                        Err(e) => (classify(&e), error_name(&e), describe(&e)),
                    },
                    None => (
                        Outcome::NoResponse,
                        "ConnectionClosed".to_owned(),
                        "connection closed".to_owned(),
                    ),
                };
                st.resp = None;
                st.permit = None;
                let (mut outcome, name, message) = failure;
                let mut last_error = Error::Network {
                    url: redact_url(st.spec.url.as_str()),
                    message,
                };
                // Resume until an attempt delivers the rest or the retries run out.
                loop {
                    let Some(wait) = st
                        .inner
                        .plan_retry(&st.spec, outcome, st.attempt, None, &name)
                    else {
                        st.done = true;
                        return Some((Err(last_error), st));
                    };
                    st.attempt += 1;
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                    match st.inner.start(&st.spec, st.received).await {
                        Started::Body(resp, permit) => {
                            let status = resp.status().as_u16();
                            if status == 206 {
                                let starts_right = resp
                                    .headers()
                                    .get("content-range")
                                    .and_then(|v| v.to_str().ok())
                                    .and_then(|v| v.strip_prefix("bytes "))
                                    .and_then(|v| v.split('-').next())
                                    .and_then(|v| v.trim().parse::<u64>().ok())
                                    == Some(st.received);
                                if !starts_right {
                                    last_error = Error::Network {
                                        url: redact_url(st.spec.url.as_str()),
                                        message:
                                            "the server resumed the download at the wrong offset"
                                                .to_owned(),
                                    };
                                    continue;
                                }
                            } else {
                                st.skip = st.received;
                            }
                            st.resp = Some(resp);
                            st.permit = Some(permit);
                            break;
                        }
                        Started::Failed(o, _, _, err) => {
                            if let Some(err) = err {
                                last_error = err;
                            }
                            if !retry::should_retry(o, false) {
                                st.done = true;
                                return Some((Err(last_error), st));
                            }
                            outcome = o;
                        }
                    }
                }
            }
        });
        Ok(TarballBody {
            url: url_out,
            content_length: total,
            stream: stream.boxed(),
        })
    }

    pub async fn tarball_bytes(self: &Arc<Self>, url: &str) -> Result<Bytes> {
        let body = self.tarball(url).await?;
        let mut buf =
            BytesMut::with_capacity(body.content_length.unwrap_or(0).min(256 << 20) as usize);
        let mut stream = body.stream;
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(buf.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_segments() {
        assert!(!hides_dot_segment("/pkg/-/pkg-1.0.0.tgz"));
        assert!(hides_dot_segment("/a/../b.tgz"));
        assert!(hides_dot_segment("/a/%2e%2e/b.tgz"));
        assert!(hides_dot_segment("/a%2f..%2fb.tgz"));
        assert!(hides_dot_segment("/a/..;x=1/b"));
        assert!(hides_dot_segment("/a%5C.%2E%5Cb"));
        assert!(!hides_dot_segment("/a/...b/c"));
    }

    #[test]
    fn tarball_urls() {
        let (u, info, normalized) =
            prepare_url("https://user:pw@r.example/@s/p/-/p-1.0.0.tgz").unwrap();
        assert_eq!(u.as_str(), "https://r.example/@s/p/-/p-1.0.0.tgz");
        assert_eq!(info.as_deref(), Some("user:pw"));
        assert!(normalized);
        let (u, _, normalized) = prepare_url("https://r.example:443/a/./b.tgz").unwrap();
        assert_eq!(u.as_str(), "https://r.example/a/b.tgz");
        assert!(normalized);
        let (_, _, normalized) = prepare_url("https://r.example/a/%2e%2e%2fb.tgz").unwrap();
        assert!(!normalized);
        let e = prepare_url("ftp://r.example/x.tgz")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Expected tarball URL to start with https:// or http://"),
            "{e}"
        );
        assert!(prepare_url("git+https://x/y").is_err());
    }
}
