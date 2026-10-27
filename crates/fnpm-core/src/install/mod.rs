//! The install pipeline (`docs/ARCHITECTURE.md#install-pipeline`): fast path, config and
//! workspace, lockfile (or migration), streaming resolve, fetch + extract overlapping the
//! resolve, link overlapping the downloads, bins, lifecycle scripts, then the lockfile,
//! package.json edits, the store's project record and the no-op state file.

mod backfill;
mod edits;
mod lock;
mod plan;
mod summary;

use std::collections::{HashMap, HashSet};
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
    // An edited `file:` folder is read again by the next install.
    for p in &graph.packages {
        if let fnpm_types::Resolution::Folder { path } = &p.resolution {
            let rel = format!("{path}/package.json");
            if !v.contains(&rel) {
                v.push(rel);
            }
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

pub(crate) use lock::{LockState, workspace_warnings};

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

/// Completes a graph migrated from yarn.lock or pnpm-lock.yaml from the registry manifests
/// (bins, install scripts, os/cpu, integrity), like bun does right after those migrations.
pub(crate) async fn backfill_migrated(
    lock: &mut lock::LockState,
    client: &fnpm_registry::RegistryClient,
    config: &Config,
) {
    if let Some(fill) = backfill::needed(lock.format)
        && let Some(graph) = lock.graph.as_mut()
    {
        backfill::run(graph, client, config, fill).await;
    }
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
    had_node_modules: bool,
    /// The workspace (with its warnings) and the previous lockfile, read while the runtime and
    /// the link pool start on other threads. `None` for a no-op.
    loaded: Option<(Workspace, Vec<String>, lock::LockState)>,
    pending_patch: Option<crate::commands::patch::PendingCommit>,
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
/// the install is not a no-op, the workspace and the lockfile are read here (with
/// `start_runtime`, while the tokio runtime starts on a background thread).
pub(crate) fn prepare(
    opts: InstallOptions,
    env: Env,
    start_runtime: bool,
) -> Result<PreparedInstall> {
    prepare_with_patch(opts, env, start_runtime, None)
}

fn prepare_with_patch(
    opts: InstallOptions,
    env: Env,
    start_runtime: bool,
    pending_patch: Option<crate::commands::patch::PendingCommit>,
) -> Result<PreparedInstall> {
    let start = Instant::now();
    let mut phases = Phases::new(&env);
    let mut opts = opts;
    if opts.analyze {
        // `--analyze <files>`: what the files import is added like positionals, with
        // `--only-missing` (bun runs its bundler; fnpm scans the import statements).
        let cwd = opts.pm.cwd.clone();
        let files = std::mem::take(&mut opts.packages);
        opts.packages = crate::analyze::packages(&cwd, &files)?;
        opts.analyze = false;
        opts.only_missing = true;
        opts.command = if opts.packages.is_empty() {
            InstallCommand::Install
        } else {
            InstallCommand::Add
        };
    }
    let mut cli = opts.pm.to_cli_overrides();
    if opts.exact {
        cli.exact = Some(true);
    }
    cli.update = opts.command == InstallCommand::Update;
    let is_add = opts.command == InstallCommand::Add
        || (opts.command == InstallCommand::Install && !opts.packages.is_empty());
    let project = Project::load_with_root(&opts.pm.cwd, cli, env, is_add, opts.dlx.is_some())?;
    phases.mark("config");

    // 1. Fast path.
    let config = &project.config;
    let hash = config_hash(config);
    // Sidecar/cache writes may recreate node_modules before resolution.
    let had_node_modules = project.root_dir.join("node_modules").is_dir();
    let mut noop = None;
    if pending_patch.is_none()
        && opts.command == InstallCommand::Install
        && opts.packages.is_empty()
        && opts.filter.is_empty()
        && !config.force
        && !opts.pm.lockfile_only
        && !opts.pm.trust
        && config.install_packages
        && config.security_scanner.is_none()
        // `--save-text-lockfile` turns bun.lockb into fnpm.lock, even on a no-op install.
        && !(config.save_text_lockfile == Some(true)
            && project.root_dir.join(fnpm_lockfile::BUN_LOCKB).exists())
        && opts.patch.is_none()
        && opts.manifest_updates.is_empty()
        && opts.interactive_choices.is_none()
        && let Check::Noop { packages, installs } =
            fast_path::check(&project.root_dir, &project.meta_dir(), hash)
    {
        noop = Some(InstallSummary {
            // A project without dependencies checks nothing: bun prints `[1.00ms] done`.
            checked: (installs > 0).then_some(CheckedCount { installs, packages }),
            elapsed_ms: start.elapsed().as_millis() as u64,
            noop: true,
            ..InstallSummary::default()
        });
    }
    phases.mark("fast path");
    let loaded = if noop.is_none() {
        if start_runtime {
            let _ = std::thread::Builder::new()
                .name("fnpm-rt-warm".into())
                .spawn(|| {
                    let _ = crate::runtime();
                });
        }
        let (ws, warnings, lock) = load_workspace(config, &project.root_dir)?;
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
        had_node_modules,
        loaded,
        pending_patch,
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

pub(crate) async fn run_with_patch(
    opts: InstallOptions,
    events: Arc<dyn EventSink>,
    env: Env,
    pending: crate::commands::patch::PendingCommit,
) -> Result<InstallSummary> {
    prepare_with_patch(opts, env, false, Some(pending))?
        .run(events)
        .await
}

/// The workspace and its lockfile. Like bun, `--frozen-lockfile` tolerates a listed workspace
/// whose package.json is missing when the lockfile lists it (a pruned checkout); any other
/// missing one is still `Workspace not found`.
fn load_workspace(
    config: &Config,
    root: &Path,
) -> Result<(Workspace, Vec<String>, lock::LockState)> {
    let (ws, warnings) = match Workspace::load_with(root, &Default::default()) {
        Err(fnpm_workspace::Error::WorkspaceNotFound(entry)) if config.frozen_lockfile => {
            let skip = fnpm_workspace::LoadOptions {
                missing: fnpm_workspace::MissingWorkspace::Skip,
            };
            let (ws, warnings) = Workspace::load_with(root, &skip)?;
            let lock = lock::load(config, &ws)?;
            let listed = |path: &str| {
                lock.graph
                    .as_ref()
                    .is_some_and(|g| g.workspaces.contains_key(path))
            };
            let entries = match ws.root.raw.get("workspaces") {
                Some(serde_json::Value::Array(a)) => Some(a),
                Some(serde_json::Value::Object(o)) => o.get("packages").and_then(|p| p.as_array()),
                _ => None,
            };
            let missing = entries
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .filter(|e| !e.contains(['*', '?', '{', '[', '!']))
                .find(|e| {
                    let path = e.trim_start_matches("./").trim_end_matches('/');
                    !root.join(path).join("package.json").exists() && !listed(path)
                });
            if let Some(entry) = missing.or((lock.graph.is_none()).then_some(entry.as_str())) {
                return Err(not_found(root, entry));
            }
            return Ok((ws, warnings, lock));
        }
        Err(fnpm_workspace::Error::WorkspaceNotFound(entry)) => {
            return Err(not_found(root, &entry));
        }
        other => other?,
    };
    let lock = lock::load(config, &ws)?;
    Ok((ws, warnings, lock))
}

/// bun's `Workspace not found "<entry>"`, framed at the entry in the root package.json.
fn not_found(root: &Path, entry: &str) -> Error {
    let error = Error::from(fnpm_workspace::Error::WorkspaceNotFound(entry.to_owned()));
    let path = root.join("package.json");
    let frame = std::fs::read_to_string(&path).ok().and_then(|source| {
        fnpm_workspace::frame::Frame::of_string_after(&source, "workspaces", entry)
    });
    match frame {
        Some(frame) => Error::Framed {
            message: frame.framed(&path, &error.to_string()),
            source: Box::new(error),
        },
        None => error,
    }
}

/// Writes fnpm.lock (with its sidecar), or bun.lockb when the project keeps it. Converting
/// bun.lockb to fnpm.lock removes it, as bun does. Returns whether a file changed.
fn write_lockfile(
    root: &Path,
    config: &Config,
    lock: &lock::LockState,
    graph: &LockGraph,
    prepared: Option<fnpm_lockfile::PreparedSave>,
) -> Result<bool> {
    if lock::saves_binary(config, lock) {
        return Ok(fnpm_lockfile::save_binary(root, graph)?);
    }
    let path = root.join(fnpm_lockfile::FNPM_LOCK);
    let before = std::fs::read(&path).ok();
    let hash = match prepared {
        Some(p) => {
            fnpm_lockfile::save_prepared_with(root, &p, Some(&lock::sidecar_path(config, root)))?
        }
        None => fnpm_lockfile::save_with(root, graph, Some(&lock::sidecar_path(config, root)))?,
    };
    if lock.format == Some(Format::BunBinary) {
        let lockb = root.join(fnpm_lockfile::BUN_LOCKB);
        match std::fs::remove_file(&lockb) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&lockb)(e)),
        }
    }
    Ok(before.is_none_or(|b| fnpm_lockfile::content_hash(&b) != hash))
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
        pending_patch,
        had_node_modules,
        ..
    } = prepared;
    let config = project.config.clone();
    let root = project.root_dir.clone();
    // 2. Workspace and 3. lockfile (read by `prepare`).
    let (mut ws, ws_warnings, mut lock) = match loaded {
        Some(l) => l,
        None => load_workspace(&config, &root)?,
    };
    for w in lock::workspace_warnings(&ws_warnings, &lock)
        .cloned()
        .chain(ws.self_contained().1)
    {
        events.emit(Event::warn(w, None));
    }
    for e in &lock.failures {
        events.emit(e.clone());
    }
    for w in &lock.warnings {
        events.emit(Event::warn(w.clone(), None));
    }
    // Like bun, another manager's lockfile is migrated without asking; fnpm.lock is written
    // next to it, which stays as it is.
    if lock.migrated
        && let Some(name) = lock.path.as_deref().and_then(Path::file_name)
    {
        events.emit(Event::notice(format!(
            "migrated lockfile from {}",
            name.to_string_lossy()
        )));
        if config.frozen_lockfile
            && !opts.pm.lockfile_only
            && config.log_level == fnpm_config::LogLevel::Verbose
        {
            let files = if lock.write_back.is_empty() {
                "fnpm.lock"
            } else {
                "fnpm.lock and package.json"
            };
            events.emit(Event::notice(format!(
                "note: lockfile is frozen, migration from {} not written to {files}; run 'fnpm install' and commit the result",
                name.to_string_lossy()
            )));
        }
    }
    lock::apply_in_memory(&mut ws, &lock.write_back)?;
    if let Some(pending) = &pending_patch {
        ws.root = fnpm_workspace::PackageJson::parse(&pending.manifest)?;
    }
    let mut deferred_edits = Edits::default();
    for (member, updates) in &opts.manifest_updates {
        let (manifest, path) = if member.is_empty() {
            (&mut ws.root, root.join("package.json"))
        } else {
            let m = ws
                .members
                .iter_mut()
                .find(|m| &m.path == member)
                .ok_or_else(|| Error::usage(format!("workspace not found: {member}")))?;
            (&mut m.manifest, m.dir.join("package.json"))
        };
        let edits: Vec<_> = updates
            .iter()
            .map(|(name, spec)| Edit::Update {
                name: name.clone(),
                spec: spec.clone(),
            })
            .collect();
        let text = edits::apply_text(&manifest.raw.to_string(), &edits)?;
        let value = serde_json::from_str(&text).map_err(|e| Error::usage(e.to_string()))?;
        *manifest = fnpm_workspace::PackageJson::from_value(value, &path)?;
        deferred_edits.by_workspace.insert(member.clone(), edits);
    }

    if let Some(choices) = &opts.interactive_choices {
        for (member, edits) in crate::commands::interactive_edits(&root, &ws, choices.clone())? {
            let (manifest, path) = if member.is_empty() {
                (&mut ws.root, root.join("package.json"))
            } else {
                let m = ws
                    .members
                    .iter_mut()
                    .find(|m| m.path == member)
                    .ok_or_else(|| Error::usage(format!("workspace not found: {member}")))?;
                (&mut m.manifest, m.dir.join("package.json"))
            };
            let text = edits::apply_text(&manifest.raw.to_string(), &edits)?;
            let value = serde_json::from_str(&text).map_err(|e| Error::usage(e.to_string()))?;
            *manifest = fnpm_workspace::PackageJson::from_value(value, &path)?;
            deferred_edits
                .by_workspace
                .entry(member)
                .or_default()
                .extend(edits);
        }
    }

    if let (Some(patch), Some(graph)) = (&opts.patch, lock.graph.as_mut()) {
        (patch.0)(graph);
    }

    // What the command asks for.
    let mut planned = plan::plan(&opts, &config, &ws, &project.cwd, &*events)?;

    if opts.dlx.is_some_and(|dlx| dlx.refresh) {
        let mut names: Vec<_> = lock
            .graph
            .as_ref()
            .into_iter()
            .flat_map(|g| g.packages.iter().skip(1).map(|p| p.name.clone()))
            .collect();
        names.extend(planned.requests.iter().map(|r| r.name.clone()));
        names.sort();
        names.dedup();
        planned.mode = Mode::Update {
            names,
            latest: false,
        };
    }

    // 4. + 5. Resolve, streaming packages to the fetcher.
    let client = project.client()?;
    client.set_event_sink(events.clone());
    let telemetry = crate::events::TelemetryGuard::start(
        &client,
        &events,
        opts.pm.progress
            && matches!(
                opts.pm.log_level,
                fnpm_config::LogLevel::Default | fnpm_config::LogLevel::Verbose
            ),
    );
    // `add <name>`: start reading (or fetching) the requested manifests while the resolve is
    // set up; the resolver joins the request in flight.
    let registry_names: Vec<String> = planned
        .requests
        .iter()
        .filter(|r| {
            fnpm_types::name::is_npm_package_name(&r.name)
                && !r.spec.contains(':')
                && !r.spec.contains('/')
        })
        .map(|r| r.name.clone())
        .collect();
    // An age gate needs full manifests; abbreviated look-ahead would issue a second request.
    if !registry_names.is_empty() && config.minimum_release_age_ms.is_none_or(|age| age == 0) {
        client.prefetch(registry_names);
    }
    if lock.migrated {
        backfill_migrated(&mut lock, &client, &config).await;
    }
    let store = project.store()?;
    let fetcher = Fetcher::new(
        store.clone(),
        client.clone(),
        root.clone(),
        config.cache_dir.clone(),
        config.verify_integrity,
        events.clone(),
    )
    .with_reuse_counter(telemetry.as_ref().map(|guard| guard.reuse_counter()))
    .with_unpinned_reuse(!config.cache_disabled);
    let omits = config.omit.dev || config.omit.optional || config.omit.peer || config.production;
    let stream =
        config.install_packages && !opts.pm.lockfile_only && !omits && planned.selected.is_none();
    let previous = if config.force && planned.mode != Mode::Frozen {
        None
    } else {
        lock.graph.as_ref()
    };
    let resolver = Resolver::new(config.clone(), client.clone()).with_patch_hashes(
        pending_patch
            .as_ref()
            .map(|p| p.patch_hashes())
            .unwrap_or_default(),
    );
    // With node_modules in place (add, remove, an edited package.json), the packages the
    // previous lockfile had are most likely installed already: the linker verifies them in
    // place and only asks the store for the ones it has to (re)build, so they are not looked up
    // (or downloaded) up front.
    // A migrated lockfile may lack integrity values the fetches fill in, so it fetches all.
    let in_place = !config.force && !lock.migrated && had_node_modules;
    let skip = Arc::new(if stream {
        let mut skip = not_fetched(previous, &config);
        if in_place && let Some(prev) = previous {
            skip.extend(prev.packages.iter().map(|p| p.key()));
        }
        skip
    } else {
        HashSet::new()
    });
    let lockfile_name = lock.path.as_deref().and_then(Path::file_name).map_or_else(
        || fnpm_lockfile::FNPM_LOCK.to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let mut out = resolve(
        &resolver,
        &ws,
        previous,
        &planned,
        stream.then(|| (&fetcher, skip.clone())),
        &events,
        &lockfile_name,
        config.log_level == fnpm_config::LogLevel::Silent,
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
            stream.then(|| (&fetcher, skip.clone())),
            &events,
            &lockfile_name,
            config.log_level == fnpm_config::LogLevel::Silent,
        )
        .await?;
    }
    // bun writes an added range to package.json before it resolves it, so a range that the
    // workspace of that name satisfies links the workspace: `fnpm add no-deps@latest` next to
    // a no-deps 2.0.0 member writes `^2.0.0` and links the member. Resolve once more with it.
    if config.link_workspace_packages {
        let mut relinked = false;
        for add in &mut planned.adds {
            let req = &planned.requests[add.request];
            if add.catalog.is_some() || !add.positional.has_version() {
                continue;
            }
            let Some(member) = ws.member(&req.name) else {
                continue;
            };
            let Some(version) = out
                .requests
                .iter()
                .find(|(r, _)| r == req)
                .and_then(|(_, id)| out.graph.get(*id))
                .filter(|p| p.name == req.name)
                .and_then(|p| p.resolution.npm_version())
            else {
                continue;
            };
            let literal = crate::spec::added_literal(&add.positional, version, config.exact);
            let links = matches!(
                (
                    fnpm_semver::Range::parse(&literal),
                    fnpm_semver::Version::parse(&member.version),
                ),
                (Ok(r), Ok(v)) if r.satisfies(&v)
            );
            if links {
                planned.requests[add.request].spec = literal;
                add.relinked = true;
                relinked = true;
            }
        }
        if relinked {
            out = resolve(
                &resolver,
                &ws,
                previous,
                &planned,
                stream.then(|| (&fetcher, skip.clone())),
                &events,
                &lockfile_name,
                config.log_level == fnpm_config::LogLevel::Silent,
            )
            .await?;
        }
    }
    // bun announces the resolve when it had to ask the network; its warnings follow.
    let mut announced = false;
    announce_resolving(&client, &*events, &mut announced);
    for w in &out.warnings {
        events.emit(Event::warn(w.clone(), None));
    }
    phases.mark("resolve");
    let ResolveOutput {
        mut graph,
        mut changed,
        behind,
        requests: resolved_requests,
        unsaved,
        ..
    } = out;
    let config = if unsaved {
        let mut c = (*config).clone();
        c.lockfile.save = false;
        Arc::new(c)
    } else {
        config
    };

    // package.json edits, mirrored into the graph so the saved lockfile matches them.
    let mut edits = package_json_edits(
        &opts,
        &config,
        &ws,
        &planned,
        &resolved_requests,
        &mut graph,
    );

    for (workspace, updates) in deferred_edits.by_workspace {
        edits
            .by_workspace
            .entry(workspace)
            .or_default()
            .extend(updates);
    }

    // bun writes the ranges `update` picks to package.json before it resolves them, so a range
    // that now reaches the workspace of that name (directly or through a rule) links it.
    if opts.command == InstallCommand::Update
        && config.link_workspace_packages
        && let Some(edited) = edits::edited_for_members(&ws, &edits)
    {
        let again = resolver
            .resolve(
                ResolveInput {
                    workspace: &edited,
                    previous,
                    mode: Mode::Normal,
                    requests: Vec::new(),
                    removals: Vec::new(),
                },
                None,
                &*events,
            )
            .await?;
        graph = again.graph;
        changed = true;
    }

    let kind = fnpm_linker::choose_linker(
        config.linker.fixed().or(graph.node_linker),
        graph.config_version,
        graph.has_workspaces(),
        lock.migrated && lock.format == Some(Format::Npm),
        &root,
    );
    // An unsaved graph (an omitted local dependency left unresolved) would not load.
    if kind == NodeLinker::Isolated && !unsaved && (changed || lock.migrated || !edits.is_empty()) {
        lock::normalize_bindings(&mut graph, &config)?;
    }

    // Without a link (`--lockfile-only`, `--dry-run`) the rows are what the lockfile gained.
    let (mut added, mut updated, mut removed) = summary::changes(lock.graph.as_ref(), &graph);
    if opts.command == InstallCommand::Remove {
        // The graph differ decides which declarations were removed. Keep that gate (a
        // peer never recorded in a lockfile has no row), but display them in argument order.
        removed.sort_by_key(|name| {
            opts.packages
                .iter()
                .position(|input| input.trim() == name)
                .unwrap_or(usize::MAX)
        });
    }
    if opts.pm.lockfile_only || !config.install_packages {
        for pkg in &mut added {
            pkg.available_version =
                summary::later_version(&graph, &client, &config, &pkg.name, &pkg.version);
        }
    }
    // bun lists `↑` rows only for `update`; an install shows what it wrote as `+` rows.
    if opts.command != InstallCommand::Update {
        updated.clear();
    }
    for pkg in &mut updated {
        pkg.available_version =
            summary::later_version(&graph, &client, &config, &pkg.name, &pkg.to);
    }
    let requested = summary::requested(&graph, &resolved_requests, &config.platform);
    let mut summary = InstallSummary {
        added,
        updated,
        removed,
        requested,
        ..InstallSummary::default()
    };
    let lockfile_dirty = pending_patch.is_some()
        || changed
        || opts.patch.is_some()
        || lock.migrated
        // bun re-saves a v0 lockfile as v1: v0 is a different layout of the workspaces (a
        // project without workspace members reads the same in both).
        || lock
            .graph
            .as_ref()
            .is_some_and(|g| g.lockfile_version == 0 && g.workspaces.len() > 1)
        || lock.config_version_missing
        || !root.join(lock::target_name(&config, &lock)).exists();

    let mut patch_rollback = if !config.dry_run {
        pending_patch
            .as_ref()
            .map(|pending| {
                crate::commands::patch::CommitRollback::new(
                    &root,
                    pending,
                    config.lockfile.print_yarn,
                )
            })
            .transpose()?
    } else {
        None
    };
    if !config.dry_run
        && let Some(pending) = &pending_patch
    {
        pending.write_patch()?;
    }

    if opts.pm.lockfile_only || !config.install_packages {
        announce_resolved(&client, &*events, announced);
        // `--lockfile-only` writes the lockfile; `--dry-run` writes nothing.
        if opts.pm.lockfile_only && !config.dry_run {
            if config.save_package_json
                && let Some(pending) = &pending_patch
            {
                crate::project::write_atomic(
                    &root.join("package.json"),
                    pending.manifest.as_bytes(),
                )?;
            }
            let mut saved = save(
                &root,
                &config,
                &ws,
                &lock,
                &edits,
                &graph,
                lockfile_dirty,
                None,
                &*events,
            )?;
            // Like bun, `--lockfile-only` rewrites an up-to-date lockfile too, so the file is in
            // the writer's form afterwards; it was saved only when that changed its bytes
            // (`Saved lockfile`, `Saved fnpm.lock (N packages)`), else bun says `Checked`.
            if !saved && config.lockfile.save {
                saved = write_lockfile(&root, &config, &lock, &graph, None)?;
                if saved {
                    events.emit(Event::status("Saved lockfile"));
                }
            }
            // bun's line under `--lockfile-only`: `Saved bun.lock (N packages)`, or `Checked`;
            // nothing when the lockfile is not saved (`--frozen-lockfile`).
            summary.lockfile_only =
                (config.lockfile.save || saved).then(|| fnpm_types::LockfileOnly {
                    saved,
                    packages: graph.len() as u32,
                    binary: lock::saves_binary(&config, &lock),
                });
        }
        if let Some(rollback) = &mut patch_rollback {
            rollback.finish();
        }
        summary.elapsed_ms = start.elapsed().as_millis() as u64;
        client.flush_cache_writes().await;
        return Ok(summary);
    }

    // The security scanner sees what would be installed before anything is linked.
    if config.security_scanner.is_some() {
        let loaded_fnpm_lock = lock.graph.is_some() && !lock.migrated;
        let mut requested_ids: Vec<PackageId> =
            resolved_requests.iter().map(|(_, id)| *id).collect();
        if opts.command == InstallCommand::Update && !opts.packages.is_empty() {
            for target in &planned.updates {
                let owner = if target.workspace.is_empty() {
                    Some(PackageId::ROOT)
                } else {
                    graph.workspaces.get(&target.workspace).copied()
                };
                if let Some(owner) = owner {
                    requested_ids.extend(
                        graph.packages[owner.index()]
                            .dependencies
                            .iter()
                            .filter(|edge| edge.name == target.name)
                            .filter_map(|edge| edge.target),
                    );
                }
            }
        }
        let scan_all = requested_ids.is_empty()
            || !loaded_fnpm_lock
            || !had_node_modules
            || opts.command == InstallCommand::Remove
            || pending_patch.is_some()
            || requested_ids.iter().any(|id| {
                matches!(
                    graph.packages[id.index()].resolution,
                    Resolution::Symlink { .. }
                )
            });
        let seeds = (!scan_all).then_some(requested_ids);
        let (config, root, graph, env) = (
            config.clone(),
            root.clone(),
            graph.clone(),
            project.env.clone(),
        );
        let json = opts.pm.json;
        tokio::task::spawn_blocking(move || {
            crate::security::scan_for_install(&crate::security::Scan {
                config: &config,
                root: &root,
                graph: &graph,
                seeds,
                env: &env,
                json,
            })
        })
        .await
        .map_err(|e| Error::usage(format!("security scanner task failed: {e}")))??;
    }

    // A name fnpm.lock carries (the resolver reused it) is checked like a resolved one.
    if let Some(bad) = unsafe_dependency_name(&graph) {
        return Err(Error::UnsafeName(fnpm_types::text::escape_control_chars(
            &bad,
        )));
    }

    // 6. Link, overlapping the downloads still in flight. When node_modules is what the
    // previous lockfile's install left (the no-op state still matches it), packages that keep
    // their place are trusted without being read.
    let intact = in_place
        // A command-provided graph patch no longer describes the installed tree.
        && opts.patch.is_none()
        && previous.is_some()
        && lock.hash.is_some_and(|h| {
            fast_path::layout_intact(&root, &project.meta_dir(), hash, h, "hoisted")
        });
    fast_path::remove(&root, &project.meta_dir());
    if config.global {
        // `remove -g`: the global bin links of removed packages go before their directories.
        for (_, name) in &planned.removals {
            let dir = fnpm_linker::fsutil::join_name(&root.join("node_modules"), name);
            let _ = fnpm_linker::bins::unlink_bins(&dir, &config.global_bin_dir);
        }
    }
    let selected_ids: Option<Vec<PackageId>> = planned.selected.as_ref().and_then(|paths| {
        (opts.command != InstallCommand::Update).then(|| {
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
    // The lockfile text and its sidecar are built beside the link step when linking cannot
    // change the graph (no integrity left to fill in, no `--trust`).
    let complete = graph.packages.iter().all(|p| {
        !needs_content(&p.resolution)
            || matches!(p.resolution, Resolution::Folder { .. })
            || p.resolution.integrity().is_some()
    });
    let prepared = (config.lockfile.save
        && (lockfile_dirty || config.lockfile.force_save || !edits.is_empty())
        && !opts.pm.trust
        && complete)
        .then(|| {
            let g = graph.clone();
            tokio::task::spawn_blocking(move || fnpm_lockfile::prepare_save(&g))
        });
    let lazy: HashSet<String> = match previous {
        Some(prev) if in_place => prev.packages.iter().map(|p| p.key()).collect(),
        _ => HashSet::new(),
    };
    let linked = link(
        &project,
        &ws,
        &lock,
        &fetcher,
        &mut graph,
        selected_ids.clone(),
        &trust_names,
        &lazy,
        intact.then_some(previous).flatten(),
        &events,
        opts.dlx,
        telemetry.as_ref(),
    )
    .await?;
    phases.mark("link");
    // `+` rows: the direct dependencies of the cwd's workspace that this run wrote.
    let ws_path = ws
        .member_for_dir(&project.cwd)
        .map(|m| m.path.clone())
        .unwrap_or_default();
    summary.added = summary::installed_direct(&graph, &ws_path, &linked.report.written)
        .into_iter()
        .filter(|pkg| !summary.updated.iter().any(|u| u.name == pkg.name))
        .collect();
    for pkg in &mut summary.added {
        let real = pkg.alias.as_ref().and_then(|_| {
            graph
                .workspaces
                .get(&ws_path)
                .and_then(|id| graph.get(*id))
                .and_then(|p| p.dependencies.iter().find(|e| e.name == pkg.name))
                .and_then(|e| e.target)
                .and_then(|t| graph.get(t))
                .map(|t| t.name.clone())
        });
        let name = real.unwrap_or_else(|| pkg.name.clone());
        pkg.available_version =
            summary::later_version(&graph, &client, &config, &name, &pkg.version);
    }
    // bun counts each package once, however many places it was copied to.
    summary.installed_count = linked.report.installed_packages;
    summary.removed_count = linked.report.removed;
    summary.failed_count = linked.failed;
    // Nothing written: bun reports the places it checked (`Checked 4 installs across 3
    // packages`), for `install` and `update` (not for an `add` or `remove`).
    if summary.installed_count == 0
        && summary.removed_count == 0
        && summary.failed_count == 0
        && (linked.report.verified > 0 || !opts.filter.is_empty())
        && matches!(
            opts.command,
            InstallCommand::Install | InstallCommand::Update
        )
    {
        summary.checked = Some(CheckedCount {
            installs: linked.report.verified,
            packages: graph.len() as u32,
        });
    }

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

    // Every download is done (the link waited for them).
    announce_resolved(&client, &*events, announced);

    // Global installs link the direct dependencies' bins into the global bin directory.
    if config.global {
        link_global_bins(&config, &root, &graph)?;
    }

    // 8. Save (before scripts, so a failing script leaves a consistent lockfile).
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
    let prepared = match prepared {
        Some(task) if !integrity_filled && linked.failed == 0 => {
            task.await.ok().and_then(Result::ok)
        }
        _ => None,
    };
    if config.save_package_json
        && let Some(pending) = &pending_patch
    {
        crate::project::write_atomic(&root.join("package.json"), pending.manifest.as_bytes())?;
    }
    let saved = save(
        &root,
        &config,
        &ws,
        &lock,
        &edits,
        &graph,
        lockfile_dirty || integrity_filled,
        prepared,
        &*events,
    )?;
    if let Some(rollback) = &mut patch_rollback {
        rollback.finish();
    }
    phases.mark("save");
    // Caches and the store's project record, off the critical path (awaited before the
    // state file).
    let packages = fetcher.store_dir_names(&graph, &linked.reachable).await;
    let background = {
        let (store, root, config) = (store.clone(), root.clone(), config.clone());
        // `save` wrote the sidecar with the lockfile; otherwise a parsed fnpm.lock leaves one.
        let loaded = (!saved && lock.sidecar_missing && lock.format == Some(Format::Fnpm))
            .then(|| lock.graph.clone().zip(lock.hash))
            .flatten();
        tokio::task::spawn_blocking(move || {
            let _ = store.set_project_packages(&root, packages);
            if let Some((graph, hash)) = loaded {
                lock::save_sidecar(&config, &root, hash, &graph);
            }
        })
    };
    phases.mark("register");

    if linked.linker == NodeLinker::Isolated {
        layout_aliases(&root, &*events);
    }

    // 7. Lifecycle scripts.
    let (blocked, failed_optional) = run_scripts(
        &project,
        &ws,
        &graph,
        &linked,
        selected_ids.as_deref(),
        &trust_names,
        &events,
    )
    .await?;
    phases.mark("scripts");
    summary.blocked_scripts = blocked;

    let _ = background.await;
    // The state file goes last, after everything succeeded.
    // Not when this run left the lockfile or package.json behind node_modules (`--no-save`,
    // a frozen install never changes either).
    // Not either after a failed optional script: like bun, the next install tries it again.
    let saved_everything = !behind
        && (config.lockfile.save || !(lockfile_dirty || integrity_filled))
        && (config.save_package_json || edits.is_empty())
        && !failed_optional;
    // A `--filter` install links only part of the project; `update -r` links all of it.
    if selected_ids.is_none() && saved_everything {
        // Directories the linker moved aside are deleted in the background; their removal
        // would change node_modules' mtime after the state recorded it.
        if linked.report.removed > 0 || linked.report.installed > 0 {
            let _ = tokio::task::spawn_blocking(fnpm_linker::flush_background_deletes).await;
        }
        let installs = linked.report.installed + linked.report.verified;
        write_state(
            &root,
            &project.meta_dir(),
            &ws,
            &graph,
            hash,
            &linked.linker,
            installs,
        );
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

/// [`LockGraph::reachable`] that stops at packages built for another platform: like bun, their
/// dependencies are not installed either (`@img/sharp-wasm32` under the disabled
/// `@img/sharp-webcontainers-wasm32`), so they are neither fetched, linked nor counted.
pub(crate) fn installable(
    graph: &LockGraph,
    include: fnpm_types::Behavior,
    roots: Option<&[PackageId]>,
    platform: &fnpm_types::Platform,
) -> Vec<bool> {
    use fnpm_types::Behavior;
    let mut seen = vec![false; graph.packages.len()];
    let mut stack: Vec<PackageId> = match roots {
        Some(r) => r.to_vec(),
        None => graph.workspaces.values().copied().collect(),
    };
    while let Some(id) = stack.pop() {
        let Some(p) = graph.packages.get(id.index()) else {
            continue;
        };
        if seen[id.index()] {
            continue;
        }
        let local = p.resolution.is_local_tree();
        if !local && p.is_disabled(platform) {
            continue;
        }
        seen[id.index()] = true;
        for e in &p.dependencies {
            let Some(t) = e.target else { continue };
            let b = if local {
                e.behavior
            } else {
                e.behavior.with(Behavior::DEV, false)
            };
            let follows =
                b.intersects(include) || (b.is_workspace() && include.intersects(Behavior::PROD));
            if follows && !seen.get(t.index()).copied().unwrap_or(true) {
                stack.push(t);
            }
        }
    }
    seen
}

/// Whether the store must hold `p` for this install: it has content, and it is not a bundled
/// dependency (those come inside their parent's tarball).
fn fetches(p: &fnpm_types::Package) -> bool {
    needs_content(&p.resolution) && !p.meta.bundled
}

/// The `name@resolution` keys of the previous graph that this install will not fetch
/// (bundled, or only reachable through packages for another platform). The streaming consumer
/// skips them; a package that does end up installed is fetched by the link step.
fn not_fetched(previous: Option<&LockGraph>, config: &Config) -> HashSet<String> {
    let Some(g) = previous else {
        return HashSet::new();
    };
    let include = Linker::include_for(config);
    let wanted = installable(g, include, None, &config.platform);
    g.packages
        .iter()
        .enumerate()
        .filter(|(i, p)| needs_content(&p.resolution) && !(wanted[*i] && fetches(p)))
        .map(|(_, p)| p.key())
        .collect()
}

/// One resolve; with a fetcher, every streamed package starts its fetch immediately (except
/// the `skip` keys).
#[allow(clippy::too_many_arguments)]
async fn resolve(
    resolver: &Resolver,
    ws: &Workspace,
    previous: Option<&LockGraph>,
    planned: &Planned,
    fetcher: Option<(&Fetcher, Arc<HashSet<String>>)>,
    events: &Arc<dyn EventSink>,
    lockfile: &str,
    silent: bool,
) -> Result<ResolveOutput> {
    let (tx, consumer) = match fetcher {
        Some((f, skip)) => {
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
                    if needs_content(&resolution)
                        && (skip.is_empty() || !skip.contains(&format!("{name}@{resolution}")))
                    {
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
    match out {
        // bun's pruned-checkout error says nothing under `--silent`, and still fails.
        Err(fnpm_resolver::Error::PrunedWorkspace { .. }) if silent => Err(Error::Reported),
        out => Ok(out.map_err(|e| e.with_lockfile(lockfile))?),
    }
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
                if add.relinked {
                    req.spec.clone()
                } else if p.literal.starts_with("workspace:")
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
            let entry = add
                .catalog_existing
                .clone()
                .or_else(|| add.catalog_seed.clone())
                .unwrap_or(literal);
            if add.catalog_existing.is_none() {
                edits.push(
                    "",
                    Edit::Catalog {
                        catalog: cat.clone(),
                        name: req.name.clone(),
                        spec: entry.clone(),
                    },
                );
                graph
                    .catalogs
                    .entries_for(cat, &req.name)
                    .insert(req.name.clone(), entry);
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
        // A `catalog:` reference stays; the catalog entry it reads moves instead (bun).
        // bun moves the entry of a `catalog:` reference, and neither the entry nor the reference
        // of ` catalog:` (leading whitespace).
        if u.original.trim_start() != u.original && u.original.trim().starts_with("catalog:") {
            continue;
        }
        if let Some(catalog) = u.original.strip_prefix("catalog:") {
            let catalog = match catalog.trim() {
                "default" => "",
                c => c,
            };
            let Some(entry) = edits::catalog_entry(&ws.root.raw, catalog, &u.name) else {
                continue;
            };
            let entry = entry.trim().to_owned();
            let new = updated_literal(&entry, &version, config.exact, planned.latest)
                .unwrap_or_else(|| entry.clone());
            let map = graph.catalogs.entries_for(catalog, &u.name);
            if map.get(&u.name) == Some(&new) && new == entry {
                continue;
            }
            map.insert(u.name.clone(), new.clone());
            let edit = Edit::Catalog {
                catalog: catalog.to_owned(),
                name: u.name.clone(),
                spec: new,
            };
            if !edits
                .by_workspace
                .get("")
                .is_some_and(|e| e.contains(&edit))
            {
                edits.push("", edit);
            }
            continue;
        }
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
/// Packages reachable from the root and the workspaces through edges that are not optional.
fn required_without_optional(graph: &LockGraph, reachable: &[bool]) -> Vec<bool> {
    let mut required = vec![false; graph.packages.len()];
    let mut queue: Vec<usize> = graph
        .packages
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            matches!(
                p.resolution,
                fnpm_types::Resolution::Root | fnpm_types::Resolution::Workspace { .. }
            )
        })
        .map(|(i, _)| i)
        .collect();
    for &i in &queue {
        required[i] = true;
    }
    while let Some(i) = queue.pop() {
        for e in &graph.packages[i].dependencies {
            if e.behavior.is_optional() || e.behavior.is_optional_peer() {
                continue;
            }
            if let Some(t) = e.target
                && reachable.get(t.index()).copied().unwrap_or(false)
                && !required[t.index()]
            {
                required[t.index()] = true;
                queue.push(t.index());
            }
        }
    }
    required
}

/// The optional dependencies, not otherwise required, whose subtree contains one of `lost`.
fn optional_roots_reaching(
    graph: &LockGraph,
    lost: &[PackageId],
    required: &[bool],
) -> Vec<PackageId> {
    let mut roots = Vec::new();
    for p in &graph.packages {
        for e in &p.dependencies {
            let Some(t) = e.target else { continue };
            if !e.behavior.is_optional() || required[t.index()] || roots.contains(&t) {
                continue;
            }
            // Below `t` through anything not already required elsewhere.
            let mut seen = vec![false; graph.packages.len()];
            let mut stack = vec![t];
            let mut hit = false;
            while let Some(x) = stack.pop() {
                if seen[x.index()] {
                    continue;
                }
                seen[x.index()] = true;
                if lost.contains(&x) {
                    hit = true;
                    break;
                }
                for d in &graph.packages[x.index()].dependencies {
                    if let Some(y) = d.target
                        && !required[y.index()]
                    {
                        stack.push(y);
                    }
                }
            }
            if hit {
                roots.push(t);
            }
        }
    }
    roots
}

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
/// (`WORKSPACE | PROD`) per workspace member, and every declared dependency on a member, of
/// the root or of another workspace, is a plain edge. The resolver marks every edge into a
/// project package with `WORKSPACE` and adds no membership edges, and graphs read from
/// fnpm.lock have neither, so both are normalised here.
pub(crate) fn link_view(graph: &LockGraph) -> LockGraph {
    use fnpm_types::{Behavior, DepEdge};
    let mut g = graph.clone();
    let is_membership = |e: &DepEdge| {
        e.behavior.is_workspace()
            && e.target.is_some()
            && graph.workspaces.get(&e.literal).copied() == e.target
    };
    let mut have: Vec<PackageId> = Vec::new();
    for (i, p) in g.packages.iter_mut().enumerate() {
        for e in &mut p.dependencies {
            if i == 0 && is_membership(e) {
                have.extend(e.target);
            } else if e.behavior.is_workspace() {
                e.behavior = e.behavior.with(Behavior::WORKSPACE, false);
            }
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

/// Whether the flags of the install leave packages of the tree out (`--production`, `--omit`,
/// another `--os` / `--cpu` / `--libc`).
fn omits_packages(config: &Config) -> bool {
    config.production
        || config.omit.dev
        || config.omit.optional
        || config.omit.peer
        || config.platform != fnpm_types::Platform::current()
}

/// The layout for `view` (a [`link_view`] of the graph). The hoisted tree from
/// `fnpm_lockfile::tree::hoist` records a workspace's own `node_modules` entries with the
/// workspace's node as parent and lists them in `workspace_roots`; the linker places
/// `workspace_roots` entries as top-level nodes of that workspace, so they are made tops here.
/// `reserved`: workspaces in the lockfile that a frozen install leaves out (pruned from disk),
/// whose root folders stay theirs.
pub(crate) fn plan_layout(
    linker: &Linker,
    view: &LockGraph,
    kind: NodeLinker,
    selected: Option<Vec<PackageId>>,
    reserved: &[&str],
) -> Layout {
    match kind {
        NodeLinker::Hoisted => {
            let mut layout = Layout::Hoisted(fnpm_lockfile::tree::hoist_reserving(
                view,
                &fnpm_lockfile::tree::HoistOptions {
                    include: linker.include(),
                    platform: linker.config().platform,
                    workspaces: selected,
                    self_contained: linker.self_contained_ids(view),
                },
                reserved,
            ));
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
    lazy: &HashSet<String>,
    intact: Option<&LockGraph>,
    events: &Arc<dyn EventSink>,
    dlx: Option<crate::options::DlxMode>,
    telemetry: Option<&crate::events::TelemetryGuard>,
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
    // The names of the workspaces fnpm.lock has that a frozen install skips (not on disk).
    let pruned: Vec<&str> = match lock.graph.as_ref() {
        Some(prev) if config.frozen_lockfile => prev
            .workspaces
            .iter()
            .filter(|(path, _)| !path.is_empty() && !ws.members.iter().any(|m| &m.path == *path))
            .filter_map(|(_, id)| prev.get(*id).map(|p| p.name.as_str()))
            .collect(),
        _ => Vec::new(),
    };
    let mut failed = 0u32;
    let mut offline_missing = false;
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
            .with_native_dependencies(native_dependencies(config, ws))
            .with_self_contained(ws.self_contained().0);
        let mut view = link_view(graph);
        // A root scanner remains available even when its devDependency group is
        // omitted. Only the linking view changes; the saved declaration stays dev.
        if let Some(scanner) = config.security_scanner.as_deref() {
            for edge in &mut view.packages[0].dependencies {
                if edge.name == scanner {
                    edge.behavior = fnpm_types::Behavior::PROD;
                }
            }
        }
        let layout = plan_layout(&linker, &view, kind, selected.clone(), &pruned);
        // The previous install's tree, for packages that keep their place (hoisted only,
        // without patches, whose files may have changed under the same key).
        let previous = match (intact, &layout) {
            (Some(prev), Layout::Hoisted(_))
                if prev.patched_dependencies.is_empty()
                    && graph.patched_dependencies.is_empty()
                    && selected.is_none() =>
            {
                let prev_view = link_view(prev);
                match plan_layout(&linker, &prev_view, NodeLinker::Hoisted, None, &[]) {
                    Layout::Hoisted(tree) => Some(fnpm_linker::Previous {
                        graph: Arc::new(prev_view),
                        tree: Arc::new(tree),
                    }),
                    Layout::Isolated(_) => None,
                }
            }
            _ => None,
        };
        // Threads for the work there is: with the previous layout trusted, only the packages
        // that are new or moved are placed, and a full pool costs more to start than that work.
        let threads = match (&previous, &layout) {
            (Some(prev), Layout::Hoisted(tree)) => {
                let before: HashSet<String> = prev.graph.packages.iter().map(|p| p.key()).collect();
                let new = tree
                    .nodes
                    .iter()
                    .filter(|n| !before.contains(&view.packages[n.package.index()].key()))
                    .count();
                new.div_ceil(16).clamp(1, LinkOptions::default().threads)
            }
            _ => LinkOptions::default().threads,
        };
        if let Some(telemetry) = telemetry {
            telemetry.link_threads(threads);
        }
        let reachable = installable(
            &view,
            linker.include(),
            selected.as_deref(),
            &config.platform,
        );
        // Start every fetch the link will wait for; `lazy` packages are fetched when the linker
        // finds them missing.
        for (i, p) in graph.packages.iter().enumerate() {
            if reachable[i] && fetches(p) && (lazy.is_empty() || !lazy.contains(&p.key())) {
                fetcher.ensure_package(p);
            }
        }
        // --offline: like bun, every required package missing from the cache is reported, the
        // rest is installed, and the install fails.
        if config.network.offline == fnpm_config::OfflineMode::Offline {
            // What is not in the cache. A package node_modules already holds (`lazy`) needs none.
            let mut absent = vec![false; graph.packages.len()];
            for (i, p) in graph.packages.iter().enumerate() {
                if !reachable[i] || !fetches(p) || lazy.contains(&p.key()) {
                    continue;
                }
                if let Err(f) = fetcher.ensure_package(p).wait().await
                    && f.code == "OFFLINE"
                {
                    absent[i] = true;
                }
            }
            // An optional dependency is left out with everything below it that only it needs;
            // what the rest of the tree requires must be there.
            let required = required_without_optional(graph, &reachable);
            let mut reported = std::collections::BTreeSet::new();
            let mut skip = Vec::new();
            for (i, p) in graph.packages.iter().enumerate() {
                if !absent[i] {
                    continue;
                }
                if required[i] {
                    if reported.insert(p.name.clone()) {
                        events.emit(Event::Error {
                            message: format!("--offline: \"{}\" is not in the cache", p.name),
                            code: Some("OFFLINE".into()),
                        });
                    }
                    offline_missing = true;
                }
                skip.push(PackageId(i as u32));
            }
            // Optional edges whose subtree lost a package are dropped, like a failed optional.
            let skipped_optional = optional_roots_reaching(graph, &skip, &required);
            for p in &mut graph.packages {
                for e in &mut p.dependencies {
                    if e.target
                        .is_some_and(|t| skip.contains(&t) || skipped_optional.contains(&t))
                    {
                        e.target = None;
                    }
                }
            }
            if !skip.is_empty() {
                // Plan the layout again without them.
                continue;
            }
        }
        let previous_hoisted_workspaces = if kind == NodeLinker::Hoisted && graph.has_workspaces() {
            lock.graph
                .as_ref()
                .filter(|previous| *previous != graph)
                .map(|previous| {
                    let old_view = link_view(previous);
                    let Layout::Hoisted(old_tree) =
                        plan_layout(&linker, &old_view, kind, None, &[])
                    else {
                        unreachable!("hoisted layout requested");
                    };
                    old_tree
                        .workspace_roots
                        .iter()
                        .filter_map(|(id, _)| {
                            let path = previous
                                .workspaces
                                .iter()
                                .find_map(|(path, old)| (old == id).then_some(path))?;
                            graph.workspaces.get(path).copied()
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if dlx.is_some() {
            // `fnpm x`: nothing is linked until every package is in the store, so a refresh
            // that cannot download a package leaves the tree that runs as it was.
            for (i, p) in graph.packages.iter().enumerate() {
                if !reachable[i] || !fetches(p) {
                    continue;
                }
                if let Err(f) = fetcher.ensure_package(p).wait().await
                    && !optional_only(graph, PackageId(i as u32))
                {
                    return Err(Error::Fetch {
                        package: p.key(),
                        message: f.message,
                        code: f.code,
                    });
                }
            }
            let package_json = root.join("package.json");
            std::fs::write(&package_json, crate::options::DlxMode::UNFINISHED)
                .map_err(Error::io(&package_json))?;
        }
        // Previously blocked scripts need a fresh installed copy even when their
        // package resolution did not change. The script scheduler consumes rebuilt dirs.
        let newly_trusted = !config.ignore_scripts
            && lock.graph.as_ref().is_some_and(|before| {
                let old_policy = TrustPolicy::from_graph(before, std::iter::empty());
                let new_policy = TrustPolicy::from_graph(graph, trust_names.iter().cloned());
                let registry = registry_lookup(config);
                graph.ids().any(|id| {
                    let package = &graph.packages[id.index()];
                    package.meta.needs_scripts()
                        && !package.resolution.is_local_tree()
                        && new_policy.is_trusted_with_registry(graph, id, &registry)
                        && before.ids().any(|old| {
                            before.packages[old.index()].key() == package.key()
                                && !old_policy.is_trusted_with_registry(before, old, &registry)
                        })
                })
            });
        let link_opts = LinkOptions {
            previous_hoisted_workspaces,
            backend: config.backend.unwrap_or_else(fnpm_linker::default_backend),
            force: config.force || newly_trusted || dlx.is_some_and(|d| d.relink),
            fresh: !root.join("node_modules").exists(),
            selected_workspaces: selected.clone(),
            // bun places every package wherever the flags of the install leave them and only
            // skips what they omit: a package `--production`, `--omit` or `--os` leaves out
            // stays in a node_modules that has it, like one that `--filter` does not select.
            hoisted_prune_tree: (kind == NodeLinker::Hoisted
                && (selected.is_some() || omits_packages(config)))
            .then(|| {
                let mut all =
                    fnpm_lockfile::tree::HoistOptions::all(fnpm_types::Platform::current());
                all.self_contained = linker.self_contained_ids(&view);
                // A peer that `--omit=peer` leaves out has no place of its own to keep.
                if config.omit.peer {
                    all.include = all.include.with(fnpm_types::Behavior::PEER, false);
                }
                let full = if omits_packages(config) {
                    let mut tree = fnpm_lockfile::tree::hoist(&view, &all);
                    for (_, ids) in &tree.workspace_roots.clone() {
                        for &id in ids {
                            if let Some(node) = tree.nodes.get_mut(id as usize) {
                                node.parent = None;
                            }
                        }
                    }
                    Layout::Hoisted(tree)
                } else {
                    plan_layout(&linker, &view, kind, None, &pruned)
                };
                match full {
                    Layout::Hoisted(tree) => tree,
                    _ => unreachable!("hoisted layout requested"),
                }
            }),
            global_store: config.global_store,
            check_unpatched: lock
                .graph
                .as_ref()
                .is_some_and(|g| !g.patched_dependencies.is_empty()),
            threads,
            previous: previous.clone(),
        };
        let (f, ev, dir) = (fetcher.clone(), events.clone(), root.clone());
        let result = tokio::task::spawn_blocking(move || {
            let source = f.source(&view);
            linker.link(&dir, &view, &layout, &source, &link_opts, &*ev)
        })
        .await
        .map_err(|e| Error::Io {
            path: root.join("node_modules"),
            source: std::io::Error::other(format!("link task failed: {e}")),
        })?;
        match result {
            Ok(_) if offline_missing => return Err(Error::Reported),
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
    prepared: Option<fnpm_lockfile::PreparedSave>,
    events: &dyn EventSink,
) -> Result<bool> {
    if config.save_package_json && !edits.is_empty() {
        edits.apply(root, ws)?;
    }
    // `--save-text-lockfile` turns bun.lockb into fnpm.lock even when nothing else is saved
    // (bun `converts_binary_lockfile_to_text`).
    let converts =
        lock.format == Some(Format::BunBinary) && config.save_text_lockfile == Some(true);
    let save_lock = converts
        || config.lockfile.save && (dirty || config.lockfile.force_save || !edits.is_empty());
    if !save_lock {
        return Ok(false);
    }
    if config.save_package_json && !lock.write_back.is_empty() {
        for line in lock::write_back(root, ws, &lock.write_back)? {
            events.emit(Event::notice(line));
        }
    }
    // Bun deletes the lockfile when remove leaves only the dependency-free root.
    // Keep this inside the normal save gate so --no-save and frozen installs preserve it.
    let removing = edits
        .by_workspace
        .values()
        .flatten()
        .any(|edit| matches!(edit, Edit::Remove { .. }));
    if removing && graph.len() == 1 && graph.packages[0].dependencies.is_empty() {
        let path = root.join(fnpm_lockfile::FNPM_LOCK);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&path)(e)),
        }
        let _ = std::fs::remove_file(lock::sidecar_path(config, root));
        if !config.global {
            events.emit(Event::status(
                "\npackage.json has no dependencies! Deleted empty lockfile",
            ));
        }
        return Ok(true);
    }
    write_lockfile(root, config, lock, graph, prepared)?;
    events.emit(Event::status("Saved lockfile"));
    if config.lockfile.print_yarn {
        fnpm_lockfile::save_yarn(
            root,
            graph,
            &fnpm_lockfile::print::YarnOptions {
                registry: config.registry.url.clone(),
                ..Default::default()
            },
        )?;
        events.emit(Event::status("Saved yarn.lock"));
    }
    Ok(true)
}

/// The first edge whose name cannot be a folder under `node_modules` (workspace links aside).
fn unsafe_dependency_name(graph: &LockGraph) -> Option<String> {
    for p in &graph.packages {
        for e in &p.dependencies {
            if e.target.is_some()
                && !e.behavior.is_workspace()
                && !fnpm_types::name::alias_is_safe_install_target(&e.name)
            {
                return Some(e.name.clone());
            }
        }
    }
    None
}

/// Registry tasks so far: bun schedules the HTTP request and the manifest parse / tarball
/// extraction separately. A 304 response reuses the parsed manifest.
fn network_tasks(client: &fnpm_registry::RegistryClient) -> u64 {
    let stats = client.stats();
    2 * (stats.manifest_requests + stats.tarball_requests) - stats.not_modified
}

/// bun's `Resolving dependencies` (on stderr, without a progress bar), once, when the install
/// had to ask the network.
fn announce_resolving(
    client: &fnpm_registry::RegistryClient,
    events: &dyn EventSink,
    announced: &mut bool,
) {
    if !*announced && network_tasks(client) > 0 {
        events.emit(Event::status("Resolving dependencies"));
        *announced = true;
    }
}

/// bun's `Resolved, downloaded and extracted [N]`, after the downloads.
fn announce_resolved(
    client: &fnpm_registry::RegistryClient,
    events: &dyn EventSink,
    announced: bool,
) {
    let tasks = network_tasks(client);
    if tasks == 0 {
        return;
    }
    if !announced {
        events.emit(Event::status("Resolving dependencies"));
    }
    events.emit(Event::status(format!(
        "Resolved, downloaded and extracted [{tasks}]"
    )));
}

/// `node_modules/.bun` (next to bun.lock / bun.lockb) and `node_modules/.pnpm` (next to
/// pnpm-lock.yaml) link to `node_modules/.fnpm`, so scripts written for those managers'
/// isolated layouts find packages where they look (entries are named alike:
/// `@scope+name@1.0.0`). Announced when the link is made.
fn layout_aliases(root: &Path, events: &dyn EventSink) {
    let nm = root.join("node_modules");
    let store = nm.join(".fnpm");
    let Ok(target) = std::fs::canonicalize(&store) else {
        return;
    };
    for (manager, alias, lockfiles) in [
        ("bun", ".bun", &["bun.lock", "bun.lockb"][..]),
        ("pnpm", ".pnpm", &["pnpm-lock.yaml"][..]),
    ] {
        if !lockfiles.iter().any(|name| root.join(name).exists()) {
            continue;
        }
        let link = nm.join(alias);
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if std::fs::canonicalize(&link).is_ok_and(|resolved| resolved == target) {
                    continue;
                }
                let _ = std::fs::remove_file(&link).or_else(|_| std::fs::remove_dir(&link));
            }
            // The other manager's own store, stale after this install: aside, like a linker
            // switch, for a later install to delete.
            Ok(_) => {
                let aside = nm.join(format!(
                    "{}{:016x}",
                    fnpm_linker::fsutil::OLD_MODULES_PREFIX,
                    fnpm_linker::fsutil::random_suffix()
                ));
                if std::fs::rename(&link, &aside).is_err() {
                    continue;
                }
            }
            Err(_) => {}
        }
        match fnpm_store::fsops::symlink_dir(&store, &link) {
            // Said when fnpm.lock sits beside the other manager's lockfile; a project that keeps
            // bun.lockb as its lockfile gets the alias silently, as bun has the folder.
            Ok(()) if root.join(fnpm_lockfile::FNPM_LOCK).exists() => {
                events.emit(Event::notice(format!(
                    "node_modules/{alias} links to node_modules/.fnpm for scripts written for {manager}"
                )))
            }
            Ok(()) => {}
            Err(e) => events.emit(Event::warn(
                format!("could not link node_modules/{alias} to node_modules/.fnpm: {e}"),
                None,
            )),
        }
    }
}

/// Step 7: lifecycle scripts of trusted packages, in dependency order.
/// Returns the number of blocked (untrusted) scripts.
async fn run_scripts(
    project: &Project,
    ws: &Workspace,
    graph: &LockGraph,
    linked: &Linked,
    members: Option<&[PackageId]>,
    trust_names: &[String],
    events: &Arc<dyn EventSink>,
) -> Result<(u32, bool)> {
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
            members,
        },
        &locate,
    )
    .map_err(Error::io(&root))?;
    let blocked = plan.blocked_scripts();
    if plan.jobs.is_empty() {
        return Ok((blocked, false));
    }
    let opts = fnpm_scripts::RunOptions {
        concurrency: config.concurrent_scripts,
        shell: None,
        env: Vec::new(),
        silent: config.log_level == fnpm_config::LogLevel::Silent,
        verbose: config.log_level == fnpm_config::LogLevel::Verbose,
        project_dir: root.clone(),
    };
    let cleanup = |failed: &Path| -> std::io::Result<&'static str> {
        let Some(id) = linked
            .report
            .dirs
            .iter()
            .find_map(|(id, dir)| (dir == failed).then_some(*id))
        else {
            // A member reached only through an optional dependency: bun keeps its folder and
            // removes the links to it.
            if ws.members.iter().any(|m| m.dir == failed) {
                let real = std::fs::canonicalize(failed)?;
                remove_optional_links(&root.join("node_modules"), &real)?;
                for member in &ws.members {
                    remove_optional_links(&root.join(&member.path).join("node_modules"), &real)?;
                }
                return Ok(if linked.linker == NodeLinker::Isolated {
                    "skipping"
                } else {
                    "deleting"
                });
            }
            return Ok("deleting");
        };
        // A `link:` package: the folder is not the install's, only the links to it are.
        if matches!(
            graph.get(id).map(|p| &p.resolution),
            Some(Resolution::Symlink { .. })
        ) {
            let real = std::fs::canonicalize(failed)?;
            remove_optional_links(&root.join("node_modules"), &real)?;
            for member in &ws.members {
                remove_optional_links(&root.join(&member.path).join("node_modules"), &real)?;
            }
            return Ok("deleting");
        }
        // Global bin links point through the root alias rather than an isolated store path.
        if config.global {
            for edge in graph
                .root()
                .dependencies
                .iter()
                .filter(|e| e.target == Some(id))
            {
                fnpm_linker::bins::unlink_bins(
                    &root.join("node_modules").join(&edge.name),
                    &config.global_bin_dir,
                )
                .map_err(std::io::Error::other)?;
            }
        }
        // A hoisted package can have several physical copies. Remove every copy and every
        // bin link into it before releasing parent and root lifecycle scripts.
        for (_, dir) in linked
            .report
            .dirs
            .iter()
            .filter(|(package, _)| *package == id)
        {
            if !dir.starts_with(&root) || dir == &root {
                continue;
            }
            if linked.linker == NodeLinker::Isolated {
                let real = std::fs::canonicalize(dir)?;
                remove_optional_links(&root.join("node_modules"), &real)?;
                for member in &ws.members {
                    remove_optional_links(&root.join(&member.path).join("node_modules"), &real)?;
                }
            }
            for ancestor in dir
                .ancestors()
                .filter(|p| p.file_name().is_some_and(|n| n == "node_modules"))
            {
                fnpm_linker::bins::unlink_bins(dir, &ancestor.join(".bin"))
                    .map_err(std::io::Error::other)?;
            }
            if config.global {
                fnpm_linker::bins::unlink_bins(dir, &config.global_bin_dir)
                    .map_err(std::io::Error::other)?;
            }
            match std::fs::remove_dir_all(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok("deleting")
    };
    let report =
        fnpm_scripts::run_dag_with_cleanup(plan.jobs, plan.deps, &opts, &**events, Some(&cleanup))
            .await?;
    Ok((blocked, !report.failed_optional.is_empty()))
}

/// Remove isolated aliases and their bins while the failed package still exists, so realpath
/// can identify aliases in root, workspace, hidden-hoist and local virtual-store directories.
/// Never follow other package links into shared store entries or external workspace folders.
fn remove_optional_links(nm: &Path, failed: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(nm) {
        Ok(meta) if !meta.file_type().is_symlink() && meta.is_dir() => {}
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    let entries = match std::fs::read_dir(nm) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            if std::fs::canonicalize(&path).is_ok_and(|p| p == failed) {
                for ancestor in path
                    .ancestors()
                    .filter(|p| p.file_name().is_some_and(|name| name == "node_modules"))
                {
                    fnpm_linker::bins::unlink_bins(&path, &ancestor.join(".bin"))
                        .map_err(std::io::Error::other)?;
                }
                std::fs::remove_file(&path)?;
            }
        } else if kind.is_dir() {
            let name = entry.file_name();
            if name == ".fnpm" {
                remove_optional_links(&path.join("node_modules"), failed)?;
                for slot in std::fs::read_dir(&path)? {
                    let slot = slot?;
                    if slot.file_type()?.is_dir() && slot.file_name() != "node_modules" {
                        remove_optional_links(&slot.path().join("node_modules"), failed)?;
                    }
                }
            } else if name.to_string_lossy().starts_with('@') {
                remove_optional_links(&path, failed)?;
            } else if !name.to_string_lossy().starts_with('.') {
                remove_optional_links(&path.join("node_modules"), failed)?;
            }
        }
    }
    Ok(())
}

/// Writes the no-op state for the files as they are now.
fn write_state(
    root: &Path,
    meta: &Path,
    ws: &Workspace,
    graph: &LockGraph,
    config_hash: u64,
    linker: &NodeLinker,
    installs: u32,
) {
    let Some(bytes) = fast_path::lockfile_bytes(root) else {
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
        virtual_store: (0, 0, 0),
        linker: match linker {
            NodeLinker::Hoisted => "hoisted".into(),
            NodeLinker::Isolated => "isolated".into(),
        },
        packages: graph.len() as u32,
        installs,
        workspace_patterns,
        workspace_dirs,
    };
    let _ = fast_path::write(root, meta, &state);
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>(_: T) {}
    let _ = |o: InstallOptions, e: Arc<dyn EventSink>| is_send(run(o, e, Env::default()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notices(sink: &fnpm_types::event::VecSink) -> Vec<String> {
        sink.0
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|event| match event {
                Event::Notice { message } => Some(message),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn layout_aliases_follow_the_lockfiles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let nm = root.join("node_modules");
        std::fs::create_dir_all(nm.join(".fnpm/macos-alias@2.2.0/node_modules/macos-alias"))
            .unwrap();
        let sink = fnpm_types::event::VecSink::default();
        std::fs::write(root.join("fnpm.lock"), "{}").unwrap();

        // No other lockfile: no alias.
        layout_aliases(root, &sink);
        assert!(std::fs::symlink_metadata(nm.join(".bun")).is_err());

        // bun.lock: `.bun` shows the store, announced once.
        std::fs::write(root.join("bun.lock"), "{}").unwrap();
        layout_aliases(root, &sink);
        assert_eq!(
            notices(&sink),
            ["node_modules/.bun links to node_modules/.fnpm for scripts written for bun"]
        );
        assert!(
            nm.join(".bun/macos-alias@2.2.0/node_modules/macos-alias")
                .is_dir()
        );
        layout_aliases(root, &sink);
        assert!(notices(&sink).is_empty());

        // bun's own store left behind is moved aside for a later install to delete.
        std::fs::remove_file(nm.join(".bun")).unwrap();
        std::fs::create_dir_all(nm.join(".bun/stale@1.0.0")).unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        layout_aliases(root, &sink);
        assert_eq!(notices(&sink).len(), 2);
        assert!(nm.join(".bun/macos-alias@2.2.0").is_dir());
        assert!(nm.join(".pnpm/macos-alias@2.2.0").is_dir());
        let leftovers: Vec<_> = std::fs::read_dir(&nm)
            .unwrap()
            .flatten()
            .filter(|e| fnpm_linker::fsutil::is_leftover(&e.file_name().to_string_lossy()))
            .collect();
        assert_eq!(leftovers.len(), 1);
        assert!(leftovers[0].path().join("stale@1.0.0").is_dir());
    }
}
