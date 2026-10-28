//! Redaction of secrets in URLs and config echoes (bun `fmt::redacted_npm_url`).

fn is_scheme_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
}

/// The userinfo of the authority that starts `rest` (right after `scheme://`): up to the last
/// `@` before `/`, `?`, `#` or whitespace. The ssh user `git` is not a secret.
fn find_authority_userinfo(rest: &[u8]) -> Option<(usize, usize)> {
    let end = rest
        .iter()
        .position(|b| matches!(b, b'/' | b'?' | b'#' | b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(rest.len());
    let rest = &rest[..end];
    let at = rest.iter().rposition(|&b| b == b'@')?;
    (at > 0 && &rest[..at] != b"git").then_some((0, at))
}

fn find_url_userinfo(s: &[u8]) -> Option<(usize, usize)> {
    if !s.first().is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    let scheme_len = s.iter().take_while(|&&b| is_scheme_byte(b)).count();
    if !s[scheme_len..].starts_with(b"://") {
        return None;
    }
    let scheme_end = scheme_len + 3;
    let (offset, len) = find_authority_userinfo(&s[scheme_end..])?;
    Some((scheme_end + offset, len))
}

fn next_url_userinfo(s: &[u8], mut from: usize) -> Option<(usize, usize)> {
    while let Some(sep) = s[from..].windows(3).position(|w| w == b"://") {
        let sep = from + sep;
        let scheme_len = s[from..sep]
            .iter()
            .rev()
            .take_while(|&&b| is_scheme_byte(b))
            .count();
        let not_letters = s[sep - scheme_len..sep]
            .iter()
            .take_while(|b| !b.is_ascii_alphabetic())
            .count();
        let start = sep - scheme_len + not_letters;
        if let Some((offset, len)) = find_url_userinfo(&s[start..]) {
            return Some((start + offset, len));
        }
        from = sep + 3;
    }
    None
}

fn starts_with_uuid(s: &[u8]) -> bool {
    s.len() >= 36
        && s[..36].iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// `npm_` / `npms_` followed by 36 to 48 alphanumerics (case-insensitive prefix).
fn npm_secret_len(s: &[u8]) -> usize {
    if s.len() < 3 || !s[..3].eq_ignore_ascii_case(b"npm") {
        return 0;
    }
    let mut i = 3;
    if i < s.len() && (s[i] == b's' || s[i] == b'S') {
        i += 1;
    }
    if i >= s.len() || s[i] != b'_' {
        return 0;
    }
    i += 1;
    let prefix = i;
    while i < s.len() && i - prefix < 48 && s[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i - prefix < 36 { 0 } else { i }
}

/// Masks the userinfo of every URL, UUIDs (npm tokens) and `npm_…` secrets with `***`.
/// A registry written without a scheme (`//user:pass@host/`) has its userinfo masked too.
pub fn redact_url(url: &str) -> String {
    let s = url.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut userinfo = if url.contains("://") {
        next_url_userinfo(s, 0)
    } else {
        let authority = if url.starts_with("//") { 2 } else { 0 };
        find_authority_userinfo(&s[authority..]).map(|(o, l)| (authority + o, l))
    };
    let mut i = 0;
    while i < s.len() {
        if let Some((offset, len)) = userinfo
            && i == offset
        {
            out.extend_from_slice(b"***");
            i += len;
            userinfo = next_url_userinfo(s, i);
            continue;
        }
        if starts_with_uuid(&s[i..]) {
            out.extend_from_slice(b"***");
            i += 36;
            continue;
        }
        let n = npm_secret_len(&s[i..]);
        if n > 0 {
            out.extend_from_slice(b"***");
            i += n;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    mask_token_markers(&String::from_utf8_lossy(&out))
}

/// Masks the value after `_authToken=`, `_auth=`, `_password=` and `token=` (any case), up to
/// the next `/`, `&`, `?` or `#`: credentials written into a registry URL's path or query.
fn mask_token_markers(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let mut out = String::with_capacity(url.len());
    let mut i = 0;
    while i < url.len() {
        let marker = ["_authtoken=", "_auth=", "_password=", "token="]
            .into_iter()
            .find(|m| lower[i..].starts_with(m));
        let Some(marker) = marker else {
            let ch = url[i..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        };
        out.push_str(&url[i..i + marker.len()]);
        i += marker.len();
        // Up to the end of the value: a URL separator, or what ends a URL quoted in a message.
        let end = url[i..]
            .find([
                '/', '&', '#', '?', '"', '\'', ' ', '\t', '\n', ',', ')', ']', '}',
            ])
            .map_or(url.len(), |e| i + e);
        if end > i {
            out.push_str("***");
        }
        i = end;
    }
    out
}

/// bun `fmt::redacted`: [`redact_url`] for a specifier or resolution that holds a URL; anything
/// without `://` (a version, a range, a path) is printed as it is.
pub fn redacted(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains("://") {
        std::borrow::Cow::Owned(redact_url(text))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// A warning or error as it may be shown: the credentials of every URL in it masked, as bun
/// masks each URL it echoes. Text without a URL is unchanged.
pub fn redacted_message(text: &str) -> std::borrow::Cow<'_, str> {
    redacted(text)
}

/// Masks a secret for display, keeping nothing of it.
pub fn redact_secret(secret: &str) -> &'static str {
    if secret.is_empty() { "" } else { "***" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(redact_url("https://user:pass@host/x"), "https://***@host/x");
        assert_eq!(redact_url("https://:tok@host/"), "https://***@host/");
        assert_eq!(redact_url("https://host/x"), "https://host/x");
        assert_eq!(
            redact_url("git+ssh://git@github.com/a/b"),
            "git+ssh://git@github.com/a/b"
        );
        assert_eq!(redact_url("//u:p@host/"), "//***@host/");
        assert_eq!(redact_url("u:p@host/"), "***@host/");
        assert_eq!(
            redact_url("a https://u:p@h/ b http://x:y@z/"),
            "a https://***@h/ b http://***@z/"
        );
        assert_eq!(
            redact_url("https://h/-/x/12345678-1234-1234-1234-123456789abc/y"),
            "https://h/-/x/***/y"
        );
        let npm = format!("npm_{}", "a".repeat(36));
        assert_eq!(redact_url(&format!("https://h/{npm}")), "https://h/***");
        assert_eq!(redact_url("https://h/npm_short"), "https://h/npm_short");
        assert_eq!(redact_url("https://h/é/ü"), "https://h/é/ü");
        assert_eq!(
            redact_url("https://h/api/:_authToken=SECRET/pkg"),
            "https://h/api/:_authToken=***/pkg"
        );
        assert_eq!(
            redact_url("https://h/?TOKEN=x&a=1"),
            "https://h/?TOKEN=***&a=1"
        );
        assert_eq!(
            redacted_message("unrecognised dependency format: @https://u:p@h:1/o/r"),
            "unrecognised dependency format: @https://***@h:1/o/r"
        );
        assert_eq!(redacted_message("ftp://a:b@c@h/x"), "ftp://***@h/x");
        assert_eq!(
            redact_url("\"host?_AuthToken=s3cret\""),
            "\"host?_AuthToken=***\""
        );
        let uuid = "no url, 12345678-1234-1234-1234-123456789abc";
        assert_eq!(redacted_message(uuid), uuid);
    }
}
