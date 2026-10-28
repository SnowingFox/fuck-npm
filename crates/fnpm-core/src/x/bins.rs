//! Which file `fnpm x` runs: the bin of a package (bun `get_bin_from_subpath`), found in the
//! project's `node_modules` (bun `find_in_project`) or in a cached tree (`find_in_cache`).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use fnpm_scripts::shell::{is_executable, which};
use serde_json::Value;

/// One entry of a package's `bin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageBin {
    /// The command name.
    pub name: String,
    /// The file, relative to the package directory.
    pub file: String,
}

/// Why no bin was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinError {
    /// The package.json was read and names no bin that can be linked.
    NoBin,
    /// The package.json could not be read or parsed.
    Unreadable,
    /// The path of the bin the package.json names does not fit in a path (bun `PathTooLong`).
    PathTooLong,
}

/// bun `MAX_PATH_BYTES`.
const MAX_PATH_BYTES: usize = if cfg!(target_vendor = "apple") {
    1024
} else {
    4096
};

/// `which` resolves a name with a slash against the cwd, not against `node_modules/.bin`.
fn is_safe_bin_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The bin of the package whose package.json is `package_json`: the one named `preferred` if
/// there is one, else the first that the bin linker would link.
pub fn bin_of(package_json: &Path, preferred: &str) -> Result<PackageBin, BinError> {
    let json = read_json(package_json).ok_or(BinError::Unreadable)?;
    let bin = |name: &str, file: &str| PackageBin {
        name: name.to_owned(),
        file: file.to_owned(),
    };
    match json.get("bin") {
        Some(Value::Object(map)) => {
            let mut first = None;
            for (name, target) in map {
                let Some(target) = target.as_str().filter(|t| {
                    !t.is_empty() && !fnpm_linker::bins::bin_target_escapes_package_dir(t)
                }) else {
                    continue;
                };
                if !is_safe_bin_name(name) {
                    continue;
                }
                if name == preferred {
                    return Ok(bin(name, target));
                }
                first.get_or_insert_with(|| bin(name, target));
            }
            if let Some(first) = first {
                return Ok(first);
            }
        }
        Some(Value::String(target))
            if !target.is_empty() && !fnpm_linker::bins::bin_target_escapes_package_dir(target) =>
        {
            // The command of a scoped `@scope/pkg` is its unscoped part.
            if let Some(name) = json
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
            {
                let name = name.rsplit('/').next().unwrap_or(name);
                if is_safe_bin_name(name) {
                    return Ok(bin(name, target));
                }
            }
        }
        _ => {}
    }
    if let Some(dir) = json
        .get("directories")
        .and_then(|d| d.get("bin"))
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty() && !fnpm_linker::bins::bin_target_escapes_package_dir(d))
    {
        let base = package_json.parent().unwrap_or(Path::new("."));
        let entries = std::fs::read_dir(base.join(dir)).map_err(|_| BinError::Unreadable)?;
        let mut first = None;
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.is_empty() {
                continue;
            }
            let file = format!("{dir}/{name}");
            if name == preferred {
                return Ok(bin(&name, &file));
            }
            first.get_or_insert_with(|| bin(&name, &file));
        }
        if let Some(first) = first {
            return Ok(first);
        }
    }
    Err(BinError::NoBin)
}

/// What the project has of the package that a request without a version names.
#[derive(Debug)]
pub enum InProject {
    /// Not there (or its bin links out of the package).
    Absent,
    /// Run this file.
    Run(PathBuf),
    /// The file of the bin has no execute bit.
    NotExecutable(PathBuf),
    /// `realpath` of the bin's file failed.
    Unresolved {
        /// The file.
        file: PathBuf,
        /// The error.
        error: std::io::Error,
        /// The package is in a `node_modules` (an install put it there).
        in_node_modules: bool,
    },
}

#[cfg(not(windows))]
fn is_inside(real: &Path, dir: &Path) -> bool {
    real.strip_prefix(dir)
        .is_ok_and(|rest| !rest.as_os_str().is_empty())
}

/// Finds the bin in the nearest `node_modules/<package>` from `cwd` up to `project`. Two
/// packages can share a bin name, so the `.bin` entry is used only if it resolves into the
/// package.
pub fn find_in_project(
    cwd: &Path,
    project: &Path,
    package: &str,
    bin_name: &str,
    only_that_bin: bool,
) -> Result<InProject, BinError> {
    let mut next = Some(cwd);
    while let Some(dir) = next {
        if !dir.starts_with(project) {
            break;
        }
        next = dir.parent();
        let node_modules = dir.join("node_modules");
        let package_dir = node_modules.join(package);
        let package_json = package_dir.join("package.json");
        let bin = match bin_of(&package_json, bin_name) {
            Ok(bin) => bin,
            Err(BinError::NoBin) => return Err(BinError::NoBin),
            Err(BinError::Unreadable | BinError::PathTooLong) => continue,
        };
        if only_that_bin && bin.name != bin_name {
            return Err(BinError::NoBin);
        }
        let entry = which(&bin.name, node_modules.join(".bin").as_os_str(), cwd);
        #[cfg(windows)]
        {
            return Ok(entry.map_or(InProject::Absent, InProject::Run));
        }
        #[cfg(not(windows))]
        {
            let file_path = package_dir.join(&bin.file);
            let file = is_executable(&file_path).then(|| file_path.clone());
            let Ok(real_dir) = std::fs::canonicalize(&package_dir) else {
                return Ok(InProject::Absent);
            };
            // The installer links the bin of a package with native builds into an optional
            // dependency, and skips the install script that replaces the package's own file.
            if let Some(entry) = entry
                && let Ok(real) = std::fs::canonicalize(&entry)
                && (is_inside(&real, &real_dir) || is_in_platform_package(&package_json, &real))
            {
                return Ok(InProject::Run(entry));
            }
            let Some(file) = file else {
                return Ok(match std::fs::canonicalize(&file_path) {
                    Ok(real) if is_inside(&real, &real_dir) => InProject::NotExecutable(file_path),
                    Ok(_) => InProject::Absent,
                    Err(error) => InProject::Unresolved {
                        file: file_path,
                        error,
                        in_node_modules: real_dir
                            .parent()
                            .is_some_and(|p| package_parent_is_node_modules(p, package)),
                    },
                });
            };
            return Ok(match std::fs::canonicalize(&file) {
                Ok(real) if is_inside(&real, &real_dir) => InProject::Run(file),
                _ => InProject::Absent,
            });
        }
    }
    Ok(InProject::Absent)
}

/// `real_dir` is `<parent>/<package>`: whether `<parent>` (minus a scope) is a `node_modules`.
#[cfg(not(windows))]
fn package_parent_is_node_modules(parent: &Path, package: &str) -> bool {
    let parent = if package.starts_with('@') {
        parent.parent()
    } else {
        Some(parent)
    };
    parent.and_then(Path::file_name) == Some(OsStr::new("node_modules"))
}

/// Whether `real` is in one of the `optionalDependencies` of the package at `package_json`
/// that has an `os` and a `cpu`.
#[cfg(not(windows))]
fn is_in_platform_package(package_json: &Path, real: &Path) -> bool {
    let Some(json) = read_json(package_json) else {
        return false;
    };
    let Some(Value::Object(optional)) = json.get("optionalDependencies") else {
        return false;
    };
    let real = real.to_string_lossy();
    let found = optional.keys().find_map(|name| {
        let needle = format!("/{name}/");
        let at = real.rfind(&needle)?;
        Some(PathBuf::from(format!(
            "{}/{name}/package.json",
            &real[..at]
        )))
    });
    found
        .and_then(|p| read_json(&p))
        .is_some_and(|dep| dep.get("os").is_some() && dep.get("cpu").is_some())
}

/// The state of a tree (bun `Tree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tree {
    /// Never installed.
    Missing,
    /// Unfinished, or older than a day.
    Stale,
    /// Installed less than a day ago.
    Fresh,
}

/// A tree is refreshed after a day (bun `SECONDS_CACHE_VALID`).
pub const SECONDS_CACHE_VALID: u64 = 60 * 60 * 24;

/// The state of the tree at `dir`, from its package.json.
pub fn tree_state(dir: &Path) -> Tree {
    let meta = match std::fs::metadata(dir.join("package.json")) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Tree::Missing,
        Err(_) => return Tree::Stale,
    };
    let age = meta
        .modified()
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
        .map_or(0, |d| d.as_secs());
    if meta.len() <= crate::options::DlxMode::UNFINISHED.len() as u64 || age > SECONDS_CACHE_VALID {
        Tree::Stale
    } else {
        Tree::Fresh
    }
}

/// Finds the bin in a tree. `package` is `None` with `--package`, which runs any command: the
/// tree's bins first, then `path`.
pub fn find_in_cache(
    tree: &Path,
    cwd: &Path,
    initial_bin_name: &str,
    package: Option<&str>,
    path: &OsStr,
) -> Result<Option<PathBuf>, BinError> {
    // The bin's name comes from the package's own package.json: one of its dependencies can
    // have a bin of the name the user typed.
    let own = package.map(|name| {
        bin_of(
            &tree.join("node_modules").join(name).join("package.json"),
            initial_bin_name,
        )
    });
    let bin_name = match &own {
        Some(Ok(bin)) => bin.name.as_str(),
        _ => initial_bin_name,
    };
    let candidate = tree
        .join("node_modules")
        .join(".bin")
        .join(format!("{bin_name}{}", std::env::consts::EXE_SUFFIX));
    if candidate.as_os_str().len() >= MAX_PATH_BYTES {
        return Err(BinError::PathTooLong);
    }
    if is_executable(&candidate) {
        return Ok(Some(candidate));
    }
    match own {
        Some(Err(BinError::NoBin)) => Err(BinError::NoBin),
        Some(_) => Ok(None),
        None => Ok(which(initial_bin_name, path, cwd)),
    }
}

/// `{CODE}: {description}` for an I/O error, like bun's `Output::err`.
pub fn errno_label(error: &std::io::Error) -> String {
    let (code, text) = match error.raw_os_error() {
        Some(2) => ("ENOENT", "No such file or directory"),
        Some(13) => ("EACCES", "Permission denied"),
        Some(20) => ("ENOTDIR", "Not a directory"),
        #[cfg(target_os = "linux")]
        Some(40) => ("ELOOP", "Too many levels of symbolic links"),
        #[cfg(target_vendor = "apple")]
        Some(62) => ("ELOOP", "Too many levels of symbolic links"),
        _ => return error.to_string(),
    };
    format!("{code}: {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn bin_rules() {
        let t = tempfile::tempdir().unwrap();
        let pj = t.path().join("package.json");
        write(
            &pj,
            r#"{"name":"many","bin":{"not-a-string":1,"empty":"","outside":"../../x.js","real":"cli.js","other":"o.js"}}"#,
        );
        assert_eq!(bin_of(&pj, "zzz").unwrap().name, "real");
        assert_eq!(bin_of(&pj, "other").unwrap().file, "o.js");
        write(&pj, r#"{"name":"@corp/tool","bin":"cli.js"}"#);
        assert_eq!(bin_of(&pj, "x").unwrap().name, "tool");
        for target in ["", "../outside", "/outside", "..\\outside", "C:\\outside"] {
            write(
                &pj,
                &serde_json::json!({ "name": "tool", "bin": target }).to_string(),
            );
            assert_eq!(bin_of(&pj, "tool"), Err(BinError::NoBin), "{target:?}");
        }
        write(&pj, r#"{"name":"none"}"#);
        assert_eq!(bin_of(&pj, "x"), Err(BinError::NoBin));
        write(&pj, r#"{"name":"d","directories":{"bin":"bin"}}"#);
        write(&t.path().join("bin/one"), "");
        assert_eq!(bin_of(&pj, "x").unwrap().file, "bin/one");
        write(&pj, r#"{"name":"d","directories":{"bin":"../out"}}"#);
        assert_eq!(bin_of(&pj, "x"), Err(BinError::NoBin));
        assert_eq!(
            bin_of(&t.path().join("missing.json"), "x"),
            Err(BinError::Unreadable)
        );
    }

    #[test]
    fn tree_states() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(tree_state(t.path()), Tree::Missing);
        std::fs::write(t.path().join("package.json"), "{}\n").unwrap();
        assert_eq!(tree_state(t.path()), Tree::Stale);
        std::fs::write(
            t.path().join("package.json"),
            r#"{"dependencies":{"a":"1"}}"#,
        )
        .unwrap();
        assert_eq!(tree_state(t.path()), Tree::Fresh);
    }
}
