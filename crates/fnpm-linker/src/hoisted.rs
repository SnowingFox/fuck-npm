//! The hoisted linker (bun `hoisted_install.rs`, `PackageInstaller.rs`, `PackageInstall.rs`).
//!
//! Every tree node is a directory `node_modules/<a>/node_modules/<b>`. Top-level nodes start in
//! parallel on the link pool; a node's children start as soon as the node's own directory is
//! in place (so a nested package never waits for unrelated work). With a brand-new
//! `node_modules` nothing is verified or deleted. Otherwise each node is checked cheaply
//! (`package.json` name and version, `.fnpm-tag`, or the inode a link resolves to); a mismatch
//! is moved aside and deleted in the background, and its subtree is rebuilt. Entries no longer
//! in the tree are removed, and dangling `.bin` links pruned. Bins are linked per `.bin`
//! directory once every package is placed, direct dependencies first.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use fnpm_lockfile::tree::Tree;
use fnpm_types::{PackageId, Resolution};
use parking_lot::Mutex;

use crate::bins::{BinLinker, BinOptions, LinkOrder};
use crate::fsutil::{self, Kind};
use crate::verify;
use crate::{Error, LinkCx, LinkReport, Result};

/// How a node is placed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    /// Materialised from the store.
    Copy,
    /// A symlink to this absolute directory (workspace, `link:`, the root).
    Link(PathBuf),
}

struct Plan<'t> {
    tree: &'t Tree,
    dirs: Vec<PathBuf>,
    places: Vec<Place>,
    children: Vec<Vec<u32>>,
    tops: Vec<u32>,
    /// Packages whose every incoming edge is optional: a failed download skips them.
    optional_only: Vec<bool>,
    skipped: Vec<bool>,
    /// Expected children of each node_modules, including unselected workspaces when filtered.
    expected: HashMap<PathBuf, Vec<String>>,
}

/// Packages that some root or workspace declares (prod, dev, optional or workspace edge).
pub(crate) fn direct_packages(graph: &fnpm_types::LockGraph) -> Vec<bool> {
    let mut direct = vec![false; graph.packages.len()];
    for (i, p) in graph.packages.iter().enumerate() {
        if !matches!(
            p.resolution,
            Resolution::Root | Resolution::Workspace { .. }
        ) {
            continue;
        }
        let _ = i;
        for e in &p.dependencies {
            let b = e.behavior;
            let declares = b.is_prod() || b.is_dev() || b.is_optional() || b.is_workspace();
            if declares
                && let Some(t) = e.target
                && t.index() < direct.len()
            {
                direct[t.index()] = true;
            }
        }
    }
    direct
}

pub(crate) fn optional_only(graph: &fnpm_types::LockGraph) -> Vec<bool> {
    let mut seen = vec![false; graph.packages.len()];
    let mut required = vec![false; graph.packages.len()];
    for p in &graph.packages {
        for e in &p.dependencies {
            let Some(t) = e.target else { continue };
            let Some(i) = (t.index() < seen.len()).then_some(t.index()) else {
                continue;
            };
            seen[i] = true;
            if !(e.behavior.is_optional() || e.behavior.is_optional_peer()) {
                required[i] = true;
            }
        }
    }
    seen.iter().zip(&required).map(|(s, r)| *s && !*r).collect()
}

/// Absolute directory of a `link:` / workspace / root package (a `link:<name>` registration
/// resolves against the project, see [`local_target_with`]).
pub(crate) fn local_target(project_dir: &Path, resolution: &Resolution) -> Option<PathBuf> {
    local_target_with(project_dir, None, resolution)
}

/// Like [`local_target`]; `link:<package name>` (an `fnpm link` registration rather than a
/// path, bun `PackageInstaller` "a name is relative to the global link directory") resolves
/// to `<global_link_dir>/<name>` when the directory is known.
pub(crate) fn local_target_with(
    project_dir: &Path,
    global_link_dir: Option<&Path>,
    resolution: &Resolution,
) -> Option<PathBuf> {
    match resolution {
        Resolution::Root => Some(project_dir.to_path_buf()),
        Resolution::Symlink { path }
            if global_link_dir.is_some() && !fnpm_types::spec::is_link_path(path) =>
        {
            global_link_dir.map(|d| fsutil::join_name(d, path))
        }
        Resolution::Workspace { path } | Resolution::Symlink { path } => {
            let p = Path::new(path);
            Some(if p.is_absolute() {
                fsutil::normalize(p)
            } else {
                fsutil::normalize(&project_dir.join(p))
            })
        }
        _ => None,
    }
}

fn build_plan<'t>(cx: &LinkCx<'_>, tree: &'t Tree) -> Result<Plan<'t>> {
    let nm = cx.project_dir.join("node_modules");
    let n = tree.nodes.len();
    let mut ws_base: HashMap<u32, PathBuf> = HashMap::new();
    for (ws, ids) in &tree.workspace_roots {
        let Some(p) = cx.graph.get(*ws) else { continue };
        let Some(dir) = local_target(cx.project_dir, &p.resolution) else {
            continue;
        };
        for id in ids {
            ws_base.insert(*id, dir.join("node_modules"));
        }
    }
    let mut dirs: Vec<PathBuf> = Vec::with_capacity(n);
    let mut places = Vec::with_capacity(n);
    let mut children = vec![Vec::new(); n];
    let mut tops = Vec::new();
    let mut skipped = vec![false; n];
    let mut expected: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for (i, node) in tree.nodes.iter().enumerate() {
        let label = || {
            node.parent
                .and_then(|p| cx.graph.get(tree.nodes[p as usize].package))
                .map_or_else(|| "root".to_owned(), |p| p.key())
        };
        if !fnpm_types::name::is_safe_install_folder_name(&node.name) {
            return Err(Error::UnsafePath {
                package: label(),
                path: node.name.clone(),
            });
        }
        let base = match (ws_base.get(&(i as u32)), node.parent) {
            (Some(b), _) => b.clone(),
            (None, None) => nm.clone(),
            (None, Some(p)) => {
                let p = p as usize;
                if p >= i {
                    return Err(Error::UnsafePath {
                        package: label(),
                        path: node.path.clone(),
                    });
                }
                children[p].push(i as u32);
                dirs[p].join("node_modules")
            }
        };
        if node.parent.is_none() {
            tops.push(i as u32);
        }
        expected
            .entry(base.clone())
            .or_default()
            .push(node.name.clone());
        dirs.push(fsutil::join_name(&base, &node.name));
        let pkg = cx.graph.get(node.package).ok_or_else(|| Error::Missing {
            package: node.path.clone(),
            message: format!("package id {} is not in the graph", node.package.0),
        })?;
        let parent_skipped = node.parent.is_some_and(|p| skipped[p as usize]);
        skipped[i] = parent_skipped
            || (!pkg.resolution.is_local_tree() && !pkg.platform.matches(&cx.platform));
        places.push(match cx.local_target(&pkg.resolution) {
            Some(t) => Place::Link(t),
            None => Place::Copy,
        });
    }
    Ok(Plan {
        tree,
        dirs,
        places,
        children,
        tops,
        optional_only: optional_only(cx.graph),
        skipped,
        expected,
    })
}

/// One key per tree node: its workspace, folder path and package (`name@resolution`). Two
/// nodes with the same key are the same package in the same directory.
fn node_keys(tree: &Tree, graph: &fnpm_types::LockGraph) -> Vec<String> {
    let mut ws_of: HashMap<u32, String> = HashMap::new();
    for (ws, ids) in &tree.workspace_roots {
        let key = graph.get(*ws).map(|p| p.key()).unwrap_or_default();
        for &i in ids {
            ws_of.insert(i, key.clone());
        }
    }
    let mut ws: Vec<String> = Vec::with_capacity(tree.nodes.len());
    let mut keys = Vec::with_capacity(tree.nodes.len());
    for (i, node) in tree.nodes.iter().enumerate() {
        let w = match (ws_of.get(&(i as u32)), node.parent) {
            (Some(w), _) => w.clone(),
            (None, Some(p)) if (p as usize) < i => ws[p as usize].clone(),
            _ => String::new(),
        };
        let pkg = graph.get(node.package).map(|p| p.key()).unwrap_or_default();
        keys.push(format!("{w}\0{}\0{}\0{pkg}", node.path, node.name));
        ws.push(w);
    }
    keys
}

/// For each node of `tree`: the previous install placed the same package in the same
/// directory, and that directory had no nested `node_modules` unless this node has one too
/// (a leftover nested copy would shadow a hoisted one, so that case is checked).
fn unchanged_nodes(cx: &LinkCx<'_>, tree: &Tree) -> Vec<bool> {
    let Some(prev) = cx.opts.previous.as_ref() else {
        return vec![false; tree.nodes.len()];
    };
    let mut prev_has_children = vec![false; prev.tree.nodes.len()];
    for n in &prev.tree.nodes {
        if let Some(p) = n.parent
            && let Some(slot) = prev_has_children.get_mut(p as usize)
        {
            *slot = true;
        }
    }
    let before: HashMap<String, bool> = node_keys(&prev.tree, &prev.graph)
        .into_iter()
        .zip(prev_has_children)
        .collect();
    let mut has_children = vec![false; tree.nodes.len()];
    for n in &tree.nodes {
        if let Some(p) = n.parent
            && let Some(slot) = has_children.get_mut(p as usize)
        {
            *slot = true;
        }
    }
    node_keys(tree, cx.graph)
        .iter()
        .zip(has_children)
        .map(|(k, children)| before.get(k).is_some_and(|had| children || !had))
        .collect()
}

struct Out {
    installed: AtomicU32,
    /// Per package: written by this run (`installed_packages` counts each once).
    written: Vec<std::sync::atomic::AtomicBool>,
    installed_packages: AtomicU32,
    verified: AtomicU32,
    removed: AtomicU32,
    skipped: AtomicU32,
    dirs: Mutex<Vec<(PackageId, PathBuf)>>,
    refreshed: Vec<std::sync::atomic::AtomicBool>,
    error: Mutex<Option<Error>>,
}

impl Out {
    fn fail(&self, e: Error) {
        let mut g = self.error.lock();
        if g.is_none() {
            *g = Some(e);
        }
    }
}

pub(crate) fn link(cx: &LinkCx<'_>, tree: &Tree) -> Result<LinkReport> {
    let nm = cx.project_dir.join("node_modules");
    let mut fresh = cx.opts.fresh || !nm.exists();
    if !fresh && crate::is_isolated_tree(cx.project_dir) {
        // Switching from the isolated layout: move the whole tree aside.
        crate::move_node_modules_aside(&nm, &[]);
        for (ws, _) in &tree.workspace_roots {
            if let Some(dir) = cx
                .graph
                .get(*ws)
                .and_then(|p| local_target(cx.project_dir, &p.resolution))
                && crate::is_inside_project(cx.project_dir, &dir)
            {
                crate::move_node_modules_aside(&dir.join("node_modules"), &[]);
            }
        }
        for p in &cx.graph.packages {
            if let Resolution::Workspace { .. } = p.resolution
                && let Some(dir) = local_target(cx.project_dir, &p.resolution)
                && crate::is_inside_project(cx.project_dir, &dir)
            {
                crate::move_node_modules_aside(&dir.join("node_modules"), &[]);
            }
        }
        fresh = true;
    }
    fsutil::create_dir_all(&nm).map_err(Error::NodeModules)?;
    let mut plan = build_plan(cx, tree)?;
    if let Some(full) = &cx.opts.hoisted_prune_tree {
        plan.expected = build_plan(cx, full)?.expected;
    }
    cx.total
        .fetch_add(plan.dirs.len() as u32, Ordering::Relaxed);

    let out = Out {
        installed: AtomicU32::new(0),
        written: (0..cx.graph.packages.len())
            .map(|_| Default::default())
            .collect(),
        installed_packages: AtomicU32::new(0),
        verified: AtomicU32::new(0),
        removed: AtomicU32::new(0),
        skipped: AtomicU32::new(0),
        dirs: Mutex::new(Vec::new()),
        refreshed: (0..plan.dirs.len()).map(|_| Default::default()).collect(),
        error: Mutex::new(None),
    };
    let force = cx.opts.force;
    let unchanged = if fresh || force {
        Vec::new()
    } else {
        unchanged_nodes(cx, tree)
    };
    let unchanged = &unchanged;
    // FIFO, biggest packages first: every worker starts on the largest remaining one, so a
    // big package does not start last and stretch the whole link.
    let tops = crate::largest_first(cx.source, &plan.tops, |t| {
        plan.tree.nodes[t as usize].package
    });
    rayon::scope_fifo(|s| {
        for top in tops {
            let (plan, out) = (&plan, &out);
            s.spawn_fifo(move |s| place_node(s, cx, plan, out, unchanged, top, fresh, force));
        }
    });
    if let Some(e) = out.error.lock().take() {
        return Err(e);
    }

    if !fresh {
        let mut removed = 0;
        removed += remove_stale(
            &nm,
            plan.expected
                .get(&nm)
                .into_iter()
                .flatten()
                .map(String::as_str),
        );
        let project_root = std::fs::canonicalize(cx.project_dir).ok();
        // A workspace with no remaining nested dependencies has no workspace_roots row.
        // Visit importers as well so removing its last nested dependency prunes the old copy.
        for ws in cx.graph.workspaces.values().filter(|&&id| {
            id != PackageId::ROOT
                && (tree.workspace_roots.iter().any(|(ws, _)| *ws == id)
                    || cx.opts.previous_hoisted_workspaces.contains(&id))
                && cx
                    .opts
                    .selected_workspaces
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&id))
        }) {
            if let Some(dir) = cx
                .graph
                .get(*ws)
                .and_then(|p| local_target(cx.project_dir, &p.resolution))
            {
                let nm = dir.join("node_modules");
                // Only prune real package trees inside this project. An external workspace
                // or a symlinked node_modules can belong to another install entirely.
                if !crate::is_inside_project(cx.project_dir, &dir)
                    || fsutil::kind(&nm) != Kind::Dir
                    || !project_root.as_ref().is_some_and(|root| {
                        std::fs::canonicalize(&nm).is_ok_and(|real| real.starts_with(root))
                    })
                {
                    continue;
                }
                removed += remove_stale(
                    &nm,
                    plan.expected
                        .get(&nm)
                        .into_iter()
                        .flatten()
                        .map(String::as_str),
                );
                crate::bins::prune_dangling_bins(&nm.join(".bin"));
            }
        }
        out.removed.fetch_add(removed, Ordering::Relaxed);
    }

    link_all_bins(cx, &plan, &out, fresh)?;

    let mut dirs = out.dirs.into_inner();
    dirs.sort();
    Ok(LinkReport {
        installed: out.installed.load(Ordering::Relaxed),
        installed_packages: out.installed_packages.load(Ordering::Relaxed),
        verified: out.verified.load(Ordering::Relaxed),
        written: out
            .written
            .iter()
            .map(|w| w.load(Ordering::Relaxed))
            .collect(),
        removed: out.removed.load(Ordering::Relaxed),
        skipped: out.skipped.load(Ordering::Relaxed),
        dirs,
        backend_used: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn place_node<'s>(
    s: &rayon::ScopeFifo<'s>,
    cx: &'s LinkCx<'s>,
    plan: &'s Plan<'_>,
    out: &'s Out,
    unchanged: &'s [bool],
    i: u32,
    parent_fresh: bool,
    force: bool,
) {
    let idx = i as usize;
    let node = &plan.tree.nodes[idx];
    if plan.skipped[idx] {
        out.skipped.fetch_add(1, Ordering::Relaxed);
        cx.tick();
        return;
    }
    let dest = &plan.dirs[idx];
    let nm = cx.project_dir.join("node_modules");
    // Never write through a symlink planted in a parent (`node_modules/@scope -> /elsewhere`).
    let unplant = || -> Result<()> {
        if let Some(parent) = dest.parent()
            && parent.starts_with(&nm)
        {
            fsutil::remove_planted_links(parent, &nm).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        Ok(())
    };
    let result = (|| -> Result<Option<bool>> {
        match &plan.places[idx] {
            Place::Link(target) => {
                if !parent_fresh
                    && !force
                    && fsutil::kind(dest) == Kind::Symlink
                    && verify::same_file(dest, target)
                {
                    out.verified.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(false));
                }
                if !parent_fresh {
                    unplant()?;
                    fsutil::move_aside_and_delete(dest).map_err(|source| Error::Io {
                        path: dest.clone(),
                        source,
                    })?;
                }
                let parent = dest.parent().unwrap_or(cx.project_dir);
                let rel = fsutil::relative(parent, target);
                fsutil::ensure_symlink(&rel, dest, fsutil::Strategy::ExpectMissing, None).map_err(
                    |source| Error::Io {
                        path: dest.clone(),
                        source,
                    },
                )?;
                // bun counts a new workspace link as installed.
                out.installed.fetch_add(1, Ordering::Relaxed);
                if !out.written[node.package.index()].swap(true, Ordering::Relaxed) {
                    out.installed_packages.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Some(true))
            }
            Place::Copy => {
                let pkg = &cx.graph.packages[node.package.index()];
                if !parent_fresh && !force {
                    let trusted = unchanged.get(idx).copied().unwrap_or(false)
                        && !matches!(pkg.resolution, Resolution::Folder { .. });
                    // Removing a scoped or nested package does not change the root
                    // node_modules identity. Require the placement and completion marker
                    // even when the previous tree lets us skip parsing package.json.
                    if trusted
                        && fsutil::kind(dest) == Kind::Dir
                        && fsutil::kind(&dest.join("package.json")) == Kind::File
                    {
                        return Ok(Some(false));
                    }
                    let ok = match &pkg.resolution {
                        Resolution::Folder { .. } => match cx.source.wait(node.package) {
                            Ok(entry) => verify::same_file(
                                &dest.join("package.json"),
                                &entry.root.join("package.json"),
                            ),
                            Err(_) => false,
                        },
                        r => {
                            verify::verify(dest, &verify::expected_for(&pkg.name, &pkg.version, r))
                                && cx.patch_ok(node.package, dest)
                        }
                    };
                    if ok && fsutil::kind(dest) == Kind::Dir {
                        if plan.children[idx].is_empty() {
                            // Nested copies an earlier tree left there would shadow hoisted ones.
                            let nested = dest.join("node_modules");
                            if fsutil::kind(&nested) == Kind::Dir {
                                out.removed.fetch_add(
                                    remove_stale(
                                        &nested,
                                        plan.expected
                                            .get(&nested)
                                            .into_iter()
                                            .flatten()
                                            .map(String::as_str),
                                    ),
                                    Ordering::Relaxed,
                                );
                            }
                        }
                        out.verified.fetch_add(1, Ordering::Relaxed);
                        return Ok(Some(false));
                    }
                    unplant()?;
                    fsutil::move_aside_and_delete(dest).map_err(|source| Error::Io {
                        path: dest.clone(),
                        source,
                    })?;
                }
                match cx.materialize(node.package, dest) {
                    Ok(()) => {}
                    Err(Error::Missing { .. }) if plan.optional_only[node.package.index()] => {
                        out.skipped.fetch_add(1, Ordering::Relaxed);
                        return Ok(None);
                    }
                    Err(e) => return Err(e),
                }
                out.installed.fetch_add(1, Ordering::Relaxed);
                if !out.written[node.package.index()].swap(true, Ordering::Relaxed) {
                    out.installed_packages.fetch_add(1, Ordering::Relaxed);
                }
                out.dirs.lock().push((node.package, dest.clone()));
                Ok(Some(true))
            }
        }
    })();
    cx.tick();
    let rebuilt = match result {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            out.fail(e);
            return;
        }
    };
    out.refreshed[idx].store(rebuilt, Ordering::Relaxed);
    let child_fresh = parent_fresh || (rebuilt && plan.places[idx] == Place::Copy);
    if !child_fresh && !plan.children[idx].is_empty() && plan.places[idx] == Place::Copy {
        let nested = dest.join("node_modules");
        out.removed.fetch_add(
            remove_stale(
                &nested,
                plan.expected
                    .get(&nested)
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            ),
            Ordering::Relaxed,
        );
    }
    for &c in &plan.children[idx] {
        s.spawn_fifo(move |s| place_node(s, cx, plan, out, unchanged, c, child_fresh, force));
    }
}

/// Removes linker-made entries of `node_modules` whose name is not `expected` (scoped names
/// handled per scope). Dot entries (`.bin`, `.fnpm`, other tools') stay. Returns the count.
pub(crate) fn remove_stale<'a>(
    node_modules: &Path,
    expected: impl Iterator<Item = &'a str>,
) -> u32 {
    let Ok(rd) = std::fs::read_dir(node_modules) else {
        return 0;
    };
    let mut top: HashSet<&str> = HashSet::new();
    let mut scoped: BTreeMap<&str, HashSet<&str>> = BTreeMap::new();
    for name in expected {
        match name.split_once('/') {
            Some((scope, rest)) if name.starts_with('@') => {
                scoped.entry(scope).or_default().insert(rest);
            }
            _ => {
                top.insert(name);
            }
        }
    }
    let mut removed = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            if fsutil::is_leftover(&name) {
                fsutil::delete_in_background(node_modules.join(&name));
            }
            continue;
        }
        let path = node_modules.join(&name);
        if name.starts_with('@') && fsutil::kind(&path) == Kind::Dir {
            let keep = scoped.get(name.as_str());
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            for ie in inner.flatten() {
                let iname = ie.file_name().to_string_lossy().into_owned();
                if keep.is_some_and(|k| k.contains(iname.as_str())) {
                    continue;
                }
                if fsutil::move_aside_into(&path.join(&iname), node_modules).is_ok() {
                    removed += 1;
                }
            }
            let _ = std::fs::remove_dir(&path);
            continue;
        }
        if top.contains(name.as_str()) {
            continue;
        }
        if fsutil::move_aside_and_delete(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn link_all_bins(cx: &LinkCx<'_>, plan: &Plan<'_>, out: &Out, fresh: bool) -> Result<()> {
    let direct = direct_packages(cx.graph);
    let mut groups: BTreeMap<PathBuf, Vec<u32>> = BTreeMap::new();
    for (i, node) in plan.tree.nodes.iter().enumerate() {
        if plan.skipped[i] {
            continue;
        }
        let Some(pkg) = cx.graph.get(node.package) else {
            continue;
        };
        if pkg.bin.is_none() || fsutil::kind(&plan.dirs[i]) == Kind::Missing {
            continue;
        }
        let parent = plan.dirs[i]
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        // `node_modules/@scope/name` links into `node_modules/.bin`.
        let nm = if node.name.contains('/') {
            parent.parent().map(Path::to_path_buf).unwrap_or(parent)
        } else {
            parent
        };
        groups.entry(nm.join(".bin")).or_default().push(i as u32);
    }
    let errors: Mutex<Option<Error>> = Mutex::new(None);
    let groups: Vec<(PathBuf, Vec<u32>)> = groups.into_iter().collect();
    rayon::scope(|s| {
        for (bin_dir, mut ids) in groups.iter().cloned() {
            let errors = &errors;
            let direct = &direct;
            s.spawn(move |_| {
                ids.sort_by(|&a, &b| {
                    let key = |i: u32| {
                        let node = &plan.tree.nodes[i as usize];
                        let pkg = &cx.graph.packages[node.package.index()];
                        LinkOrder::new(
                            direct[node.package.index()],
                            pkg.resolution.tag(),
                            node.name.as_str(),
                        )
                    };
                    key(a).cmp(&key(b))
                });
                if !fresh
                    && let Some(nm) = bin_dir.parent()
                    && let Err(e) = fsutil::remove_planted_links(&bin_dir, nm)
                {
                    let mut g = errors.lock();
                    if g.is_none() {
                        *g = Some(Error::Io {
                            path: bin_dir.clone(),
                            source: e,
                        });
                    }
                    return;
                }
                let mut linker = BinLinker::new(&bin_dir);
                for i in ids {
                    let node = &plan.tree.nodes[i as usize];
                    let pkg = &cx.graph.packages[node.package.index()];
                    let dir = &plan.dirs[i as usize];
                    let redirect = cx
                        .native_target(node.package)
                        .and_then(|t| cx.graph.get(t))
                        .and_then(|t| crate::node_resolve(dir, &t.name));
                    let refresh = out.refreshed[i as usize].load(Ordering::Relaxed);
                    if let Err(e) = linker.link(
                        &node.name,
                        &pkg.bin,
                        dir,
                        BinOptions {
                            redirect_dir: redirect.as_deref(),
                            refresh,
                            ..BinOptions::default()
                        },
                    ) {
                        let mut g = errors.lock();
                        if g.is_none() {
                            *g = Some(e);
                        }
                    }
                }
                if !fresh {
                    crate::bins::prune_dangling_bins(&bin_dir);
                }
            });
        }
    });
    if !fresh {
        crate::bins::prune_dangling_bins(&cx.project_dir.join("node_modules").join(".bin"));
    }
    match errors.into_inner() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
