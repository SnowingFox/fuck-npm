//! HTTP(S) proxy selection and `NO_PROXY` matching (bun `dotenv/env_loader.rs` and
//! `dotenv/no_proxy.rs`).
//!
//! `.npmrc` `proxy` / `https-proxy` (as `Config::network.proxy`) win; otherwise `http_proxy` /
//! `HTTP_PROXY` for http and `https_proxy` / `HTTPS_PROXY` for https (the lowercase name first,
//! falling through when it is empty), then `all_proxy` / `ALL_PROXY` for both, but only when it
//! names an http(s) proxy or has no scheme (a SOCKS proxy is left alone). A value with no scheme
//! is an HTTP proxy. `http://DOMAIN\user:pass@proxy:8080` is a domain login, as curl reads it.

use std::net::IpAddr;

use fnpm_config::{Env, ProxyConfig};
use percent_encoding::percent_decode_str;
use url::Url;

/// A proxy URL without its credentials, and the credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProxyTarget {
    pub url: Url,
    pub basic_auth: Option<(String, String)>,
}

/// The proxies of one client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProxySettings {
    pub http: Option<ProxyTarget>,
    pub https: Option<ProxyTarget>,
    /// The `NO_PROXY` list, entries separated by commas or whitespace.
    pub no_proxy: String,
    /// Proxy values that could not be used, as messages.
    pub invalid: Vec<String>,
}

fn is_emptyish(v: &str) -> bool {
    let v = v.trim();
    v.is_empty() || v == "\"\"" || v == "''"
}

fn lower_then_upper<'e>(env: &'e Env, lower: &str, upper: &str) -> Option<&'e str> {
    match env.get(lower) {
        Some(v) if !v.is_empty() => Some(v),
        _ => env.get(upper),
    }
    .filter(|v| !is_emptyish(v))
}

/// Parses a proxy value. `None` if it is not an http(s) proxy.
pub(crate) fn parse_proxy(raw: &str) -> Result<ProxyTarget, String> {
    let raw = raw.trim();
    let with_scheme = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    };
    let scheme_end = with_scheme.find("://").map_or(0, |i| i + 3);
    let rest = &with_scheme[scheme_end..];
    // The userinfo ends at the last `@` before the path; a `\` in it is part of the user name.
    let at = rest
        .find('/')
        .map_or(rest, |slash| &rest[..slash])
        .rfind('@');
    let at = at.or_else(|| rest.rfind('@').filter(|&i| rest[..i].contains('\\')));
    let (userinfo, host_part) = match at {
        Some(at) => (Some(&rest[..at]), &rest[at + 1..]),
        None => (None, rest),
    };
    let url = Url::parse(&format!("{}{}", &with_scheme[..scheme_end], host_part))
        .map_err(|e| format!("invalid proxy URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("unsupported proxy protocol {:?}", url.scheme()));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("proxy URL has no host".to_owned());
    }
    let basic_auth = userinfo.filter(|u| !u.is_empty()).map(|u| {
        let (user, pass) = u.split_once(':').unwrap_or((u, ""));
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
        (decode(user), decode(pass))
    });
    Ok(ProxyTarget { url, basic_auth })
}

impl ProxySettings {
    pub fn resolve(config: &ProxyConfig, env: &Env) -> Self {
        let mut out = Self::default();
        let all = lower_then_upper(env, "all_proxy", "ALL_PROXY").filter(|v| match v.find("://") {
            None => true,
            Some(i) => matches!(v[..i].to_ascii_lowercase().as_str(), "http" | "https"),
        });
        let pick = |configured: &Option<String>, lower: &str, upper: &str| {
            configured
                .as_deref()
                .filter(|v| !is_emptyish(v))
                .or_else(|| lower_then_upper(env, lower, upper))
                .or(all)
                .map(str::to_owned)
        };
        let http = pick(&config.http, "http_proxy", "HTTP_PROXY");
        let https = pick(&config.https, "https_proxy", "HTTPS_PROXY");
        for (raw, slot) in [(http, &mut out.http), (https, &mut out.https)] {
            if let Some(raw) = raw {
                match parse_proxy(&raw) {
                    Ok(t) => *slot = Some(t),
                    Err(e) => out
                        .invalid
                        .push(format!("{e} ({})", crate::auth::redact_url(&raw))),
                }
            }
        }
        out.no_proxy = if config.no_proxy.is_empty() {
            env.get("no_proxy")
                .filter(|v| !is_emptyish(v))
                .or_else(|| env.get("NO_PROXY").filter(|v| !is_emptyish(v)))
                .unwrap_or("")
                .to_owned()
        } else {
            config.no_proxy.join(",")
        };
        out
    }

    /// The proxy for a request URL, or `None` to go direct.
    pub fn for_url(&self, url: &Url) -> Option<&ProxyTarget> {
        let target = match url.scheme() {
            "http" => self.http.as_ref(),
            "https" => self.https.as_ref(),
            _ => None,
        }?;
        let host = url.host_str().unwrap_or("");
        if no_proxy_matches(
            &self.no_proxy,
            host,
            url.port_or_known_default().unwrap_or(0),
        ) {
            return None;
        }
        Some(target)
    }
}

fn strip_brackets(h: &str) -> &str {
    h.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(h)
}

fn strip_trailing_dot(h: &str) -> &str {
    h.strip_suffix('.').unwrap_or(h)
}

fn parse_ip(text: &str) -> Option<IpAddr> {
    text.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

fn split_port(entry: &str) -> (&str, Option<&str>) {
    if let Some(rest) = entry.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            let after = &rest[close + 1..];
            return (&rest[..close], after.strip_prefix(':'));
        }
        return (entry, None);
    }
    if entry.matches(':').count() == 1 {
        let (h, p) = entry.split_once(':').unwrap_or((entry, ""));
        return (h, Some(p));
    }
    (entry, None)
}

fn parse_digits<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn cidr_contains(entry: &str, slash: usize, host: IpAddr) -> bool {
    let Ok(written) = strip_brackets(&entry[..slash]).parse::<IpAddr>() else {
        return false;
    };
    let Some(mut bits) = parse_digits::<u8>(&entry[slash + 1..]) else {
        return false;
    };
    let network = written.to_canonical();
    if written.is_ipv6() && network.is_ipv4() {
        let Some(v4) = bits.checked_sub(96) else {
            return false;
        };
        bits = v4;
    }
    fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
        let bits = usize::from(bits);
        if bits > a.len() * 8 {
            return false;
        }
        let (whole, rem) = (bits / 8, bits % 8);
        if a[..whole] != b[..whole] {
            return false;
        }
        rem == 0 || (a[whole] ^ b[whole]) >> (8 - rem) == 0
    }
    match (network, host) {
        (IpAddr::V4(n), IpAddr::V4(h)) => prefix_eq(&n.octets(), &h.octets(), bits),
        (IpAddr::V6(n), IpAddr::V6(h)) => prefix_eq(&n.octets(), &h.octets(), bits),
        _ => false,
    }
}

/// Whether `hostname` (no port; an IPv6 literal may be bracketed) on `port` bypasses the proxy:
/// `*`, a domain and its subdomains (`example.com`, `.example.com`, `*.example.com`), an IP
/// address, or a CIDR block, each optionally with `:port`. An IP-literal host only matches
/// addresses and CIDR blocks.
pub(crate) fn no_proxy_matches(list: &str, hostname: &str, port: u16) -> bool {
    let hostname = strip_trailing_dot(strip_brackets(hostname));
    if hostname.is_empty() {
        return false;
    }
    let host_ip = parse_ip(hostname);
    for entry in list
        .split([',', ' ', '\t', '\r', '\n'])
        .filter(|e| !e.is_empty())
    {
        if entry == "*" {
            return true;
        }
        if let Some(slash) = entry.find('/') {
            if host_ip.is_some_and(|ip| cidr_contains(entry, slash, ip)) {
                return true;
            }
            continue;
        }
        let (entry_host, entry_port) = split_port(entry);
        if let Some(p) = entry_port {
            match parse_digits::<u16>(p) {
                Some(p) if p == port => {}
                _ => continue,
            }
        }
        let entry_host = entry_host
            .strip_prefix("*.")
            .or_else(|| entry_host.strip_prefix('.'))
            .unwrap_or(entry_host);
        let entry_host = strip_trailing_dot(entry_host);
        if entry_host.is_empty() {
            continue;
        }
        if let Some(ip) = host_ip {
            if parse_ip(entry_host) == Some(ip) {
                return true;
            }
            continue;
        }
        if hostname.len() == entry_host.len() {
            if hostname.eq_ignore_ascii_case(entry_host) {
                return true;
            }
        } else if hostname.len() > entry_host.len()
            && hostname.as_bytes()[hostname.len() - entry_host.len() - 1] == b'.'
            && hostname[hostname.len() - entry_host.len()..].eq_ignore_ascii_case(entry_host)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_domains() {
        let list = "example.com, .other.org *.wild.net";
        assert!(no_proxy_matches(list, "example.com", 443));
        assert!(no_proxy_matches(list, "a.example.com", 443));
        assert!(no_proxy_matches(list, "EXAMPLE.COM.", 443));
        assert!(!no_proxy_matches(list, "notexample.com", 443));
        assert!(no_proxy_matches(list, "other.org", 80));
        assert!(no_proxy_matches(list, "x.y.other.org", 80));
        assert!(no_proxy_matches(list, "wild.net", 80));
        assert!(no_proxy_matches(list, "a.wild.net", 80));
        assert!(!no_proxy_matches(list, "registry.npmjs.org", 443));
        assert!(no_proxy_matches("*", "anything", 1));
        assert!(!no_proxy_matches("", "anything", 1));
        assert!(!no_proxy_matches("example.com", "", 1));
    }

    #[test]
    fn no_proxy_ports() {
        assert!(no_proxy_matches("example.com:8080", "example.com", 8080));
        assert!(!no_proxy_matches("example.com:8080", "example.com", 443));
        assert!(!no_proxy_matches("example.com:8_0", "example.com", 80));
        assert!(no_proxy_matches("[::1]:3000", "[::1]", 3000));
        assert!(!no_proxy_matches("[::1]:3000", "::1", 3001));
    }

    #[test]
    fn no_proxy_addresses() {
        assert!(no_proxy_matches("127.0.0.1", "127.0.0.1", 80));
        assert!(no_proxy_matches("::1", "[::1]", 80));
        assert!(no_proxy_matches("::ffff:127.0.0.1", "127.0.0.1", 80));
        assert!(no_proxy_matches("10.0.0.0/8", "10.1.2.3", 80));
        assert!(!no_proxy_matches("10.0.0.0/8", "11.1.2.3", 80));
        assert!(no_proxy_matches("::ffff:10.0.0.0/104", "10.200.0.1", 80));
        assert!(no_proxy_matches("fd00::/8", "fd12::1", 80));
        assert!(!no_proxy_matches("fd00::/8", "fe80::1", 80));
        assert!(no_proxy_matches("192.168.1.0/24", "192.168.1.255", 80));
        assert!(!no_proxy_matches("192.168.1.0/33", "192.168.1.1", 80));
        // An IP host never matches a domain suffix.
        assert!(!no_proxy_matches("0.0.1", "10.0.0.1", 80));
        // A domain host never matches a CIDR block.
        assert!(!no_proxy_matches("10.0.0.0/8", "example.com", 80));
    }

    #[test]
    fn proxy_values() {
        let t = parse_proxy("proxy.local:3128").unwrap();
        assert_eq!(t.url.as_str(), "http://proxy.local:3128/");
        assert_eq!(t.basic_auth, None);
        let t = parse_proxy("http://u%40x:p%3Aw@proxy:8080/").unwrap();
        assert_eq!(t.url.as_str(), "http://proxy:8080/");
        assert_eq!(t.basic_auth, Some(("u@x".into(), "p:w".into())));
        let t = parse_proxy(r"http://DOMAIN\user:pass@proxy:8080").unwrap();
        assert_eq!(t.url.host_str(), Some("proxy"));
        assert_eq!(t.basic_auth, Some((r"DOMAIN\user".into(), "pass".into())));
        assert!(parse_proxy("socks5://proxy:1080").is_err());
    }

    #[test]
    fn env_and_config_precedence() {
        let env = Env::from_pairs([
            ("http_proxy", ""),
            ("HTTP_PROXY", "http://upper:1"),
            ("https_proxy", "http://lower:2"),
            ("HTTPS_PROXY", "http://ignored:3"),
            ("NO_PROXY", "localhost"),
        ]);
        let s = ProxySettings::resolve(&ProxyConfig::default(), &env);
        assert_eq!(s.http.as_ref().unwrap().url.host_str(), Some("upper"));
        assert_eq!(s.https.as_ref().unwrap().url.host_str(), Some("lower"));
        assert_eq!(s.no_proxy, "localhost");
        let https_url = Url::parse("https://registry.npmjs.org/x").unwrap();
        assert_eq!(s.for_url(&https_url).unwrap().url.host_str(), Some("lower"));
        assert!(
            s.for_url(&Url::parse("http://localhost:4873/x").unwrap())
                .is_none()
        );

        let config = ProxyConfig {
            http: Some("http://cfg:9".into()),
            https: None,
            no_proxy: vec!["a.com".into(), "b.com".into()],
        };
        let s = ProxySettings::resolve(&config, &env);
        assert_eq!(s.http.as_ref().unwrap().url.host_str(), Some("cfg"));
        assert_eq!(s.no_proxy, "a.com,b.com");

        let env = Env::from_pairs([("ALL_PROXY", "socks5://s:1080")]);
        let s = ProxySettings::resolve(&ProxyConfig::default(), &env);
        assert!(s.http.is_none() && s.https.is_none());
        let env = Env::from_pairs([("all_proxy", "allp:8080"), ("http_proxy", "''")]);
        let s = ProxySettings::resolve(&ProxyConfig::default(), &env);
        assert_eq!(s.http.as_ref().unwrap().url.host_str(), Some("allp"));
        assert_eq!(s.https.as_ref().unwrap().url.host_str(), Some("allp"));
        let env = Env::from_pairs([("HTTPS_PROXY", "socks5://bad:1")]);
        let s = ProxySettings::resolve(&ProxyConfig::default(), &env);
        assert!(s.https.is_none());
        assert_eq!(s.invalid.len(), 1);
    }
}
