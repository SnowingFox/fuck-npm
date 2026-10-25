//! Lexical path helpers. Workspace paths are never canonicalised (bun keeps the paths it was
//! given, so a project reached through a symlink keeps its spelling).

use std::path::{Component, Path, PathBuf};

/// `path` made absolute against the process cwd and lexically normalised.
pub(crate) fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        normalize(&cwd.join(path))
    }
}

/// Resolves `.` and `..` without touching the disk. `..` at the root stays at the root.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// `to` relative to `from` (both absolute and normalised), `/`-separated; empty when equal.
pub(crate) fn relative_posix(from: &Path, to: &Path) -> String {
    let a: Vec<Component<'_>> = from.components().collect();
    let b: Vec<Component<'_>> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..a.len() {
        parts.push("..".to_owned());
    }
    for c in &b[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    parts.join("/")
}

/// A path with `/` separators (backslashes converted on Windows).
pub(crate) fn to_posix(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// `./a//b/` -> `a/b`; `.` -> ``.
pub(crate) fn clean_relative(path: &str) -> String {
    let path = if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    };
    path.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Joins `/`-separated `rel` onto the absolute posix `base`, resolving `.` and `..` lexically
/// and dropping empty segments and the trailing slash. An absolute `rel` replaces `base`. Glob
/// characters are kept as they are.
pub(crate) fn join_posix(base: &str, rel: &str) -> String {
    let rel = if cfg!(windows) {
        rel.replace('\\', "/")
    } else {
        rel.to_owned()
    };
    let start = if rel.starts_with('/') { "" } else { base };
    let mut segs: Vec<&str> = Vec::new();
    for s in start.split('/').chain(rel.split('/')) {
        match s {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    let mut out = String::with_capacity(base.len() + rel.len() + 1);
    for s in &segs {
        out.push('/');
        out.push_str(s);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_relative() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(
            relative_posix(Path::new("/a/b"), Path::new("/a/b/c/d")),
            "c/d"
        );
        assert_eq!(relative_posix(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert_eq!(relative_posix(Path::new("/a/b"), Path::new("/a/b")), "");
        assert_eq!(clean_relative("./packages//a/"), "packages/a");
        assert_eq!(clean_relative("."), "");
    }

    #[test]
    fn join() {
        assert_eq!(join_posix("/root", "./packages/*"), "/root/packages/*");
        assert_eq!(join_posix("/root/apps", ".."), "/root");
        assert_eq!(join_posix("/root", "./"), "/root");
        assert_eq!(join_posix("/root", "/abs/x/"), "/abs/x");
        assert_eq!(join_posix("/", ".."), "/");
        assert_eq!(join_posix("/root", "a//b/./c/"), "/root/a/b/c");
    }
}
