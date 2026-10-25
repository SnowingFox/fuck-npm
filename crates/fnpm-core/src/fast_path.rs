//! The no-op fast path: `node_modules/.fnpm/.state/state`.
//!
//! If the lockfile, every workspace package.json, the effective config and the
//! `node_modules` directory are unchanged since the last successful install, `fnpm install`
//! exits after a handful of `stat`s and reads. Runs before the workspace and the lockfile are
//! loaded; the only work before it is the project-root lookup and `Config::load`.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Where the state lives, relative to the project root. In a directory of its own, so writing it
/// changes neither `node_modules` nor `node_modules/.fnpm` (both identities are recorded).
pub const STATE_FILE: &str = "node_modules/.fnpm/.state/state";

/// The state file of fnpm versions before `.state/` (deleted with the current one).
const LEGACY_STATE_FILE: &str = "node_modules/.fnpm/state";
/// Bumped when the meaning of a field changes; a different version is never a no-op.
pub const STATE_VERSION: u32 = 3;

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
    /// Packages in the lockfile graph (the no-op summary's `checked` count).
    pub packages: u32,
    /// The `workspaces` patterns (root package.json or pnpm-workspace.yaml).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_patterns: Vec<String>,
    /// Every directory the patterns selected ([`fnpm_workspace::Workspace::candidate_dirs`]).
    /// A directory that appears, disappears or gains a package.json is a new or removed
    /// member, which no manifest entry would show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_dirs: Vec<String>,
}

/// The result of [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Nothing changed; report `checked` counts.
    Noop {
        /// Packages in the graph.
        packages: u32,
    },
    /// Something changed; carry the parsed state so the install can reuse the hashes.
    Changed(Option<State>),
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

/// Reads and parses the state file; `None` when it is missing or unreadable.
pub fn read(project_dir: &Path) -> Option<State> {
    let bytes = std::fs::read(project_dir.join(STATE_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Compares `STATE_FILE` with the current files. Package.json contents are only hashed when
/// size or mtime differ.
pub fn check(project_dir: &Path, config_hash: u64) -> Check {
    let Some(state) = read(project_dir) else {
        return Check::Changed(None);
    };
    if is_unchanged(project_dir, config_hash, &state) {
        Check::Noop {
            packages: state.packages,
        }
    } else {
        Check::Changed(Some(state))
    }
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
    match dir_identity(&nm.join(".fnpm")) {
        Ok(id) if id == state.virtual_store => {}
        _ => return false,
    }
    match std::fs::read(project_dir.join(fnpm_lockfile::FNPM_LOCK)) {
        Ok(bytes) if fnpm_lockfile::content_hash(&bytes) == state.lockfile_hash => {}
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
    manifests_unchanged && members_unchanged(project_dir, state)
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

/// Writes the state file atomically. `state.node_modules` is replaced by the identity of
/// `node_modules` taken after `node_modules/.fnpm` exists, so writing the file itself does not
/// invalidate it.
pub fn write(project_dir: &Path, state: &State) -> io::Result<()> {
    let path = project_dir.join(STATE_FILE);
    let dir = path.parent().expect("STATE_FILE has a parent");
    std::fs::create_dir_all(dir)?;
    let mut state = state.clone();
    state.node_modules = dir_identity(&project_dir.join("node_modules"))?;
    state.virtual_store = dir_identity(&project_dir.join("node_modules/.fnpm"))?;
    let _ = std::fs::remove_file(project_dir.join(LEGACY_STATE_FILE));
    let bytes = serde_json::to_vec(&state).map_err(io::Error::other)?;
    let tmp = dir.join(format!(".state-{}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Deletes the state file (at the start of an install that changes `node_modules`).
pub fn remove(project_dir: &Path) {
    let _ = std::fs::remove_file(project_dir.join(STATE_FILE));
    let _ = std::fs::remove_file(project_dir.join(LEGACY_STATE_FILE));
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
            version: STATE_VERSION,
            fnpm_version: crate::version().to_owned(),
            lockfile_hash: fnpm_lockfile::content_hash(b"{}\n"),
            manifests: vec![manifest_entry(dir, "package.json").unwrap()],
            config_hash: 7,
            node_modules: (0, 0, 0),
            virtual_store: (0, 0, 0),
            linker: "hoisted".into(),
            packages: 3,
            workspace_patterns: Vec::new(),
            workspace_dirs: Vec::new(),
        }
    }

    #[test]
    fn unchanged_project_is_a_noop() {
        let dir = project();
        write(dir.path(), &state_for(dir.path())).unwrap();
        assert_eq!(check(dir.path(), 7), Check::Noop { packages: 3 });
    }

    #[test]
    fn every_input_invalidates() {
        let dir = project();
        let p = dir.path();
        write(p, &state_for(p)).unwrap();
        assert!(matches!(check(p, 8), Check::Changed(Some(_))), "config");

        std::fs::write(p.join("fnpm.lock"), "{ }\n").unwrap();
        assert!(matches!(check(p, 7), Check::Changed(_)), "lockfile");
        std::fs::write(p.join("fnpm.lock"), "{}\n").unwrap();
        assert_eq!(check(p, 7), Check::Noop { packages: 3 });

        std::fs::write(p.join("package.json"), r#"{"name":"app2"}"#).unwrap();
        assert!(matches!(check(p, 7), Check::Changed(_)), "manifest");
        std::fs::write(p.join("package.json"), r#"{"name":"app"}"#).unwrap();
        // Same content, new mtime: hashed and accepted.
        assert_eq!(check(p, 7), Check::Noop { packages: 3 });

        std::fs::create_dir(p.join("node_modules/left-pad")).unwrap();
        assert!(matches!(check(p, 7), Check::Changed(_)), "node_modules");
        write(p, &state_for(p)).unwrap();
        assert_eq!(check(p, 7), Check::Noop { packages: 3 });

        // An isolated entry deleted from the virtual store.
        std::fs::create_dir(p.join("node_modules/.fnpm/a@1.0.0")).unwrap();
        write(p, &state_for(p)).unwrap();
        assert_eq!(check(p, 7), Check::Noop { packages: 3 });
        // Directory mtimes come from the coarse kernel clock (a few ms per tick).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::remove_dir(p.join("node_modules/.fnpm/a@1.0.0")).unwrap();
        assert!(matches!(check(p, 7), Check::Changed(_)), "virtual store");
        write(p, &state_for(p)).unwrap();
        assert_eq!(check(p, 7), Check::Noop { packages: 3 });

        std::fs::write(p.join(PNPM_WORKSPACE), "packages: []\n").unwrap();
        assert!(matches!(check(p, 7), Check::Changed(_)), "pnpm workspace");
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
        write(p, &s).unwrap();
        assert_eq!(check(p, 7), Check::Noop { packages: 3 });

        // A directory without package.json gains one: a new member.
        std::fs::write(p.join("packages/empty/package.json"), r#"{"name":"e"}"#).unwrap();
        assert!(
            matches!(check(p, 7), Check::Changed(_)),
            "package.json added"
        );
        std::fs::remove_file(p.join("packages/empty/package.json")).unwrap();
        assert_eq!(check(p, 7), Check::Noop { packages: 3 });

        // A new directory matching the glob.
        std::fs::create_dir_all(p.join("packages/b")).unwrap();
        std::fs::write(p.join("packages/b/package.json"), r#"{"name":"b"}"#).unwrap();
        assert!(matches!(check(p, 7), Check::Changed(_)), "new member dir");
    }

    #[test]
    fn missing_or_foreign_state_is_changed() {
        let dir = project();
        assert_eq!(check(dir.path(), 7), Check::Changed(None));
        let mut s = state_for(dir.path());
        s.fnpm_version = "0.0.0-other".into();
        write(dir.path(), &s).unwrap();
        assert!(matches!(check(dir.path(), 7), Check::Changed(Some(_))));
        remove(dir.path());
        assert_eq!(check(dir.path(), 7), Check::Changed(None));
    }
}
