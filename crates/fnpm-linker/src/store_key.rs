//! Names of isolated store entries (bun `isolated_install/Store.rs` `StoreKeyFormatter`,
//! `resolution.rs` `StorePathFormatter` / `StoreURLFormatter`, `repository.rs`).
//!
//! `node_modules/.fnpm/<name>@<resolution>[+<peer hash>]`: the name and resolution are
//! spelled as one path component (`/ \ : # ?` become `+`), URLs lose their userinfo and query
//! string (a hash of the full URL stands in for them), and a resolution longer than
//! [`MAX_RESOLUTION_LEN`] bytes is cut and suffixed with a hash of the whole text.

use fnpm_types::{HashAlgorithm, Integrity, Resolution};

/// Bounds the resolution part so that script cwds stay under Windows' `MAX_PATH`.
pub const MAX_RESOLUTION_LEN: usize = 80;
/// Bytes kept of a resolution that is too long: `<cut>+<16 hex>` is at most 80 bytes.
pub const CUT_RESOLUTION_LEN: usize = MAX_RESOLUTION_LEN - 1 - 16;

/// The 64-bit hash used in store names: the first 8 bytes (little endian) of
/// `SHA-256("0" ++ text)`. bun uses wyhash here; fnpm's conformance harness defines
/// `Bun.hash` this way, so names that embed a hash are reproducible from the tests.
pub fn name_hash(text: &[u8]) -> u64 {
    let mut buf = Vec::with_capacity(text.len() + 1);
    buf.push(b'0');
    buf.extend_from_slice(text);
    let digest = Integrity::compute(HashAlgorithm::Sha256, &buf);
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest.digest()[..8]);
    u64::from_le_bytes(first)
}

/// Appends `s` as one path component: `/`, `\`, `:`, `#` and `?` become `+`.
pub fn push_component(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '/' | '\\' | ':' | '#' | '?' => out.push('+'),
            c => out.push(c),
        }
    }
}

/// `(authority_start, host_start, query_start)` of a URL (RFC 3986 shapes, bun
/// `url_credential_offsets`).
fn url_credential_offsets(url: &str) -> (usize, usize, usize) {
    let b = url.as_bytes();
    let authority_start = match url.find(':') {
        Some(colon) if url[colon + 1..].starts_with("//") => colon + 3,
        _ => 0,
    };
    let authority_end = b[authority_start..]
        .iter()
        .position(|&c| c == b'/' || c == b'?' || c == b'#')
        .map_or(url.len(), |i| authority_start + i);
    let host_start = url[authority_start..authority_end]
        .rfind('@')
        .map_or(authority_start, |at| authority_start + at + 1);
    let query_start = url[host_start..]
        .find('?')
        .map_or(url.len(), |i| host_start + i);
    (authority_start, host_start, query_start)
}

/// A URL without userinfo and query, plus `+<hash of the full URL>` when either was there.
pub fn push_url(out: &mut String, url: &str) {
    let (authority_start, host_start, query_start) = url_credential_offsets(url);
    push_component(out, &url[..authority_start]);
    push_component(out, &url[host_start..query_start]);
    if host_start != authority_start || query_start != url.len() {
        out.push_str(&format!("+{:016x}", name_hash(url.as_bytes())));
    }
}

/// bun `Dependency::is_scp_like_path`: `user@host:path` without `://`.
pub fn is_scp_like_path(dependency: &str) -> bool {
    let b = dependency.as_bytes();
    if b.len() < 3 {
        return false;
    }
    let mut at_index: Option<usize> = None;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'@' => {
                if at_index.is_none() {
                    at_index = Some(i);
                }
            }
            b':' => {
                if b[i..].starts_with(b"://") {
                    return false;
                }
                return i > at_index.map_or(0, |a| a + 1);
            }
            b'/' => return at_index.is_some_and(|a| i > a + 1),
            _ => {}
        }
    }
    false
}

/// The resolution part, before the length bound.
fn resolution_text(resolution: &Resolution) -> String {
    let mut out = String::new();
    match resolution {
        Resolution::Root => out.push_str("root"),
        Resolution::Npm { version, .. } => out.push_str(version),
        Resolution::LocalTarball { path, .. } => push_component(&mut out, path),
        Resolution::RemoteTarball { url, .. } => push_url(&mut out, url),
        Resolution::Folder { path } => {
            out.push_str("file+");
            push_component(&mut out, path);
        }
        Resolution::Workspace { path } | Resolution::Symlink { path } => {
            push_component(&mut out, path)
        }
        Resolution::Git {
            url,
            committish,
            resolved_commit,
            ..
        } => {
            out.push_str("git+");
            if is_scp_like_path(url) {
                out.push_str("ssh++");
            }
            push_url(&mut out, url);
            push_commit(&mut out, resolved_commit, committish);
        }
        Resolution::Github {
            owner,
            repo,
            committish,
            resolved_commit,
            ..
        } => {
            out.push_str("github+");
            if !owner.is_empty() {
                push_component(&mut out, owner);
                out.push('+');
            }
            push_url(&mut out, repo);
            push_commit(&mut out, resolved_commit, committish);
        }
    }
    out
}

fn push_commit(out: &mut String, resolved: &str, committish: &str) {
    // bun stores GitHub's `resolved` as `<owner>-<repo>-<commit>`; keep the commit only.
    let commit = resolved.rsplit('-').next().unwrap_or(resolved);
    if !commit.is_empty() {
        out.push('+');
        push_component(out, commit);
    } else if !committish.is_empty() {
        out.push('+');
        push_component(out, committish);
    }
}

/// Appends the bounded resolution (bun `write_resolution`).
fn push_bounded(out: &mut String, text: &str) {
    if text.len() <= MAX_RESOLUTION_LEN {
        out.push_str(text);
        return;
    }
    let mut cut = CUT_RESOLUTION_LEN;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    out.push_str(&text[..cut]);
    out.push_str(&format!("+{:016x}", name_hash(text.as_bytes())));
}

/// `name@resolution` without a peer suffix. `project_basename` names the root when the root
/// package has no name.
pub fn store_key(name: &str, resolution: &Resolution, project_basename: &str) -> String {
    let mut out = String::with_capacity(name.len() + 16);
    if matches!(resolution, Resolution::Root) {
        if name.is_empty() {
            push_component(&mut out, project_basename);
        } else {
            push_component(&mut out, name);
            out.push_str("@root");
        }
        return out;
    }
    push_component(&mut out, name);
    out.push('@');
    push_bounded(&mut out, &resolution_text(resolution));
    out
}

/// Appends `+<16 hex>` for a non-zero peer hash.
pub fn with_peer_hash(mut key: String, peer_hash: u64) -> String {
    if peer_hash != 0 {
        key.push_str(&format!("+{peer_hash:016x}"));
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn npm(v: &str) -> Resolution {
        Resolution::npm(v, None)
    }

    #[test]
    fn npm_keys() {
        assert_eq!(store_key("react", &npm("18.3.1"), "p"), "react@18.3.1");
        assert_eq!(
            store_key("@types/node", &npm("20.1.0"), "p"),
            "@types+node@20.1.0"
        );
        assert_eq!(
            store_key("a", &npm("1.0.0+build.1"), "p"),
            "a@1.0.0+build.1"
        );
    }

    #[test]
    fn root_keys() {
        assert_eq!(store_key("app", &Resolution::Root, "dir"), "app@root");
        assert_eq!(store_key("", &Resolution::Root, "my-dir"), "my-dir");
    }

    #[test]
    fn folder_and_link_keys() {
        assert_eq!(
            store_key(
                "x",
                &Resolution::Folder {
                    path: "./pkgs/x".into()
                },
                "p"
            ),
            "x@file+.+pkgs+x"
        );
        assert_eq!(
            store_key(
                "w",
                &Resolution::Workspace {
                    path: "packages/w".into()
                },
                "p"
            ),
            "w@packages+w"
        );
        assert_eq!(
            store_key(
                "l",
                &Resolution::Symlink {
                    path: "../l".into()
                },
                "p"
            ),
            "l@..+l"
        );
        assert_eq!(
            store_key(
                "t",
                &Resolution::LocalTarball {
                    path: "vendor/t-1.0.0.tgz".into(),
                    integrity: None
                },
                "p"
            ),
            "t@vendor+t-1.0.0.tgz"
        );
    }

    #[test]
    fn url_keys_drop_credentials() {
        let plain = Resolution::RemoteTarball {
            url: "http://localhost:4873/pkg.tgz".into(),
            integrity: None,
        };
        assert_eq!(
            store_key("no-deps", &plain, "p"),
            "no-deps@http+++localhost+4873+pkg.tgz"
        );
        let url = "https://user:token@host/pkg.tgz?token=x";
        let creds = Resolution::RemoteTarball {
            url: url.into(),
            integrity: None,
        };
        assert_eq!(
            store_key("p", &creds, "p"),
            format!("p@https+++host+pkg.tgz+{:016x}", name_hash(url.as_bytes()))
        );
        let mut s = String::new();
        push_url(&mut s, "https://host/a.tgz#frag");
        assert_eq!(s, "https+++host+a.tgz+frag");
    }

    #[test]
    fn git_keys() {
        let g = Resolution::Git {
            url: "https://github.com/a/b.git".into(),
            committish: "main".into(),
            resolved_commit: "abc123".into(),
            integrity: None,
        };
        assert_eq!(
            store_key("b", &g, "p"),
            "b@git+https+++github.com+a+b.git+abc123"
        );
        let ssh = Resolution::Git {
            url: "git@github.com:a/b.git".into(),
            committish: String::new(),
            resolved_commit: "def".into(),
            integrity: None,
        };
        // The scp-style user is userinfo: dropped, and a hash of the URL stands in (bun).
        assert_eq!(
            store_key("b", &ssh, "p"),
            format!(
                "b@git+ssh++github.com+a+b.git+{:016x}+def",
                name_hash(b"git@github.com:a/b.git")
            )
        );
        let gh = Resolution::Github {
            owner: "owner".into(),
            repo: "repo".into(),
            committish: "v1".into(),
            resolved_commit: "owner-repo-0123abc".into(),
            integrity: None,
        };
        assert_eq!(
            store_key("repo", &gh, "p"),
            "repo@github+owner+repo+0123abc"
        );
        let unresolved = Resolution::Github {
            owner: "o".into(),
            repo: "r".into(),
            committish: "v1".into(),
            resolved_commit: String::new(),
            integrity: None,
        };
        assert_eq!(store_key("r", &unresolved, "p"), "r@github+o+r+v1");
    }

    #[test]
    fn scp_like() {
        assert!(is_scp_like_path("git@github.com:a/b"));
        assert!(is_scp_like_path("host:path"));
        assert!(!is_scp_like_path("https://github.com/a/b"));
        assert!(!is_scp_like_path("a/b"));
        assert!(!is_scp_like_path("h:"));
        assert!(!is_scp_like_path("git@:x"));
    }

    #[test]
    fn long_resolutions_are_cut_and_hashed() {
        let at_limit = "a".repeat(MAX_RESOLUTION_LEN - "file+".len());
        assert_eq!(
            store_key(
                "x",
                &Resolution::Folder {
                    path: at_limit.clone()
                },
                "p"
            ),
            format!("x@file+{at_limit}")
        );
        let past = format!("{at_limit}b");
        let key = store_key(
            "past-limit",
            &Resolution::Folder { path: past.clone() },
            "p",
        );
        let full = format!("file+{past}");
        assert_eq!(
            key,
            format!(
                "past-limit@{}+{:016x}",
                &full[..CUT_RESOLUTION_LEN],
                name_hash(full.as_bytes())
            )
        );
        assert_eq!(key.len() - "past-limit@".len(), MAX_RESOLUTION_LEN);
        // Two resolutions that differ only past the cut get different names.
        let k1 = store_key(
            "s",
            &Resolution::Folder {
                path: format!("{at_limit}1"),
            },
            "p",
        );
        let k2 = store_key(
            "s",
            &Resolution::Folder {
                path: format!("{at_limit}2"),
            },
            "p",
        );
        assert_ne!(k1, k2);
    }

    #[test]
    fn cut_respects_char_boundaries() {
        let folder = format!("x{}", "\u{e9}".repeat(40));
        let key = store_key(
            "non-ascii",
            &Resolution::Folder {
                path: folder.clone(),
            },
            "p",
        );
        let full = format!("file+{folder}");
        let cut = &key["non-ascii@".len()..key.len() - 17];
        assert_eq!(cut.len(), CUT_RESOLUTION_LEN - 1);
        assert_eq!(cut, format!("file+x{}", "\u{e9}".repeat(28)));
        assert!(key.ends_with(&format!("+{:016x}", name_hash(full.as_bytes()))));
    }

    #[test]
    fn peer_suffix() {
        assert_eq!(with_peer_hash("a@1.0.0".into(), 0), "a@1.0.0");
        assert_eq!(
            with_peer_hash("a@1.0.0".into(), 0xabc),
            "a@1.0.0+0000000000000abc"
        );
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(name_hash(b"x"), name_hash(b"x"));
        assert_ne!(name_hash(b"x"), name_hash(b"y"));
    }
}
