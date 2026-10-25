//! The install pipeline (`docs/ARCHITECTURE.md#install-pipeline`): fast path, config and
//! workspace, lockfile (or migration), streaming resolve, fetch + extract overlapping the
//! resolve, link overlapping the downloads, bins, lifecycle scripts, then the lockfile,
//! package.json edits, the store's project record and the no-op state file.

mod edits;
mod lock;
mod plan;
mod summary;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use fnpm_config::{Config, Env};
use fnpm_linker::{Layout, LinkOptions, LinkReport, Linker};
use fnpm_lockfile::Format;
use fnpm_resolver::{Mode, Request, ResolveInput, ResolveOutput, Resolved, Resolver};
use fnpm_scripts::{Optimizer, TrustPolicy};
use fnpm_types::event::CheckedCount;
use fnpm_types::{
    DependencyGroup, Event, EventSink, InstallSummary, LockGraph, NodeLinker, PackageId, Resolution,
};
use fnpm_workspace::Workspace;
use tokio::sync::mpsc;

use crate::fast_path::{self, Check, State};
use crate::fetch::{Fetcher, needs_content};
use crate::options::{InstallCommand, InstallOptions};
use crate::project::Project;
use crate::spec::{added_literal, updated_literal};
use crate::{Error, Result};

use edits::Edits;
use plan::Planned;

/// xxh3 over the install-relevant configuration: what changes `node_modules` for the same
/// lockfile (linker, backend, omit, platform, registries, ignoreScripts, production, the global
/// store and hoisting). The no-op fast path compares it.
pub fn config_hash(config: &Config) -> u64 {
    let scopes: Vec<(&String, &String)> = config.scopes.iter().map(|(k, v)| (k, &v.url)).collect();
    let value = serde_json::json!([
        config.linker,
        config.backend,
        config.omit,
        config.platform,
        config.registry.url,
        scopes,
        config.ignore_scripts,
        config.production,
        config.global_store,
        config.hoist,
        config.hoist_pattern,
        config.public_hoist_pattern,
        config.store_dir,
        config.verify_integrity,
    ]);
    xxhash_rust::xxh3::xxh3_64(value.to_string().as_bytes())
}

/// `FNPM_TIMING=1`: phase timings on stderr (perf work; never part of normal output).
struct Phases {
    on: bool,
    last: Instant,
    start: Instant,
}

impl Phases {
    fn new(env: &Env) -> Self {
        let now = Instant::now();
        Self {
            on: env
                .get("FNPM_TIMING")
                .is_some_and(|v| !v.is_empty() && v != "0"),
            last: now,
            start: now,
        }
    }

    fn mark(&mut self, phase: &str) {
        if self.on {
            let now = Instant::now();
            eprintln!(
                "[fnpm timing] {phase:<12} {:>8.2} ms (total {:.2} ms)",
                (now - self.last).as_secs_f64() * 1000.0,
                (now - self.start).as_secs_f64() * 1000.0
            );
            self.last = now;
        }
    }
}

/// The relative paths of every file the fast path watches: the manifests, pnpm-workspace.yaml,
/// the package.json of every directory the workspace globs select (members and not), and the
/// `patchedDependencies` patch files. Also returns the globs and the directories they
/// selected.
fn watched_files(ws: &Workspace, graph: &LockGraph) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut v = vec!["package.json".to_owned()];
    v.extend(
        ws.members
            .iter()
            .map(|m| format!("{}/package.json", m.path)),
    );
    if ws.root_dir.join(fast_path::PNPM_WORKSPACE).exists() {
        v.push(fast_path::PNPM_WORKSPACE.to_owned());
    }
    let patterns = ws.member_patterns().ok().flatten().unwrap_or_default();
    let dirs = if patterns.is_empty() {
        Vec::new()
    } else {
        Workspace::candidate_dirs(&ws.root_dir, &patterns).unwrap_or_default()
    };
    for d in &dirs {
        let rel = format!("{d}/package.json");
        if !v.contains(&rel) && ws.root_dir.join(&rel).is_file() {
            v.push(rel);
        }
    }
    for patch in graph.patched_dependencies.values() {
        let rel = patch.path.replace('\\', "/");
        if !rel.is_empty() && !v.contains(&rel) {
            v.push(rel);
        }
    }
    (v, patterns, dirs)
}

/// `registry_for` with the lifetime of the config.
fn registry_lookup<'c>(config: &'c Config) -> impl for<'x> Fn(&'x str) -> &'c str + 'c {
    move |name| config.registry_for(name).url.as_str()
}

/// The previous graph for `ws` (fnpm.lock, bun.lock(b) or a migrated foreign lockfile).
pub(crate) fn load_lock(config: &Config, ws: &Workspace) -> Result<lock::LockState> {
    lock::load(config, ws)
}

/// [`lock::migrate_from`].
pub(crate) fn migrate_lock(
    config: &Config,
    ws: &Workspace,
    names: &[&str],
) -> Result<lock::LockState> {
    lock::migrate_from(config, ws, names)
}

/// Writes a migration's package.json edits; returns bun's report lines.
pub(crate) fn write_migration(
    root: &Path,
    ws: &Workspace,
    state: &lock::LockState,
) -> Result<Vec<String>> {
    lock::write_back(root, ws, &state.write_back)
}

pub(crate) use edits::Edit;
pub(crate) use plan::name_matches;

/// Applies package.json edits to text with the format-preserving editor.
pub(crate) fn edit_package_json_text(text: &str, edits: &[Edit]) -> Result<String> {
    edits::apply_text(text, edits)
}

/// The synchronous first part of an install: options checked, project found, configuration
/// loaded and the no-op fast path checked. Needs no runtime, so a launcher can answer a no-op
/// install before it starts any thread ([`crate::prepare_install`]).
pub struct PreparedInstall {
    opts: InstallOptions,
    project: Project,
    hash: u64,
    start: Instant,
    phases: Phases,
    noop: Option<InstallSummary>,
    /// The workspace (with its warnings) and the previous lockfile, read while the runtime and
    /// the link pool start on other threads. `None` for a no-op.
    loaded: Option<(Workspace, Vec<String>, lock::LockState)>,
}

impl PreparedInstall {
    /// The summary of a no-op install (nothing changed since the last one), if this is one.
    pub fn noop(&self) -> Option<&InstallSummary> {
        self.noop.as_ref()
    }

    /// Non-fatal configuration diagnostics (also emitted as warn events by [`Self::run`]).
    pub fn warnings(&self) -> &[String] {
        &self.project.config.warnings
    }

    /// The rest of the pipeline (returns the no-op summary right away for a no-op).
    pub async fn run(self, events: Arc<dyn EventSink>) -> Result<InstallSummary> {
        self.project.report_warnings(&*events);
        if let Some(s) = self.noop {
            return Ok(s);
        }
        execute(self, events).await
    }
}

/// Checks the options, loads the project and its configuration and runs the fast path. When
/// the install is not a no-op, the link pool (and with `start_runtime` the tokio runtime) start
/// on background threads while the workspace and the lockfile are read here.
pub(crate) fn prepare(
    opts: InstallOptions,
    env: Env,
    start_runtime: bool,
) -> Result<PreparedInstall> {
    let start = Instant::now();
    let mut phases = Phases::new(&env);
    if opts.analyze {
        // `install --analyze <files>` scans imports; it belongs to a later parity phase.
        return Err(Error::NotImplemented {
            command: "install --analyze".into(),
        });
    }
    let mut cli = opts.pm.to_cli_overrides();
    if opts.exact {
        cli.exact = Some(true);
    }
    cli.update = opts.command == InstallCommand::Update;
    let is_add = opts.command == InstallCommand::Add
        || (opts.command == InstallCommand::Install && !opts.packages.is_empty());
    let project = Project::load(&opts.pm.cwd, cli, env, is_add)?;
    phases.mark("config");

    // 1. Fast path.
    let config = &project.config;
    let hash = config_hash(config);
    let mut noop = None;
    if opts.command == InstallCommand::Install
        && opts.packages.is_empty()
        && opts.filter.is_empty()
        && !config.force
        && !opts.pm.lockfile_only
        && !opts.pm.trust
        && config.install_packages
        && let Check::Noop { packages } = fast_path::check(&project.root_dir, hash)
    {
        noop = Some(InstallSummary {
            checked: Some(CheckedCount {
                installs: packages,
                packages,
            }),
            elapsed_ms: start.elapsed().as_millis() as u64,
            noop: true,
            ..InstallSummary::default()
        });
    }
    phases.mark("fast path");
    let loaded = if noop.is_none() {
        if config.install_packages && !opts.pm.lockfile_only {
            fnpm_linker::prewarm_pool(LinkOptions::default().threads);
        }
        if start_runtime {
            let _ = std::thread::Builder::new()
                .name("fnpm-rt-warm".into())
                .spawn(|| {
                    let _ = crate::runtime();
                });
        }
        let (ws, warnings) = Workspace::load_with(&project.root_dir, &Default::default())?;
        phases.mark("workspace");
        let lock = lock::load(config, &ws)?;
        phases.mark("lockfile");
        Some((ws, warnings, lock))
    } else {
        None
    };
    Ok(PreparedInstall {
        opts,
        project,
        hash,
        start,
        phases,
        noop,
        loaded,
    })
}

/// Runs one install command.
pub(crate) async fn run(
    opts: InstallOptions,
    events: Arc<dyn EventSink>,
    env: Env,
) -> Result<InstallSummary> {
    prepare(opts, env, false)?.run(events).await
}

/// Steps 2-8.
async fn execute(prepared: PreparedInstall, events: Arc<dyn EventSink>) -> Result<InstallSummary> {
    let PreparedInstall {
        opts,
        project,
        hash,
        start,
        mut phases,
        loaded,
        ..
    } = prepared;
    let config = project.config.clone();
    let root = project.root_dir.clone();
    // 2. Workspace and 3. lockfile (read by `prepare`).
    let (mut ws, ws_warnings, lock) = match loaded {
        Some(l) => l,
        None => {
            let (ws, warnings) = Workspace::load_with(&root, &Default::default())?;
            let lock = lock::load(&config, &ws)?;
            (ws, warnings, lock)
        }
    };
    for w in ws_warnings {
        events.emit(Event::warn(w, None));
    }
    for w in &lock.warnings {
        events.emit(Event::warn(w.clone(), None));
    }
    lock::apply_in_memory(&mut ws, &lock.write_back)?;

    // What the command asks for.
    let mut planned = plan::plan(&opts, &config, &ws, &project.cwd, &*events)?;

    // 4. + 5. Resolve, streaming packages to the fetcher.
    let client = project.client()?;
    client.set_event_sink(events.clone());
    let store = project.store()?;
    let fetcher = Fetcher::new(
        store.clone(),
        client.clone(),
        root.clone(),
        config.cache_dir.clone(),
        config.verify_integrity,
        events.clone(),
    );
    let omits = config.omit.dev || config.omit.optional || config.omit.peer || config.production;
    let stream = config.install_packages && !opts.pm.lockfile_only && !omits;
    let previous = if config.force && planned.mode != Mode::Frozen {
        None
    } else {
        lock.graph.as_ref()
    };
    let resolver = Resolver::new(config.clone(), client.clone());
    let mut out = resolve(
        &resolver,
        &ws,
        previous,
        &planned,
        stream.then_some(&fetcher),
        &events,
    )
    .await?;
    // Positionals without a name (git, tarball, folder) were requested under a guessed name;
    // redo the resolve once with the packages' own names.
    let mut renamed = false;
    for add in &planned.adds {
        if add.positional.name.is_some() {
            continue;
        }
        let req = &planned.requests[add.request];
        let real = out
            .requests
            .iter()
            .find(|(r, _)| r == req)
            .and_then(|(_, id)| out.graph.get(*id))
            .map(|p| p.name.clone());
        if let Some(real) = real.filter(|r| !r.is_empty() && *r != req.name) {
            planned.requests[add.request].name = real;
            renamed = true;
        }
    }
    if renamed {
        out = resolve(
            &resolver,
            &ws,
            previous,
            &planned,
            stream.then_some(&fetcher),
            &events,
        )
        .await?;
    }
    for w in &out.warnings {
        events.emit(Event::warn(w.clone(), None));
    }
    phases.mark("resolve");
    let ResolveOutput {
        mut graph,
        changed,
        requests: resolved_requests,
        ..
    } = out;

    // package.json edits, mirrored into the graph so the saved lockfile matches them.
    let edits = package_json_edits(
        &opts,
        &config,
        &ws,
        &planned,
        &resolved_requests,
        &mut graph,
    );

    let (added, updated, removed) = summary::changes(lock.graph.as_ref(), &graph);
    let requested = summary::requested(&graph, &resolved_requests, &config.platform);
    let mut summary = InstallSummary {
        added,
        updated,
        removed,
        requested,
        ..InstallSummary::default()
    };
    let lockfile_dirty = changed || lock.migrated || !root.join(fnpm_lockfile::FNPM_LOCK).exists();

    if opts.pm.lockfile_only || !config.install_packages {
        // `--lockfile-only` writes the lockfile; `--dry-run` writes nothing.
        if opts.pm.lockfile_only && !config.dry_run {
            save(
                &root,
                &config,
                &ws,
                &lock,
                &edits,
                &graph,
                lockfile_dirty,
                &*events,
            )?;
        }
        summary.elapsed_ms = start.elapsed().as_millis() as u64;
        client.flush_cache_writes().await;
        return Ok(summary);
    }

    // 6. Link, overlapping the downloads still in flight.
    fast_path::remove(&root);
    if config.global {
        // `remove -g`: the global bin links of removed packages go before their directories.
        for (_, name) in &planned.removals {
            let dir = fnpm_linker::fsutil::join_name(&root.join("node_modules"), name);
            let _ = fnpm_linker::bins::unlink_bins(&dir, &config.global_bin_dir);
        }
    }
    let selected_ids: Option<Vec<PackageId>> = planned.selected.as_ref().and_then(|paths| {
        (!paths.is_empty() && opts.command == InstallCommand::Install).then(|| {
            paths
                .iter()
                .filter_map(|p| graph.workspaces.get(p).copied())
                .collect()
        })
    });
    let trust_names: Vec<String> = if opts.pm.trust {
        resolved_requests
            .iter()
            .map(|(r, _)| r.name.clone())
            .collect()
    } else {
        Vec::new()
    };
    let linked = link(
        &project,
        &ws,
        &lock,
        &fetcher,
        &mut graph,
        selected_ids.clone(),
        &trust_names,
        &events,
    )
    .await?;
    phases.mark("link");
    summary.installed_count = linked.report.installed;
    summary.removed_count = linked.report.removed;
    summary.failed_count = linked.failed;

    // Integrity the lockfile did not have (migrations, URL tarballs) comes from the imports.
    let mut integrity_filled = false;
    for i in 0..graph.packages.len() {
        let p = &graph.packages[i];
        if !needs_content(&p.resolution) || p.resolution.integrity().is_some() {
            continue;
        }
        if matches!(p.resolution, Resolution::Folder { .. }) {
            continue;
        }
        if !linked.reachable.get(i).copied().unwrap_or(false) {
            continue;
        }
        let Some(slot) = fetcher.existing(p) else {
            continue;
        };
        if let Ok(f) = slot.wait().await {
            graph.packages[i].resolution.set_integrity(f.key.integrity);
            integrity_filled = true;
        }
    }

    // Global installs link the direct dependencies' bins into the global bin directory.
    if config.global {
        link_global_bins(&config, &root, &graph)?;
    }

    // 8. Save (before scripts, so a failing script leaves a consistent lockfile).
    let mut edits = edits;
    if opts.pm.trust && !trust_names.is_empty() {
        edits.push(
            "",
            Edit::Trust {
                names: trust_names.clone(),
            },
        );
        let set = graph
            .trusted_dependencies
            .get_or_insert_with(Default::default);
        set.extend(trust_names.iter().cloned());
    }
    let saved = save(
        &root,
        &config,
        &ws,
        &lock,
        &edits,
        &graph,
        lockfile_dirty || integrity_filled,
        &*events,
    )?;
    phases.mark("save");
    // Caches and the store's project record, off the critical path (awaited before the
    // state file).
    let packages = fetcher.store_dir_names(&graph, &linked.reachable).await;
    let background = {
        let (store, root, config) = (store.clone(), root.clone(), config.clone());
        let restore = !saved && lock.from_cached_sidecar;
        let loaded =
            (!saved && lock.sidecar_missing && !restore && lock.format == Some(Format::Fnpm))
                .then(|| lock.graph.clone().zip(lock.hash))
                .flatten();
        tokio::task::spawn_blocking(move || {
            let _ = store.set_project_packages(&root, packages);
            if saved {
                lock::mirror_sidecar(&config, &root);
            } else if restore {
                lock::restore_sidecar(&config, &root);
            } else if let Some((graph, hash)) = loaded {
                lock::save_sidecars(&config, &root, hash, &graph, true);
            }
        })
    };
    phases.mark("register");

    // 7. Lifecycle scripts.
    let blocked = run_scripts(&project, &ws, &graph, &linked, &trust_names, &events).await?;
    phases.mark("scripts");
    summary.blocked_scripts = blocked.0;
    summary.failed_count += blocked.1;

    let _ = background.await;
    // The state file goes last, after everything succeeded.
    // Not when this run left the lockfile or package.json behind node_modules (`--no-save`,
    // a frozen install never changes either).
    let saved_everything = (config.lockfile.save || !(lockfile_dirty || integrity_filled))
        && (config.save_package_json || edits.is_empty());
    if planned.selected.is_none() && saved_everything {
        // Directories the linker moved aside are deleted in the background; their removal
        // would change node_modules' mtime after the state recorded it.
        if linked.report.removed > 0 || linked.report.installed > 0 {
            let _ = tokio::task::spawn_blocking(fnpm_linker::flush_background_deletes).await;
        }
        write_state(&root, &ws, &graph, hash, &linked.linker);
    }
    phases.mark("state");
    client.flush_cache_writes().await;
    phases.mark("flush");
    if phases.on {
        eprintln!("[fnpm timing] registry {:?}", client.stats());
    }
    summary.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(summary)
}

/// One resolve; with a fetcher, every streamed package starts its fetch immediately.
async fn resolve(
    resolver: &Resolver,
    ws: &Workspace,
    previous: Option<&LockGraph>,
    planned: &Planned,
    fetcher: Option<&Fetcher>,
    events: &Arc<dyn EventSink>,
) -> Result<ResolveOutput> {
    let (tx, consumer) = match fetcher {
        Some(f) => {
            let (tx, mut rx) = mpsc::unbounded_channel::<Resolved>();
            let f = f.clone();
            let task = tokio::spawn(async move {
                while let Some(Resolved::Package {
                    name,
                    version,
                    resolution,
                    ..
                }) = rx.recv().await
                {
                    if needs_content(&resolution) {
                        f.ensure(&name, &version, &resolution);
                    }
                }
            });
            (Some(tx), Some(task))
        }
        None => (None, None),
    };
    let input = ResolveInput {
        workspace: ws,
        previous,
        mode: planned.mode.clone(),
        requests: planned.requests.clone(),
        removals: planned.removals.clone(),
    };
    let out = resolver.resolve(input, tx, &**events).await;
    if let Some(task) = consumer {
        let _ = task.await;
    }
    Ok(out?)
}

/// Points the edge `name` of workspace `ws_path` at the literal written to package.json.
fn set_edge_literal(
    graph: &mut LockGraph,
    ws_path: &str,
    name: &str,
    group: Option<DependencyGroup>,
    literal: &str,
) {
    let Some(&id) = graph.workspaces.get(ws_path) else {
        return;
    };
    let Some(pkg) = graph.get_mut(id) else { return };
    for e in &mut pkg.dependencies {
        if e.name != name {
            continue;
        }
        let matches = match group {
            Some(g) => e.behavior.group() == g,
            None => !e.behavior.is_peer(),
        };
        if matches {
            e.literal = literal.to_owned();
        }
    }
}

fn target_of(graph: &LockGraph, ws_path: &str, name: &str) -> Option<PackageId> {
    let id = *graph.workspaces.get(ws_path)?;
    graph
        .get(id)?
        .dependencies
        .iter()
        .filter(|e| e.name == name)
        .find_map(|e| e.target)
}

/// The package.json writes of the command, mirrored into `graph`.
fn package_json_edits(
    opts: &InstallOptions,
    config: &Config,
    ws: &Workspace,
    planned: &Planned,
    resolved: &[(Request, PackageId)],
    graph: &mut LockGraph,
) -> Edits {
    let mut edits = Edits::default();
    if !config.save_package_json && !opts.pm.lockfile_only {
        return edits;
    }
    let mut explicit: Vec<(String, String)> = Vec::new();
    for add in &planned.adds {
        let req = &planned.requests[add.request];
        let Some(id) = resolved.iter().find(|(r, _)| r == req).map(|(_, id)| *id) else {
            continue;
        };
        let Some(pkg) = graph.get(id) else { continue };
        let p = &add.positional;
        let literal = match &pkg.resolution {
            Resolution::Npm { version, .. } => added_literal(p, version, config.exact),
            Resolution::Workspace { .. } => {
                if p.literal.starts_with("workspace:")
                    || crate::spec::split_npm_alias(&p.literal).is_some()
                {
                    p.literal.clone()
                } else {
                    "workspace:*".to_owned()
                }
            }
            _ if p.name.is_some() => p.literal.clone(),
            _ => p.input.clone(),
        };
        explicit.push((req.workspace.clone(), req.name.clone()));
        if let Some(cat) = &add.catalog {
            let entry = add.catalog_existing.clone().unwrap_or(literal);
            if add.catalog_existing.is_none() {
                edits.push(
                    "",
                    Edit::Catalog {
                        catalog: cat.clone(),
                        name: req.name.clone(),
                        spec: entry.clone(),
                    },
                );
                let map = if cat.is_empty() || cat == "default" {
                    &mut graph.catalogs.default
                } else {
                    graph.catalogs.named.entry(cat.clone()).or_default()
                };
                map.insert(req.name.clone(), entry);
            }
            let reference = if cat.is_empty() {
                "catalog:".to_owned()
            } else {
                format!("catalog:{cat}")
            };
            edits.push(
                &req.workspace,
                Edit::Set {
                    group: req.group,
                    name: req.name.clone(),
                    spec: reference.clone(),
                },
            );
            set_edge_literal(
                graph,
                &req.workspace,
                &req.name,
                Some(req.group),
                &reference,
            );
        } else if opts.command == InstallCommand::Update {
            edits.push(
                &req.workspace,
                Edit::Update {
                    name: req.name.clone(),
                    spec: literal.clone(),
                },
            );
            set_edge_literal(graph, &req.workspace, &req.name, None, &literal);
        } else {
            edits.push(
                &req.workspace,
                Edit::Set {
                    group: req.group,
                    name: req.name.clone(),
                    spec: literal.clone(),
                },
            );
            set_edge_literal(graph, &req.workspace, &req.name, Some(req.group), &literal);
        }
    }
    for (ws_path, name) in &planned.removals {
        edits.push(ws_path, Edit::Remove { name: name.clone() });
    }
    for u in &planned.updates {
        if explicit.contains(&(u.workspace.clone(), u.name.clone())) {
            continue;
        }
        let Some(t) = target_of(graph, &u.workspace, &u.name) else {
            continue;
        };
        let Some(version) = graph
            .get(t)
            .and_then(|p| p.resolution.npm_version())
            .map(str::to_owned)
        else {
            continue;
        };
        let is_peer_only = plan::declared(ws, &u.workspace, &u.name).is_none();
        if let Some(new) = updated_literal(
            &u.original,
            &version,
            config.exact && !is_peer_only,
            planned.latest,
        ) && new != u.original
        {
            edits.push(
                &u.workspace,
                Edit::Update {
                    name: u.name.clone(),
                    spec: new.clone(),
                },
            );
            set_edge_literal(graph, &u.workspace, &u.name, None, &new);
        }
    }
    edits
}

/// The outcome of the link step.
struct Linked {
    report: LinkReport,
    linker: NodeLinker,
    /// Packages installed (omit and `--filter` applied).
    reachable: Vec<bool>,
    /// Optional packages that could not be fetched and were left out.
    failed: u32,
}

/// `package.json#nativeDependencies` for the linker's native binlink list.
fn native_dependencies(config: &Config, ws: &Workspace) -> Option<Vec<String>> {
    if config.feature_flags.disable_native_dependency_linker {
        return Some(Vec::new());
    }
    ws.root
        .raw
        .get("nativeDependencies")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
}

/// Whether every edge into `id` is optional (so a failed fetch may drop it).
fn optional_only(graph: &LockGraph, id: PackageId) -> bool {
    let mut any = false;
    for p in &graph.packages {
        for e in &p.dependencies {
            if e.target == Some(id) {
                if !e.behavior.is_optional() {
                    return false;
                }
                any = true;
            }
        }
    }
    any
}

/// The graph as the linker reads it (bun's shape): the root has one membership edge
/// (`WORKSPACE | PROD`) per workspace member, and the root's declared dependencies on members
/// are plain edges. The resolver marks every edge into a project package with `WORKSPACE`
/// and adds no membership edges, and graphs read from fnpm.lock have neither, so both are
/// normalised here.
pub(crate) fn link_view(graph: &LockGraph) -> LockGraph {
    use fnpm_types::{Behavior, DepEdge};
    let mut g = graph.clone();
    let is_membership = |e: &DepEdge| {
        e.behavior.is_workspace()
            && e.target.is_some()
            && graph.workspaces.get(&e.literal).copied() == e.target
    };
    let mut have: Vec<PackageId> = Vec::new();
    for e in &mut g.packages[0].dependencies {
        if is_membership(e) {
            have.extend(e.target);
        } else if e.behavior.is_workspace() {
            e.behavior = e.behavior.with(Behavior::WORKSPACE, false);
        }
    }
    for (path, id) in &graph.workspaces {
        if path.is_empty() || have.contains(id) {
            continue;
        }
        let Some(name) = graph.get(*id).map(|p| p.name.clone()) else {
            continue;
        };
        let mut e = DepEdge::new(name, path.clone(), Behavior::WORKSPACE | Behavior::PROD);
        e.target = Some(*id);
        g.packages[0].dependencies.push(e);
    }
    g.sort_dependencies();
    g
}

/// The layout for `view` (a [`link_view`] of the graph). The hoisted tree from
/// `fnpm_lockfile::tree::hoist` records a workspace's own `node_modules` entries with the
/// workspace's node as parent and lists them in `workspace_roots`; the linker places
/// `workspace_roots` entries as top-level nodes of that workspace, so they are made tops here.
pub(crate) fn plan_layout(
    linker: &Linker,
    view: &LockGraph,
    kind: NodeLinker,
    selected: Option<Vec<PackageId>>,
) -> Layout {
    match kind {
        NodeLinker::Hoisted => {
            let mut layout = linker.plan(view, kind);
            if let Layout::Hoisted(tree) = &mut layout {
                for (_, ids) in &tree.workspace_roots {
                    for &id in ids {
                        if let Some(node) = tree.nodes.get_mut(id as usize) {
                            node.parent = None;
                        }
                    }
                }
            }
            layout
        }
        NodeLinker::Isolated => Layout::Isolated(linker.plan_isolated(view, selected)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn link(
    project: &Project,
    ws: &Workspace,
    lock: &lock::LockState,
    fetcher: &Fetcher,
    graph: &mut LockGraph,
    selected: Option<Vec<PackageId>>,
    trust_names: &[String],
    events: &Arc<dyn EventSink>,
) -> Result<Linked> {
    let config = &project.config;
    let root = project.root_dir.clone();
    let configured = config.linker.fixed().or(graph.node_linker);
    let migrated_npm = lock.migrated && lock.format == Some(Format::Npm);
    let kind = fnpm_linker::choose_linker(
        configured,
        graph.config_version,
        graph.has_workspaces(),
        migrated_npm,
        &root,
    );
    let mut failed = 0u32;
    loop {
        let policy = Arc::new(TrustPolicy::from_graph(graph, trust_names.iter().cloned()));
        let cfg = config.clone();
        let ignore = config.ignore_scripts;
        let trusted: fnpm_linker::TrustFn = Arc::new(move |g: &LockGraph, id: PackageId| {
            !ignore
                && g.get(id).is_some_and(|p| p.meta.needs_scripts())
                && policy.is_trusted_with_registry(g, id, &registry_lookup(&cfg))
        });
        let linker = Linker::new(config.clone(), fetcher.store().clone())
            .with_trust(trusted)
            .with_native_dependencies(native_dependencies(config, ws));
        let view = link_view(graph);
        let layout = plan_layout(&linker, &view, kind, selected.clone());
        let reachable = graph.reachable(linker.include(), selected.as_deref());
        for (i, p) in graph.packages.iter().enumerate() {
            if reachable[i] && needs_content(&p.resolution) && !p.is_disabled(&config.platform) {
                fetcher.ensure_package(p);
            }
        }
        let link_opts = LinkOptions {
            backend: config.backend.unwrap_or_else(fnpm_linker::default_backend),
            force: config.force,
            fresh: !root.join("node_modules").exists(),
            global_store: config.global_store,
            ..LinkOptions::default()
        };
        let (f, ev, dir) = (fetcher.clone(), events.clone(), root.clone());
        let result = tokio::task::spawn_blocking(move || {
            let source = f.source(&view);
            linker.link(&dir, &view, &layout, &source, &link_opts, &*ev)
        })
        .await
        .map_err(|e| Error::usage(format!("link task failed: {e}")))?;
        match result {
            Ok(report) => {
                return Ok(Linked {
                    report,
                    linker: kind,
                    reachable,
                    failed,
                });
            }
            Err(fnpm_linker::Error::Missing { package, message }) => {
                let id = graph
                    .ids()
                    .find(|id| graph.packages[id.index()].key() == package);
                match id {
                    Some(id) if optional_only(graph, id) && failed < 256 => {
                        events.emit(Event::warn(
                            format!("{package}: {message} (optional dependency skipped)"),
                            Some("OPTIONAL_DEPENDENCY_FAILED"),
                        ));
                        for p in &mut graph.packages {
                            for e in &mut p.dependencies {
                                if e.target == Some(id) {
                                    e.target = None;
                                }
                            }
                        }
                        failed += 1;
                    }
                    Some(id) => {
                        // Report the fetch failure with its own code (INTEGRITY, NETWORK, ...).
                        let pkg = &graph.packages[id.index()];
                        return Err(match fetcher.ensure_package(pkg).wait().await {
                            Err(f) => Error::Fetch {
                                package,
                                message: f.message,
                                code: f.code,
                            },
                            Ok(_) => Error::Link(fnpm_linker::Error::Missing { package, message }),
                        });
                    }
                    None => {
                        return Err(Error::Link(fnpm_linker::Error::Missing {
                            package,
                            message,
                        }));
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// `-g`: the bins of the global package.json's direct dependencies go to the global bin dir.
fn link_global_bins(config: &Config, root: &Path, graph: &LockGraph) -> Result<()> {
    use fnpm_linker::bins::{BinLinker, BinOptions};
    let nm = root.join("node_modules");
    std::fs::create_dir_all(&config.global_bin_dir).map_err(Error::io(&config.global_bin_dir))?;
    let mut bins = BinLinker::global(&config.global_bin_dir, &nm);
    for edge in &graph.root().dependencies {
        let Some(p) = edge.target.and_then(|t| graph.get(t)) else {
            continue;
        };
        if p.bin.is_none() {
            continue;
        }
        let dir = nm.join(&edge.name);
        bins.link(
            &edge.name,
            &p.bin,
            &dir,
            BinOptions {
                refresh: true,
                ..BinOptions::default()
            },
        )?;
    }
    Ok(())
}

/// Step 8: package.json edits, the migration write-back and the lockfile.
#[allow(clippy::too_many_arguments)]
fn save(
    root: &Path,
    config: &Config,
    ws: &Workspace,
    lock: &lock::LockState,
    edits: &Edits,
    graph: &LockGraph,
    dirty: bool,
    events: &dyn EventSink,
) -> Result<bool> {
    if config.save_package_json && !edits.is_empty() {
        edits.apply(root, ws)?;
    }
    let save_lock =
        config.lockfile.save && (dirty || config.lockfile.force_save || !edits.is_empty());
    if !save_lock {
        return Ok(false);
    }
    if config.save_package_json && !lock.write_back.is_empty() {
        for line in lock::write_back(root, ws, &lock.write_back)? {
            events.emit(Event::warn(line, Some("MIGRATED_PACKAGE_JSON")));
        }
    }
    fnpm_lockfile::save(root, graph)?;
    if config.lockfile.print_yarn {
        fnpm_lockfile::save_yarn(
            root,
            graph,
            &fnpm_lockfile::print::YarnOptions {
                registry: config.registry.url.clone(),
                ..Default::default()
            },
        )?;
    }
    Ok(true)
}

/// Step 7: lifecycle scripts of trusted packages, in dependency order. Returns
/// `(blocked scripts, failed optional packages)`.
async fn run_scripts(
    project: &Project,
    ws: &Workspace,
    graph: &LockGraph,
    linked: &Linked,
    trust_names: &[String],
    events: &Arc<dyn EventSink>,
) -> Result<(u32, u32)> {
    let config = &project.config;
    let root = project.root_dir.clone();
    let dirs: HashMap<PackageId, PathBuf> = linked.report.dirs.iter().cloned().collect();
    let locate = |id: PackageId| -> Option<PathBuf> {
        let p = graph.get(id)?;
        match &p.resolution {
            Resolution::Root => Some(root.clone()),
            Resolution::Workspace { path } => Some(root.join(path)),
            _ => dirs.get(&id).cloned(),
        }
    };
    let policy = TrustPolicy::from_graph(graph, trust_names.iter().cloned());
    let optimizer = Optimizer::from_root(&ws.root.raw, &project.env);
    let names: Vec<&str> = trust_names.iter().map(String::as_str).collect();
    let from_requests = fnpm_scripts::trust::trusted_from_requests(graph, &names);
    let cfg: &Config = config;
    let plan = fnpm_scripts::schedule::plan_install(
        graph,
        &policy,
        &optimizer,
        &fnpm_scripts::schedule::PlanOptions {
            ignore_scripts: config.ignore_scripts,
            platform: config.platform,
            trusted_from_requests: &from_requests,
            registry_for: &|n: &str| cfg.registry_for(n).url.as_str(),
            installed: Some(&linked.reachable),
        },
        &locate,
    )
    .map_err(Error::io(&root))?;
    let blocked = plan.blocked_scripts();
    if plan.jobs.is_empty() {
        return Ok((blocked, 0));
    }
    let opts = fnpm_scripts::RunOptions {
        concurrency: config.concurrent_scripts,
        shell: None,
        env: Vec::new(),
        silent: config.log_level == fnpm_config::LogLevel::Silent,
        project_dir: root.clone(),
    };
    let report = fnpm_scripts::run_dag(plan.jobs, plan.deps, &opts, &**events).await?;
    for name in &report.failed_optional {
        events.emit(Event::warn(
            format!("{name}: optional dependency script failed"),
            Some("OPTIONAL_SCRIPT_FAILED"),
        ));
    }
    Ok((blocked, report.failed_optional.len() as u32))
}

/// Writes the no-op state for the files as they are now.
fn write_state(
    root: &Path,
    ws: &Workspace,
    graph: &LockGraph,
    config_hash: u64,
    linker: &NodeLinker,
) {
    let Ok(bytes) = std::fs::read(root.join(fnpm_lockfile::FNPM_LOCK)) else {
        return;
    };
    let (files, workspace_patterns, workspace_dirs) = watched_files(ws, graph);
    let manifests = files
        .iter()
        .filter_map(|rel| fast_path::manifest_entry(root, rel).ok())
        .collect();
    let state = State {
        version: fast_path::STATE_VERSION,
        fnpm_version: crate::version().to_owned(),
        lockfile_hash: fnpm_lockfile::content_hash(&bytes),
        manifests,
        config_hash,
        node_modules: (0, 0, 0),
        linker: match linker {
            NodeLinker::Hoisted => "hoisted".into(),
            NodeLinker::Isolated => "isolated".into(),
        },
        packages: graph.len() as u32,
        workspace_patterns,
        workspace_dirs,
    };
    let _ = fast_path::write(root, &state);
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>(_: T) {}
    let _ = |o: InstallOptions, e: Arc<dyn EventSink>| is_send(run(o, e, Env::default()));
}
