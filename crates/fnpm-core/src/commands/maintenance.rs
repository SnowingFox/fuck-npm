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

/// `fnpm prune [--check]`: removes what `node_modules` has but the lockfile does not place.
pub(crate) fn prune(cx: &Ctx) -> Result<CoreResult> {
    let (project, _, graph) = cx.load_locked("nothing to prune")?;
    let (_, layout) = current_layout(&project, &graph)?;
    let view = crate::install::link_view(&graph);
    let plan = fnpm_linker::prune::plan(&project.root_dir, &view, &layout);
    let names: Vec<String> = plan.removals.iter().map(|r| r.name.clone()).collect();
    if cx.flag("check") || cx.pm.dry_run {
        let mut lines: Vec<String> = names.iter().map(|n| format!("- {n}")).collect();
        lines.push(if names.is_empty() {
            "node_modules has nothing to prune".into()
        } else {
            format!(
                "{} {} can be pruned",
                names.len(),
                plural(names.len(), "package")
            )
        });
        let code = i32::from(cx.flag("check") && !names.is_empty());
        return Ok(exit(
            code,
            json!({ "removed": names, "checked": plan.checked }),
            lines,
        ));
    }
    let (removed, errors) = fnpm_linker::prune::execute(&project.root_dir, &plan);
    crate::fast_path::remove(&project.root_dir, &project.meta_dir());
    let mut lines: Vec<String> = names.iter().map(|n| format!("- {n}")).collect();
    for (p, e) in &errors {
        lines.push(format!("error: {}: {e}", p.display()));
    }
    lines.push(format!(
        "{removed} {} removed",
        plural(removed as usize, "package")
    ));
    let code = i32::from(!errors.is_empty());
    Ok(exit(
        code,
        json!({ "removed": names, "checked": plan.checked }),
        lines,
    ))
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
    fnpm_lockfile::save_keeping_format(&project.root_dir, &graph, None)?;
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
    for e in &state.failures {
        cx.events.emit(e.clone());
    }
    for w in &state.warnings {
        cx.events.emit(Event::warn(w.clone(), None));
    }
    let Some(graph) = state.graph.as_ref() else {
        return Err(Error::usage("could not find any other lockfile"));
    };
    let source = state
        .path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let count = graph.len().saturating_sub(1);
    // Like bun, `pm migrate` reports the migration on stderr and keeps stdout empty (a dry run
    // prints the file it would write); `fnpm-lock migrate` prints the lockfile and a count.
    let notice = || {
        cx.events
            .emit(Event::notice(format!("migrated lockfile from {source}")))
    };
    if cx.pm.dry_run && !lock_tool {
        notice();
        return Ok(ok(
            json!({ "from": source, "packages": count }),
            vec![format!("dry run: would write {}", fnpm_lockfile::FNPM_LOCK)],
        ));
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
    if !lock_tool {
        notice();
        for line in lines {
            cx.events.emit(Event::notice(line));
        }
        return Ok(ok(json!({ "from": source, "packages": count }), Vec::new()));
    }
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
pub(crate) fn cache(cx: &Ctx, rest: &[&str]) -> Result<CoreResult> {
    let project = cx.project_anywhere()?;
    let config = &project.config;
    let dry = cx.pm.dry_run;
    match rest {
        [] | ["path"] => {
            let s = config.cache_dir.to_string_lossy().into_owned();
            Ok(ok(json!({ "path": s }), vec![s]))
        }
        ["rm" | "clean"] => {
            let exe = std::env::current_exe().ok();
            let home = project.env.get("HOME").map(PathBuf::from);
            let protected = fnpm_store::cache::default_protected(
                home.as_deref(),
                exe.as_deref(),
                Some(&config.home_dir),
                Some(&project.root_dir),
                &cx.req.cwd,
            );
            let mut dirs = vec![config.cache_dir.clone()];
            if cx.pm.global || cx.flag("all") {
                dirs.push(config.store_dir.clone());
            }
            let report = fnpm_store::cache::clear_cache(&fnpm_store::cache::ClearOptions {
                dirs,
                protected,
                dry_run: dry,
                temp_dir: Some(std::env::temp_dir()),
            })?;
            let code = i32::from(!report.errors.is_empty());
            Ok(exit(
                code,
                serde_json::to_value(&report).unwrap_or(Value::Null),
                report.lines(),
            ))
        }
        ["prune"] => {
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
        ["pack"] => {
            let Some(file) = cx.req.positionals.first() else {
                return Err(Error::usage("Missing the archive to write"));
            };
            let dest = cx.req.cwd.join(file);
            let ws = project.workspace(&*cx.events)?;
            let lock = crate::install::load_lock(config, &ws)?;
            let store = project.store()?;
            let keys: Vec<fnpm_store::PackageKey> = match &lock.graph {
                Some(g) => g
                    .packages
                    .iter()
                    .filter_map(crate::fetch::store_key)
                    .collect(),
                None => Vec::new(),
            };
            let summary = fnpm_store::cache_pack::pack(&store, &keys, &dest, dry)?;
            let data = json!({
                "packages": summary.packages,
                "files": summary.files,
                "bytes": summary.bytes,
                "missing": summary.missing,
            });
            Ok(ok(data, summary.lines(file, dry)))
        }
        ["unpack"] => {
            let Some(file) = cx.req.positionals.first() else {
                return Err(Error::usage("Missing the archive to read"));
            };
            let store = project.store()?;
            let summary = fnpm_store::cache_pack::unpack(&store, &cx.req.cwd.join(file), dry)?;
            let data = json!({
                "packages": summary.packages,
                "created": summary.created,
                "alreadyCached": summary.already_cached,
                "incomplete": summary.incomplete,
            });
            Ok(ok(data, summary.lines(dry)))
        }
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

/// `pm fetch`: downloads every package of the lockfile into the store without linking.
pub(crate) async fn fetch(cx: &Ctx) -> Result<CoreResult> {
    let (project, _, graph) = cx.load_locked("nothing to fetch")?;
    let config = project.config.clone();
    let client = project.client()?;
    let fetcher = Fetcher::new(
        project.store()?,
        client.clone(),
        project.root_dir.clone(),
        config.cache_dir.clone(),
        config.verify_integrity,
        cx.events.clone(),
    );
    let mut slots = Vec::new();
    for p in &graph.packages {
        if needs_content(&p.resolution) && !p.is_disabled(&config.platform) {
            slots.push((p.key(), fetcher.ensure_package(p)));
        }
    }
    let mut errors = Vec::new();
    for (key, slot) in &slots {
        if let Err(e) = slot.wait().await {
            errors.push(format!("error: {key}: {e}"));
        }
    }
    client.flush_cache_writes().await;
    let mut lines = errors.clone();
    lines.push(format!(
        "{} {} in the store",
        slots.len() - errors.len(),
        plural(slots.len() - errors.len(), "package")
    ));
    let data = json!({ "packages": slots.len(), "errors": errors });
    Ok(exit(i32::from(!errors.is_empty()), data, lines))
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
