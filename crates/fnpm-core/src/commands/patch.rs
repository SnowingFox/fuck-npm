//! `fnpm patch <pkg>` and `fnpm patch --commit <dir>` / `fnpm patch-commit <dir>` (bun
//! `install/PackageManager/patchPackage.rs`).
//!
//! `patch` replaces the package's folder in node_modules with a real copy of its store entry
//! (the installed files may be hardlinks into the store), so edits do not leak into the store.
//! `--commit` diffs the store entry against the folder with `git diff --no-index`, checks that
//! the patch applies to a fresh copy, writes it to the patches directory, records it in the
//! root package.json's `patchedDependencies`, and installs, which applies it.

use std::path::{Path, PathBuf};

use fnpm_types::{LockGraph, Package, PackageId, Resolution};

use super::Ctx;
use super::pack::json::{self, Json};
use super::pack::out::{FResult, Fatal, Out, escape_control};
use crate::fetch::Fetcher;
use crate::{CoreResult, Result};

pub(crate) async fn run(cx: &Ctx, commit: bool) -> Result<CoreResult> {
    let out = Out::new();
    let code = match patch(cx, &out, commit).await {
        Ok(code) => code,
        Err(f) => {
            out.eprintln(&f.0);
            1
        }
    };
    Ok(CoreResult {
        exit_code: code,
        data: None,
        lines: None,
    })
}

fn note(out: &Out, text: &str) -> String {
    format!("{} {text}", out.paint_err("34", "note:"))
}

fn with_note(out: &Out, error: &str, note_text: &str) -> Fatal {
    Fatal(format!("{}\n{}", out.error(error).0, note(out, note_text)))
}

/// The package an argument names, and its folder.
struct Target {
    id: PackageId,
    /// Relative to the project root (or absolute when given so), `/`-separated.
    folder: String,
    /// The working directory is a workspace member, not the root.
    not_in_workspace_root: bool,
    /// The folder with symlinks resolved: what is replaced and diffed.
    real: PathBuf,
}

fn split_name_and_version(arg: &str) -> (&str, Option<&str>) {
    let start = usize::from(arg.starts_with('@'));
    match arg[start..].find('@') {
        Some(i) => (&arg[..start + i], Some(&arg[start + i + 1..])),
        None => (arg, None),
    }
}

fn read_name_and_version(out: &Out, folder: &Path) -> FResult<(String, String)> {
    let path = folder.join("package.json");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        out.named(
            &super::pack::out::errno_name(&e),
            &format!("failed to read \"{}\"", path.display()),
        )
    })?;
    let json = json::parse(&text).map_err(|e| {
        Fatal(format!(
            "{} {}\n{} parsing package.json in \"{}\"",
            out.paint_err("31", "error:"),
            e.message,
            out.paint_err("31", "ParserError"),
            folder.display()
        ))
    })?;
    let field = |key: &str| {
        json.get(key)
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                out.error(&format!(
                    "invalid package.json, missing or invalid property \"{key}\": {}",
                    path.display()
                ))
            })
    };
    Ok((field("name")?, field("version")?))
}

/// The resolution label patch keys use: the version for npm packages.
fn label(p: &Package) -> String {
    match &p.resolution {
        Resolution::Workspace { .. } => p.version.clone(),
        other => other.patch_label(),
    }
}

fn patch_key(p: &Package) -> String {
    p.patch_key()
}

/// The packages installed in node_modules (not only shipped inside another's tarball).
fn installed(graph: &LockGraph) -> Vec<bool> {
    let mut reach = vec![false; graph.len()];
    let mut queue = vec![PackageId::ROOT];
    for id in graph.workspaces.values() {
        queue.push(*id);
    }
    while let Some(id) = queue.pop() {
        let i = id.index();
        if i >= reach.len() || reach[i] {
            continue;
        }
        reach[i] = true;
        if let Some(p) = graph.get(id) {
            for e in &p.dependencies {
                if e.behavior.is_bundled() {
                    continue;
                }
                if let Some(t) = e.target {
                    queue.push(t);
                }
            }
        }
    }
    reach
}

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// The package directories below a `node_modules` (scopes expanded), with their names.
fn node_modules_entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(iter) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut names: Vec<String> = iter
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        let path = dir.join(&name);
        if name.starts_with('@') {
            let mut scoped: Vec<String> = std::fs::read_dir(&path)
                .map(|i| {
                    i.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            scoped.sort();
            for s in scoped {
                out.push((format!("{name}/{s}"), path.join(&s)));
            }
        } else {
            out.push((name, path));
        }
    }
    out
}

/// The first folder, breadth first from the root's and the workspaces' node_modules, that
/// holds `name` at `version` under the folder name `alias`. The displayed path stays at
/// the link position; the editable folder is selected after target lookup.
fn find_folder(
    root: &Path,
    members: &[PathBuf],
    aliases: &[String],
    version: &str,
) -> Option<PathBuf> {
    let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
    queue.push_back(root.join("node_modules"));
    for m in members {
        queue.push_back(m.join("node_modules"));
    }
    let mut seen = std::collections::HashSet::new();
    while let Some(nm) = queue.pop_front() {
        if !seen.insert(nm.clone()) {
            continue;
        }
        for (dir_name, path) in node_modules_entries(&nm) {
            if aliases.contains(&dir_name)
                && let Ok(text) = std::fs::read_to_string(path.join("package.json"))
                && let Ok(j) = json::parse(&text)
                && (version.is_empty() || j.get("version").and_then(Json::as_str) == Some(version))
            {
                return Some(path);
            }
            if !is_link(&path) {
                queue.push_back(path.join("node_modules"));
            }
        }
    }
    None
}

/// `.` and `..` resolved without reading the file system.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn inside(root: &Path, path: &Path) -> bool {
    let root_real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(root) || path.starts_with(&root_real)
}

/// Replaces each link on the way from `root` to `folder` that leads out of the project (into
/// the global store) with a real directory of links to the entries of its target, so the
/// package's folder can be replaced without touching the shared store.
fn materialize(root: &Path, folder: &Path, global_links: &Path) -> std::io::Result<()> {
    let Ok(rel) = folder.strip_prefix(root) else {
        return Ok(());
    };
    let mut cur = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for (i, comp) in comps.iter().enumerate() {
        cur.push(comp);
        let last = i + 1 == comps.len();
        if last || !is_link(&cur) {
            continue;
        }
        let target = std::fs::canonicalize(&cur)?;
        if inside(root, &target) {
            continue;
        }
        let global_links =
            std::fs::canonicalize(global_links).unwrap_or_else(|_| global_links.to_path_buf());
        if !target.starts_with(&global_links) {
            return Err(std::io::Error::other(format!(
                "refusing to prepare a package through unmanaged symlink {}",
                cur.display()
            )));
        }
        let staging = temp_sibling(&cur);
        std::fs::create_dir_all(&staging)?;
        let staged = (|| -> std::io::Result<()> {
            for e in std::fs::read_dir(&target)? {
                let e = e?;
                let dest = staging.join(e.file_name());
                #[cfg(unix)]
                std::os::unix::fs::symlink(e.path(), dest)?;
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileTypeExt;
                    let kind = e.file_type()?;
                    if kind.is_dir() || kind.is_symlink_dir() {
                        std::os::windows::fs::symlink_dir(e.path(), dest)?;
                    } else {
                        std::os::windows::fs::symlink_file(e.path(), dest)?;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = staged {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        let old = temp_sibling(&cur);
        if let Err(e) = std::fs::rename(&cur, &old) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&staging, &cur) {
            let _ = std::fs::rename(&old, &cur);
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        fnpm_linker::fsutil::remove_link(&old)?;
    }
    Ok(())
}

fn relative_to(root: &Path, path: &Path) -> String {
    let root_real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for base in [root, root_real.as_path()] {
        if let Ok(rel) = path.strip_prefix(base) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    path.to_string_lossy().into_owned()
}

fn refuse_kind(out: &Out, p: &Package) -> FResult<()> {
    let what = match &p.resolution {
        Resolution::Npm { .. }
        | Resolution::Git { .. }
        | Resolution::Github { .. }
        | Resolution::RemoteTarball { .. }
        | Resolution::LocalTarball { .. } => return Ok(()),
        Resolution::Folder { .. } => "file: folder",
        Resolution::Symlink { .. } => "link: dependency",
        _ => "workspace package",
    };
    Err(with_note(
        out,
        &format!("refusing to patch {what} \"{}\"", p.name),
        "edit the folder directly",
    ))
}

fn crash_bundled(out: &Out, name: &str) -> Fatal {
    with_note(
        out,
        &format!("refusing to patch bundled dependency \"{name}\""),
        "patch the package that bundles it, and edit it in that package's node_modules",
    )
}

fn resolve_target(
    out: &Out,
    cx: &Ctx,
    root: &Path,
    ws: &fnpm_workspace::Workspace,
    graph: &LockGraph,
    arg: &str,
    command: &str,
) -> FResult<Target> {
    let cwd = &cx.req.cwd;
    let member = ws.member_for_dir(cwd).map(|m| m.dir.clone());
    let not_in_workspace_root = member.is_some();
    let installed = installed(graph);
    let is_path = arg.contains("node_modules/");
    let (id, folder) = if is_path {
        let given = Path::new(arg);
        let abs = if given.is_absolute() {
            given.to_path_buf()
        } else {
            let from_cwd = cwd.join(given);
            let from_member = member.as_ref().map(|m| m.join(given));
            match from_member {
                Some(m) if m.exists() || !from_cwd.exists() => m,
                _ if from_cwd.exists() => from_cwd,
                _ => root.join(given),
            }
        };
        let (name, version) = read_name_and_version(out, &abs)?;
        let candidates: Vec<PackageId> = graph.by_name(&name).collect();
        let npm = candidates.iter().copied().find(|&id| {
            graph.get(id).is_some_and(|p| {
                matches!(p.resolution, Resolution::Npm { .. })
                    && fnpm_registry::publish::without_build_tag(&label(p))
                        == fnpm_registry::publish::without_build_tag(&version)
            })
        });
        let id = match npm {
            Some(id) => id,
            None => {
                let others: Vec<PackageId> = candidates
                    .iter()
                    .copied()
                    .filter(|&id| {
                        graph
                            .get(id)
                            .is_some_and(|p| !matches!(p.resolution, Resolution::Npm { .. }))
                    })
                    .collect();
                let has_npm = candidates.len() > others.len();
                match (has_npm, others.as_slice()) {
                    (false, [only]) => *only,
                    (_, [_, ..]) => {
                        return Err(out.error(&format!(
                            "cannot patch {arg}: no npm package {}@{} in lockfile. Other packages named {} have a git, tarball or folder label: run {command} <dependency>@<label>",
                            escape_control(&name),
                            escape_control(&version),
                            escape_control(&name),
                        )));
                    }
                    _ => {
                        return Err(with_note(
                            out,
                            &format!(
                                "cannot patch {arg}: {}@{} not in lockfile",
                                escape_control(&name),
                                escape_control(&version)
                            ),
                            &format!(
                                "if the edits changed \"version\" in its package.json, run {command} {}@<version in the lockfile> instead",
                                escape_control(&name)
                            ),
                        ));
                    }
                }
            }
        };
        if !installed.get(id.index()).copied().unwrap_or(false) {
            return Err(crash_bundled(out, &name));
        }
        let folder = if given.is_absolute() {
            arg.to_owned()
        } else {
            relative_to(root, &lexical(&abs))
        };
        (id, folder)
    } else {
        let (name, version) = split_name_and_version(arg);
        let mut pairs: Vec<(String, PackageId)> = Vec::new();
        let mut aliased: Vec<(String, PackageId)> = Vec::new();
        let mut is_bundled = false;
        for id in graph.ids() {
            let Some(p) = graph.get(id) else { continue };
            for e in &p.dependencies {
                let Some(t) = e.target else { continue };
                let Some(tp) = graph.get(t) else { continue };
                let list = if e.name == name {
                    &mut pairs
                } else if tp.name == name {
                    &mut aliased
                } else {
                    continue;
                };
                if let Some(v) = version
                    && label(tp) != v
                {
                    continue;
                }
                if e.behavior.is_bundled() || !installed.get(t.index()).copied().unwrap_or(false) {
                    is_bundled = true;
                    continue;
                }
                list.push((e.name.clone(), t));
            }
        }
        if pairs.is_empty() {
            pairs = aliased;
        }
        if pairs.is_empty() {
            if is_bundled {
                return Err(crash_bundled(out, arg));
            }
            return Err(Fatal(format!(
                "\n{}",
                out.error(&format!("package {} not found", escape_control(arg)))
                    .0
            )));
        }
        if version.is_none() && pairs.iter().any(|p| p.1 != pairs[0].1) {
            let mut text = format!(
                "\n{}",
                out.error(&format!(
                    "Found multiple versions of {name}, please specify a precise version from the following list:"
                ))
                .0
            );
            let mut printed: Vec<PackageId> = Vec::new();
            for (_, id) in &pairs {
                if printed.contains(id) {
                    continue;
                }
                printed.push(*id);
                if let Some(p) = graph.get(*id) {
                    text.push_str(&format!("\n  {}@{}", p.name, escape_control(&label(p))));
                }
            }
            return Err(Fatal(text));
        }
        let id = pairs[0].1;
        let mut aliases: Vec<String> = pairs
            .iter()
            .filter(|p| p.1 == id)
            .map(|p| p.0.clone())
            .collect();
        aliases.dedup();
        for a in &aliases {
            if !super::pack::bundle::alias_is_safe_install_target(a) {
                return Err(out.error(&format!(
                    "refusing to patch dependency with unsafe name \"{}\"",
                    escape_control(a)
                )));
            }
        }
        let version = graph.get(id).map(|p| p.version.clone()).unwrap_or_default();
        let members: Vec<PathBuf> = ws.members.iter().map(|m| m.dir.clone()).collect();
        let mut search_members = members.clone();
        if let Some(m) = &member {
            search_members.retain(|d| d != m);
            search_members.insert(0, m.clone());
        }
        // From a member, its own node_modules first.
        let found = match &member {
            Some(m) => find_folder(m, &[], &aliases, &version)
                .or_else(|| find_folder(root, &search_members, &aliases, &version)),
            None => find_folder(root, &search_members, &aliases, &version),
        };
        let Some(found) = found else {
            return Err(out.error(&format!(
                "could not find the folder for {} in node_modules",
                escape_control(arg)
            )));
        };
        (id, relative_to(root, &found))
    };
    let p = graph.get(id).ok_or_else(|| out.error("unknown package"))?;
    refuse_kind(out, p)?;
    let abs = if Path::new(&folder).is_absolute() {
        PathBuf::from(&folder)
    } else {
        root.join(&folder)
    };
    let mut real = lexical(&abs);
    let root_real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let managed = [root, root_real.as_path()].into_iter().any(|base| {
        real.strip_prefix(base).is_ok_and(|rel| {
            rel.components()
                .any(|part| part.as_os_str() == "node_modules")
        })
    });
    if !managed {
        return Err(out.error(&format!(
            "refusing to patch {}: the folder is outside the project's node_modules",
            escape_control(&folder)
        )));
    }
    // The local isolated layout needs the editable copy at the store position so
    // dependencies next to it stay reachable. A link into the shared global store
    // or an external checkout is replaced at its project-owned link position.
    if let Ok(link) = std::fs::read_link(&real) {
        let destination = lexical(&real.parent().unwrap_or(root).join(link));
        if destination.starts_with(root.join("node_modules/.fnpm"))
            && std::fs::canonicalize(&destination).is_ok_and(|p| inside(root, &p))
        {
            real = destination;
        }
    }
    Ok(Target {
        id,
        folder,
        not_in_workspace_root,
        real,
    })
}

/// A sibling of `folder` that `fnpm prune` and the linker treat as staging.
fn temp_sibling(folder: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let tag = nanos ^ (u64::from(std::process::id()) << 32) ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    folder
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".fnpm-staging-{tag:016x}"))
}

/// Copies `from` to `to` (files copied, not linked; modes kept; symlinks recreated).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let t = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if t.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if t.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn copy_missing(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(to) {
        Ok(meta) if !meta.is_dir() => return Ok(()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(to)?,
        Err(e) => return Err(e),
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        let kind = entry.file_type()?;
        match std::fs::symlink_metadata(&dest) {
            Ok(meta) if kind.is_dir() && meta.is_dir() => copy_missing(&entry.path(), &dest)?,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if kind.is_dir() {
                    copy_tree(&entry.path(), &dest)?;
                } else if kind.is_symlink() {
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &dest)?;
                } else {
                    std::fs::copy(entry.path(), &dest)?;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Dependencies live beside a package in the global virtual store. Its editable copy
/// needs those links inside its own node_modules; bundled entries in the copy take priority.
fn link_store_siblings(global_links: &Path, folder: &Path, staging: &Path) -> std::io::Result<()> {
    let package = std::fs::canonicalize(folder)?;
    let Ok(global_links) = std::fs::canonicalize(global_links) else {
        return Ok(());
    };
    if !package.starts_with(global_links) {
        return Ok(());
    }
    let Some(mut siblings) = package.parent() else {
        return Ok(());
    };
    if siblings
        .file_name()
        .is_some_and(|s| s.to_string_lossy().starts_with('@'))
    {
        let Some(parent) = siblings.parent() else {
            return Ok(());
        };
        siblings = parent;
    }
    link_sibling_entries(siblings, &staging.join("node_modules"), &package)
}

fn link_sibling_entries(from: &Path, to: &Path, except: &Path) -> std::io::Result<()> {
    // Never descend through a bundled symlink or replace an existing bundled file.
    match std::fs::symlink_metadata(to) {
        Ok(meta) if !meta.is_dir() => return Ok(()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(to)?,
        Err(e) => return Err(e),
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = entry.path();
        if target == except {
            continue;
        }
        let link = to.join(entry.file_name());
        if entry.file_type()?.is_dir() && entry.file_name().to_string_lossy().starts_with('@') {
            link_sibling_entries(&target, &link, except)?;
            continue;
        }
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(&target, &link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(&target, &link);
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Stages the entire replacement, including nested dependencies, before moving the old folder.
fn replace_with_copy(entry: &Path, folder: &Path, global_links: &Path) -> std::io::Result<()> {
    let staging = temp_sibling(folder);
    let staged = copy_tree(entry, &staging).and_then(|()| {
        let nested = folder.join("node_modules");
        if is_link(folder) {
            link_store_siblings(global_links, folder, &staging)?;
        } else if nested.is_dir() {
            copy_missing(&nested, &staging.join("node_modules"))?;
        }
        Ok(())
    });
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let old = temp_sibling(folder);
    let had_old = match std::fs::rename(folder, &old) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    if let Err(e) = std::fs::rename(&staging, folder) {
        if had_old && let Err(restore) = std::fs::rename(&old, folder) {
            return Err(std::io::Error::other(format!(
                "{e}; original folder retained at {}: {restore}",
                old.display()
            )));
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if had_old {
        let _ = remove_path(&old);
    }
    Ok(())
}

/// The package's store entry (fetched if the store lost it), patched with its current patch
/// when `patched`.
async fn store_entry(
    out: &Out,
    cx: &Ctx,
    project: &crate::project::Project,
    graph: &LockGraph,
    p: &Package,
    patched: bool,
) -> FResult<PreparedEntry> {
    let fetcher = Fetcher::new(
        project.store().map_err(|e| out.error(&e.to_string()))?,
        project.client().map_err(|e| out.error(&e.to_string()))?,
        project.root_dir.clone(),
        project.config.cache_dir.clone(),
        project.config.verify_integrity,
        cx.events.clone(),
    );
    let fetched = fetcher
        .ensure_package(p)
        .wait()
        .await
        .map_err(|e| out.error(&format!("{} not in cache, re-download failed: {e}", p.name)))?;
    if !patched {
        return prepare_entry(out, &project.root_dir, fetched.entry.root, p);
    }
    let Some(patch) = graph.patched_dependencies.get(&patch_key(p)) else {
        return prepare_entry(out, &project.root_dir, fetched.entry.root, p);
    };
    let (hash, text) =
        crate::fetch::read_patch(&project.root_dir, patch).map_err(|e| out.error(&e))?;
    let store = fetcher.store();
    if let Ok(Some(entry)) = store.get_patched(&fetched.key, hash) {
        return prepare_entry(out, &project.root_dir, entry.root, p);
    }
    let applied = store
        .import_patched(&fetched.key, &fetched.entry, &text, hash)
        .map_err(|e| out.error(&format!("failed applying patch file: {e}")))?;
    prepare_entry(out, &project.root_dir, applied.entry.root, p)
}

struct PreparedEntry {
    root: PathBuf,
    temporary: bool,
}

impl Drop for PreparedEntry {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn prepare_entry(out: &Out, root: &Path, entry: PathBuf, p: &Package) -> FResult<PreparedEntry> {
    if p.bin.is_none() {
        return Ok(PreparedEntry {
            root: entry,
            temporary: false,
        });
    }
    // Bin linking adjusts modes and CRLF shebangs. Do that on a private baseline so
    // neither package contents nor their file index in the shared store can change.
    let prepared = PreparedEntry {
        root: temp_sibling(&root.join("node_modules/patch-entry")),
        temporary: true,
    };
    copy_tree(&entry, &prepared.root)
        .map_err(|e| out.error(&format!("failed copying patch baseline: {e}")))?;
    let bins = PreparedEntry {
        root: temp_sibling(&root.join("node_modules/patch-bins")),
        temporary: true,
    };
    fnpm_linker::bins::link_bins(&p.name, &p.bin, &prepared.root, &bins.root)
        .map_err(|e| out.error(&format!("failed preparing patch baseline bins: {e}")))?;
    Ok(prepared)
}

fn missing_argument(out: &Out, commit: bool, patch_commit: bool) -> Fatal {
    let (message, command) = if patch_commit {
        (
            format!(
                "Missing path to the package directory containing your changes.\n  {} fnpm patch-commit {}",
                out.paint_err("2", "Usage:"),
                out.paint_err("36", "node_modules/<package>")
            ),
            "fnpm patch-commit",
        )
    } else if commit {
        (
            format!(
                "Missing path to the package directory containing your changes.\n  {} fnpm patch --commit {}",
                out.paint_err("2", "Usage:"),
                out.paint_err("36", "node_modules/<package>")
            ),
            "fnpm patch",
        )
    } else {
        (
            format!(
                "Missing package name to patch.\n  {} fnpm patch {}{}",
                out.paint_err("2", "Usage:"),
                out.paint_err("36", "<package>"),
                out.paint_err("2", "[@<version>]")
            ),
            "fnpm patch",
        )
    };
    Fatal(format!(
        "{}\n{}",
        out.error(&message).0,
        note(out, &format!("Run '{command} --help' for more information"))
    ))
}

async fn patch(cx: &Ctx, out: &Out, commit_command: bool) -> FResult<i32> {
    let commit = commit_command || cx.flag("commit");
    let Some(arg) = cx.req.positionals.first().cloned() else {
        return Err(missing_argument(out, commit, commit_command));
    };
    let quiet = matches!(
        cx.pm.log_level,
        fnpm_config::LogLevel::Silent | fnpm_config::LogLevel::Quiet
    );
    if !quiet {
        super::pack::header(
            cx,
            out,
            if commit_command {
                "patch-commit"
            } else {
                "patch"
            },
        );
    }
    if !commit {
        // bun installs first, so the folder to copy over exists.
        install(cx, out, quiet).await?;
    }
    let project = cx.project().map_err(|e| out.error(&e.to_string()))?;
    let ws = project
        .workspace(&*cx.events)
        .map_err(|e| out.error(&e.to_string()))?;
    let lock =
        crate::install::load_lock(&project.config, &ws).map_err(|e| out.error(&e.to_string()))?;
    let Some(graph) = lock.graph else {
        return Err(out.error(
            "Cannot find lockfile. Install packages with `fnpm install` before patching them.",
        ));
    };
    let root = project.root_dir.clone();
    let command = if commit {
        "fnpm patch --commit"
    } else {
        "fnpm patch"
    };
    let target = resolve_target(out, cx, &root, &ws, &graph, &arg, command)?;
    let p = graph
        .get(target.id)
        .cloned()
        .ok_or_else(|| out.error("unknown package"))?;
    if commit {
        return commit_patch(cx, out, &project, &graph, &p, &target).await;
    }

    let folder = target.real.clone();
    if cx.pm.dry_run {
        out.println(&format!(
            "\n{} would replace {} with a copy of {} to edit",
            out.paint("2", "dry run:"),
            target.folder,
            out.paint("1", &p.name)
        ));
        return Ok(0);
    }
    let entry = store_entry(out, cx, &project, &graph, &p, true).await?;
    let global_links = project
        .store()
        .map_err(|e| out.error(&e.to_string()))?
        .links_dir();
    materialize(&root, &folder, &global_links)
        .and_then(|()| replace_with_copy(&entry.root, &folder, &global_links))
        .map_err(|e| {
            out.error(&format!(
                "error overwriting folder in node_modules: {}",
                super::pack::out::errno_name(&e)
            ))
        })?;
    let shown = if target.not_in_workspace_root && !Path::new(&target.folder).is_absolute() {
        format!("{}/{}", root.display(), target.folder)
    } else {
        target.folder.clone()
    };
    out.print(&format!(
        "\nTo patch {}, edit the following folder:\n\n  {}\n\nOnce you're done with your changes, run:\n\n  {}\n",
        out.paint("1", &p.name),
        out.paint("36", &shown),
        out.paint("36", &format!("fnpm patch --commit '{shown}'"))
    ));
    Ok(0)
}

/// The characters NTFS reserves (and `/`, whitespace) are escaped, so `patches/` can be checked
/// out anywhere.
fn escape_patch_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '/' => out.push_str("%2F"),
            '\\' => out.push_str("%5c"),
            ' ' => out.push_str("%20"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            '\t' => out.push_str("%09"),
            ':' => out.push_str("%3A"),
            '?' => out.push_str("%3F"),
            '*' => out.push_str("%2A"),
            '"' => out.push_str("%22"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '|' => out.push_str("%7C"),
            c => out.push(c),
        }
    }
    out
}

/// `user:pass@` of a URL in the key is dropped from the file name.
fn without_url_credentials(key: &str) -> String {
    let Some(scheme_end) = key.find("://") else {
        return key.to_owned();
    };
    let rest = &key[scheme_end + 3..];
    let host_end = rest.find(['/', '#']).unwrap_or(rest.len());
    match rest[..host_end].rfind('@') {
        Some(at) => format!("{}{}", &key[..scheme_end + 3], &rest[at + 1..]),
        None => key.to_owned(),
    }
}

/// Moves what is not part of the package (fnpm's tag files, the dependencies the linker put in
/// its node_modules) out of `folder` for the diff; moved back when dropped.
struct HiddenFromDiff {
    stash: PathBuf,
    moved: Vec<(PathBuf, PathBuf)>,
}

impl HiddenFromDiff {
    fn new(pristine: &Path, folder: &Path) -> std::io::Result<Self> {
        let stash = temp_sibling(folder);
        std::fs::create_dir_all(&stash)?;
        let mut hidden = Self {
            stash,
            moved: Vec::new(),
        };
        for e in std::fs::read_dir(folder)? {
            let e = e?;
            let name = e.file_name();
            if !e.file_type()?.is_dir() && name.to_string_lossy().starts_with(".fnpm-") {
                hidden.hide(&e.path(), Path::new(&name))?;
            }
        }
        let nested = folder.join("node_modules");
        match std::fs::symlink_metadata(&nested) {
            Ok(meta) if meta.is_dir() => {
                hidden.hide_dependencies(
                    &pristine.join("node_modules"),
                    &nested,
                    Path::new("node_modules"),
                    true,
                )?;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(hidden)
    }

    fn hide_dependencies(
        &mut self,
        pristine: &Path,
        folder: &Path,
        rel: &Path,
        merge_groups: bool,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(folder)? {
            let entry = entry?;
            let name = entry.file_name();
            let reference = pristine.join(&name);
            let subpath = rel.join(&name);
            match std::fs::symlink_metadata(&reference) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.hide(&entry.path(), &subpath)?;
                }
                Err(e) => return Err(e),
                Ok(meta)
                    if merge_groups
                        && (name.to_string_lossy().starts_with('@') || name == ".bin")
                        && meta.is_dir()
                        && entry.file_type()?.is_dir() =>
                {
                    self.hide_dependencies(&reference, &entry.path(), &subpath, false)?;
                }
                Ok(_) => {}
            }
        }
        Ok(())
    }

    fn hide(&mut self, path: &Path, rel: &Path) -> std::io::Result<()> {
        let dest = self.stash.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(path, &dest)?;
        self.moved.push((path.to_path_buf(), dest));
        Ok(())
    }
}

impl HiddenFromDiff {
    fn restore(&mut self) -> std::io::Result<()> {
        let mut error = None;
        for (orig, dest) in self.moved.iter().rev() {
            if std::fs::symlink_metadata(dest).is_err() {
                continue;
            }
            let result = (|| {
                if let Some(parent) = orig.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(dest, orig)
            })();
            if let Err(e) = result {
                error.get_or_insert(e);
            }
        }
        if let Some(e) = error {
            return Err(std::io::Error::other(format!(
                "{e}; hidden package files retained at {}",
                self.stash.display()
            )));
        }
        self.moved.clear();
        std::fs::remove_dir_all(&self.stash)
    }
}

impl Drop for HiddenFromDiff {
    fn drop(&mut self) {
        if !self.stash.exists() {
            return;
        }
        if let Err(e) = self.restore() {
            eprintln!("warn: failed restoring package files after diff: {e}");
        }
    }
}

/// `path` relative to `cwd` with a `./` in front: the project's own path stays out of the
/// arguments git gets (it may contain a `\` or a `"`).
fn from_cwd(cwd: &Path, path: &Path) -> String {
    let cwd_parts: Vec<_> = cwd.components().collect();
    let path_parts: Vec<_> = path.components().collect();
    let common = cwd_parts
        .iter()
        .zip(&path_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut rel = PathBuf::new();
    for _ in common..cwd_parts.len() {
        rel.push("..");
    }
    for c in &path_parts[common..] {
        rel.push(c);
    }
    format!("./{}", rel.to_string_lossy())
}

fn should_skip_line(line: &[u8]) -> bool {
    line.is_empty()
        || (matches!(line[0], b' ' | b'-' | b'+')
            && !(line.len() >= 4 && (&line[..4] == b"--- " || &line[..4] == b"+++ ")))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// bun `git_diff_postprocess`: the folder prefixes are stripped from the headers, and a
/// symlink that only the store copy has is left out.
fn git_diff_postprocess(stdout: &mut Vec<u8>, old_folder: &[u8], new_folder: &[u8]) {
    while let Some(mode) = find(stdout, b"\ndeleted file mode 120000\n") {
        let start = stdout[..mode]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let end =
            find(&stdout[mode + 1..], b"\ndiff --git ").map_or(stdout.len(), |i| mode + i + 2);
        stdout.drain(start..end);
    }
    let trim = |f: &[u8]| -> Vec<u8> {
        let mut f = f;
        while f.first() == Some(&b'/') {
            f = &f[1..];
        }
        while f.last() == Some(&b'/') {
            f = &f[..f.len() - 1];
        }
        f.to_vec()
    };
    let (old_t, new_t) = (trim(old_folder), trim(new_folder));
    let prefixed = |prefix: &[u8], folder: &[u8]| [prefix, folder, b"/"].concat();
    let prefixed_folders: [(Vec<u8>, bool); 4] = [
        (prefixed(b"a/", &old_t), true),
        (prefixed(b"b/", &old_t), true),
        (prefixed(b"a/", &new_t), false),
        (prefixed(b"b/", &new_t), false),
    ];
    let mut saw_a: Option<usize> = None;
    let mut saw_b: Option<usize> = None;
    let mut line_idx = 0usize;
    let mut cursor = 0usize;
    while cursor <= stdout.len() {
        let line_start = cursor;
        let (line_end, next, exhausted) = match stdout[cursor..].iter().position(|&b| b == b'\n') {
            Some(pos) => (cursor + pos, cursor + pos + 1, false),
            None => (stdout.len(), stdout.len(), true),
        };
        let line_len = line_end - line_start;
        if !should_skip_line(&stdout[line_start..line_end]) {
            if let Some((idx, len, is_old)) =
                prefixed_folders.iter().find_map(|(needle, is_old)| {
                    find(&stdout[line_start..line_end], needle).map(|i| (i, needle.len(), *is_old))
                })
            {
                stdout.drain(line_start + idx + 2..line_start + idx + len);
                cursor = line_start;
                if is_old {
                    saw_a = Some(line_idx);
                } else {
                    saw_b = Some(line_idx);
                }
                continue;
            }
            if saw_a != Some(line_idx)
                && let Some(idx) = find(&stdout[line_start..line_end], old_folder)
                && idx + old_folder.len() < line_len
                && stdout[line_start + idx + old_folder.len()] == b'/'
            {
                stdout.drain(line_start + idx..line_start + idx + old_folder.len() + 1);
                cursor = line_start;
                saw_a = Some(line_idx);
                continue;
            }
            if saw_b != Some(line_idx)
                && let Some(idx) = find(&stdout[line_start..line_end], new_folder)
                && idx + new_folder.len() < line_len
                && stdout[line_start + idx + new_folder.len()] == b'/'
            {
                stdout.drain(line_start + idx..line_start + idx + new_folder.len() + 1);
                cursor = line_start;
                saw_b = Some(line_idx);
                continue;
            }
        }
        line_idx += 1;
        saw_a = None;
        saw_b = None;
        if exhausted {
            break;
        }
        cursor = next;
    }
}

/// `git diff --no-index` of the store copy against the edited folder, without the user's or
/// the machine's git configuration.
fn git_diff(out: &Out, cwd: &Path, old_folder: &Path, new_folder: &Path) -> FResult<Vec<u8>> {
    let old = from_cwd(cwd, old_folder);
    let new = from_cwd(cwd, new_folder);
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "-c",
        "core.safecrlf=false",
        "-c",
        "core.quotepath=false",
        "diff",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--ignore-cr-at-eol",
        "--irreversible-delete",
        "--full-index",
        "--no-index",
    ])
    .arg(&old)
    .arg(&new)
    .current_dir(cwd)
    .env_clear()
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_ATTR_NOSYSTEM", "1")
    .env("HOME", "")
    .env("XDG_CONFIG_HOME", "")
    .env("USERPROFILE", "")
    .env("GIT_DIR", if cfg!(windows) { "NUL" } else { "/dev/null" });
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(out.error("git must be installed to use `fnpm patch --commit`"));
        }
        Err(e) => return Err(out.error(&format!("failed to make diff {e}"))),
    };
    let mut stdout = output.stdout;
    let code = output.status.code();
    let ok = code == Some(0) || (code == Some(1) && !stdout.is_empty());
    if !ok {
        let mut stderr = output.stderr;
        if stderr.is_empty() {
            stderr = format!("git exited with {}", output.status).into_bytes();
        }
        if stderr.len() > 256 {
            let more = stderr.len() - 256;
            stderr.truncate(256);
            stderr.extend_from_slice(format!("... ({more} more bytes)").as_bytes());
        }
        return Err(out.error(&format!(
            "failed to make diff {}",
            String::from_utf8_lossy(&stderr)
        )));
    }
    git_diff_postprocess(&mut stdout, old.as_bytes(), new.as_bytes());
    Ok(stdout)
}

/// Applies the patch to a scratch copy of the pristine package, the way an install would.
fn patch_applies(out: &Out, pristine: &Path, folder: &Path, patch: &[u8]) -> FResult<()> {
    let parsed = fnpm_store::patch::parse_patch_file(patch)
        .map_err(|e| out.error(&format!("failed to parse patch file: {:?}", e.err)))?;
    if !parsed.skipped.is_empty() {
        let mut text = String::new();
        for w in parsed.skipped_warnings() {
            text.push_str(&format!("{} {}\n", out.paint_err("33", "warn:"), w));
        }
        return Err(Fatal(format!(
            "{text}{}",
            out.error("failed applying patch file: the patch has sections an install skips")
                .0
        )));
    }
    let scratch = temp_sibling(folder);
    let result = copy_tree(pristine, &scratch)
        .map_err(|e| {
            out.error(&format!(
                "failed to copy package to test the patch: {}",
                super::pack::out::errno_name(&e)
            ))
        })
        .and_then(|()| {
            parsed
                .apply(&scratch)
                .map_err(|e| out.error(&format!("failed applying patch file: {e}")))
        });
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// Sets (or with `None` removes) `patchedDependencies[key]` in the root package.json.
fn edited_package_json(out: &Out, root: &Path, key: &str, path: Option<&str>) -> FResult<String> {
    let file = root.join("package.json");
    let (text, mut json) = super::pack::read_package_json(out, &file)?;
    let Json::Object(props) = &mut json else {
        return Err(out.error("package.json is not an object"));
    };
    let pos = props.iter().position(|(k, _)| k == "patchedDependencies");
    match path {
        Some(path) => {
            let entry = Json::String(path.to_owned());
            match pos {
                Some(i) => match &mut props[i].1 {
                    Json::Object(deps) => match deps.iter_mut().find(|(k, _)| k == key) {
                        Some(slot) => slot.1 = entry,
                        None => deps.push((key.to_owned(), entry)),
                    },
                    other => *other = Json::Object(vec![(key.to_owned(), entry)]),
                },
                None => props.push((
                    "patchedDependencies".to_owned(),
                    Json::Object(vec![(key.to_owned(), entry)]),
                )),
            }
        }
        None => {
            if let Some(i) = pos
                && let Json::Object(deps) = &mut props[i].1
            {
                deps.retain(|(k, _)| k != key);
                if deps.is_empty() {
                    props.remove(i);
                }
            }
        }
    }
    let printed = json::print(
        &json,
        json::guess_indentation(text.as_bytes()),
        text.ends_with('\n'),
    );
    Ok(printed)
}

/// bun's install summary count line (`N packages installed`, `Checked N installs across M
/// packages (no changes)`), with the rows of added packages.
fn print_summary(out: &Out, s: &fnpm_types::InstallSummary) {
    let time = out.paint(
        "2",
        &if s.elapsed_ms as f64 <= 1500.0 {
            format!("[{:.2}ms]", s.elapsed_ms as f64)
        } else {
            format!("[{:.2}s]", s.elapsed_ms as f64 / 1000.0)
        },
    );
    let noun = |n: u32| if n == 1 { "package" } else { "packages" };
    let mut text = String::from("\n");
    for pkg in &s.added {
        text.push_str(&format!(
            "{} {}{}\n",
            out.paint("32", "+"),
            pkg.name,
            out.paint("2", &format!("@{}", pkg.version))
        ));
    }
    if !s.added.is_empty() {
        text.push('\n');
    }
    if s.installed_count > 0 {
        text.push_str(&format!(
            "{} {} installed {time}\n",
            out.paint("32", &s.installed_count.to_string()),
            noun(s.installed_count)
        ));
    } else if s.removed_count > 0 {
        text.push_str(&format!(
            "{} {} removed {time}\n",
            s.removed_count,
            noun(s.removed_count)
        ));
    } else if let Some(c) = s.checked {
        text.push_str(&format!(
            "Checked {} {} across {} {} {} {time}\n",
            out.paint("32", &c.installs.to_string()),
            if c.installs == 1 {
                "install"
            } else {
                "installs"
            },
            c.packages,
            noun(c.packages),
            out.paint("2", "(no changes)")
        ));
    } else {
        text.push_str(&format!("{time} {}\n", out.paint("2", "done")));
    }
    out.print(&text);
}

async fn install(cx: &Ctx, out: &Out, quiet: bool) -> FResult<()> {
    match super::maintenance::reinstall(cx).await {
        Ok(summary) => {
            if !quiet {
                print_summary(out, &summary);
            }
            Ok(())
        }
        Err(e) => Err(out.error(&e.to_string())),
    }
}

pub(crate) struct PendingCommit {
    pub key: String,
    pub manifest: String,
    pub patch: Option<(PathBuf, Vec<u8>)>,
}

impl PendingCommit {
    pub fn patch_hashes(&self) -> std::collections::BTreeMap<String, String> {
        self.patch
            .as_ref()
            .map(|(_, contents)| {
                (
                    self.key.clone(),
                    fnpm_types::Integrity::compute(fnpm_types::HashAlgorithm::Sha256, contents)
                        .to_hex(),
                )
            })
            .into_iter()
            .collect()
    }

    pub fn write_patch(&self) -> Result<()> {
        if let Some((path, contents)) = &self.patch {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(crate::Error::io(dir))?;
            }
            crate::project::write_atomic(path, contents).map_err(|e| crate::Error::Usage {
                message: format!("failed renaming patch file to patches dir: {e}"),
            })?;
        }
        Ok(())
    }
}

enum OriginalFile {
    Missing,
    File(Vec<u8>, std::fs::Permissions),
    Symlink(PathBuf),
}

pub(crate) struct CommitRollback {
    originals: Vec<(PathBuf, OriginalFile)>,
    armed: bool,
}

impl CommitRollback {
    pub fn new(root: &Path, pending: &PendingCommit, yarn: bool) -> Result<Self> {
        let mut paths = vec![
            root.join("package.json"),
            root.join(fnpm_lockfile::FNPM_LOCK),
        ];
        if yarn {
            paths.push(root.join("yarn.lock"));
        }
        if let Some((path, _)) = &pending.patch {
            paths.push(path.clone());
        }
        let mut originals = Vec::new();
        for path in paths {
            if originals.iter().any(|(saved, _)| saved == &path) {
                continue;
            }
            let original = match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() => OriginalFile::File(
                    std::fs::read(&path).map_err(crate::Error::io(&path))?,
                    meta.permissions(),
                ),
                Ok(meta) if meta.is_symlink() => OriginalFile::Symlink(
                    std::fs::read_link(&path).map_err(crate::Error::io(&path))?,
                ),
                // Atomic file writes cannot replace a directory or special entry.
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => OriginalFile::Missing,
                Err(e) => return Err(crate::Error::io(&path)(e)),
            };
            originals.push((path, original));
        }
        Ok(Self {
            originals,
            armed: true,
        })
    }

    pub fn finish(&mut self) {
        self.armed = false;
    }
}

impl Drop for CommitRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for (path, original) in self.originals.iter().rev() {
            let restored: std::io::Result<()> = (|| match original {
                OriginalFile::Missing => match std::fs::remove_file(path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    result => result,
                },
                OriginalFile::File(bytes, permissions) => {
                    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
                        && std::fs::read(path).is_ok_and(|current| current == *bytes)
                    {
                        return Ok(());
                    }
                    fnpm_store::fsops::write_atomic(path, bytes)?;
                    std::fs::set_permissions(path, permissions.clone())
                }
                OriginalFile::Symlink(target) => {
                    if std::fs::read_link(path).is_ok_and(|current| current == *target) {
                        return Ok(());
                    }
                    match std::fs::remove_file(path) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        result => result?,
                    }
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(target, path)?;
                    #[cfg(windows)]
                    std::os::windows::fs::symlink_file(target, path)?;
                    Ok(())
                }
            })();
            if let Err(e) = restored {
                eprintln!(
                    "warn: failed restoring {} after patch commit: {e}",
                    path.display()
                );
            }
        }
    }
}

async fn install_commit(cx: &Ctx, out: &Out, pending: PendingCommit) -> FResult<()> {
    let opts = crate::options::InstallOptions {
        pm: cx.pm.clone(),
        ..crate::options::InstallOptions::default()
    };
    let summary = crate::install::run_with_patch(opts, cx.events.clone(), cx.env.clone(), pending)
        .await
        .map_err(|e| out.error(&e.to_string()))?;
    print_summary(out, &summary);
    Ok(())
}

async fn commit_patch(
    cx: &Ctx,
    out: &Out,
    project: &crate::project::Project,
    graph: &LockGraph,
    p: &Package,
    target: &Target,
) -> FResult<i32> {
    let root = &project.root_dir;
    let key = patch_key(p);
    let pristine = store_entry(out, cx, project, graph, p, false).await?;
    let new_folder = std::fs::canonicalize(&target.real).map_err(|e| {
        out.named(
            &super::pack::out::errno_name(&e),
            &format!("failed to resolve {}", target.folder),
        )
    })?;
    if !inside(root, &new_folder) {
        return Err(with_note(
            out,
            &format!(
                "refusing to patch {}: resolves into the shared global store",
                target.folder
            ),
            "run fnpm patch on the package first to create an editable copy",
        ));
    }
    let patch = {
        let mut hidden = HiddenFromDiff::new(&pristine.root, &new_folder).map_err(|e| {
            out.named(
                &super::pack::out::errno_name(&e),
                &format!("failed to prepare {} for the diff", new_folder.display()),
            )
        })?;
        let result = git_diff(out, root, &pristine.root, &new_folder);
        hidden
            .restore()
            .map_err(|e| out.error(&format!("failed restoring package files: {e}")))?;
        result?
    };

    if patch.is_empty() {
        if let Some(existing) = graph.patched_dependencies.get(&key) {
            if cx.pm.dry_run {
                out.println(&format!(
                    "{} would remove the patch for {} ({})",
                    out.paint("2", "dry run:"),
                    out.paint("1", &key),
                    escape_control(&existing.path)
                ));
                return Ok(0);
            }
            let manifest = edited_package_json(out, root, &key, None)?;
            install_commit(
                cx,
                out,
                PendingCommit {
                    key: key.clone(),
                    manifest,
                    patch: None,
                },
            )
            .await?;
            let shared = graph.patched_dependencies.iter().any(|(k, other)| {
                *k != key
                    && (other.path == existing.path || {
                        std::fs::canonicalize(root.join(&other.path))
                            .ok()
                            .zip(std::fs::canonicalize(root.join(&existing.path)).ok())
                            .is_some_and(|(a, b)| a == b)
                    })
            });
            let file = root.join(&existing.path);
            let inside = std::fs::canonicalize(&file)
                .ok()
                .zip(std::fs::canonicalize(root).ok())
                .is_some_and(|(f, r)| f.starts_with(&r))
                && std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file());
            let (outcome, why) = if shared {
                ("kept", ", used by another package")
            } else if inside && std::fs::remove_file(&file).is_ok() {
                ("deleted", "")
            } else {
                ("did not delete", "")
            };
            out.print(&format!(
                "\nRemoved the patch for {} ({outcome} {}{why})\n",
                out.paint("1", &key),
                escape_control(&existing.path)
            ));
            return Ok(0);
        }
        out.print(&format!(
            "\nNo changes detected, comparing {} to {}\n",
            out.paint("31", &pristine.root.display().to_string()),
            out.paint("32", &target.folder)
        ));
        return Ok(0);
    }

    let text = String::from_utf8_lossy(&patch);
    let binary: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("Binary files ") && l.ends_with(" differ"))
        .collect();
    if !binary.is_empty() {
        let mut msg = out
            .error(&format!(
                "patches cannot contain binary file changes. found in {}:",
                target.folder
            ))
            .0;
        for line in binary {
            msg.push_str(&format!("\n  {}", escape_control(line)));
        }
        msg.push('\n');
        msg.push_str(&note(
            out,
            "undo the changes to these files, then commit again",
        ));
        return Err(Fatal(msg));
    }

    let filename = escape_patch_filename(&format!("{}.patch", without_url_credentials(&key)));
    if let Err(f) = patch_applies(out, &pristine.root, &new_folder, &patch) {
        return Err(Fatal(format!(
            "{}\n{}",
            f.0,
            note(
                out,
                &format!(
                    "no changes made to {}, package.json or the lockfile",
                    target.folder
                )
            )
        )));
    }

    let typed = cx
        .string("patchesDir")
        .unwrap_or_else(|| "patches".to_owned());
    let patches_dir = typed.replace('\\', "/");
    let absolute_patch = lexical(&root.join(&patches_dir)).join(&filename);
    let written = if Path::new(&patches_dir).is_absolute() {
        absolute_patch
            .strip_prefix(root)
            .unwrap_or(&absolute_patch)
            .to_path_buf()
    } else {
        fnpm_linker::fsutil::relative(root, &absolute_patch)
    };
    let patch_path = written.to_string_lossy().replace('\\', "/");

    if cx.pm.dry_run {
        out.println(&format!(
            "{} would write {} and add {} to patchedDependencies",
            out.paint("2", "dry run:"),
            escape_control(&patch_path),
            out.paint("1", &key)
        ));
        return Ok(0);
    }

    let abs_patch = if Path::new(&patch_path).is_absolute() {
        PathBuf::from(&patch_path)
    } else {
        root.join(&patch_path)
    };
    let manifest = edited_package_json(out, root, &key, Some(&patch_path))?;
    install_commit(
        cx,
        out,
        PendingCommit {
            key,
            manifest,
            patch: Some((abs_patch, patch)),
        },
    )
    .await?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            split_name_and_version("is-even@1.0.0"),
            ("is-even", Some("1.0.0"))
        );
        assert_eq!(split_name_and_version("@s/p"), ("@s/p", None));
        assert_eq!(split_name_and_version("@s/p@2"), ("@s/p", Some("2")));
        assert_eq!(
            escape_patch_filename("@s/p@1.0.0.patch"),
            "@s%2Fp@1.0.0.patch"
        );
        assert_eq!(
            without_url_credentials("x@https://u:p@host/a.tgz"),
            "x@https://host/a.tgz"
        );
        assert_eq!(from_cwd(Path::new("/a/b"), Path::new("/a/c/d")), "./../c/d");
    }

    fn directory_link(from: &Path, to: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(from, to).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(from, to).unwrap();
    }

    #[test]
    fn ancestor_materialization_keeps_entries_and_store_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let links = tmp.path().join("links");
        let entry = links.join("entry");
        let scope = entry.join("node_modules/@scope");
        let package = scope.join("a");
        let outside = tmp.path().join("outside");
        let pristine = tmp.path().join("pristine");
        let ancestor = root.join("node_modules/.fnpm/entry");
        let folder = ancestor.join("node_modules/@scope/a");
        for dir in [
            root.join("node_modules/.fnpm"),
            package.clone(),
            scope.join("b"),
            outside.clone(),
            pristine.clone(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(package.join("index.js"), "store a").unwrap();
        std::fs::write(scope.join("b/index.js"), "store b").unwrap();
        std::fs::write(entry.join("metadata"), "store metadata").unwrap();
        std::fs::write(pristine.join("index.js"), "pristine a").unwrap();
        directory_link(&outside, &entry.join("linked-dir"));
        #[cfg(unix)]
        std::os::unix::fs::symlink(entry.join("metadata"), entry.join("linked-file")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(entry.join("metadata"), entry.join("linked-file"))
            .unwrap();
        directory_link(&entry, &ancestor);

        materialize(&root, &folder, &links).unwrap();

        assert!(!is_link(&ancestor));
        for name in ["metadata", "linked-dir", "linked-file"] {
            assert_eq!(
                std::fs::canonicalize(ancestor.join(name)).unwrap(),
                std::fs::canonicalize(entry.join(name)).unwrap()
            );
        }
        for name in ["a", "b"] {
            assert_eq!(
                std::fs::canonicalize(ancestor.join("node_modules/@scope").join(name)).unwrap(),
                std::fs::canonicalize(scope.join(name)).unwrap()
            );
        }
        replace_with_copy(&pristine, &folder, &links).unwrap();
        std::fs::write(folder.join("index.js"), "edited a").unwrap();
        assert_eq!(
            std::fs::read_to_string(package.join("index.js")).unwrap(),
            "store a"
        );
        assert_eq!(
            std::fs::read_to_string(ancestor.join("node_modules/@scope/b/index.js")).unwrap(),
            "store b"
        );
        assert_eq!(
            std::fs::read_to_string(ancestor.join("linked-file")).unwrap(),
            "store metadata"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&entry).unwrap().count(), 4);
    }

    #[test]
    fn ancestor_materialization_refuses_unmanaged_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let links = tmp.path().join("links");
        let outside = tmp.path().join("outside");
        let ancestor = root.join("node_modules/@scope");
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(outside.join("a")).unwrap();
        std::fs::create_dir(&links).unwrap();
        std::fs::write(outside.join("a/index.js"), "external a").unwrap();
        directory_link(&outside, &ancestor);

        let error = materialize(&root, &ancestor.join("a"), &links).unwrap_err();

        assert!(error.to_string().contains("unmanaged symlink"));
        assert!(is_link(&ancestor));
        assert_eq!(
            std::fs::read_to_string(outside.join("a/index.js")).unwrap(),
            "external a"
        );
        assert_eq!(
            std::fs::read_dir(root.join("node_modules"))
                .unwrap()
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn global_copy_keeps_bundled_entries_and_external_checkout_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let links = tmp.path().join("links");
        let siblings = links.join("entry/node_modules");
        let package = siblings.join("a");
        let pristine = tmp.path().join("pristine");
        let outside = tmp.path().join("outside");
        for dir in [
            package.clone(),
            siblings.join("b"),
            siblings.join("c"),
            siblings.join("@deps/b"),
            siblings.join("@outside/d"),
            pristine.join("node_modules/b"),
            pristine.join("node_modules/@deps/b"),
            outside.clone(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(pristine.join("node_modules/b/index.js"), "bundled b").unwrap();
        std::fs::write(
            pristine.join("node_modules/@deps/b/index.js"),
            "bundled scoped b",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, pristine.join("node_modules/@outside")).unwrap();
        let folder = tmp.path().join("editable");
        std::os::unix::fs::symlink(&package, &folder).unwrap();
        replace_with_copy(&pristine, &folder, &links).unwrap();
        assert_eq!(
            std::fs::read_to_string(folder.join("node_modules/b/index.js")).unwrap(),
            "bundled b"
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("node_modules/@deps/b/index.js")).unwrap(),
            "bundled scoped b"
        );
        assert_eq!(
            std::fs::canonicalize(folder.join("node_modules/c")).unwrap(),
            siblings.join("c")
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        let external_link = tmp.path().join("external-link");
        std::os::unix::fs::symlink(&outside, &external_link).unwrap();
        replace_with_copy(&pristine, &external_link, &links).unwrap();
        assert!(!external_link.join("node_modules/c").exists());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn nested_copy_does_not_write_through_a_bundled_node_modules_link() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let outside = tmp.path().join("outside");
        let destination = tmp.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(source.join("index.js"), "nested dependency").unwrap();
        std::os::unix::fs::symlink(&outside, &destination).unwrap();
        copy_missing(&source, &destination).unwrap();
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(is_link(&destination));
    }

    #[cfg(unix)]
    #[test]
    fn diff_hiding_does_not_move_entries_through_dependency_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let pristine = tmp.path().join("pristine");
        let folder = tmp.path().join("editable");
        let outside = tmp.path().join("outside");
        for dir in [&pristine, &folder, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(outside.join("keep"), "shared dependency").unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("node_modules")).unwrap();
        let mut hidden = HiddenFromDiff::new(&pristine, &folder).unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).unwrap(),
            "shared dependency"
        );
        hidden.restore().unwrap();
        assert!(is_link(&folder.join("node_modules")));

        std::fs::remove_file(folder.join("node_modules")).unwrap();
        std::fs::create_dir(folder.join("node_modules")).unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("node_modules/@scope")).unwrap();
        let mut hidden = HiddenFromDiff::new(&pristine, &folder).unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).unwrap(),
            "shared dependency"
        );
        hidden.restore().unwrap();
        assert!(is_link(&folder.join("node_modules/@scope")));
    }

    #[cfg(unix)]
    #[test]
    fn diff_hiding_merges_bundled_and_linker_bins() {
        let tmp = tempfile::tempdir().unwrap();
        let pristine = tmp.path().join("pristine");
        let folder = tmp.path().join("editable");
        for base in [&pristine, &folder] {
            std::fs::create_dir_all(base.join("node_modules/.bin")).unwrap();
            std::fs::write(base.join("node_modules/.bin/bundled"), "bundled bin").unwrap();
        }
        let added = folder.join("node_modules/.bin/linked");
        std::os::unix::fs::symlink("../dependency/cli.js", &added).unwrap();
        let mut hidden = HiddenFromDiff::new(&pristine, &folder).unwrap();
        assert!(std::fs::symlink_metadata(&added).is_err());
        assert!(folder.join("node_modules/.bin/bundled").is_file());
        hidden.restore().unwrap();
        assert_eq!(
            std::fs::read_link(&added).unwrap(),
            Path::new("../dependency/cli.js")
        );
    }

    #[cfg(unix)]
    #[test]
    fn patch_baseline_bin_normalization_keeps_store_contents_and_modes() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let entry = tmp.path().join("store/package");
        std::fs::create_dir_all(project.join("node_modules")).unwrap();
        std::fs::create_dir_all(&entry).unwrap();
        let original = b"#!/usr/bin/env node\r\nconsole.log('bin');\r\n";
        std::fs::write(entry.join("cli.js"), original).unwrap();
        std::fs::set_permissions(entry.join("cli.js"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let mut p = Package::new("a", "1.0.0", Resolution::Root);
        p.bin = fnpm_types::Bin::File {
            path: "cli.js".into(),
        };
        let prepared = prepare_entry(&Out::new(), &project, entry.clone(), &p).unwrap();
        assert_eq!(std::fs::read(entry.join("cli.js")).unwrap(), original);
        assert_eq!(
            std::fs::metadata(entry.join("cli.js"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        let normalized = prepared.root.join("cli.js");
        assert!(
            std::fs::read(&normalized)
                .unwrap()
                .starts_with(b"#!/usr/bin/env node\n")
        );
        assert_ne!(
            std::fs::metadata(&normalized).unwrap().permissions().mode() & 0o111,
            0
        );
        let scratch = prepared.root.clone();
        drop(prepared);
        assert!(!scratch.exists());
    }

    #[test]
    fn postprocess() {
        let mut d = b"diff --git a/./old/index.js b/./new/index.js\nindex 1..2 100644\n--- a/./old/index.js\n+++ b/./new/index.js\n@@ -1 +1 @@\n-a\n+b\n".to_vec();
        git_diff_postprocess(&mut d, b"./old", b"./new");
        assert_eq!(
            String::from_utf8(d).unwrap(),
            "diff --git a/index.js b/index.js\nindex 1..2 100644\n--- a/index.js\n+++ b/index.js\n@@ -1 +1 @@\n-a\n+b\n"
        );
    }
}
