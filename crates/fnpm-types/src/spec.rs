//! Dependency specifiers: the right-hand side of a package.json dependency.
//!
//! A port of `Tag::infer` and `parse_with_tag` from `bun/src/install/dependency.rs`, plus
//! `#semver:` and `::path:` committish parts (npm-package-arg) that bun does not read.

use serde::{Deserialize, Serialize};

use crate::behavior::Behavior;
use crate::hosted_git::{HostProvider, HostedGitInfo, Representation, is_github_shorthand};
use crate::loose_version;
use crate::name::{contains_control_character, is_npm_package_name, is_scoped_package_name};

/// The protocol a specifier uses, before its payload is parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecTag {
    /// A semver range, possibly behind `npm:` (`^1.2.3`, `npm:foo@^1`).
    Npm,
    /// `latest`, `beta`, `npm:foo@next`. The empty string is `latest`.
    DistTag,
    /// `https://host/x.tgz`, `./x.tgz`, `file:x.tgz`.
    Tarball,
    /// `./dir`, `../dir`, `/abs`, `~/dir`, `file:dir`.
    Folder,
    /// `link:dir`.
    Symlink,
    /// `workspace:*`, `workspace:^`, `workspace:~`, `workspace:1.2.3`.
    Workspace,
    /// `git+ssh://`, `git+https://`, `git://`, `ssh://`, `git+file://`, scp-like, non-github hosts.
    Git,
    /// `github:user/repo`, `user/repo`, `https://github.com/user/repo`.
    Github,
    /// `catalog:` or `catalog:name`.
    Catalog,
}

impl SpecTag {
    /// npm and dist-tag specs both go to the registry.
    pub fn is_npm(self) -> bool {
        matches!(self, Self::Npm | Self::DistTag)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("invalid or unsupported dependency \"{0}\"")]
    InvalidTarball(String),
    #[error("Unsupported protocol {0}")]
    UnsupportedProtocol(String),
    #[error("invalid GitHub dependency \"{0}\"")]
    InvalidGithub(String),
    #[error("invalid catalog dependency \"{0}\"")]
    InvalidCatalog(String),
}

/// Where a tarball lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "lowercase")]
pub enum TarballUri {
    /// `http://` or `https://` URL, exactly as written.
    Remote(String),
    /// A path relative to the declaring package (no `file:` prefix).
    Local(String),
}

/// The part after `#` in a git specifier, split on `::` as npm-package-arg does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Committish {
    /// A branch, tag or commit. `None` means the remote's default branch (`HEAD`).
    pub reference: Option<String>,
    /// `#semver:^1.2`: the highest tag that satisfies the range.
    pub semver: Option<String>,
    /// `#::path:packages/foo`: install a subdirectory of the repository.
    pub path: Option<String>,
}

impl Committish {
    pub fn parse(raw: &str) -> Self {
        let mut c = Self::default();
        if raw.is_empty() {
            return c;
        }
        for part in raw.split("::") {
            if let Some(range) = part.strip_prefix("semver:") {
                c.semver = Some(range.to_owned());
            } else if let Some(path) = part.strip_prefix("path:") {
                c.path = Some(path.to_owned());
            } else if !part.is_empty() && c.reference.is_none() {
                c.reference = Some(part.to_owned());
            }
        }
        c
    }

    pub fn is_empty(&self) -> bool {
        self.reference.is_none() && self.semver.is_none() && self.path.is_none()
    }

    /// The text after `#`, or `""`.
    pub fn to_raw(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(r) = &self.reference {
            parts.push(r.clone());
        }
        if let Some(s) = &self.semver {
            parts.push(format!("semver:{s}"));
        }
        if let Some(p) = &self.path {
            parts.push(format!("path:{p}"));
        }
        if self.reference.is_none() && self.semver.is_none() && self.path.is_some() {
            return format!("::{}", parts.join("::"));
        }
        parts.join("::")
    }
}

/// A git repository specifier (bun's `Repository` for `Tag::Git`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitSpec {
    /// The URL without a leading `git+` and without the `#committish`. `ssh://`, `https://`,
    /// `git://`, `file://`, or scp-like `git@host:path`.
    pub url: String,
    pub committish: Committish,
    /// Set when the URL is on a known host (gitlab, bitbucket, ...), so the fetcher can use a
    /// tarball endpoint instead of `git clone`.
    pub hosted: Option<HostedGitInfo>,
}

/// A GitHub specifier. Fetched as a tarball from the GitHub API.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GithubSpec {
    pub owner: String,
    pub repo: String,
    pub committish: Committish,
}

/// `workspace:` payloads.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum WorkspaceSpec {
    /// `workspace:*` (or `workspace:`). Published as the exact version.
    Any,
    /// `workspace:^`. Published as `^<version>`.
    Caret,
    /// `workspace:~`. Published as `~<version>`.
    Tilde,
    /// `workspace:^1.2.0`, `workspace:1.2.3`. Published as written.
    Range(String),
    /// `workspace:../pkg`. A path to the workspace folder.
    Path(String),
}

impl WorkspaceSpec {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "" | "*" => Self::Any,
            "^" => Self::Caret,
            "~" => Self::Tilde,
            _ if raw.starts_with("./") || raw.starts_with("../") || raw.starts_with('/') || raw == "." || raw == ".." => {
                Self::Path(raw.to_owned())
            }
            _ => Self::Range(raw.to_owned()),
        }
    }

    pub fn to_raw(&self) -> &str {
        match self {
            Self::Any => "*",
            Self::Caret => "^",
            Self::Tilde => "~",
            Self::Range(r) | Self::Path(r) => r,
        }
    }

    /// The specifier written to a published package.json for a workspace at `version`.
    pub fn publish_spec(&self, version: &str) -> String {
        match self {
            Self::Any | Self::Path(_) => version.to_owned(),
            Self::Caret => format!("^{version}"),
            Self::Tilde => format!("~{version}"),
            Self::Range(r) => r.clone(),
        }
    }
}

/// A parsed dependency specifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DependencySpec {
    /// A registry range. `name` is the registry name: the alias target for `npm:` aliases,
    /// else the dependency's own name. `range` is the semver text with a single leading `v`
    /// removed; empty means `*`.
    Npm { name: String, range: String, is_alias: bool },
    DistTag { name: String, tag: String, is_alias: bool },
    Tarball { uri: TarballUri },
    Folder { path: String },
    Symlink { path: String },
    Workspace { spec: WorkspaceSpec },
    Git(GitSpec),
    Github(GithubSpec),
    /// The catalog name; `""` is the default catalog.
    Catalog { catalog: String },
}

impl DependencySpec {
    /// Infers the tag and parses. `alias` is the dependency's key in package.json.
    pub fn parse(alias: &str, literal: &str) -> Result<Self, SpecError> {
        let lit = trim_literal(literal);
        Self::parse_with_tag(alias, lit, infer_tag(lit))
    }

    /// Parses with a known tag (e.g. a lockfile that stores the tag beside the literal).
    pub fn parse_with_tag(alias: &str, literal: &str, tag: SpecTag) -> Result<Self, SpecError> {
        let dep = trim_literal(literal);
        match tag {
            SpecTag::Npm => {
                let mut input = dep;
                let mut is_alias = false;
                let mut name = alias;
                if let Some(target) = dep.strip_prefix("npm:") {
                    let (n, version) = split_npm_alias(target);
                    if version.is_none() && is_range_after_npm_protocol(target) {
                        input = target;
                    } else {
                        is_alias = true;
                        name = n;
                        input = version.unwrap_or("");
                    }
                }
                if input.len() > 1 && input.starts_with('v') {
                    input = &input[1..];
                }
                Ok(Self::Npm { name: name.to_owned(), range: input.to_owned(), is_alias })
            }
            SpecTag::DistTag => {
                let (name, tag, is_alias) = match dep.strip_prefix("npm:") {
                    Some(target) => {
                        let (n, t) = split_npm_alias(target);
                        (n, t.unwrap_or(""), true)
                    }
                    None => (alias, dep, false),
                };
                let tag = if tag.is_empty() { "latest" } else { tag };
                Ok(Self::DistTag { name: name.to_owned(), tag: tag.to_owned(), is_alias })
            }
            SpecTag::Git => {
                let input = dep.strip_prefix("git+").unwrap_or(dep);
                let (url, committish) = match input.rfind('#') {
                    Some(i) => (&input[..i], &input[i + 1..]),
                    None => (input, ""),
                };
                let hosted = HostedGitInfo::from_url(dep).ok().flatten();
                Ok(Self::Git(GitSpec { url: url.to_owned(), committish: Committish::parse(committish), hosted }))
            }
            SpecTag::Github => {
                let info = match HostedGitInfo::from_url(dep) {
                    Ok(Some(info)) => info,
                    _ => return Err(SpecError::InvalidGithub(dep.to_owned())),
                };
                let (before_fragment, fragment) = dep.split_once('#').unwrap_or((dep, ""));
                let find = |part: &str| -> Result<String, SpecError> {
                    if part.is_empty() || before_fragment.contains(part) {
                        Ok(part.to_owned())
                    } else {
                        Err(SpecError::InvalidGithub(dep.to_owned()))
                    }
                };
                let committish = match info.committish.as_deref() {
                    None => String::new(),
                    Some(in_path) if fragment.is_empty() => find(in_path)?,
                    Some(_) if contains_control_character(fragment) => {
                        return Err(SpecError::InvalidGithub(dep.to_owned()));
                    }
                    Some(_) => fragment.to_owned(),
                };
                Ok(Self::Github(GithubSpec {
                    owner: find(info.user.as_deref().unwrap_or(""))?,
                    repo: find(&info.project)?,
                    committish: Committish::parse(&committish),
                }))
            }
            SpecTag::Tarball => {
                let uri = if is_remote_tarball(dep) {
                    TarballUri::Remote(dep.to_owned())
                } else if let Some(p) = dep.strip_prefix("file://") {
                    TarballUri::Local(p.to_owned())
                } else if let Some(p) = dep.strip_prefix("file:") {
                    TarballUri::Local(p.to_owned())
                } else if dep.contains("://") {
                    return Err(SpecError::InvalidTarball(dep.to_owned()));
                } else {
                    TarballUri::Local(dep.to_owned())
                };
                Ok(Self::Tarball { uri })
            }
            SpecTag::Folder => parse_folder(dep).map(|path| Self::Folder { path }),
            SpecTag::Symlink => {
                let path = dep.split_once(':').map_or(dep, |(_, rest)| rest);
                Ok(Self::Symlink { path: path.to_owned() })
            }
            SpecTag::Workspace => {
                let raw = dep.strip_prefix("workspace:").unwrap_or(dep);
                Ok(Self::Workspace { spec: WorkspaceSpec::parse(raw) })
            }
            SpecTag::Catalog => {
                let group = dep.strip_prefix("catalog:").ok_or_else(|| SpecError::InvalidCatalog(dep.to_owned()))?;
                Ok(Self::Catalog { catalog: group.trim().to_owned() })
            }
        }
    }

    pub fn tag(&self) -> SpecTag {
        match self {
            Self::Npm { .. } => SpecTag::Npm,
            Self::DistTag { .. } => SpecTag::DistTag,
            Self::Tarball { .. } => SpecTag::Tarball,
            Self::Folder { .. } => SpecTag::Folder,
            Self::Symlink { .. } => SpecTag::Symlink,
            Self::Workspace { .. } => SpecTag::Workspace,
            Self::Git(_) => SpecTag::Git,
            Self::Github(_) => SpecTag::Github,
            Self::Catalog { .. } => SpecTag::Catalog,
        }
    }

    /// The name to request from a registry: the alias target for `npm:` aliases, else `alias`.
    pub fn registry_name<'a>(&'a self, alias: &'a str) -> &'a str {
        match self {
            Self::Npm { name, .. } | Self::DistTag { name, .. } => name,
            _ => alias,
        }
    }

    pub fn is_alias(&self) -> bool {
        matches!(self, Self::Npm { is_alias: true, .. } | Self::DistTag { is_alias: true, .. })
    }
}

/// A dependency edge as declared in a package.json.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dependency {
    /// The key in package.json; the folder name under `node_modules`.
    pub name: String,
    /// The value exactly as written (trimmed of leading whitespace).
    pub literal: String,
    pub spec: DependencySpec,
    pub behavior: Behavior,
}

impl Dependency {
    pub fn parse(name: &str, literal: &str, behavior: Behavior) -> Result<Self, SpecError> {
        let lit = trim_literal(literal);
        Ok(Self { name: name.to_owned(), literal: lit.to_owned(), spec: DependencySpec::parse(name, lit)?, behavior })
    }

    /// The name of the package on the registry.
    pub fn registry_name(&self) -> &str {
        self.spec.registry_name(&self.name)
    }
}

/// A literal can have leading whitespace; classify and parse the rest.
pub fn trim_literal(literal: &str) -> &str {
    literal.trim_start_matches([' ', '\t', '\n', '\r'])
}

/// `.tgz`, `.tar.gz` or `.tar`, case-insensitive. Query strings are not considered.
pub fn is_tarball(dep: &str) -> bool {
    let lower_ends = |suffix: &str| {
        dep.len() >= suffix.len() && dep.as_bytes()[dep.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
    };
    lower_ends(".tgz") || lower_ends(".tar.gz") || lower_ends(".tar")
}

pub fn is_remote_tarball(dep: &str) -> bool {
    dep.starts_with("https://") || dep.starts_with("http://")
}

/// `host:path` or `user@host:path`, but not `proto://`.
pub fn is_scp_like_path(dep: &str) -> bool {
    let b = dep.as_bytes();
    if b.len() < 3 {
        return false;
    }
    let mut at: Option<usize> = None;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'@' => {
                if at.is_none() {
                    at = Some(i);
                }
            }
            b':' => {
                if b[i..].starts_with(b"://") {
                    return false;
                }
                return i > at.map_or(0, |a| a + 1);
            }
            b'/' => return at.is_some_and(|a| i > a + 1),
            _ => {}
        }
    }
    false
}

/// `<org>/<repo>/tarball/<ref>` after `github.com/`, or anything ending like a tarball.
pub fn is_github_tarball_path(path: &str) -> bool {
    is_tarball(path) || path.split('/').nth(2).is_some_and(|p| p == "tarball")
}

/// `2x`, `1beta` and `v2x` are tags, like in npm-package-arg.
fn is_dist_tag_with_leading_digit(dep: &str) -> bool {
    let b = dep.as_bytes();
    if b.len() <= 17 {
        let mut dots = 0;
        let mut after_digit = false;
        let plain = b.iter().all(|&c| match c {
            b'0'..=b'9' => {
                after_digit = true;
                true
            }
            b'.' if after_digit => {
                after_digit = false;
                dots += 1;
                true
            }
            _ => false,
        });
        if plain && after_digit && dots == 2 {
            return false;
        }
    }
    if dep.contains(['|', '+', ' ', '\t', '\n', '\r', '\x0B', '\x0C']) {
        return false;
    }
    let s = loose_version::scan(b);
    !s.valid && s.wildcard != loose_version::Wildcard::None
}

fn starts_with_windows_drive_letter(dep: &str) -> bool {
    let b = dep.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn hosted_tag(info: &HostedGitInfo) -> SpecTag {
    if info.provider == HostProvider::Github && info.default_representation == Representation::Shortcut {
        SpecTag::Github
    } else {
        SpecTag::Git
    }
}

/// Splits the text after `npm:`: `@scope/name@version` gives (`@scope/name`, `Some(version)`).
pub fn split_npm_alias(target: &str) -> (&str, Option<&str>) {
    let scope = usize::from(target.starts_with('@'));
    match target[scope..].find('@') {
        Some(at) => (&target[..scope + at], Some(&target[scope + at + 1..])),
        None => (target, None),
    }
}

/// `npm:^1.0.0` and `npm:1.0.0` are ranges for the dependency's own name (like Yarn);
/// `npm:x`, `npm:1`, `npm:1.x` are aliases, because those are valid package names.
fn is_range_after_npm_protocol(target: &str) -> bool {
    (target.contains(['^', '~', '>', '<', '=', '*', '|', ' ']) || loose_version::is_exact_version(target))
        && infer_tag(target) == SpecTag::Npm
}

/// `npm:^1.0.0` gives `^1.0.0`; an alias such as `npm:name@^1.0.0` is returned unchanged.
pub fn without_npm_protocol(literal: &str) -> &str {
    match literal.strip_prefix("npm:") {
        Some(target) if split_npm_alias(target).1.is_none() && is_range_after_npm_protocol(target) => target,
        _ => literal,
    }
}

/// `npm:`, `npm:@1.0.0`, `npm:@scope/@1`: aliases without a usable name.
pub fn nameless_npm_alias(literal: &str) -> Option<&str> {
    let (name, _) = split_npm_alias(trim_literal(literal).strip_prefix("npm:")?);
    is_scoped_package_name(name).is_err().then_some(name)
}

/// A `link:` target names a directory unless it is a package name (a `fnpm link` registration).
pub fn is_link_path(target: &str) -> bool {
    !target.is_empty() && !is_npm_package_name(target)
}

/// The `./` keeps [`is_link_path`] true when the directory is read back.
pub fn link_path_for_lockfile(folder: &str) -> std::borrow::Cow<'_, str> {
    if folder.is_empty() {
        return ".".into();
    }
    if matches!(folder, "." | "..")
        || folder.starts_with("./")
        || folder.starts_with("../")
        || folder.starts_with('/')
        || starts_with_windows_drive_letter(folder)
    {
        return folder.into();
    }
    format!("./{folder}").into()
}

#[cfg(windows)]
fn windows_abs_path_with_leading_slashes(dep: &str) -> Option<&str> {
    let trimmed = dep.trim_start_matches('/');
    (trimmed.len() < dep.len() && starts_with_windows_drive_letter(trimmed)).then_some(trimmed)
}

fn parse_folder(dep: &str) -> Result<String, SpecError> {
    let Some(protocol) = dep.find(':') else {
        return Ok(dep.to_owned());
    };
    if &dep[..protocol] == "file" {
        let after = &dep[protocol + 1..];
        // npm turns `file://../foo` into `file:../foo`.
        if let Some(rest) = after.strip_prefix('/') {
            let maybe_dot_dot = if let Some(rest2) = rest.strip_prefix('/') {
                rest2.strip_prefix('/').unwrap_or(rest2)
            } else {
                rest
            };
            if maybe_dot_dot.starts_with("..") {
                return Ok(maybe_dot_dot.to_owned());
            }
        }
        #[cfg(windows)]
        if let Some(p) = windows_abs_path_with_leading_slashes(after) {
            return Ok(p.to_owned());
        }
        return Ok(after.to_owned());
    }
    #[cfg(windows)]
    {
        if protocol == 1 && starts_with_windows_drive_letter(dep) {
            return Ok(dep.to_owned());
        }
        if let Some(p) = windows_abs_path_with_leading_slashes(dep) {
            return Ok(p.to_owned());
        }
    }
    Err(SpecError::UnsupportedProtocol(dep.to_owned()))
}

/// bun's `Tag::infer`.
pub fn infer_tag(dep: &str) -> SpecTag {
    let b = dep.as_bytes();
    if b.is_empty() {
        return SpecTag::DistTag;
    }
    if starts_with_windows_drive_letter(dep) && (b[2] == b'/' || (cfg!(windows) && b[2] == b'\\')) {
        return if is_tarball(dep) { SpecTag::Tarball } else { SpecTag::Folder };
    }
    match b[0] {
        b'=' | b'>' | b'<' | b'^' | b'*' | b'|' => return SpecTag::Npm,
        b'.' | b'/' => return if is_tarball(dep) { SpecTag::Tarball } else { SpecTag::Folder },
        b'~' => {
            if b.len() > 1 && b[1] == b'/' {
                return if is_tarball(dep) { SpecTag::Tarball } else { SpecTag::Folder };
            }
            return SpecTag::Npm;
        }
        b'0'..=b'9' => {
            if is_tarball(dep) {
                return SpecTag::Tarball;
            }
            if is_dist_tag_with_leading_digit(dep) {
                return SpecTag::DistTag;
            }
            return SpecTag::Npm;
        }
        b'f' => {
            if dep.starts_with("file:") {
                return if is_tarball(dep) { SpecTag::Tarball } else { SpecTag::Folder };
            }
        }
        b'c' => {
            if dep.starts_with("catalog:") {
                return SpecTag::Catalog;
            }
        }
        b'g' => {
            if let Some(mut url) = dep.strip_prefix("git")
                && url.len() > 2
            {
                match url.as_bytes()[0] {
                    b':' => {
                        if let Some(rest) = url.strip_prefix("://") {
                            if let Some(path) = rest.strip_prefix("github.com/")
                                && is_github_shorthand(path)
                            {
                                return SpecTag::Github;
                            }
                            if let Ok(Some(info)) = HostedGitInfo::from_url(dep) {
                                return hosted_tag(&info);
                            }
                            return SpecTag::Git;
                        }
                    }
                    b'+' => {
                        if url.starts_with("+ssh:") || url.starts_with("+file:") || url.starts_with("+git:") {
                            return SpecTag::Git;
                        }
                        if let Some(rest) = url.strip_prefix("+http") {
                            url = rest;
                            let advanced = if let Some(r) = url.strip_prefix("://") {
                                url = r;
                                true
                            } else if let Some(r) = url.strip_prefix("s://") {
                                url = r;
                                true
                            } else {
                                false
                            };
                            if advanced {
                                if let Some(path) = url.strip_prefix("github.com/")
                                    && is_github_shorthand(path)
                                {
                                    return SpecTag::Github;
                                }
                                if let Ok(Some(info)) = HostedGitInfo::from_url(dep) {
                                    return hosted_tag(&info);
                                }
                                return SpecTag::Git;
                            }
                        }
                    }
                    b'h' => {
                        if let Some(rest) = url.strip_prefix("hub:")
                            && is_github_shorthand(rest)
                        {
                            return SpecTag::Github;
                        }
                    }
                    _ => {}
                }
            }
        }
        b'h' => {
            if dep.starts_with("http:") || dep.starts_with("https:") {
                let mut url = &dep[4..];
                if let Some(r) = url.strip_prefix("://") {
                    url = r;
                } else if let Some(r) = url.strip_prefix("s://") {
                    url = r;
                }
                if let Some(path) = url.strip_prefix("github.com/") {
                    if is_github_tarball_path(path) {
                        return SpecTag::Tarball;
                    }
                    if is_github_shorthand(path) {
                        return SpecTag::Github;
                    }
                }
                if let Ok(Some(info)) = HostedGitInfo::from_url(dep) {
                    return hosted_tag(&info);
                }
                return SpecTag::Tarball;
            }
        }
        b's' => {
            if dep.starts_with("ssh:") {
                return SpecTag::Git;
            }
        }
        b'l' => {
            if dep.starts_with("link:") {
                return SpecTag::Symlink;
            }
        }
        b'n' => {
            if let Some(target) = dep.strip_prefix("npm:") {
                return match split_npm_alias(target) {
                    (_, Some(version)) => infer_tag(version),
                    (_, None) => SpecTag::Npm,
                };
            }
        }
        b'v' => {
            if is_tarball(dep) {
                return SpecTag::Tarball;
            }
            if is_github_shorthand(dep) {
                return SpecTag::Github;
            }
            if is_scp_like_path(dep) {
                return SpecTag::Git;
            }
            if b.len() == 1 {
                return SpecTag::DistTag;
            }
            return match b[1] {
                b'0'..=b'9' if !is_dist_tag_with_leading_digit(dep) => SpecTag::Npm,
                _ => SpecTag::DistTag,
            };
        }
        b'w' => {
            if dep.starts_with("workspace:") {
                return SpecTag::Workspace;
            }
        }
        b'x' | b'X' => {
            if b.len() == 1 || b[1] == b'.' {
                return SpecTag::Npm;
            }
        }
        b'p' if dep.starts_with("patch:") => return SpecTag::Npm,
        _ => {}
    }
    if is_tarball(dep) {
        return SpecTag::Tarball;
    }
    if is_github_shorthand(dep) {
        return SpecTag::Github;
    }
    if is_scp_like_path(dep) {
        if let Ok(Some(info)) = HostedGitInfo::from_url(dep) {
            return hosted_tag(&info);
        }
        return SpecTag::Git;
    }
    if !dep.contains('|') {
        return SpecTag::DistTag;
    }
    SpecTag::Npm
}
