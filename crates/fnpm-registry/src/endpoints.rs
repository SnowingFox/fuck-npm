//! Registry JSON endpoints besides manifests: generic JSON requests, `whoami` (bun
//! `npm::whoami`), `view` (`pm_view_command.rs`), and GitHub commit resolution.

use bytes::Bytes;
use reqwest::Method;
use reqwest::header::HeaderMap;
use serde_json::Value;
use url::Url;

use crate::auth::redact_url;
use crate::client::{Fetched, Inner, Spec, arch_name, os_name, put};
use crate::limiter::Priority;
use crate::npm::{ACCEPT_FULL, manifest_url};
use crate::{Error, Result};

/// Characters escaped in one URL path segment.
const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// A JSON response with the npm notice the registry attached, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonResponse {
    pub status: u16,
    pub body: Value,
    /// `npm-notice` (unless `x-local-cache` is set), for the caller to print.
    pub notice: Option<String>,
}

/// The `error` string of an npm error body.
pub(crate) fn error_message(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("error").and_then(Value::as_str).map(str::to_owned)
}

pub(crate) fn npm_notice(f: &Fetched) -> Option<String> {
    if f.headers.contains_key("x-local-cache") {
        return None;
    }
    f.header("npm-notice").map(str::to_owned)
}

/// A non-2xx response as an error with the registry's message.
pub(crate) fn response_error(url: &Url, f: &Fetched) -> Error {
    match (f.status, error_message(&f.body)) {
        (404, None) => Error::NotFound(redact_url(url.as_str())),
        (status, Some(message)) => Error::Registry {
            url: redact_url(url.as_str()),
            status,
            message,
        },
        (status, None) => Error::Status {
            url: redact_url(url.as_str()),
            status,
        },
    }
}

fn parse_method(method: &str) -> Result<Method> {
    Method::from_bytes(method.to_ascii_uppercase().as_bytes()).map_err(|_| Error::Network {
        url: String::new(),
        message: format!("invalid HTTP method {method:?}"),
    })
}

fn parse_url(url: &str) -> Result<Url> {
    Url::parse(url).map_err(|e| Error::Network {
        url: redact_url(url),
        message: format!("invalid URL: {e}"),
    })
}

impl Inner {
    /// `<ua> <os> <arch> workspaces/<bool>[ ci/<name>]`, as npm sends for publish and whoami.
    pub fn npm_user_agent(&self, workspaces: bool) -> String {
        self.npm_user_agent_with(&self.user_agent, workspaces)
    }

    /// [`Inner::npm_user_agent`] with `base` in place of the client's user agent.
    pub fn npm_user_agent_with(&self, base: &str, workspaces: bool) -> String {
        let mut ua = format!(
            "{base} {} {} workspaces/{workspaces}",
            os_name(),
            arch_name()
        );
        if let Some(ci) = self.ci_name {
            ua.push_str(" ci/");
            ua.push_str(ci);
        }
        ua
    }

    pub async fn request_json_full(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
        otp: Option<&str>,
    ) -> Result<JsonResponse> {
        let method = parse_method(method)?;
        let url = parse_url(url)?;
        let mut headers = HeaderMap::new();
        put(&mut headers, "accept", "application/json");
        if let Some(auth) = self.scopes.endpoint_authorization(&url) {
            put(&mut headers, "authorization", &auth);
        }
        put(
            &mut headers,
            "npm-auth-type",
            if otp.is_some() { "legacy" } else { "web" },
        );
        if let Some(otp) = otp {
            put(&mut headers, "npm-otp", otp);
        }
        let body = match body {
            Some(v) => {
                put(&mut headers, "content-type", "application/json");
                Some(Bytes::from(serde_json::to_vec(v).map_err(|e| {
                    Error::Network {
                        url: redact_url(url.as_str()),
                        message: e.to_string(),
                    }
                })?))
            }
            None => None,
        };
        // Only requests that change nothing are sent twice.
        let retry = matches!(method, Method::GET | Method::HEAD);
        let spec = Spec {
            method,
            url,
            headers,
            body,
            priority: Priority::Manifest,
            blocking: true,
            retry,
            what: "registry request".to_owned(),
        };
        let f = self.fetch(&spec).await?;
        if f.status >= 400 {
            return Err(response_error(&spec.url, &f));
        }
        let parsed = if f.body.iter().all(u8::is_ascii_whitespace) {
            Value::Null
        } else {
            serde_json::from_slice(&f.body).map_err(|e| Error::InvalidResponse {
                url: redact_url(spec.url.as_str()),
                message: format!("not JSON: {e}"),
            })?
        };
        Ok(JsonResponse {
            status: f.status,
            body: parsed,
            notice: npm_notice(&f),
        })
    }

    /// The user name of the default registry's credentials, or `GET /-/whoami`.
    pub async fn whoami(&self, auth_type: Option<&str>) -> Result<(String, Option<String>)> {
        let scope = &self.scopes.default;
        if let Some(user) = &scope.user {
            return Ok((user.clone(), None));
        }
        let configured = Url::parse(scope.configured.trim()).ok();
        if let Some(u) = configured.as_ref().filter(|u| !u.username().is_empty()) {
            let user = percent_encoding::percent_decode_str(u.username())
                .decode_utf8_lossy()
                .into_owned();
            return Ok((user, None));
        }
        let Some(auth) = &scope.authorization else {
            return Err(Error::NeedAuth(
                "missing authentication (run `fnpm x npm login`)".to_owned(),
            ));
        };
        let base = scope.url.clone().ok_or_else(|| Error::Network {
            url: redact_url(&scope.configured),
            message: "Registry URL must be http:// or https://".to_owned(),
        })?;
        let url = parse_url(&format!("{}/-/whoami", base.as_str().trim_end_matches('/')))?;
        let mut headers = HeaderMap::new();
        put(&mut headers, "accept", "*/*");
        put(&mut headers, "authorization", auth);
        put(&mut headers, "npm-auth-type", auth_type.unwrap_or("web"));
        put(&mut headers, "npm-command", "whoami");
        put(&mut headers, "user-agent", &self.npm_user_agent(false));
        let spec = Spec {
            method: Method::GET,
            url,
            headers,
            body: None,
            priority: Priority::Manifest,
            blocking: true,
            retry: true,
            what: "whoami".to_owned(),
        };
        let f = self.fetch(&spec).await?;
        if f.status >= 400 {
            return Err(response_error(&spec.url, &f));
        }
        let notice = npm_notice(&f);
        let v: Value = serde_json::from_slice(&f.body).map_err(|e| Error::InvalidResponse {
            url: redact_url(spec.url.as_str()),
            message: format!("failed to parse '/-/whoami' response body as JSON: {e}"),
        })?;
        match v.get("username").and_then(Value::as_str) {
            Some(u) => Ok((u.to_owned(), notice)),
            None => Err(Error::InvalidResponse {
                url: redact_url(spec.url.as_str()),
                message: "probably invalid auth".to_owned(),
            }),
        }
    }

    /// The full packument as JSON (`fnpm info`), with the scope's credentials.
    pub async fn view(&self, name: &str) -> Result<Value> {
        let scope = self.scopes.for_package(name);
        let url = manifest_url(scope, name)?;
        let mut headers = HeaderMap::new();
        put(&mut headers, "accept", ACCEPT_FULL);
        if let Some(auth) = &scope.authorization {
            put(&mut headers, "authorization", auth);
            put(&mut headers, "npm-auth-type", "legacy");
        }
        let spec = Spec {
            method: Method::GET,
            url,
            headers,
            body: None,
            priority: Priority::Manifest,
            blocking: true,
            retry: true,
            what: format!("package manifest {name}"),
        };
        let f = self.fetch(&spec).await?;
        if f.status == 404 {
            return Err(Error::NotFound(name.to_owned()));
        }
        if f.status >= 400 {
            return Err(response_error(&spec.url, &f));
        }
        serde_json::from_slice(&f.body).map_err(|e| Error::InvalidManifest {
            name: name.to_owned(),
            message: e.to_string(),
        })
    }

    /// `GET {api}/repos/{owner}/{repo}/commits/{ref}` with `Accept: application/vnd.github.sha`.
    pub async fn resolve_github(
        &self,
        owner: &str,
        repo: &str,
        committish: &str,
    ) -> Result<String> {
        let reference = if committish.is_empty() {
            "HEAD"
        } else {
            committish
        };
        if reference.len() == 40 && reference.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(reference.to_ascii_lowercase());
        }
        let enc = |s: &str| percent_encoding::utf8_percent_encode(s, PATH_SEGMENT).to_string();
        let url = parse_url(&format!(
            "{}/repos/{}/{}/commits/{}",
            self.github_api,
            enc(owner),
            enc(repo),
            enc(reference)
        ))?;
        let mut headers = HeaderMap::new();
        put(&mut headers, "accept", "application/vnd.github.sha");
        if let Some(token) = &self.github_token {
            put(&mut headers, "authorization", &format!("Bearer {token}"));
        }
        let spec = Spec {
            method: Method::GET,
            url,
            headers,
            body: None,
            priority: Priority::Manifest,
            blocking: false,
            retry: true,
            what: format!("GitHub commit {owner}/{repo}#{reference}"),
        };
        let f = self.fetch(&spec).await?;
        match f.status {
            200 => {
                let sha = String::from_utf8_lossy(&f.body).trim().to_ascii_lowercase();
                if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                    Ok(sha)
                } else {
                    Err(Error::InvalidResponse {
                        url: redact_url(spec.url.as_str()),
                        message: "not a commit SHA".to_owned(),
                    })
                }
            }
            404 | 422 => Err(Error::NotFound(format!(
                "github:{owner}/{repo}#{reference}"
            ))),
            _ => Err(response_error(&spec.url, &f)),
        }
    }

    /// `{api}/repos/{owner}/{repo}/tarball/{sha}`; an empty repo (a `https://github.com/...` URL
    /// that names none) leaves out its slash, like bun.
    pub fn github_tarball_url(&self, owner: &str, repo: &str, sha: &str) -> String {
        let sep = if repo.is_empty() { "" } else { "/" };
        format!("{}/repos/{owner}/{repo}{sep}tarball/{sha}", self.github_api)
    }
}

/// The name of the root folder of GitHub's tarball of `commit` (`<owner>-<repo>-<sha7>`), which
/// is what a lockfile records; `None` if it would not be one safe path component.
pub fn github_tarball_tag(owner: &str, repo: &str, commit: &str) -> Option<String> {
    let short = commit.get(..commit.len().min(7))?;
    let tag = format!("{owner}-{repo}-{short}");
    crate::git::is_safe_resolved_tag(&tag).then_some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies() {
        assert_eq!(
            error_message(br#"{"error":"nope"}"#).as_deref(),
            Some("nope")
        );
        assert_eq!(error_message(b"<html>"), None);
        assert_eq!(error_message(br#"{"error":1}"#), None);
    }

    #[test]
    fn github_tags() {
        assert_eq!(
            github_tarball_tag("oven-sh", "bun", "0123456789abcdef").as_deref(),
            Some("oven-sh-bun-0123456")
        );
        assert_eq!(github_tarball_tag("a/b", "c", "0123456"), None);
        assert_eq!(github_tarball_tag("-x", "c", "0123456"), None);
        // Not a char boundary at 7: no panic, and not a safe tag.
        assert_eq!(github_tarball_tag("a", "b", "012345\u{e9}9"), None);
        assert_eq!(
            github_tarball_tag("a", "b", "\u{e9}\u{e9}\u{e9}\u{e9}"),
            None
        );
    }
}
