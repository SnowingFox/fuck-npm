//! Workspace discovery (bun `lockfile/Package/WorkspaceMap.rs`) and root detection (bun
//! `PackageManager.rs` `find_workspace_root_above`).

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use fnpm_types::Catalogs;
use rayon::prelude::*;
use serde_json::Value;

use crate::glob::{self, Dots, Glob, NegatedWorkspaces, Seg};
use crate::manifest::{PackageJson, workspaces_strings};
use crate::paths::{self, join_posix, relative_posix, to_posix};
use crate::{Error, Member, Result, Workspace, WorkspaceSettings};

/// Directory names a workspace glob never enters (bun `IGNORED_PATHS`).
pub(crate) const IGNORED_PATHS: [&str; 3] = ["node_modules", ".git", "CMakeFiles"];

/// What to do with a listed (non-glob) workspace whose package.json is missing (bun
/// `MissingWorkspace`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MissingWorkspace {
    /// `Workspace not found "<entry>"` ([`Error::WorkspaceNotFound`]); `fnpm install`.
    #[default]
    Error,
    /// Skip it silently (lockfile migration, root detection).
    Skip,
    /// Like `Skip`, and also skip a member whose package.json is empty (runtime auto-install).
    SkipEmptyToo,
}

/// Options for [`Workspace::load_with`].
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    pub missing: MissingWorkspace,
    /// An empty root package.json reads as `{}` (bun: `touch package.json && bun install`,
    /// `bun add` into an empty file). Otherwise it is an error.
    pub empty_root: bool,
}

/// The absolute posix spelling used for glob matching (`/C:/x` on Windows).
pub(crate) fn abs_posix(path: &Path) -> String {
    join_posix(&to_posix(path), "")
}

fn posix_to_path(s: &str) -> PathBuf {
    if cfg!(windows) {
        let t = s.strip_prefix('/').unwrap_or(s);
        if t.split('/')
            .next()
            .is_some_and(|first| first.ends_with(':'))
        {
            return PathBuf::from(t.replace('/', "\\") + if t.ends_with(':') { "\\" } else { "" });
        }
    }
    PathBuf::from(s)
}

/// The `workspaces` entries of a root manifest plus whether they came from
/// `pnpm-workspace.yaml`. Errors on a non-string item like bun.
fn workspace_entries(root: &PackageJson) -> Result<Option<Vec<String>>> {
    match root.raw.get("workspaces") {
        Some(Value::Object(o)) => match o.get("packages") {
            None => Ok(None),
            Some(Value::Array(_)) => {
                workspaces_strings(root.raw.get("workspaces")).map_err(bad_root)
            }
            Some(_) => Err(bad_root(
                "\"workspaces.packages\" expects an array of strings, e.g.\n  \"workspaces\": {\n    \"packages\": [\n      \"path/to/package\"\n    ]\n  }",
            )),
        },
        other => workspaces_strings(other).map_err(bad_root),
    }
}

fn bad_root(message: &str) -> Error {
    Error::InvalidPackageJson {
        path: PathBuf::from("package.json"),
        message: message.to_owned(),
    }
}

/// A parsed `pnpm-workspace.yaml`.
#[derive(Debug, Default)]
pub(crate) struct PnpmWorkspace {
    pub packages: Option<Vec<String>>,
    pub catalog: Vec<(String, String)>,
    pub catalogs: Vec<(String, Vec<(String, String)>)>,
    pub overrides: Vec<(String, String)>,
    pub only_built_dependencies: Option<Vec<String>>,
}

pub(crate) fn read_pnpm_workspace(
    dir: &Path,
    warnings: &mut Vec<String>,
) -> Result<Option<PnpmWorkspace>> {
    let path = dir.join("pnpm-workspace.yaml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(None);
        }
        Err(source) => return Err(Error::Io { path, source }),
    };
    let doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&text).map_err(|e| Error::InvalidPackageJson {
            path: path.clone(),
            message: e.to_string(),
        })?;
    let mut ws = PnpmWorkspace::default();
    let get = |key: &str| doc.as_mapping().and_then(|m| m.get(key));
    if let Some(seq) = get("packages").and_then(|v| v.as_sequence()) {
        ws.packages = Some(
            seq.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        );
    }
    let strings = |field: &str, map: &serde_yaml_ng::Mapping, warnings: &mut Vec<String>| {
        let mut out = Vec::new();
        let mut skipped = 0;
        for (k, v) in map {
            match (k.as_str(), v.as_str()) {
                (Some(k), Some(v)) => out.push((k.to_owned(), v.to_owned())),
                _ => skipped += 1,
            }
        }
        if skipped > 0 {
            warnings.push(format!(
                "skipping non-string entries of \"{field}\" in pnpm-workspace.yaml ({skipped})"
            ));
        }
        out
    };
    if let Some(m) = get("catalog").and_then(|v| v.as_mapping()) {
        ws.catalog = strings("catalog", m, warnings);
    }
    if let Some(m) = get("catalogs").and_then(|v| v.as_mapping()) {
        for (name, group) in m {
            if let (Some(name), Some(group)) = (name.as_str(), group.as_mapping()) {
                let entries = strings("catalogs", group, warnings);
                ws.catalogs.push((name.to_owned(), entries));
            }
        }
    }
    if let Some(m) = get("overrides").and_then(|v| v.as_mapping()) {
        ws.overrides = strings("overrides", m, warnings);
    }
    if let Some(seq) = get("onlyBuiltDependencies").and_then(|v| v.as_sequence()) {
        ws.only_built_dependencies = Some(
            seq.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        );
    }
    Ok(Some(ws))
}

/// package.json catalogs (bun `CatalogMap::parse_append` called as in `Package.rs`):
/// `workspaces.catalog(s)`; if neither is there and the root has `workspaces`, top-level
/// `catalog(s)`.
fn package_json_catalogs(root: &PackageJson) -> Result<Catalogs> {
    let mut catalogs = Catalogs::default();
    let Some(workspaces) = root.raw.get("workspaces") else {
        return Ok(catalogs);
    };
    let source = if workspaces.get("catalog").is_some() || workspaces.get("catalogs").is_some() {
        workspaces
    } else {
        &root.raw
    };
    let string_entries = |v: &Value| -> Vec<(String, String)> {
        v.as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    };
    if let Some(c) = source.get("catalog") {
        catalogs.default.extend(string_entries(c));
    }
    if let Some(Value::Object(groups)) = source.get("catalogs") {
        for (name, group) in groups {
            let entries = string_entries(group);
            if name == "default" {
                for (dep, version) in entries {
                    if catalogs.default.contains_key(&dep) {
                        return Err(bad_root(&format!(
                            "\"{dep}\" is defined in both \"catalog\" and \"catalogs.default\"; keep one of them"
                        )));
                    }
                    catalogs.default.insert(dep, version);
                }
            } else {
                catalogs
                    .named
                    .entry(name.clone())
                    .or_default()
                    .extend(entries);
            }
        }
    }
    Ok(catalogs)
}

fn settings(root: &PackageJson, pnpm: Option<&PnpmWorkspace>) -> Result<WorkspaceSettings> {
    let mut catalogs = package_json_catalogs(root)?;
    let mut overrides = Vec::new();
    let mut only_built: Option<Vec<String>> = None;
    if let Some(list) = root
        .raw
        .get("pnpm")
        .and_then(|p| p.get("onlyBuiltDependencies"))
        .and_then(Value::as_array)
    {
        only_built = Some(
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        );
    }
    if let Some(ws) = pnpm {
        for (dep, version) in &ws.catalog {
            catalogs
                .default
                .entry(dep.clone())
                .or_insert_with(|| version.clone());
        }
        for (name, entries) in &ws.catalogs {
            let map = if name == "default" {
                &mut catalogs.default
            } else {
                catalogs.named.entry(name.clone()).or_default()
            };
            for (dep, version) in entries {
                map.entry(dep.clone()).or_insert_with(|| version.clone());
            }
        }
        overrides.clone_from(&ws.overrides);
        if let Some(list) = &ws.only_built_dependencies {
            let all = only_built.get_or_insert_with(Vec::new);
            for name in list {
                if !all.contains(name) {
                    all.push(name.clone());
                }
            }
        }
    }
    Ok(WorkspaceSettings {
        catalogs,
        overrides,
        only_built_dependencies: only_built,
    })
}

/// One `workspaces` entry, classified.
enum Entry {
    Literal(String),
    Glob(String),
}

/// Splits entries into literal paths and globs, collecting `!` entries (bun
/// `process_names_array`). `""`, `.`, `./` and `.\` are ignored.
fn classify(entries: &[String], negated: &mut NegatedWorkspaces) -> Vec<Entry> {
    let mut out = Vec::new();
    for e in entries {
        if matches!(e.as_str(), "" | "." | "./" | ".\\") {
            continue;
        }
        if glob::detect_glob_syntax(e) {
            if !negated.take(e) {
                out.push(Entry::Glob(e.clone()));
            }
            continue;
        }
        out.push(Entry::Literal(e.clone()));
    }
    out
}

/// Expands one glob `user_pattern` (relative to `root_posix`, or absolute) into the directories
/// that match it (whether they have a package.json is checked by the caller). Hidden segments are not matched by
/// wildcards; ignored names are never entered. Symlinked directories are followed; one that
/// points back at an ancestor is matched but not entered.
pub(crate) fn expand_glob(
    root_posix: &str,
    user_pattern: &str,
    parallel: bool,
) -> Result<Vec<PathBuf>> {
    expand_glob_with(
        root_posix,
        user_pattern,
        &IGNORED_PATHS,
        Dots::Hide,
        parallel,
    )
}

/// [`expand_glob`] with the names never entered and how wildcards treat dot segments.
pub(crate) fn expand_glob_opts(
    root_posix: &str,
    user_pattern: &str,
    ignored: &[&str],
    dots: Dots,
) -> Result<Vec<PathBuf>> {
    expand_glob_with(root_posix, user_pattern, ignored, dots, true)
}

/// [`expand_glob_opts`]; `parallel: false` never touches the rayon pool (the no-op fast path
/// runs before any thread is started).
fn expand_glob_with(
    root_posix: &str,
    user_pattern: &str,
    ignored: &[&str],
    dots: Dots,
    parallel: bool,
) -> Result<Vec<PathBuf>> {
    let joined = join_posix(&glob::escape_literal(root_posix), user_pattern);
    let mut out = Vec::new();
    for alt in glob::expand_braces(&joined) {
        let segs = glob::compile_segments(&alt);
        let literal = segs.iter().take_while(|s| s.literal().is_some()).count();
        let mut base = String::new();
        for s in &segs[..literal] {
            base.push('/');
            base.push_str(s.literal().expect("literal"));
        }
        if base.is_empty() {
            base.push('/');
        }
        let rest = &segs[literal..];
        let base_path = posix_to_path(&base);
        if !rest.is_empty() && Path::new(user_pattern.trim_start_matches("./")).is_absolute() {
            // bun opens the literal prefix of an absolute pattern before walking it.
            if let Err(source) = std::fs::read_dir(&base_path) {
                return Err(glob_failed(user_pattern, "open", base_path, source));
            }
        }
        let mut states = vec![false; rest.len() + 1];
        states[0] = true;
        glob::close(rest, &mut states);
        let walker = Walker {
            segs: rest,
            pattern: user_pattern,
            ignored,
            dots,
            parallel,
        };
        walker.visit(&base_path, &states, &mut Vec::new(), &mut out)?;
    }
    Ok(out)
}

fn glob_failed(
    pattern: &str,
    syscall: &'static str,
    path: PathBuf,
    source: std::io::Error,
) -> Error {
    Error::GlobFailed {
        pattern: pattern.to_owned(),
        syscall,
        path,
        source,
    }
}

fn ignorable(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

struct Walker<'a> {
    segs: &'a [Seg],
    pattern: &'a str,
    ignored: &'a [&'a str],
    dots: Dots,
    parallel: bool,
}

/// Fan out to rayon only when a directory has this many candidate children.
const PARALLEL_CHILDREN: usize = 8;

impl Walker<'_> {
    fn visit(
        &self,
        dir: &Path,
        states: &[bool],
        ancestors: &mut Vec<PathBuf>,
        out: &mut Vec<PathBuf>,
    ) -> Result<()> {
        if states[self.segs.len()] {
            // The package.json is checked when it is read.
            out.push(dir.to_path_buf());
        }
        if !states[..self.segs.len()].iter().any(|&s| s) {
            return Ok(());
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if ignorable(&e) => return Ok(()),
            Err(source) => {
                return Err(glob_failed(self.pattern, "open", dir.to_path_buf(), source));
            }
        };
        let mut children: Vec<(PathBuf, Vec<bool>, bool)> = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|source| glob_failed(self.pattern, "open", dir.to_path_buf(), source))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if self.ignored.contains(&name) {
                continue;
            }
            let next = glob::step(self.segs, states, name, self.dots);
            if !next.iter().any(|&s| s) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            let is_link = ft.is_symlink();
            let is_dir = if is_link {
                std::fs::metadata(&path).is_ok_and(|m| m.is_dir())
            } else {
                ft.is_dir()
            };
            if is_dir {
                children.push((path, next, is_link));
            }
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        ancestors.push(dir.to_path_buf());
        let result = if self.parallel && children.len() >= PARALLEL_CHILDREN {
            let parts: Vec<Result<Vec<PathBuf>>> = children
                .par_iter()
                .map(|(path, next, is_link)| {
                    let mut local = Vec::new();
                    let mut anc = ancestors.clone();
                    self.child(path, next, *is_link, &mut anc, &mut local)
                        .map(|()| local)
                })
                .collect();
            parts.into_iter().try_for_each(|p| p.map(|v| out.extend(v)))
        } else {
            children.iter().try_for_each(|(path, next, is_link)| {
                self.child(path, next, *is_link, ancestors, out)
            })
        };
        ancestors.pop();
        result
    }

    fn child(
        &self,
        path: &Path,
        next: &[bool],
        is_link: bool,
        ancestors: &mut Vec<PathBuf>,
        out: &mut Vec<PathBuf>,
    ) -> Result<()> {
        if is_link && links_to_ancestor(path, ancestors) {
            // Match it, but do not descend: the loop would repeat until the path is too long.
            let only_here: Vec<bool> = (0..next.len())
                .map(|i| i == self.segs.len() && next[i])
                .collect();
            return self.visit(path, &only_here, ancestors, out);
        }
        self.visit(path, next, ancestors, out)
    }
}

#[cfg(unix)]
fn links_to_ancestor(path: &Path, ancestors: &[PathBuf]) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(target) = std::fs::metadata(path) else {
        return false;
    };
    ancestors.iter().any(|a| {
        std::fs::metadata(a).is_ok_and(|m| m.dev() == target.dev() && m.ino() == target.ino())
    })
}

#[cfg(not(unix))]
fn links_to_ancestor(path: &Path, ancestors: &[PathBuf]) -> bool {
    let Ok(target) = std::fs::canonicalize(path) else {
        return false;
    };
    ancestors
        .iter()
        .any(|a| std::fs::canonicalize(a).is_ok_and(|c| c == target))
}

/// A member directory found by an entry.
#[derive(Debug, Clone)]
pub(crate) struct Found {
    pub dir: PathBuf,
    /// The entry that found it; literal entries report `Workspace not found`.
    pub entry: String,
    pub literal: bool,
}

/// Every member directory `entries` select, keyed by path relative to `root_dir`. Literal
/// entries come first and win over globs for the same directory.
pub(crate) fn member_dirs(
    root_dir: &Path,
    entries: &[String],
    parallel: bool,
) -> Result<BTreeMap<String, Found>> {
    let root_posix = abs_posix(root_dir);
    let mut negated = NegatedWorkspaces::default();
    let classified = classify(entries, &mut negated);
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    for entry in &classified {
        match entry {
            Entry::Literal(p) => {
                let dir = posix_to_path(&join_posix(&root_posix, p));
                let rel = relative_posix(root_dir, &dir);
                if rel.is_empty() {
                    continue;
                }
                found.insert(
                    rel,
                    Found {
                        dir,
                        entry: p.clone(),
                        literal: true,
                    },
                );
            }
            Entry::Glob(p) => {
                for dir in expand_glob(&root_posix, p, parallel)? {
                    let rel = relative_posix(root_dir, &dir);
                    if rel.is_empty() || negated.excludes(&rel) {
                        continue;
                    }
                    found.entry(rel).or_insert(Found {
                        dir,
                        entry: p.clone(),
                        literal: false,
                    });
                }
            }
        }
    }
    Ok(found)
}

/// The `workspaces` entries members are discovered from: the root package.json's, else the
/// `packages` of `pnpm-workspace.yaml` when the root has no `workspaces` field.
pub(crate) fn member_patterns(root_dir: &Path, root: &PackageJson) -> Result<Option<Vec<String>>> {
    let root_pj = root_dir.join("package.json");
    match workspace_entries(root).map_err(|e| with_path(e, &root_pj))? {
        Some(e) => Ok(Some(e)),
        None if root.raw.get("workspaces").is_none() => {
            let mut warnings = Vec::new();
            Ok(read_pnpm_workspace(root_dir, &mut warnings)?.and_then(|p| p.packages))
        }
        None => Ok(None),
    }
}

/// [`crate::Workspace::load_with`].
pub(crate) fn load(root_dir: &Path, options: &LoadOptions) -> Result<(Workspace, Vec<String>)> {
    let root_dir = paths::absolute(root_dir);
    let root_pj = root_dir.join("package.json");
    let root = match PackageJson::read(&root_pj) {
        Err(Error::InvalidPackageJson { message, .. })
            if options.empty_root && message == crate::manifest::EMPTY_FILE =>
        {
            PackageJson::parse_at("{}", &root_pj)?
        }
        Err(Error::Io { source, .. })
            if matches!(
                source.kind(),
                ErrorKind::NotFound | ErrorKind::NotADirectory
            ) =>
        {
            return Err(Error::NoProject(root_dir));
        }
        other => other?,
    };
    let mut warnings = Vec::new();
    let pnpm = read_pnpm_workspace(&root_dir, &mut warnings)?;
    let entries = match workspace_entries(&root).map_err(|e| with_path(e, &root_pj))? {
        Some(e) => Some(e),
        None if root.raw.get("workspaces").is_none() => {
            pnpm.as_ref().and_then(|p| p.packages.clone())
        }
        None => None,
    };
    let settings = settings(&root, pnpm.as_ref()).map_err(|e| with_path(e, &root_pj))?;
    let mut members = Vec::new();
    if let Some(entries) = entries.filter(|e| !e.is_empty()) {
        let dirs: Vec<(String, Found)> = member_dirs(&root_dir, &entries, true)?
            .into_iter()
            .collect();
        let parsed: Vec<Result<Outcome>> = dirs
            .par_iter()
            .map(|(rel, found)| read_member(rel, found, options.missing))
            .collect();
        for (result, (rel, _)) in parsed.into_iter().zip(&dirs) {
            match result? {
                Outcome::Member(m) => members.push(*m),
                Outcome::Nameless {
                    declares_dependencies: true,
                } => {
                    warnings.push(format!(
                        "skipping workspace \"{rel}\": package.json has no \"name\""
                    ));
                }
                Outcome::Nameless { .. } | Outcome::Skipped => {}
            }
        }
    }
    let mut by_name: BTreeMap<&str, &str> = BTreeMap::new();
    for m in &members {
        if let Some(first) = by_name.insert(&m.name, &m.path) {
            return Err(Error::DuplicateName {
                name: m.name.clone(),
                first: first.to_owned(),
                second: m.path.clone(),
            });
        }
    }
    Ok((
        Workspace {
            root_dir,
            root,
            members,
            settings,
        },
        warnings,
    ))
}

fn with_path(err: Error, path: &Path) -> Error {
    match err {
        Error::InvalidPackageJson { message, .. } => Error::InvalidPackageJson {
            path: path.to_path_buf(),
            message,
        },
        other => other,
    }
}

enum Outcome {
    Member(Box<Member>),
    Nameless { declares_dependencies: bool },
    Skipped,
}

fn read_member(rel: &str, found: &Found, missing: MissingWorkspace) -> Result<Outcome> {
    let pj = found.dir.join("package.json");
    let text = match std::fs::read_to_string(&pj) {
        Ok(t) => t,
        Err(e) => {
            let not_found = matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory);
            let not_a_file = !found.literal && e.kind() == ErrorKind::IsADirectory;
            if not_a_file || not_found && (!found.literal || missing != MissingWorkspace::Error) {
                return Ok(Outcome::Skipped);
            }
            if found.literal {
                if not_found
                    || matches!(
                        e.kind(),
                        ErrorKind::IsADirectory | ErrorKind::PermissionDenied
                    )
                {
                    return Err(Error::WorkspaceNotFound(found.entry.clone()));
                }
                return Err(Error::Io {
                    path: pj,
                    source: e,
                });
            }
            // A directory that cannot be listed fails on `open`, like bun's walker.
            if let Err(source) = std::fs::read_dir(&found.dir) {
                return Err(glob_failed(&found.entry, "open", found.dir.clone(), source));
            }
            return Err(glob_failed(&found.entry, "fstatat", pj, e));
        }
    };
    if missing == MissingWorkspace::SkipEmptyToo && text.trim().is_empty() {
        return Ok(Outcome::Skipped);
    }
    let manifest = PackageJson::parse_at(&text, &pj)?;
    let Some(name) = manifest.name.clone() else {
        return Ok(Outcome::Nameless {
            declares_dependencies: manifest.declares_dependencies(),
        });
    };
    Ok(Outcome::Member(Box::new(Member {
        name,
        version: manifest.version.clone().unwrap_or_default(),
        path: rel.to_owned(),
        dir: found.dir.clone(),
        manifest,
    })))
}

/// Returns true if `entries` (of the package.json in `root_dir`) would make `abs_dir` a member,
/// given that it has a package.json; never reads the disk (bun `WorkspaceMap::lists`).
pub(crate) fn lists(entries: &[String], root_dir: &Path, abs_dir: &Path) -> bool {
    let rel = relative_posix(root_dir, abs_dir);
    if rel.is_empty() {
        return false;
    }
    let root_posix = abs_posix(root_dir);
    let dir_posix = abs_posix(abs_dir);
    let mut negated = NegatedWorkspaces::default();
    let mut matched = false;
    for entry in classify(entries, &mut negated) {
        match entry {
            Entry::Literal(p) => {
                if join_posix(&root_posix, &p) == dir_posix {
                    return true;
                }
            }
            Entry::Glob(p) => {
                if rel.split('/').any(|s| IGNORED_PATHS.contains(&s)) {
                    continue;
                }
                matched = matched
                    || Glob::new(&join_posix(&glob::escape_literal(&root_posix), &p))
                        .matches(&dir_posix, Dots::Hide);
            }
        }
    }
    matched && !negated.excludes(&rel)
}

/// The workspace entries a directory declares: package.json `workspaces`, else the `packages`
/// of a sibling `pnpm-workspace.yaml`. `Ok(None)` if it declares none.
fn declared_entries(dir: &Path, pkg: &PackageJson) -> Result<Option<Vec<String>>> {
    if pkg.raw.get("workspaces").is_some() {
        return Ok(workspaces_strings(pkg.raw.get("workspaces")).ok().flatten());
    }
    Ok(read_pnpm_workspace(dir, &mut Vec::new())?.and_then(|p| p.packages))
}

/// Returns true if `dir` is sticky and group- or world-writable, like `/tmp`, or cannot be
/// stat'd: any local user can plant a package.json there, so the upward walk ignores it (bun
/// `is_shared_directory`).
#[cfg(unix)]
pub(crate) fn is_shared_directory(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    !std::fs::metadata(dir).is_ok_and(|m| {
        let mode = m.permissions().mode();
        mode & 0o1000 == 0 || mode & 0o022 == 0
    })
}

#[cfg(not(unix))]
pub(crate) fn is_shared_directory(_: &Path) -> bool {
    false
}

/// bun's warning for a package.json the upward walk ignores.
pub(crate) fn shared_directory_warning(dir: &Path) -> String {
    format!(
        "ignoring package.json in {}: directory is writable by other users",
        dir.display()
    )
}

/// [`crate::Workspace::find_root`].
pub(crate) fn find_root(cwd: &Path) -> Result<PathBuf> {
    let cwd = paths::absolute(cwd);
    let mut child: Option<PathBuf> = None;
    let mut dir = Some(cwd.as_path());
    while let Some(d) = dir {
        dir = d.parent();
        if !d.join("package.json").is_file() {
            continue;
        }
        if d != cwd && is_shared_directory(d) {
            continue;
        }
        child = Some(d.to_path_buf());
        break;
    }
    let Some(child) = child else {
        return Err(Error::NoProject(cwd));
    };
    // Only a named package can be a member (bun matches the members it read, and skips
    // nameless or unreadable ones), so anything else is its own root.
    let child_pj = child.join("package.json");
    let named = std::fs::read_to_string(&child_pj)
        .ok()
        .and_then(|text| PackageJson::parse_at(&text, &child_pj).ok())
        .is_some_and(|pkg| pkg.name.is_some());
    if !named {
        return Ok(child);
    }
    let mut parent = child.parent();
    while let Some(p) = parent {
        parent = p.parent();
        if is_shared_directory(p) {
            continue;
        }
        let pj = p.join("package.json");
        let pkg = match std::fs::read_to_string(&pj) {
            Ok(text) => Some(PackageJson::parse_at(&text, &pj)?),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::PermissionDenied
                ) =>
            {
                None
            }
            Err(source) => return Err(Error::Io { path: pj, source }),
        };
        let declares = pkg
            .as_ref()
            .is_some_and(|pkg| pkg.raw.get("workspaces").is_some())
            || p.join("pnpm-workspace.yaml").is_file();
        if declares {
            let entries = match &pkg {
                Some(pkg) => declared_entries(p, pkg)?,
                None => read_pnpm_workspace(p, &mut Vec::new())?.and_then(|w| w.packages),
            };
            return Ok(match entries {
                Some(entries) if pkg.is_some() && lists(&entries, p, &child) => p.to_path_buf(),
                _ => child,
            });
        }
    }
    Ok(child)
}
