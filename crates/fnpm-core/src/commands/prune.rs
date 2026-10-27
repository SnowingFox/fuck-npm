//! `fnpm prune [--dry-run | --check] [--production] [--omit x] [--filter p]` (bun
//! `install/prune.rs`): removes what `node_modules` holds but an install with this run's options
//! would not place, without touching fnpm.lock or package.json.
//!
//! Hoisted installs are compared folder by folder with the hoisted tree; a folder whose owner is
//! not the version fnpm.lock expects, or whose expected copy is missing or another version, keeps
//! what shadows it (with a warning). Isolated installs keep the store entries an install would
//! create (with every dependency type and with this run's) and the links each importer
//! declares. Symlinked `node_modules` folders are never followed: the project is refused.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use fnpm_config::LogLevel;
use fnpm_linker::Layout;
use fnpm_linker::fsutil::{self, Kind};
use fnpm_types::{Behavior, LockGraph, PackageId, Resolution};
use fnpm_workspace::{RootSelection, SelectOptions, Selector};
use serde_json::{Value, json};

use super::{Ctx, exit};
use crate::{CoreResult, Error, Result};

const ROOT_DIR: &str = "node_modules";
const STORE_DIR: &str = "node_modules/.fnpm";

#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderKind {
    NodeModules,
    Scope { parent: usize },
    Store,
}

struct Folder {
    /// How rows name it, relative to the project root, `/`-separated.
    rel: String,
    /// Where it is on disk, relative to the project root (a workspace's own folder for the
    /// tree that bun names `node_modules/<workspace>/node_modules`).
    disk: String,
    kind: FolderKind,
    direct: Option<Vec<String>>,
    touched: bool,
}

struct Removal {
    folder: usize,
    name: String,
    is_link: bool,
    alias: String,
    version: Option<String>,
}

#[derive(Default)]
struct Plan {
    folders: Vec<Folder>,
    removals: Vec<Removal>,
    checked: usize,
    /// No install runs, so staging folders are leftovers of a killed install.
    leftovers: bool,
    /// `node_modules` folders that are symlinks: the plan is not run.
    linked_folders: Vec<String>,
}

/// One entry of a `node_modules` folder.
struct Entry<'a> {
    alias: &'a str,
    /// The path of the entry.
    path: &'a Path,
    is_link: bool,
}

fn join(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

/// `name@version[+peers]` or `@scope+name@version`: the alias and the version of a store folder.
fn split_store_key(key: &str) -> (String, Option<String>) {
    let Some(at) = key.get(1..).and_then(|k| k.find('@')).map(|i| i + 1) else {
        return (key.to_owned(), None);
    };
    let (name, version) = (&key[..at], &key[at + 1..]);
    let alias = match name.find('+') {
        Some(plus) if name.starts_with('@') => format!("{}/{}", &name[..plus], &name[plus + 1..]),
        _ => name.to_owned(),
    };
    (alias, Some(version.to_owned()))
}

impl Plan {
    fn push_folder(&mut self, rel: &str, disk: &str, kind: FolderKind) -> usize {
        self.folders.push(Folder {
            rel: rel.to_owned(),
            disk: disk.to_owned(),
            kind,
            direct: None,
            touched: false,
        });
        self.folders.len() - 1
    }

    fn remove(&mut self, folder: usize, name: &str, is_link: bool) {
        let (alias, version) = match self.folders[folder].kind {
            FolderKind::NodeModules => (name.to_owned(), None),
            FolderKind::Scope { parent } => {
                let scope = &self.folders[folder].rel[self.folders[parent].rel.len() + 1..];
                (format!("{scope}/{name}"), None)
            }
            FolderKind::Store => split_store_key(name),
        };
        self.removals.push(Removal {
            folder,
            name: name.to_owned(),
            is_link,
            alias,
            version,
        });
        self.folders[folder].touched = true;
        if let FolderKind::Scope { parent } = self.folders[folder].kind {
            self.folders[parent].touched = true;
        }
    }

    fn disk(&self, r: &Removal) -> String {
        join(&self.folders[r.folder].disk, &r.name)
    }

    /// The `node_modules` a row lives in; empty for the root folder and the store.
    fn location(&self, r: &Removal) -> &str {
        let folder = &self.folders[r.folder];
        let rel = match folder.kind {
            FolderKind::NodeModules => &folder.rel,
            FolderKind::Scope { parent } => &self.folders[parent].rel,
            FolderKind::Store => return "",
        };
        if rel == ROOT_DIR { "" } else { rel }
    }

    fn resolve_versions(&mut self, root: &Path) {
        for i in 0..self.removals.len() {
            if self.removals[i].version.is_some() {
                continue;
            }
            let path = root.join(self.disk(&self.removals[i]));
            if let Some((_, Some(version))) = installed_package_json(&path) {
                self.removals[i].version = Some(version);
            }
        }
    }

    fn sort_rows(&mut self) {
        let mut removals = std::mem::take(&mut self.removals);
        removals.sort_by(|a, b| {
            (&a.alias, self.location(a), &a.version).cmp(&(&b.alias, self.location(b), &b.version))
        });
        self.removals = removals;
    }

    fn row(&self, r: &Removal) -> String {
        let esc = fnpm_types::text::escape_control_chars;
        let mut line = format!("- {}", esc(&r.alias));
        if let Some(v) = &r.version {
            line.push('@');
            line.push_str(&esc(v));
        }
        let location = self.location(r);
        if !location.is_empty() {
            line.push_str(&format!(" ({})", esc(location)));
        }
        line
    }
}

/// `(name, version)` of the package.json in `dir`.
fn installed_package_json(dir: &Path) -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v = super::json_text::parse_package_json(&text).ok()?;
    let name = v.get("name")?.as_str()?.to_owned();
    let version = v.get("version").and_then(Value::as_str).map(str::to_owned);
    Some((name, version))
}

/// The installed package.json lists bundled dependencies.
fn declares_bundled_deps(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
        return false;
    };
    let Ok(v) = super::json_text::parse_package_json(&text) else {
        return false;
    };
    ["bundleDependencies", "bundledDependencies"]
        .iter()
        .any(|k| match v.get(*k) {
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Bool(b)) => *b,
            _ => false,
        })
}

/// Only directories and symlinks, sorted; names starting with a dot are skipped unless
/// `leftovers` keeps the leftovers of a killed install.
fn read_entries(dir: &Path, leftovers: bool) -> Vec<(String, bool)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !(leftovers && is_leftover(&name)) {
            continue;
        }
        match fsutil::kind(&dir.join(&name)) {
            Kind::Dir => out.push((name, false)),
            Kind::Symlink => out.push((name, true)),
            _ => {}
        }
    }
    out.sort();
    out
}

/// What a killed install leaves behind: fnpm's staging names, and bun's (a bun install
/// switched to fnpm).
fn is_leftover(name: &str) -> bool {
    fsutil::is_leftover(name)
        || [".bun-tmp-", ".old_modules-", ".old-"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// A folder (or one of its scopes) holds leftovers of a killed install.
fn has_leftovers(dir: &Path) -> bool {
    read_entries(dir, true).iter().any(|(name, is_link)| {
        name.starts_with('.')
            || (name.starts_with('@')
                && !is_link
                && read_entries(&dir.join(name), true)
                    .iter()
                    .any(|(inner, _)| inner.starts_with('.')))
    })
}

fn is_dangling(path: &Path) -> bool {
    std::fs::metadata(path).is_err()
}

fn real_dir(path: &Path) -> bool {
    fsutil::kind(path) == Kind::Dir
}

/// A `node_modules` of the project or a workspace: never followed when it is a symlink.
fn open_workspace_folder(root: &Path, rel: &str, linked: &mut Vec<String>) -> Option<PathBuf> {
    let abs = root.join(rel);
    match fsutil::kind(&abs) {
        Kind::Dir => Some(abs),
        Kind::Symlink => {
            linked.push(rel.to_owned());
            None
        }
        _ => None,
    }
}

fn scan_folder(
    root: &Path,
    (rel, disk): (&str, &str),
    touched: bool,
    keep: &dyn Fn(&Entry) -> bool,
    plan: &mut Plan,
) -> usize {
    let idx = plan.push_folder(rel, disk, FolderKind::NodeModules);
    plan.folders[idx].touched = touched;
    let abs = root.join(disk);
    for (name, is_link) in read_entries(&abs, plan.leftovers) {
        if name.starts_with('.') {
            plan.remove(idx, &name, is_link);
            continue;
        }
        if name.starts_with('@') && !is_link {
            let scope_rel = join(rel, &name);
            let scope_disk = join(disk, &name);
            let scope_idx =
                plan.push_folder(&scope_rel, &scope_disk, FolderKind::Scope { parent: idx });
            let scope_abs = abs.join(&name);
            for (inner, inner_link) in read_entries(&scope_abs, plan.leftovers) {
                if inner.starts_with('.') {
                    plan.remove(scope_idx, &inner, inner_link);
                    continue;
                }
                let alias = format!("{name}/{inner}");
                let path = scope_abs.join(&inner);
                plan.checked += 1;
                if !keep(&Entry {
                    alias: &alias,
                    path: &path,
                    is_link: inner_link,
                }) {
                    plan.remove(scope_idx, &inner, inner_link);
                }
            }
            continue;
        }
        let path = abs.join(&name);
        plan.checked += 1;
        if !keep(&Entry {
            alias: &name,
            path: &path,
            is_link,
        }) {
            plan.remove(idx, &name, is_link);
        }
    }
    idx
}

fn extracted(r: &Resolution) -> bool {
    matches!(
        r,
        Resolution::Npm { .. }
            | Resolution::LocalTarball { .. }
            | Resolution::RemoteTarball { .. }
            | Resolution::Git { .. }
            | Resolution::Github { .. }
    )
}

fn has_bundled_deps(graph: &LockGraph, id: PackageId) -> bool {
    graph
        .get(id)
        .is_some_and(|p| p.dependencies.iter().any(|e| e.behavior.is_bundled()))
}

/// `<path>/node_modules` of a workspace inside the project.
fn workspace_node_modules(graph: &LockGraph, id: PackageId) -> Option<String> {
    match &graph.get(id)?.resolution {
        Resolution::Workspace { path } => {
            let path = path.trim_end_matches('/');
            (!path.is_empty() && !path.starts_with("..")).then(|| join(path, ROOT_DIR))
        }
        _ => None,
    }
}

/// The importers `--filter` selects, and the names that the others' closures use (an
/// unselected importer's dependencies stay).
struct Selection {
    selected: HashSet<PackageId>,
    protected_aliases: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Installed {
    Matches,
    Missing,
    Mismatch,
}

struct TreeFolder {
    /// The tree's name for it (`node_modules/<workspace>/node_modules` for a workspace).
    rel: String,
    /// Where it is on disk.
    disk: String,
    parent: Option<usize>,
    /// Sorted by alias.
    expected: Vec<(String, PackageId)>,
    /// The tree node whose `node_modules` this is.
    owner: Option<u32>,
    importer: PackageId,
}

/// The hoisted tree as folders (bun `HoistedTree`).
struct HoistedTree<'a> {
    graph: &'a LockGraph,
    root: &'a Path,
    folders: Vec<TreeFolder>,
    /// Node index → its own folder (where its children go).
    children_folder: HashMap<u32, usize>,
    /// Node index → `(folder that contains it, alias)`.
    node_at: Vec<(usize, String)>,
    quiet: bool,
    kept_mismatched: std::cell::Cell<bool>,
    cache: std::cell::RefCell<HashMap<(usize, String), Installed>>,
}

impl<'a> HoistedTree<'a> {
    fn new(
        graph: &'a LockGraph,
        tree: &fnpm_lockfile::tree::Tree,
        root: &'a Path,
        quiet: bool,
    ) -> Self {
        let mut folders = vec![TreeFolder {
            rel: ROOT_DIR.into(),
            disk: ROOT_DIR.into(),
            parent: None,
            expected: Vec::new(),
            owner: None,
            importer: PackageId::ROOT,
        }];
        let mut in_workspace: HashMap<u32, usize> = HashMap::new();
        for (ws, nodes) in &tree.workspace_roots {
            let Some(disk) = workspace_node_modules(graph, *ws) else {
                continue;
            };
            let name = &graph.packages[ws.index()].name;
            folders.push(TreeFolder {
                rel: join(&join(ROOT_DIR, name), ROOT_DIR),
                disk,
                parent: Some(0),
                expected: Vec::new(),
                owner: None,
                importer: *ws,
            });
            for &n in nodes {
                in_workspace.insert(n, folders.len() - 1);
            }
        }
        let mut children_folder: HashMap<u32, usize> = HashMap::new();
        let mut node_at: Vec<(usize, String)> = Vec::with_capacity(tree.nodes.len());
        for (i, node) in tree.nodes.iter().enumerate() {
            let folder = match node.parent {
                None => in_workspace.get(&(i as u32)).copied().unwrap_or(0),
                Some(p) => match children_folder.get(&p) {
                    Some(&f) => f,
                    None => {
                        let (pf, palias) = node_at[p as usize].clone();
                        let rel = join(&join(&folders[pf].rel, &palias), ROOT_DIR);
                        let disk = join(&join(&folders[pf].disk, &palias), ROOT_DIR);
                        let importer = folders[pf].importer;
                        folders.push(TreeFolder {
                            rel,
                            disk,
                            parent: Some(pf),
                            expected: Vec::new(),
                            owner: Some(p),
                            importer,
                        });
                        children_folder.insert(p, folders.len() - 1);
                        folders.len() - 1
                    }
                },
            };
            folders[folder]
                .expected
                .push((node.name.clone(), node.package));
            node_at.push((folder, node.name.clone()));
        }
        for f in &mut folders {
            f.expected.sort();
            f.expected.dedup_by(|a, b| a.0 == b.0);
        }
        HoistedTree {
            graph,
            root,
            folders,
            children_folder,
            node_at,
            quiet,
            kept_mismatched: std::cell::Cell::new(false),
            cache: Default::default(),
        }
    }

    fn expected_in(&self, folder: usize, alias: &str) -> Option<PackageId> {
        let e = &self.folders.get(folder)?.expected;
        e.binary_search_by(|(n, _)| n.as_str().cmp(alias))
            .ok()
            .map(|i| e[i].1)
    }

    fn expected_from(&self, from: Option<usize>, alias: &str) -> Option<(usize, PackageId)> {
        let mut id = from;
        while let Some(f) = id {
            if let Some(pkg) = self.expected_in(f, alias) {
                return Some((f, pkg));
            }
            id = self.folders[f].parent;
        }
        None
    }

    fn installed(&self, folder: usize, alias: &str, pkg: PackageId) -> Installed {
        let key = (folder, alias.to_owned());
        if let Some(&i) = self.cache.borrow().get(&key) {
            return i;
        }
        let i = self.check_installed(folder, alias, pkg);
        self.cache.borrow_mut().insert(key, i);
        i
    }

    fn check_installed(&self, folder: usize, alias: &str, pkg: PackageId) -> Installed {
        let Some(p) = self.graph.get(pkg) else {
            return Installed::Mismatch;
        };
        let path = self.root.join(&self.folders[folder].disk).join(alias);
        let kind = fsutil::kind(&path);
        match kind {
            Kind::Missing => return Installed::Missing,
            Kind::File => return Installed::Mismatch,
            _ => {}
        }
        match &p.resolution {
            Resolution::Symlink { .. } | Resolution::Workspace { .. } => {
                return if kind == Kind::Symlink {
                    Installed::Matches
                } else {
                    Installed::Mismatch
                };
            }
            Resolution::Folder { .. } if kind == Kind::Symlink => return Installed::Matches,
            _ => {}
        }
        if kind == Kind::Symlink && is_dangling(&path) {
            return Installed::Missing;
        }
        let version = match &p.resolution {
            Resolution::Npm { version, .. } => version.as_str(),
            _ => p.version.as_str(),
        };
        let matches = match &p.resolution {
            Resolution::Folder { .. }
            | Resolution::LocalTarball { .. }
            | Resolution::RemoteTarball { .. } => {
                installed_package_json(&path).is_some_and(|(name, _)| name == p.name)
            }
            r => fnpm_linker::verify::verify(
                &path,
                &fnpm_linker::verify::expected_for(&p.name, version, r),
            ),
        };
        if matches {
            Installed::Matches
        } else {
            Installed::Mismatch
        }
    }

    fn is_installed(&self, folder: usize, alias: &str) -> bool {
        self.expected_in(folder, alias)
            .is_some_and(|pkg| self.installed(folder, alias, pkg) == Installed::Matches)
    }

    /// Whether every package that contains `folder` is the version the tree expects there.
    fn owners_installed(&self, mut folder: usize) -> bool {
        while let Some(owner) = self.folders[folder].owner {
            let (at, alias) = &self.node_at[owner as usize];
            if !self.is_installed(*at, alias) {
                return false;
            }
            folder = *at;
        }
        true
    }

    fn removable(&self, from: Option<usize>, alias: &str, entry_folder: &str) -> bool {
        let Some((at, pkg)) = self.expected_from(from, alias) else {
            return true;
        };
        let installed = self.installed(at, alias, pkg);
        if installed == Installed::Matches {
            return true;
        }
        self.kept_mismatched.set(true);
        if !self.quiet {
            let why = match installed {
                Installed::Missing => "is missing",
                _ => "is not the version fnpm.lock expects",
            };
            let esc = fnpm_types::text::escape_control_chars;
            eprintln!(
                "warn: {} {why}; keeping {}",
                esc(&join(&self.folders[at].rel, alias)),
                esc(&join(entry_folder, alias))
            );
        }
        false
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn elapsed(start: Instant) -> String {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    if ms >= 1000.0 {
        format!("[{:.2}s]", ms / 1000.0)
    } else {
        format!("[{ms:.2}ms]")
    }
}

/// Writes to stdout right away, so that the order with stderr is the order of events.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The workspaces fnpm.lock lists that are not on disk (a pruned checkout).
fn pruned_workspaces(graph: &LockGraph, root: &Path) -> Vec<(PackageId, String)> {
    let mut out: Vec<(PackageId, String)> = graph
        .workspaces
        .iter()
        .filter(|(path, _)| !path.is_empty() && !root.join(path).join("package.json").is_file())
        .map(|(_, id)| (*id, graph.packages[id.index()].name.clone()))
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// `<who> depends on workspace "<name>" (<path>), which is listed in fnpm.lock but not on disk`
/// when the root or a workspace on disk depends on a workspace that is gone.
pub(crate) fn missing_workspace_dependency(graph: &LockGraph, root: &Path) -> Option<String> {
    let pruned: HashSet<PackageId> = pruned_workspaces(graph, root)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    if pruned.is_empty() {
        return None;
    }
    for (i, p) in graph.packages.iter().enumerate() {
        if pruned.contains(&PackageId(i as u32))
            || !matches!(
                p.resolution,
                Resolution::Root | Resolution::Workspace { .. }
            )
        {
            continue;
        }
        for e in &p.dependencies {
            let Some(t) = e.target.filter(|t| pruned.contains(t)) else {
                continue;
            };
            // The root's membership rows are not dependencies.
            if i == 0 && graph.workspaces.get(&e.literal) == Some(&t) {
                continue;
            }
            let path = match &graph.packages[t.index()].resolution {
                Resolution::Workspace { path } => path.clone(),
                _ => String::new(),
            };
            let who = if i == 0 {
                "the root package".to_owned()
            } else {
                format!("workspace \"{}\"", p.name)
            };
            return Some(format!(
                "{who} depends on workspace \"{}\" ({path}), which is listed in fnpm.lock but not on disk",
                graph.packages[t.index()].name
            ));
        }
    }
    None
}

/// The workspace, with listed members that are not on disk skipped (a pruned checkout, like a
/// Docker layer that copied only package.json and fnpm.lock).
pub(crate) fn workspace_on_disk(
    project: &crate::project::Project,
    events: &dyn fnpm_types::EventSink,
) -> Result<fnpm_workspace::Workspace> {
    let (ws, warnings) = fnpm_workspace::Workspace::load_with(
        &project.root_dir,
        &fnpm_workspace::LoadOptions {
            missing: fnpm_workspace::MissingWorkspace::Skip,
            ..Default::default()
        },
    )?;
    for w in warnings {
        events.emit(fnpm_types::Event::warn(w, None));
    }
    Ok(ws)
}

/// `note: skipped N workspace(s) listed in fnpm.lock but not on disk: "a", "b"`.
pub(crate) fn pruned_note(graph: &LockGraph, root: &Path) -> Option<String> {
    let pruned = pruned_workspaces(graph, root);
    if pruned.is_empty() {
        return None;
    }
    let names: Vec<String> = pruned.iter().map(|(_, n)| format!("\"{n}\"")).collect();
    Some(format!(
        "note: skipped {} workspace{} listed in fnpm.lock but not on disk: {}",
        pruned.len(),
        plural(pruned.len()),
        names.join(", ")
    ))
}

/// `fnpm prune`.
pub(crate) fn prune(cx: &Ctx) -> Result<CoreResult> {
    let start = Instant::now();
    let quiet = cx.pm.log_level == LogLevel::Silent;
    let summary = !quiet && cx.pm.summary;
    // A usage error: `--silent` does not hide it.
    if !cx.req.positionals.is_empty() {
        eprintln!(
            "error: fnpm prune does not take arguments, it always prunes the whole node_modules"
        );
        eprintln!("note: run 'fnpm prune --help' for more information");
        return Err(Error::Reported);
    }
    let project = match cx.project() {
        Ok(p) => p,
        Err(Error::Workspace(fnpm_workspace::Error::NoProject(_))) => {
            if !quiet {
                eprintln!("error: missing package.json, nothing to prune");
            }
            return Err(Error::Reported);
        }
        Err(e) => return Err(e),
    };
    if summary {
        say(&format!(
            "fnpm prune v{}\n",
            cx.string("cliVersion")
                .unwrap_or_else(|| crate::version().to_owned())
        ));
    }
    let ws = workspace_on_disk(&project, &*cx.events)?;
    let lock = match crate::install::load_lock(&project.config, &ws) {
        Ok(lock) => lock,
        Err(e) => {
            if !quiet {
                eprintln!("error: failed to load lockfile: {e}");
            }
            return Err(Error::Reported);
        }
    };
    let Some(graph) = lock.graph else {
        if let Some(w) = lock.warnings.first()
            && project.root_dir.join(fnpm_lockfile::FNPM_LOCK).exists()
        {
            if !quiet {
                eprintln!("error: failed to load lockfile: {w}");
            }
            return Err(Error::Reported);
        }
        if !quiet {
            eprintln!("error: missing lockfile, nothing to prune");
            eprintln!("note: run 'fnpm install' first");
        }
        return Err(Error::Reported);
    };
    let root = project.root_dir.clone();
    let pruned = pruned_workspaces(&graph, &root);
    let pruned_ids: HashSet<PackageId> = pruned.iter().map(|(id, _)| *id).collect();
    // A workspace that is gone from disk is not a change to package.json (a pruned checkout).
    let removed_paths: HashSet<String> = graph
        .workspaces
        .iter()
        .filter(|(_, id)| pruned_ids.contains(id))
        .map(|(path, _)| format!(": {path} was removed"))
        .collect();
    let changes: Vec<String> = fnpm_lockfile::out_of_sync(&graph, &ws)
        .into_iter()
        .filter(|c| !removed_paths.iter().any(|r| c.ends_with(r.as_str())))
        .collect();
    if !changes.is_empty() {
        if !quiet {
            eprintln!("error: fnpm.lock does not match package.json, nothing to prune");
            eprintln!("note: run 'fnpm install' first");
        }
        return Err(Error::Reported);
    }
    if !pruned.is_empty() && !quiet {
        let names: Vec<String> = pruned.iter().map(|(_, n)| format!("\"{n}\"")).collect();
        eprintln!(
            "note: skipped {} workspace{} listed in fnpm.lock but not on disk: {}",
            pruned.len(),
            plural(pruned.len()),
            names.join(", ")
        );
    }
    // What is left must not depend on what is gone, like an install.
    if let Some(message) = missing_workspace_dependency(&graph, &root) {
        if !quiet {
            eprintln!("error: {message}");
        }
        return Err(Error::Reported);
    }
    // The graph as if the missing workspaces had never been there.
    let graph = if pruned_ids.is_empty() {
        graph
    } else {
        let mut g = graph;
        g.workspaces.retain(|_, id| !pruned_ids.contains(id));
        for p in &mut g.packages {
            p.dependencies
                .retain(|e| !e.target.is_some_and(|t| pruned_ids.contains(&t)));
        }
        g
    };

    if fsutil::kind(&root.join(ROOT_DIR)) == Kind::File {
        if !quiet {
            eprintln!("error: ENOTDIR: failed to open node_modules");
        }
        return Err(Error::Reported);
    }
    if fsutil::kind(&root.join(ROOT_DIR)) == Kind::Missing {
        if summary {
            say(&format!(
                "Done! No node_modules folder (nothing to prune) {}",
                elapsed(start)
            ));
        }
        return Ok(exit(0, json!({ "removed": [] }), Vec::new()));
    }

    let view = crate::install::link_view(&graph);
    let (_, layout) = super::maintenance::current_layout(&project, &graph)?;
    let is_isolated = matches!(layout, Layout::Isolated(_));
    let workspace_names: HashSet<String> = view
        .packages
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            matches!(p.resolution, Resolution::Workspace { .. })
                && !pruned_ids.contains(&PackageId(*i as u32))
        })
        .map(|(_, p)| p.name.clone())
        .collect();
    let selection = select_importers(cx, &ws, &view, &pruned_ids, quiet)?;

    let mut plan = Plan {
        leftovers: true,
        ..Default::default()
    };
    let _ = open_workspace_folder(&root, ROOT_DIR, &mut plan.linked_folders);
    let hoisted = match &layout {
        Layout::Hoisted(tree) => {
            let h = HoistedTree::new(&view, tree, &root, quiet);
            plan_hoisted(
                &h,
                &workspace_names,
                selection.as_ref(),
                &pruned_ids,
                &mut plan,
            );
            Some(h)
        }
        Layout::Isolated(iso) => {
            plan_isolated(
                &project,
                &graph,
                iso,
                &workspace_names,
                selection.as_ref(),
                &pruned_ids,
                &mut plan,
            )?;
            None
        }
    };
    if !plan.linked_folders.is_empty() {
        if !quiet {
            plan.linked_folders.sort();
            plan.linked_folders.dedup();
            for folder in &plan.linked_folders {
                eprintln!(
                    "error: cannot prune symlink {}",
                    fnpm_types::text::escape_control_chars(folder)
                );
            }
            eprintln!("note: to prune this project, remove the link and run 'fnpm install'");
        }
        return Err(Error::Reported);
    }
    if let Some(h) = &hoisted
        && h.kept_mismatched.get()
        && !quiet
    {
        eprintln!("note: run 'fnpm install' first");
    }

    let n = plan.removals.len();
    let checked = plan.checked;
    if n == 0 {
        if summary {
            let folders = plan
                .folders
                .iter()
                .filter(|f| matches!(f.kind, FolderKind::NodeModules | FolderKind::Store))
                .count();
            say(&format!(
                "Done! Checked {checked} installed package{} across {folders} folder{} (nothing to prune) {}",
                plural(checked),
                plural(folders),
                elapsed(start)
            ));
        }
        return Ok(exit(
            0,
            json!({ "removed": [], "checked": checked }),
            Vec::new(),
        ));
    }
    plan.resolve_versions(&root);
    plan.sort_rows();
    let names: Vec<Value> = plan
        .removals
        .iter()
        .map(|r| json!({ "name": r.alias, "version": r.version, "location": plan.location(r) }))
        .collect();

    let check = cx.flag("check");
    if cx.pm.dry_run || project.config.dry_run || check {
        if !quiet {
            for r in &plan.removals {
                say(&plan.row(r));
            }
            say(&format!(
                "{n} package{} can be removed (checked {checked} installed package{}) {}",
                plural(n),
                plural(checked),
                elapsed(start)
            ));
            say(&apply_hint());
        }
        return Ok(exit(
            i32::from(check),
            json!({ "removed": names, "checked": checked }),
            Vec::new(),
        ));
    }

    let failed = execute(&plan, &root, quiet);
    let lost_bins = housekeeping(&plan, &root, is_isolated, &layout);
    hand_over_names(&plan, &lost_bins, &layout, hoisted.as_ref(), &view, &root);
    crate::fast_path::remove(&root, &project.meta_dir());
    if !quiet {
        let removed = n - failed;
        let mut line = format!("{removed} package{} removed", plural(removed));
        if failed > 0 {
            line.push_str(&format!(", {failed} failed"));
        }
        line.push_str(&format!(
            " (checked {checked} installed package{}) {}",
            plural(checked),
            elapsed(start)
        ));
        say(&line);
    }
    Ok(exit(
        i32::from(failed > 0),
        json!({ "removed": names, "checked": checked }),
        Vec::new(),
    ))
}

/// The line `--dry-run` ends with: the invocation without `--dry-run` / `--check`.
fn apply_hint() -> String {
    let mut line = String::from("  fnpm prune");
    let args: Vec<String> = std::env::args().collect();
    if let Some(at) = args.iter().position(|a| a == "prune") {
        for arg in &args[at + 1..] {
            if arg.starts_with("--dry-run") || arg.starts_with("--check") {
                continue;
            }
            line.push(' ');
            line.push_str(arg);
        }
    }
    line
}

fn select_importers(
    cx: &Ctx,
    ws: &fnpm_workspace::Workspace,
    graph: &LockGraph,
    pruned: &HashSet<PackageId>,
    quiet: bool,
) -> Result<Option<Selection>> {
    let filters = cx.strings("filter");
    if filters.is_empty() {
        return Ok(None);
    }
    let selectors = Selector::parse_all(&filters)?;
    let sel = fnpm_workspace::select_with(
        ws,
        &selectors,
        &cx.req.cwd,
        &SelectOptions {
            root: RootSelection::Implicit,
            ..SelectOptions::default()
        },
    );
    let selected: HashSet<PackageId> = sel
        .selected
        .iter()
        .filter_map(|&i| {
            let path = if i == usize::MAX {
                String::new()
            } else {
                ws.members[i].path.clone()
            };
            graph.workspaces.get(&path).copied()
        })
        .collect();
    if selected.is_empty() {
        if !quiet {
            eprintln!("error: {}", fnpm_workspace::unmatched_message(&filters));
        }
        return Err(Error::Reported);
    }
    if !quiet && !sel.unmatched_patterns.is_empty() {
        let unmatched: Vec<&String> = sel
            .unmatched_patterns
            .iter()
            .filter_map(|&i| filters.get(i))
            .collect();
        eprintln!("warn: {}", fnpm_workspace::unmatched_message(&unmatched));
    }
    let is_importer = |id: PackageId| {
        matches!(
            graph.packages[id.index()].resolution,
            Resolution::Root | Resolution::Workspace { .. }
        )
    };
    let mut protected_aliases = HashSet::new();
    let mut visited = vec![false; graph.packages.len()];
    for (i, _) in graph.packages.iter().enumerate() {
        let id = PackageId(i as u32);
        if selected.contains(&id) || !is_importer(id) || pruned.contains(&id) {
            continue;
        }
        visited[i] = true;
        let mut work = vec![id];
        while let Some(p) = work.pop() {
            for e in &graph.packages[p.index()].dependencies {
                if let Some(t) = e.target
                    && is_importer(t)
                    && pruned.contains(&t)
                {
                    continue;
                }
                protected_aliases.insert(e.name.clone());
                let Some(t) = e.target else { continue };
                if visited[t.index()] {
                    continue;
                }
                visited[t.index()] = true;
                if !is_importer(t) {
                    work.push(t);
                }
            }
        }
    }
    Ok(Some(Selection {
        selected,
        protected_aliases,
    }))
}

fn plan_hoisted(
    h: &HoistedTree<'_>,
    workspace_names: &HashSet<String>,
    selection: Option<&Selection>,
    pruned: &HashSet<PackageId>,
    plan: &mut Plan,
) {
    let graph = h.graph;
    let root = h.root;
    let empty = HashSet::new();
    let root_protected = selection.map_or(&empty, |s| &s.protected_aliases);
    let mut skipped: HashSet<usize> = HashSet::new();
    let mut visited: HashSet<PackageId> = HashSet::new();
    for fid in 0..h.folders.len() {
        let folder = &h.folders[fid];
        if let Some(parent) = folder.parent
            && skipped.contains(&parent)
            && folder.owner.is_some()
        {
            skipped.insert(fid);
            continue;
        }
        if skipped.contains(&fid) {
            continue;
        }
        let importer = folder.importer;
        if pruned.contains(&importer) {
            continue;
        }
        if selection.is_some_and(|s| importer != PackageId::ROOT && !s.selected.contains(&importer))
        {
            continue;
        }
        let protected = if importer == PackageId::ROOT {
            root_protected
        } else {
            &empty
        };
        if folder.owner.is_none() {
            visited.insert(importer);
        }
        if let Some(owner) = folder.owner {
            let (at, alias) = &h.node_at[owner as usize];
            let pkg = h.folders[*at]
                .expected
                .iter()
                .find(|(n, _)| n == alias)
                .map(|(_, p)| *p);
            if pkg.is_some_and(|p| has_bundled_deps(graph, p)) {
                continue;
            }
        }
        let abs = root.join(&folder.disk);
        let exists = if folder.owner.is_none() {
            open_workspace_folder(root, &folder.disk, &mut plan.linked_folders).is_some()
        } else {
            real_dir(&abs) && h.owner_dirs_real(fid)
        };
        if !exists {
            continue;
        }
        for (alias, pkg) in &folder.expected {
            let Some(p) = graph.get(*pkg) else { continue };
            if !extracted(&p.resolution) || has_bundled_deps(graph, *pkg) {
                continue;
            }
            let package = abs.join(alias);
            if !real_dir(&package) {
                continue;
            }
            if let Some(&child) = h
                .children_folder
                .get(&h.node_index(fid, alias).unwrap_or(u32::MAX))
            {
                if declares_bundled_deps(&package) {
                    skipped.insert(child);
                }
                continue;
            }
            if !real_dir(&package.join(ROOT_DIR)) || declares_bundled_deps(&package) {
                continue;
            }
            let nested_rel = join(&join(&folder.rel, alias), ROOT_DIR);
            let nested_disk = join(&join(&folder.disk, alias), ROOT_DIR);
            scan_folder(
                root,
                (&nested_rel, &nested_disk),
                false,
                &|entry: &Entry| {
                    protected.contains(entry.alias)
                        || !h.is_installed(fid, alias)
                        || !h.owners_installed(fid)
                        || !h.removable(Some(fid), entry.alias, &nested_rel)
                },
                plan,
            );
        }
        let parent = folder.parent;
        let rel = folder.rel.clone();
        let disk = folder.disk.clone();
        scan_folder(
            root,
            (&rel, &disk),
            false,
            &|entry: &Entry| {
                h.expected_in(fid, entry.alias).is_some()
                    // A workspace's name is only kept in the root `node_modules`.
                    || (fid == 0 && workspace_names.contains(entry.alias))
                    || protected.contains(entry.alias)
                    || !h.owners_installed(fid)
                    || !h.removable(parent, entry.alias, &rel)
            },
            plan,
        );
    }
    // Workspaces the tree places nothing for still get their `node_modules` checked.
    for (i, _) in graph.packages.iter().enumerate() {
        let id = PackageId(i as u32);
        let Some(rel) = workspace_node_modules(graph, id) else {
            continue;
        };
        if visited.contains(&id)
            || selection.is_some_and(|s| !s.selected.contains(&id))
            || pruned.contains(&id)
        {
            continue;
        }
        if open_workspace_folder(root, &rel, &mut plan.linked_folders).is_some() {
            scan_folder(
                root,
                (&rel, &rel),
                false,
                &|entry: &Entry| !h.removable(Some(0), entry.alias, &rel),
                plan,
            );
        }
    }
}

impl HoistedTree<'_> {
    /// The node placed as `alias` in `folder`.
    fn node_index(&self, folder: usize, alias: &str) -> Option<u32> {
        self.node_at
            .iter()
            .position(|(f, a)| *f == folder && a == alias)
            .map(|i| i as u32)
    }

    /// Every package folder on the way to `folder` is a real directory (links are not followed).
    fn owner_dirs_real(&self, mut folder: usize) -> bool {
        while let Some(owner) = self.folders[folder].owner {
            let (at, alias) = &self.node_at[owner as usize];
            if !real_dir(&self.root.join(&self.folders[*at].disk).join(alias)) {
                return false;
            }
            folder = *at;
        }
        true
    }
}

/// The store folder a link points into (`.../.fnpm/<key>/...`).
fn store_link_target(path: &Path) -> Option<String> {
    let link = std::fs::read_link(path).ok()?;
    let mut parts = link.components();
    while let Some(c) = parts.next() {
        if c.as_os_str() == ".fnpm" {
            return parts
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .filter(|e| e.contains('@'));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn plan_isolated(
    project: &crate::project::Project,
    graph: &LockGraph,
    own: &fnpm_linker::IsolatedLayout,
    workspace_names: &HashSet<String>,
    selection: Option<&Selection>,
    pruned: &HashSet<PackageId>,
    plan: &mut Plan,
) -> Result<()> {
    let root = &project.root_dir;
    let view = crate::install::link_view(graph);
    let config = &project.config;
    let walk = super::licenses::Walk {
        graph: &view,
        dev: !config.production && !config.omit.dev,
        optional: !config.omit.optional,
        peer: !config.omit.peer,
        optional_peer: !config.omit.peer,
        platform: Some(&config.platform),
        follow_workspace_edges: true,
    };
    let importers: Vec<PackageId> = view
        .workspaces
        .values()
        .copied()
        .filter(|id| !pruned.contains(id))
        .collect();
    let mut wanted = walk.packages_from(&importers);
    if let Some(sel) = selection {
        let unselected: Vec<PackageId> = importers
            .iter()
            .copied()
            .filter(|id| !sel.selected.contains(id))
            .collect();
        let full = super::licenses::Walk {
            dev: true,
            optional: true,
            peer: true,
            optional_peer: true,
            follow_workspace_edges: false,
            ..walk
        };
        for (i, w) in full.packages_from(&unselected).into_iter().enumerate() {
            wanted[i] |= w;
        }
    }
    // Entries an install with every dependency type and one with this run's options would
    // create (their peer hashes can differ).
    let mut full_config = fnpm_config::Config::clone(config);
    full_config.production = false;
    full_config.omit = Default::default();
    let full_linker = fnpm_linker::Linker::new(std::sync::Arc::new(full_config), project.store()?);
    let full = match crate::install::plan_layout(
        &full_linker,
        &view,
        fnpm_types::NodeLinker::Isolated,
        None,
        &[],
    ) {
        Layout::Isolated(iso) => iso,
        Layout::Hoisted(_) => fnpm_linker::IsolatedLayout::default(),
    };
    let mut names: HashSet<String> = HashSet::new();
    for layout in [&full, own] {
        for (i, e) in layout.entries.iter().enumerate().skip(1) {
            if matches!(
                view.packages[e.package.index()].resolution,
                Resolution::Root | Resolution::Workspace { .. }
            ) || !wanted.get(e.package.index()).copied().unwrap_or(false)
            {
                continue;
            }
            names.insert(e.key.clone());
            if let Some(g) = layout.global_dir_name(i as u32) {
                names.insert(g);
            }
        }
    }

    let mut removed_store: HashSet<String> = HashSet::new();
    if let Some(store) = open_workspace_folder(root, STORE_DIR, &mut plan.linked_folders) {
        let idx = plan.push_folder(STORE_DIR, STORE_DIR, FolderKind::Store);
        for (name, is_link) in read_entries(&store, false) {
            if name == ROOT_DIR {
                continue;
            }
            plan.checked += 1;
            if names.contains(&name) {
                let nested = join(&join(STORE_DIR, &name), ROOT_DIR);
                if plan.leftovers && has_leftovers(&root.join(&nested)) {
                    let checked = plan.checked;
                    scan_folder(root, (&nested, &nested), false, &|_| true, plan);
                    plan.checked = checked;
                }
                continue;
            }
            plan.remove(idx, &name, is_link);
            removed_store.insert(name);
        }
    }
    let store_touched = !removed_store.is_empty();
    let public: HashSet<&str> = own.public_hoist.iter().map(|(n, _)| n.as_str()).collect();
    let mut importer_ids: Vec<PackageId> = vec![PackageId::ROOT];
    importer_ids.extend(
        view.packages
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.resolution, Resolution::Workspace { .. }))
            .map(|(i, _)| PackageId(i as u32)),
    );
    for id in importer_ids {
        let rel = if id == PackageId::ROOT {
            ROOT_DIR.to_owned()
        } else {
            match workspace_node_modules(&view, id) {
                Some(rel) => rel,
                None => continue,
            }
        };
        if selection.is_some_and(|s| !s.selected.contains(&id)) || pruned.contains(&id) {
            continue;
        }
        let exists = if id == PackageId::ROOT {
            real_dir(&root.join(&rel))
        } else {
            open_workspace_folder(root, &rel, &mut plan.linked_folders).is_some()
        };
        if !exists {
            continue;
        }
        let mut direct: Vec<String> = own
            .roots
            .iter()
            .find(|(pkg, _)| *pkg == id)
            .map(|(_, links)| links.iter().map(|(alias, _)| alias.clone()).collect())
            .unwrap_or_default();
        // The root's membership rows name every workspace (bun links them from there).
        if id == PackageId::ROOT {
            direct.extend(workspace_names.iter().cloned());
        }
        direct.sort();
        direct.dedup();
        let idx = scan_folder(
            root,
            (&rel, &rel),
            store_touched,
            &|entry: &Entry| {
                direct
                    .binary_search_by(|d| d.as_str().cmp(entry.alias))
                    .is_ok()
                    || (!entry.is_link && workspace_names.contains(entry.alias))
                    || public.contains(entry.alias)
                    || (entry.is_link
                        && store_link_target(entry.path)
                            .is_some_and(|target| removed_store.contains(&target)))
            },
            plan,
        );
        plan.folders[idx].direct = Some(direct);
    }
    Ok(())
}

fn execute(plan: &Plan, root: &Path, quiet: bool) -> usize {
    let mut failed = 0;
    let mut removed_from: HashSet<usize> = HashSet::new();
    for r in &plan.removals {
        let path = root.join(plan.disk(r));
        let result = if r.is_link {
            fsutil::remove_link(&path)
        } else {
            std::fs::remove_dir_all(&path)
        };
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!(
                    "error: failed to remove {}: {} ({e})",
                    fnpm_types::text::escape_control_chars(&join(
                        &plan.folders[r.folder].rel,
                        &r.name
                    )),
                    super::pack::out::errno_name(&e)
                );
                failed += 1;
                continue;
            }
        }
        if !quiet {
            say(&plan.row(r));
        }
        removed_from.insert(r.folder);
    }
    for (idx, folder) in plan.folders.iter().enumerate() {
        if let FolderKind::Scope { .. } = folder.kind
            && removed_from.contains(&idx)
        {
            let _ = std::fs::remove_dir(root.join(&folder.disk));
        }
    }
    failed
}

/// Removes the links in `dir` (and its scopes) that `unlink` picks; whether one went.
fn unlink_links(dir: &Path, unlink: &dyn Fn(&Path, &str) -> bool) -> bool {
    let mut any = false;
    for (name, is_link) in read_entries(dir, false) {
        let path = dir.join(&name);
        if is_link {
            if unlink(&path, &name) {
                any |= fsutil::remove_link(&path).is_ok();
            }
        } else if name.starts_with('@') {
            let mut in_scope = false;
            for (inner, inner_link) in read_entries(&path, false) {
                if !inner_link {
                    continue;
                }
                let p = path.join(&inner);
                if unlink(&p, &format!("{name}/{inner}")) {
                    in_scope |= fsutil::remove_link(&p).is_ok();
                }
            }
            if in_scope {
                let _ = std::fs::remove_dir(&path);
            }
            any |= in_scope;
        }
    }
    any
}

/// Whether the package folder that should hold `target` (relative to `node_modules`) exists.
fn leads_into_package(nm: &Path, target: &str) -> bool {
    let parts: Vec<&str> = target.split('/').collect();
    let last_nm = parts
        .iter()
        .rposition(|p| *p == ROOT_DIR)
        .map_or(0, |i| i + 1);
    let take = if parts.get(last_nm).is_some_and(|p| p.starts_with('@')) {
        last_nm + 2
    } else {
        last_nm + 1
    };
    if take >= parts.len() {
        return false;
    }
    !is_dangling(&nm.join(parts[..take].join("/")))
}

/// Removes `.bin` links whose target is gone, unless their package is still installed (its
/// build writes the file). Whether one went.
fn prune_bins(nm: &Path) -> bool {
    let bin = nm.join(".bin");
    // A symlinked `.bin` is never cleaned through.
    if !real_dir(&bin) {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(&bin) else {
        return false;
    };
    let mut removed = false;
    for e in rd.flatten() {
        let path = e.path();
        if fsutil::kind(&path) != Kind::Symlink || !is_dangling(&path) {
            continue;
        }
        if let Ok(link) = std::fs::read_link(&path)
            && let Some(rest) = link.to_str().and_then(|l| l.strip_prefix("../"))
            && leads_into_package(nm, rest)
        {
            continue;
        }
        removed |= fsutil::remove_link(&path).is_ok();
    }
    removed
}

/// Folder indexes whose `.bin` lost a link.
fn housekeeping(plan: &Plan, root: &Path, isolated: bool, layout: &Layout) -> Vec<usize> {
    let public: HashSet<&str> = match layout {
        Layout::Isolated(iso) => iso.public_hoist.iter().map(|(n, _)| n.as_str()).collect(),
        Layout::Hoisted(_) => HashSet::new(),
    };
    let mut lost = Vec::new();
    for (idx, folder) in plan.folders.iter().enumerate() {
        if !folder.touched {
            continue;
        }
        let abs = root.join(&folder.disk);
        match folder.kind {
            FolderKind::Scope { .. } => {}
            FolderKind::Store => {
                if !isolated {
                    continue;
                }
                unlink_links(&abs.join(ROOT_DIR), &|p, _| is_dangling(p));
                for (entry, is_link) in read_entries(&abs, false) {
                    if is_link || entry == ROOT_DIR {
                        continue;
                    }
                    let nm = abs.join(&entry).join(ROOT_DIR);
                    if !real_dir(&nm) {
                        continue;
                    }
                    unlink_links(&nm, &|p, _| is_dangling(p));
                    // A dependency with the package's own name is linked inside the package.
                    let (package, _) = split_store_key(&entry);
                    let own = nm.join(&package);
                    if real_dir(&own.join(ROOT_DIR))
                        && unlink_links(&own.join(ROOT_DIR), &|p, alias| {
                            alias == package && is_dangling(p)
                        })
                    {
                        let _ = std::fs::remove_dir(own.join(ROOT_DIR));
                    }
                }
            }
            FolderKind::NodeModules => {
                if isolated && let Some(direct) = &folder.direct {
                    unlink_links(&abs, &|p, alias| {
                        if direct.binary_search_by(|d| d.as_str().cmp(alias)).is_ok() {
                            return false;
                        }
                        if public.contains(alias) {
                            return is_dangling(p);
                        }
                        true
                    });
                }
                if prune_bins(&abs) {
                    lost.push(idx);
                }
            }
        }
    }
    lost
}

/// A removed package may have owned a name in `.bin`: relinks it to the package an install
/// would link it to.
fn hand_over_names(
    plan: &Plan,
    lost: &[usize],
    layout: &Layout,
    hoisted: Option<&HoistedTree<'_>>,
    graph: &LockGraph,
    root: &Path,
) {
    use fnpm_linker::bins::{BinLinker, BinOptions, LinkOrder};
    if lost.is_empty() {
        if let Layout::Isolated(iso) = layout {
            relink_hidden_names(plan, iso, graph, root);
        }
        return;
    }
    let lost_rel: HashSet<&str> = lost
        .iter()
        .map(|&i| plan.folders[i].disk.as_str())
        .collect();
    let direct = direct_packages(graph);
    let link = |rel: &str, placed: Vec<(String, PackageId)>| {
        let mut ordered: Vec<(LinkOrder<'_>, &String, PackageId)> = placed
            .iter()
            .filter_map(|(alias, id)| {
                let p = graph.get(*id)?;
                Some((
                    LinkOrder::new(direct[id.index()], p.resolution.tag(), alias),
                    alias,
                    *id,
                ))
            })
            .collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        let nm = root.join(rel);
        let mut linker = BinLinker::new(nm.join(".bin"));
        linker.seed_existing();
        for (_, alias, id) in ordered {
            let p = &graph.packages[id.index()];
            let _ = linker.link(alias, &p.bin, &nm.join(alias), BinOptions::default());
        }
    };
    match (layout, hoisted) {
        (Layout::Hoisted(_), Some(h)) => {
            for f in &h.folders {
                if lost_rel.contains(f.disk.as_str()) {
                    link(&f.disk, f.expected.clone());
                }
            }
        }
        (Layout::Isolated(iso), _) => {
            relink_hidden_names(plan, iso, graph, root);
            for (id, links) in &iso.roots {
                let rel = if *id == PackageId::ROOT {
                    ROOT_DIR.to_owned()
                } else {
                    match workspace_node_modules(graph, *id) {
                        Some(r) => r,
                        None => continue,
                    }
                };
                if !lost_rel.contains(rel.as_str()) {
                    continue;
                }
                let placed = links
                    .iter()
                    .filter_map(|(alias, e)| {
                        Some((alias.clone(), iso.entries.get(*e as usize)?.package))
                    })
                    .collect();
                link(&rel, placed);
            }
        }
        _ => {}
    }
}

/// A removed store entry may have held a name in `node_modules/.fnpm/node_modules`: links each
/// free name the layout hoists there to its entry, when that entry is on disk.
fn relink_hidden_names(
    plan: &Plan,
    iso: &fnpm_linker::IsolatedLayout,
    graph: &LockGraph,
    root: &Path,
) {
    if !plan
        .folders
        .iter()
        .any(|f| f.touched && f.kind == FolderKind::Store)
    {
        return;
    }
    let store = root.join(STORE_DIR);
    let hidden = store.join(ROOT_DIR);
    if !real_dir(&store) || !real_dir(&hidden) {
        return;
    }
    for (name, entry) in &iso.hidden_hoist {
        let Some(e) = iso.entries.get(*entry as usize) else {
            continue;
        };
        let p = &graph.packages[e.package.index()];
        if !extracted(&p.resolution) {
            continue;
        }
        let dest = hidden.join(name);
        if fsutil::kind(&dest) != Kind::Missing {
            continue;
        }
        let package = store.join(&e.key).join(ROOT_DIR).join(&p.name);
        if is_dangling(&package) {
            continue;
        }
        #[cfg(unix)]
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::os::unix::fs::symlink(fsutil::relative(parent, &package), &dest);
        }
    }
}

/// Packages the root or a workspace declares (bun `LinkOrder::direct_packages`).
fn direct_packages(graph: &LockGraph) -> Vec<bool> {
    let mut direct = vec![false; graph.packages.len()];
    for p in &graph.packages {
        if !matches!(
            p.resolution,
            Resolution::Root | Resolution::Workspace { .. }
        ) {
            continue;
        }
        for e in &p.dependencies {
            let b = e.behavior;
            if (b.is_prod() || b.is_dev() || b.is_optional() || b.intersects(Behavior::WORKSPACE))
                && let Some(t) = e.target
            {
                direct[t.index()] = true;
            }
        }
    }
    direct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_keys_and_bin_targets() {
        assert_eq!(
            split_store_key("a@1.0.0"),
            ("a".into(), Some("1.0.0".into()))
        );
        assert_eq!(
            split_store_key("@s+b@2.0.0+abc"),
            ("@s/b".into(), Some("2.0.0+abc".into()))
        );
        assert_eq!(split_store_key(".fnpm-tmp"), (".fnpm-tmp".into(), None));
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("a")).unwrap();
        assert!(leads_into_package(t.path(), "a/cli.js"));
        assert!(!leads_into_package(t.path(), "b/cli.js"));
    }
}
