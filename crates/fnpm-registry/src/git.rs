//! `git` dependencies through the `git` executable (bun `install/git_runner.rs`,
//! `install/repository.rs`).
//!
//! Every git command runs with hooks disabled (`core.hooksPath=/dev/null`), submodules off,
//! `GIT_TERMINAL_PROMPT=0`, `GIT_NO_REPLACE_OBJECTS=1`, and, unless the user configured their
//! own, `GIT_ASKPASS=echo` and `GIT_SSH_COMMAND="ssh -oStrictHostKeyChecking=accept-new"`.
//! Clones are bare and cached per URL under `<cache_dir>/git/<16 hex>`; a URL is tried as https
//! first, then ssh.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use fnpm_semver::{Range, Version};
use fnpm_types::HostProvider;
use fnpm_types::spec::Committish;
use tokio::process::Command;

use crate::auth::redact_url;
use crate::semver_ext;
use crate::{Error, Result};

/// The longest resolved tag that may become part of a cache folder name.
pub const MAX_RESOLVED_TAG_LEN: usize = 256;

/// A commit SHA or GitHub tag that is one safe path component: no separators, no NUL, no
/// leading `-` (git would read an option), not `.`/`..`.
pub fn is_safe_resolved_tag(resolved: &str) -> bool {
    !resolved.is_empty()
        && resolved.len() <= MAX_RESOLVED_TAG_LEN
        && !resolved.starts_with('-')
        && resolved != "."
        && resolved != ".."
        && resolved
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn shorthand_domain(host: &str) -> Option<&'static str> {
    HostProvider::from_shortcut(host).map(HostProvider::domain)
}

/// `host:path` in an scp-like URL (`git@github.com:u/r`); an `ssh://` URL that parses is a real
/// URL whose colon precedes a port.
fn scp_like_path(url: &str) -> Option<&str> {
    let path = match url.strip_prefix("ssh://") {
        Some(_) if url::Url::parse(url).is_ok() => return None,
        Some(path) => path,
        None => url,
    };
    fnpm_types::spec::is_scp_like_path(path).then_some(path)
}

/// `host:path` to `<prefix>host/path`, with a shorthand host (`github`) as its domain.
fn scp_like_to_url(prefix: &str, url: &str) -> String {
    let bracket = match url.find(['[', ':']) {
        Some(at) if url.as_bytes()[at] == b'[' => url.find(']').unwrap_or(0),
        _ => 0,
    };
    let Some(colon) = url[bracket..].find(':').map(|i| i + bracket) else {
        return format!("{prefix}{url}");
    };
    let (host, path) = (&url[..colon], &url[colon + 1..]);
    format!("{prefix}{}/{path}", shorthand_domain(host).unwrap_or(host))
}

/// The ssh form of a git URL; `None` for an explicit `http(s)://` URL.
pub fn try_ssh(url: &str) -> Option<String> {
    if url.starts_with("http") {
        return None;
    }
    if url.starts_with("git@")
        && url
            .find([':', '/'])
            .is_some_and(|at| url.as_bytes()[at] == b':')
    {
        return Some(url.to_owned());
    }
    if let Some(path) = scp_like_path(url) {
        let host_end = path.find([':', '/']).unwrap_or(path.len());
        let has_user = !path.starts_with('-') && path[..host_end].contains('@');
        return Some(scp_like_to_url(
            if has_user { "ssh://" } else { "ssh://git@" },
            path,
        ));
    }
    url.starts_with("ssh://").then(|| url.to_owned())
}

/// The https form of a git URL, if it has one.
pub fn try_https(url: &str) -> Option<String> {
    if url.starts_with("http") {
        return Some(url.to_owned());
    }
    if let Some(path) = scp_like_path(url) {
        return Some(scp_like_to_url("https://", path));
    }
    let rest = url.strip_prefix("ssh://")?;
    let hosted = rest.strip_prefix("git@").filter(|h| {
        let end = h.find('/').unwrap_or(h.len());
        HostProvider::from_domain(&h[..end]).is_some()
    });
    Some(format!("https://{}", hosted.unwrap_or(rest)))
}

/// The URLs to try, in order: https first, then ssh; any other URL with a scheme as is.
pub fn clone_urls(url: &str) -> Vec<String> {
    let url = url.strip_prefix("git+").unwrap_or(url);
    let mut out = Vec::new();
    out.extend(try_https(url));
    out.extend(try_ssh(url));
    if out.is_empty() && url.contains("://") {
        out.push(url.to_owned());
    }
    out.dedup();
    out
}

/// The cache folder of a URL's bare clone: `<cache_dir>/git/<xxh3-64 hex>`.
pub fn clone_dir(cache_dir: &Path, url: &str) -> PathBuf {
    cache_dir.join("git").join(format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64(url.as_bytes())
    ))
}

struct GitEnv {
    git: Option<PathBuf>,
    vars: Vec<(String, String)>,
}

fn which_git() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &["git.exe", "git.cmd"]
    } else {
        &["git"]
    };
    std::env::split_paths(&path)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

fn git_env() -> &'static GitEnv {
    static ENV: OnceLock<GitEnv> = OnceLock::new();
    ENV.get_or_init(|| {
        let git = which_git();
        let mut vars = vec![
            ("GIT_NO_REPLACE_OBJECTS".to_owned(), "1".to_owned()),
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ];
        // Only set what the user's git configuration does not set itself.
        let configured = git.as_ref().and_then(|g| {
            std::process::Command::new(g)
                .args([
                    if cfg!(windows) {
                        "--git-dir=NUL"
                    } else {
                        "--git-dir=/dev/null"
                    },
                    "config",
                    "--name-only",
                    "--get-regexp",
                    r"^core\.(askpass|sshcommand)$",
                ])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase())
        });
        let configured = configured.unwrap_or_default();
        if std::env::var_os("GIT_ASKPASS").is_none() && !configured.contains("core.askpass") {
            vars.push(("GIT_ASKPASS".to_owned(), "echo".to_owned()));
        }
        if std::env::var_os("GIT_SSH_COMMAND").is_none()
            && std::env::var_os("GIT_SSH").is_none()
            && !configured.contains("core.sshcommand")
        {
            vars.push((
                "GIT_SSH_COMMAND".to_owned(),
                "ssh -oStrictHostKeyChecking=accept-new".to_owned(),
            ));
        }
        GitEnv { git, vars }
    })
}

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Runs `git <args>` and returns stdout. `extra_env` is added for this child only.
async fn git(args: &[&str], extra_env: &[(&str, &str)], what: &str) -> Result<Vec<u8>> {
    let env = git_env();
    let Some(bin) = &env.git else {
        return Err(Error::Git(format!(
            "\"git\" is not installed (needed for \"{}\")",
            redact_url(what)
        )));
    };
    let hooks = format!("core.hooksPath={NULL_DEVICE}");
    let mut cmd = Command::new(bin);
    cmd.args(["-c", &hooks, "-c", "submodule.recurse=false"])
        .args(args)
        .env_remove("BUN_INTERNAL_BUNX_INSTALL")
        .env_remove("BUN_INTERNAL_BUNX_HAS_COPY")
        .envs(env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .envs(extra_env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .map_err(|e| Error::Git(format!("failed to run git: {e}")))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(Error::Git(format!(
        "git {} failed for \"{}\": {}",
        args.iter()
            .find(|a| !a.starts_with('-'))
            .copied()
            .unwrap_or(""),
        redact_url(what),
        redact_url(stderr.trim())
    )))
}

/// `git ls-remote` output: `(sha, refname)` pairs.
pub(crate) fn parse_ls_remote(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let (sha, name) = l.split_once('\t')?;
            Some((sha.trim().to_ascii_lowercase(), name.trim().to_owned()))
        })
        .collect()
}

fn is_full_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The commit a ref names among `ls-remote` refs: a branch, a tag (peeled when annotated), a
/// full ref name, or `HEAD` for none.
pub(crate) fn find_ref(refs: &[(String, String)], reference: Option<&str>) -> Option<String> {
    let lookup = |name: &str| refs.iter().find(|(_, n)| n == name).map(|(s, _)| s.clone());
    let Some(r) = reference.filter(|r| !r.is_empty() && *r != "HEAD") else {
        return lookup("HEAD");
    };
    lookup(&format!("refs/tags/{r}^{{}}"))
        .or_else(|| lookup(&format!("refs/heads/{r}")))
        .or_else(|| lookup(&format!("refs/tags/{r}")))
        .or_else(|| lookup(&format!("{r}^{{}}")))
        .or_else(|| lookup(r))
}

/// `#semver:<range>`: the commit of the highest tag (`v` prefix allowed) that satisfies.
pub(crate) fn pick_semver_tag(refs: &[(String, String)], range: &Range) -> Option<String> {
    let mut best: Option<(Version, String)> = None;
    for (sha, name) in refs {
        let Some(tag) = name.strip_prefix("refs/tags/") else {
            continue;
        };
        let (tag, peeled) = match tag.strip_suffix("^{}") {
            Some(t) => (t, true),
            None => (tag, false),
        };
        let Some(v) = semver_ext::parse_version(tag) else {
            continue;
        };
        if !semver_ext::satisfies(range, &v, false) {
            continue;
        }
        let better = match &best {
            None => true,
            Some((b, _)) => {
                semver_ext::cmp(&v, b).is_gt() || (semver_ext::cmp(&v, b).is_eq() && peeled)
            }
        };
        if better {
            best = Some((v, sha.clone()));
        }
    }
    best.map(|(_, sha)| sha)
}

/// Resolves a committish with `git ls-remote`: a full SHA is taken as is, `#semver:` picks the
/// highest matching tag, a ref name is looked up, nothing means `HEAD`. An abbreviated SHA
/// needs the objects, so it goes through a clone in the system temp directory.
pub async fn resolve_commit(url: &str, committish: &Committish) -> Result<String> {
    if let Some(r) = committish.reference.as_deref()
        && is_full_sha(r)
        && committish.semver.is_none()
    {
        return Ok(r.to_ascii_lowercase());
    }
    let mut last = None;
    for candidate in clone_urls(url) {
        let out = match git(&["ls-remote", "--", &candidate], &[], &candidate).await {
            Ok(out) => out,
            Err(e) => {
                last = Some(e);
                continue;
            }
        };
        let refs = parse_ls_remote(&String::from_utf8_lossy(&out));
        if let Some(range) = &committish.semver {
            let range = Range::parse(range)
                .map_err(|e| Error::Git(format!("invalid semver range {range:?}: {e}")))?;
            return pick_semver_tag(&refs, &range).ok_or_else(|| {
                Error::Git(format!(
                    "no tag of \"{}\" satisfies {:?}",
                    redact_url(url),
                    committish.semver.as_deref().unwrap_or("")
                ))
            });
        }
        if let Some(sha) = find_ref(&refs, committish.reference.as_deref()) {
            return Ok(sha);
        }
        if let Some(r) = committish
            .reference
            .as_deref()
            .filter(|r| r.len() >= 4 && r.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            let tmp = std::env::temp_dir().join("fnpm-git");
            return resolve_in_clone(&tmp, url, r, false).await;
        }
        return Err(Error::Git(format!(
            "\"{}\" has no ref {:?}",
            redact_url(url),
            committish.reference.as_deref().unwrap_or("HEAD")
        )));
    }
    Err(last.unwrap_or_else(|| Error::Git(format!("not a git URL: \"{}\"", redact_url(url)))))
}

/// Resolves a committish in the bare clone cache (bun's `git log` lookup), cloning or fetching
/// first. Works for abbreviated SHAs too. With `offline`, only an existing clone is used.
pub async fn resolve_commit_in(
    cache_dir: &Path,
    url: &str,
    committish: &Committish,
    offline: bool,
) -> Result<String> {
    if let Some(range) = &committish.semver {
        let repo = ensure_clone(cache_dir, url, None, offline).await?;
        let git_dir = format!("--git-dir={}", repo.display());
        let out = git(&[&git_dir, "show-ref", "--tags", "-d"], &[], url)
            .await
            .unwrap_or_default();
        let refs: Vec<(String, String)> = String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| l.split_once(' ').map(|(s, n)| (s.to_owned(), n.to_owned())))
            .collect();
        let range = Range::parse(range)
            .map_err(|e| Error::Git(format!("invalid semver range {range:?}: {e}")))?;
        return pick_semver_tag(&refs, &range).ok_or_else(|| {
            Error::Git(format!(
                "no tag of \"{}\" satisfies {range:?}",
                redact_url(url)
            ))
        });
    }
    resolve_in_clone(
        cache_dir,
        url,
        committish.reference.as_deref().unwrap_or(""),
        offline,
    )
    .await
}

async fn resolve_in_clone(
    cache_dir: &Path,
    url: &str,
    reference: &str,
    offline: bool,
) -> Result<String> {
    let repo = ensure_clone(cache_dir, url, None, offline).await?;
    let git_dir = format!("--git-dir={}", repo.display());
    let out = if reference.is_empty() || reference == "HEAD" {
        git(
            &[
                &git_dir,
                "log",
                "--format=%H",
                "-1",
                "--no-walk=unsorted",
                "--ignore-missing",
                "refs/remotes/origin/HEAD",
                "HEAD",
            ],
            &[],
            url,
        )
        .await?
    } else {
        git(
            &[
                &git_dir,
                "log",
                "--format=%H",
                "-1",
                "--end-of-options",
                reference,
            ],
            &[],
            url,
        )
        .await?
    };
    let sha = String::from_utf8_lossy(&out).trim().to_ascii_lowercase();
    if is_full_sha(&sha) {
        Ok(sha)
    } else {
        Err(Error::Git(format!(
            "\"{}\" has no commit {reference:?}",
            redact_url(url)
        )))
    }
}

fn is_own_clone(repo: &Path) -> bool {
    repo.join("HEAD").exists()
        && repo.join("objects").is_dir()
        && !repo.join("commondir").exists()
        && !repo.join("objects/info/alternates").exists()
}

async fn has_commit(repo: &Path, sha: &str) -> bool {
    let git_dir = format!("--git-dir={}", repo.display());
    let object = format!("{sha}^{{commit}}");
    git(&[&git_dir, "cat-file", "-e", &object], &[], "")
        .await
        .is_ok()
}

static TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp_name(parent: &Path, prefix: &str) -> PathBuf {
    parent.join(format!(
        ".{prefix}-{}-{}",
        std::process::id(),
        TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// The bare clone of `url`, cloned or fetched as needed. A clone that already has `want` is
/// not fetched again.
async fn ensure_clone(
    cache_dir: &Path,
    url: &str,
    want: Option<&str>,
    offline: bool,
) -> Result<PathBuf> {
    let repo = clone_dir(cache_dir, url);
    let urls = clone_urls(url);
    if is_own_clone(&repo) {
        if let Some(sha) = want
            && has_commit(&repo, sha).await
        {
            return Ok(repo);
        }
        if offline {
            return Ok(repo);
        }
        let git_dir = format!("--git-dir={}", repo.display());
        let mut last = None;
        for candidate in &urls {
            match git(
                &[
                    &git_dir,
                    "-c",
                    "core.longpaths=true",
                    "fetch",
                    "--quiet",
                    "--prune",
                    "--no-recurse-submodules",
                    "--",
                    candidate,
                    "+HEAD*:refs/remotes/origin/HEAD*",
                    "+refs/heads/*:refs/heads/*",
                    "+refs/tags/*:refs/tags/*",
                ],
                &[],
                candidate,
            )
            .await
            {
                Ok(_) => return Ok(repo),
                Err(e) => last = Some(e),
            }
        }
        return Err(
            last.unwrap_or_else(|| Error::Git(format!("not a git URL: \"{}\"", redact_url(url))))
        );
    }
    if offline {
        return Err(Error::Offline(format!(
            "--offline: git repository for \"{}\" is not in the cache",
            redact_url(url)
        )));
    }
    if urls.is_empty() {
        return Err(Error::Git(format!(
            "not a git URL: \"{}\"",
            redact_url(url)
        )));
    }
    let parent = repo
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cache_dir.to_path_buf());
    tokio::fs::create_dir_all(&parent).await?;
    let mut last = None;
    for candidate in &urls {
        let tmp = tmp_name(&parent, "clone");
        let tmp_s = tmp.to_string_lossy().into_owned();
        match git(
            &[
                "clone",
                "-c",
                "core.longpaths=true",
                "--quiet",
                "--bare",
                "--template=",
                "--",
                candidate,
                &tmp_s,
            ],
            &[],
            candidate,
        )
        .await
        {
            Ok(_) => {
                if tokio::fs::rename(&tmp, &repo).await.is_err() {
                    // Another install finished first; use its clone.
                    let _ = tokio::fs::remove_dir_all(&tmp).await;
                    if !is_own_clone(&repo) {
                        let _ = tokio::fs::remove_dir_all(&repo).await;
                        continue;
                    }
                }
                return Ok(repo);
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&tmp).await;
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Git(format!("clone of \"{}\" failed", redact_url(url)))))
}

/// `git ls-tree -r -z` entries: `(mode, path)`.
fn parse_ls_tree(out: &[u8]) -> Vec<(String, String)> {
    out.split(|&b| b == 0)
        .filter(|e| !e.is_empty())
        .filter_map(|e| {
            let text = String::from_utf8_lossy(e);
            let (meta, path) = text.split_once('\t')?;
            Some((meta.split(' ').next()?.to_owned(), path.to_owned()))
        })
        .collect()
}

/// git wrote each symlink as a file holding its target (`core.symlinks=false`). A link to a
/// file inside the tree becomes a copy of it; anything else is removed.
fn materialize_links(root: &Path, tree: &[(String, String)]) -> std::io::Result<()> {
    for (mode, path) in tree {
        if mode != "120000" {
            continue;
        }
        let link = root.join(path);
        let target = std::fs::read_to_string(&link).unwrap_or_default();
        let base = Path::new(path).parent().unwrap_or(Path::new(""));
        let mut resolved = PathBuf::new();
        let mut inside = !target.starts_with('/');
        for comp in base.join(&target).components() {
            match comp {
                std::path::Component::Normal(c) => resolved.push(c),
                std::path::Component::ParentDir => inside &= resolved.pop(),
                std::path::Component::CurDir => {}
                _ => inside = false,
            }
        }
        let source = root.join(&resolved);
        let is_link = tree
            .iter()
            .any(|(m, p)| m == "120000" && Path::new(p) == resolved);
        std::fs::remove_file(&link)?;
        if inside && !is_link && source.is_file() {
            std::fs::copy(&source, &link)?;
        }
    }
    Ok(())
}

/// Checks out `sha` from the cached clone of `url` into `dest` (created; must not exist or be
/// empty), without `.git`. `subdir` (`#::path:`) installs only that folder.
pub async fn checkout(
    cache_dir: &Path,
    url: &str,
    sha: &str,
    subdir: Option<&str>,
    dest: &Path,
) -> Result<()> {
    checkout_with(cache_dir, url, sha, subdir, dest, false).await
}

/// [`checkout`], with `--offline` (only an existing clone is used).
pub async fn checkout_with(
    cache_dir: &Path,
    url: &str,
    sha: &str,
    subdir: Option<&str>,
    dest: &Path,
    offline: bool,
) -> Result<()> {
    if !is_safe_resolved_tag(sha) {
        return Err(Error::Git(format!(
            "invalid git commit \"{sha}\" for \"{}\"",
            redact_url(url)
        )));
    }
    if let Some(sub) = subdir
        && (sub.split(['/', '\\']).any(|c| c == "..") || Path::new(sub).is_absolute())
    {
        return Err(Error::Git(format!(
            "invalid path {sub:?} in \"{}\"",
            redact_url(url)
        )));
    }
    let repo = ensure_clone(cache_dir, url, Some(sha), offline).await?;
    let git_dir = format!("--git-dir={}", repo.display());
    let tree_out = git(&[&git_dir, "ls-tree", "-r", "-z", sha], &[], url).await?;
    let tree = parse_ls_tree(&tree_out);

    let parent = dest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    tokio::fs::create_dir_all(&parent).await?;
    let staging = tmp_name(&parent, "checkout");
    tokio::fs::create_dir_all(&staging).await?;
    let result = async {
        if !tree.is_empty() {
            let index = format!("{}.index", staging.display());
            let work_tree = format!("--work-tree={}", staging.display());
            let source = format!("--source={sha}");
            git(
                &[
                    &git_dir,
                    &work_tree,
                    "-c",
                    "core.longpaths=true",
                    "-c",
                    "core.symlinks=false",
                    "restore",
                    "--quiet",
                    &source,
                    "--worktree",
                    "--",
                    ".",
                ],
                &[("GIT_INDEX_FILE", &index)],
                url,
            )
            .await?;
            let _ = tokio::fs::remove_file(&index).await;
            let (root, tree) = (staging.clone(), tree.clone());
            tokio::task::spawn_blocking(move || materialize_links(&root, &tree))
                .await
                .map_err(|e| Error::Git(e.to_string()))??;
        }
        let from = match subdir.filter(|s| !s.is_empty()) {
            Some(sub) => staging.join(sub),
            None => staging.clone(),
        };
        if !from.is_dir() {
            return Err(Error::Git(format!(
                "\"{}\" has no folder {:?} at {sha}",
                redact_url(url),
                subdir.unwrap_or("")
            )));
        }
        if dest.is_dir() {
            // Only an empty folder may be replaced.
            let _ = tokio::fs::remove_dir(dest).await;
        }
        tokio::fs::rename(&from, dest).await?;
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&staging).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnpm_semver::{Comparator, Op};

    #[test]
    fn resolved_tags() {
        assert!(is_safe_resolved_tag("0123abc"));
        assert!(is_safe_resolved_tag("oven-sh-bun-0123456"));
        for bad in ["", "-x", ".", "..", "a/b", "a\0b", "a b", &"a".repeat(257)] {
            assert!(!is_safe_resolved_tag(bad), "{bad:?}");
        }
    }

    #[test]
    fn url_forms() {
        // Like bun, the scp-like user stays in the https form.
        assert_eq!(
            try_https("git@github.com:user/repo.git").as_deref(),
            Some("https://git@github.com/user/repo.git")
        );
        assert_eq!(
            try_ssh("git@github.com:user/repo.git").as_deref(),
            Some("git@github.com:user/repo.git")
        );
        assert_eq!(try_ssh("https://github.com/u/r"), None);
        assert_eq!(
            try_https("https://github.com/u/r").as_deref(),
            Some("https://github.com/u/r")
        );
        assert_eq!(
            try_https("ssh://git@github.com/u/r.git").as_deref(),
            Some("https://github.com/u/r.git")
        );
        assert_eq!(
            try_https("ssh://git@example.com/u/r.git").as_deref(),
            Some("https://git@example.com/u/r.git")
        );
        assert_eq!(
            try_ssh("ssh://git@example.com/u/r.git").as_deref(),
            Some("ssh://git@example.com/u/r.git")
        );
        assert_eq!(
            try_ssh("github:user/repo").as_deref(),
            Some("ssh://git@github.com/user/repo")
        );
        assert_eq!(
            try_ssh("gitlab:user/repo").as_deref(),
            Some("ssh://git@gitlab.com/user/repo")
        );
        assert_eq!(
            try_https("bitbucket:user/repo").as_deref(),
            Some("https://bitbucket.org/user/repo")
        );
        assert_eq!(
            try_ssh("me@host.example:path/repo").as_deref(),
            Some("ssh://me@host.example/path/repo")
        );
        assert_eq!(
            clone_urls("git+https://h/r.git"),
            vec!["https://h/r.git".to_owned()]
        );
        assert_eq!(
            clone_urls("git://h/r.git"),
            vec!["git://h/r.git".to_owned()]
        );
        assert_eq!(
            clone_urls("git@github.com:u/r.git"),
            vec![
                "https://git@github.com/u/r.git".to_owned(),
                "git@github.com:u/r.git".to_owned()
            ]
        );
    }

    #[test]
    fn ls_remote_lookup() {
        let refs = parse_ls_remote(
            "aaaa\tHEAD\nbbbb\trefs/heads/main\ncccc\trefs/tags/v1.0.0\ndddd\trefs/tags/v1.0.0^{}\neeee\trefs/tags/v1.2.0\nffff\trefs/tags/2.0.0-beta\n1111\trefs/heads/feature/x\n",
        );
        assert_eq!(find_ref(&refs, None).as_deref(), Some("aaaa"));
        assert_eq!(find_ref(&refs, Some("main")).as_deref(), Some("bbbb"));
        assert_eq!(find_ref(&refs, Some("v1.0.0")).as_deref(), Some("dddd"));
        assert_eq!(find_ref(&refs, Some("feature/x")).as_deref(), Some("1111"));
        assert_eq!(
            find_ref(&refs, Some("refs/heads/main")).as_deref(),
            Some("bbbb")
        );
        assert_eq!(find_ref(&refs, Some("nope")), None);

        let v = |s: &str| semver_ext::parse_version(s).unwrap();
        let caret1 = Range {
            sets: vec![vec![
                Comparator {
                    op: Op::Gte,
                    version: v("1.0.0"),
                },
                Comparator {
                    op: Op::Lt,
                    version: v("2.0.0-0"),
                },
            ]],
        };
        assert_eq!(pick_semver_tag(&refs, &caret1).as_deref(), Some("eeee"));
        let exact = Range {
            sets: vec![vec![Comparator {
                op: Op::Eq,
                version: v("1.0.0"),
            }]],
        };
        assert_eq!(pick_semver_tag(&refs, &exact).as_deref(), Some("dddd"));
        let none = Range {
            sets: vec![vec![Comparator {
                op: Op::Gte,
                version: v("3.0.0"),
            }]],
        };
        assert_eq!(pick_semver_tag(&refs, &none), None);
    }

    #[test]
    fn ls_tree_entries() {
        let t = parse_ls_tree(
            b"100644 blob abc\tpackage.json\x00120000 blob def\tlink\x00040000 tree x\tdir name\x00",
        );
        assert_eq!(t[0], ("100644".to_owned(), "package.json".to_owned()));
        assert_eq!(t[1], ("120000".to_owned(), "link".to_owned()));
        assert_eq!(t[2].1, "dir name");
    }

    #[test]
    fn links_become_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("lib/real.js"), "x").unwrap();
        std::fs::write(root.join("alias.js"), "lib/real.js").unwrap();
        std::fs::write(root.join("escape.js"), "../../etc/passwd").unwrap();
        let tree = vec![
            ("100644".to_owned(), "lib/real.js".to_owned()),
            ("120000".to_owned(), "alias.js".to_owned()),
            ("120000".to_owned(), "escape.js".to_owned()),
        ];
        materialize_links(root, &tree).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("alias.js")).unwrap(), "x");
        assert!(!root.join("escape.js").exists());
    }

    async fn run(dir: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    #[tokio::test]
    async fn clone_resolve_checkout_against_a_local_repository() {
        if which_git().is_none() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("packages/sub")).unwrap();
        run(&src, &["init", "-q", "-b", "main"]).await;
        std::fs::write(
            src.join("package.json"),
            r#"{"name":"g","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(src.join("packages/sub/package.json"), r#"{"name":"sub"}"#).unwrap();
        run(&src, &["add", "."]).await;
        run(
            &src,
            &[
                "-c",
                "user.email=a@b",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "one",
            ],
        )
        .await;
        run(&src, &["tag", "v1.0.0"]).await;
        std::fs::write(
            src.join("package.json"),
            r#"{"name":"g","version":"1.1.0"}"#,
        )
        .unwrap();
        run(
            &src,
            &[
                "-c",
                "user.email=a@b",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-am",
                "two",
            ],
        )
        .await;
        run(
            &src,
            &[
                "-c",
                "user.email=a@b",
                "-c",
                "user.name=t",
                "tag",
                "-a",
                "-m",
                "t",
                "v1.1.0",
            ],
        )
        .await;
        let url = format!("file://{}", src.display());

        let head = resolve_commit(&url, &Committish::default()).await.unwrap();
        assert_eq!(head.len(), 40);
        let v1 = resolve_commit(&url, &Committish::parse("v1.0.0"))
            .await
            .unwrap();
        assert_ne!(v1, head);
        let tagged = resolve_commit(&url, &Committish::parse("v1.1.0"))
            .await
            .unwrap();
        assert_eq!(tagged, head, "annotated tags resolve to the commit");
        assert!(
            resolve_commit(&url, &Committish::parse("nope"))
                .await
                .is_err()
        );

        let cache = tmp.path().join("cache");
        let short = &v1[..8];
        assert_eq!(
            resolve_commit_in(&cache, &url, &Committish::parse(short), false)
                .await
                .unwrap(),
            v1
        );
        assert!(clone_dir(&cache, &url).join("HEAD").exists());
        assert_eq!(
            resolve_commit_in(&cache, &url, &Committish::default(), true)
                .await
                .unwrap(),
            head
        );

        let dest = tmp.path().join("out/g");
        checkout(&cache, &url, &v1, None, &dest).await.unwrap();
        assert!(
            std::fs::read_to_string(dest.join("package.json"))
                .unwrap()
                .contains("1.0.0")
        );
        assert!(!dest.join(".git").exists());
        let sub = tmp.path().join("out/sub");
        checkout(&cache, &url, &head, Some("packages/sub"), &sub)
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(sub.join("package.json"))
                .unwrap()
                .contains("sub")
        );
        assert!(
            checkout(&cache, &url, &head, Some("../x"), &tmp.path().join("out/x"))
                .await
                .is_err()
        );
        assert!(
            checkout(&cache, &url, "-bad", None, &tmp.path().join("out/y"))
                .await
                .is_err()
        );

        let other = tmp.path().join("cache2");
        let e = checkout_with(&other, &url, &v1, None, &tmp.path().join("out/z"), true)
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Offline(_)), "{e}");
    }
}
