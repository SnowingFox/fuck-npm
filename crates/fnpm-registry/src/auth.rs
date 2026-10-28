//! Registry scopes and credentials: which `Authorization` goes on which request.
//!
//! Ports bun's `npm::registry::Scope::build` (credentials embedded in a registry URL), npm's
//! `regFromURI` walk over `.npmrc` URL keys (`UrlAuth::find_entry`), and the tarball rule of
//! `PackageManagerOptions::tarball_credentials`: the `.npmrc` line for the tarball's own URL
//! (https only), else the registry's credentials when the tarball is on the registry's origin,
//! else the URL's own userinfo. Credentials only ever go to http(s) URLs.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use fnpm_config::{Auth, Config, RegistryScope};
use percent_encoding::percent_decode_str;
use url::Url;

/// A configured registry, resolved for requests.
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    /// The registry URL without credentials, ending with `/`. `None` if it does not parse.
    pub url: Option<Url>,
    /// The URL as configured (for messages; may still carry credentials, so redact it).
    pub configured: String,
    /// `Authorization` value.
    pub authorization: Option<String>,
    /// The user name, for `whoami` without a request.
    pub user: Option<String>,
    /// The `.npmrc` key this registry's URL resolves to.
    pub own_key: Option<String>,
    /// Client certificate and key (`certfile` / `keyfile`).
    pub identity: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

impl Scope {
    /// `href` of the credential-free URL, ending with `/`.
    pub fn href(&self) -> &str {
        self.url
            .as_ref()
            .map_or(self.configured.as_str(), Url::as_str)
    }
}

/// One `.npmrc` `//host/path/:_authToken=` style key.
#[derive(Debug, Clone)]
pub(crate) struct UrlAuth {
    /// `host/path/`, lowercase host, no scheme.
    pub key: String,
    pub authorization: String,
}

/// Every scope of a configuration.
#[derive(Debug, Clone)]
pub(crate) struct Scopes {
    pub default: Scope,
    /// `@scope` (with the `@`) to its registry.
    pub named: Vec<(String, Scope)>,
    pub url_auth: Vec<UrlAuth>,
    /// Credentials that were refused (a CR, LF or NUL in the value), as messages.
    pub warnings: Vec<String>,
}

fn breaks_header_line(s: &str) -> bool {
    s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

/// The `Authorization` value of configured credentials. Empty credentials send nothing.
pub(crate) fn authorization_of(auth: &Auth) -> Option<String> {
    match auth {
        Auth::None => None,
        Auth::Token { token } if !token.is_empty() => Some(format!("Bearer {token}")),
        Auth::Basic { username, password } if !username.is_empty() && !password.is_empty() => Some(
            format!("Basic {}", B64.encode(format!("{username}:{password}"))),
        ),
        Auth::Legacy { auth } if !auth.is_empty() => Some(format!("Basic {auth}")),
        _ => None,
    }
}

fn user_of(auth: &Auth) -> Option<String> {
    match auth {
        Auth::Basic { username, .. } if !username.is_empty() => Some(username.clone()),
        Auth::Legacy { auth } => {
            let decoded = B64.decode(auth.trim()).ok()?;
            let decoded = String::from_utf8(decoded).ok()?;
            let colon = decoded.find(':')?;
            (colon > 0 && colon + 1 < decoded.len()).then(|| decoded[..colon].to_owned())
        }
        _ => None,
    }
}

const EMBEDDED_OPTIONS: [&str; 4] = ["_authToken=", "_auth=", "username=", "_password="];

/// The marker nearest the end of `path`: `(start, option index)`. The whole marker is matched,
/// because base64 values can contain `/`.
fn last_embedded_marker(path: &str) -> Option<(usize, usize)> {
    let bytes = path.as_bytes();
    (0..bytes.len())
        .rev()
        .filter(|&i| matches!(bytes[i], b':' | b'/'))
        .find_map(|start| {
            let rest = &path[start + 1..];
            EMBEDDED_OPTIONS
                .iter()
                .position(|name| rest.starts_with(name))
                .map(|o| (start, o))
        })
}

/// A trailing `:<word>=` that is not a known option: a misspelt credential, stripped unused.
fn trailing_misspelt_marker(path: &str) -> Option<usize> {
    let unslashed = path.strip_suffix('/').unwrap_or(path);
    let start = unslashed.rfind('/')? + 1;
    let tail = &unslashed[start..];
    let rest = tail.get(1..)?;
    let word = &rest[..rest.find('=')?];
    let shaped = tail.starts_with(':')
        && word
            .bytes()
            .next()
            .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
        && word
            .bytes()
            .all(|c| c == b'_' || c == b'-' || c.is_ascii_alphanumeric());
    shaped.then_some(start)
}

/// Credentials appended to a registry path: yarn's `/:_authToken=T`, JFrog's `/_auth=<b64>`,
/// `/:username=u/:_password=p`. Returns the path without them and the values in
/// [`EMBEDDED_OPTIONS`] order. A token or `_auth` ends the scan (bun `parse_embedded_auth`).
pub(crate) fn strip_embedded_auth(path: &str) -> (String, [Option<String>; 4]) {
    let mut values: [Option<String>; 4] = Default::default();
    let mut terminal = false;
    let mut path = path;
    loop {
        let start = if let Some((start, option)) = last_embedded_marker(path) {
            let value = &path[start + 1 + EMBEDDED_OPTIONS[option].len()..];
            let value = value.strip_suffix('/').unwrap_or(value);
            if !terminal && !value.is_empty() {
                values[option] = Some(percent_decode_str(value).decode_utf8_lossy().into_owned());
                terminal = option < 2;
            }
            if path.as_bytes()[start] == b'/' {
                path = &path[..start + 1];
                continue;
            }
            start
        } else if let Some(start) = trailing_misspelt_marker(path) {
            start
        } else {
            break;
        };
        path = &path[..start];
        if path.len() > 1 && path.ends_with('/') {
            path = &path[..path.len() - 1];
        }
    }
    (path.to_owned(), values)
}

/// Parses a registry URL and moves any credentials in it into `Auth` (userinfo first, then
/// embedded path markers). `explicit` wins over both.
fn resolve_url(configured: &str, explicit: &Auth) -> (Option<Url>, Auth) {
    let Ok(mut url) = Url::parse(configured.trim()) else {
        return (None, explicit.clone());
    };
    let mut from_url = Auth::None;
    let user = percent_decode_str(url.username())
        .decode_utf8_lossy()
        .into_owned();
    let pass = url
        .password()
        .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned());
    match (user.is_empty(), pass.as_deref()) {
        (false, Some(p)) if !p.is_empty() => {
            from_url = Auth::Basic {
                username: user,
                password: p.to_owned(),
            }
        }
        (false, _) => {
            from_url = Auth::Legacy {
                auth: B64.encode(format!("{user}:")),
            }
        }
        (true, Some(p)) if !p.is_empty() => {
            from_url = Auth::Token {
                token: p.to_owned(),
            }
        }
        _ => {}
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);

    let (path, embedded) = strip_embedded_auth(url.path());
    if path != url.path() {
        url.set_path(&path);
    }
    if matches!(from_url, Auth::None) {
        let [token, legacy, username, password] = embedded;
        from_url = match (token, legacy, username, password) {
            (Some(token), _, _, _) => Auth::Token { token },
            (None, Some(auth), _, _) => Auth::Legacy { auth },
            (None, None, Some(username), Some(password)) => Auth::Basic { username, password },
            _ => Auth::None,
        };
    }
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    let auth = if authorization_of(explicit).is_some() {
        explicit.clone()
    } else {
        from_url
    };
    (Some(url), auth)
}

/// npm's key for a URL (`host[:port]/path/`) and the variant with the scheme's default port.
pub(crate) fn registry_key(url: &Url) -> (String, Option<String>) {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let path = url.path();
    let path = path.strip_suffix('/').unwrap_or(path);
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let key = format!("{host}{port}{path}/");
    let with_default = if url.port().is_none() {
        match url.scheme() {
            "https" => Some(format!("{host}:443{path}/")),
            "http" => Some(format!("{host}:80{path}/")),
            _ => None,
        }
    } else {
        None
    };
    (key, with_default)
}

/// npm's `regFromURI` walk: for `https://h/a/` it yields `h/a/`, `h:443/a/`, `h/a`, `h:443/a`,
/// `h/`, `h:443/`, `h`, `h:443`.
pub(crate) fn walk_keys(url: &Url) -> Vec<String> {
    let (key, with_default) = registry_key(url);
    let mut out = Vec::new();
    let mut end = Some(key.len());
    while let Some(e) = end {
        let current = &key[..e];
        out.push(current.to_owned());
        if let Some(d) = &with_default {
            out.push(d[..e + d.len() - key.len()].to_owned());
        }
        let shorter = match current.strip_suffix('/') {
            Some(rest) => rest.len(),
            None => current.rfind('/').map_or(0, |i| i + 1),
        };
        end = (shorter > 0).then_some(shorter);
    }
    out
}

/// Normalises a configured `.npmrc` key (`//host/path/`, `host/path`, `https://host/path`) to
/// `host/path/` with a lowercase host.
pub(crate) fn normalize_key(key: &str) -> String {
    let k = key.trim();
    let k = k.split_once("://").map_or(k, |(_, rest)| rest);
    let k = k.trim_start_matches('/');
    let (host, path) = k.split_once('/').map_or((k, ""), |(h, p)| (h, p));
    let mut out = host.to_ascii_lowercase();
    out.push('/');
    out.push_str(path);
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

impl Scopes {
    pub fn from_config(config: &Config) -> Self {
        let mut warnings = Vec::new();
        let url_auth: Vec<UrlAuth> = config
            .url_auth
            .iter()
            .filter_map(|(key, auth)| {
                let authorization = authorization_of(auth)?;
                if breaks_header_line(&authorization) {
                    warnings.push(format!("//{key} in .npmrc contains a newline or NUL byte"));
                    return None;
                }
                Some(UrlAuth {
                    key: normalize_key(key),
                    authorization,
                })
            })
            .collect();
        let mut build = |name: &str, scope: &RegistryScope| {
            let (url, auth) = resolve_url(&scope.url, &scope.auth);
            let mut authorization = authorization_of(&auth);
            if authorization.as_deref().is_some_and(breaks_header_line) {
                warnings.push(if name.is_empty() {
                    "the credentials of the default registry contain a newline or NUL byte"
                        .to_owned()
                } else {
                    format!("the credentials of the {name} registry contain a newline or NUL byte")
                });
                authorization = None;
            }
            let own = url.as_ref().and_then(|u| find_entry(&url_auth, u));
            let own_key = own.map(|e| e.key.clone());
            if authorization.is_none() {
                authorization = own.map(|e| e.authorization.clone());
            }
            let identity = scope.cert_file.clone().zip(scope.key_file.clone());
            Scope {
                url,
                configured: scope.url.clone(),
                authorization,
                user: user_of(&auth),
                own_key,
                identity,
            }
        };
        let default = build("", &config.registry);
        let named = config
            .scopes
            .iter()
            .map(|(name, s)| (name.clone(), build(name, s)))
            .collect();
        Self {
            default,
            named,
            url_auth,
            warnings,
        }
    }

    /// The registry of a package name: `@scope/x` uses its scope's registry (exact scope match).
    pub fn for_package(&self, name: &str) -> &Scope {
        name.strip_prefix('@')
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(scope, _)| {
                self.named
                    .iter()
                    .find(|(n, _)| n.strip_prefix('@').unwrap_or(n) == scope)
            })
            .map_or(&self.default, |(_, s)| s)
    }

    /// The scope that owns a URL: same origin, longest path prefix; the default registry if none.
    pub fn for_url(&self, url: &Url) -> &Scope {
        std::iter::once(&self.default)
            .chain(self.named.iter().map(|(_, s)| s))
            .filter(|s| s.url.as_ref().is_some_and(|base| same_origin(url, base)))
            .max_by_key(|s| {
                let base = s.url.as_ref().map_or("", Url::path);
                if url.path().starts_with(base) {
                    base.len() + 1
                } else {
                    0
                }
            })
            .unwrap_or(&self.default)
    }

    /// Credentials for a tarball URL (bun `tarball_credentials`, then the URL's userinfo).
    /// `normalized`: the URL parsed to a stable form, so its own `.npmrc` line may apply.
    pub fn tarball_authorization(
        &self,
        url: &Url,
        userinfo: Option<&str>,
        normalized: bool,
    ) -> Option<String> {
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let scope = self.for_url(url);
        let own = if normalized && url.scheme() == "https" {
            find_entry(&self.url_auth, url)
        } else {
            None
        };
        let configured = match (&scope.authorization, &scope.url) {
            (Some(auth), Some(base)) if same_origin(url, base) => match own {
                Some(entry) if scope.own_key.as_deref() != Some(entry.key.as_str()) => {
                    Some(entry.authorization.clone())
                }
                _ => Some(auth.clone()),
            },
            _ => own.map(|e| e.authorization.clone()),
        };
        configured.or_else(|| userinfo.map(basic_from_userinfo))
    }

    /// Credentials for a registry endpoint URL (manifests, audit, publish, whoami): the owning
    /// registry's if same-origin, else the `.npmrc` line of the URL (https only).
    pub fn endpoint_authorization(&self, url: &Url) -> Option<String> {
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let scope = self.for_url(url);
        if let (Some(auth), Some(base)) = (&scope.authorization, &scope.url)
            && same_origin(url, base)
        {
            return Some(auth.clone());
        }
        if url.scheme() == "https" {
            return find_entry(&self.url_auth, url).map(|e| e.authorization.clone());
        }
        None
    }
}

/// The longest `.npmrc` key on the walk of `url`.
pub(crate) fn find_entry<'a>(list: &'a [UrlAuth], url: &Url) -> Option<&'a UrlAuth> {
    if list.is_empty() || !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    walk_keys(url)
        .into_iter()
        .find_map(|key| list.iter().find(|e| e.key == key))
}

/// Same scheme, host and effective port (`:443` written out or not).
pub(crate) fn same_origin(a: &Url, b: &Url) -> bool {
    b.host_str().is_some_and(|h| !h.is_empty())
        && a.scheme().eq_ignore_ascii_case(b.scheme())
        && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Splits `http://user:pass@host/x.tgz` into the raw userinfo and the URL without it. Only an `@`
/// in the authority counts, not one in `/@scope/`.
pub(crate) fn split_userinfo(raw: &str) -> (Option<String>, String) {
    let Some(scheme_end) = raw.find("://") else {
        return (None, raw.to_owned());
    };
    let authority_start = scheme_end + 3;
    let rest = &raw[authority_start..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    match authority.rfind('@') {
        Some(at) => {
            let userinfo = &authority[..at];
            let without = format!("{}{}", &raw[..authority_start], &rest[at + 1..]);
            ((!userinfo.is_empty()).then(|| userinfo.to_owned()), without)
        }
        None => (None, raw.to_owned()),
    }
}

/// `Basic` of URL userinfo, percent-decoded as curl and node send it. `user` means `user:`.
pub(crate) fn basic_from_userinfo(userinfo: &str) -> String {
    let mut decoded: Vec<u8> = percent_decode_str(userinfo).collect();
    if !userinfo.contains(':') {
        decoded.push(b':');
    }
    format!("Basic {}", B64.encode(decoded))
}

/// A URL with secrets masked as `***`: userinfo (also of a URL without a scheme),
/// `_authToken=`/`_auth=`/`_password=`/`token=` values, UUIDs and npm tokens. Used for every URL
/// in an error or warning.
pub fn redact_url(raw: &str) -> String {
    fnpm_config::redact_url(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn authorization_headers() {
        assert_eq!(
            authorization_of(&Auth::Token { token: "t".into() }).as_deref(),
            Some("Bearer t")
        );
        assert_eq!(
            authorization_of(&Auth::Basic {
                username: "u".into(),
                password: "p".into()
            })
            .as_deref(),
            Some("Basic dTpw")
        );
        assert_eq!(
            authorization_of(&Auth::Legacy {
                auth: "abc=".into()
            })
            .as_deref(),
            Some("Basic abc=")
        );
        assert_eq!(
            authorization_of(&Auth::Token {
                token: String::new()
            }),
            None
        );
        assert_eq!(
            authorization_of(&Auth::Basic {
                username: "u".into(),
                password: String::new()
            }),
            None
        );
        assert_eq!(
            user_of(&Auth::Legacy {
                auth: B64.encode("me:pw")
            })
            .as_deref(),
            Some("me")
        );
        assert_eq!(
            user_of(&Auth::Legacy {
                auth: B64.encode(":pw")
            }),
            None
        );
        assert_eq!(
            user_of(&Auth::Legacy {
                auth: B64.encode("me:")
            }),
            None
        );
    }

    #[test]
    fn embedded_credentials_are_parsed_and_stripped() {
        let (p, v) = strip_embedded_auth("/api/npm/:_authToken=SECRET/");
        assert_eq!(p, "/api/npm");
        assert_eq!(v[0].as_deref(), Some("SECRET"));
        let (p, v) = strip_embedded_auth("/artifactory/api/npm/npm/_auth=dXNlcjpwYXNz/");
        assert_eq!(p, "/artifactory/api/npm/npm/");
        assert_eq!(v[1].as_deref(), Some("dXNlcjpwYXNz"));
        let (p, v) = strip_embedded_auth("/r/:username=me/:_password=cHc=");
        assert_eq!(p, "/r");
        assert_eq!(
            (v[2].as_deref(), v[3].as_deref()),
            (Some("me"), Some("cHc="))
        );
        // A base64 value may contain `/`.
        let (_, v) = strip_embedded_auth("/r/_auth=ab/cd==");
        assert_eq!(v[1].as_deref(), Some("ab/cd=="));
        // Misspelt marker: stripped, never used.
        let (p, v) = strip_embedded_auth("/r/:_authtoken=X");
        assert_eq!(p, "/r");
        assert!(v.iter().all(Option::is_none));
        let (p, _) = strip_embedded_auth("/plain/path/");
        assert_eq!(p, "/plain/path/");
    }

    #[test]
    fn registry_urls_with_credentials() {
        let (u, a) = resolve_url("https://user:p%40ss@reg.example/npm", &Auth::None);
        assert_eq!(u.unwrap().as_str(), "https://reg.example/npm/");
        assert_eq!(
            a,
            Auth::Basic {
                username: "user".into(),
                password: "p@ss".into()
            }
        );
        let (_, a) = resolve_url("https://:tok@reg.example/", &Auth::None);
        assert_eq!(
            a,
            Auth::Token {
                token: "tok".into()
            }
        );
        let (_, a) = resolve_url("https://user@reg.example/", &Auth::None);
        assert_eq!(authorization_of(&a).as_deref(), Some("Basic dXNlcjo="));
        let (u, a) = resolve_url("https://reg.example/api/:_authToken=T/", &Auth::None);
        assert_eq!(u.unwrap().as_str(), "https://reg.example/api/");
        assert_eq!(a, Auth::Token { token: "T".into() });
        // Configured credentials win, the URL is still stripped.
        let (u, a) = resolve_url(
            "https://:tok@reg.example/",
            &Auth::Token {
                token: "cfg".into(),
            },
        );
        assert_eq!(u.unwrap().as_str(), "https://reg.example/");
        assert_eq!(
            a,
            Auth::Token {
                token: "cfg".into()
            }
        );
        assert!(resolve_url("not a url", &Auth::None).0.is_none());
    }

    #[test]
    fn npm_key_walk() {
        assert_eq!(
            walk_keys(&url("https://h/a/")),
            vec![
                "h/a/", "h:443/a/", "h/a", "h:443/a", "h/", "h:443/", "h", "h:443"
            ]
        );
        assert_eq!(
            walk_keys(&url("http://H:8080/x")),
            vec!["h:8080/x/", "h:8080/x", "h:8080/", "h:8080"]
        );
        assert_eq!(normalize_key("//Reg.Example/npm/"), "reg.example/npm/");
        assert_eq!(normalize_key("reg.example/npm"), "reg.example/npm/");
        assert_eq!(normalize_key("https://reg.example"), "reg.example/");
    }

    #[test]
    fn userinfo_split_only_in_authority() {
        assert_eq!(
            split_userinfo("https://u:p@h/x.tgz"),
            (Some("u:p".to_owned()), "https://h/x.tgz".to_owned())
        );
        assert_eq!(
            split_userinfo("https://h/@scope/x.tgz"),
            (None, "https://h/@scope/x.tgz".to_owned())
        );
        assert_eq!(
            split_userinfo("https://@h/x"),
            (None, "https://h/x".to_owned())
        );
        assert_eq!(basic_from_userinfo("user"), "Basic dXNlcjo=");
        assert_eq!(
            basic_from_userinfo("u%3A1:p%20w"),
            format!("Basic {}", B64.encode("u:1:p w"))
        );
    }

    #[test]
    fn redaction() {
        assert_eq!(redact_url("https://u:p@h/x"), "https://***@h/x");
        assert_eq!(
            redact_url("https://h/api/:_authToken=SECRET/pkg"),
            "https://h/api/:_authToken=***/pkg"
        );
        assert_eq!(
            redact_url("https://h/x?token=abc&y=1"),
            "https://h/x?token=***&y=1"
        );
        let token = format!("npm_{}", "a".repeat(36));
        assert_eq!(
            redact_url(&format!("https://h/{token}/x")),
            "https://h/***/x"
        );
        assert_eq!(
            redact_url("https://registry.npmjs.org/react"),
            "https://registry.npmjs.org/react"
        );
    }

    #[test]
    fn origins() {
        assert!(same_origin(&url("https://h:443/a"), &url("https://h/")));
        assert!(same_origin(&url("https://H/a"), &url("https://h/")));
        assert!(!same_origin(&url("http://h/a"), &url("https://h/")));
        assert!(!same_origin(&url("https://h:8443/a"), &url("https://h/")));
        assert!(!same_origin(&url("https://other/a"), &url("https://h/")));
    }
}
