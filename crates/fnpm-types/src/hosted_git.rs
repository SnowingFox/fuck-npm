//! Port of `bun/src/install/hosted_git_info.rs`, which mirrors npm's `hosted-git-info` 9.0.0
//! bug for bug. Parses git URLs and shortcuts (`github:user/repo`, `user/repo`,
//! `git@github.com:user/repo.git`, ...) into the host provider, user, project and committish.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HostedGitError {
    /// A part of the URL could not be percent-decoded.
    #[error("InvalidURL")]
    InvalidUrl,
}

/// How a parsed URL is printed by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Representation {
    /// `foo/bar`, `github:foo/bar`
    Shortcut,
    /// `git+ssh://git@domain/user/project.git#committish`
    Sshurl,
    /// `https://domain/user/project.git#committish`
    Https,
    /// `git://domain/user/project.git#committish`
    Git,
    /// `http://domain/user/project.git#committish`
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostProvider {
    Bitbucket,
    Gist,
    Github,
    Gitlab,
    Sourcehut,
}

impl HostProvider {
    pub const ALL: [HostProvider; 5] = [Self::Bitbucket, Self::Gist, Self::Github, Self::Gitlab, Self::Sourcehut];

    pub fn type_str(self) -> &'static str {
        match self {
            Self::Bitbucket => "bitbucket",
            Self::Gist => "gist",
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Sourcehut => "sourcehut",
        }
    }

    pub fn domain(self) -> &'static str {
        match self {
            Self::Bitbucket => "bitbucket.org",
            Self::Gist => "gist.github.com",
            Self::Github => "github.com",
            Self::Gitlab => "gitlab.com",
            Self::Sourcehut => "git.sr.ht",
        }
    }

    /// `github` or `github:`.
    pub fn from_shortcut(shortcut: &str) -> Option<Self> {
        let s = shortcut.strip_suffix(':').unwrap_or(shortcut);
        Self::ALL.into_iter().find(|p| p.type_str() == s)
    }

    pub fn from_domain(domain: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.domain() == domain)
    }

    fn from_url_domain(url: &Url) -> Option<Self> {
        // bun's `URL::hostname()` includes the port, so `github.com:22` matches no provider.
        let host = url.host_str()?;
        let host: Cow<'_, str> = match url.port() {
            Some(port) => format!("{host}:{port}").into(),
            None => host.into(),
        };
        Self::from_domain(host.strip_prefix("www.").unwrap_or(&host))
    }

    fn from_url(url: &Url) -> Option<Self> {
        Self::from_shortcut(url.scheme()).or_else(|| Self::from_url_domain(url))
    }

    fn extract(self, url: &Url) -> Result<Option<Extracted>, HostedGitError> {
        match self {
            Self::Github => extract_github(url),
            Self::Bitbucket => user_project_committish(url, "get", false, false),
            Self::Gitlab => extract_gitlab(url),
            Self::Gist => user_project_committish(url, "raw", true, true),
            Self::Sourcehut => user_project_committish(url, "archive", false, true),
        }
    }
}

/// The parsed form of a hosted git URL. All parts are percent-decoded.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostedGitInfo {
    pub provider: HostProvider,
    pub user: Option<String>,
    pub project: String,
    pub committish: Option<String>,
    pub default_representation: Representation,
}

struct Extracted {
    user: Option<String>,
    project: String,
    committish: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WellDefinedProtocol {
    Git,
    GitPlusFile,
    GitPlusFtp,
    GitPlusHttp,
    GitPlusHttps,
    GitPlusRsync,
    GitPlusSsh,
    Http,
    Https,
    Ssh,
    Github,
    Bitbucket,
    Gitlab,
    Gist,
    Sourcehut,
}

impl WellDefinedProtocol {
    /// Looks up `proto:` (with the colon).
    fn from_str_with_colon(s: &str) -> Option<Self> {
        Some(match s.strip_suffix(':')? {
            "bitbucket" => Self::Bitbucket,
            "gist" => Self::Gist,
            "git+file" => Self::GitPlusFile,
            "git+ftp" => Self::GitPlusFtp,
            "git+http" => Self::GitPlusHttp,
            "git+https" => Self::GitPlusHttps,
            "git+rsync" => Self::GitPlusRsync,
            "git+ssh" => Self::GitPlusSsh,
            "git" => Self::Git,
            "github" => Self::Github,
            "gitlab" => Self::Gitlab,
            "http" => Self::Http,
            "https" => Self::Https,
            "sourcehut" => Self::Sourcehut,
            "ssh" => Self::Ssh,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Bitbucket => "bitbucket",
            Self::Gist => "gist",
            Self::GitPlusFile => "git+file",
            Self::GitPlusFtp => "git+ftp",
            Self::GitPlusHttp => "git+http",
            Self::GitPlusHttps => "git+https",
            Self::GitPlusRsync => "git+rsync",
            Self::GitPlusSsh => "git+ssh",
            Self::Git => "git",
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Http => "http",
            Self::Https => "https",
            Self::Sourcehut => "sourcehut",
            Self::Ssh => "ssh",
        }
    }

    fn is_shortcut(self) -> bool {
        matches!(self, Self::Github | Self::Bitbucket | Self::Gitlab | Self::Gist | Self::Sourcehut)
    }

    /// `github:foo/bar` but `git+ssh://foo/bar`.
    fn concat_token(self) -> &'static str {
        if self.is_shortcut() { "" } else { "//" }
    }

    fn default_representation(self) -> Representation {
        match self {
            Self::GitPlusSsh | Self::Ssh | Self::GitPlusHttp => Representation::Sshurl,
            Self::GitPlusHttps | Self::Https => Representation::Https,
            Self::GitPlusFile | Self::GitPlusFtp | Self::GitPlusRsync | Self::Git => Representation::Git,
            Self::Http => Representation::Http,
            _ => Representation::Shortcut,
        }
    }

    fn host_provider(self) -> Option<HostProvider> {
        match self {
            Self::Github => Some(HostProvider::Github),
            Self::Bitbucket => Some(HostProvider::Bitbucket),
            Self::Gitlab => Some(HostProvider::Gitlab),
            Self::Gist => Some(HostProvider::Gist),
            Self::Sourcehut => Some(HostProvider::Sourcehut),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum UrlProtocol<'a> {
    WellFormed(WellDefinedProtocol),
    /// Includes the `:` but not the `//`.
    Custom(&'a str),
    Unknown,
}

impl UrlProtocol<'_> {
    fn default_representation(self) -> Representation {
        match self {
            Self::WellFormed(p) => p.default_representation(),
            _ => Representation::Sshurl,
        }
    }
}

struct ProtocolPair<'a> {
    url: Cow<'a, str>,
    protocol: UrlProtocol<'a>,
}

impl ProtocolPair<'_> {
    fn to_url(&self) -> Option<Url> {
        let s = match self.protocol {
            UrlProtocol::Unknown => format!("git+ssh://{}", self.url),
            UrlProtocol::Custom(p) => format!("{p}//{}", self.url),
            UrlProtocol::WellFormed(p) => format!("{}:{}{}", p.as_str(), p.concat_token(), self.url),
        };
        Url::parse(&s).ok()
    }
}

/// hosted-git-info's `correctProtocol`.
fn normalize_protocol(s: &str) -> ProtocolPair<'_> {
    let first_colon = s.find(':');
    let proto = first_colon.map_or("", |i| &s[..=i]);
    if let Some(p) = WellDefinedProtocol::from_str_with_colon(proto) {
        let post = &s[proto.len()..];
        let post = post.strip_prefix("//").unwrap_or(post);
        return ProtocolPair { url: post.into(), protocol: UrlProtocol::WellFormed(p) };
    }
    if let Some(at) = s.find('@') {
        let protocol = match first_colon {
            // `user:pass@host`
            Some(colon) if at > colon => UrlProtocol::WellFormed(WellDefinedProtocol::GitPlusSsh),
            // `user@host:path`
            Some(_) => UrlProtocol::Unknown,
            // `user@host`
            None => UrlProtocol::WellFormed(WellDefinedProtocol::GitPlusSsh),
        };
        return ProtocolPair { url: s.into(), protocol };
    }
    if let Some(dup) = s.find("//")
        && Some(dup) == first_colon.map(|c| c + 1)
    {
        return ProtocolPair { url: s[dup + 2..].into(), protocol: UrlProtocol::Custom(&s[..dup]) };
    }
    if let Some(colon) = first_colon {
        return ProtocolPair { url: s[colon + 1..].into(), protocol: UrlProtocol::Custom(&s[..=colon]) };
    }
    ProtocolPair { url: s.into(), protocol: UrlProtocol::Unknown }
}

/// Last index of `needle` before the first `stop` (or the end).
fn last_index_before(s: &str, needle: char, stop: char) -> Option<usize> {
    let end = s.find(stop).unwrap_or(s.len());
    s[..end].rfind(needle)
}

/// Turns an scp-style `host:path` into `host/path`.
fn correct_url<'a>(pair: &ProtocolPair<'a>) -> ProtocolPair<'a> {
    let at = last_index_before(&pair.url, '@', '#');
    let col = last_index_before(&pair.url, ':', '#');
    if let Some(c) = col
        && at.is_none_or(|a| c > a)
    {
        let mut owned = pair.url.to_string();
        owned.replace_range(c..=c, "/");
        return ProtocolPair { url: owned.into(), protocol: UrlProtocol::WellFormed(WellDefinedProtocol::GitPlusSsh) };
    }
    let protocol = if col.is_none() && matches!(pair.protocol, UrlProtocol::Unknown) {
        UrlProtocol::WellFormed(WellDefinedProtocol::GitPlusSsh)
    } else {
        pair.protocol
    };
    ProtocolPair { url: pair.url.clone(), protocol }
}

fn parse_url(s: &str) -> Option<(Url, UrlProtocol<'_>)> {
    let pair = normalize_protocol(s);
    if let Some(url) = pair.to_url() {
        return Some((url, pair.protocol));
    }
    let corrected = correct_url(&pair);
    corrected.to_url().map(|u| (u, corrected.protocol))
}

/// Strict percent-decoding: a `%` must be followed by two hex digits and the result must be
/// UTF-8.
pub(crate) fn percent_decode_strict(input: &str) -> Result<String, HostedGitError> {
    let b = input.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() || !(b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit()) {
                return Err(HostedGitError::InvalidUrl);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    percent_encoding::percent_decode_str(input)
        .decode_utf8()
        .map(Cow::into_owned)
        .map_err(|_| HostedGitError::InvalidUrl)
}

fn build(user: Option<&str>, project: &str, committish: Option<&str>) -> Result<Extracted, HostedGitError> {
    Ok(Extracted {
        user: user.map(percent_decode_strict).transpose()?,
        project: percent_decode_strict(project)?,
        committish: committish.map(percent_decode_strict).transpose()?,
    })
}

fn path_of(url: &Url) -> &str {
    url.path().strip_prefix('/').unwrap_or(url.path())
}

fn fragment_of(url: &Url) -> Option<&str> {
    url.fragment().filter(|f| !f.is_empty())
}

/// `/user/project[.git][/aux]` with the committish in the fragment.
fn user_project_committish(
    url: &Url,
    reject_aux: &str,
    user_optional: bool,
    error_as_none: bool,
) -> Result<Option<Extracted>, HostedGitError> {
    let mut iter = path_of(url).split('/');
    let Some(mut user) = iter.next() else { return Ok(None) };
    let mut project = iter.next();
    if iter.next() == Some(reject_aux) {
        return Ok(None);
    }
    if user_optional && project.is_none_or(str::is_empty) {
        project = Some(user);
        user = "";
    }
    let Some(project) = project else { return Ok(None) };
    let project = project.strip_suffix(".git").unwrap_or(project);
    if project.is_empty() {
        return Ok(None);
    }
    let user = if user.is_empty() {
        if !user_optional {
            return Ok(None);
        }
        None
    } else {
        Some(user)
    };
    match build(user, project, fragment_of(url)) {
        Ok(r) => Ok(Some(r)),
        Err(_) if error_as_none => Ok(None),
        Err(e) => Err(e),
    }
}

fn extract_github(url: &Url) -> Result<Option<Extracted>, HostedGitError> {
    let mut iter = path_of(url).split('/');
    let Some(user) = iter.next() else { return Ok(None) };
    let Some(project) = iter.next() else { return Ok(None) };
    let type_part = iter.next();
    let committish_part = iter.next();
    let project = project.strip_suffix(".git").unwrap_or(project);
    if user.is_empty() || project.is_empty() {
        return Ok(None);
    }
    // Only `/tree/<committish>` is a deep GitHub URL that is understood.
    if type_part.is_some_and(|t| t != "tree") {
        return Ok(None);
    }
    let committish = if type_part.is_none() { fragment_of(url) } else { committish_part };
    build(Some(user), project, committish).map(Some)
}

fn extract_gitlab(url: &Url) -> Result<Option<Extracted>, HostedGitError> {
    let path = path_of(url);
    if path.contains("/-/") || path.contains("/archive.tar.gz") {
        return Ok(None);
    }
    let Some(end) = path.rfind('/') else { return Ok(None) };
    let user = &path[..end];
    let project = &path[end + 1..];
    let project = project.strip_suffix(".git").unwrap_or(project);
    if user.is_empty() || project.is_empty() {
        return Ok(None);
    }
    let user = percent_decode_strict(user)?;
    let project = percent_decode_strict(project)?;
    let committish = match fragment_of(url) {
        Some(c) => match percent_decode_strict(c) {
            Ok(c) => Some(c),
            Err(_) => return Ok(None),
        },
        None => None,
    };
    Ok(Some(Extracted { user: Some(user), project, committish }))
}

const PATH_SEGMENT: &percent_encoding::AsciiSet =
    &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// hosted-git-info's `isGitHubShorthand`: `user/repo`, `user/repo#ref`.
pub fn is_github_shorthand(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b[0] == b'.' || b[0] == b'/' {
        return false;
    }
    let mut pound: Option<usize> = None;
    let mut seen_slash = false;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b':' | b'@' => {
                if pound.is_none() {
                    return false;
                }
            }
            b'#' => pound = Some(i),
            b'/' => {
                if seen_slash && pound.is_none() {
                    return false;
                }
                seen_slash = true;
            }
            b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C if pound.is_none() => return false,
            _ => {}
        }
    }
    let not_end_slash = match pound {
        Some(p) => p == 0 || b[p - 1] != b'/',
        None => b[b.len() - 1] != b'/',
    };
    seen_slash && not_end_slash
}

impl HostedGitInfo {
    /// Parses a URL or shortcut. `Ok(None)` when it is not a hosted git URL; `Err` when it is one
    /// but a part cannot be decoded.
    pub fn from_url(git_url: &str) -> Result<Option<Self>, HostedGitError> {
        let prefixed;
        let input = if is_github_shorthand(git_url) {
            prefixed = format!("github:{git_url}");
            prefixed.as_str()
        } else {
            git_url
        };
        let Some((url, proto)) = parse_url(input) else { return Ok(None) };
        let provider = match proto {
            UrlProtocol::WellFormed(p) => p.host_provider().or_else(|| HostProvider::from_url_domain(&url)),
            UrlProtocol::Unknown => HostProvider::from_url_domain(&url),
            UrlProtocol::Custom(_) => HostProvider::from_url(&url),
        };
        let Some(provider) = provider else { return Ok(None) };

        let is_shortcut = matches!(proto, UrlProtocol::WellFormed(p) if p.is_shortcut());
        if !is_shortcut {
            let Some(e) = provider.extract(&url)? else { return Ok(None) };
            return Ok(Some(Self {
                provider,
                user: e.user,
                project: e.project,
                committish: e.committish,
                default_representation: proto.default_representation(),
            }));
        }

        // Shortcuts (`github:user/repo`): from-url.js lines 68-96.
        let mut pathname = url.path().strip_prefix('/').unwrap_or(url.path());
        if let Some(at) = pathname.find('@') {
            pathname = &pathname[at + 1..];
        }
        let (user, project) = match pathname.rfind('/') {
            Some(i) => {
                let u = &pathname[..i];
                ((!u.is_empty()).then_some(u), &pathname[i + 1..])
            }
            None => (None, pathname),
        };
        let project = project.strip_suffix(".git").unwrap_or(project);
        let e = build(user, project, fragment_of(&url))?;
        Ok(Some(Self {
            provider,
            user: e.user,
            project: e.project,
            committish: e.committish,
            default_representation: Representation::Shortcut,
        }))
    }

    fn hash_committish(&self) -> String {
        self.committish.as_ref().map_or(String::new(), |c| format!("#{c}"))
    }

    fn user_slash(&self) -> String {
        match (&self.user, self.provider) {
            (_, HostProvider::Gist) | (None, _) => String::new(),
            (Some(u), _) => format!("{u}/"),
        }
    }

    /// `github:user/project#committish`
    pub fn shortcut(&self) -> String {
        format!("{}:{}{}{}", self.provider.type_str(), self.user_slash(), self.project, self.hash_committish())
    }

    /// `git+ssh://git@github.com/user/project.git#committish`
    pub fn ssh_url(&self) -> String {
        format!(
            "git+ssh://git@{}/{}{}.git{}",
            self.provider.domain(),
            self.user_slash(),
            self.project,
            self.hash_committish()
        )
    }

    /// `git+https://github.com/user/project.git#committish`
    pub fn https_url(&self) -> String {
        format!(
            "git+https://{}/{}{}.git{}",
            self.provider.domain(),
            self.user_slash(),
            self.project,
            self.hash_committish()
        )
    }

    /// The URL `git clone` accepts for this repository, without the committish.
    pub fn clone_url(&self, ssh: bool) -> String {
        if ssh {
            format!("ssh://git@{}/{}{}.git", self.provider.domain(), self.user_slash(), self.project)
        } else {
            format!("https://{}/{}{}.git", self.provider.domain(), self.user_slash(), self.project)
        }
    }

    /// A tarball of the tree at the committish (or `HEAD`).
    pub fn tarball_url(&self) -> String {
        let c = self.committish.as_deref().unwrap_or("HEAD");
        let c = percent_encoding::utf8_percent_encode(c, PATH_SEGMENT).to_string();
        let d = self.provider.domain();
        let u = self.user.as_deref().unwrap_or("");
        let p = &self.project;
        match self.provider {
            HostProvider::Github => format!("https://codeload.github.com/{u}/{p}/tar.gz/{c}"),
            HostProvider::Gitlab => format!("https://{d}/{u}/{p}/repository/archive.tar.gz?ref={c}"),
            HostProvider::Bitbucket => format!("https://{d}/{u}/{p}/get/{c}.tar.gz"),
            HostProvider::Gist => format!("https://codeload.github.com/gist/{p}/tar.gz/{c}"),
            HostProvider::Sourcehut => format!("https://{d}/{u}/{p}/archive/{c}.tar.gz"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand() {
        assert!(is_github_shorthand("foo/bar"));
        assert!(is_github_shorthand("foo/bar#main"));
        assert!(is_github_shorthand("foo/bar#a/b:c@d"));
        for bad in ["", ".foo/bar", "/foo/bar", "foo", "foo/bar/", "foo/bar/baz", "user@foo/bar", "foo/ bar", "a:b/c", "foo/#x"] {
            assert!(!is_github_shorthand(bad), "{bad}");
        }
    }

    #[test]
    fn basic_forms() {
        let i = HostedGitInfo::from_url("github:foo/bar#main").unwrap().unwrap();
        assert_eq!(i.provider, HostProvider::Github);
        assert_eq!(i.user.as_deref(), Some("foo"));
        assert_eq!(i.project, "bar");
        assert_eq!(i.committish.as_deref(), Some("main"));
        assert_eq!(i.default_representation, Representation::Shortcut);

        let i = HostedGitInfo::from_url("foo/bar").unwrap().unwrap();
        assert_eq!(i.provider, HostProvider::Github);
        assert_eq!(i.shortcut(), "github:foo/bar");

        let i = HostedGitInfo::from_url("git@github.com:foo/bar.git").unwrap().unwrap();
        assert_eq!(i.project, "bar");
        assert_eq!(i.default_representation, Representation::Sshurl);

        let i = HostedGitInfo::from_url("https://github.com/foo/bar/tree/dev").unwrap().unwrap();
        assert_eq!(i.committish.as_deref(), Some("dev"));
        assert_eq!(i.default_representation, Representation::Https);

        let i = HostedGitInfo::from_url("https://gitlab.com/a/b/c.git#v1").unwrap().unwrap();
        assert_eq!(i.provider, HostProvider::Gitlab);
        assert_eq!(i.user.as_deref(), Some("a/b"));
        assert_eq!(i.project, "c");

        assert!(HostedGitInfo::from_url("https://google.com").unwrap().is_none());
        assert!(HostedGitInfo::from_url("git+ssh://git@nothosted.com/abc/def").unwrap().is_none());
        assert!(HostedGitInfo::from_url("https://github.com/foo/bar/issues").unwrap().is_none());
    }

    #[test]
    fn empty_shortcuts() {
        for (url, provider) in [
            ("github:", HostProvider::Github),
            ("gitlab:", HostProvider::Gitlab),
            ("bitbucket:", HostProvider::Bitbucket),
            ("gist:", HostProvider::Gist),
            ("sourcehut:", HostProvider::Sourcehut),
            ("github:.git", HostProvider::Github),
            ("github:/", HostProvider::Github),
            ("github://", HostProvider::Github),
            ("github:#", HostProvider::Github),
            ("github:?x", HostProvider::Github),
            ("github:@", HostProvider::Github),
            ("github:a@", HostProvider::Github),
            ("gitlab:.git", HostProvider::Gitlab),
        ] {
            let i = HostedGitInfo::from_url(url).unwrap().unwrap_or_else(|| panic!("{url}"));
            assert_eq!(i.provider, provider, "{url}");
            assert_eq!(i.user, None, "{url}");
            assert_eq!(i.project, "", "{url}");
            assert_eq!(i.committish, None, "{url}");
            assert_eq!(i.default_representation, Representation::Shortcut);
        }
    }

    #[test]
    fn extractor_edge_cases() {
        for url in [
            "https://bitbucket.org/foo/bar/get/archive.tar.gz",
            "https://gist.github.com/foo/feedbeef/raw/fix%2Fbug/",
            "https://git.sr.ht/~foo/bar/archive/HEAD.tar.gz",
            "https://bitbucket.org/foo",
            "https://git.sr.ht/~foo",
            "https://gist.github.com/",
            "https://bitbucket.org//bar",
            "https://git.sr.ht//bar",
            "https://gist.github.com/foo/bar%0N",
            "https://git.sr.ht/~foo/bar%0N",
            "https://gitlab.com/foo/-/something",
            "https://gitlab.com/foo/bar/repository/archive.tar.gz",
        ] {
            assert!(HostedGitInfo::from_url(url).unwrap().is_none(), "{url}");
        }
        for (url, p, user, project) in [
            ("https://bitbucket.org/foo/bar/raw", HostProvider::Bitbucket, Some("foo"), "bar"),
            ("https://bitbucket.org/foo/bar/archive", HostProvider::Bitbucket, Some("foo"), "bar"),
            ("https://gist.github.com/foo/feedbeef/get", HostProvider::Gist, Some("foo"), "feedbeef"),
            ("https://gist.github.com/foo/feedbeef/archive", HostProvider::Gist, Some("foo"), "feedbeef"),
            ("https://git.sr.ht/~foo/bar/get", HostProvider::Sourcehut, Some("~foo"), "bar"),
            ("https://git.sr.ht/~foo/bar/raw", HostProvider::Sourcehut, Some("~foo"), "bar"),
            ("https://gist.github.com/feedbeef", HostProvider::Gist, None, "feedbeef"),
            ("https://gist.github.com//feedbeef", HostProvider::Gist, None, "feedbeef"),
        ] {
            let i = HostedGitInfo::from_url(url).unwrap().unwrap_or_else(|| panic!("{url}"));
            assert_eq!((i.provider, i.user.as_deref(), i.project.as_str()), (p, user, project), "{url}");
        }
        assert_eq!(HostedGitInfo::from_url("https://bitbucket.org/foo/bar%0N"), Err(HostedGitError::InvalidUrl));
        assert!(HostedGitInfo::from_url("github:foo%0N/bar").is_err());
    }

    #[test]
    fn formatting() {
        let i = HostedGitInfo::from_url("github:foo/bar#v1.0.0").unwrap().unwrap();
        assert_eq!(i.ssh_url(), "git+ssh://git@github.com/foo/bar.git#v1.0.0");
        assert_eq!(i.https_url(), "git+https://github.com/foo/bar.git#v1.0.0");
        assert_eq!(i.clone_url(false), "https://github.com/foo/bar.git");
        assert_eq!(i.tarball_url(), "https://codeload.github.com/foo/bar/tar.gz/v1.0.0");
    }
}
