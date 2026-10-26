//! Lockfile and disk maintenance: `prune`, `dedupe`, `migrate` / `fnpm-lock migrate`,
//! `fnpm-lock export`, `pm cache …`, `store …`, `pm fetch` and `doctor`.

use std::path::{Path, PathBuf};

use fnpm_linker::{Layout, Linker};
use fnpm_lockfile::Format;
use fnpm_store::{PruneOptions, Store};
use fnpm_types::{Event, LockGraph};
use serde_json::{Value, json};

use super::{Ctx, exit, ok};
use crate::fetch::{Fetcher, needs_content};
use crate::options::{InstallCommand, InstallOptions};
use crate::{CoreResult, Error, Result};

/// The layout the project's `node_modules` has (or would get).
pub(crate) fn current_layout(
    project: &crate::project::Project,
    graph: &LockGraph,
) -> Result<(Linker, Layout)> {
    let config = &project.config;
    let configured = config
        .linker
        .fixed()
        .or(graph.node_linker)
        .unwrap_or_else(|| {
            fnpm_linker::choose_linker(
                None,
                graph.config_version,
                graph.has_workspaces(),
                false,
                &project.root_dir,
            )
        });
    let kind = fnpm_linker::prune::detect_layout(&project.root_dir, graph, configured);
    let linker = Linker::new(config.clone(), project.store()?);
    let view = crate::install::link_view(graph);
    let layout = crate::install::plan_layout(&linker, &view, kind, None);
    Ok((linker, layout))
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

/// Runs a normal install with the request's flags (after a command rewrote the lockfile or
/// package.json).
pub(crate) async fn reinstall(cx: &Ctx) -> Result<fnpm_types::InstallSummary> {
    let opts = InstallOptions {
        pm: cx.pm.clone(),
        command: InstallCommand::Install,
        ..InstallOptions::default()
    };
    crate::install::run(opts, cx.events.clone(), cx.env.clone()).await
}

/// `fnpm dedupe [--check]` / `fnpm-lock dedupe [--check] [--dry-run]`.
pub(crate) async fn dedupe(cx: &Ctx, lock_tool: bool) -> Result<CoreResult> {
    let (project, _, mut graph) = cx.load_locked("nothing to dedupe")?;
    let report = fnpm_resolver::dedupe(&mut graph);
    let mut lines = Vec::new();
    for row in &report.rows {
        let to = if row.to.is_empty() {
            "(removed)".to_owned()
        } else {
            row.to.join(", ")
        };
        let down = if row.downgrade { " (downgrade)" } else { "" };
        lines.push(format!("{} {} -> {to}{down}", row.name, row.from));
    }
    lines.extend(report.kept.iter().cloned());
    let rows: Vec<Value> = report
        .rows
        .iter()
        .map(|r| json!({ "name": r.name, "from": r.from, "to": r.to, "downgrade": r.downgrade }))
        .collect();
    let data = json!({ "removed": rows, "checked": report.checked });
    let check = cx.flag("check");
    if report.rows.is_empty() {
        lines.push("No duplicate versions to remove".into());
        return Ok(ok(data, lines));
    }
    lines.push(format!(
        "{} duplicate {}",
        report.rows.len(),
        plural(report.rows.len(), "version")
    ));
    if check || cx.pm.dry_run {
        return Ok(exit(i32::from(check), data, lines));
    }
    fnpm_lockfile::save_with(&project.root_dir, &graph, None)?;
    if !lock_tool {
        reinstall(cx).await?;
    }
    Ok(ok(data, lines))
}

/// The files `--from <format>` selects, in bun's order.
fn migration_sources(from: Option<&str>) -> Result<Vec<&'static str>> {
    Ok(match from.map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("auto") => vec![
            "bun.lock",
            "bun.lockb",
            "npm-shrinkwrap.json",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
        ],
        Some("bun") => vec!["bun.lock", "bun.lockb"],
        Some("npm") => vec!["npm-shrinkwrap.json", "package-lock.json"],
        Some("yarn") => vec!["yarn.lock"],
        Some("pnpm") => vec!["pnpm-lock.yaml"],
        Some(other) => {
            return Err(Error::usage(format!(
                "unknown lockfile format \"{other}\" (expected bun, npm, yarn or pnpm)"
            )));
        }
    })
}

/// `fnpm migrate` / `pm migrate` / `fnpm-lock migrate [--from x] [--dry-run] [--force]`.
pub(crate) async fn migrate(cx: &Ctx, lock_tool: bool) -> Result<CoreResult> {
    let project = cx.project()?;
    let ws = project.workspace(&*cx.events)?;
    let root = project.root_dir.clone();
    let target = root.join(fnpm_lockfile::FNPM_LOCK);
    if target.exists() && !cx.pm.force && !cx.pm.dry_run {
        return Err(Error::usage(
            "fnpm.lock already exists (pass --force to overwrite it)",
        ));
    }
    let from = cx.string("from");
    let sources = migration_sources(from.as_deref())?;
    let mut state = crate::install::migrate_lock(&project.config, &ws, &sources)?;
    if state.migrated {
        let client = project.client()?;
        crate::install::backfill_migrated(&mut state, &client, &project.config).await;
    }
    for w in &state.warnings {
        cx.events.emit(Event::warn(w.clone(), None));
    }
    let Some(graph) = state.graph.as_ref() else {
        return Err(Error::usage(format!(
            "no lockfile to migrate from in {}",
            root.display()
        )));
    };
    let source = state
        .path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let count = graph.len().saturating_sub(1);
    if !lock_tool {
        // bun `pm migrate`: the migration is reported on stderr, stdout only says what a dry
        // run would write.
        let quiet = cx.pm.log_level == fnpm_config::LogLevel::Silent;
        if !quiet {
            eprintln!("migrated lockfile from {source}");
        }
        let data = json!({ "from": source, "packages": count });
        if cx.pm.dry_run {
            let lines = if quiet {
                Vec::new()
            } else {
                vec![format!("dry run: would write {}", fnpm_lockfile::FNPM_LOCK)]
            };
            return Ok(ok(data, lines));
        }
        for line in crate::install::write_migration(&root, &ws, &state)? {
            eprintln!("{line}");
        }
        fnpm_lockfile::save_with(&root, graph, None)?;
        return Ok(ok(data, Vec::new()));
    }
    if cx.pm.dry_run {
        let text =
            String::from_utf8(fnpm_lockfile::serialize(Format::Fnpm, graph)?).unwrap_or_default();
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.push(format!(
            "dry run: would migrate {count} packages from {source}"
        ));
        return Ok(ok(
            json!({ "from": source, "packages": count, "lockfile": text }),
            lines,
        ));
    }
    let mut lines = crate::install::write_migration(&root, &ws, &state)?;
    fnpm_lockfile::save_with(&root, graph, None)?;
    lines.push(format!(
        "migrated lockfile from {source} ({count} {})",
        plural(count, "package")
    ));
    Ok(ok(json!({ "from": source, "packages": count }), lines))
}

fn export_format(to: &str) -> Result<Format> {
    Ok(match to.to_ascii_lowercase().as_str() {
        "npm" | "package-lock" | "package-lock.json" => Format::Npm,
        "yarn" | "yarn.lock" => Format::YarnV1,
        "pnpm" | "pnpm-lock.yaml" => Format::Pnpm,
        "bun" | "bun.lock" => Format::BunText,
        "fnpm" | "fnpm.lock" => Format::Fnpm,
        other => {
            return Err(Error::usage(format!(
                "unknown lockfile format \"{other}\" (expected npm, yarn, pnpm or bun)"
            )));
        }
    })
}

/// `fnpm-lock export --to <format> [-o path] [--dry-run]`.
pub(crate) fn export(cx: &Ctx) -> Result<CoreResult> {
    let Some(to) = cx.string("to") else {
        return Err(Error::usage(
            "Missing --to <format> (npm, yarn, pnpm or bun)",
        ));
    };
    let format = export_format(&to)?;
    let (project, _, graph) = cx.load_locked("nothing to export")?;
    let bytes = fnpm_lockfile::serialize(format, &graph)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if cx.pm.dry_run {
        let lines = text.lines().map(str::to_owned).collect();
        return Ok(ok(json!({ "format": to, "text": text }), lines));
    }
    let out = match cx.string("output") {
        Some(o) => cx.req.cwd.join(o),
        None => project.root_dir.join(format.file_name()),
    };
    crate::project::write_atomic(&out, &bytes)?;
    Ok(ok(
        json!({ "format": to, "path": out }),
        vec![format!("Wrote {}", out.display())],
    ))
}

/// `pm cache [path] | rm | prune [--max-age d] | pack <file> | unpack <file>`.
/// `pm cache rm` and `pm cache prune` act on the whole cache, so a word after them is an error
/// (otherwise `rm <package>` would clear everything).
fn reject_extra_argument(cx: &Ctx) -> Result<()> {
    if let Some(word) = cx.req.positionals.first() {
        eprintln!(
            "error: unexpected argument \"{}\"\nusage: fnpm pm cache [rm | prune | pack <file> | unpack <file>]",
            fnpm_types::text::escape_control_chars(word)
        );
        return Err(Error::Reported);
    }
    Ok(())
}

/// The cache `pm cache rm` deletes (bun `fetch_cache_directory_path` with the process
/// environment): a project's `.env`, bunfig.toml, fnpm.toml or `.npmrc` does not choose it.
/// `--cache-dir` and the user's own config files (`~/.bunfig.toml`, `~/.fnpm/fnpm.toml`) do;
/// `~/.npmrc` does not, its `cache` is npm's.
fn cache_to_delete(cx: &Ctx) -> PathBuf {
    let env = &cx.env;
    let home = env.home();
    let configured = cx.pm.cache_dir.clone().or_else(|| {
        let fnpm_home = match env.get_not_empty("FNPM_HOME") {
            Some(v) => PathBuf::from(v),
            None => home.clone().unwrap_or_default().join(".fnpm"),
        };
        let mut dir = None;
        for file in fnpm_config::files::global_config_candidates(
            home.as_deref(),
            env.xdg_config_home().as_deref(),
            &fnpm_home,
        ) {
            if let Ok(Some(install)) = fnpm_config::files::load_install_table(&file)
                && let Some(d) = install.cache_dir
            {
                dir = Some(d);
            }
        }
        dir
    });
    fnpm_store::cache::fetch_cache_directory_path(
        &|key: &str| env.get(key).map(str::to_owned),
        configured.as_deref(),
        &cx.req.cwd,
    )
    .path
}

/// What `pm cache rm` and `pm cache prune` must not delete (bun `protected_in`).
fn cache_protected(cx: &Ctx, project: &crate::project::Project) -> Vec<(String, PathBuf)> {
    let env = &cx.env;
    let exe = env
        .get_not_empty("FNPM_LAUNCHER")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok());
    let mut protected = Vec::new();
    for (what, path) in [
        (
            "the home directory",
            env.get_not_empty("HOME").map(PathBuf::from),
        ),
        ("the fnpm executable", exe),
        (
            "$FNPM_HOME",
            env.get_not_empty("FNPM_HOME").map(PathBuf::from),
        ),
        (
            "$BUN_INSTALL",
            env.get_not_empty("BUN_INSTALL").map(PathBuf::from),
        ),
        ("the project directory", Some(project.root_dir.clone())),
        ("the current directory", Some(cx.req.cwd.clone())),
    ] {
        if let Some(path) = path {
            protected.push((what.to_owned(), path));
        }
    }
    protected
}

/// `refusing to <verb> "<dir>": is or contains <what>` and bun's note.
fn refuse_cache(verb: &str, dir: &Path, what: &str) {
    eprintln!(
        "error: refusing to {verb} \"{}\": is or contains {what}",
        dir.display()
    );
    eprintln!(
        "note: cache directory comes from $FNPM_CACHE_DIR, --cache-dir, ~/.bunfig.toml or $BUN_INSTALL. point it at a directory used only for the fnpm install cache"
    );
}

/// `fnpm pm cache rm [--dry-run]` (bun `PackageManagerCommand`): empties the install cache,
/// unless it is or contains something that must not go, then the `fnpm x` trees in the temp
/// directory.
fn cache_rm(cx: &Ctx, project: &crate::project::Project) -> Result<CoreResult> {
    reject_extra_argument(cx)?;
    let dry_run = cx.pm.dry_run;
    let dir = cache_to_delete(cx);
    // No fallback here: the directory that could not be opened is reported.
    if !fnpm_config::path_fits(&dir) {
        eprintln!("ENAMETOOLONG getting cache directory");
        return Err(Error::Reported);
    }
    let exists = dir.is_dir();
    let rm_path = if exists {
        std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone())
    } else {
        dir.clone()
    };
    let protected = cache_protected(cx, project);
    let refused = exists
        .then(|| fnpm_store::cache::protected_in(&rm_path, &protected))
        .flatten();
    let mut lines = Vec::new();
    let mut had_err = false;
    // What `fnpm x` installed lives in the install cache too: counted before it goes.
    let mut deleted = match refused {
        None => fnpm_store::dlx::count_cached_packages(&rm_path),
        Some(_) => 0,
    };
    if let Some(what) = refused {
        refuse_cache("clear", &rm_path, what);
        had_err = true;
    } else {
        let has_global_store = exists && rm_path.join("links").is_dir();
        if dry_run {
            lines.push(format!(
                "dry run: would delete 'fnpm install' cache at {}",
                rm_path.display()
            ));
        } else if exists && let Err(e) = fnpm_store::cache::delete_children(&rm_path) {
            eprintln!("error: Could not delete {}: {e}", rm_path.display());
            had_err = true;
        } else {
            lines.push("Cleared 'fnpm install' cache".to_owned());
        }
        if has_global_store {
            eprintln!(
                "note: global store {} removed too, run 'fnpm install' in each project that uses it",
                if dry_run { "would be" } else { "was" }
            );
        }
        // fnpm extra: `-g` / `--all` also empty the package store.
        if !dry_run && (cx.pm.global || cx.flag("all")) {
            let store = &project.config.store_dir;
            if store.is_dir() && fnpm_store::cache::protected_in(store, &protected).is_none() {
                let _ = fnpm_store::cache::delete_children(store);
            }
        }
    }
    let tmp = std::env::temp_dir();
    let root = fnpm_store::dlx::cache_root_name();
    let prefix = format!("{}-", &root[1..]);
    let mut names: Vec<String> = std::fs::read_dir(&tmp)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    for name in names {
        let is_root = name == root;
        if !is_root && !name.starts_with(&prefix) {
            continue;
        }
        let packages = if is_root {
            fnpm_store::dlx::count_cached_packages(&tmp)
        } else {
            1
        };
        let path = tmp.join(&name);
        if dry_run {
            lines.push(format!(
                "dry run: would delete {}",
                tmp.join(fnpm_types::text::escape_control_chars(&name))
                    .display()
            ));
        } else if let Err(e) =
            std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path))
        {
            eprintln!("error: Could not delete {name}: {e}");
            had_err = true;
            continue;
        }
        deleted += packages;
    }
    lines.push(format!(
        "{} {deleted} cached 'fnpmx' packages",
        if dry_run { "Would clear" } else { "Cleared" }
    ));
    Ok(exit(
        i32::from(had_err),
        json!({ "path": rm_path, "dryRun": dry_run, "fnpmx": deleted }),
        lines,
    ))
}

/// `fnpm pm cache pack <file>` / `unpack <file>` (bun `cache_pack::exec`).
fn cache_pack(cx: &Ctx, project: &crate::project::Project, verb: &str) -> Result<CoreResult> {
    let file = match cx.req.positionals.as_slice() {
        [file] if !file.is_empty() => file.clone(),
        _ => {
            eprintln!("error: expected exactly one <file>\nusage: fnpm pm cache {verb} <file>");
            return Err(Error::Reported);
        }
    };
    let config = &project.config;
    let quiet = cx.pm.log_level == fnpm_config::LogLevel::Silent;
    let dry = cx.pm.dry_run || config.dry_run;
    let started = std::time::Instant::now();
    let elapsed = || format!("[{:.1}ms]", started.elapsed().as_secs_f64() * 1000.0);
    let failed = |e: Error| {
        eprintln!("error: fnpm pm cache {verb} failed: {e}");
        Error::Reported
    };
    let store = project.store()?;
    let path = cx.req.cwd.join(&file);
    if verb == "pack" {
        let ws = project.workspace(&*cx.events)?;
        let lock = crate::install::load_lock(config, &ws)?;
        let Some(graph) = &lock.graph else {
            eprintln!("error: missing lockfile, nothing to pack");
            eprintln!("note: run 'fnpm install' first");
            return Err(Error::Reported);
        };
        let keys: Vec<fnpm_store::PackageKey> = graph
            .packages
            .iter()
            .filter_map(crate::fetch::store_key)
            .collect();
        let summary = fnpm_store::cache_pack::pack(&store, &keys, &path, dry)
            .map_err(|e| failed(e.into()))?;
        let mut lines = summary.lines(&file, dry);
        if summary.missing > 0 && lines.len() > 1 {
            eprintln!("{}", lines.remove(1));
        }
        if let Some(first) = lines.first_mut() {
            first.push(' ');
            first.push_str(&elapsed());
        }
        let data = json!({
            "packages": summary.packages,
            "files": summary.files,
            "bytes": summary.bytes,
            "missing": summary.missing,
        });
        return Ok(ok(data, if quiet { Vec::new() } else { lines }));
    }
    let summary =
        fnpm_store::cache_pack::unpack(&store, &path, dry).map_err(|e| failed(e.into()))?;
    let mut lines = summary.lines(dry);
    if summary.incomplete > 0 && lines.len() > 1 {
        eprintln!("{}", lines.remove(1));
    }
    if let Some(first) = lines.first_mut() {
        first.push(' ');
        first.push_str(&elapsed());
    }
    let data = json!({
        "packages": summary.packages,
        "created": summary.created,
        "alreadyCached": summary.already_cached,
        "incomplete": summary.incomplete,
    });
    Ok(ok(data, if quiet { Vec::new() } else { lines }))
}

pub(crate) fn cache(cx: &Ctx, rest: &[&str]) -> Result<CoreResult> {
    let project = cx.project_anywhere()?;
    let config = &project.config;
    let dry = cx.pm.dry_run;
    match rest {
        [] | ["path"] => {
            // The real path, like bun (`get_fd_path` of the opened directory).
            let dir = std::fs::canonicalize(&config.cache_dir)
                .unwrap_or_else(|_| config.cache_dir.clone());
            let s = dir.to_string_lossy().into_owned();
            Ok(ok(json!({ "path": s }), vec![s]))
        }
        ["rm"] => cache_rm(cx, &project),
        ["prune"] => {
            reject_extra_argument(cx)?;
            let dir = cache_to_delete(cx);
            if dir.is_dir() {
                let real = std::fs::canonicalize(&dir).unwrap_or(dir);
                if let Some(what) =
                    fnpm_store::cache::protected_in(&real, &cache_protected(cx, &project))
                {
                    refuse_cache("prune", &real, what);
                    return Err(Error::Reported);
                }
            }
            let days = cx.number("maxAge").unwrap_or(30) as u32;
            let store = project.store()?;
            let report = store.prune_older_than(days, dry)?;
            let data = json!({
                "checked": report.checked,
                "removed": report.removed,
                "bytes": report.bytes,
                "errors": report.errors,
            });
            Ok(ok(data, report.lines()))
        }
        [verb @ ("pack" | "unpack")] => cache_pack(cx, &project, verb),
        other => Err(Error::UnknownCommand(
            std::iter::once("cache".to_owned())
                .chain(other.iter().map(|s| (*s).to_owned()))
                .collect(),
        )),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `store path | status | prune [--dry-run]`.
pub(crate) fn store(cx: &Ctx, rest: &[&str]) -> Result<CoreResult> {
    let project = cx.project_anywhere()?;
    let store_dir = project.config.store_dir.clone();
    match rest {
        [] | ["path"] => {
            let s = store_dir.to_string_lossy().into_owned();
            Ok(ok(json!({ "path": s }), vec![s]))
        }
        ["status"] => {
            let store = Store::open(&store_dir)?;
            let st = store.status()?;
            let lines = vec![
                format!("store: {}", st.path.display()),
                format!("packages: {}", st.packages),
                format!("files: {}", st.files),
                format!("global links: {}", st.links),
                format!("size on disk: {}", human_bytes(st.bytes)),
            ];
            Ok(ok(serde_json::to_value(&st).unwrap_or(Value::Null), lines))
        }
        ["prune"] => {
            let store = Store::open(&store_dir)?;
            let r = store.prune(PruneOptions {
                dry_run: cx.pm.dry_run,
            })?;
            let verb = if cx.pm.dry_run {
                "would remove"
            } else {
                "removed"
            };
            let lines = vec![format!(
                "{verb} {} packages, {} global links and {} files ({})",
                r.removed_packages,
                r.removed_links,
                r.removed_files,
                human_bytes(r.bytes_freed)
            )];
            Ok(ok(serde_json::to_value(&r).unwrap_or(Value::Null), lines))
        }
        other => Err(Error::UnknownCommand(
            std::iter::once("store".to_owned())
                .chain(other.iter().map(|s| (*s).to_owned()))
                .collect(),
        )),
    }
}

/// Resolves like `fnpm install` (frozen with `--frozen-lockfile`), writing nothing.
async fn resolve_for_fetch(
    cx: &Ctx,
    config: &fnpm_config::Config,
    client: &fnpm_registry::RegistryClient,
    ws: &fnpm_workspace::Workspace,
    previous: Option<&LockGraph>,
) -> std::result::Result<LockGraph, fnpm_resolver::Error> {
    let mode = if config.frozen_lockfile {
        fnpm_resolver::Mode::Frozen
    } else {
        fnpm_resolver::Mode::Normal
    };
    let config = std::sync::Arc::new(config.clone());
    let out = fnpm_resolver::Resolver::new(config, client.clone())
        .resolve(
            fnpm_resolver::ResolveInput {
                workspace: ws,
                previous,
                mode,
                requests: Vec::new(),
                removals: Vec::new(),
            },
            None,
            &*cx.events,
        )
        .await?;
    Ok(out.graph)
}

/// `fnpm pm fetch` (bun `pm_fetch_command.rs` and `PopulatePackageCache.rs`): resolves like
/// `fnpm install` (with the lockfile, or without one) and downloads every package into the
/// store, without linking, running scripts or writing fnpm.lock or package.json.
pub(crate) async fn fetch(cx: &Ctx) -> Result<CoreResult> {
    let start = std::time::Instant::now();
    let started = std::time::SystemTime::now();
    let loud = cx.pm.log_level != fnpm_config::LogLevel::Silent;
    let summary = loud && cx.pm.summary;
    let say = |line: &str| {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    if summary {
        say(&format!(
            "fnpm pm fetch v{}\n",
            cx.string("cliVersion")
                .unwrap_or_else(|| crate::version().to_owned())
        ));
    }
    let project = cx.project()?;
    let config = project.config.clone();
    let dry_run = cx.pm.dry_run || config.dry_run;
    let ws = super::prune::workspace_on_disk(&project, &*cx.events)?;
    let lock = crate::install::load_lock(&config, &ws)?;
    // A pruned checkout fetches what fnpm.lock pins for its missing workspaces too.
    let mut pinned = None;
    if let Some(graph) = &lock.graph {
        if let Some(message) = super::prune::missing_workspace_dependency(graph, &project.root_dir)
        {
            if loud {
                eprintln!("error: {message}");
            }
            return Err(Error::Reported);
        }
        if let Some(note) = super::prune::pruned_note(graph, &project.root_dir) {
            if loud {
                eprintln!("{note}");
            }
            pinned = Some(graph.clone());
        }
    }
    let client = project.client()?;
    let resolved = match pinned {
        Some(graph) => Ok(graph),
        None => resolve_for_fetch(cx, &config, &client, &ws, lock.graph.as_ref()).await,
    };
    let graph = match resolved {
        Ok(graph) => graph,
        Err(e) => {
            if loud {
                eprintln!("error: {e}");
            }
            return Err(Error::Reported);
        }
    };
    let store = project.store()?;
    let candidates: Vec<&fnpm_types::Package> = graph
        .packages
        .iter()
        .skip(1)
        .filter(|p| {
            needs_content(&p.resolution)
                && !matches!(p.resolution, fnpm_types::Resolution::Folder { .. })
                && !p.is_disabled(&config.platform)
        })
        .collect();
    // A store entry made before this run was already in the cache. One the resolve itself
    // made (a git or GitHub package without a lockfile) was fetched by this run, as in bun.
    let made_before = |key: &fnpm_store::PackageKey| {
        std::fs::metadata(store.package_dir(key))
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < started)
    };
    let (mut fetched, mut already_cached) = (0usize, 0usize);
    let mut failed = false;
    if dry_run {
        // Nothing is downloaded: only what the lockfile pins can be found in the store.
        for p in &candidates {
            match crate::fetch::store_key(p) {
                Some(key) if store.get(&key).ok().flatten().is_some() => already_cached += 1,
                _ => fetched += 1,
            }
        }
    } else {
        let fetcher = Fetcher::new(
            store.clone(),
            client.clone(),
            project.root_dir.clone(),
            config.cache_dir.clone(),
            config.verify_integrity,
            cx.events.clone(),
        );
        let slots: Vec<_> = candidates
            .iter()
            .map(|p| (p.key(), fetcher.ensure_package(p)))
            .collect();
        let mut errors = Vec::new();
        let mut offline_missing = Vec::new();
        for ((key, slot), p) in slots.iter().zip(&candidates) {
            match slot.wait().await {
                Ok(f) if made_before(&f.key) => already_cached += 1,
                Ok(_) => fetched += 1,
                Err(e) if e.code == "OFFLINE" => offline_missing.push(p.name.clone()),
                Err(e) => {
                    // bun names the request that failed: `GET <url> - 404`.
                    let message = match e
                        .message
                        .strip_prefix("package \"")
                        .and_then(|m| m.strip_suffix("\" not found"))
                    {
                        Some(url) if url.contains("://") => format!("GET {url} - 404"),
                        _ => format!("{key}: {e}"),
                    };
                    errors.push(message);
                }
            }
        }
        if loud && (fetched > 0 || !errors.is_empty()) {
            eprintln!("Fetching packages");
        }
        if !offline_missing.is_empty() {
            if loud {
                for name in &offline_missing {
                    eprintln!("error: --offline: \"{name}\" is not in the cache");
                }
            }
            return Err(Error::Reported);
        }
        for message in &errors {
            failed = true;
            if loud {
                eprintln!("error: {message}");
            }
        }
    }
    client.flush_cache_writes().await;
    if failed {
        return Err(Error::Reported);
    }
    if config.security_scanner.is_some() && cx.pm.log_level == fnpm_config::LogLevel::Verbose {
        eprintln!("note: packages are not scanned until fnpm install runs the security scanner");
    }
    let data = json!({ "fetched": fetched, "alreadyCached": already_cached });
    if !summary {
        return Ok(ok(data, Vec::new()));
    }
    let elapsed = format!("[{:.2}ms]", start.elapsed().as_secs_f64() * 1000.0);
    let s = |n: usize| if n == 1 { "" } else { "s" };
    if fetched > 0 {
        say(&format!(
            "{} {fetched} package{} into cache {elapsed}",
            if dry_run { "Would fetch" } else { "Fetched" },
            s(fetched)
        ));
    } else if already_cached > 0 {
        say(&format!(
            "Done! {already_cached} package{} already in cache {elapsed}",
            s(already_cached)
        ));
    } else {
        say(&format!("Done! No packages to fetch {elapsed}"));
    }
    say(&format!("Cache: {}", config.cache_dir.display()));
    Ok(ok(data, Vec::new()))
}

#[cfg(unix)]
fn dev_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut cur = Some(p);
    while let Some(c) = cur {
        if let Ok(m) = std::fs::metadata(c) {
            return Some(m.dev());
        }
        cur = c.parent();
    }
    None
}

#[cfg(not(unix))]
fn dev_of(_: &Path) -> Option<u64> {
    None
}

/// `fnpm doctor [--skip-network]`.
pub(crate) async fn doctor(cx: &Ctx) -> Result<CoreResult> {
    let project = cx.project_anywhere()?;
    let config = project.config.clone();
    let mut checks: Vec<Value> = Vec::new();
    let mut lines = vec![format!("fnpm {}", crate::version())];
    let mut failed = false;
    fn check(
        out: (&mut Vec<String>, &mut Vec<Value>, &mut bool),
        name: &str,
        okay: bool,
        detail: String,
    ) {
        out.0.push(format!(
            "{} {name}: {detail}",
            if okay { "ok  " } else { "FAIL" }
        ));
        out.1
            .push(json!({ "name": name, "ok": okay, "detail": detail }));
        *out.2 |= !okay;
    }
    check(
        (&mut lines, &mut checks, &mut failed),
        "project",
        true,
        project.root_dir.display().to_string(),
    );
    let store_ok = Store::open(&config.store_dir).is_ok();
    check(
        (&mut lines, &mut checks, &mut failed),
        "store",
        store_ok,
        config.store_dir.display().to_string(),
    );
    let same_fs = matches!((dev_of(&config.store_dir), dev_of(&project.root_dir)), (Some(a), Some(b)) if a == b);
    let backend = config.backend.unwrap_or_else(fnpm_linker::default_backend);
    lines.push(format!(
        "{} filesystem: {}",
        if same_fs { "ok  " } else { "warn" },
        if same_fs {
            format!("store and project share a filesystem ({backend:?})")
        } else {
            "store and project are on different filesystems: files are copied, not linked".into()
        }
    ));
    checks.push(json!({ "name": "filesystem", "ok": same_fs, "backend": format!("{backend:?}").to_lowercase() }));
    check(
        (&mut lines, &mut checks, &mut failed),
        "cache",
        true,
        config.cache_dir.display().to_string(),
    );
    if !cx.flag("skipNetwork") {
        let client = project.client()?;
        let url = format!("{}-/ping", config.registry.url);
        let reach = client.request_json("GET", &url, None, None).await;
        check(
            (&mut lines, &mut checks, &mut failed),
            "registry",
            reach.is_ok(),
            match &reach {
                Ok(_) => config.registry.url.clone(),
                Err(e) => format!("{}: {e}", config.registry.url),
            },
        );
    }
    for w in &config.warnings {
        lines.push(format!("warn config: {w}"));
    }
    let data = json!({ "version": crate::version(), "checks": checks });
    Ok(exit(i32::from(failed), data, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(export_format("npm").unwrap(), Format::Npm);
        assert_eq!(export_format("bun").unwrap(), Format::BunText);
        assert!(export_format("cargo").is_err());
        assert_eq!(migration_sources(Some("yarn")).unwrap(), vec!["yarn.lock"]);
        assert!(migration_sources(Some("cargo")).is_err());
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(plural(1, "package"), "package");
        assert_eq!(plural(2, "package"), "packages");
    }
}
