//! The no-op fast path: `<cache>/projects/<project>/state` ([`meta_dir`]).
//!
//! Stored in the configured cache so the hoisted package tree needs no `.fnpm` directory.
//!
//! If the lockfile, every workspace package.json, the effective config and the
//! `node_modules` directory are unchanged since the last successful install, `fnpm install`
//! exits after a handful of `stat`s and reads. Runs before the workspace and the lockfile are
//! loaded; the only work before it is the project-root lookup and `Config::load`.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The state file in the meta directory ([`meta_dir`]).
pub const STATE_FILE: &str = "state";

/// Project metadata (state and parsed lockfile) in `<cache>/projects/<xxh3 of the project path>`.
/// Usually outside the project; `cache = false` puts the cache in `node_modules/.cache`.
/// Later writes inside this directory leave the recorded package-tree identities unchanged.
pub fn meta_dir(cache_dir: &Path, project_dir: &Path) -> std::path::PathBuf {
    let key = xxhash_rust::xxh3::xxh3_64(project_dir.to_string_lossy().as_bytes());
    cache_dir.join("projects").join(format!("{key:016x}"))
}

/// The bytes of the project's lockfile: fnpm.lock, else the bun.lockb a project keeps. A
/// project without dependencies has none (bun deletes the empty lockfile): no bytes then.
pub(crate) fn lockfile_bytes(project_dir: &Path) -> Option<Vec<u8>> {
    match std::fs::read(project_dir.join(fnpm_lockfile::FNPM_LOCK))
        .or_else(|_| std::fs::read(project_dir.join(fnpm_lockfile::BUN_LOCKB)))
    {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(Vec::new()),
        Err(_) => None,
    }
}

/// `dir_identity`, or zeros when the directory does not exist (the hoisted layout has no
/// `node_modules/.fnpm`).
fn identity_or_zero(path: &Path) -> io::Result<(u64, u64, i128)> {
    match dir_identity(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((0, 0, 0)),
        other => other,
    }
}
/// Bumped when the meaning of a field changes; a different version is never a no-op.
pub const STATE_VERSION: u32 = 5;

/// Written last by a successful install; JSON (small, debuggable).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    /// [`STATE_VERSION`].
    pub version: u32,
    /// The fnpm version that wrote the state.
    pub fnpm_version: String,
    /// xxh3 of fnpm.lock bytes.
    pub lockfile_hash: u64,
    /// `(path relative to root, size, mtime ns, xxh3 of contents)` per package.json (and
    /// `pnpm-workspace.yaml` when present).
    pub manifests: Vec<(String, u64, i128, u64)>,
    /// xxh3 over the serialized install-relevant config (linker, backend, omit, platform,
    /// registry URLs, ignoreScripts, production, globalStore).
    pub config_hash: u64,
    /// `(dev, ino, mtime ns)` of `node_modules` after the install finished.
    pub node_modules: (u64, u64, i128),
    /// The same for `node_modules/.fnpm` (the isolated layout's entries live there: a deleted
    /// entry does not touch `node_modules` itself).
    #[serde(default)]
    pub virtual_store: (u64, u64, i128),
    /// `hoisted` or `isolated`.
    pub linker: String,
    /// Packages in the lockfile graph (the no-op summary's `across N packages`).
    pub packages: u32,
    /// Places in `node_modules` (a package copied to two places counts twice, a workspace
    /// link counts): the no-op summary's `Checked N installs`, as bun counts them.
    #[serde(default)]
    pub installs: u32,
    /// The `workspaces` patterns (root package.json or pnpm-workspace.yaml).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_patterns: Vec<String>,
    /// Every directory the patterns selected ([`fnpm_workspace::Workspace::candidate_dirs`]).
    /// A directory that appears, disappears or gains a package.json is a new or removed
    /// member, which no manifest entry would show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_dirs: Vec<String>,
    /// Bin files of workspace packages that did not exist (a build writes them): once one
    /// does, the next install links it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_bins: Vec<String>,
}

/// The result of [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Nothing changed; report `checked` counts.
    Noop {
        /// Packages in the graph.
        packages: u32,
        /// Places in `node_modules` ([`State::installs`]).
        installs: u32,
    },
    /// Something changed; carry the parsed state so the install can reuse the hashes.
    Changed(Option<Box<State>>),
}

/// `(size, mtime ns)` of a file.
fn size_mtime(meta: &std::fs::Metadata) -> (u64, i128) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i128);
    (meta.len(), mtime)
}

/// `(dev, ino, mtime ns)` of a directory (dev/ino are 0 where the platform has none).
pub fn dir_identity(path: &Path) -> io::Result<(u64, u64, i128)> {
    let meta = std::fs::metadata(path)?;
    let (_, mtime) = size_mtime(&meta);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((meta.dev(), meta.ino(), mtime))
    }
    #[cfg(not(unix))]
    {
        Ok((0, 0, mtime))
    }
}

/// The state entry of one manifest file (`rel` is relative to `project_dir`, `/`-separated).
pub fn manifest_entry(project_dir: &Path, rel: &str) -> io::Result<(String, u64, i128, u64)> {
    let path = project_dir.join(rel);
    let bytes = std::fs::read(&path)?;
    let meta = std::fs::metadata(&path)?;
    let (size, mtime) = size_mtime(&meta);
    Ok((
        rel.to_owned(),
        size,
        mtime,
        xxhash_rust::xxh3::xxh3_64(&bytes),
    ))
}

/// Reads and parses the state file of the meta directory `meta`; `None` when it is missing or
/// unreadable.
pub fn read(meta: &Path) -> Option<State> {
    let bytes = std::fs::read(meta.join(STATE_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Compares `STATE_FILE` with the current files. Package.json contents are only hashed when
/// size or mtime differ.
pub fn check(project_dir: &Path, meta: &Path, config_hash: u64) -> Check {
    let Some(state) = read(meta) else {
        return Check::Changed(None);
    };
    if is_unchanged(project_dir, config_hash, &state) {
        Check::Noop {
            packages: state.packages,
            installs: state.installs,
        }
    } else {
        Check::Changed(Some(Box::new(state)))
    }
}

/// Whether `node_modules` is exactly what the install of the lockfile with hash
/// `lockfile_hash` left (same fnpm, same config, `node_modules` and `node_modules/.fnpm`
/// untouched since), with the linker `linker`. Then the linker may trust every package that
/// stays at the same place without reading it: the same test the no-op path relies on.
/// Must be called before [`remove`].
pub fn layout_intact(
    project_dir: &Path,
    meta: &Path,
    config_hash: u64,
    lockfile_hash: u64,
    linker: &str,
) -> bool {
    let Some(state) = read(meta) else {
        return false;
    };
    let nm = project_dir.join("node_modules");
    state.version == STATE_VERSION
        && state.fnpm_version == crate::version()
        && state.config_hash == config_hash
        && state.lockfile_hash == lockfile_hash
        && state.linker == linker
        && dir_identity(&nm).is_ok_and(|id| id == state.node_modules)
        && identity_or_zero(&nm.join(".fnpm")).is_ok_and(|id| id == state.virtual_store)
}

fn is_unchanged(project_dir: &Path, config_hash: u64, state: &State) -> bool {
    if state.version != STATE_VERSION
        || state.fnpm_version != crate::version()
        || state.config_hash != config_hash
    {
        return false;
    }
    let nm = project_dir.join("node_modules");
    match dir_identity(&nm) {
        Ok(id) if id == state.node_modules => {}
        _ => return false,
    }
    match identity_or_zero(&nm.join(".fnpm")) {
        Ok(id) if id == state.virtual_store => {}
        _ => return false,
    }
    match lockfile_bytes(project_dir) {
        Some(bytes) if fnpm_lockfile::content_hash(&bytes) == state.lockfile_hash => {}
        _ => return false,
    }
    // A pnpm-workspace.yaml that appeared since the install changes the workspace.
    let has_pnpm_ws = state.manifests.iter().any(|m| m.0 == PNPM_WORKSPACE);
    if !has_pnpm_ws && project_dir.join(PNPM_WORKSPACE).exists() {
        return false;
    }
    let manifests_unchanged = state.manifests.iter().all(|(rel, size, mtime, hash)| {
        let path = project_dir.join(rel);
        let Ok(meta) = std::fs::metadata(&path) else {
            return false;
        };
        if size_mtime(&meta) == (*size, *mtime) {
            return true;
        }
        std::fs::read(&path).is_ok_and(|b| xxhash_rust::xxh3::xxh3_64(&b) == *hash)
    });
    manifests_unchanged
        && members_unchanged(project_dir, state)
        && !state
            .missing_bins
            .iter()
            .any(|rel| project_dir.join(rel).exists())
}

/// The workspace globs select the same directories, and none of them gained a package.json.
fn members_unchanged(project_dir: &Path, state: &State) -> bool {
    if state.workspace_patterns.is_empty() {
        return true;
    }
    match fnpm_workspace::Workspace::candidate_dirs(project_dir, &state.workspace_patterns) {
        Ok(dirs) if dirs == state.workspace_dirs => {}
        _ => return false,
    }
    state.workspace_dirs.iter().all(|dir| {
        let rel = format!("{dir}/package.json");
        state.manifests.iter().any(|m| m.0 == rel) || !project_dir.join(&rel).exists()
    })
}

/// The pnpm workspace file, part of the manifest set when present.
pub const PNPM_WORKSPACE: &str = "pnpm-workspace.yaml";

/// Writes the state file atomically. Creates the metadata directory and removes legacy
/// metadata before recording package-tree identities, so these writes do not invalidate it.
pub fn write(project_dir: &Path, meta: &Path, state: &State) -> io::Result<()> {
    let path = meta.join(STATE_FILE);
    let dir = meta;
    std::fs::create_dir_all(dir)?;
    remove_legacy_metadata(project_dir);
    let mut state = state.clone();
    state.node_modules = dir_identity(&project_dir.join("node_modules"))?;
    state.virtual_store = identity_or_zero(&project_dir.join("node_modules/.fnpm"))?;
    let bytes = serde_json::to_vec(&state).map_err(io::Error::other)?;
    let tmp = dir.join(format!(".state-{}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Deletes the state file (at the start of an install that changes `node_modules`).
pub fn remove(project_dir: &Path, meta: &Path) {
    let _ = std::fs::remove_file(meta.join(STATE_FILE));
    remove_legacy_metadata(project_dir);
}

/// Remove only old bookkeeping files and empty directories, preserving isolated packages.
fn remove_legacy_metadata(project_dir: &Path) {
    let node_modules = project_dir.join("node_modules");
    let virtual_store = node_modules.join(".fnpm");
    let real_dir =
        |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir());
    // Bookkeeping from an older install is optional. Never follow a planted parent link
    // while deleting it: the target may belong to another project or to the user.
    if !real_dir(&node_modules) || !real_dir(&virtual_store) {
        return;
    }
    let state_dir = virtual_store.join(".state");
    if real_dir(&state_dir) {
        let _ = std::fs::remove_file(state_dir.join("state"));
        let _ = std::fs::remove_dir(&state_dir);
    }
    let removed = [virtual_store.join("state"), virtual_store.join("lock.bin")]
        .map(|file| std::fs::remove_file(file).is_ok());
    let legacy = removed.contains(&true);
    // An empty `.fnpm` without those files is an isolated tree with no store entry (only
    // workspaces and `link:` packages): it stays, so the next install knows the layout.
    if legacy {
        let _ = std::fs::remove_dir(virtual_store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"app"}"#).unwrap();
        std::fs::write(dir.path().join("fnpm.lock"), "{}\n").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        dir
    }

    fn state_for(dir: &Path) -> State {
        State {
            missing_bins: Vec::new(),
            version: STATE_VERSION,
            fnpm_version: crate::version().to_owned(),
            lockfile_hash: fnpm_lockfile::content_hash(b"{}\n"),
            manifests: vec![manifest_entry(dir, "package.json").unwrap()],
            config_hash: 7,
            node_modules: (0, 0, 0),
            virtual_store: (0, 0, 0),
            linker: "hoisted".into(),
            packages: 3,
            installs: 4,
            workspace_patterns: Vec::new(),
            workspace_dirs: Vec::new(),
        }
    }

    #[test]
    fn hoisted_layout_trust_uses_cached_metadata_without_virtual_store() {
        let p = project();
        let cache = tempfile::tempdir().unwrap();
        let meta = meta_dir(cache.path(), p.path());
        let mut state = state_for(p.path());
        state.linker = "hoisted".into();
        state.virtual_store = (0, 0, 0);
        state.node_modules = dir_identity(&p.path().join("node_modules")).unwrap();
        write(p.path(), &meta, &state).unwrap();
        assert!(layout_intact(
            p.path(),
            &meta,
            state.config_hash,
            state.lockfile_hash,
            "hoisted"
        ));
        assert!(!layout_intact(
            p.path(),
            &meta,
            state.config_hash.wrapping_add(1),
            state.lockfile_hash,
            "hoisted"
        ));
        std::fs::create_dir(p.path().join("node_modules/new-entry")).unwrap();
        // Some filesystems coarsen directory mtimes; make the observed mutation deterministic.
        let modules = std::fs::File::open(p.path().join("node_modules")).unwrap();
        modules
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
            )
            .unwrap();
        assert!(!layout_intact(
            p.path(),
            &meta,
            state.config_hash,
            state.lockfile_hash,
            "hoisted"
        ));
    }

    #[test]
    fn unchanged_project_is_a_noop() {
        for meta_path in ["meta", "node_modules/.cache/projects/test"] {
            let dir = project();
            let meta = dir.path().join(meta_path);
            write(dir.path(), &meta, &state_for(dir.path())).unwrap();
            assert_eq!(
                check(dir.path(), &meta, 7),
                Check::Noop {
                    packages: 3,
                    installs: 4
                }
            );
        }
    }

    #[test]
    fn legacy_metadata_cleanup_preserves_isolated_packages_and_noop() {
        for isolated in [false, true] {
            let dir = project();
            let p = dir.path();
            let meta = p.join("meta");
            let virtual_store = p.join("node_modules/.fnpm");
            std::fs::create_dir_all(virtual_store.join(".state")).unwrap();
            for file in [".state/state", "state", "lock.bin"] {
                std::fs::write(virtual_store.join(file), "old metadata").unwrap();
            }
            if isolated {
                std::fs::create_dir(virtual_store.join("a@1.0.0")).unwrap();
            }
            write(p, &meta, &state_for(p)).unwrap();
            assert_eq!(virtual_store.exists(), isolated);
            if isolated {
                assert!(virtual_store.join("a@1.0.0").is_dir());
                assert_eq!(std::fs::read_dir(&virtual_store).unwrap().count(), 1);
            }
            assert!(matches!(check(p, &meta, 7), Check::Noop { .. }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_metadata_cleanup_does_not_follow_virtual_store_symlinks() {
        for symlink_state in [false, true] {
            let dir = project();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("state"), "keep").unwrap();
            std::fs::write(outside.path().join("lock.bin"), "keep").unwrap();
            let virtual_store = dir.path().join("node_modules/.fnpm");
            let link = if symlink_state {
                std::fs::create_dir(&virtual_store).unwrap();
                virtual_store.join(".state")
            } else {
                virtual_store
            };
            std::os::unix::fs::symlink(outside.path(), link).unwrap();
            remove(dir.path(), &dir.path().join("meta"));
            assert_eq!(
                std::fs::read_to_string(outside.path().join("state")).unwrap(),
                "keep"
            );
            assert_eq!(
                std::fs::read_to_string(outside.path().join("lock.bin")).unwrap(),
                "keep"
            );
        }
    }

    #[test]
    fn every_input_invalidates() {
        let dir = project();
        let p = dir.path();
        write(p, &p.join("meta"), &state_for(p)).unwrap();
        assert!(
            matches!(check(p, &p.join("meta"), 8), Check::Changed(Some(_))),
            "config"
        );

        std::fs::write(p.join("fnpm.lock"), "{ }\n").unwrap();
        assert!(
            matches!(check(p, &p.join("meta"), 7), Check::Changed(_)),
            "lockfile"
        );
        std::fs::write(p.join("fnpm.lock"), "{}\n").unwrap();
        assert_eq!(
            check(p, &p.join("meta"), 7),
            Check::Noop {
                packages: 3,
                installs: 4
            }
        );

        std::fs::write(p.join("package.json"), r#"{"name":"app2"}"#).unwrap();
        assert!(
            matches!(check(p, &p.join("meta"), 7), Check::Changed(_)),
            "manifest"
        );
        std::fs::write(p.join("package.json"), r#"{"name":"app"}"#).unwrap();
        // Same content, new mtime: hashed and accepted.
        assert_eq!(
            check(p, &p.join("meta"), 7),
            Check::Noop {
                packages: 3,
                installs: 4
            }
        );

        std::fs::create_dir(p.join("node_modules/left-pad")).unwrap();
        assert!(
            matches!(check(p, &p.join("meta"), 7), Check::Changed(_)),
            "node_modules"
        );
        write(p, &p.join("meta"), &state_for(p)).unwrap();
        assert_eq!(
            check(p, &p.join("meta"), 7),
            Check::Noop {
                packages: 3,
                installs: 4
            }
        );

        // An isolated entry deleted from the virtual store.
        std::fs::create_dir_all(p.join("node_modules/.fnpm/a@1.0.0")).unwrap();
        write(p, &p.join("meta"), &state_for(p)).unwrap();
        assert_eq!(
            check(p, &p.join("meta"), 7),
            Check::Noop {
                packages: 3,
                installs: 4
            }
        );
        // Directory mtimes come from the coarse kernel clock (a few ms per tick).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::remove_dir(p.join("node_modules/.fnpm/a@1.0.0")).unwrap();
        assert!(
            matches!(check(p, &p.join("meta"), 7), Check::Changed(_)),
            "virtual store"
        );
        write(p, &p.join("meta"), &state_for(p)).unwrap();
        assert_eq!(
            check(p, &p.join("meta"), 7),
            Check::Noop {
                packages: 3,
                installs: 4
            }
        );

        std::fs::write(p.join(PNPM_WORKSPACE), "packages: []\n").unwrap();
        assert!(
            matches!(check(p, &p.join("meta"), 7), Check::Changed(_)),
            "pnpm workspace"
        );
    }

    #[test]
    fn new_workspace_member_is_a_change() {
        let dir = project();
        let p = dir.path();
        std::fs::create_dir_all(p.join("packages/a")).unwrap();
        std::fs::write(p.join("packages/a/package.json"), r#"{"name":"a"}"#).unwrap();
        std::fs::create_dir_all(p.join("packages/empty")).unwrap();
        let patterns = vec!["packages/*".to_owned()];
        let mut s = state_for(p);
        s.manifests
            .push(manifest_entry(p, "packages/a/package.json").unwrap());
        s.workspace_dirs = fnpm_workspace::Workspace::candidate_dirs(p, &patterns).unwrap();
        assert_eq!(s.workspace_dirs, vec!["packages/a", "packages/empty"]);
        s.workspace_patterns = patterns;
        write(p, &p.join("meta"), &s).unwrap();
        assert_eq!(
            check(p, &p.join("meta"), 7),
            Check::Noop {
                packages: 3,
                installs: 4
            }
        );

        // A directory without package.json gains one: a new member.
        std::fs::write(p.join("packages/empty/package.json"), r#"{"name":"e"}"#).unwrap();
        assert!(
            matches!(check(p, &p.join("meta"), 7), Check::Changed(_)),
            "package.json added"
        );
        std::fs::remove_file(p.join("packages/empty/package.json")).unwrap();
        assert_eq!(
            check(p, &p.join("meta"), 7),
            Check::Noop {
                packages: 3,
                installs: 4
            }
        );

        // A new directory matching the glob.
        std::fs::create_dir_all(p.join("packages/b")).unwrap();
        std::fs::write(p.join("packages/b/package.json"), r#"{"name":"b"}"#).unwrap();
        assert!(
            matches!(check(p, &p.join("meta"), 7), Check::Changed(_)),
            "new member dir"
        );
    }

    #[test]
    fn missing_or_foreign_state_is_changed() {
        let dir = project();
        let meta = dir.path().join("meta");
        assert_eq!(
            check(dir.path(), &dir.path().join("meta"), 7),
            Check::Changed(None)
        );
        let mut s = state_for(dir.path());
        s.version = STATE_VERSION - 1;
        write(dir.path(), &meta, &s).unwrap();
        assert!(matches!(
            check(dir.path(), &meta, 7),
            Check::Changed(Some(_))
        ));
        s.version = STATE_VERSION;
        s.fnpm_version = "0.0.0-other".into();
        write(dir.path(), &dir.path().join("meta"), &s).unwrap();
        assert!(matches!(
            check(dir.path(), &dir.path().join("meta"), 7),
            Check::Changed(Some(_))
        ));
        remove(dir.path(), &dir.path().join("meta"));
        assert_eq!(
            check(dir.path(), &dir.path().join("meta"), 7),
            Check::Changed(None)
        );
    }
}
