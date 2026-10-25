//! Building the isolated layout on disk (bun `isolated_install.rs` `install_isolated_packages`,
//! `isolated_install/{Installer,Symlinker}.rs`).
//!
//! Phase A materialises every entry that is missing (in parallel, each waiting on its package):
//! project-local entries into `.fnpm/<key>/node_modules/<name>`, global-store entries into a
//! per-process staging directory `<store>/links/<key>-<hash>.tmp-<rand>`. Phase B writes the
//! dependency links and `.bin` links of every entry and importer (bin targets are checked at
//! their real location, so links into a dependency that is still staged work). Phase C
//! publishes staged global entries with one `rename` each (`EEXIST`: another project won; its
//! bytes are as good) and points `.fnpm/<key>` at them. A warm global hit costs one `readlink`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use fnpm_types::{PackageId, Resolution};
use parking_lot::Mutex;
use rayon::prelude::*;

use crate::bins::{BinLinker, BinOptions, LinkOrder};
use crate::fsutil::{self, Kind, Strategy};
use crate::hoisted::{local_target, optional_only, remove_stale};
use crate::isolated::IsolatedLayout;
use crate::verify;
use crate::{Error, LinkCx, LinkReport, Result};

/// What an entry is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kindof {
    /// The root or a workspace: links go into its own `node_modules`.
    Importer(PathBuf),
    /// Another first entry of an importer, or a link-only package: nothing to do.
    Nothing,
    /// A `link:` package: dependents link to this directory.
    External(PathBuf),
    /// `.fnpm/<key>` in the project.
    Local,
    /// `<store>/links/<key>-<hash>`.
    Global { final_dir: PathBuf },
}

struct EntryState {
    kind: Kindof,
    /// Materialised in this run (phase A).
    built: AtomicBool,
    /// Where the entry's files are during this run (staging for a global entry being built).
    real_entry_dir: Mutex<Option<PathBuf>>,
    /// Skipped (optional package whose download failed, or platform mismatch).
    skipped: AtomicBool,
}

struct Paths<'a> {
    fnpm: PathBuf,
    layout: &'a IsolatedLayout,
    cx: &'a LinkCx<'a>,
}

impl Paths<'_> {
    fn name(&self, e: u32) -> &str {
        &self.cx.graph.packages[self.layout.entries[e as usize].package.index()].name
    }

    fn local_entry_dir(&self, e: u32) -> PathBuf {
        self.fnpm.join(&self.layout.entries[e as usize].key)
    }

    /// The package directory a dependent links to. `from_global`: the dependent lives in the
    /// global store, so it links to the dependency's global location.
    fn store_path(&self, states: &[EntryState], e: u32, from_global: bool) -> Option<PathBuf> {
        match &states[e as usize].kind {
            Kindof::Importer(dir) => Some(dir.clone()),
            Kindof::External(dir) => Some(dir.clone()),
            Kindof::Nothing => {
                let pkg = &self.cx.graph.packages[self.layout.entries[e as usize].package.index()];
                self.cx.local_target(&pkg.resolution)
            }
            Kindof::Global { final_dir } if from_global => Some(fsutil::join_name(
                &final_dir.join("node_modules"),
                self.name(e),
            )),
            _ => Some(fsutil::join_name(
                &self.local_entry_dir(e).join("node_modules"),
                self.name(e),
            )),
        }
    }

    /// Where the dependency's files really are right now (for bin checks).
    fn real_path(&self, states: &[EntryState], e: u32) -> Option<PathBuf> {
        if let Some(dir) = states[e as usize].real_entry_dir.lock().as_ref() {
            return Some(fsutil::join_name(&dir.join("node_modules"), self.name(e)));
        }
        match &states[e as usize].kind {
            Kindof::Global { final_dir } => Some(fsutil::join_name(
                &final_dir.join("node_modules"),
                self.name(e),
            )),
            _ => self.store_path(states, e, false),
        }
    }
}

pub(crate) fn link(cx: &LinkCx<'_>, layout: &IsolatedLayout) -> Result<LinkReport> {
    let nm = cx.project_dir.join("node_modules");
    let fnpm = nm.join(".fnpm");
    let mut is_new_nm = cx.opts.fresh;
    match std::fs::create_dir(&nm) {
        Ok(()) => is_new_nm = true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fsutil::create_dir_all(&nm).map_err(Error::NodeModules)?;
            is_new_nm = true;
        }
        Err(e) => return Err(Error::NodeModules(e)),
    }
    let workspace_dirs: Vec<PathBuf> = cx
        .graph
        .packages
        .iter()
        .filter(|p| matches!(p.resolution, Resolution::Workspace { .. }))
        .filter_map(|p| local_target(cx.project_dir, &p.resolution))
        .filter(|d| crate::is_inside_project(cx.project_dir, d))
        .collect();
    let mut is_new_fnpm = is_new_nm;
    if !is_new_nm && !crate::is_isolated_tree(cx.project_dir) {
        // Another linker made this tree (or none did): move its entries aside.
        crate::move_node_modules_aside(&nm, &[".fnpm"]);
        for ws in &workspace_dirs {
            crate::move_node_modules_aside(&ws.join("node_modules"), &[]);
        }
        is_new_fnpm = true;
    } else if is_new_nm {
        for ws in &workspace_dirs {
            crate::move_node_modules_aside(&ws.join("node_modules"), &[]);
        }
    }
    if !is_new_fnpm {
        // `.fnpm` and `.fnpm/node_modules` are created by the linker: a link there came with
        // the project and is removed, not written through.
        fsutil::remove_planted_links(&fnpm.join("node_modules"), &nm)
            .map_err(Error::NodeModules)?;
    }
    fsutil::create_dir_all(&fnpm).map_err(Error::NodeModules)?;
    let fresh = is_new_fnpm;
    let links_dir = cx.opts.global_store.then(|| cx.store.links_dir());

    let paths = Paths {
        fnpm: fnpm.clone(),
        layout,
        cx,
    };
    let first_importer: HashSet<u32> = {
        let mut seen = HashSet::new();
        let mut firsts = HashSet::new();
        for (i, e) in layout.entries.iter().enumerate() {
            let r = &cx.graph.packages[e.package.index()].resolution;
            if matches!(r, Resolution::Root | Resolution::Workspace { .. })
                && seen.insert(e.package)
            {
                firsts.insert(i as u32);
            }
        }
        firsts
    };
    let states: Vec<EntryState> = layout
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let pkg = &cx.graph.packages[e.package.index()];
            let kind = match &pkg.resolution {
                Resolution::Root | Resolution::Workspace { .. } => {
                    if first_importer.contains(&(i as u32)) {
                        Kindof::Importer(
                            local_target(cx.project_dir, &pkg.resolution).unwrap_or_default(),
                        )
                    } else {
                        Kindof::Nothing
                    }
                }
                Resolution::Symlink { .. } => {
                    Kindof::External(cx.local_target(&pkg.resolution).unwrap_or_default())
                }
                _ => match (e.global_hash, &links_dir) {
                    (Some(h), Some(links)) => Kindof::Global {
                        final_dir: links.join(format!("{}-{h:016x}", e.key)),
                    },
                    _ => Kindof::Local,
                },
            };
            EntryState {
                kind,
                built: AtomicBool::new(false),
                real_entry_dir: Mutex::new(None),
                skipped: AtomicBool::new(false),
            }
        })
        .collect();
    cx.total
        .fetch_add(layout.entries.len() as u32, Ordering::Relaxed);

    let optional = optional_only(cx.graph);
    let installed = AtomicU32::new(0);
    let skipped = AtomicU32::new(0);
    let dirs: Mutex<Vec<(PackageId, PathBuf)>> = Mutex::new(Vec::new());
    let force = cx.opts.force;
    let staging_suffix = fsutil::random_suffix();
    if let Some(links) = &links_dir
        && states
            .iter()
            .any(|s| matches!(s.kind, Kindof::Global { .. }))
    {
        fsutil::create_dir_all(links).map_err(|source| Error::Io {
            path: links.clone(),
            source,
        })?;
    }

    // Phase A: materialise.
    let first_error: Mutex<Option<Error>> = Mutex::new(None);
    let fail = |e: Error| {
        let mut g = first_error.lock();
        if g.is_none() {
            *g = Some(e);
        }
    };
    let materialize_one = |i: usize| {
        let e = &layout.entries[i];
        let st = &states[i];
        let pkg = &cx.graph.packages[e.package.index()];
        let name = pkg.name.as_str();
        let r = (|| -> Result<()> {
            if !pkg.resolution.is_local_tree() && !pkg.platform.matches(&cx.platform) {
                st.skipped.store(true, Ordering::Relaxed);
                skipped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            match &st.kind {
                Kindof::Importer(_) | Kindof::Nothing | Kindof::External(_) => Ok(()),
                Kindof::Global { final_dir } => {
                    if !force && final_dir.is_dir() {
                        return Ok(());
                    }
                    let staging =
                        PathBuf::from(format!("{}.tmp-{staging_suffix:016x}", final_dir.display()));
                    let _ = fsutil::remove_any(&staging);
                    let dest = fsutil::join_name(&staging.join("node_modules"), name);
                    install(cx, e.package, &dest, &optional, st, &skipped)?;
                    if !st.skipped.load(Ordering::Relaxed) {
                        *st.real_entry_dir.lock() = Some(staging);
                        st.built.store(true, Ordering::Relaxed);
                        installed.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }
                Kindof::Local => {
                    let entry_dir = paths.local_entry_dir(i as u32);
                    let pkg_dir = fsutil::join_name(&entry_dir.join("node_modules"), name);
                    if !fresh {
                        if fsutil::kind(&entry_dir) == Kind::Symlink {
                            // An entry that left the global store is still a link into it.
                            fsutil::remove_link(&entry_dir).map_err(|source| Error::Io {
                                path: entry_dir.clone(),
                                source,
                            })?;
                        } else if !force && is_installed(cx, e.package, &pkg_dir) {
                            return Ok(());
                        } else {
                            fsutil::move_aside_and_delete(&pkg_dir).map_err(|source| {
                                Error::Io {
                                    path: pkg_dir.clone(),
                                    source,
                                }
                            })?;
                        }
                    }
                    install(cx, e.package, &pkg_dir, &optional, st, &skipped)?;
                    if !st.skipped.load(Ordering::Relaxed) {
                        st.built.store(true, Ordering::Relaxed);
                        installed.fetch_add(1, Ordering::Relaxed);
                        dirs.lock().push((e.package, pkg_dir));
                    }
                    Ok(())
                }
            }
        })();
        if let Err(err) = r {
            fail(err);
        }
        cx.tick();
    };
    // FIFO, biggest packages first (see `crate::largest_first`).
    let all: Vec<usize> = (0..layout.entries.len()).collect();
    let order = crate::largest_first(cx.source, &all, |i| layout.entries[i].package);
    rayon::scope_fifo(|s| {
        for i in order {
            let f = &materialize_one;
            s.spawn_fifo(move |_| f(i));
        }
    });
    if let Some(e) = first_error.lock().take() {
        cleanup_staging(&states);
        return Err(e);
    }

    // Phase B: dependency links and bins.
    (0..layout.entries.len()).into_par_iter().for_each(|i| {
        if let Err(err) = link_entry(cx, &paths, &states, i as u32, fresh) {
            fail(err);
        }
    });
    if let Some(e) = first_error.lock().take() {
        cleanup_staging(&states);
        return Err(e);
    }

    // Hidden hoist: `.fnpm/node_modules/<alias>`.
    let hidden = fnpm.join("node_modules");
    let hidden_res: Vec<Result<()>> = layout
        .hidden_hoist
        .par_iter()
        .map(|(alias, e)| {
            if states[*e as usize].skipped.load(Ordering::Relaxed) {
                return Ok(());
            }
            let dest = fsutil::join_name(&hidden, alias);
            let target = fsutil::join_name(
                &paths.local_entry_dir(*e).join("node_modules"),
                paths.name(*e),
            );
            let rel = fsutil::relative(dest.parent().unwrap_or(&hidden), &target);
            let strategy = if fresh {
                Strategy::ExpectMissing
            } else {
                Strategy::ExpectExisting
            };
            fsutil::ensure_symlink(&rel, &dest, strategy, Some(&fnpm))
                .map(|_| ())
                .map_err(|source| Error::Io { path: dest, source })
        })
        .collect();
    for r in hidden_res {
        r?;
    }
    let mut removed = 0;
    if !fresh {
        removed += remove_stale(&hidden, layout.hidden_hoist.iter().map(|(a, _)| a.as_str()));
    }

    // Phase C: publish global entries and link the project to them.
    let publish: Vec<Result<()>> = (0..layout.entries.len())
        .into_par_iter()
        .map(|i| {
            let Kindof::Global { final_dir } = &states[i].kind else {
                return Ok(());
            };
            if states[i].skipped.load(Ordering::Relaxed) {
                return Ok(());
            }
            let local = paths.local_entry_dir(i as u32);
            if let Some(staging) = states[i].real_entry_dir.lock().take() {
                publish_global(&staging, final_dir, force)?;
                let pkg = layout.entries[i].package;
                dirs.lock().push((
                    pkg,
                    fsutil::join_name(&local.join("node_modules"), paths.name(i as u32)),
                ));
            }
            link_project_to_global(&local, final_dir, fresh)
        })
        .collect();
    for r in publish {
        r?;
    }

    // Stale top-level entries and dangling bins.
    if !is_new_nm {
        for (pkg, deps) in &layout.roots {
            let Some(dir) = cx
                .graph
                .get(*pkg)
                .and_then(|p| local_target(cx.project_dir, &p.resolution))
            else {
                continue;
            };
            let is_root = *pkg == PackageId::ROOT;
            let mut expected: Vec<&str> = deps.iter().map(|(a, _)| a.as_str()).collect();
            if is_root {
                expected.extend(layout.public_hoist.iter().map(|(a, _)| a.as_str()));
            }
            let dir_nm = dir.join("node_modules");
            removed += remove_stale(&dir_nm, expected.into_iter());
            crate::bins::prune_dangling_bins(&dir_nm.join(".bin"));
        }
    }

    let mut dirs = dirs.into_inner();
    dirs.sort();
    Ok(LinkReport {
        installed: installed.load(Ordering::Relaxed),
        removed,
        skipped: skipped.load(Ordering::Relaxed),
        dirs,
        backend_used: None,
    })
}

fn cleanup_staging(states: &[EntryState]) {
    for s in states {
        if let Some(staging) = s.real_entry_dir.lock().take() {
            fsutil::delete_in_background(staging);
        }
    }
}

fn install(
    cx: &LinkCx<'_>,
    id: PackageId,
    dest: &Path,
    optional: &[bool],
    st: &EntryState,
    skipped: &AtomicU32,
) -> Result<()> {
    match cx.materialize(id, dest) {
        Ok(()) => Ok(()),
        Err(Error::Missing { .. }) if optional[id.index()] => {
            st.skipped.store(true, Ordering::Relaxed);
            skipped.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// The marker check for an existing project-local entry (bun: the package directory appears
/// only once fully linked; tarballs and git checkouts also compare `.fnpm-tag`).
fn is_installed(cx: &LinkCx<'_>, id: PackageId, pkg_dir: &Path) -> bool {
    let pkg = &cx.graph.packages[id.index()];
    match &pkg.resolution {
        Resolution::Folder { .. } => match cx.source.wait(id) {
            Ok(entry) => verify::same_file(
                &pkg_dir.join("package.json"),
                &entry.root.join("package.json"),
            ),
            Err(_) => false,
        },
        Resolution::Npm { .. } => pkg_dir.join("package.json").exists() && cx.patch_ok(id, pkg_dir),
        r => {
            verify::verify(pkg_dir, &verify::expected_for(&pkg.name, &pkg.version, r))
                && cx.patch_ok(id, pkg_dir)
        }
    }
}

/// Links an entry's dependencies and their bins (plus its own bins).
fn link_entry(
    cx: &LinkCx<'_>,
    paths: &Paths<'_>,
    states: &[EntryState],
    i: u32,
    fresh: bool,
) -> Result<()> {
    let layout = paths.layout;
    let e = &layout.entries[i as usize];
    let st = &states[i as usize];
    if st.skipped.load(Ordering::Relaxed) {
        return Ok(());
    }
    let pkg = &cx.graph.packages[e.package.index()];
    let (entry_nm, own_name, from_global, strategy, real_base): (
        PathBuf,
        Option<&str>,
        bool,
        Strategy,
        PathBuf,
    ) = match &st.kind {
        Kindof::Importer(dir) => {
            let nm = dir.join("node_modules");
            (nm.clone(), None, false, Strategy::ExpectExisting, nm)
        }
        Kindof::Nothing | Kindof::External(_) => return Ok(()),
        Kindof::Local => {
            let nm = paths.local_entry_dir(i).join("node_modules");
            let strat = if st.built.load(Ordering::Relaxed) && fresh {
                Strategy::ExpectMissing
            } else {
                Strategy::ExpectExisting
            };
            (nm.clone(), Some(pkg.name.as_str()), false, strat, nm)
        }
        Kindof::Global { .. } => {
            let Some(staging) = st.real_entry_dir.lock().clone() else {
                // A warm hit: the published entry already has its links and bins.
                return Ok(());
            };
            let nm = staging.join("node_modules");
            (
                nm.clone(),
                Some(pkg.name.as_str()),
                true,
                Strategy::ExpectMissing,
                nm,
            )
        }
    };
    let mut deps: Vec<&(String, u32)> = e.deps.iter().collect();
    let public: Vec<&(String, u32)> = if i == 0 {
        layout.public_hoist.iter().collect()
    } else {
        Vec::new()
    };
    deps.extend(public);
    if matches!(st.kind, Kindof::Importer(_)) {
        fsutil::create_dir_all(&entry_nm).map_err(|source| Error::Io {
            path: entry_nm.clone(),
            source,
        })?;
    }
    for (alias, dep) in &deps {
        if !fnpm_types::name::is_safe_install_folder_name(alias) {
            return Err(Error::UnsafePath {
                package: pkg.key(),
                path: alias.clone(),
            });
        }
        if states[*dep as usize].skipped.load(Ordering::Relaxed) {
            continue;
        }
        let dest = link_path(&entry_nm, alias, own_name);
        let Some(target) = paths.store_path(states, *dep, from_global) else {
            continue;
        };
        let rel = fsutil::relative(dest.parent().unwrap_or(&entry_nm), &target);
        fsutil::ensure_symlink(&rel, &dest, strategy, Some(&real_base))
            .map_err(|source| Error::Io { path: dest, source })?;
    }

    // Bins of the dependencies, direct ones first (bun `link_dependency_bins`).
    let is_importer = matches!(st.kind, Kindof::Importer(_));
    let declares = |alias: &str| {
        is_importer
            && pkg.dependencies.iter().any(|d| {
                d.name == alias
                    && (d.behavior.is_prod()
                        || d.behavior.is_dev()
                        || d.behavior.is_optional()
                        || d.behavior.is_workspace())
            })
    };
    let mut with_bins: Vec<&(String, u32)> = deps
        .iter()
        .copied()
        .filter(|(_, d)| {
            !states[*d as usize].skipped.load(Ordering::Relaxed)
                && !cx.graph.packages[layout.entries[*d as usize].package.index()]
                    .bin
                    .is_none()
        })
        .collect();
    with_bins.sort_by(|a, b| {
        let dpa = &cx.graph.packages[layout.entries[a.1 as usize].package.index()];
        let dpb = &cx.graph.packages[layout.entries[b.1 as usize].package.index()];
        LinkOrder::new(declares(&a.0), dpa.resolution.tag(), a.0.as_str()).cmp(&LinkOrder::new(
            declares(&b.0),
            dpb.resolution.tag(),
            b.0.as_str(),
        ))
    });
    let bin_dir = entry_nm.join(".bin");
    if is_importer && !fresh {
        fsutil::remove_planted_links(&bin_dir, &entry_nm).map_err(|source| Error::Io {
            path: bin_dir.clone(),
            source,
        })?;
    }
    let mut linker = BinLinker::new(&bin_dir);
    let mut first_err: Option<Error> = None;
    for (alias, dep) in with_bins {
        let dep_entry = &layout.entries[*dep as usize];
        let dp = &cx.graph.packages[dep_entry.package.index()];
        let text_dir = link_path(&entry_nm, alias, own_name);
        let real = paths.real_path(states, *dep);
        let (redirect_text, redirect_real) = native_redirect(cx, paths, states, *dep, from_global);
        let refresh = states[*dep as usize].built.load(Ordering::Relaxed);
        let opts = BinOptions {
            redirect_dir: redirect_text.as_deref(),
            refresh,
            real_dir: real.as_deref(),
            real_redirect_dir: redirect_real.as_deref(),
        };
        if let Err(err) = linker.link(alias, &dp.bin, &text_dir, opts) {
            first_err.get_or_insert(err);
        }
    }
    // Own bins, so the package's scripts find its commands (bun `Step::Binaries`).
    if let Some(own) = own_name
        && !pkg.bin.is_none()
    {
        let text_dir = fsutil::join_name(&entry_nm, own);
        let refresh = st.built.load(Ordering::Relaxed);
        let (redirect_text, redirect_real) = native_redirect(cx, paths, states, i, from_global);
        let opts = BinOptions {
            redirect_dir: redirect_text.as_deref(),
            refresh,
            real_dir: None,
            real_redirect_dir: redirect_real.as_deref(),
        };
        if let Err(err) = linker.link(own, &pkg.bin, &text_dir, opts) {
            first_err.get_or_insert(err);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `<nm>/<alias>`, or `<nm>/<name>/node_modules/<name>` when a dependency has the entry's own
/// name (bun `append_dependency_link_name`).
fn link_path(entry_nm: &Path, alias: &str, own_name: Option<&str>) -> PathBuf {
    let p = fsutil::join_name(entry_nm, alias);
    if own_name == Some(alias) {
        fsutil::join_name(&p.join("node_modules"), alias)
    } else {
        p
    }
}

/// The platform package of a native binlink package, as `(path for link text, real path)`.
fn native_redirect(
    cx: &LinkCx<'_>,
    paths: &Paths<'_>,
    states: &[EntryState],
    dep: u32,
    from_global: bool,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let layout = paths.layout;
    let dep_pkg = layout.entries[dep as usize].package;
    let Some(target_pkg) = cx.native_target(dep_pkg) else {
        return (None, None);
    };
    // The platform package is a dependency of the native package's entry.
    let Some(&(_, target_entry)) = layout.entries[dep as usize]
        .deps
        .iter()
        .find(|(_, d)| layout.entries[*d as usize].package == target_pkg)
    else {
        return (None, None);
    };
    if states[target_entry as usize]
        .skipped
        .load(Ordering::Relaxed)
    {
        return (None, None);
    }
    (
        paths.store_path(states, target_entry, from_global),
        paths.real_path(states, target_entry),
    )
}

/// Renames the staged entry into place. A collision means another install published the same
/// content first; under `--force` the existing entry is swapped out instead.
fn publish_global(staging: &Path, final_dir: &Path, force: bool) -> Result<()> {
    match std::fs::rename(staging, final_dir) {
        Ok(()) => Ok(()),
        Err(e) if is_collision(&e) => {
            if !force {
                fsutil::delete_in_background(staging.to_path_buf());
                return Ok(());
            }
            let old = PathBuf::from(format!(
                "{}.old-{:016x}",
                final_dir.display(),
                fsutil::random_suffix()
            ));
            std::fs::rename(final_dir, &old).map_err(|source| Error::Io {
                path: final_dir.to_path_buf(),
                source,
            })?;
            match std::fs::rename(staging, final_dir) {
                Ok(()) => {
                    fsutil::delete_in_background(old);
                    Ok(())
                }
                Err(e) if is_collision(&e) => {
                    fsutil::delete_in_background(old);
                    fsutil::delete_in_background(staging.to_path_buf());
                    Ok(())
                }
                Err(source) => {
                    fsutil::delete_in_background(staging.to_path_buf());
                    Err(Error::Io {
                        path: final_dir.to_path_buf(),
                        source,
                    })
                }
            }
        }
        Err(source) => {
            fsutil::delete_in_background(staging.to_path_buf());
            Err(Error::Io {
                path: final_dir.to_path_buf(),
                source,
            })
        }
    }
}

fn is_collision(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AlreadyExists
        || e.raw_os_error()
            .is_some_and(|c| c == libc::ENOTEMPTY || c == libc::EEXIST)
}

/// `.fnpm/<key>` -> `<store>/links/<key>-<hash>` (absolute, so the project may move). A correct
/// link costs one `readlink`; a different one is replaced atomically.
fn link_project_to_global(local: &Path, final_dir: &Path, fresh: bool) -> Result<()> {
    let io = |source| Error::Io {
        path: local.to_path_buf(),
        source,
    };
    if !fresh {
        match std::fs::read_link(local) {
            Ok(cur) if cur == final_dir => return Ok(()),
            Ok(_) => return fsutil::replace_symlink_atomic(final_dir, local).map_err(io),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                // A real directory from an install without the global store: out of the way
                // with one rename, deleted in the background.
                fsutil::move_aside_and_delete(local).map_err(io)?;
            }
        }
    }
    fsutil::ensure_symlink(final_dir, local, Strategy::ExpectMissing, None)
        .map(|_| ())
        .map_err(io)
}
