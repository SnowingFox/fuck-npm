//! Package names: npm validation rules, scoped names and the name helpers that
//! `bun/src/install/dependency.rs` and `bun_core::strings::is_npm_package_name` provide.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The longest name npm accepts.
pub const MAX_NAME_LEN: usize = 214;

/// A registry package name such as `react` or `@types/node`.
///
/// Construction through [`PackageName::parse`] applies npm's rules for names that may already
/// exist on a registry (upper case is allowed for old packages). Use
/// [`validate_new_package_name`] for the stricter rules that apply to `init`, `pack` and
/// `publish`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackageName(Box<str>);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("package name is empty")]
    Empty,
    #[error("package name is longer than {MAX_NAME_LEN} characters")]
    TooLong,
    #[error("invalid package name {0:?}")]
    Invalid(String),
}

impl PackageName {
    /// Validates with npm's rules for existing registry names (bun's `is_npm_package_name`).
    pub fn parse(name: &str) -> Result<Self, NameError> {
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        if name.len() > MAX_NAME_LEN {
            return Err(NameError::TooLong);
        }
        if !is_npm_package_name(name) || is_scoped_package_name(name).is_err() {
            return Err(NameError::Invalid(name.to_owned()));
        }
        Ok(Self(name.into()))
    }

    /// Skips validation. For names read back from a trusted source such as our own lockfile,
    /// or aliases that are only checked with [`is_safe_install_folder_name`].
    pub fn new_unchecked(name: impl Into<Box<str>>) -> Self {
        Self(name.into())
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[inline]
    pub fn is_scoped(&self) -> bool {
        self.0.starts_with('@')
    }

    /// `@types/node` gives `Some("@types")`.
    pub fn scope(&self) -> Option<&str> {
        if !self.is_scoped() {
            return None;
        }
        self.0.find('/').map(|i| &self.0[..i])
    }

    /// `@types/node` gives `node`.
    pub fn unscoped(&self) -> &str {
        unscoped_package_name(&self.0)
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl AsRef<str> for PackageName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for PackageName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for PackageName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PackageName {
    type Error = NameError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl TryFrom<&str> for PackageName {
    type Error = NameError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<PackageName> for String {
    fn from(value: PackageName) -> Self {
        value.0.into()
    }
}

/// bun's `strings::is_npm_package_name`: the lenient rule for names that already exist.
pub fn is_npm_package_name(target: &str) -> bool {
    target.len() <= MAX_NAME_LEN && is_npm_package_name_ignore_length(target)
}

pub fn is_npm_package_name_ignore_length(target: &str) -> bool {
    let target = target.as_bytes();
    if target.is_empty() {
        return false;
    }
    let scoped = match target[0] {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'$' | b'-' => false,
        b'@' => true,
        _ => return false,
    };
    let mut slash_index = 0usize;
    for (i, &c) in target[1..].iter().enumerate() {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {}
            b'/' => {
                if !scoped || slash_index > 0 {
                    return false;
                }
                slash_index = i + 1;
            }
            // encodeURIComponent leaves these alone; only valid in a scope (`@~3/svelte_mount`).
            b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                if !scoped || slash_index > 0 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    !scoped || (slash_index > 0 && slash_index + 1 < target.len())
}

/// One reason a name is not acceptable for a new package (validate-npm-package-name).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameProblem {
    #[error("name length must be greater than zero")]
    Empty,
    #[error("name cannot start with a period")]
    LeadingPeriod,
    #[error("name cannot start with an underscore")]
    LeadingUnderscore,
    #[error("name cannot contain leading or trailing spaces")]
    SurroundingSpaces,
    #[error("{0} is a blacklisted name")]
    Blacklisted(String),
    #[error("{0} is a core module name")]
    CoreModule(String),
    #[error("name can no longer contain more than 214 characters")]
    TooLong,
    #[error("name can no longer contain capital letters")]
    Uppercase,
    #[error("name can no longer contain special characters (\"~'!()*\")")]
    SpecialCharacters,
    #[error("name can only contain URL-friendly characters")]
    NotUrlFriendly,
}

const NODE_BUILTINS: &[&str] = &[
    "assert", "async_hooks", "buffer", "child_process", "cluster", "console", "constants",
    "crypto", "dgram", "diagnostics_channel", "dns", "domain", "events", "fs", "http", "http2",
    "https", "inspector", "module", "net", "os", "path", "perf_hooks", "process", "punycode",
    "querystring", "readline", "repl", "stream", "string_decoder", "sys", "timers", "tls",
    "trace_events", "tty", "url", "util", "v8", "vm", "wasi", "worker_threads", "zlib",
];

fn is_uri_component_safe(s: &str) -> bool {
    s.bytes().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
    })
}

/// The rules npm applies to a new package (`validForNewPackages`). Every problem is reported.
pub fn validate_new_package_name(name: &str) -> Result<(), Vec<NameProblem>> {
    let mut problems = Vec::new();
    if name.is_empty() {
        problems.push(NameProblem::Empty);
        return Err(problems);
    }
    if name.starts_with('.') {
        problems.push(NameProblem::LeadingPeriod);
    }
    if name.starts_with('_') {
        problems.push(NameProblem::LeadingUnderscore);
    }
    if name.trim() != name {
        problems.push(NameProblem::SurroundingSpaces);
    }
    let lower = name.to_ascii_lowercase();
    if lower == "node_modules" || lower == "favicon.ico" {
        problems.push(NameProblem::Blacklisted(name.to_owned()));
    }
    if NODE_BUILTINS.contains(&lower.as_str()) {
        problems.push(NameProblem::CoreModule(name.to_owned()));
    }
    if name.len() > MAX_NAME_LEN {
        problems.push(NameProblem::TooLong);
    }
    if lower != name {
        problems.push(NameProblem::Uppercase);
    }
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.bytes().any(|c| matches!(c, b'~' | b'\'' | b'!' | b'(' | b')' | b'*')) {
        problems.push(NameProblem::SpecialCharacters);
    }
    if !is_uri_component_safe(name) {
        let scoped_ok = name
            .strip_prefix('@')
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(user, pkg)| {
                !user.is_empty() && !pkg.is_empty() && is_uri_component_safe(user) && is_uri_component_safe(pkg)
            });
        if !scoped_ok {
            problems.push(NameProblem::NotUrlFriendly);
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems) }
}

/// `@scope/name` gives `name`; an unscoped name is returned as is.
pub fn unscoped_package_name(name: &str) -> &str {
    if !name.starts_with('@') {
        return name;
    }
    match name[1..].find('/') {
        Some(i) => &name[i + 2..],
        None => name,
    }
}

/// `Ok(true)` for `@scope/name`, `Ok(false)` for `name`, and an error for `@scope`, `@/x`,
/// `@x/` and the empty string.
pub fn is_scoped_package_name(name: &str) -> Result<bool, NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if !name.starts_with('@') {
        return Ok(false);
    }
    if let Some(slash) = name.find('/')
        && slash != 1
        && slash != name.len() - 1
    {
        return Ok(true);
    }
    Err(NameError::Invalid(name.to_owned()))
}

/// `foo@1.1.1` gives (`foo`, `Some("1.1.1")`), `@foo/bar@1` gives (`@foo/bar`, `Some("1")`),
/// `foo` and `foo@` give (`foo`, `None`).
pub fn split_name_and_maybe_version(s: &str) -> (&str, Option<&str>) {
    let Some(at) = s.find('@') else {
        return (s, None);
    };
    if at != 0 {
        let rest = &s[at + 1..];
        return (&s[..at], (!rest.is_empty()).then_some(rest));
    }
    let Some(second) = s[1..].find('@') else {
        return (s, None);
    };
    let second = second + 1;
    let rest = &s[second + 1..];
    (&s[..second], (!rest.is_empty()).then_some(rest))
}

/// Like [`split_name_and_maybe_version`] but a missing version is `latest`.
pub fn split_name_and_version_or_latest(s: &str) -> (&str, &str) {
    let (name, version) = split_name_and_maybe_version(s);
    (name, version.unwrap_or("latest"))
}

/// C0 controls, DEL and UTF-8 encoded C1 controls, which terminals interpret.
pub fn contains_control_character(name: &str) -> bool {
    let b = name.as_bytes();
    b.iter().enumerate().any(|(i, &byte)| {
        byte.is_ascii_control() || (byte == 0xC2 && b.get(i + 1).is_some_and(|n| (0x80..=0x9F).contains(n)))
    })
}

/// An untrusted name or alias becomes a directory under `node_modules/` and is printed.
pub fn is_safe_install_folder_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 255 + 1 + 255 {
        return false;
    }
    let safe = |c: &[u8]| c.len() <= 255 && !matches!(c, b"" | b"." | b"..");
    let mut start = 0;
    for (i, &byte) in b.iter().enumerate() {
        match byte {
            b'/' => {
                if !safe(&b[start..i]) {
                    return false;
                }
                start = i + 1;
            }
            b'\\' | b':' => return false,
            0xC2 if matches!(b.get(i + 1), Some(0x80..=0x9F)) => return false,
            _ if byte.is_ascii_control() => return false,
            _ => {}
        }
    }
    safe(&b[start..])
}

/// A name that a lockfile's `"<name>@<resolution>"` keys can round-trip.
pub fn is_safe_lockfile_package_name(name: &str) -> bool {
    is_safe_install_folder_name(name) && !name[1..].contains('@')
}

/// For a package whose package.json has no name: `https://host/foo.tgz?token=x` gives `foo`.
pub fn fallback_package_name(location: &str) -> &str {
    let path = match location.find(['?', '#']) {
        Some(end) if location.contains("://") => &location[..end],
        _ => location,
    };
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = base.strip_suffix(".tgz").unwrap_or(base);
    let name = name.strip_suffix(".tar.gz").unwrap_or(name);
    if is_safe_lockfile_package_name(name) { name } else { "unnamed-package" }
}

/// `1.2.3+build` gives `1.2.3`.
pub fn without_build_tag(version: &str) -> &str {
    version.split_once('+').map_or(version, |(v, _)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names() {
        assert!(PackageName::parse("react").is_ok());
        assert!(PackageName::parse("@types/node").is_ok());
        assert!(PackageName::parse("JSONStream").is_ok());
        assert!(PackageName::parse("@~3/svelte_mount").is_ok());
        assert!(PackageName::parse("$").is_ok());
        assert_eq!(PackageName::parse(""), Err(NameError::Empty));
        assert!(PackageName::parse(&"a".repeat(215)).is_err());
        for bad in ["@types", "@types/", "@/x", "a/b", ".foo", "_foo", "foo bar", "fo~o", "@a/b/c", "@a/b~"] {
            assert!(PackageName::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scope_helpers() {
        let n = PackageName::parse("@types/node").unwrap();
        assert!(n.is_scoped());
        assert_eq!(n.scope(), Some("@types"));
        assert_eq!(n.unscoped(), "node");
        let n = PackageName::parse("react").unwrap();
        assert_eq!(n.scope(), None);
        assert_eq!(n.unscoped(), "react");
        assert_eq!(unscoped_package_name("@scope"), "@scope");
    }

    #[test]
    fn scoped_check() {
        assert!(is_scoped_package_name("@a/b").unwrap());
        assert!(!is_scoped_package_name("a").unwrap());
        assert!(is_scoped_package_name("@a").is_err());
        assert!(is_scoped_package_name("@/a").is_err());
        assert!(is_scoped_package_name("@a/").is_err());
        assert!(is_scoped_package_name("").is_err());
    }

    #[test]
    fn new_package_rules() {
        assert!(validate_new_package_name("some-package").is_ok());
        assert!(validate_new_package_name("@npm/thingy").is_ok());
        assert!(validate_new_package_name("example.com").is_ok());
        assert_eq!(validate_new_package_name("Hello").unwrap_err(), vec![NameProblem::Uppercase]);
        let has = |n: &str, p: NameProblem| validate_new_package_name(n).unwrap_err().contains(&p);
        assert!(has(" leading", NameProblem::SurroundingSpaces));
        assert!(has(".start", NameProblem::LeadingPeriod));
        assert!(has("_start", NameProblem::LeadingUnderscore));
        assert!(has("node_modules", NameProblem::Blacklisted("node_modules".into())));
        assert!(has("fs", NameProblem::CoreModule("fs".into())));
        assert!(has("crazy!", NameProblem::SpecialCharacters));
        assert!(has("a/b", NameProblem::NotUrlFriendly));
        assert!(has("", NameProblem::Empty));
        assert!(has(&"a".repeat(215), NameProblem::TooLong));
    }

    #[test]
    fn split_name_version() {
        assert_eq!(split_name_and_maybe_version("foo@1.1.1"), ("foo", Some("1.1.1")));
        assert_eq!(split_name_and_maybe_version("@foo/bar@1.1.1"), ("@foo/bar", Some("1.1.1")));
        assert_eq!(split_name_and_maybe_version("@foo/bar"), ("@foo/bar", None));
        assert_eq!(split_name_and_maybe_version("foo"), ("foo", None));
        assert_eq!(split_name_and_maybe_version("foo@"), ("foo", None));
        assert_eq!(split_name_and_maybe_version("foo@npm:bar@2"), ("foo", Some("npm:bar@2")));
        assert_eq!(split_name_and_version_or_latest("foo"), ("foo", "latest"));
    }

    #[test]
    fn folder_safety() {
        assert!(is_safe_install_folder_name("foo"));
        assert!(is_safe_install_folder_name("@a/b"));
        for bad in ["", ".", "..", "a/..", "a\\b", "c:x", "a\nb", "a/", "/a", "a\u{85}"] {
            assert!(!is_safe_install_folder_name(bad), "{bad:?}");
        }
        assert!(contains_control_character("a\u{1b}[0m"));
        assert!(contains_control_character("a\u{9b}"));
        assert!(!contains_control_character("héllo"));
        assert!(is_safe_lockfile_package_name("@a/b"));
        assert!(!is_safe_lockfile_package_name("a@b"));
    }

    #[test]
    fn fallback_names() {
        assert_eq!(fallback_package_name("https://host/foo.tgz?token=x"), "foo");
        assert_eq!(fallback_package_name("./pkgs/bar.tar.gz"), "bar");
        assert_eq!(fallback_package_name("./pkgs/baz"), "baz");
        assert_eq!(fallback_package_name("./pkgs/.."), "unnamed-package");
        assert_eq!(without_build_tag("1.2.3+abc"), "1.2.3");
    }

    #[test]
    fn serde_roundtrip() {
        let n: PackageName = serde_json::from_str("\"@a/b\"").unwrap();
        assert_eq!(serde_json::to_string(&n).unwrap(), "\"@a/b\"");
        assert!(serde_json::from_str::<PackageName>("\"@a\"").is_err());
    }
}
