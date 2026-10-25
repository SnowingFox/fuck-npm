//! The install cache directory (`pm cache`, bun `PackageManagerDirectories.rs`
//! `fetch_cache_directory_path` and `runtime/cli/package_manager_command.rs`).
//!
//! `pm cache rm` decides what it deletes only from trusted sources: the environment, the
//! `--cache-dir` flag and the user's own config, never a project's `.npmrc` / `fnpm.toml`
//! (a cloned repository must not be able to point it at `$HOME`). It refuses a directory
//! that is or contains the home directory, the fnpm executable, `$FNPM_HOME`, the project or
//! the current directory, and deletes only the directory's contents.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The fallback below the working directory when nothing else is configured.
pub const CWD_FALLBACK: &str = "node_modules/.fnpm-cache";

/// A resolved cache directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheDir {
    /// Absolute path.
    pub path: PathBuf,
    /// No cache directory is configured, so `path` is below the working directory.
    pub is_cwd_fallback: bool,
}

/// `~` and `~/…` expanded with `home`.
fn expand_home(value: &str, home: Option<&str>) -> PathBuf {
    match (value, home) {
        ("~", Some(h)) => PathBuf::from(h),
        (v, Some(h)) if v.starts_with("~/") => Path::new(h).join(&v[2..]),
        (v, _) => PathBuf::from(v),
    }
}

/// The cache directory, in bun's order with fnpm names first: `FNPM_CACHE_DIR`,
/// `BUN_INSTALL_CACHE_DIR`, then `configured` (`--cache-dir`, the user's `install.cache.dir`),
/// then `$FNPM_HOME/cache`, then `$XDG_CACHE_HOME/.fnpm/cache` or `$HOME/.fnpm/cache`, then
/// [`CWD_FALLBACK`]. An empty variable counts as unset. Relative values are taken from `cwd`.
pub fn fetch_cache_directory_path(
    env: &dyn Fn(&str) -> Option<String>,
    configured: Option<&Path>,
    cwd: &Path,
) -> CacheDir {
    let var = |name: &str| env(name).filter(|v| !v.trim().is_empty());
    let home = var("HOME").or_else(|| var("USERPROFILE"));
    let home = home.as_deref();
    let (path, fallback) =
        if let Some(dir) = var("FNPM_CACHE_DIR").or_else(|| var("BUN_INSTALL_CACHE_DIR")) {
            (expand_home(&dir, home), false)
        } else if let Some(dir) = configured.filter(|d| !d.as_os_str().is_empty()) {
            (dir.to_path_buf(), false)
        } else if let Some(dir) = var("FNPM_HOME") {
            (expand_home(&dir, home).join("cache"), false)
        } else if let Some(dir) = var("XDG_CACHE_HOME").or_else(|| home.map(str::to_owned)) {
            (expand_home(&dir, home).join(".fnpm").join("cache"), false)
        } else {
            (PathBuf::from(CWD_FALLBACK), true)
        };
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    CacheDir {
        path,
        is_cwd_fallback: fallback,
    }
}

fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// What `dir` (a real path) must not be deleted for: the label of the first protected path
/// that `dir` is or contains. Paths are compared as real paths, so a symlink to `$HOME`
/// counts as `$HOME`.
pub fn protected_in<'a>(dir: &Path, protected: &'a [(String, PathBuf)]) -> Option<&'a str> {
    let dir = real(dir);
    protected
        .iter()
        .find(|(_, path)| real(path).starts_with(&dir))
        .map(|(what, _)| what.as_str())
}

/// The protected paths bun checks, given what the caller knows: `[("the home directory",
/// home), ("the fnpm executable", exe), ("$FNPM_HOME", fnpm_home), ("the project directory",
/// project), ("the current directory", cwd)]`, skipping the unknown ones.
pub fn default_protected(
    home: Option<&Path>,
    exe: Option<&Path>,
    fnpm_home: Option<&Path>,
    project: Option<&Path>,
    cwd: &Path,
) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for (what, path) in [
        ("the home directory", home),
        ("the fnpm executable", exe),
        ("$FNPM_HOME", fnpm_home),
        ("the project directory", project),
        ("the current directory", Some(cwd)),
    ] {
        if let Some(p) = path {
            out.push((what.to_owned(), p.to_path_buf()));
        }
    }
    out
}

/// Options of [`clear_cache`].
#[derive(Debug, Clone, Default)]
pub struct ClearOptions {
    /// The directories to empty (the install cache, and the store when it should go too).
    pub dirs: Vec<PathBuf>,
    /// From [`default_protected`].
    pub protected: Vec<(String, PathBuf)>,
    /// Report only.
    pub dry_run: bool,
    /// The system temp dir, whose `.fnpmx-<uid>` / `fnpmx-<uid>-*` trees are removed too
    /// ([`crate::dlx`]). `None` skips them.
    pub temp_dir: Option<PathBuf>,
}

/// What [`clear_cache`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearReport {
    /// Directories that were emptied (or would be).
    pub cleared: Vec<PathBuf>,
    /// One of the directories contains a global store (`links/`), which goes too.
    pub had_global_store: bool,
    /// `fnpm x` trees removed from the temp dir.
    pub dlx_dirs: Vec<PathBuf>,
    /// `fnpm x` packages counted in what was removed.
    pub dlx_packages: usize,
    /// Failures, as messages.
    pub errors: Vec<String>,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

impl ClearReport {
    /// bun's output for this result.
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self.errors.clone();
        for dir in &self.cleared {
            if self.dry_run {
                out.push(format!(
                    "dry run: would delete 'fnpm install' cache at {}",
                    dir.display()
                ));
            } else {
                out.push("Cleared 'fnpm install' cache".to_owned());
            }
        }
        if self.had_global_store {
            out.push(format!(
                "note: global store {} removed too, run 'fnpm install' in each project that uses it",
                if self.dry_run { "would be" } else { "was" }
            ));
        }
        for dir in &self.dlx_dirs {
            if self.dry_run {
                out.push(format!("dry run: would delete {}", dir.display()));
            }
        }
        out.push(format!(
            "{} {} cached 'fnpmx' packages",
            if self.dry_run {
                "Would clear"
            } else {
                "Cleared"
            },
            self.dlx_packages
        ));
        out
    }
}

fn delete_children(dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        crate::sys::remove_tree(&entry.path())?;
    }
    Ok(())
}

/// `pm cache rm`: empties each directory (the directory itself may be a mount point or a
/// symlink target), after the safety check. A refused directory is an [`Error::Refused`] and
/// nothing is deleted.
pub fn clear_cache(opts: &ClearOptions) -> Result<ClearReport> {
    for dir in &opts.dirs {
        if dir.exists()
            && let Some(what) = protected_in(dir, &opts.protected)
        {
            return Err(Error::Refused {
                message: format!(
                    "refusing to clear \"{}\": is or contains {what}\nnote: cache directory comes from $FNPM_CACHE_DIR, $BUN_INSTALL_CACHE_DIR, --cache-dir, ~/.fnpm/fnpm.toml or $FNPM_HOME. point it at a directory used only for the fnpm install cache",
                    dir.display()
                ),
            });
        }
    }
    let mut report = ClearReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };
    for dir in &opts.dirs {
        if !dir.exists() {
            continue;
        }
        report.had_global_store |= dir.join("links").is_dir();
        report.dlx_packages += crate::dlx::count_cached_packages(dir);
        if !opts.dry_run
            && let Err(e) = delete_children(dir)
        {
            report
                .errors
                .push(format!("error: Could not delete {}: {e}", dir.display()));
            continue;
        }
        report.cleared.push(dir.clone());
    }
    if let Some(tmp) = &opts.temp_dir {
        let root = crate::dlx::cache_root_name();
        let prefix = format!("{}-", &root[1..]);
        if let Ok(entries) = std::fs::read_dir(tmp) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_root = name == root;
                if !is_root && !name.starts_with(&prefix) {
                    continue;
                }
                report.dlx_packages += if is_root {
                    crate::dlx::count_cached_packages(tmp)
                } else {
                    1
                };
                if !opts.dry_run
                    && let Err(e) = crate::sys::remove_tree(&entry.path())
                {
                    report
                        .errors
                        .push(format!("error: Could not delete {name}: {e}"));
                    continue;
                }
                report.dlx_dirs.push(entry.path());
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn resolution_order() {
        let cwd = Path::new("/work/app");
        let full = env(&[
            ("FNPM_CACHE_DIR", "/a"),
            ("BUN_INSTALL_CACHE_DIR", "/b"),
            ("FNPM_HOME", "/h"),
            ("XDG_CACHE_HOME", "/x"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            fetch_cache_directory_path(&full, Some(Path::new("/c")), cwd).path,
            PathBuf::from("/a")
        );
        let e = env(&[("BUN_INSTALL_CACHE_DIR", "/b"), ("HOME", "/home/u")]);
        assert_eq!(
            fetch_cache_directory_path(&e, Some(Path::new("/c")), cwd).path,
            PathBuf::from("/b")
        );
        let e = env(&[
            ("FNPM_CACHE_DIR", ""),
            ("HOME", "/home/u"),
            ("FNPM_HOME", "/h"),
        ]);
        assert_eq!(
            fetch_cache_directory_path(&e, Some(Path::new("/c")), cwd).path,
            PathBuf::from("/c")
        );
        assert_eq!(
            fetch_cache_directory_path(&e, None, cwd).path,
            PathBuf::from("/h/cache")
        );
        let e = env(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/home/u")]);
        assert_eq!(
            fetch_cache_directory_path(&e, None, cwd).path,
            PathBuf::from("/x/.fnpm/cache")
        );
        let e = env(&[("HOME", "/home/u")]);
        assert_eq!(
            fetch_cache_directory_path(&e, None, cwd).path,
            PathBuf::from("/home/u/.fnpm/cache")
        );
        let e = env(&[("FNPM_CACHE_DIR", "~/c"), ("HOME", "/home/u")]);
        assert_eq!(
            fetch_cache_directory_path(&e, None, cwd).path,
            PathBuf::from("/home/u/c")
        );
        let e = env(&[("FNPM_CACHE_DIR", "rel")]);
        assert_eq!(
            fetch_cache_directory_path(&e, None, cwd).path,
            PathBuf::from("/work/app/rel")
        );
        let none = env(&[]);
        let d = fetch_cache_directory_path(&none, None, cwd);
        assert_eq!(
            d,
            CacheDir {
                path: PathBuf::from("/work/app/node_modules/.fnpm-cache"),
                is_cwd_fallback: true
            }
        );
    }

    #[test]
    fn refuses_home_and_project() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let project = home.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let cache = home.join(".fnpm/cache");
        std::fs::create_dir_all(&cache).unwrap();
        let protected = default_protected(Some(&home), None, None, Some(&project), &project);
        assert_eq!(protected_in(&home, &protected), Some("the home directory"));
        assert_eq!(
            protected_in(&project, &protected),
            Some("the project directory")
        );
        assert_eq!(protected_in(&cache, &protected), None);
        #[cfg(unix)]
        {
            let link = t.path().join("link-to-home");
            std::os::unix::fs::symlink(&home, &link).unwrap();
            assert_eq!(protected_in(&link, &protected), Some("the home directory"));
        }
        let err = clear_cache(&ClearOptions {
            dirs: vec![home.clone()],
            protected: protected.clone(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("refusing to clear"), "{err}");
        assert!(project.exists());
    }

    #[test]
    fn clears_contents_only_and_dry_run_keeps_them() {
        let t = tempfile::tempdir().unwrap();
        let cache = t.path().join("cache");
        std::fs::create_dir_all(cache.join("links/x")).unwrap();
        std::fs::write(cache.join("metadata"), "m").unwrap();
        let tmp = t.path().join("tmp");
        let root = crate::dlx::cache_root_name();
        std::fs::create_dir_all(tmp.join(&root).join("pkg@1")).unwrap();
        std::fs::create_dir_all(tmp.join("unrelated")).unwrap();
        let opts = ClearOptions {
            dirs: vec![cache.clone()],
            protected: vec![],
            dry_run: true,
            temp_dir: Some(tmp.clone()),
        };
        let r = clear_cache(&opts).unwrap();
        assert!(r.had_global_store);
        assert_eq!(r.dlx_packages, 1);
        assert!(cache.join("metadata").exists());
        assert!(
            r.lines()
                .iter()
                .any(|l| l.starts_with("dry run: would delete 'fnpm install' cache at"))
        );
        let r = clear_cache(&ClearOptions {
            dry_run: false,
            ..opts
        })
        .unwrap();
        assert_eq!(r.cleared, vec![cache.clone()]);
        assert!(cache.exists());
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        assert!(!tmp.join(&root).exists());
        assert!(tmp.join("unrelated").exists());
        assert!(
            r.lines()
                .contains(&"Cleared 1 cached 'fnpmx' packages".to_owned())
        );
    }
}
