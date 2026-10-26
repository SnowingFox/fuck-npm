//! `projects.json`, `store prune`, `store status` and `pm cache prune` (bun
//! `runtime/cli/pm_cache_prune_command.rs`).
//!
//! Reference counting: every install records its project with
//! [`Store::set_project_packages`](crate::Store::set_project_packages) (the `pkgs/` entries it
//! used); global-store entries are found by following the project's `node_modules/.fnpm/*`
//! symlinks into `links/`, and from there the symlinks between `links/` entries. A project
//! whose directory is gone is dropped. Entries younger than a grace period are always kept,
//! so a prune never races an install that has not recorded its packages yet. A pruned package
//! that is still hard linked into some `node_modules` keeps its files there; only the bytes
//! nothing else links to are reported as freed.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::{Error, PruneOptions, PruneReport, Result, Store, StoreStatus};

/// Entries modified more recently than this are never pruned.
const GRACE: Duration = Duration::from_secs(15 * 60);
const PROJECTS_FILE: &str = "projects.json";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Projects {
    version: u32,
    projects: BTreeMap<String, ProjectRecord>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRecord {
    /// `pkgs/` directory names; `None`: unknown, so every package is kept.
    #[serde(default)]
    packages: Option<Vec<String>>,
}

fn io_err(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// An exclusive advisory lock on `projects.json.lock`, released on drop.
struct ProjectsLock {
    _file: std::fs::File,
}

fn lock_projects(store: &Store) -> Result<ProjectsLock> {
    let path = store.root().join("projects.json.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_err(&path))?;
    #[cfg(unix)]
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).map_err(|e| Error::Io {
        path: path.clone(),
        source: e.into(),
    })?;
    Ok(ProjectsLock { _file: file })
}

fn read_projects(store: &Store) -> Projects {
    let path = store.root().join(PROJECTS_FILE);
    std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Projects {
            version: 1,
            projects: BTreeMap::new(),
        })
}

fn write_projects(store: &Store, projects: &Projects) -> Result<()> {
    let path = store.root().join(PROJECTS_FILE);
    let bytes = serde_json::to_vec_pretty(projects).expect("plain data serialises");
    crate::fsops::write_atomic(&path, &bytes).map_err(io_err(&path))
}

fn project_key(project_dir: &Path) -> String {
    let abs = std::fs::canonicalize(project_dir).unwrap_or_else(|_| {
        if project_dir.is_absolute() {
            project_dir.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|c| c.join(project_dir))
                .unwrap_or_else(|_| project_dir.to_path_buf())
        }
    });
    abs.to_string_lossy().into_owned()
}

pub(crate) fn register_project(
    store: &Store,
    project_dir: &Path,
    packages: Option<Vec<String>>,
) -> Result<()> {
    let key = project_key(project_dir);
    let _lock = lock_projects(store)?;
    let mut projects = read_projects(store);
    let packages = packages.map(|mut p| {
        p.sort_unstable();
        p.dedup();
        p
    });
    let existed = projects.projects.contains_key(&key);
    let record = projects.projects.entry(key).or_default();
    let next = ProjectRecord {
        packages: packages.or_else(|| record.packages.clone()),
    };
    if existed && *record == next && projects.version == 1 {
        return Ok(());
    }
    *record = next;
    projects.version = 1;
    write_projects(store, &projects)
}

fn older_than(meta: &std::fs::Metadata, cutoff: SystemTime) -> bool {
    meta.modified().is_ok_and(|m| m <= cutoff)
}

fn dir_names(dir: &Path) -> Vec<(String, std::fs::Metadata)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, std::fs::Metadata)> = entries
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.metadata().ok()?)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The `links/` entry a symlink at `link` points into, if any.
fn links_target(link: &Path, links_dir: &Path) -> Option<String> {
    let target = std::fs::read_link(link).ok()?;
    let absolute = if target.is_absolute() {
        target
    } else {
        link.parent()?.join(target)
    };
    let normalized = normalize(&absolute);
    let rest = normalized.strip_prefix(links_dir).ok()?;
    rest.components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .map(str::to_owned)
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Symlinks one or two levels below `dir` (`<name>` and `@scope/<name>`).
fn symlinks_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            out.push(e.path());
        } else if ft.is_dir()
            && e.file_name().to_string_lossy().starts_with('@')
            && let Ok(scoped) = std::fs::read_dir(e.path())
        {
            out.extend(
                scoped
                    .flatten()
                    .filter(|s| s.file_type().is_ok_and(|t| t.is_symlink()))
                    .map(|s| s.path()),
            );
        }
    }
    out
}

fn referenced_links(store: &Store, projects: &[String]) -> HashSet<String> {
    let links_dir = normalize(&store.links_dir());
    let canonical_links = std::fs::canonicalize(&links_dir).unwrap_or_else(|_| links_dir.clone());
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let visit = |link: &Path, seen: &mut HashSet<String>, queue: &mut VecDeque<String>| {
        let name = links_target(link, &links_dir).or_else(|| links_target(link, &canonical_links));
        if let Some(name) = name
            && seen.insert(name.clone())
        {
            queue.push_back(name);
        }
    };
    for project in projects {
        let dot = Path::new(project).join("node_modules").join(".fnpm");
        if let Ok(entries) = std::fs::read_dir(&dot) {
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_symlink()) {
                    visit(&e.path(), &mut seen, &mut queue);
                }
            }
        }
        for link in symlinks_below(&Path::new(project).join("node_modules")) {
            visit(&link, &mut seen, &mut queue);
        }
    }
    while let Some(name) = queue.pop_front() {
        for link in symlinks_below(&links_dir.join(&name).join("node_modules")) {
            visit(&link, &mut seen, &mut queue);
        }
    }
    seen
}

#[cfg(unix)]
fn physical_size(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn physical_size(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

/// Files below `dir` and their physical bytes, counting only inodes that no path outside
/// `dir` links to (what removing `dir` frees).
#[cfg(unix)]
fn exclusive_usage(dir: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let mut links: std::collections::HashMap<(u64, u64), (u64, u64, u64)> =
        std::collections::HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                let slot = links.entry((meta.dev(), meta.ino())).or_insert((
                    0,
                    meta.nlink(),
                    physical_size(&meta),
                ));
                slot.0 += 1;
            }
        }
    }
    links
        .values()
        .filter(|(seen, nlink, _)| seen >= nlink)
        .fold((0, 0), |(f, b), (_, _, size)| (f + 1, b + size))
}

#[cfg(not(unix))]
fn exclusive_usage(dir: &Path) -> (u64, u64) {
    let mut out = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                out.0 += 1;
                out.1 += physical_size(&meta);
            }
        }
    }
    out
}

pub(crate) fn prune(store: &Store, opts: PruneOptions) -> Result<PruneReport> {
    prune_with_grace(store, opts, GRACE)
}

pub(crate) fn prune_with_grace(
    store: &Store,
    opts: PruneOptions,
    grace: Duration,
) -> Result<PruneReport> {
    let cutoff = SystemTime::now()
        .checked_sub(grace)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let _lock = lock_projects(store)?;
    let mut projects = read_projects(store);
    let before = projects.projects.len();
    projects.projects.retain(|dir, _| Path::new(dir).is_dir());
    if projects.projects.len() != before && !opts.dry_run {
        write_projects(store, &projects)?;
    }
    let live: Vec<String> = projects.projects.keys().cloned().collect();
    let keep_pkgs: Option<HashSet<String>> =
        if projects.projects.values().any(|r| r.packages.is_none()) {
            None
        } else {
            Some(
                projects
                    .projects
                    .values()
                    .flat_map(|r| r.packages.iter().flatten().cloned())
                    .collect(),
            )
        };
    let keep_links = referenced_links(store, &live);
    let mut report = PruneReport::default();

    let links_dir = store.links_dir();
    for (name, meta) in dir_names(&links_dir) {
        if keep_links.contains(&name) || !older_than(&meta, cutoff) {
            continue;
        }
        if opts.dry_run || crate::sys::remove_tree(&links_dir.join(&name)).is_ok() {
            report.removed_links += 1;
        }
    }
    if let Some(keep) = keep_pkgs {
        let pkgs_dir = store.pkgs_dir();
        for (name, meta) in dir_names(&pkgs_dir) {
            if keep.contains(&name) || !older_than(&meta, cutoff) {
                continue;
            }
            let dir = pkgs_dir.join(&name);
            let (files, bytes) = exclusive_usage(&dir);
            if opts.dry_run || crate::sys::remove_tree(&dir).is_ok() {
                report.removed_packages += 1;
                report.removed_files += files;
                report.bytes_freed += bytes;
            }
        }
    }
    if !opts.dry_run {
        // Staging left behind by a crashed import. Only names the store creates are touched:
        // the staging dir may be a shared FNPM_TMPDIR.
        let tmp = store.tmp_dir().to_path_buf();
        for (name, meta) in dir_names(&tmp) {
            if crate::sys::is_staging_name(&name) && meta.is_dir() && older_than(&meta, cutoff) {
                let _ = crate::sys::remove_tree(&tmp.join(name));
            }
        }
    }
    Ok(report)
}

pub(crate) fn status(store: &Store) -> Result<StoreStatus> {
    let mut status = StoreStatus {
        path: store.root().to_path_buf(),
        ..Default::default()
    };
    status.packages = dir_names(&store.pkgs_dir())
        .iter()
        .filter(|(_, m)| m.is_dir())
        .count() as u64;
    status.links = dir_names(&store.links_dir())
        .iter()
        .filter(|(_, m)| m.is_dir())
        .count() as u64;
    for (name, meta) in dir_names(&store.pkgs_dir()) {
        if meta.is_dir() {
            let (files, bytes) = package_usage(&store.pkgs_dir().join(name).join("package"));
            status.files += files;
            status.bytes += bytes;
        }
    }
    Ok(status)
}

/// Files below `dir` and their physical bytes, hard links within `dir` counted once.
fn package_usage(dir: &Path) -> (u64, u64) {
    let mut seen = HashSet::new();
    let mut out = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() && inode(&meta, &e.path()).is_none_or(|id| seen.insert(id)) {
                out.0 += 1;
                out.1 += physical_size(&meta);
            }
        }
    }
    out
}

#[cfg(unix)]
fn inode(meta: &std::fs::Metadata, _path: &Path) -> Option<(u64, u128)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino().into()))
}

#[cfg(windows)]
fn inode(_meta: &std::fs::Metadata, path: &Path) -> Option<(u64, u128)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    // Query attributes without requiring read permission or following a final symlink.
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the handle is open and info is writable for the specified size. FileIdInfo
    // returns the volume plus a 128-bit ID, covering both NTFS and ReFS file identities.
    let rc = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    (rc != 0).then_some((
        info.VolumeSerialNumber,
        u128::from_le_bytes(info.FileId.Identifier),
    ))
}

#[cfg(not(any(unix, windows)))]
fn inode(_meta: &std::fs::Metadata, _path: &Path) -> Option<(u64, u128)> {
    None
}

/// What `pm cache prune` did (or would do).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePruneReport {
    /// Store packages looked at.
    pub checked: u64,
    /// Packages removed (or that would be, in a dry run).
    pub removed: u64,
    /// Packages that could not be removed.
    pub failed: u64,
    /// Bytes freed (files of the removed packages nothing else linked to).
    pub bytes: u64,
    /// The `pkgs/` names removed (dry run: that would be removed).
    pub removed_names: Vec<String>,
    /// `Could not delete <name>: <error>` per failure.
    pub errors: Vec<String>,
    /// The `--max-age` in days.
    pub max_age_days: u32,
    /// Whether nothing was removed.
    pub dry_run: bool,
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `1.5KB`-style sizes (powers of 1024, like bun's `fmt::size`).
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes}B");
    }
    let s = format!("{value:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}{}", UNITS[unit])
}

impl CachePruneReport {
    /// The lines bun prints for this result (without colours).
    pub fn lines(&self) -> Vec<String> {
        let days = u64::from(self.max_age_days);
        let mut out: Vec<String> = Vec::new();
        if self.dry_run {
            out.extend(
                self.removed_names
                    .iter()
                    .map(|n| format!("- {}", crate::tar::escape_control(n))),
            );
        }
        out.extend(self.errors.iter().cloned());
        if self.removed == 0 && self.failed == 0 {
            out.push(format!(
                "Done! Checked {} package{}, none older than {} day{} (nothing to prune)",
                self.checked,
                plural(self.checked),
                days,
                plural(days)
            ));
        } else if self.dry_run {
            out.push(format!(
                "{} package{} older than {} day{} can be removed (checked {})",
                self.removed,
                plural(self.removed),
                days,
                plural(days),
                self.checked
            ));
            out.push("Run without --dry-run to remove them.".to_owned());
        } else {
            let mut line = format!(
                "Removed {} package{} older than {} day{} ({})",
                self.removed,
                plural(self.removed),
                days,
                plural(days),
                format_size(self.bytes)
            );
            if self.failed > 0 {
                line.push_str(&format!(", {} failed", self.failed));
            }
            out.push(line);
        }
        out
    }
}

fn last_used(dir: &Path, meta: &std::fs::Metadata) -> SystemTime {
    let mut t = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    // Linking reads index.bin: relatime refreshes atime on Linux, and the store
    // explicitly refreshes it on macOS where reads may leave it unchanged.
    if let Ok(index) = std::fs::metadata(dir.join("index.bin")) {
        for candidate in [index.accessed(), index.modified()].into_iter().flatten() {
            t = t.max(candidate);
        }
    }
    t
}

pub(crate) fn prune_by_age(
    store: &Store,
    max_age_days: u32,
    dry_run: bool,
) -> Result<CachePruneReport> {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(max_age_days) * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut report = CachePruneReport {
        max_age_days,
        dry_run,
        ..Default::default()
    };
    let pkgs = store.pkgs_dir();
    for (name, meta) in dir_names(&pkgs) {
        if !meta.is_dir() {
            continue;
        }
        let dir = pkgs.join(&name);
        report.checked += 1;
        if last_used(&dir, &meta) > cutoff {
            continue;
        }
        if dry_run {
            report.removed += 1;
            report.removed_names.push(name);
            continue;
        }
        let (_, bytes) = exclusive_usage(&dir);
        match crate::sys::remove_tree(&dir) {
            Ok(()) => {
                report.removed += 1;
                report.bytes += bytes;
                report.removed_names.push(name);
            }
            Err(e) => {
                report.failed += 1;
                report.errors.push(format!("Could not delete {name}: {e}"));
            }
        }
    }
    Ok(report)
}
