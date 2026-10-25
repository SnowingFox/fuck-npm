//! Registry URLs and credentials: bun's lenient URL reader, `NpmRegistry::from_url`,
//! `Scope::build` (embedded `:_authToken=` path markers, `$VAR` credentials), npm's
//! `nerfDart` keys and the `regFromURI` walk used by `getAuth`.

use base64::Engine as _;

use crate::{Env, Error, Result};

// ─── lenient URL ─────────────────────────────────────────────────────────────

/// bun's `bun_url::URL::parse`: never fails, keeps what was written (no normalisation), and
/// understands scheme-less and protocol-relative registry values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LenientUrl<'a> {
    pub href: &'a str,
    /// Without `:` / `://`; empty when none was written.
    pub protocol: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    /// Hostname with the port, as written.
    pub host: &'a str,
    pub hostname: &'a str,
    pub port: &'a str,
    /// The path including the query (bun's `pathname`); `/` when empty.
    pub pathname: &'a str,
}

fn ends_authority(byte: u8, backslash_ends_it: bool) -> bool {
    matches!(byte, b'/' | b'?' | b'#') || (backslash_ends_it && byte == b'\\')
}

impl<'a> LenientUrl<'a> {
    pub fn parse(base: &'a str) -> Self {
        let mut url = LenientUrl {
            href: base,
            pathname: "/",
            ..Default::default()
        };
        if base.is_empty() {
            return url;
        }
        let bytes = base.as_bytes();
        let mut offset = 0usize;
        match bytes[0] {
            b'@' => {
                if let Some(at) = base.find(['@', '/', '?']).filter(|&i| bytes[i] == b'@') {
                    url.password = &base[..at];
                    offset = at + 1;
                }
                offset += url.parse_host(&base[offset..]);
            }
            b'[' => offset += url.parse_host(base),
            b'/' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b':' => {
                let protocol_relative = base.starts_with("//");
                if protocol_relative {
                    offset = 2;
                } else if let Some(len) = url.parse_protocol(base) {
                    offset = len;
                }
                let relative_path = !protocol_relative && bytes[0] == b'/';
                if !relative_path {
                    if offset > 0 {
                        let rest = &base[offset..];
                        if let Some(at) = url.userinfo_end(rest) {
                            let userinfo = &rest[..at];
                            let (u, p) = userinfo.split_once(':').unwrap_or((userinfo, ""));
                            url.username = u;
                            url.password = p;
                            offset += at + 1;
                        }
                    }
                    offset += url.parse_host(&base[offset..]);
                }
            }
            _ => {}
        }
        if offset < base.len() && bytes[offset] == b'/' {
            let rest = &base[offset..];
            let end = rest.find('#').unwrap_or(rest.len());
            let mut pathname = &rest[..end];
            while pathname.len() > 1 && pathname.starts_with("//") {
                pathname = &pathname[1..];
            }
            if !pathname.is_empty() {
                url.pathname = pathname;
            }
        }
        url
    }

    fn has_special_scheme(&self) -> bool {
        ["http", "https", "ws", "wss", "ftp", "file"]
            .iter()
            .any(|s| self.protocol.eq_ignore_ascii_case(s))
    }

    fn userinfo_end(&self, after_scheme: &str) -> Option<usize> {
        let backslash = self.has_special_scheme();
        let mut last_at = None;
        for (i, b) in after_scheme.bytes().enumerate() {
            if ends_authority(b, backslash) {
                break;
            }
            if b == b'@' {
                last_at = Some(i);
            }
        }
        last_at
    }

    fn parse_protocol(&mut self, s: &'a str) -> Option<usize> {
        if s.len() < 3 {
            return None;
        }
        let bytes = s.as_bytes();
        for i in 0..bytes.len() {
            match bytes[i] {
                b'/' | b'?' | b'%' => return None,
                b':' if i + 3 <= bytes.len() && bytes[i + 1] == b'/' && bytes[i + 2] == b'/' => {
                    let protocol = &s[..i];
                    let is_scheme = protocol
                        .bytes()
                        .next()
                        .is_some_and(|b| b.is_ascii_alphabetic())
                        && protocol
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
                    if !is_scheme {
                        return None;
                    }
                    self.protocol = protocol;
                    return Some(i + 3);
                }
                _ => {}
            }
        }
        None
    }

    fn parse_host(&mut self, s: &'a str) -> usize {
        let backslash = self.has_special_scheme();
        let bytes = s.as_bytes();
        let mut i = 0;
        if bytes.first() == Some(&b'[') {
            i = 1;
            let mut close = None;
            let mut colon = None;
            while i < bytes.len() {
                if close.is_none() && bytes[i] == b']' {
                    close = Some(i);
                }
                if close.is_some() && colon.is_none() && bytes[i] == b':' {
                    colon = Some(i);
                }
                if ends_authority(bytes[i], backslash) {
                    break;
                }
                i += 1;
            }
            self.host = &s[..i];
            if let Some(close) = close {
                self.hostname = &s[..=close];
            }
            if let Some(colon) = colon {
                self.port = &s[colon + 1..i];
            }
        } else {
            let mut colon = None;
            while i < bytes.len() {
                if colon.is_none() && bytes[i] == b':' {
                    colon = Some(i);
                }
                if ends_authority(bytes[i], backslash) {
                    break;
                }
                i += 1;
            }
            self.host = &s[..i];
            match colon {
                Some(colon) => {
                    self.hostname = &s[..colon];
                    self.port = &s[colon + 1..i];
                }
                None => self.hostname = self.host,
            }
        }
        i
    }

    pub fn is_https(&self) -> bool {
        self.protocol.eq_ignore_ascii_case("https")
    }

    pub fn is_http(&self) -> bool {
        self.protocol.eq_ignore_ascii_case("http")
    }

    fn display_protocol(&self) -> &str {
        if !self.protocol.is_empty() {
            return self.protocol;
        }
        if self.port == "443" {
            return "https";
        }
        "http"
    }

    /// The URL without its userinfo, ending in exactly one `/`: `http://host/`, `http://host/npm/`.
    pub fn href_without_auth(&self) -> String {
        let path = self.pathname.trim_matches('/');
        let host = if self.host.is_empty() {
            "localhost"
        } else {
            self.host
        };
        let mut out = format!("{}://{}/", self.display_protocol(), host);
        if !path.is_empty() {
            out.push_str(path);
            out.push('/');
        }
        out
    }

    /// `pathname` without the query.
    pub fn query_free_path(&self) -> &'a str {
        let p = self.pathname;
        &p[..p.find('?').unwrap_or(p.len())]
    }
}

fn percent_decode_lenient(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Standard base64 with padding.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Node's `Buffer.from(value, "base64")`: accepts the standard and URL-safe alphabets, skips
/// every other byte, stops at the first `=`, and never fails.
pub(crate) fn base64_decode_lenient(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for b in value.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Strict base64 decode (for deriving the `whoami` user of an `_auth`).
fn base64_decode_strict(value: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

/// CR, LF or NUL would break the `Authorization` header line.
pub(crate) fn breaks_header_line(value: &str) -> bool {
    value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
}

/// One registry as a config file wrote it (bun's `Api::NpmRegistry`): a URL plus the
/// credentials that came with it. Credentials are not resolved yet (`$VAR`, embedded
/// `:_authToken=` markers); [`Config`](crate::Config) loading does that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrySource {
    /// The URL with any `user:pass@` removed.
    pub url: String,
    /// The user name of `username` + `password`.
    pub username: String,
    /// Plain text (not base64).
    pub password: String,
    /// `_authToken` / `token`.
    pub token: String,
    /// `.npmrc` `_auth`, sent verbatim as `Basic <auth>`.
    pub auth: String,
    /// Set by a `registry=` / `@scope:registry=` line: a credential inside that URL yields to
    /// any `.npmrc` credential line for it.
    pub declared_in_npmrc: bool,
}

impl RegistrySource {
    /// Splits the credentials written into a URL out of it. Userinfo is percent-decoded
    /// (RFC 3986 §3.2.1): `user:pass@` is a username and password, `user@` is `user` with an
    /// empty password (kept as the `_auth` it amounts to), `:token@` is a token.
    pub fn from_url(s: &str) -> Self {
        let url = LenientUrl::parse(s);
        let mut registry = RegistrySource::default();
        if !url.username.is_empty() && !url.password.is_empty() {
            registry.username = percent_decode_lenient(url.username);
            registry.password = percent_decode_lenient(url.password);
        } else if !url.username.is_empty() {
            let username = percent_decode_lenient(url.username);
            if breaks_header_line(&username) {
                // Left where the credential check refuses it.
                registry.username = username;
            } else {
                registry.auth = base64_encode(format!("{username}:").as_bytes());
            }
        } else if !url.password.is_empty() {
            registry.token = percent_decode_lenient(url.password);
        } else {
            registry.url = s.to_owned();
            return registry;
        }
        registry.url = href_without_auth(&url);
        registry
    }

    /// A URL with only these credentials.
    pub fn with_url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    /// Any credential field is set.
    pub fn has_credentials(&self) -> bool {
        !self.token.is_empty()
            || !self.auth.is_empty()
            || !self.username.is_empty()
            || !self.password.is_empty()
    }
}

/// bun's `NpmRegistry::href_without_auth`: no `http://` is invented for a scheme-less URL.
fn href_without_auth(url: &LenientUrl<'_>) -> String {
    let href = url.href_without_auth();
    if !url.protocol.is_empty() {
        return href;
    }
    href[url.display_protocol().len() + 3..].to_owned()
}

/// Credentials of one registry, after every source was applied.
pub use crate::Auth;

impl Auth {
    /// Any credential.
    pub fn is_some(&self) -> bool {
        !matches!(self, Auth::None)
    }

    /// The `Authorization` header value.
    pub fn header_value(&self) -> Option<String> {
        match self {
            Auth::None => None,
            Auth::Token { token } => Some(format!("Bearer {token}")),
            Auth::Basic { username, password } => Some(format!(
                "Basic {}",
                base64_encode(format!("{username}:{password}").as_bytes())
            )),
            Auth::Legacy { auth } => Some(format!("Basic {auth}")),
        }
    }

    /// The user name, for `pm whoami`: the Basic user, or what a decodable `_auth` names.
    pub fn username(&self) -> Option<String> {
        match self {
            Auth::Basic { username, .. } => Some(username.clone()),
            Auth::Legacy { auth } => {
                let decoded = String::from_utf8(base64_decode_strict(auth)?).ok()?;
                let colon = decoded.find(':')?;
                (colon > 0 && colon + 1 < decoded.len()).then(|| decoded[..colon].to_owned())
            }
            _ => None,
        }
    }
}

/// A resolved registry (bun's `Npm::registry::Scope`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scope {
    /// The WHATWG serialisation of the URL (or the URL as written if it does not parse).
    pub url: String,
    pub auth: Auth,
    pub declared_in_npmrc: bool,
    /// The `.npmrc` key this registry's URL resolves to, once every source was applied.
    pub url_auth_key: Option<String>,
}

/// After `:` in yarn's shape (`/api/:_authToken=T`), after `/` in JFrog's (`/npm/_auth=<b64>`).
const EMBEDDED_OPTIONS: [&str; 4] = ["_authToken=", "_auth=", "username=", "_password="];

#[derive(Default)]
struct EmbeddedAuth<'a> {
    values: [Option<&'a str>; 4],
    terminal: bool,
}

fn last_embedded_marker(pathname: &str) -> Option<(usize, usize)> {
    let bytes = pathname.as_bytes();
    (0..bytes.len())
        .rev()
        .filter(|&s| matches!(bytes[s], b':' | b'/'))
        .find_map(|start| {
            let rest = &pathname[start + 1..];
            let option = EMBEDDED_OPTIONS
                .iter()
                .position(|name| rest.starts_with(name))?;
            Some((start, option))
        })
}

fn trailing_misspelt_marker(pathname: &str) -> Option<usize> {
    let unslashed = pathname.strip_suffix('/').unwrap_or(pathname);
    let start = unslashed.rfind('/')? + 1;
    let tail = &unslashed[start..];
    let after = tail.get(1..)?;
    let word = &after[..after.find('=')?];
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

/// Strips `:_authToken=…` style markers from the path; the values are returned.
fn parse_embedded_auth<'a>(pathname: &mut &'a str) -> EmbeddedAuth<'a> {
    let mut out = EmbeddedAuth::default();
    let mut path: &'a str = pathname;
    loop {
        let start = if let Some((start, option)) = last_embedded_marker(path) {
            let value = &path[start + 1 + EMBEDDED_OPTIONS[option].len()..];
            let value = value.strip_suffix('/').unwrap_or(value);
            if !out.terminal && !value.is_empty() {
                out.values[option] = Some(value);
                out.terminal = option < 2;
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
    *pathname = path;
    out
}

/// Normalises a URL to its WHATWG serialisation, keeping the input if it does not parse.
pub(crate) fn whatwg_href(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => u.to_string(),
        Err(_) => url.to_owned(),
    }
}

/// Where a credential came from, for the error that refuses a CR/LF/NUL in it.
#[derive(Clone, Copy)]
pub(crate) enum CredentialSite<'a> {
    /// The registry of a config file; `""` names the default registry.
    Registry(&'a str),
    /// A `//<key>:<opt>` line of `.npmrc`.
    NpmrcLine(&'a str),
}

fn refuse(site: CredentialSite<'_>, what: &str, npmrc_option: &str) -> Error {
    let message = match site {
        CredentialSite::Registry("") => {
            format!("the {what} of the default registry contains a newline or NUL byte")
        }
        CredentialSite::Registry(name) => {
            format!(
                "the {what} of the @{} registry contains a newline or NUL byte",
                name.escape_debug()
            )
        }
        CredentialSite::NpmrcLine(key) => {
            format!(
                "//{}:{npmrc_option} in .npmrc contains a newline or NUL byte",
                key.escape_debug()
            )
        }
    };
    Error::InvalidValue {
        key: what.to_owned(),
        message,
    }
}

/// bun's `Scope::build`: resolves `$URL`, the URL's userinfo, `$VAR` credentials and embedded
/// path markers, and refuses credentials that would break the header line.
/// `unset_variable_is_no_credential` is `false` for `.npmrc` lines, where `$` before no
/// variable's name is a literal.
pub(crate) fn build_scope(
    site: CredentialSite<'_>,
    mut registry: RegistrySource,
    env: &Env,
    unset_variable_is_no_credential: bool,
    warnings: &mut Vec<String>,
) -> Result<Scope> {
    if let Some(name) = registry.url.strip_prefix('$')
        && let Some(replaced) = env.get(name.trim_matches('/'))
        && replaced.len() > 1
    {
        registry.url = replaced.to_owned();
    }
    let from_url = RegistrySource::from_url(&registry.url);
    registry.url = from_url.url.clone();
    if !registry.has_credentials() {
        registry.token = from_url.token;
        registry.auth = from_url.auth;
        registry.username = from_url.username;
        registry.password = from_url.password;
    }

    for (what, field) in [
        ("token", &mut registry.token),
        ("username", &mut registry.username),
        ("password", &mut registry.password),
    ] {
        let Some(name) = field.strip_prefix('$') else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            continue;
        }
        match env.get(name) {
            Some(value) => *field = value.to_owned(),
            None if unset_variable_is_no_credential => {
                warnings.push(format!(
                    "registry {what} references unset variable {field}, ignoring credential"
                ));
                field.clear();
            }
            None => {}
        }
    }

    let url = LenientUrl::parse(&registry.url);
    let mut pathname = url.pathname;
    let original_len = pathname.len();
    let embedded = parse_embedded_auth(&mut pathname);
    let needs_normalize = pathname.len() != original_len;

    let mut auth = String::new();
    if registry.token.is_empty() {
        'outer: {
            if registry.password.is_empty() && registry.auth.is_empty() {
                let [token, embedded_auth, username, password] = embedded.values;
                if let Some(token) = token {
                    registry.token = env.get_auto(token).to_owned();
                }
                if let Some(a) = embedded_auth {
                    auth = a.to_owned();
                }
                if let Some(username) = username {
                    registry.username = env.get_auto(username).to_owned();
                }
                if let Some(password) = password {
                    registry.password = env.get_auto(password).to_owned();
                }
                if embedded.terminal {
                    break 'outer;
                }
            }
            if !registry.auth.is_empty() {
                auth = std::mem::take(&mut registry.auth);
                break 'outer;
            }
            if breaks_header_line(&registry.username) {
                return Err(refuse(site, "username", "username"));
            }
            if breaks_header_line(&registry.password) {
                return Err(refuse(site, "password", "_password"));
            }
        }
    }
    if breaks_header_line(&registry.token) {
        return Err(refuse(site, "token", "_authToken"));
    }
    if breaks_header_line(&auth) {
        return Err(refuse(site, "_auth", "_auth"));
    }

    let resolved = if !registry.token.is_empty() {
        Auth::Token {
            token: registry.token,
        }
    } else if !auth.is_empty() {
        Auth::Legacy { auth }
    } else if !registry.username.is_empty() && !registry.password.is_empty() {
        Auth::Basic {
            username: registry.username,
            password: registry.password,
        }
    } else {
        Auth::None
    };

    let final_href = if needs_normalize {
        let stripped = LenientUrl {
            pathname,
            ..url.clone()
        };
        href_without_auth(&stripped)
    } else {
        registry.url.clone()
    };
    Ok(Scope {
        url: whatwg_href(&final_href),
        auth: resolved,
        declared_in_npmrc: registry.declared_in_npmrc,
        url_auth_key: None,
    })
}

// ─── npm config keys ─────────────────────────────────────────────────────────

/// npm's `nerfDart` for the text between `//` and `:<opt>=`: drops a scheme, lowercases the
/// host, and drops the port if it is the default of a scheme that was written out.
pub fn normalize_key(raw: &str) -> String {
    let has_scheme = |scheme: &str| {
        raw.len() >= scheme.len() && raw[..scheme.len()].eq_ignore_ascii_case(scheme)
    };
    let (default_port, rest) = if has_scheme("https://") {
        (Some("443"), &raw[8..])
    } else if has_scheme("http://") {
        (Some("80"), &raw[7..])
    } else {
        (None, raw)
    };
    let host_end = rest.find('/').unwrap_or(rest.len());
    let mut key = rest[..host_end].to_ascii_lowercase();
    // In a bracketed IPv6 authority only a colon after `]` introduces a port.
    if let Some(default_port) = default_port
        && let Some(colon) = key.rfind(':')
    {
        let is_port = !key.starts_with('[') || (colon > 0 && key.as_bytes()[colon - 1] == b']');
        if is_port && &key[colon + 1..] == default_port {
            key.truncate(colon);
        }
    }
    key.push_str(&rest[host_end..]);
    key
}

/// A URL as npm's key walk sees it: its WHATWG serialisation without the scheme and the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryKey {
    /// `<host><path>/`, npm's key for the registry itself and where the walk starts.
    key: String,
    /// `key` with the scheme's default port written out (`//host:443/` lines).
    with_default_port: Option<String>,
}

impl RegistryKey {
    /// Parses `url` as a WHATWG URL, falling back to the lenient reader for values like
    /// `//host:8080/npm/`.
    pub fn from_url(url: &str) -> Self {
        match url::Url::parse(url) {
            Ok(parsed) if parsed.host_str().is_some() => Self::from_whatwg(&parsed),
            _ => Self::from_lenient(&LenientUrl::parse(url)),
        }
    }

    fn build(
        host: &str,
        explicit_port: Option<String>,
        path: &str,
        default_port: Option<&str>,
    ) -> Self {
        let path = path.strip_suffix('/').unwrap_or(path);
        let with_port = |port: &str| {
            let mut key = String::with_capacity(host.len() + port.len() + path.len() + 1);
            key.push_str(&host.to_ascii_lowercase());
            if let Some(p) = &explicit_port {
                key.push(':');
                key.push_str(p);
            }
            key.push_str(port);
            key.push_str(path);
            key.push('/');
            key
        };
        let default_port = if explicit_port.is_some() {
            None
        } else {
            default_port
        };
        Self {
            key: with_port(""),
            with_default_port: default_port.map(with_port),
        }
    }

    pub(crate) fn from_whatwg(url: &url::Url) -> Self {
        let host = url.host_str().unwrap_or("");
        let default_port = match url.scheme() {
            "https" => Some(":443"),
            "http" => Some(":80"),
            _ => None,
        };
        Self::build(
            host,
            url.port().map(|p| p.to_string()),
            url.path(),
            default_port,
        )
    }

    fn from_lenient(url: &LenientUrl<'_>) -> Self {
        let default_port = if url.is_https() {
            Some(":443")
        } else if url.is_http() {
            Some(":80")
        } else {
            None
        };
        // The lenient host already carries the port as written.
        let default_port = if url.port.is_empty() {
            default_port
        } else {
            None
        };
        Self::build(url.host, None, url.query_free_path(), default_port)
    }

    /// The registry's own key (`host/path/`).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// npm's `regFromURI`. For `https://h/a/` it yields `h/a/`, `h:443/a/`, `h/a`, `h:443/a`,
    /// `h/`, `h:443/`, `h`, `h:443`.
    pub fn walk(&self) -> impl Iterator<Item = &str> {
        let mut next = Some(self.key.len());
        let mut pending_port: Option<&str> = None;
        std::iter::from_fn(move || {
            if let Some(key) = pending_port.take() {
                return Some(key);
            }
            let end = next.take()?;
            let current = &self.key[..end];
            pending_port = self
                .with_default_port
                .as_deref()
                .map(|k| &k[..end + k.len() - self.key.len()]);
            // npm's `regKey.replace(/([^/]+|\/)$/, '')`
            let shorter = match current.strip_suffix('/') {
                Some(rest) => rest.len(),
                None => current.rfind('/').map_or(0, |i| i + 1),
            };
            next = (shorter > 0).then_some(shorter);
            Some(current)
        })
    }
}

/// The deepest `.npmrc` key that covers `url`, walking like npm's `getAuth`. Only http(s)
/// URLs get one. `keys` yields each entry's key.
pub(crate) fn find_entry<'a, T>(list: &'a [(String, T)], url: &str) -> Option<&'a (String, T)> {
    if list.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let key = RegistryKey::from_whatwg(&parsed);
    key.walk()
        .find_map(|k| list.iter().find(|(entry, _)| entry == k))
}

/// Same scheme, host and effective port.
pub(crate) fn same_origin(url: &str, base: &str) -> bool {
    let (Ok(a), Ok(b)) = (url::Url::parse(url), url::Url::parse(base)) else {
        return false;
    };
    b.host_str().is_some_and(|h| !h.is_empty())
        && a.scheme().eq_ignore_ascii_case(b.scheme())
        && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && a.port_or_known_default() == b.port_or_known_default()
}

/// The `/`-separated, non-empty segments of a URL's query-free path.
pub(crate) fn path_segments(url: &str) -> Vec<String> {
    LenientUrl::parse(url)
        .query_free_path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The lenient host (with port) of `url`, without a trailing `/`.
pub(crate) fn lenient_host(url: &str) -> String {
    LenientUrl::parse(url).host.trim_end_matches('/').to_owned()
}

/// `true` for `https://` URLs (case-insensitive).
pub(crate) fn is_https(url: &str) -> bool {
    LenientUrl::parse(url).is_https()
}

#[cfg(test)]
#[path = "tests/auth.rs"]
mod tests;
