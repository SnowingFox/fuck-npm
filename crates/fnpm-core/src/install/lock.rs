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
    /// bun's report of lockfiles that did not migrate, printed before the warnings.
    pub failures: Vec<fnpm_types::Event>,
    /// xxh3 of fnpm.lock when the graph came from it.
    pub hash: Option<u64>,
    /// fnpm.lock has no `configVersion` (an older writer): it is saved again with one.
    pub config_version_missing: bool,
    /// The sidecar ([`sidecar_path`]) was not fresh, so it should be written from
    /// [`LockState::graph`] after the install.
    pub sidecar_missing: bool,
}

/// The workspace's warnings that are not the migration's to give. A pnpm migration reads
/// pnpm-workspace.yaml again and reports its skipped entries as bun counts them (scalars are
/// copied as written), so the workspace loader's count of them is left out.
pub(crate) fn workspace_warnings<'a>(
    warnings: &'a [String],
    lock: &LockState,
) -> impl Iterator<Item = &'a String> {
    let skipped = |w: &str| w.starts_with("skipping non-string entries of ");
    let rereads = (lock.migrated && lock.format == Some(Format::Pnpm))
        || lock.warnings.iter().any(|w| skipped(w));
    warnings.iter().filter(move |w| !(rereads && skipped(w)))
}

/// The project keeps bun.lockb (bun `save_format`): `--save-text-lockfile` writes fnpm.lock, a
/// loaded bun.lockb stays binary, and a new or migrated lockfile is binary only with
/// `saveTextLockfile = false`.
pub(crate) fn saves_binary(config: &Config, lock: &LockState) -> bool {
    match config.save_text_lockfile {
        Some(true) => false,
        _ if lock.graph.is_some() && !lock.migrated => lock.format == Some(Format::BunBinary),
        Some(false) => true,
        None => false,
    }
}

/// The lockfile an install writes ([`saves_binary`]).
pub(crate) fn target_name(config: &Config, lock: &LockState) -> &'static str {
    if saves_binary(config, lock) {
        fnpm_lockfile::BUN_LOCKB
    } else {
        fnpm_lockfile::FNPM_LOCK
    }
}

/// The parsed-lockfile sidecar in the project's cache metadata directory, validated by the
/// lockfile hash. With the default cache, it survives `rm -rf node_modules` (`fnpm ci`).
pub(crate) fn sidecar_path(config: &Config, root: &Path) -> PathBuf {
    crate::fast_path::meta_dir(&config.cache_dir, root).join("lock.bin")
}

/// Writes the sidecar for the loaded graph. Best effort: it is a cache.
pub(crate) fn save_sidecar(config: &Config, root: &Path, hash: u64, graph: &LockGraph) {
    let _ = fnpm_lockfile::sidecar::save_at(&sidecar_path(config, root), hash, graph);
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

/// Why a foreign lockfile did not migrate, in bun's terms.
pub(crate) struct Failure {
    /// bun's error name (`InvalidNPMLockfile`, `InvalidLockfile`, ...).
    tag: &'static str,
    /// The reason bun logs before the name, if it logs one.
    reason: Option<String>,
    /// An unsupported `lockfileVersion`: the sentence and the command that upgrades the file.
    unsupported: Option<(String, &'static str)>,
}

impl Failure {
    pub(crate) fn of(name: &str, bytes: &[u8], e: &fnpm_lockfile::Error) -> Self {
        let message = match e {
            fnpm_lockfile::Error::Parse { message, .. } => message.clone(),
            other => other.to_string(),
        };
        let text = String::from_utf8_lossy(bytes);
        let tag = match name {
            "pnpm-lock.yaml" => "InvalidLockfile",
            "yarn.lock" if text.contains("__metadata") => "InvalidYarnBerryLockfile",
            "yarn.lock" if !text.contains("# yarn lockfile v1") => "UnsupportedYarnLockfileVersion",
            "yarn.lock" => "InvalidYarnLockfile",
            _ => "InvalidNPMLockfile",
        };
        let upgrade = if name == "pnpm-lock.yaml" {
            "pnpm install --lockfile-only"
        } else {
            "npm install --package-lock-only --lockfile-version=3"
        };
        let unsupported = message.contains("unsupported lockfileVersion").then(|| {
            let sentence = message.split(" (run `").next().unwrap_or(&message);
            (sentence.to_owned(), upgrade)
        });
        // bun names the npm and yarn v1 parsers' failures without a reason.
        let reason = (!message.starts_with(tag) && tag != "UnsupportedYarnLockfileVersion")
            .then_some(message);
        Self {
            tag,
            reason,
            unsupported,
        }
    }

    /// The error a command that cannot go on without the lockfile reports (`pm migrate`, a
    /// frozen install); `install` names the file.
    pub(crate) fn error(&self, name: &str, install: bool) -> Error {
        if let Some((sentence, upgrade)) = &self.unsupported {
            return Error::usage(format!("{sentence}\nnote: {upgrade}"));
        }
        let failed = if install {
            format!("{}: failed to migrate lockfile: '{name}'", self.tag)
        } else {
            format!("error: failed to migrate lockfile: {}", self.tag)
        };
        Error::usage(match &self.reason {
            Some(reason) => format!("{reason}\n{failed}"),
            None => failed,
        })
    }

    /// What an install that goes on without it prints: `next` is tried, or package.json resolved.
    fn report(&self, name: &str, next: Option<&str>, out: &mut Vec<fnpm_types::Event>) {
        use fnpm_types::Event;
        match (&self.unsupported, next) {
            (Some((sentence, _)), Some(next)) => {
                out.push(Event::warn(format!("{sentence}, trying {next}"), None));
            }
            (Some((sentence, upgrade)), None) => {
                out.push(Event::warn(
                    format!("{sentence}, resolving from package.json"),
                    None,
                ));
                out.push(Event::notice(format!("note: {upgrade}")));
            }
            (None, Some(next)) => {
                if let Some(reason) = &self.reason {
                    out.push(Event::notice(format!("error: {reason}")));
                }
                out.push(Event::warn(
                    format!("cannot migrate {name} ({}), trying {next}", self.tag),
                    None,
                ));
            }
            (None, None) => {
                if let Some(reason) = &self.reason {
                    out.push(Event::notice(format!("error: {reason}")));
                }
                out.push(Event::notice(format!(
                    "{}: failed to migrate lockfile: '{name}'",
                    self.tag
                )));
                out.push(Event::notice(String::new()));
                out.push(Event::warn("Ignoring lockfile".to_owned(), None));
            }
        }
    }
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
            sidecar_path: Some(sidecar_path(config, dir)),
        };
        match fnpm_lockfile::load_with(dir, ws, &opts) {
            Ok(Some(loaded)) => {
                state.warnings.extend(loaded.warnings);
                state.config_version_missing = loaded.config_version_missing;
                if loaded.format == Format::Fnpm {
                    state.hash = Some(loaded.hash);
                    state.sidecar_missing = !fnpm_lockfile::sidecar::is_fresh_at(
                        &sidecar_path(config, dir),
                        loaded.hash,
                    );
                } else {
                    state.hash = None;
                    state.sidecar_missing = false;
                }
                // bun.lockb stays the project's lockfile unless `--save-text-lockfile` asks
                // for fnpm.lock (bun `save_format`); bun.lock is migrated to fnpm.lock.
                state.migrated = match loaded.format {
                    Format::Fnpm => false,
                    Format::BunBinary => config.save_text_lockfile == Some(true),
                    _ => true,
                };
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
                record_copied_extensions(&mut graph, &write_back);
                state.graph = Some(graph);
                state.format = Some(format);
                state.path = Some(dir.join(name));
                state.migrated = true;
                state.write_back = write_back;
                return Ok(state);
            }
            Err(e) if config.frozen_lockfile && next.is_none() => {
                return Err(Failure::of(name, bytes, &e).error(name, true));
            }
            Err(e) => Failure::of(name, bytes, &e).report(name, next, &mut state.failures),
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

/// The `packageExtensions` a migration copies into the root package.json, recorded in the graph
/// as the next resolve records them, so fnpm.lock has them and a frozen install accepts it.
fn record_copied_extensions(graph: &mut LockGraph, write_back: &[PackageJsonWriteBack]) {
    if !graph.package_extensions.is_empty() {
        return;
    }
    let copied = write_back
        .iter()
        .filter(|w| w.path.is_empty())
        .flat_map(|w| &w.fields)
        .find(|(key, _)| key == "packageExtensions");
    if let Some((_, value)) = copied {
        let root = serde_json::json!({ "packageExtensions": value });
        graph.package_extensions = fnpm_resolver::extensions::parse_package_extensions(&root).1;
    }
}

/// Migrates one of `names` (in order; bun.lock, bun.lockb and the foreign lockfiles) for
/// `fnpm migrate` / `fnpm-lock migrate`, even when fnpm.lock exists. A file that fails is
/// reported and the next one tried; the last error is returned.
pub(crate) fn migrate_from(config: &Config, ws: &Workspace, names: &[&str]) -> Result<LockState> {
    let dir = &ws.root_dir;
    let mut state = LockState::default();
    let found: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| dir.join(n).exists())
        .collect();
    let files = DiskFiles;
    let mut last: Option<Error> = None;
    for (i, name) in found.iter().enumerate() {
        let next = found.get(i + 1).copied();
        let path = dir.join(name);
        let attempt: Result<Attempt> = if matches!(*name, "bun.lock" | "bun.lockb") {
            let opts = LoadOptions {
                lockfile_path: Some(path.clone()),
                parse: parse_options(config),
                no_sidecar: true,
                sidecar_path: None,
            };
            fnpm_lockfile::load_with(dir, ws, &opts)
                .map_err(Error::from)
                .and_then(|l| {
                    l.map(|l| (l.graph, l.format, Vec::new(), l.warnings))
                        .ok_or_else(|| Error::usage(format!("{name} is empty")))
                })
        } else {
            std::fs::read(&path)
                .map_err(Error::io(&path))
                .and_then(|bytes| {
                    migrate_one(config, ws, &files, name, &bytes, next).map_err(Error::from)
                })
        };
        match attempt {
            Ok((mut graph, format, write_back, warnings)) => {
                state.warnings.extend(warnings);
                if !matches!(format, Format::BunText | Format::BunBinary) {
                    fnpm_lockfile::migration::finish_migration(&mut graph, ws);
                    record_copied_extensions(&mut graph, &write_back);
                }
                state.graph = Some(graph);
                state.format = Some(format);
                state.path = Some(path);
                state.migrated = true;
                state.write_back = write_back;
                return Ok(state);
            }
            Err(e) => {
                let bytes = std::fs::read(&path).unwrap_or_default();
                let failure = match &e {
                    Error::Lockfile(e) => Some(Failure::of(name, &bytes, e)),
                    _ => None,
                };
                match (failure, next) {
                    (Some(f), Some(next)) => f.report(name, Some(next), &mut state.failures),
                    (Some(f), None) => {
                        last = Some(f.error(name, false));
                        continue;
                    }
                    (None, Some(next)) => state
                        .warnings
                        .push(format!("cannot migrate {name} ({e}), trying {next}")),
                    (None, None) => {}
                }
                last = Some(e);
            }
        }
    }
    match last {
        Some(e) => Err(e),
        None => Ok(state),
    }
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
            // bun prints these (`skipping N unused package-lock.json entries`) with --verbose.
            if config.log_level == fnpm_config::LogLevel::Verbose {
                warnings.extend(report.verbose);
            }
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

/// Match the bindings a saved text lockfile will load while retaining ids and metadata used
/// by in-flight fetches and command results. The isolated linker consumes peer bindings
/// directly, so using the pre-write graph would make cold and warm layouts differ.
pub(crate) fn normalize_bindings(graph: &mut LockGraph, ws: &Workspace) -> Result<()> {
    let bytes = fnpm_lockfile::serialize(Format::Fnpm, graph)?;
    let loaded = fnpm_lockfile::parse(Format::Fnpm, &bytes, ws)?;
    let ids: std::collections::HashMap<String, fnpm_types::PackageId> = graph
        .packages
        .iter()
        .enumerate()
        .map(|(i, p)| (p.key(), fnpm_types::PackageId(i as u32)))
        .collect();
    let loaded_ids: Vec<_> = loaded
        .packages
        .iter()
        .map(|p| ids.get(&p.key()).copied())
        .collect();
    let packages: std::collections::HashMap<_, _> =
        loaded.packages.iter().map(|p| (p.key(), p)).collect();
    for p in &mut graph.packages {
        let Some(before) = packages.get(&p.key()) else {
            continue;
        };
        for e in &mut p.dependencies {
            e.target = before
                .dependencies
                .iter()
                .find(|old| old.name == e.name && old.behavior.group() == e.behavior.group())
                .and_then(|old| old.target)
                .and_then(|id| loaded_ids[id.index()]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnpm_types::{Behavior, DepEdge, Package, PackageId, Resolution};
    use serde_json::json;

    #[test]
    fn normalization_uses_text_bindings_without_replacing_package_metadata_or_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            json!({
                "name": "app", "dependencies": {"plugin": "1.0.0", "provider": "1.0.0"}
            })
            .to_string(),
        )
        .unwrap();
        let ws = Workspace::load(dir.path()).unwrap();
        let mut graph = LockGraph::new(Package::new("app", "", Resolution::Root));
        let extra = graph.push(Package::new(
            "extra",
            "1.0.0",
            Resolution::npm("1.0.0", None),
        ));
        let plugin = graph.push(Package::new(
            "plugin",
            "1.0.0",
            Resolution::npm("1.0.0", None),
        ));
        let provider = graph.push(Package::new(
            "provider",
            "1.0.0",
            Resolution::npm("1.0.0", None),
        ));
        for (owner, name, behavior, target) in [
            (PackageId::ROOT, "plugin", Behavior::PROD, Some(plugin)),
            (PackageId::ROOT, "provider", Behavior::PROD, Some(provider)),
            (provider, "extra", Behavior::PROD, Some(extra)),
            (plugin, "extra", Behavior::PEER | Behavior::OPTIONAL, None),
        ] {
            let mut edge = DepEdge::new(name, "1.0.0", behavior);
            edge.target = target;
            graph.packages[owner.index()].dependencies.push(edge);
        }
        graph.packages[plugin.index()].meta.license = Some("MIT".into());
        assert!(
            graph.packages[plugin.index()]
                .dependency("extra")
                .unwrap()
                .target
                .is_none()
        );
        normalize_bindings(&mut graph, &ws).unwrap();
        assert_eq!(
            graph.packages[plugin.index()]
                .dependency("extra")
                .unwrap()
                .target,
            Some(extra)
        );
        assert_eq!(
            graph.packages[plugin.index()].meta.license.as_deref(),
            Some("MIT")
        );
        assert_eq!(graph.packages[extra.index()].name, "extra");
        let before = graph.clone();
        normalize_bindings(&mut graph, &ws).unwrap();
        assert_eq!(graph, before);
    }
}
