//! `fnpm publish` (bun `runtime/cli/publish_command.rs`): the normalised manifest, the request
//! body, and the PUT with npm's two-factor flows (web login polling, then an OTP prompt).

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use fnpm_types::{HashAlgorithm, Integrity};
use futures_util::future::BoxFuture;
use reqwest::Method;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use url::Url;

use crate::auth::{redact_url, same_origin};
use crate::client::{Fetched, Inner, Spec, put};
use crate::endpoints::{error_message, npm_notice};
use crate::limiter::Priority;
use crate::{Error, Result};

/// `_nodeVersion` in published manifests.
pub const REPORTED_NODE_VERSION: &str = "22.0.0";
/// `_npmVersion` in published manifests (bun sends the same fixed value).
pub const REPORTED_NPM_VERSION: &str = "10.8.3";

/// What the CLI asks the user during publish.
pub trait OtpPrompt: Send + Sync {
    /// The registry offers a browser login at `auth_url`; show it (and open it on request).
    fn web_login(&self, auth_url: &str);
    /// "This operation requires a one-time password." `None` cancels.
    fn read_otp(&self) -> BoxFuture<'_, Option<String>>;
    /// The `npm-notice` of a response that is not the final one (a 401, a web-login poll, an
    /// error), as soon as it arrives. The final one is [`Published::notice`].
    fn notice(&self, _notice: &str) {}
}

/// Publish settings (`publishConfig` merged with flags).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishOptions {
    /// Default `latest`.
    pub tag: Option<String>,
    /// `public` / `restricted`; `None` sends `"access": null`.
    pub access: Option<String>,
    /// `--auth-type` (`web` by default; `legacy` whenever an OTP is sent).
    pub auth_type: Option<String>,
    /// `--otp`.
    pub otp: Option<String>,
    /// `--filter` / `--recursive`: reported in the user agent.
    pub workspaces: bool,
    /// Replaces the client's user agent before `<os> <arch> workspaces/..`.
    pub user_agent: Option<String>,
}

/// One package to publish.
#[derive(Debug, Clone, Copy)]
pub struct PublishPackage<'a> {
    pub name: &'a str,
    /// As in package.json; a `+build` suffix is dropped from the published version.
    pub version: &'a str,
    /// The manifest after [`normalize_manifest`], as JSON text.
    pub manifest_json: &'a str,
    pub tarball: &'a [u8],
}

/// How a publish ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// `npm-notice` of the last response.
    pub notice: Option<String>,
}

/// `1.2.3+build` to `1.2.3`.
pub fn without_build_tag(version: &str) -> &str {
    version.split_once('+').map_or(version, |(v, _)| v)
}

/// `<name>-<version>.tgz` with the full name (bun's `Raw` tarball name style).
pub fn tarball_filename(name: &str, version: &str) -> String {
    format!("{name}-{version}.tgz")
}

/// bun `normalize_buf::<Posix>`: `.` and empty segments are dropped, `..` removes the segment
/// before it and is dropped at the start of a relative path, a trailing `/` is kept, `\\` also
/// separates. An empty path is `.`.
fn normalize_posix(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut joined = parts.join("/");
    if absolute {
        joined.insert(0, '/');
    }
    if trailing && !joined.is_empty() && !joined.ends_with('/') {
        joined.push('/');
    }
    joined
}

fn is_package_root_or_outside(p: &str) -> bool {
    p.is_empty() || p == "." || p == ".." || p.starts_with("../") || p.starts_with('/')
}

fn non_empty_bin(json: &Value) -> bool {
    match json.get("bin") {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => false,
    }
}

fn push_bin(
    out: &mut Map<String, Value>,
    key: &str,
    value: &str,
    files: &[&str],
    warnings: &mut Vec<String>,
) {
    let target = normalize_posix(value);
    if is_package_root_or_outside(&target) {
        return;
    }
    let below = target.trim_end_matches('/');
    let has = files.iter().any(|f| {
        f.strip_prefix(below)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    });
    if !has {
        warnings.push(format!("bin '{target}' does not exist"));
    }
    out.insert(key.to_owned(), Value::String(target));
}

/// bun `normalize_bin`: `bin` becomes an object of normalised paths; `directories.bin` (when
/// `bin` is empty) lists the tarball's files below that directory.
fn normalize_bin(
    json: &mut Map<String, Value>,
    name: &str,
    files: &[&str],
    warnings: &mut Vec<String>,
) {
    let use_directories = !non_empty_bin(&Value::Object(json.clone()));
    if let Some(bin) = json.get("bin").cloned() {
        let mut props = Map::new();
        match bin {
            Value::String(s) => push_bin(&mut props, name, &s, files, warnings),
            Value::Object(entries) => {
                for (key, value) in entries {
                    if key.is_empty() {
                        continue;
                    }
                    let key = normalize_posix(&key);
                    let key = key.strip_prefix("./").unwrap_or(&key).to_owned();
                    if key.is_empty() {
                        continue;
                    }
                    let Value::String(value) = value else {
                        continue;
                    };
                    push_bin(&mut props, &key, &value, files, warnings);
                }
            }
            _ => return,
        }
        json.insert("bin".to_owned(), Value::Object(props));
    }
    if !use_directories {
        return;
    }
    let Some(dir) = json
        .get("directories")
        .and_then(|d| d.get("bin"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let dir = normalize_posix(dir).trim_end_matches('/').to_owned();
    if is_package_root_or_outside(&dir) {
        return;
    }
    let mut props = Map::new();
    for path in files {
        if path
            .strip_prefix(dir.as_str())
            .is_some_and(|below| below.len() > 1 && below.starts_with('/'))
        {
            let base = path.rsplit('/').next().unwrap_or(path);
            props.insert(base.to_owned(), Value::String((*path).to_owned()));
        }
    }
    if props.is_empty() {
        warnings.push(format!("bin directory '{dir}' does not exist"));
        return;
    }
    json.insert("bin".to_owned(), Value::Object(props));
}

/// Fills in what the registry expects of a published version (bun `normalized_package`):
/// `version` without build metadata, `_id`, `_integrity`, `_nodeVersion`, `_npmVersion`,
/// `integrity`, `shasum`, `readme` (unless package.json has one), `dist`, and a normalised
/// `bin`. `files` are the tarball's paths relative to the package root. Returns warnings.
pub fn normalize_manifest(
    manifest: &mut Value,
    name: &str,
    version: &str,
    tarball: &[u8],
    registry_href: &str,
    readme: Option<(&str, &str)>,
    files: &[&str],
) -> Vec<String> {
    let mut warnings = Vec::new();
    let Value::Object(json) = manifest else {
        return vec!["package.json is not an object".to_owned()];
    };
    let published = without_build_tag(version);
    let integrity = Integrity::compute(HashAlgorithm::Sha512, tarball).to_sri();
    let shasum = Integrity::compute(HashAlgorithm::Sha1, tarball).to_hex();
    let set = |json: &mut Map<String, Value>, k: &str, v: &str| {
        json.insert(k.to_owned(), Value::String(v.to_owned()));
    };
    set(json, "version", published);
    set(json, "_id", &format!("{name}@{published}"));
    set(json, "_integrity", &integrity);
    set(json, "_nodeVersion", REPORTED_NODE_VERSION);
    set(json, "_npmVersion", REPORTED_NPM_VERSION);
    set(json, "integrity", &integrity);
    set(json, "shasum", &shasum);
    if let Some((filename, contents)) = readme
        && !json.contains_key("readme")
    {
        set(json, "readme", contents);
        set(json, "readmeFilename", filename);
    }
    // Always http, never https, as in npm's libnpmpublish.
    let host_path = registry_href
        .strip_prefix("https://")
        .or_else(|| registry_href.strip_prefix("http://"))
        .unwrap_or(registry_href)
        .trim_end_matches('/');
    let mut dist = Map::new();
    dist.insert("integrity".to_owned(), Value::String(integrity.clone()));
    dist.insert("shasum".to_owned(), Value::String(shasum));
    dist.insert(
        "tarball".to_owned(),
        Value::String(format!(
            "http://{host_path}/{name}/-/{}",
            tarball_filename(name, version)
        )),
    );
    json.insert("dist".to_owned(), Value::Object(dist));
    normalize_bin(json, name, files, &mut warnings);
    warnings
}

/// The PUT body: `_id`, `name`, `dist-tags`, `versions` (the normalised manifest verbatim),
/// `access`, and the base64 tarball in `_attachments`.
pub fn publish_body(pkg: &PublishPackage<'_>, options: &PublishOptions) -> Vec<u8> {
    let tag = options
        .tag
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or("latest");
    let version = without_build_tag(pkg.version);
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned());
    let encoded = B64.encode(pkg.tarball);
    let mut out = String::with_capacity(pkg.manifest_json.len() + encoded.len() + 256);
    out.push_str(&format!(
        "{{\"_id\":{},\"name\":{}",
        q(pkg.name),
        q(pkg.name)
    ));
    out.push_str(&format!(",\"dist-tags\":{{{}:{}}}", q(tag), q(version)));
    out.push_str(&format!(
        ",\"versions\":{{{}:{}}}",
        q(version),
        pkg.manifest_json
    ));
    match &options.access {
        Some(access) => out.push_str(&format!(",\"access\":{}", q(access))),
        None => out.push_str(",\"access\":null"),
    }
    out.push_str(&format!(
        ",\"_attachments\":{{{}:{{\"content_type\":\"application/octet-stream\",\"data\":\"{encoded}\",\"length\":{}}}}}}}",
        q(&tarball_filename(pkg.name, pkg.version)),
        pkg.tarball.len()
    ));
    out.into_bytes()
}

/// What a 401 asks for (`www-authenticate`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Challenge {
    Otp,
    IpAddress,
    Other(String),
}

pub(crate) fn challenge(f: &Fetched) -> Challenge {
    let Some(value) = f.header("www-authenticate") else {
        let text = String::from_utf8_lossy(&f.body);
        return if text.contains("one-time pass") {
            Challenge::Otp
        } else {
            Challenge::Other(String::new())
        };
    };
    for part in value.split(',') {
        let p = part.trim();
        if p.eq_ignore_ascii_case("ipaddress") {
            return Challenge::IpAddress;
        }
        if p.eq_ignore_ascii_case("otp") {
            return Challenge::Otp;
        }
    }
    Challenge::Other(value.to_owned())
}

fn publish_error(url: &Url, f: &Fetched, name: &str, version: &str, after_otp: bool) -> Error {
    let message = error_message(&f.body);
    let url = redact_url(url.as_str());
    if f.status == 404 {
        return Error::Registry {
            url,
            status: 404,
            message: format!("'{name}@{version}' does not exist in this registry"),
        };
    }
    match message {
        Some(m)
            if after_otp
                && f.status == 401
                && m.contains("You must provide a one-time pass. Upgrade your client to npm@latest in order to use 2FA.") =>
        {
            Error::Registry { url, status: 401, message: "Received invalid OTP".to_owned() }
        }
        Some(message) => Error::Registry { url, status: f.status, message },
        None => Error::Status { url, status: f.status },
    }
}

impl Inner {
    fn publish_headers(
        &self,
        auth: &str,
        options: &PublishOptions,
        with_body: bool,
        otp: Option<&str>,
    ) -> HeaderMap {
        let mut h = HeaderMap::new();
        put(&mut h, "accept", "*/*");
        put(&mut h, "authorization", auth);
        if with_body {
            // Not `application/json; charset=utf-8`: verdaccio refuses anything else.
            put(&mut h, "content-type", "application/json");
        }
        let auth_type = if otp.is_some() {
            "legacy"
        } else {
            options.auth_type.as_deref().unwrap_or("web")
        };
        put(&mut h, "npm-auth-type", auth_type);
        if let Some(otp) = otp {
            put(&mut h, "npm-otp", otp);
        }
        put(&mut h, "npm-command", "publish");
        let user_agent = match &options.user_agent {
            Some(base) => self.npm_user_agent_with(base, options.workspaces),
            None => self.npm_user_agent(options.workspaces),
        };
        put(&mut h, "user-agent", &user_agent);
        h
    }

    /// The registry already has `name@version` (`--tolerate-republish`).
    pub async fn version_exists(&self, name: &str, version: &str) -> bool {
        self.version_exists_noticed(name, version, &|notice| self.warn(notice.to_owned()))
            .await
    }

    /// [`Inner::version_exists`], handing the `npm-notice` of the response to `notice`.
    pub async fn version_exists_noticed(
        &self,
        name: &str,
        version: &str,
        notice: &(dyn Fn(&str) + Send + Sync),
    ) -> bool {
        let scope = self.scopes.for_package(name);
        let Some(base) = &scope.url else {
            return false;
        };
        let Ok(url) = Url::parse(&format!(
            "{}/{}",
            base.as_str().trim_end_matches('/'),
            name.replace('/', "%2f")
        )) else {
            return false;
        };
        let mut headers = HeaderMap::new();
        put(&mut headers, "accept", "application/json");
        if let Some(auth) = &scope.authorization {
            put(&mut headers, "authorization", auth);
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
        let Ok(f) = self.fetch(&spec).await else {
            return false;
        };
        if let Some(n) = npm_notice(&f) {
            notice(&n);
        }
        if f.status != 200 {
            return false;
        }
        serde_json::from_slice::<Value>(&f.body)
            .ok()
            .is_some_and(|v| {
                v.get("versions")
                    .and_then(|vs| vs.get(without_build_tag(version)))
                    .is_some()
            })
    }

    pub async fn publish(
        &self,
        pkg: &PublishPackage<'_>,
        options: &PublishOptions,
        prompt: &dyn OtpPrompt,
    ) -> Result<Published> {
        let scope = self.scopes.for_package(pkg.name).clone();
        let Some(auth) = scope.authorization.clone() else {
            return Err(Error::NeedAuth(
                "missing authentication (run `fnpm x npm login`)".to_owned(),
            ));
        };
        let base = scope
            .url
            .clone()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| Error::Network {
                url: redact_url(&scope.configured),
                message: "Registry URL must be http:// or https://".to_owned(),
            })?;
        let url = Url::parse(&format!(
            "{}/{}",
            base.as_str().trim_end_matches('/'),
            pkg.name.replace('/', "%2f")
        ))
        .map_err(|e| Error::Network {
            url: redact_url(base.as_str()),
            message: e.to_string(),
        })?;
        let body = Bytes::from(publish_body(pkg, options));
        let otp = options.otp.as_deref().filter(|o| !o.is_empty());
        let spec = |otp: Option<&str>| Spec {
            method: Method::PUT,
            url: url.clone(),
            headers: self.publish_headers(&auth, options, true, otp),
            body: Some(body.clone()),
            priority: Priority::Manifest,
            blocking: true,
            retry: false,
            what: "publish".to_owned(),
        };
        let f = self.fetch(&spec(otp)).await?;
        if f.status < 400 {
            return Ok(Published {
                notice: npm_notice(&f),
            });
        }
        if let Some(n) = npm_notice(&f) {
            prompt.notice(&n);
        }
        if f.status != 401 {
            return Err(publish_error(&url, &f, pkg.name, pkg.version, false));
        }
        match challenge(&f) {
            Challenge::Otp => {}
            Challenge::IpAddress => {
                return Err(Error::NeedAuth(
                    "login is not allowed from your IP address".to_owned(),
                ));
            }
            Challenge::Other(need) if !need.is_empty() => {
                return Err(Error::NeedAuth(format!(
                    "unable to authenticate, need: {need}"
                )));
            }
            Challenge::Other(_) => {
                return Err(publish_error(&url, &f, pkg.name, pkg.version, false));
            }
        }
        let otp = self.get_otp(&f, &base, &auth, options, prompt, pkg).await?;
        let f = self.fetch(&spec(Some(&otp))).await?;
        if f.status >= 400 {
            if let Some(n) = npm_notice(&f) {
                prompt.notice(&n);
            }
            return Err(publish_error(&url, &f, pkg.name, pkg.version, true));
        }
        Ok(Published {
            notice: npm_notice(&f),
        })
    }

    /// The web login (`authUrl` / `doneUrl` in the 401 body, polled until it yields a token),
    /// else an OTP from the prompt.
    async fn get_otp(
        &self,
        unauthorized: &Fetched,
        registry: &Url,
        auth: &str,
        options: &PublishOptions,
        prompt: &dyn OtpPrompt,
        pkg: &PublishPackage<'_>,
    ) -> Result<String> {
        let body: Option<Value> = serde_json::from_slice(&unauthorized.body).ok();
        let urls = body.as_ref().and_then(|b| {
            let auth_url = b.get("authUrl")?.as_str()?.to_owned();
            let done_url = Url::parse(b.get("doneUrl")?.as_str()?).ok()?;
            Some((auth_url, done_url))
        });
        if let Some((auth_url, done_url)) = urls
            && matches!(done_url.scheme(), "http" | "https")
            && same_origin(&done_url, registry)
        {
            prompt.web_login(&auth_url);
            loop {
                let spec = Spec {
                    method: Method::GET,
                    url: done_url.clone(),
                    headers: self.publish_headers(auth, options, false, None),
                    body: None,
                    priority: Priority::Manifest,
                    blocking: true,
                    retry: false,
                    what: "web login".to_owned(),
                };
                let f = self.fetch(&spec).await?;
                if let Some(n) = npm_notice(&f) {
                    prompt.notice(&n);
                }
                match f.status {
                    202 => {
                        let wait = f
                            .header("retry-after")
                            .and_then(|v| v.trim().parse::<u32>().ok())
                            .map_or(Duration::from_millis(500), |s| {
                                Duration::from_secs(u64::from(s))
                            });
                        tokio::time::sleep(wait).await;
                    }
                    200 => {
                        let v: Value = serde_json::from_slice(&f.body).map_err(|_| {
                            Error::WebLogin("failed to parse response json".to_owned())
                        })?;
                        let Some(token) = v.get("token").and_then(Value::as_str) else {
                            return Err(Error::WebLogin(
                                "missing `token` field in reponse json".to_owned(),
                            ));
                        };
                        if token.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
                            return Err(Error::WebLogin(
                                "invalid `token` field in response json".to_owned(),
                            ));
                        }
                        return Ok(token.to_owned());
                    }
                    _ => return Err(publish_error(&done_url, &f, pkg.name, pkg.version, false)),
                }
            }
        }
        match prompt.read_otp().await {
            Some(otp) if !otp.trim().is_empty() => Ok(otp.trim().to_owned()),
            _ => Err(Error::NeedAuth(
                "This operation requires a one-time password.".to_owned(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths() {
        assert_eq!(normalize_posix("./bin/cli.js"), "bin/cli.js");
        assert_eq!(normalize_posix("bin//x/../cli.js"), "bin/cli.js");
        assert_eq!(normalize_posix("../x"), "x");
        assert_eq!(normalize_posix("lib/"), "lib/");
        assert_eq!(normalize_posix("."), "");
        assert_eq!(normalize_posix(""), ".");
        assert!(is_package_root_or_outside(&normalize_posix("./")));
        assert_eq!(normalize_posix("a/../../x"), "x");
        assert!(is_package_root_or_outside(&normalize_posix("/x")));
        assert_eq!(without_build_tag("1.0.0+abc"), "1.0.0");
        assert_eq!(tarball_filename("@s/p", "1.0.0"), "@s/p-1.0.0.tgz");
    }

    #[test]
    fn normalized_manifest() {
        let mut m =
            json!({ "name": "@s/p", "version": "1.0.0+b", "bin": "./cli.js", "readme": "own" });
        let warnings = normalize_manifest(
            &mut m,
            "@s/p",
            "1.0.0+b",
            b"tarball",
            "https://r.example/npm/",
            Some(("README.md", "# hi")),
            &["cli.js", "package.json"],
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(m["version"], "1.0.0");
        assert_eq!(m["_id"], "@s/p@1.0.0");
        assert_eq!(m["readme"], "own");
        assert!(m.get("readmeFilename").is_none());
        assert_eq!(m["bin"], json!({ "@s/p": "cli.js" }));
        assert_eq!(
            m["dist"]["tarball"],
            "http://r.example/npm/@s/p/-/@s/p-1.0.0+b.tgz"
        );
        let sri = Integrity::compute(HashAlgorithm::Sha512, b"tarball").to_sri();
        assert_eq!(m["_integrity"], sri.as_str());
        assert_eq!(m["dist"]["integrity"], sri.as_str());
        assert_eq!(m["shasum"].as_str().unwrap().len(), 40);
        assert_eq!(m["_npmVersion"], REPORTED_NPM_VERSION);
    }

    #[test]
    fn bins() {
        let mut m = json!({ "bin": { "./a": "./x/../a.js", "": "b.js", "b": 1, "c": "../out.js", "d": "missing.js", "e": "." } });
        let w = normalize_manifest(
            &mut m,
            "p",
            "1.0.0",
            b"",
            "https://r/",
            None,
            &["a.js", "out.js"],
        );
        assert_eq!(
            m["bin"],
            json!({ "a": "a.js", "c": "out.js", "d": "missing.js" })
        );
        assert_eq!(w, vec!["bin 'missing.js' does not exist".to_owned()]);

        let mut m = json!({ "directories": { "bin": "./bin" } });
        normalize_manifest(
            &mut m,
            "p",
            "1.0.0",
            b"",
            "https://r/",
            Some(("README", "r")),
            &["bin/a", "bin/sub/b", "binx/c", "bin"],
        );
        assert_eq!(m["bin"], json!({ "a": "bin/a", "b": "bin/sub/b" }));
        assert_eq!(m["readme"], "r");
        assert_eq!(m["readmeFilename"], "README");

        let mut m = json!({ "bin": {}, "directories": { "bin": "nope" } });
        let w = normalize_manifest(&mut m, "p", "1.0.0", b"", "https://r/", None, &["a"]);
        assert_eq!(m["bin"], json!({}));
        assert_eq!(w, vec!["bin directory 'nope' does not exist".to_owned()]);

        let mut m = json!({ "bin": "x", "directories": { "bin": "bin" } });
        normalize_manifest(
            &mut m,
            "p",
            "1.0.0",
            b"",
            "https://r/",
            None,
            &["x", "bin/y"],
        );
        assert_eq!(m["bin"], json!({ "p": "x" }));
    }

    #[test]
    fn body() {
        let pkg = PublishPackage {
            name: "p",
            version: "1.0.0+b",
            manifest_json: r#"{"name":"p"}"#,
            tarball: b"abc",
        };
        let body = publish_body(&pkg, &PublishOptions::default());
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            json!({
                "_id": "p", "name": "p",
                "dist-tags": { "latest": "1.0.0" },
                "versions": { "1.0.0": { "name": "p" } },
                "access": null,
                "_attachments": { "p-1.0.0+b.tgz": { "content_type": "application/octet-stream", "data": "YWJj", "length": 3 } }
            })
        );
        let options = PublishOptions {
            tag: Some("next".into()),
            access: Some("public".into()),
            ..Default::default()
        };
        let v: Value = serde_json::from_slice(&publish_body(&pkg, &options)).unwrap();
        assert_eq!(v["dist-tags"], json!({ "next": "1.0.0" }));
        assert_eq!(v["access"], "public");
    }

    #[test]
    fn challenges() {
        let f = |header: Option<&str>, body: &str| {
            let mut headers = HeaderMap::new();
            if let Some(h) = header {
                put(&mut headers, "www-authenticate", h);
            }
            Fetched {
                status: 401,
                headers,
                body: Bytes::from(body.to_owned()),
            }
        };
        assert_eq!(challenge(&f(Some("OTP"), "")), Challenge::Otp);
        assert_eq!(
            challenge(&f(Some("Bearer, ipaddress"), "")),
            Challenge::IpAddress
        );
        assert_eq!(
            challenge(&f(Some("Basic"), "")),
            Challenge::Other("Basic".into())
        );
        assert_eq!(
            challenge(&f(None, "You must provide a one-time pass")),
            Challenge::Otp
        );
        assert_eq!(challenge(&f(None, "nope")), Challenge::Other(String::new()));
    }
}
