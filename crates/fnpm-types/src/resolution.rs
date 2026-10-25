//! Where a resolved package comes from (bun's `resolution.rs` `TaggedValue`).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::integrity::Integrity;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Resolution {
    /// The project root package.json.
    Root,
    /// A registry package.
    Npm {
        version: String,
        /// `None` when the tarball is at the registry's default location
        /// (`<registry>/<name>/-/<unscoped>-<version>.tgz`), which keeps lockfiles portable
        /// across registry mirrors.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tarball_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integrity: Option<Integrity>,
    },
    /// A git repository fetched with `git`.
    Git {
        /// Repository URL without `git+` and `#`.
        url: String,
        /// What was asked for (`main`, `v1`, `semver:^1`), `""` for the default branch.
        committish: String,
        /// The full commit SHA it resolved to.
        resolved_commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integrity: Option<Integrity>,
    },
    /// A GitHub repository fetched as a tarball.
    Github {
        owner: String,
        repo: String,
        committish: String,
        resolved_commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integrity: Option<Integrity>,
    },
    RemoteTarball {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integrity: Option<Integrity>,
    },
    /// A `.tgz` on disk, relative to the project root, `/`-separated. Root
    /// declarations preserve their requested spelling for lockfile and patch labels.
    LocalTarball {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integrity: Option<Integrity>,
    },
    /// `file:dir`: copied into node_modules like a package. Relative to the project root.
    Folder { path: String },
    /// `link:dir`: symlinked. Relative to the project root, or a `fnpm link` name.
    Symlink { path: String },
    /// A workspace member. Relative to the project root, `/`-separated.
    Workspace { path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResolutionTag {
    Root,
    Npm,
    Git,
    Github,
    RemoteTarball,
    LocalTarball,
    Folder,
    Symlink,
    Workspace,
}

impl Resolution {
    pub fn npm(version: impl Into<String>, integrity: Option<Integrity>) -> Self {
        Self::Npm {
            version: version.into(),
            tarball_url: None,
            integrity,
        }
    }

    /// The label used in patchedDependencies (local tarballs omit the file: prefix).
    pub fn patch_label(&self) -> String {
        let (prefix, owner, repo, requested, resolved) = match self {
            Self::LocalTarball { path, .. } | Self::Folder { path } => return path.clone(),
            Self::Git {
                url,
                committish,
                resolved_commit,
                ..
            } => ("git+", "", url.as_str(), committish, resolved_commit),
            Self::Github {
                owner,
                repo,
                committish,
                resolved_commit,
                ..
            } => (
                "github:",
                owner.as_str(),
                repo.as_str(),
                committish,
                resolved_commit,
            ),
            other => return other.to_string(),
        };
        let mut label = prefix.to_owned();
        if !owner.is_empty() {
            label.push_str(owner);
            label.push('/');
        } else if crate::spec::is_scp_like_path(repo) {
            label.push_str("ssh://");
        }
        label.push_str(repo);
        let commit = if resolved.is_empty() {
            requested.as_str()
        } else {
            resolved
                .rsplit_once('-')
                .map_or(resolved.as_str(), |(_, c)| c)
        };
        if !commit.is_empty() {
            label.push('#');
            label.push_str(commit);
        }
        label
    }

    pub fn tag(&self) -> ResolutionTag {
        match self {
            Self::Root => ResolutionTag::Root,
            Self::Npm { .. } => ResolutionTag::Npm,
            Self::Git { .. } => ResolutionTag::Git,
            Self::Github { .. } => ResolutionTag::Github,
            Self::RemoteTarball { .. } => ResolutionTag::RemoteTarball,
            Self::LocalTarball { .. } => ResolutionTag::LocalTarball,
            Self::Folder { .. } => ResolutionTag::Folder,
            Self::Symlink { .. } => ResolutionTag::Symlink,
            Self::Workspace { .. } => ResolutionTag::Workspace,
        }
    }

    pub fn integrity(&self) -> Option<&Integrity> {
        match self {
            Self::Npm { integrity, .. }
            | Self::Git { integrity, .. }
            | Self::Github { integrity, .. }
            | Self::RemoteTarball { integrity, .. }
            | Self::LocalTarball { integrity, .. } => integrity.as_ref(),
            _ => None,
        }
    }

    pub fn set_integrity(&mut self, value: Integrity) {
        match self {
            Self::Npm { integrity, .. }
            | Self::Git { integrity, .. }
            | Self::Github { integrity, .. }
            | Self::RemoteTarball { integrity, .. }
            | Self::LocalTarball { integrity, .. } => *integrity = Some(value),
            _ => {}
        }
    }

    /// The registry version for [`Resolution::Npm`].
    pub fn npm_version(&self) -> Option<&str> {
        match self {
            Self::Npm { version, .. } => Some(version),
            _ => None,
        }
    }

    /// Fetched over the network or from the store (as opposed to linked from the project).
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            Self::Npm { .. } | Self::Git { .. } | Self::Github { .. } | Self::RemoteTarball { .. }
        )
    }

    /// Content is addressed by integrity and can live in the global store.
    pub fn is_storable(&self) -> bool {
        self.is_remote() || matches!(self, Self::LocalTarball { .. })
    }

    /// Lives in the project (root, workspace, folder, link): never copied into the store.
    pub fn is_local_tree(&self) -> bool {
        matches!(
            self,
            Self::Root | Self::Workspace { .. } | Self::Folder { .. } | Self::Symlink { .. }
        )
    }
}

/// The `name@<this>` text bun.lock and `fnpm why` print: `1.2.3`, `file:../x`, `link:../x`,
/// `workspace:packages/a`, `github:user/repo#<sha>`, `git+<url>#<sha>`, `root:`.
impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => f.write_str("root:"),
            Self::Npm { version, .. } => f.write_str(version),
            Self::Git {
                url,
                resolved_commit,
                ..
            } => write!(f, "git+{url}#{resolved_commit}"),
            Self::Github {
                owner,
                repo,
                resolved_commit,
                ..
            } => write!(f, "github:{owner}/{repo}#{resolved_commit}"),
            Self::RemoteTarball { url, .. } => f.write_str(url),
            Self::LocalTarball { path, .. } | Self::Folder { path } => write!(f, "file:{path}"),
            Self::Symlink { path } => write!(f, "link:{path}"),
            Self::Workspace { path } => write!(f, "workspace:{path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::HashAlgorithm;

    #[test]
    fn display_and_flags() {
        let i = Integrity::compute(HashAlgorithm::Sha512, b"x");
        let mut r = Resolution::npm("1.2.3", None);
        assert_eq!(r.to_string(), "1.2.3");
        assert!(r.is_remote() && r.is_storable() && !r.is_local_tree());
        r.set_integrity(i);
        assert_eq!(r.integrity(), Some(&i));
        assert_eq!(
            Resolution::Folder {
                path: "../x".into()
            }
            .to_string(),
            "file:../x"
        );
        assert_eq!(
            Resolution::Workspace {
                path: "packages/a".into()
            }
            .to_string(),
            "workspace:packages/a"
        );
        let gh = Resolution::Github {
            owner: "u".into(),
            repo: "r".into(),
            committish: "main".into(),
            resolved_commit: "abc".into(),
            integrity: None,
        };
        assert_eq!(gh.to_string(), "github:u/r#abc");
        assert_eq!(gh.patch_label(), "github:u/r#abc");
        let tgz = Resolution::LocalTarball {
            path: "./pkg.tgz".into(),
            integrity: None,
        };
        assert_eq!(tgz.patch_label(), "./pkg.tgz");
        let git = Resolution::Git {
            url: "git@example.invalid:repo.git".into(),
            committish: "main".into(),
            resolved_commit: "abc123".into(),
            integrity: None,
        };
        assert_eq!(
            git.patch_label(),
            "git+ssh://git@example.invalid:repo.git#abc123"
        );
        assert_eq!(gh.tag(), ResolutionTag::Github);
        assert!(Resolution::Root.is_local_tree());
        assert!(
            Resolution::LocalTarball {
                path: "x.tgz".into(),
                integrity: None
            }
            .is_storable()
        );
    }

    #[test]
    fn serde_shape() {
        let r = Resolution::npm("1.0.0", None);
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(j, r#"{"type":"npm","version":"1.0.0"}"#);
        assert_eq!(serde_json::from_str::<Resolution>(&j).unwrap(), r);
    }
}
