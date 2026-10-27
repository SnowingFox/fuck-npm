//! `fnpm prune` and post-remove cleanup (bun `install/prune.rs`).
//!
//! [`plan`] compares what is in `node_modules` with a planned [`Layout`] (built with the same
//! `--omit` / `--production` / `--os` / `--cpu` filters the install would use) and lists what
//! can go: hoisted package folders that are not in the tree, top-level links that no importer
//! declares, `.fnpm` store entries no entry uses, stale hidden-hoist links and leftovers of
//! interrupted installs. It never touches workspaces' sources, dot entries of other tools, the
//! registry or the global store. [`execute`] removes them and prunes dangling `.bin` links.
//! [`detect_layout`] votes on which linker made an existing tree.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use fnpm_types::{LockGraph, NodeLinker, Resolution};

use crate::fsutil::{self, Kind};
use crate::hoisted::local_target;
use crate::{FNPM_META_FILES, Layout};

/// One thing [`plan`] would remove.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Removal {
    /// The absolute path.
    pub path: PathBuf,
    /// What it is: the package folder name, the store key, or the leftover's name.
    pub name: String,
    /// A symlink (removed with `unlink`) rather than a directory tree.
    pub is_link: bool,
}

/// The result of [`plan`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    /// Sorted by path.
    pub removals: Vec<Removal>,
    /// Installed entries that were looked at.
    pub checked: u32,
    /// `node_modules` folders that are symlinks: bun refuses to prune those.
    pub linked_folders: Vec<PathBuf>,
}

/// Which linker made the tree in `project_dir`: links into `.fnpm` vote isolated, package
/// directories vote hoisted; a tie (or a mix) goes to `configured`, no evidence to isolated when
/// `.fnpm` has store entries and hoisted otherwise (bun `detect_layout`).
pub fn detect_layout(project_dir: &Path, graph: &LockGraph, configured: NodeLinker) -> NodeLinker {
    let mut extracted: HashSet<&str> = HashSet::new();
    for p in &graph.packages {
        for e in &p.dependencies {
            if let Some(t) = e.target
                && graph.get(t).is_some_and(|t| t.resolution.is_storable())
            {
                extracted.insert(e.name.as_str());
            }
        }
    }
    let mut hoisted = false;
    let mut isolated = false;
    let mut scan = |nm: &Path| {
        for (alias, path, kind) in list_packages(nm) {
            match kind {
                Kind::Dir if extracted.contains(alias.as_str()) => hoisted = true,
                Kind::Symlink => {
                    if let Ok(link) = std::fs::read_link(&path)
                        && link
                            .components()
                            .any(|c| c.as_os_str() == ".fnpm" || c.as_os_str() == ".bun")
                        && path.exists()
                    {
                        isolated = true;
                    }
                }
                _ => {}
            }
        }
    };
    scan(&project_dir.join("node_modules"));
    for p in &graph.packages {
        if let Resolution::Workspace { .. } = p.resolution
            && let Some(dir) = local_target(project_dir, &p.resolution)
        {
            scan(&dir.join("node_modules"));
        }
    }
    match (isolated, hoisted) {
        (true, false) => NodeLinker::Isolated,
        (false, true) => NodeLinker::Hoisted,
        (true, true) => configured,
        (false, false) if crate::is_isolated_tree(project_dir) => NodeLinker::Isolated,
        // Nothing installed tells: the linker an install would use.
        (false, false) => configured,
    }
}

/// `(alias, path, kind)` of every package-like entry of a `node_modules` (scoped expanded;
/// dot entries skipped).
fn list_packages(nm: &Path) -> Vec<(String, PathBuf, Kind)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(nm) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = nm.join(&name);
        let kind = fsutil::kind(&path);
        if name.starts_with('@') && kind == Kind::Dir {
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            for ie in inner.flatten() {
                let iname = ie.file_name().to_string_lossy().into_owned();
                if iname.starts_with('.') {
                    continue;
                }
                let ip = path.join(&iname);
                let k = fsutil::kind(&ip);
                out.push((format!("{name}/{iname}"), ip, k));
            }
            continue;
        }
        out.push((name, path, kind));
    }
    out.sort();
    out
}

fn leftovers(dir: &Path, out: &mut Vec<Removal>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if fsutil::is_leftover(&name) {
            let path = dir.join(&name);
            let is_link = fsutil::kind(&path) == Kind::Symlink;
            out.push(Removal {
                path,
                name,
                is_link,
            });
        }
    }
}

/// Lists what is in `project_dir/node_modules` (and the workspaces') but not in `layout`.
pub fn plan(project_dir: &Path, graph: &LockGraph, layout: &Layout) -> PrunePlan {
    let mut out = PrunePlan::default();
    let nm = project_dir.join("node_modules");
    if fsutil::kind(&nm) == Kind::Symlink {
        out.linked_folders.push(nm.clone());
    }
    match layout {
        Layout::Hoisted(tree) => {
            // Expected paths, absolute.
            let mut expected: HashSet<PathBuf> = HashSet::new();
            let mut ws_base: std::collections::HashMap<u32, PathBuf> =
                std::collections::HashMap::new();
            let mut folders: Vec<PathBuf> = vec![nm.clone()];
            for (ws, ids) in &tree.workspace_roots {
                if let Some(dir) = graph
                    .get(*ws)
                    .and_then(|p| local_target(project_dir, &p.resolution))
                {
                    folders.push(dir.join("node_modules"));
                    for id in ids {
                        ws_base.insert(*id, dir.join("node_modules"));
                    }
                }
            }
            let mut dirs: Vec<PathBuf> = Vec::with_capacity(tree.nodes.len());
            for (i, n) in tree.nodes.iter().enumerate() {
                let base = match (ws_base.get(&(i as u32)), n.parent) {
                    (Some(b), _) => b.clone(),
                    (None, None) => nm.clone(),
                    (None, Some(p)) => dirs
                        .get(p as usize)
                        .map(|d| d.join("node_modules"))
                        .unwrap_or_else(|| nm.clone()),
                };
                let d = fsutil::join_name(&base, &n.name);
                expected.insert(d.clone());
                dirs.push(d);
            }
            // Nested node_modules of every installed package.
            for d in &dirs {
                let nested = d.join("node_modules");
                if fsutil::kind(d) == Kind::Dir && fsutil::kind(&nested) == Kind::Dir {
                    folders.push(nested);
                }
            }
            folders.sort();
            folders.dedup();
            for folder in &folders {
                leftovers(folder, &mut out.removals);
                for (alias, path, kind) in list_packages(folder) {
                    out.checked += 1;
                    if !expected.contains(&path) {
                        out.removals.push(Removal {
                            path,
                            name: alias,
                            is_link: kind == Kind::Symlink,
                        });
                    }
                }
            }
        }
        Layout::Isolated(l) => {
            for (pkg, deps) in &l.roots {
                let Some(dir) = graph
                    .get(*pkg)
                    .and_then(|p| local_target(project_dir, &p.resolution))
                else {
                    continue;
                };
                let folder = dir.join("node_modules");
                if *pkg != fnpm_types::PackageId::ROOT && fsutil::kind(&folder) == Kind::Symlink {
                    out.linked_folders.push(folder.clone());
                    continue;
                }
                let mut names: HashSet<&str> = deps.iter().map(|(a, _)| a.as_str()).collect();
                if *pkg == fnpm_types::PackageId::ROOT {
                    names.extend(l.public_hoist.iter().map(|(a, _)| a.as_str()));
                }
                leftovers(&folder, &mut out.removals);
                for (alias, path, kind) in list_packages(&folder) {
                    out.checked += 1;
                    if !names.contains(alias.as_str()) {
                        out.removals.push(Removal {
                            path,
                            name: alias,
                            is_link: kind == Kind::Symlink,
                        });
                    }
                }
            }
            let fnpm = nm.join(".fnpm");
            let keys: HashSet<&str> = l
                .entries
                .iter()
                .filter(|e| {
                    graph.get(e.package).is_some_and(|p| {
                        !matches!(
                            p.resolution,
                            Resolution::Root
                                | Resolution::Workspace { .. }
                                | Resolution::Symlink { .. }
                        )
                    })
                })
                .map(|e| e.key.as_str())
                .collect();
            if let Ok(rd) = std::fs::read_dir(&fnpm) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name == "node_modules" || FNPM_META_FILES.contains(&name.as_str()) {
                        continue;
                    }
                    let path = fnpm.join(&name);
                    if fsutil::is_leftover(&name)
                        || (!name.starts_with('.') && !keys.contains(name.as_str()))
                    {
                        out.checked += 1;
                        let is_link = fsutil::kind(&path) == Kind::Symlink;
                        out.removals.push(Removal {
                            path,
                            name,
                            is_link,
                        });
                    } else if !name.starts_with('.') {
                        out.checked += 1;
                    }
                }
            }
            let hidden: HashSet<&str> = l.hidden_hoist.iter().map(|(a, _)| a.as_str()).collect();
            for (alias, path, kind) in list_packages(&fnpm.join("node_modules")) {
                if !hidden.contains(alias.as_str()) {
                    out.removals.push(Removal {
                        path,
                        name: alias,
                        is_link: kind == Kind::Symlink,
                    });
                }
            }
        }
    }
    out.removals.sort();
    out.removals.dedup();
    out
}

/// Removes everything in `plan` (links unlinked, trees deleted), empty scope folders, then
/// dangling `.bin` links of the root. Returns `(removed, failed paths)`.
pub fn execute(project_dir: &Path, plan: &PrunePlan) -> (u32, Vec<(PathBuf, std::io::Error)>) {
    let mut removed = 0;
    let mut failed = Vec::new();
    let mut scopes: HashSet<PathBuf> = HashSet::new();
    for r in &plan.removals {
        let res = if r.is_link {
            fsutil::remove_link(&r.path)
        } else {
            fsutil::remove_any(&r.path)
        };
        match res {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => removed += 1,
            Err(e) => {
                failed.push((r.path.clone(), e));
                continue;
            }
        }
        if let Some(parent) = r.path.parent()
            && parent
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('@'))
        {
            scopes.insert(parent.to_path_buf());
        }
    }
    for s in scopes {
        let _ = std::fs::remove_dir(s);
    }
    crate::bins::prune_dangling_bins(&project_dir.join("node_modules").join(".bin"));
    (removed, failed)
}

/// After `fnpm remove` (or an install that skipped a group): delete the top-level
/// `node_modules/<name>` folders that `layout` no longer places and prune dangling bins.
/// Returns how many entries went.
pub fn remove_unplaced(project_dir: &Path, graph: &LockGraph, layout: &Layout) -> u32 {
    let p = plan(project_dir, graph, layout);
    let nm = project_dir.join("node_modules");
    let top: PrunePlan = PrunePlan {
        removals: p
            .removals
            .into_iter()
            .filter(|r| {
                r.path.parent() == Some(nm.as_path())
                    || r.path.parent().and_then(Path::parent) == Some(nm.as_path())
                        && r.name.contains('/')
            })
            .collect(),
        ..Default::default()
    };
    execute(project_dir, &top).0
}
