//! Where `fnpm x` installs packages (bun `runtime/cli/bunx_command.rs`).
//!
//! `<cache>/.fnpmx-<uid>/`, owned by this user with mode 0700, is the trust boundary: nothing
//! below it is checked again. When the install cache cannot hold one, the system temp dir is
//! tried, with a warning, because other users can see it. On Windows (`GetTempPath` is per
//! user) trees are `%TEMP%\fnpmx-<id>-<tree>`.
//!
//! A tree is named after the package and every option that changes its contents:
//! `<name>@<version>[+trust][+ignore-scripts][+min-age=<s>][+registry=<hash>]`, or
//! `<bin>@<tag>@<hash>` when the request holds characters that cannot be part of `$PATH`.

use std::path::{Path, PathBuf};

/// `.fnpmx-<uid>`.
pub fn cache_root_name() -> String {
    format!(".fnpmx-{}", user_id())
}

#[cfg(unix)]
fn user_id() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(not(unix))]
fn user_id() -> u32 {
    let user = std::env::var("USERNAME").unwrap_or_default();
    xxhash_rust::xxh3::xxh3_64(user.as_bytes()) as u32
}

/// How many packages are in `<base>/.fnpmx-<uid>`; the lock files beside the trees are not
/// counted.
pub fn count_cached_packages(base: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(base.join(cache_root_name())) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_file()))
        .count()
}

/// A tree directory for `fnpm x`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DlxDir {
    /// `<root>/<tree name>`.
    pub path: PathBuf,
    /// The root is in the shared temp dir; the caller prints [`DlxDir::SHARED_WARNING`].
    pub shared: bool,
}

impl DlxDir {
    /// bun's warning when the private cache dir cannot be used.
    pub const SHARED_WARNING: &'static str = "no usable private cache directory, installing into shared temp directory. Set FNPM_CACHE_DIR to a directory only you can write.";
}

#[cfg(unix)]
fn is_private_dir(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_dir() && m.uid() == user_id() && m.mode() & 0o077 == 0)
}

/// The tree directory for `tree_name`: below the install cache (`None` if it is only the
/// working-directory fallback), else below `temp_dir`. With `create`, the root is made with
/// mode 0700. `None` when neither root is private to this user.
#[cfg(unix)]
pub fn private_cache_dir(
    install_cache: Option<&Path>,
    temp_dir: &Path,
    tree_name: &str,
    create: bool,
) -> Option<DlxDir> {
    use std::os::unix::fs::DirBuilderExt;
    let root_name = cache_root_name();
    for (base, shared) in [(install_cache, false), (Some(temp_dir), true)] {
        let Some(base) = base else { continue };
        let root = base.join(&root_name);
        if create {
            let _ = std::fs::create_dir_all(base);
            let _ = std::fs::DirBuilder::new().mode(0o700).create(&root);
        }
        if !is_private_dir(&root) {
            continue;
        }
        return Some(DlxDir {
            path: root.join(tree_name),
            shared,
        });
    }
    None
}

/// The tree directory for `tree_name` (Windows: the per-user temp dir).
#[cfg(not(unix))]
pub fn private_cache_dir(
    _install_cache: Option<&Path>,
    temp_dir: &Path,
    tree_name: &str,
    _create: bool,
) -> Option<DlxDir> {
    Some(DlxDir {
        path: temp_dir.join(format!("fnpmx-{}-{tree_name}", user_id())),
        shared: false,
    })
}

/// What a tree is installed with.
#[derive(Debug, Clone, Default)]
pub struct TreeOptions<'a> {
    /// The requested package name (may be empty for a URL).
    pub name: &'a str,
    /// The version as typed, or `latest`.
    pub version: &'a str,
    /// The kind of version (`npm`, `git`, `tarball`, ...), used when the name is not usable.
    pub version_tag: &'a str,
    /// The bin that will run.
    pub bin_name: &'a str,
    /// `--trust`.
    pub trust: bool,
    /// `--ignore-scripts`.
    pub ignore_scripts: bool,
    /// `--minimum-release-age`, as typed.
    pub minimum_release_age: Option<&'a str>,
    /// `--registry`.
    pub registry: Option<&'a str>,
}

#[cfg(windows)]
const BANNED_PATH_CHARS: &[char] = &[':', '*', '?', '<', '>', '|', ';'];
#[cfg(not(windows))]
const BANNED_PATH_CHARS: &[char] = &[':'];

fn hash(s: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(s.as_bytes())
}

/// The tree directory name for a request (see the module docs).
pub fn tree_name(o: &TreeOptions<'_>) -> String {
    // `..` (`pkg@../folder`) would climb out of the root; `:` is the `$PATH` separator.
    let banned = [o.name, o.version]
        .iter()
        .any(|p| p.contains(BANNED_PATH_CHARS) || p.contains(".."));
    let mut name = if banned {
        format!(
            "{}@{}@{}",
            o.bin_name,
            o.version_tag,
            hash(o.name).wrapping_add(hash(o.version))
        )
    } else {
        format!("{}@{}", o.name, o.version)
    };
    if o.trust {
        name.push_str("+trust");
    }
    if o.ignore_scripts {
        name.push_str("+ignore-scripts");
    }
    if let Some(age) = o.minimum_release_age {
        name.push_str("+min-age=");
        name.push_str(age);
    }
    if let Some(registry) = o.registry {
        name.push_str(&format!("+registry={:x}", hash(registry)));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_names() {
        let base = TreeOptions {
            name: "cowsay",
            version: "latest",
            version_tag: "dist_tag",
            bin_name: "cowsay",
            ..Default::default()
        };
        assert_eq!(tree_name(&base), "cowsay@latest");
        let opts = TreeOptions {
            trust: true,
            ignore_scripts: true,
            minimum_release_age: Some("60"),
            ..base.clone()
        };
        assert_eq!(
            tree_name(&opts),
            "cowsay@latest+trust+ignore-scripts+min-age=60"
        );
        let reg = tree_name(&TreeOptions {
            registry: Some("https://r.example/"),
            ..base.clone()
        });
        assert!(reg.starts_with("cowsay@latest+registry="));
        let url = tree_name(&TreeOptions {
            name: "",
            version: "https://x.test/a.tgz",
            version_tag: "tarball",
            bin_name: "a",
            ..base.clone()
        });
        assert!(url.starts_with("a@tarball@"), "{url}");
        let dots = tree_name(&TreeOptions {
            name: "pkg",
            version: "../folder",
            version_tag: "folder",
            bin_name: "pkg",
            ..base
        });
        assert!(dots.starts_with("pkg@folder@"), "{dots}");
        assert!(!dots.contains(".."));
    }

    #[cfg(unix)]
    #[test]
    fn private_root_is_0700_and_falls_back_to_temp() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let cache = t.path().join("cache");
        let tmp = t.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let d = private_cache_dir(Some(&cache), &tmp, "x@1", true).unwrap();
        assert!(!d.shared);
        assert_eq!(d.path, cache.join(cache_root_name()).join("x@1"));
        let mode = std::fs::metadata(cache.join(cache_root_name()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        // A root others can write is not used.
        std::fs::set_permissions(
            cache.join(cache_root_name()),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let d = private_cache_dir(Some(&cache), &tmp, "x@1", true).unwrap();
        assert!(d.shared);
        assert_eq!(d.path, tmp.join(cache_root_name()).join("x@1"));
        assert!(private_cache_dir(None, &tmp, "y", false).is_some());
        std::fs::create_dir_all(cache.join(cache_root_name()).join("pkg-a")).unwrap();
        std::fs::write(cache.join(cache_root_name()).join("pkg-a.lock"), "").unwrap();
        assert_eq!(count_cached_packages(&cache), 1);
    }
}
