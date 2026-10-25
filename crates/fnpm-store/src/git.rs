//! Preparing a fresh git checkout before it is imported (bun `install/git_runner.rs`
//! `finish_checkout`, `install/repository.rs`).
//!
//! - `.git` and a checked-in tag file (`.bun-tag`, `.fnpm-tag`) are removed.
//! - git was told to check symlinks out as plain files; each one is replaced by a copy of its
//!   final target (resolved in the tree listing, never on disk), or removed when it leaves
//!   the checkout or loops.
//! - A checked-in `node_modules` is removed, as npm and pnpm do, unless the package bundles
//!   dependencies.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::{Error, Result};

/// The tag becomes part of a directory name.
pub const MAX_RESOLVED_TAG_LEN: usize = 256;

/// `resolved` (a commit SHA, or `<owner>-<repo>-<sha>`) is a single safe path component: no
/// separators, no NUL, no leading `-` that git would parse as an option.
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

/// The root folder of GitHub's tarball of `commit` (`<owner>-<repo>-<7 chars>`), if safe.
pub fn github_tarball_tag(owner: &str, repo: &str, commit: &str) -> Option<String> {
    let short: String = commit.chars().take(7).collect();
    let tag = format!("{owner}-{repo}-{short}");
    is_safe_resolved_tag(&tag).then_some(tag)
}

/// Whether package.json text declares bundled dependencies (`true`, or a non-empty list, in
/// `bundleDependencies` or `bundledDependencies`).
pub fn bundles_dependencies(package_json: &str) -> bool {
    let Ok(serde_json::Value::Object(root)) =
        serde_json::from_str::<serde_json::Value>(package_json)
    else {
        return false;
    };
    let field = root
        .get("bundleDependencies")
        .or_else(|| root.get("bundledDependencies"));
    match field {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Array(a)) => !a.is_empty(),
        _ => false,
    }
}

/// A path from git's listing that stays inside the folder it is opened from.
fn stays_in_checkout(path: &str) -> bool {
    if cfg!(windows) {
        return crate::patch::is_safe_patch_path(path.as_bytes());
    }
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && !path.split('/').any(|p| p == "..")
}

const LINK: &str = "120000";

enum LinkEnd {
    Unknown,
    Walking,
    At(Option<String>),
}

/// Where each symlink in `entries` (sorted `(path, mode)`) finally leads, following links
/// through other links using only `targets` (the link texts).
fn link_ends(entries: &[(&str, &str)], targets: &[Option<String>]) -> Vec<LinkEnd> {
    struct Walk<'a> {
        link: usize,
        at: String,
        rest: Vec<&'a str>,
    }
    let lookup = |path: &str| entries.binary_search_by(|(p, _)| (*p).cmp(path)).ok();
    let walk_of = |link: usize| {
        let path = entries[link].0;
        let mut rest: Vec<&str> = targets[link]
            .as_deref()
            .map(|t| t.split('/').collect())
            .unwrap_or_default();
        rest.reverse();
        Walk {
            link,
            at: path
                .rfind('/')
                .map_or(String::new(), |i| path[..i].to_owned()),
            rest,
        }
    };
    let mut ends: Vec<LinkEnd> = targets
        .iter()
        .map(|t| match t {
            Some(t) if !t.starts_with('/') => LinkEnd::Unknown,
            _ => LinkEnd::At(None),
        })
        .collect();
    let mut walks: Vec<Walk<'_>> = Vec::new();
    for first in 0..ends.len() {
        if !matches!(ends[first], LinkEnd::Unknown) {
            continue;
        }
        ends[first] = LinkEnd::Walking;
        walks.push(walk_of(first));
        while let Some(walk) = walks.last_mut() {
            let end = match walk.rest.pop() {
                None => Some(std::mem::take(&mut walk.at)),
                Some("" | ".") => continue,
                Some("..") if walk.at.is_empty() => None,
                Some("..") => {
                    let parent = walk.at.rfind('/').unwrap_or(0);
                    walk.at.truncate(parent);
                    continue;
                }
                Some(part) => {
                    let parent = walk.at.len();
                    if parent > 0 {
                        walk.at.push('/');
                    }
                    walk.at.push_str(part);
                    let Some(next) = lookup(&walk.at).filter(|&n| targets[n].is_some()) else {
                        continue;
                    };
                    match &ends[next] {
                        LinkEnd::At(Some(end)) => {
                            walk.at.clone_from(end);
                            continue;
                        }
                        LinkEnd::At(None) | LinkEnd::Walking => None,
                        LinkEnd::Unknown => {
                            walk.at.truncate(parent);
                            walk.rest.push(part);
                            ends[next] = LinkEnd::Walking;
                            walks.push(walk_of(next));
                            continue;
                        }
                    }
                }
            };
            let link = walk.link;
            ends[link] = LinkEnd::At(end);
            walks.pop();
        }
    }
    ends
}

fn io_err(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Replaces the symlinks git checked out as plain files. `tree` is `git ls-tree -r -z`
/// output (`<mode> <type> <hash>\t<path>\0`...).
pub fn copy_linked_files(checkout: &Path, tree: &[u8]) -> Result<()> {
    let tree = String::from_utf8_lossy(tree);
    if !tree.split('\0').any(|e| e.starts_with(LINK)) {
        return Ok(());
    }
    let mut entries: Vec<(&str, &str)> = tree
        .split('\0')
        .filter_map(|e| e.split_once('\t'))
        .map(|(head, path)| (path, &head[..head.len().min(LINK.len())]))
        .collect();
    entries.sort_unstable();
    entries.dedup_by(|a, b| a.0 == b.0);
    // Every target is read before any link is replaced: a link can point to another link.
    let mut targets: Vec<Option<String>> = Vec::with_capacity(entries.len());
    for &(path, mode) in &entries {
        if mode != LINK || !stays_in_checkout(path) {
            targets.push(None);
            continue;
        }
        let file = checkout.join(path);
        match std::fs::symlink_metadata(&file) {
            Ok(m) if m.is_file() => targets.push(Some(
                String::from_utf8_lossy(&std::fs::read(&file).map_err(io_err(&file))?).into_owned(),
            )),
            Ok(_) => targets.push(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => targets.push(None),
            Err(e) => return Err(io_err(&file)(e)),
        }
    }
    let modes: HashMap<&str, &str> = entries.iter().copied().collect();
    let ends = link_ends(&entries, &targets);
    for (link, end) in ends.iter().enumerate() {
        if targets[link].is_none() {
            continue;
        }
        let path = checkout.join(entries[link].0);
        std::fs::remove_file(&path).map_err(io_err(&path))?;
        let LinkEnd::At(Some(end)) = end else {
            continue;
        };
        let Some(mode) = modes.get(end.as_str()) else {
            continue;
        };
        let perm = match *mode {
            "100755" => 0o755,
            "100644" => 0o644,
            _ => continue,
        };
        if !stays_in_checkout(end) {
            continue;
        }
        let from = checkout.join(end);
        match std::fs::symlink_metadata(&from) {
            Ok(m) if m.is_file() => {}
            _ => continue,
        }
        std::fs::copy(&from, &path).map_err(io_err(&path))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(perm));
        }
        #[cfg(not(unix))]
        let _ = perm;
    }
    Ok(())
}

/// Removes a checked-in `node_modules` unless the package bundles dependencies.
pub fn strip_node_modules(checkout: &Path) -> Result<()> {
    let nm = checkout.join("node_modules");
    match std::fs::symlink_metadata(&nm) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(&nm)(e)),
        Ok(m) if !m.is_dir() => return std::fs::remove_file(&nm).map_err(io_err(&nm)),
        Ok(_) => {}
    }
    let pj = std::fs::read_to_string(checkout.join("package.json")).unwrap_or_default();
    if bundles_dependencies(&pj) {
        return Ok(());
    }
    std::fs::remove_dir_all(&nm).map_err(io_err(&nm))
}

/// All of the cleanup of a fresh checkout at `checkout`; `tree` is the `git ls-tree -r -z`
/// listing of the commit, if the caller has it.
pub fn finish_checkout(checkout: &Path, tree: Option<&[u8]>) -> Result<()> {
    for name in [".git", ".bun-tag", ".fnpm-tag"] {
        crate::sys::remove_tree(&checkout.join(name)).map_err(io_err(checkout))?;
    }
    if let Some(tree) = tree {
        copy_linked_files(checkout, tree)?;
    }
    strip_node_modules(checkout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_tags() {
        assert!(is_safe_resolved_tag("0123456789abcdef"));
        assert!(is_safe_resolved_tag("owner-repo-abc1234"));
        for bad in ["", "-c", ".", "..", "a/b", "a\\b", "a b", "a\0b"] {
            assert!(!is_safe_resolved_tag(bad), "{bad:?}");
        }
        assert!(!is_safe_resolved_tag(&"a".repeat(257)));
        assert_eq!(
            github_tarball_tag("oven-sh", "bun", "abcdef0123").as_deref(),
            Some("oven-sh-bun-abcdef0")
        );
        assert_eq!(github_tarball_tag("-x", "y", "z"), None);
    }

    #[test]
    fn bundled() {
        assert!(bundles_dependencies(r#"{"bundleDependencies":true}"#));
        assert!(bundles_dependencies(r#"{"bundledDependencies":["a"]}"#));
        assert!(!bundles_dependencies(r#"{"bundledDependencies":[]}"#));
        assert!(!bundles_dependencies(r#"{"bundleDependencies":false}"#));
        assert!(!bundles_dependencies("not json"));
    }

    fn listing(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (mode, path) in entries {
            out.extend_from_slice(
                format!("{mode} blob 0000000000000000000000000000000000000000\t{path}\0")
                    .as_bytes(),
            );
        }
        out
    }

    #[test]
    fn checkout_cleanup() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        std::fs::create_dir_all(d.join(".git/objects")).unwrap();
        std::fs::write(d.join(".bun-tag"), "x").unwrap();
        std::fs::create_dir_all(d.join("node_modules/dep")).unwrap();
        std::fs::create_dir_all(d.join("lib")).unwrap();
        std::fs::write(d.join("package.json"), r#"{"name":"x"}"#).unwrap();
        std::fs::write(d.join("lib/real.js"), "real").unwrap();
        // Links checked out as files holding their target text.
        std::fs::write(d.join("index.js"), "lib/real.js").unwrap();
        std::fs::write(d.join("chain.js"), "index.js").unwrap();
        std::fs::write(d.join("escape.js"), "../outside").unwrap();
        std::fs::write(d.join("abs.js"), "/etc/passwd").unwrap();
        std::fs::write(d.join("loop-a"), "loop-b").unwrap();
        std::fs::write(d.join("loop-b"), "loop-a").unwrap();
        let tree = listing(&[
            ("100644", "package.json"),
            ("100755", "lib/real.js"),
            ("120000", "index.js"),
            ("120000", "chain.js"),
            ("120000", "escape.js"),
            ("120000", "abs.js"),
            ("120000", "loop-a"),
            ("120000", "loop-b"),
        ]);
        finish_checkout(d, Some(&tree)).unwrap();
        assert!(!d.join(".git").exists());
        assert!(!d.join(".bun-tag").exists());
        assert!(!d.join("node_modules").exists());
        assert_eq!(std::fs::read_to_string(d.join("index.js")).unwrap(), "real");
        assert_eq!(std::fs::read_to_string(d.join("chain.js")).unwrap(), "real");
        for gone in ["escape.js", "abs.js", "loop-a", "loop-b"] {
            assert!(!d.join(gone).exists(), "{gone}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(d.join("index.js"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn bundled_node_modules_are_kept() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        std::fs::create_dir_all(d.join("node_modules/dep")).unwrap();
        std::fs::write(d.join("package.json"), r#"{"bundleDependencies":["dep"]}"#).unwrap();
        finish_checkout(d, None).unwrap();
        assert!(d.join("node_modules/dep").exists());
        std::fs::remove_dir_all(d.join("node_modules")).unwrap();
        std::fs::write(d.join("node_modules"), "file").unwrap();
        std::fs::write(d.join("package.json"), "{}").unwrap();
        strip_node_modules(d).unwrap();
        assert!(!d.join("node_modules").exists());
    }
}
