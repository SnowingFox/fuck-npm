//! Step 3 of the pipeline: the previous graph. `fnpm.lock` (through the rkyv sidecar when it
//! is fresh), else `bun.lock` / `bun.lockb`, else the first foreign lockfile that migrates
//! (`npm-shrinkwrap.json`, `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`; a lockfile that
//! fails is reported and the next one is tried), else pnpm-workspace.yaml settings. Migrations
//! carry bun's package.json write-back, applied when the new lockfile is saved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fnpm_config::Config;
use fnpm_lockfile::formats::foreign::{self, DiskFiles, PackageJsonWriteBack};
use fnpm_lockfile::{Format, LoadOptions, ParseOptions};
use fnpm_types::LockGraph;
use fnpm_workspace::{PackageJson, Workspace};

use crate::{Error, Result};

/// What was found.
#[derive(Debug, Default)]
pub(crate) struct LockState {
    /// The previous graph, if any lockfile loaded.
    pub graph: Option<LockGraph>,
    /// Its format.
    pub format: Option<Format>,
    /// The file it came from.
    pub path: Option<PathBuf>,
    /// It did not come from fnpm.lock: the new lockfile must be written.
    pub migrated: bool,
    /// package.json edits that come with the migration.
    pub write_back: Vec<PackageJsonWriteBack>,
    /// Warnings to report.
    pub warnings: Vec<String>,
}

fn parse_options(config: &Config) -> ParseOptions {
    ParseOptions {
        default_registry: Some(config.registry.url.clone()),
        scope_registries: config
            .scopes
            .iter()
            .map(|(k, v)| (k.trim_start_matches('@').to_owned(), v.url.clone()))
            .collect(),
    }
}

fn foreign_options<'a>(config: &Config, files: &'a DiskFiles) -> foreign::MigrateOptions<'a> {
    let mut o = foreign::MigrateOptions::from_process(files);
    o.registry = config.registry.url.clone();
    o.scope_registries = config
        .scopes
        .iter()
        .map(|(k, v)| (k.trim_start_matches('@').to_owned(), v.url.clone()))
        .collect::<BTreeMap<_, _>>();
    o.link_workspace_packages = config.link_workspace_packages;
    o.frozen = config.frozen_lockfile;
    o
}

/// The foreign lockfiles, in bun's order.
const FOREIGN: [&str; 4] = [
    "npm-shrinkwrap.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
];

/// Loads the previous graph for `ws`. A broken fnpm.lock / bun.lock is ignored with a warning
/// (an error with `--frozen-lockfile`).
pub(crate) fn load(config: &Config, ws: &Workspace) -> Result<LockState> {
    let dir = &ws.root_dir;
    let mut state = LockState::default();
    if !config.load_lockfile {
        return Ok(state);
    }
    let own: Vec<PathBuf> = match &config.lockfile.path {
        Some(p) => vec![if p.is_absolute() {
            p.clone()
        } else {
            dir.join(p)
        }],
        None => ["fnpm.lock", "bun.lock", "bun.lockb"]
            .iter()
            .map(|n| dir.join(n))
            .collect(),
    };
    for path in own {
        if !path.exists() {
            continue;
        }
        let opts = LoadOptions {
            lockfile_path: Some(path.clone()),
            parse: parse_options(config),
            no_sidecar: false,
        };
        match fnpm_lockfile::load_with(dir, ws, &opts) {
            Ok(Some(loaded)) => {
                state.warnings.extend(loaded.warnings);
                state.migrated = loaded.format != Format::Fnpm;
                state.format = Some(loaded.format);
                state.path = Some(loaded.path);
                state.graph = Some(loaded.graph);
                return Ok(state);
            }
            Ok(None) => {}
            Err(e) if config.frozen_lockfile => return Err(e.into()),
            Err(e) => {
                state.warnings.push(format!("Ignoring lockfile: {e}"));
                return Ok(state);
            }
        }
    }
    if config.lockfile.path.is_some() {
        return Ok(state);
    }
    let found: Vec<(&str, Vec<u8>)> = FOREIGN
        .iter()
        .filter_map(|name| std::fs::read(dir.join(name)).ok().map(|b| (*name, b)))
        .collect();
    let files = DiskFiles;
    for (i, (name, bytes)) in found.iter().enumerate() {
        let next = found.get(i + 1).map(|f| f.0);
        let attempt = migrate_one(config, ws, &files, name, bytes, next);
        match attempt {
            Ok((mut graph, format, write_back, warnings)) => {
                state.warnings.extend(warnings);
                fnpm_lockfile::migration::finish_migration(&mut graph, ws);
                state.graph = Some(graph);
                state.format = Some(format);
                state.path = Some(dir.join(name));
                state.migrated = true;
                state.write_back = write_back;
                return Ok(state);
            }
            Err(e) if config.frozen_lockfile && next.is_none() => return Err(e.into()),
            Err(e) => {
                let detail = match &e {
                    fnpm_lockfile::Error::Parse { message, .. } => message.clone(),
                    other => other.to_string(),
                };
                state.warnings.push(match next {
                    Some(next) => format!("cannot migrate {name} ({detail}), trying {next}"),
                    None => format!("cannot migrate {name} ({detail})"),
                });
            }
        }
    }
    if found.is_empty() {
        let (edit, warnings) = fnpm_lockfile::formats::pnpm::migrate_workspace_config(
            ws,
            &foreign_options(config, &files),
        );
        state.warnings.extend(warnings);
        state.write_back.extend(edit);
    }
    Ok(state)
}

type Attempt = (LockGraph, Format, Vec<PackageJsonWriteBack>, Vec<String>);

fn migrate_one(
    config: &Config,
    ws: &Workspace,
    files: &DiskFiles,
    name: &str,
    bytes: &[u8],
    next: Option<&str>,
) -> fnpm_lockfile::Result<Attempt> {
    match name {
        "yarn.lock" => {
            let m = fnpm_lockfile::formats::yarn_v1::migrate(
                bytes,
                ws,
                &foreign_options(config, files),
            )?;
            let format = if fnpm_lockfile::migration::is_yarn_berry(bytes) {
                Format::YarnBerry
            } else {
                Format::YarnV1
            };
            Ok((m.graph, format, m.write_back, m.warnings))
        }
        "pnpm-lock.yaml" => {
            let m =
                fnpm_lockfile::formats::pnpm::migrate(bytes, ws, &foreign_options(config, files))?;
            Ok((m.graph, Format::Pnpm, m.write_back, m.warnings))
        }
        _ => {
            let opts = fnpm_lockfile::migration::MigrateOptions::from_config(config);
            let (graph, report) =
                fnpm_lockfile::formats::npm::migrate(bytes, ws, &opts, name, next)?;
            let mut warnings = report.warnings;
            warnings.extend(report.notes);
            Ok((graph, Format::Npm, Vec::new(), warnings))
        }
    }
}

/// Applies write-back edits to the in-memory workspace, so the resolver reads the migrated
/// settings (overrides, catalogs, trusted dependencies) before they are saved.
pub(crate) fn apply_in_memory(ws: &mut Workspace, edits: &[PackageJsonWriteBack]) -> Result<()> {
    for edit in edits {
        let (raw, path) = if edit.path.is_empty() {
            (&mut ws.root, ws.root_dir.join("package.json"))
        } else {
            let Some(m) = ws.members.iter_mut().find(|m| m.path == edit.path) else {
                continue;
            };
            let path = m.dir.join("package.json");
            (&mut m.manifest, path)
        };
        let mut value = raw.raw.clone();
        edit.apply_to(&mut value);
        *raw = PackageJson::from_value(value, &path)?;
    }
    Ok(())
}

/// Writes the write-back edits with the format-preserving editor.
pub(crate) fn write_back(
    root: &Path,
    ws: &Workspace,
    edits: &[PackageJsonWriteBack],
) -> Result<Vec<String>> {
    let mut reports = Vec::new();
    for edit in edits {
        let dir = if edit.path.is_empty() {
            root.to_path_buf()
        } else {
            match ws.members.iter().find(|m| m.path == edit.path) {
                Some(m) => m.dir.clone(),
                None => continue,
            }
        };
        let path = dir.join("package.json");
        let text = std::fs::read_to_string(&path).map_err(Error::io(&path))?;
        let mut editor = fnpm_workspace::PackageJsonEditor::new(text.clone());
        for (key, value) in &edit.fields {
            editor.set_path(key, value.clone());
        }
        let out = editor.finish();
        if out != text {
            crate::project::write_atomic(&path, out.as_bytes())?;
            let rel = if edit.path.is_empty() {
                "package.json".to_owned()
            } else {
                format!("{}/package.json", edit.path)
            };
            reports.push(format!("{} in {rel}", edit.changes.join(", ")));
        }
    }
    Ok(reports)
}
