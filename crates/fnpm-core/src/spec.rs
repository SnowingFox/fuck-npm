//! `fnpm add` / `update` positionals and the specifiers written back to package.json
//! (bun `UpdateRequest.rs` and `PackageJSONEditor.rs`).

use fnpm_semver::{PinnedVersion, Version};
use fnpm_types::name::is_npm_package_name;
use fnpm_types::spec::{DependencySpec, SpecTag, TarballUri, infer_tag, is_tarball};

use crate::{Error, Result};

/// One parsed positional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Positional {
    /// The text as given (trimmed, `\\` turned into `/`).
    pub input: String,
    /// The dependency name when the input names it (`react`, `react@18`, `alias@npm:x`).
    /// `None` for git, GitHub, tarball and path specifiers: the name comes from the package.
    pub name: Option<String>,
    /// The specifier (`""` when only a name was given).
    pub literal: String,
    /// The parsed specifier (an empty literal parses as the `latest` tag).
    pub spec: DependencySpec,
}

impl Positional {
    /// Parses `input` like bun's `UpdateRequest::parse`.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim().replace('\\', "/");
        let unrecognised = || Error::usage(format!("unrecognised dependency format: {input}"));
        if input.is_empty() {
            return Err(unrecognised());
        }
        let mut name: Option<String> = None;
        let mut literal = input.clone();
        if !is_tarball(&input) && is_npm_package_name(&input) {
            name = Some(input.clone());
            literal.clear();
        } else if input.len() > 1
            && let Some(at) = input[1..].find('@')
        {
            let candidate = &input[..at + 1];
            if is_npm_package_name(candidate) {
                name = Some(candidate.to_owned());
                literal = input[at + 2..].to_owned();
            }
        }
        // scp-like `user@host:path`: `user` is not a package name.
        if let Some(n) = &name
            && !n.contains('/')
            && infer_tag(&literal) == SpecTag::Git
            && matches!(infer_tag(&input), SpecTag::Git | SpecTag::Github)
        {
            name = None;
            literal = input.clone();
        }
        let spec_literal = if literal.is_empty() {
            "latest"
        } else {
            &literal
        };
        let spec = DependencySpec::parse(name.as_deref().unwrap_or(""), spec_literal)
            .map_err(|_| unrecognised())?;
        if name.is_none()
            && matches!(
                spec,
                DependencySpec::Npm { .. } | DependencySpec::DistTag { .. }
            )
        {
            let aliased = matches!(
                &spec,
                DependencySpec::Npm { is_alias: true, .. }
                    | DependencySpec::DistTag { is_alias: true, .. }
            );
            if !aliased {
                return Err(unrecognised());
            }
        }
        Ok(Self {
            input,
            name,
            literal,
            spec,
        })
    }

    /// The name to request the dependency under before the package is known: the given
    /// name, else a guess from the specifier (repository or file name). The install corrects
    /// it once the package's own name is known.
    pub fn request_name(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        match &self.spec {
            DependencySpec::Npm { name, .. } | DependencySpec::DistTag { name, .. } => name.clone(),
            DependencySpec::Github(g) => g.repo.clone(),
            DependencySpec::Git(g) => g
                .hosted
                .as_ref()
                .map(|h| h.project.clone())
                .unwrap_or_else(|| last_segment(&g.url).trim_end_matches(".git").to_owned()),
            DependencySpec::Tarball { uri } => {
                let path = match uri {
                    TarballUri::Remote(u) => u.as_str(),
                    TarballUri::Local(p) => p.as_str(),
                };
                tarball_stem(last_segment(path))
            }
            DependencySpec::Folder { path } | DependencySpec::Symlink { path } => {
                last_segment(path.trim_end_matches('/')).to_owned()
            }
            _ => self.input.clone(),
        }
    }

    /// Re-spells a local positional, interpreted from `cwd`, relative to the package.json
    /// receiving it. Registry names and global `link:` registrations keep their spelling.
    pub(crate) fn relative_to(&self, cwd: &std::path::Path, target: &std::path::Path) -> Self {
        let path = match &self.spec {
            DependencySpec::Folder { path } => path,
            DependencySpec::Symlink { path } if fnpm_types::spec::is_link_path(path) => path,
            DependencySpec::Tarball {
                uri: TarballUri::Local(path),
            } => path,
            _ => return self.clone(),
        };
        if std::path::Path::new(path).is_absolute() || path.starts_with('~') {
            return self.clone();
        }
        let relative = fnpm_store::fsops::relative_path(target, &cwd.join(path));
        let relative = relative.to_string_lossy().replace('\\', "/");
        let relative = if relative.starts_with('.') {
            relative
        } else {
            format!("./{relative}")
        };
        let prefix = if self.literal.starts_with("file:") {
            "file:"
        } else if self.literal.starts_with("link:") {
            "link:"
        } else {
            ""
        };
        let literal = format!("{prefix}{relative}");
        let input = match &self.name {
            Some(name) => format!("{name}@{literal}"),
            None => literal,
        };
        Self::parse(&input).expect("rebasing a local path preserves its dependency format")
    }

    /// Whether the user named a version, range or tag (`react@18`, not `react`).
    pub fn has_version(&self) -> bool {
        !self.literal.is_empty()
    }

    /// The specifier to resolve (`latest` for a bare name).
    pub fn resolve_literal(&self) -> String {
        if self.literal.is_empty() {
            "latest".into()
        } else {
            self.literal.clone()
        }
    }
}

fn last_segment(s: &str) -> &str {
    let s = s.split(['#', '?']).next().unwrap_or(s);
    s.rsplit(['/', ':']).next().unwrap_or(s)
}

/// `left-pad-1.3.0.tgz` -> `left-pad`.
fn tarball_stem(file: &str) -> String {
    let stem = file
        .strip_suffix(".tgz")
        .or_else(|| file.strip_suffix(".tar.gz"))
        .unwrap_or(file);
    match stem.rfind('-') {
        Some(i) if stem[i + 1..].starts_with(|c: char| c.is_ascii_digit()) => stem[..i].to_owned(),
        _ => stem.to_owned(),
    }
}

/// The literal `fnpm add` writes for a positional that resolved to registry version
/// `resolved`: a dist-tag (or no version) and a bare `npm:` alias become `^<resolved>`
/// (`<resolved>` with `--exact`); with `--exact` a range that is not an exact version is
/// pinned too; anything else is kept as typed.
pub fn added_literal(p: &Positional, resolved: &str, exact: bool) -> String {
    let prefix = if exact { "" } else { "^" };
    let alias_target = split_npm_alias(&p.literal);
    let bare_alias = alias_target.is_some_and(|(_, v)| v.is_empty());
    let is_tag = p.literal.is_empty() || matches!(p.spec, DependencySpec::DistTag { .. });
    let range_to_pin = exact
        && matches!(&p.spec, DependencySpec::Npm { range, .. }
            if Version::parse(range.trim().trim_start_matches(['=', 'v'])).is_err());
    if is_tag || bare_alias || range_to_pin {
        let version = format!("{prefix}{resolved}");
        return match alias_target {
            Some((alias, _)) => format!("npm:{alias}@{version}"),
            None => version,
        };
    }
    p.literal.clone()
}

/// `npm:name@version` -> `(name, version)`; `npm:name` -> `(name, "")`.
pub fn split_npm_alias(literal: &str) -> Option<(&str, &str)> {
    let rest = literal.strip_prefix("npm:")?;
    let start = usize::from(rest.starts_with('@'));
    Some(match rest[start..].find('@') {
        Some(at) => (&rest[..start + at], &rest[start + at + 1..]),
        None => (rest, ""),
    })
}

fn split_digits(s: &str) -> Option<(&str, &str)> {
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    (n > 0).then(|| s.split_at(n))
}

fn all_zeros(s: &str) -> bool {
    !s.bytes().any(|b| (b'1'..=b'9').contains(&b))
}

/// `~1`, `^0` and `^0.0` take their ceiling from the omitted components.
fn short_range_keeps_ceiling(op: char, tail: &str) -> bool {
    let Some((major, rest)) = split_digits(tail) else {
        return false;
    };
    let Some((minor, rest)) = rest.strip_prefix('.').and_then(split_digits) else {
        return op == '^' && !all_zeros(major);
    };
    if rest.strip_prefix('.').and_then(split_digits).is_some() {
        return true;
    }
    op == '~' || !(all_zeros(major) && all_zeros(minor))
}

/// A plain `fnpm update` only rewrites `^x…`, `~x…` and exact versions; every other range is
/// kept as written (bun `keeps_declared_range`).
pub fn keeps_declared_range(literal: &str) -> bool {
    let rest = literal
        .trim()
        .trim_start_matches(['=', 'v', ' ', '\t', '\n', '\r']);
    if let Some(op) = rest.chars().next().filter(|c| *c == '^' || *c == '~') {
        let tail = rest[1..].trim();
        return tail.contains([' ', '\t', '|', '<', '>']) || !short_range_keeps_ceiling(op, tail);
    }
    let core = split_digits(rest)
        .and_then(|(_, r)| r.strip_prefix('.'))
        .and_then(split_digits)
        .and_then(|(_, r)| r.strip_prefix('.'))
        .and_then(split_digits);
    let Some((_, rest)) = core else {
        return true;
    };
    match rest.chars().next() {
        None => false,
        Some('-' | '+') => rest[1..].contains([' ', '\t', '|', '<', '>']),
        _ => true,
    }
}

/// The literal `fnpm update` writes for a declared `original` that now resolves to
/// `resolved`, in the declared pin style; `None` keeps the declaration (bun
/// `updated_version_literal`). `latest` (`--latest`) rewrites every range and tag.
pub fn updated_literal(
    original: &str,
    resolved: &str,
    exact: bool,
    latest: bool,
) -> Option<String> {
    let (alias, range) = match split_npm_alias(original) {
        Some((alias, range)) => (Some(alias), range),
        None => (None, original),
    };
    if !latest && keeps_declared_range(range) {
        return None;
    }
    let exact_prefix = if range.trim().starts_with('=') {
        "="
    } else {
        ""
    };
    let prefix = if exact {
        exact_prefix
    } else {
        match Version::which_version_is_pinned(range) {
            PinnedVersion::Patch => exact_prefix,
            PinnedVersion::Minor => "~",
            PinnedVersion::Major => "^",
        }
    };
    let v = format!("{prefix}{resolved}");
    Some(match alias {
        Some(a) => format!("npm:{a}@{v}"),
        None => v,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Positional {
        Positional::parse(s).unwrap()
    }

    #[test]
    fn names_and_literals() {
        let x = p("react");
        assert_eq!((x.name.as_deref(), x.literal.as_str()), (Some("react"), ""));
        let x = p("react@^18");
        assert_eq!(
            (x.name.as_deref(), x.literal.as_str()),
            (Some("react"), "^18")
        );
        let x = p("@types/node@20.1.0");
        assert_eq!(x.name.as_deref(), Some("@types/node"));
        assert_eq!(x.literal, "20.1.0");
        let x = p("my-react@npm:react@18");
        assert_eq!(x.name.as_deref(), Some("my-react"));
        assert_eq!(x.literal, "npm:react@18");
        let x = p("github:owner/some-repo#main");
        assert_eq!(x.name, None);
        assert_eq!(x.request_name(), "some-repo");
        let x = p("git@github.com:owner/repo.git");
        assert_eq!(x.name, None);
        let x = p("https://example.com/left-pad-1.3.0.tgz");
        assert_eq!(x.name, None);
        assert_eq!(x.request_name(), "left-pad");
        let x = p("./packages/lib");
        assert_eq!(x.name, None);
        assert_eq!(x.request_name(), "lib");
        assert!(Positional::parse("").is_err());
    }

    #[test]
    fn windows_local_positionals_normalize_path_separators() {
        for input in [
            r"C:\Users\runner\local-package-1.0.0.tgz",
            r"C:\Users\runner\local-package-1.0.0.tar.gz",
            r".\fixtures\local-package-1.0.0.tgz",
            r"file:C:\Users\runner\local-package-1.0.0.tgz",
        ] {
            let positional = p(input);
            assert_eq!(positional.name, None);
            assert_eq!(positional.request_name(), "local-package");
            assert!(!positional.input.contains('\\'));
            assert_eq!(positional.literal, input.replace('\\', "/"));
            assert!(matches!(
                positional.spec,
                DependencySpec::Tarball {
                    uri: TarballUri::Local(_)
                }
            ));
        }
        let positional = p(r".\fixtures\local-package\");
        assert_eq!(positional.request_name(), "local-package");
        assert!(matches!(positional.spec, DependencySpec::Folder { .. }));
        #[cfg(windows)]
        {
            let positional = p(r"C:\Users\runner\local-package\");
            assert_eq!(positional.request_name(), "local-package");
            assert!(matches!(positional.spec, DependencySpec::Folder { .. }));
        }
        let positional = p(r"alias@file:C:\Users\runner\local-package-1.0.0.tgz");
        assert_eq!(positional.name.as_deref(), Some("alias"));
        assert_eq!(positional.request_name(), "alias");
        assert_eq!(
            positional.literal,
            "file:C:/Users/runner/local-package-1.0.0.tgz"
        );
    }

    #[test]
    fn added_literals_follow_bun() {
        assert_eq!(added_literal(&p("react"), "18.3.1", false), "^18.3.1");
        assert_eq!(added_literal(&p("react"), "18.3.1", true), "18.3.1");
        assert_eq!(
            added_literal(&p("react@latest"), "18.3.1", false),
            "^18.3.1"
        );
        assert_eq!(added_literal(&p("react@next"), "19.0.0", false), "^19.0.0");
        assert_eq!(added_literal(&p("react@^18"), "18.3.1", false), "^18");
        assert_eq!(added_literal(&p("react@^18"), "18.3.1", true), "18.3.1");
        assert_eq!(added_literal(&p("react@18.2.0"), "18.2.0", true), "18.2.0");
        assert_eq!(
            added_literal(&p("r@npm:react"), "18.3.1", false),
            "npm:react@^18.3.1"
        );
        assert_eq!(
            added_literal(&p("r@npm:react@18"), "18.3.1", false),
            "npm:react@18"
        );
    }

    #[test]
    fn update_literals_follow_bun() {
        assert_eq!(
            updated_literal("^1.0.0", "1.4.0", false, false).as_deref(),
            Some("^1.4.0")
        );
        assert_eq!(
            updated_literal("~1.0.0", "1.0.5", false, false).as_deref(),
            Some("~1.0.5")
        );
        assert_eq!(
            updated_literal("1.0.0", "1.0.0", false, false).as_deref(),
            Some("1.0.0")
        );
        assert_eq!(updated_literal(">=1 <3", "2.0.0", false, false), None);
        // `^1` keeps its ceiling when refilled; `~1` would narrow.
        assert_eq!(
            updated_literal("^1", "1.4.0", false, false).as_deref(),
            Some("^1.4.0")
        );
        assert_eq!(updated_literal("~1", "1.4.0", false, false), None);
        assert_eq!(updated_literal("^0.0", "0.0.3", false, false), None);
        assert_eq!(updated_literal("latest", "1.4.0", false, false), None);
        assert_eq!(
            updated_literal("latest", "1.4.0", false, true).as_deref(),
            Some("^1.4.0")
        );
        assert_eq!(
            updated_literal("^1.0.0", "2.0.0", true, true).as_deref(),
            Some("2.0.0")
        );
        assert_eq!(
            updated_literal("npm:react@^17.0.0", "17.0.2", false, false).as_deref(),
            Some("npm:react@^17.0.2")
        );
    }
}
