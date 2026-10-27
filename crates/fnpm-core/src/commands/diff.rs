//! `fnpm pm diff` (bun `pm_diff_command.rs`): what changed between two versions of a package, or
//! between a package and a folder or tarball. Both sides are read into memory; nothing is
//! installed and no script runs.
//!
//! Piped, the output is a unified patch that `git apply` accepts; on a terminal it is bun's view
//! with per-file headers and a line-number gutter. bun re-prints JavaScript and CSS through its
//! parser before comparing them; fnpm has no JavaScript or CSS parser, so those files are compared
//! as text. JSON is re-printed like bun does.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{IsTerminal, Read, Write as _};
use std::path::{Path, PathBuf};

use fnpm_store::tar::{EntryKind, TarEvent, TarParser};
use fnpm_types::{HashAlgorithm, Integrity, Resolution};
use serde_json::Value;

use super::Ctx;
use super::pack::json::{self as pj, Json};
use super::pack::out::{self as pout, Fatal, Out};
use super::pack::walk::{Walker, bytes_path};
use crate::{CoreResult, Result};

type Fail<T> = std::result::Result<T, Fatal>;

#[derive(Clone, Copy)]
struct Flags {
    /// Compare bytes, not the re-print of JSON.
    raw: bool,
    json: bool,
    /// Files differing only in whitespace count as unchanged (terminal view).
    ignore_space: bool,
    name_only: bool,
    stat: bool,
    context: usize,
}

/// One side of the comparison, as the user wrote it.
enum Spec {
    Registry {
        name: String,
        version: String,
        /// The integrity fnpm.lock records, when the version came from the lockfile.
        locked: Option<Integrity>,
    },
    Dir(String),
    Tarball(String),
}

fn registry(name: &str, version: &str) -> Spec {
    Spec::Registry {
        name: name.to_owned(),
        version: version.to_owned(),
        locked: None,
    }
}

/// A side read into memory: every file's contents by relative path.
#[derive(Default)]
struct Tree {
    label: String,
    files: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Permission bits per path, when the source records them.
    modes: BTreeMap<Vec<u8>, u32>,
    /// The registry document's entry for this version, for a registry side.
    registry_entry: Option<Json>,
}

pub(crate) async fn run(cx: &Ctx) -> Result<CoreResult> {
    let out = Out::new();
    let code = match diff(cx, &out).await {
        Ok(()) => 0,
        Err(fatal) => {
            out.eprintln(&fatal.0);
            1
        }
    };
    Ok(CoreResult {
        exit_code: code,
        data: None,
        lines: None,
    })
}

async fn diff(cx: &Ctx, out: &Out) -> Fail<()> {
    let flags = Flags {
        raw: cx.flag("raw") || cx.flag("unformatted"),
        json: cx.req.json,
        ignore_space: cx.flag("ignoreSpace"),
        name_only: cx.flag("nameOnly"),
        stat: cx.flag("stat"),
        context: cx.number("unified").unwrap_or(3).min(1 << 20) as usize,
    };
    // `:path` (alone, or as a suffix on a registry spec) and anything after the two sides narrow
    // the diff to files.
    let mut filters: Vec<String> = Vec::new();
    let mut typed: Vec<String> = Vec::new();
    for arg in cx.strings("diff").iter().chain(cx.req.positionals.iter()) {
        if let Some(only) = arg.strip_prefix(':') {
            filters.push(only.to_owned());
        } else if typed.len() == 2 {
            filters.push(arg.clone());
        } else if let Some((spec, only)) = split_filter(arg) {
            typed.push(spec.to_owned());
            filters.push(only.to_owned());
        } else {
            typed.push(arg.clone());
        }
    }
    filters.retain(|f| !f.is_empty());
    let cwd = cx.req.cwd.clone();
    let home = cx.env.get("HOME").map(str::to_owned);
    let args: Vec<String> = typed
        .iter()
        .map(|arg| match (arg.strip_prefix("~/"), &home) {
            (Some(rest), Some(home)) => Path::new(home).join(rest).to_string_lossy().into_owned(),
            _ if looks_like_path(arg) && !Path::new(arg).is_absolute() => {
                cwd.join(arg).to_string_lossy().into_owned()
            }
            _ => arg.clone(),
        })
        .collect();

    let (mut left_spec, mut right_spec) = resolve_sides(cx, &args)?;
    // Two folders are compared as written, because the patch between them has to apply to the
    // files on disk.
    let as_published = !matches!((&left_spec, &right_spec), (Spec::Dir(_), Spec::Dir(_)));
    let mut client = None;
    // `fnpm pm diff ./pkg` / `./pkg 2.0.0`: a registry side without a name takes it from the
    // local side's package.json.
    let mut left_early = None;
    if let (Spec::Dir(_) | Spec::Tarball(_), Spec::Registry { name, .. }) =
        (&left_spec, &mut right_spec)
        && name.is_empty()
    {
        let local = materialize(cx, out, &mut client, &left_spec, as_published).await?;
        *name = package_name_in(out, &local)?;
        left_early = Some(local);
    }
    let mut right = materialize(cx, out, &mut client, &right_spec, as_published).await?;
    if let Spec::Registry { name, .. } = &mut left_spec
        && name.is_empty()
    {
        *name = package_name_in(out, &right)?;
    }
    let mut left = match left_early {
        Some(tree) => tree,
        None => materialize(cx, out, &mut client, &left_spec, as_published).await?,
    };
    // Show local paths the way they were typed, not as resolved against the invoking folder.
    for (tree, spec) in [(&mut left, &left_spec), (&mut right, &right_spec)] {
        if let Spec::Dir(p) | Spec::Tarball(p) = spec {
            match args.iter().position(|a| a == p) {
                Some(i) => tree.label = typed[i].clone(),
                None if args.is_empty() => tree.label = ".".to_owned(),
                None => {}
            }
        }
    }
    if !filters.is_empty() {
        let before = left.files.len() + right.files.len();
        for tree in [&mut left, &mut right] {
            tree.files
                .retain(|path, _| filters.iter().any(|f| filter_matches(f, path)));
            tree.modes
                .retain(|path, _| filters.iter().any(|f| filter_matches(f, path)));
        }
        if left.files.is_empty() && right.files.is_empty() && before > 0 {
            return Err(out.error(&format!(
                "no file in either side matches {}",
                filters.join(", ")
            )));
        }
    }
    let style = Style::detect(cx, out, flags);
    let text = print_diff(&left, &right, flags, style);
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&text);
    let _ = stdout.flush();
    Ok(())
}

/// `name@1.2.3:lib/x.js` → (`name@1.2.3`, `lib/x.js`). Paths and URLs keep their colons.
fn split_filter(arg: &str) -> Option<(&str, &str)> {
    if looks_like_path(arg)
        || arg.contains("://")
        || arg.starts_with("file:")
        || arg.starts_with("npm:")
    {
        return None;
    }
    let at = arg.find(':')?;
    (at > 0).then(|| (&arg[..at], &arg[at + 1..]))
}

/// A filter names a file exactly, a directory (prefix), a bare file name anywhere in the
/// package, or a glob.
fn filter_matches(filter: &str, path: &[u8]) -> bool {
    let filter = filter.strip_prefix("./").unwrap_or(filter);
    let filter = filter.strip_suffix('/').unwrap_or(filter);
    let f = filter.as_bytes();
    if path == f || (path.len() > f.len() && path.starts_with(f) && path[f.len()] == b'/') {
        return true;
    }
    let base = basename(path);
    if filter.contains(['*', '?', '[', '{']) {
        let glob = |p: &[u8]| {
            let m = super::pack::glob::glob_match(f, p);
            m.matches && !m.negated
        };
        return glob(path) || (!filter.contains('/') && glob(base));
    }
    !filter.contains('/') && base == f
}

fn basename(path: &[u8]) -> &[u8] {
    path.rsplit(|&b| b == b'/').next().unwrap_or(path)
}

fn looks_like_path(spec: &str) -> bool {
    spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with("~/")
        || (cfg!(windows)
            && (spec.starts_with('\\')
                || (spec.len() > 2
                    && spec.as_bytes()[1] == b':'
                    && matches!(spec.as_bytes()[2], b'\\' | b'/'))))
}

/// `name@version`, minding a leading scope `@`. No `@` means no version.
fn split_name_and_version(spec: &str) -> (&str, Option<&str>) {
    match spec[1.min(spec.len())..].find('@') {
        Some(at) => (&spec[..at + 1], Some(&spec[at + 2..])),
        None => (spec, None),
    }
}

fn classify(spec: &str) -> Spec {
    if looks_like_path(spec) {
        if spec.ends_with(".tgz") || spec.ends_with(".tar.gz") || spec.ends_with(".tar") {
            return Spec::Tarball(spec.to_owned());
        }
        return Spec::Dir(spec.to_owned());
    }
    let (name, version) = split_name_and_version(spec);
    // An empty version means "none given", which differs from an explicit `@latest`.
    registry(name, version.unwrap_or(""))
}

/// Turns 0, 1 or 2 user arguments into the two sides to compare.
fn resolve_sides(cx: &Ctx, args: &[String]) -> Fail<(Spec, Spec)> {
    let out = Out::new();
    match args {
        // In a package folder: what is published under this name → the folder.
        [] => Ok((
            registry(&root_package_name(cx, &out)?, "latest"),
            Spec::Dir(cx.req.cwd.to_string_lossy().into_owned()),
        )),
        [one] => match classify(one) {
            Spec::Registry { name, version, .. } => {
                // `name@a..b`
                if let Some(dots) = version.find("..") {
                    return Ok((
                        registry(&name, &version[..dots]),
                        registry(&name, &version[dots + 2..]),
                    ));
                }
                // `name` / `name@b`: the version this project has installed → b (default: latest).
                let Some((name, installed, locked)) = installed_version(cx, &name) else {
                    return Err(out.error(&format!(
                        "{name} is not in this project's lockfile; give two versions to compare, e.g. `fnpm pm diff {name}@1.0.0 {name}@2.0.0`"
                    )));
                };
                let target = if version.is_empty() {
                    "latest".to_owned()
                } else {
                    version
                };
                Ok((
                    Spec::Registry {
                        name: name.clone(),
                        version: installed,
                        locked,
                    },
                    registry(&name, &target),
                ))
            }
            // A folder or tarball on its own: what is published under its package.json name → it.
            local => Ok((registry("", "latest"), local)),
        },
        [a, b] => {
            let mut left = classify(a);
            // `name 2`: a bare name on the left means the installed version of that package, if
            // there is one.
            if let Spec::Registry {
                name,
                version,
                locked,
            } = &mut left
                && version.is_empty()
                && let Some(found) = installed_version(cx, name)
            {
                (*name, *version, *locked) = found;
            }
            // `2`, `^2`, `>=2 <4` and `latest`: what package.json would read as a version or
            // range. Other dist-tags do not count, because a word like `next` is also a package.
            let bare_version = |s: &Spec| {
                matches!(s, Spec::Registry { name, version, .. } if version.is_empty()
                    && (fnpm_types::spec::infer_tag(name) == fnpm_types::SpecTag::Npm || name == "latest"))
            };
            let right = match classify(b) {
                Spec::Registry { name: bare, .. } if bare_version(&classify(b)) => match &left {
                    // `name@1 2`: a bare second version reuses the first name.
                    Spec::Registry { name, .. } => registry(name, &bare),
                    // `./pkg 2`: that package's own name, filled in once it is read.
                    _ => registry("", &bare),
                },
                other => other,
            };
            Ok((left, right))
        }
        _ => unreachable!("at most two sides are collected"),
    }
}

fn package_name_in(out: &Out, tree: &Tree) -> Fail<String> {
    tree.files
        .get(b"package.json".as_slice())
        .and_then(|bytes| package_json_name(bytes))
        .ok_or_else(|| {
            out.error(&format!(
                "{} has no package.json \"name\" to look up in the registry",
                lossy(&defang(tree.label.as_bytes()))
            ))
        })
}

fn package_json_name(bytes: &[u8]) -> Option<String> {
    let json = pj::parse(std::str::from_utf8(bytes).ok()?).ok()?;
    let name = json.get("name")?.as_str()?;
    is_npm_name(name).then(|| name.to_owned())
}

/// npm's package-name grammar, so nothing else from a package.json is ever printed as a name or
/// sent to a registry.
fn is_npm_name(name: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.len() <= 214
            && !p.starts_with(['.', '_'])
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._~".contains(&b))
    };
    match name.strip_prefix('@') {
        Some(rest) => rest.split_once('/').is_some_and(|(a, b)| part(a) && part(b)),
        None => part(name),
    }
}

fn root_package_name(cx: &Ctx, out: &Out) -> Fail<String> {
    // The folder we are in, before the workspace root above it.
    if let Some(name) = std::fs::read(cx.req.cwd.join("package.json"))
        .ok()
        .and_then(|b| package_json_name(&b))
    {
        return Ok(name);
    }
    cx.project_anywhere()
        .ok()
        .and_then(|p| std::fs::read(p.root_dir.join("package.json")).ok())
        .and_then(|b| package_json_name(&b))
        .ok_or_else(|| {
            out.error("no package name to compare against: run this inside a package, or pass two specs (e.g. `fnpm pm diff react@18.0.0 react@19.0.0`)")
        })
}

/// The npm package that `name` refers to in the folder the command was run in, the version the
/// lockfile resolved for it and its integrity. `name` may be an alias of that package.
fn installed_version(cx: &Ctx, name: &str) -> Option<(String, String, Option<Integrity>)> {
    let (project, _, Some(graph)) = cx.load().ok()? else {
        return None;
    };
    let member = cx
        .req
        .cwd
        .strip_prefix(&project.root_dir)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let member = member.trim_end_matches('/');
    let importer = graph
        .packages
        .iter()
        .position(|p| matches!(&p.resolution, Resolution::Workspace { path } if path.trim_end_matches('/') == member))
        .unwrap_or(0);
    // Several copies may be installed, so search in the order `require(name)` would: the
    // dependencies of the package the command was run in, then the root node_modules, then a copy
    // that is nested deeper or only installed under an alias.
    let own = graph.packages[importer]
        .dependencies
        .iter()
        .filter(|e| e.name == name)
        .filter_map(|e| e.target.map(|t| t.index()));
    let view = crate::install::link_view(&graph);
    let tree = fnpm_lockfile::tree::hoist(
        &view,
        &fnpm_lockfile::tree::HoistOptions::all(project.config.platform),
    );
    let in_workspace: std::collections::HashSet<u32> = tree
        .workspace_roots
        .iter()
        .flat_map(|(_, ids)| ids.iter().copied())
        .collect();
    let hoisted: Vec<usize> = tree
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, n)| n.parent.is_none() && !in_workspace.contains(&(*i as u32)) && n.name == name)
        .map(|(_, n)| n.package.index())
        .collect();
    let by_own_name = (0..graph.packages.len()).filter(|&i| graph.packages[i].name == name);
    let found = own
        .chain(hoisted)
        .chain(by_own_name)
        .find(|&i| matches!(graph.packages.get(i).map(|p| &p.resolution), Some(Resolution::Npm { .. })))?;
    let p = &graph.packages[found];
    let Resolution::Npm {
        version, integrity, ..
    } = &p.resolution
    else {
        return None;
    };
    Some((p.name.clone(), version.clone(), *integrity))
}

/// The client for registry sides, created on first use.
type Client = Option<(crate::project::Project, fnpm_registry::RegistryClient)>;

async fn materialize(
    cx: &Ctx,
    out: &Out,
    client: &mut Client,
    spec: &Spec,
    as_published: bool,
) -> Fail<Tree> {
    match spec {
        Spec::Registry {
            name,
            version,
            locked,
        } => {
            if client.is_none() {
                let project = cx.project_anywhere().map_err(|e| out.error(&e.to_string()))?;
                let c = project.client().map_err(|e| out.error(&e.to_string()))?;
                *client = Some((project, c));
            }
            let (_, c) = client.as_ref().expect("created above");
            fetch_registry_tree(out, c, name, version, *locked).await
        }
        Spec::Tarball(path) => {
            let bytes = std::fs::read(path).map_err(|e| {
                out.named(
                    &pout::errno_name(&e),
                    &format!("failed to read {}", lossy(&defang(path.as_bytes()))),
                )
            })?;
            let mut tree = Tree {
                label: path.clone(),
                ..Default::default()
            };
            read_tar(out, &bytes, &mut tree)?;
            Ok(tree)
        }
        Spec::Dir(path) => read_dir_tree(cx, out, Path::new(path), as_published),
    }
}

/// The bytes a reader keeps in memory at most; a side bigger than this is an error.
const MAX_DIFF_BYTES: u64 = 2 << 30;

fn read_tar(out: &Out, bytes: &[u8], tree: &mut Tree) -> Fail<()> {
    let result = if bytes.starts_with(&[0x1f, 0x8b]) {
        read_tar_stream(flate2::read::GzDecoder::new(bytes), tree, MAX_DIFF_BYTES)
    } else {
        read_tar_stream(bytes, tree, MAX_DIFF_BYTES)
    };
    result.map_err(|message| {
        out.error(&format!(
            "{}: {message}",
            lossy(&defang(tree.label.as_bytes()))
        ))
    })
}

/// Reads every regular file of a tarball. npm tarballs root everything under one folder
/// (usually `package/`), which the parser strips.
fn read_tar_stream(mut input: impl Read, tree: &mut Tree, limit: u64) -> std::result::Result<(), String> {
    let mut parser = TarParser::new();
    let mut current: Option<Vec<u8>> = None;
    let mut total = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    let files = &mut tree.files;
    let modes = &mut tree.modes;
    loop {
        let n = input.read(&mut buf).map_err(|e| format!("invalid tarball: {e}"))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > limit {
            return Err("package archive exceeds the diff size limit".to_owned());
        }
        parser
            .feed(&buf[..n], &mut |event| {
                match event {
                    TarEvent::Entry(header) if header.kind == EntryKind::File => {
                        let path = header.path.into_bytes();
                        modes.insert(path.clone(), header.mode & 0o777);
                        files.insert(path.clone(), Vec::new());
                        current = Some(path);
                    }
                    TarEvent::Data(data) => {
                        if let Some(file) = current.as_ref().and_then(|p| files.get_mut(p)) {
                            file.extend_from_slice(data);
                        }
                    }
                    TarEvent::EntryEnd => current = None,
                    _ => {}
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
    }
    parser.finish().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> Option<u32> {
    None
}

/// A folder. A package (it has a package.json) is read as `fnpm pm pack` would publish it:
/// `files`, .npmignore/.gitignore and bins, not the whole checkout. With `as_published`, its
/// `workspace:` and `catalog:` versions read as pack publishes them.
fn read_dir_tree(cx: &Ctx, out: &Out, root: &Path, as_published: bool) -> Fail<Tree> {
    let shown = root.to_string_lossy().into_owned();
    let mut tree = Tree {
        label: shown.clone(),
        ..Default::default()
    };
    if let Err(e) = std::fs::read_dir(root) {
        return Err(out.named(
            &pout::errno_name(&e),
            &format!("failed to open {}", lossy(&defang(shown.as_bytes()))),
        ));
    }
    // A tree with holes would report those files as deleted, so any read failure is fatal.
    let fail = |e: &std::io::Error, rel: &[u8]| {
        out.named(
            &pout::errno_name(e),
            &format!(
                "failed to read {}/{}",
                lossy(&defang(shown.as_bytes())),
                lossy(&defang(rel))
            ),
        )
    };
    if let Ok(pkg) = std::fs::read(root.join("package.json"))
        && let Ok(text) = std::str::from_utf8(&pkg)
        && let Ok(json) = pj::parse(text)
    {
        // pack refuses a malformed `files`; here that just means "not a package we understand":
        // the whole folder is read.
        let files_ok = json.get("files").is_none_or(|f| {
            matches!(f, Json::Array(items, _) if items.iter().all(|i| matches!(i, Json::String(_))))
        });
        if files_ok {
            let walker = Walker {
                out,
                verbose: false,
            };
            let (queue, _bins) = walker.published_files(root, &json)?;
            for rel in queue {
                let path = root.join(bytes_path(&rel));
                let bytes = std::fs::read(&path).map_err(|e| fail(&e, &rel))?;
                if let Some(mode) = std::fs::metadata(&path).ok().as_ref().and_then(mode_of) {
                    tree.modes.insert(rel.clone(), mode);
                }
                tree.files.insert(rel, bytes);
            }
            let manifest = match as_published
                .then(|| manifests_for(cx, out, root, text))
                .flatten()
            {
                Some(manifests) => with_published_versions(text, &json, &manifests, root).into_bytes(),
                None => pkg,
            };
            tree.files.insert(b"package.json".to_vec(), manifest);
            return Ok(tree);
        }
    }
    let mut stack: Vec<(PathBuf, Vec<u8>)> = vec![(root.to_path_buf(), Vec::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| fail(&e, &prefix))?;
        for entry in entries {
            let entry = entry.map_err(|e| fail(&e, &prefix))?;
            let name = super::pack::walk::os_bytes(&entry.file_name());
            if name == b"node_modules" || name == b".git" {
                continue;
            }
            let mut rel = prefix.clone();
            if !rel.is_empty() {
                rel.push(b'/');
            }
            rel.extend_from_slice(&name);
            let path = entry.path();
            let own = std::fs::symlink_metadata(&path).map_err(|e| fail(&e, &rel))?;
            // A symlink is read through to a file; a dangling one has nothing to show and a
            // linked folder could loop.
            let meta = if own.file_type().is_symlink() {
                match std::fs::metadata(&path) {
                    Ok(meta) => meta,
                    Err(_) => continue,
                }
            } else {
                own.clone()
            };
            if meta.is_dir() {
                if !own.file_type().is_symlink() {
                    stack.push((path, rel));
                }
            } else if meta.is_file() {
                let bytes = std::fs::read(&path).map_err(|e| fail(&e, &rel))?;
                if let Some(mode) = mode_of(&meta) {
                    tree.modes.insert(rel.clone(), mode);
                }
                tree.files.insert(rel, bytes);
            }
        }
    }
    Ok(tree)
}

/// The workspaces and catalogs `fnpm pm pack` in `dir` would resolve the `workspace:` and
/// `catalog:` versions of `manifest` with: those of the project the command runs in, when `dir`
/// is one of its packages.
fn manifests_for(
    cx: &Ctx,
    out: &Out,
    dir: &Path,
    manifest: &str,
) -> Option<super::pack::manifests::WorkspaceManifests> {
    if !manifest.contains("workspace:") && !manifest.contains("catalog:") {
        return None;
    }
    let root = fnpm_workspace::Workspace::find_root(&cx.req.cwd).ok()?;
    // Run outside of a project, there is no package.json to load.
    if !root.join("package.json").is_file() {
        return None;
    }
    super::pack::manifests::WorkspaceManifests::load(out, &root)
        .ok()
        .filter(|m| m.contains_package_dir(dir))
}

/// `manifest` (parsed as `json`) with its `workspace:` and `catalog:` versions as published.
/// Only the version strings change; the rest of the text stays as written.
fn with_published_versions(
    manifest: &str,
    json: &Json,
    manifests: &super::pack::manifests::WorkspaceManifests,
    dir: &Path,
) -> String {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for group in super::pack::manifests::GROUPS {
        let Some(deps) = json.get(group).and_then(Json::as_object) else {
            continue;
        };
        let Some(group_at) = find_key(manifest, 0, group) else {
            continue;
        };
        for (name, value) in deps {
            let Some(spec) = value.as_str() else {
                continue;
            };
            // A version that doesn't resolve is left as written; pack would refuse to publish it.
            let Some(published) = manifests.published_spec(&dir, name, spec) else {
                continue;
            };
            // So is one spelled with JSON escapes.
            let Some(value_at) = find_key(manifest, group_at, name) else {
                continue;
            };
            let token = pj::quote(spec);
            if manifest[value_at..].starts_with(&token) {
                edits.push((value_at, value_at + token.len(), published));
            }
        }
    }
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(manifest.len());
    let mut copied = 0;
    for (at, end, published) in edits {
        if at < copied {
            continue;
        }
        out.push_str(&manifest[copied..at]);
        out.push_str(&pj::quote(&published));
        copied = end;
    }
    out.push_str(&manifest[copied..]);
    out
}

/// Where the value of the first `"key":` at or after `from` starts.
fn find_key(text: &str, from: usize, key: &str) -> Option<usize> {
    let token = pj::quote(key);
    let mut at = from;
    while let Some(found) = text[at..].find(&token) {
        let after = at + found + token.len();
        let rest = &text[after..];
        let trimmed = rest.trim_start();
        if let Some(value) = trimmed.strip_prefix(':') {
            let value = value.trim_start();
            return Some(text.len() - value.len());
        }
        at = after;
    }
    None
}

fn registry_error(client: &fnpm_registry::RegistryClient, name: &str, e: fnpm_registry::Error) -> String {
    use fnpm_registry::Error as E;
    match e {
        E::NotFound(_) => format!(
            "GET {}{} - 404",
            client.registry_url_for(name),
            name.replace('/', "%2f")
        ),
        E::Status { url, status } | E::Registry { url, status, .. } => {
            format!("GET {url} - {status}")
        }
        other => other.to_string(),
    }
}

/// Fetches `name`'s document, resolves `version` (exact, range or dist-tag) like install would,
/// downloads that tarball and reads it in memory.
async fn fetch_registry_tree(
    out: &Out,
    client: &fnpm_registry::RegistryClient,
    name: &str,
    version: &str,
    locked: Option<Integrity>,
) -> Fail<Tree> {
    let doc = client
        .view(name)
        .await
        .map_err(|e| out.error(&lossy(&defang(registry_error(client, name, e).as_bytes()))))?;
    let version = if version.is_empty() { "latest" } else { version };
    let Some(found) = find_by_spec(&doc, version) else {
        let mut text = out
            .error(&format!(
                "no version of {} matches {}",
                lossy(&defang(name.as_bytes())),
                lossy(&defang(version.as_bytes()))
            ))
            .0;
        // What is there instead: the newest releases and the dist-tags.
        let mut releases: Vec<fnpm_semver::Version> = doc
            .get("versions")
            .and_then(Value::as_object)
            .map(|v| {
                v.keys()
                    .filter_map(|k| fnpm_semver::Version::parse(k).ok())
                    .filter(|v| v.pre.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        releases.sort_by(|a, b| b.cmp_precedence(a));
        let recent: Vec<String> = releases.iter().take(5).map(ToString::to_string).collect();
        if !recent.is_empty() {
            let _ = write!(text, "\n{} {}", out.paint_err("2", "recent versions:"), recent.join(", "));
        }
        let tags: Vec<String> = doc
            .get("dist-tags")
            .and_then(Value::as_object)
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| Some(format!("{}: {}", lossy(&defang(k.as_bytes())), v.as_str()?)))
                    .collect()
            })
            .unwrap_or_default();
        if !tags.is_empty() {
            let _ = write!(text, "\n{} {}", out.paint_err("2", "tags:"), tags.join(", "));
        }
        return Err(Fatal(text));
    };
    let label = format!("{name}@{found}");
    let entry = &doc["versions"][&found];
    let registry_entry = serde_json::to_string(entry).ok().and_then(|t| pj::parse(&t).ok());
    let dist = &entry["dist"];
    let tarball = dist["tarball"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| client.default_tarball_url(name, &found));
    let data = client
        .tarball_bytes(&tarball)
        .await
        .map_err(|e| out.error(&lossy(&defang(registry_error(client, name, e).as_bytes()))))?;
    let listed = Integrity::from_dist(dist["integrity"].as_str(), dist["shasum"].as_str());
    for expected in [listed, locked].into_iter().flatten() {
        if !expected.verify(&data) {
            return Err(out.error(&format!(
                "Integrity check failed for tarball: {}",
                lossy(&defang(label.as_bytes()))
            )));
        }
    }
    let mut tree = Tree {
        label,
        registry_entry,
        ..Default::default()
    };
    read_tar(out, &data, &mut tree)?;
    Ok(tree)
}

/// The version `spec` names in a registry document: a dist-tag is looked up only in `dist-tags`;
/// a range takes `latest` when it satisfies it, else the highest match.
fn find_by_spec(doc: &Value, spec: &str) -> Option<String> {
    let versions = doc.get("versions")?.as_object()?;
    let tag = |t: &str| {
        doc.get("dist-tags")
            .and_then(|tags| tags.get(t))
            .and_then(Value::as_str)
            .filter(|v| versions.contains_key(*v))
            .map(str::to_owned)
    };
    if fnpm_types::spec::infer_tag(spec) != fnpm_types::SpecTag::Npm {
        return tag(spec);
    }
    if versions.contains_key(spec) {
        return Some(spec.to_owned());
    }
    let range = fnpm_semver::Range::parse(spec).ok()?;
    if let Some(latest) = tag("latest")
        && fnpm_semver::Version::parse(&latest).is_ok_and(|v| range.satisfies(&v))
    {
        return Some(latest);
    }
    versions
        .keys()
        .filter_map(|v| fnpm_semver::Version::parse(v).ok().map(|parsed| (parsed, v)))
        .filter(|(v, _)| range.satisfies(v))
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(_, s)| s.clone())
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Semantic {
    #[default]
    Text,
    /// Both sides re-print identically.
    FormattingOnly,
    /// JSON shown as its re-print.
    Reprinted,
    /// `-w`: the bytes differ but not once runs of whitespace are collapsed.
    WhitespaceOnly,
    /// CRLF ↔ LF and nothing else.
    LineEndingsOnly,
    /// Same bytes, different permission bits.
    ModeOnly,
}

#[derive(Default)]
struct FileChange<'a> {
    path: &'a [u8],
    semantic: Semantic,
    /// Both sides re-print identically (known in every mode; only the terminal view acts on it).
    formatting_only: bool,
    /// (old, new) permission bits when the executable bits differ, or a new file arrives
    /// executable (old is 0 then).
    mode_change: Option<(u32, u32)>,
    /// Why a file of a re-printed kind was compared as text, for the header.
    not_normalized: Option<&'static str>,
    /// New `import`/`require` specifiers and risky-API counts against the old side.
    new_imports: Vec<String>,
    signal_deltas: Vec<(&'static str, usize)>,
    old: Option<&'a [u8]>,
    new: Option<&'a [u8]>,
    added: usize,
    removed: usize,
    binary: bool,
    /// `*.js.map`: regenerated on every build, never worth reading.
    sourcemap: bool,
    hunks: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Js,
    Css,
    Json,
}

fn kind_for(path: &[u8]) -> Option<Kind> {
    // `.d.ts` carries no statements.
    if path.ends_with(b".d.ts") || path.ends_with(b".d.mts") || path.ends_with(b".d.cts") {
        return None;
    }
    let dot = path.iter().rposition(|&b| b == b'.')?;
    match &path[dot + 1..] {
        b"js" | b"mjs" | b"cjs" | b"jsx" | b"ts" | b"mts" | b"cts" | b"tsx" => Some(Kind::Js),
        b"css" => Some(Kind::Css),
        b"json" => Some(Kind::Json),
        _ => None,
    }
}

/// Files bigger than this are compared as text.
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// The text a file is compared on, and the modules it imports.
struct Norm<'a> {
    text: Cow<'a, [u8]>,
    imports: Vec<String>,
}

/// JSON is re-printed the way bun prints it; JavaScript is kept as written (its imports are read
/// from the text); CSS is not read.
fn normalize<'a>(path: &[u8], bytes: &'a [u8]) -> Option<Norm<'a>> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    match kind_for(path)? {
        Kind::Json => {
            let value = pj::parse(std::str::from_utf8(bytes).ok()?).ok()?;
            Some(Norm {
                text: Cow::Owned(pj::print(&value, pj::Indent::default(), true).into_bytes()),
                imports: Vec::new(),
            })
        }
        Kind::Js => Some(Norm {
            text: Cow::Borrowed(bytes),
            imports: crate::analyze::specifiers(&String::from_utf8_lossy(bytes)),
        }),
        Kind::Css => None,
    }
}

/// Things a reviewer wants called out when they newly appear in a release.
const SIGNALS: &[(&[u8], &str)] = &[
    (b"eval(", "eval()"),
    (b"eval)(", "eval()"),
    (b"Function(", "Function()"),
    (b"process.env", "process.env"),
    (b"fetch(", "fetch()"),
    (b"XMLHttpRequest", "XMLHttpRequest"),
    (b"WebSocket(", "WebSocket"),
    (b"http://", "http:// URL"),
    (b"https://", "https:// URL"),
];

fn count_signals(text: &[u8]) -> [usize; SIGNALS.len()] {
    let mut out = [0usize; SIGNALS.len()];
    for (i, (needle, _)) in SIGNALS.iter().enumerate() {
        let mut at = 0;
        while let Some(pos) = find(&text[at..], needle) {
            let start = at + pos;
            // `refetch(`/`_eval(` are someone's own function, not the global.
            let own = start > 0
                && matches!(text[start - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$' | b'.');
            if !own {
                out[i] += 1;
            }
            at = start + needle.len();
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Node builtins whose first appearance in a package deserves a line in the summary.
fn notable_builtin(spec: &str) -> bool {
    matches!(
        spec.strip_prefix("node:").unwrap_or(spec),
        "child_process" | "fs" | "fs/promises" | "net" | "http" | "https" | "http2" | "dgram"
            | "dns" | "tls" | "vm" | "worker_threads" | "os" | "inspector"
    )
}

fn has_terminal_escapes(bytes: &[u8]) -> bool {
    bytes.contains(&0x1b) || find(bytes, b"\xc2\x9b").is_some()
}

/// U+202A–U+202E, U+2066–U+2069 (embedding/override/isolate): the Trojan Source set.
fn has_bidi_controls(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|w| {
        w[0] == 0xe2
            && ((w[1] == 0x80 && (0xaa..=0xae).contains(&w[2]))
                || (w[1] == 0x81 && (0xa6..=0xa9).contains(&w[2])))
    })
}

/// Makes bytes from a package safe to print to a terminal: C0 and C1 control characters (except
/// tab), bidi overrides and the two Unicode line separators become visible stand-ins.
fn defang(text: &[u8]) -> Cow<'_, [u8]> {
    let suspicious = |b: &u8| (*b < 0x20 && *b != b'\t') || *b == 0x7f || *b == 0xc2 || *b == 0xe2;
    if !text.iter().any(suspicious) {
        return Cow::Borrowed(text);
    }
    let mut out = Vec::with_capacity(text.len() + 16);
    let mut i = 0;
    while i < text.len() {
        let b = text[i];
        if b == 0x1b {
            out.extend_from_slice("␛".as_bytes());
        } else if (b < 0x20 && b != b'\t') || b == 0x7f {
            out.push(b'^');
            out.push(if b == 0x7f { b'?' } else { b + 0x40 });
        } else if b == 0xc2 && text.get(i + 1).is_some_and(|c| (0x80..=0x9f).contains(c)) {
            let _ = std::io::Write::write_fmt(&mut out, format_args!("‹U+{:04X}›", text[i + 1]));
            i += 2;
            continue;
        } else if b == 0xe2
            && i + 2 < text.len()
            && ((text[i + 1] == 0x80 && (0xa8..=0xae).contains(&text[i + 2]))
                || (text[i + 1] == 0x81 && (0xa6..=0xa9).contains(&text[i + 2])))
        {
            let cp = 0x2000 + (u32::from(text[i + 1] & 0x3f) << 6) + u32::from(text[i + 2] & 0x3f);
            let _ = std::io::Write::write_fmt(&mut out, format_args!("‹U+{cp:04X}›"));
            i += 3;
            continue;
        } else {
            out.push(b);
        }
        i += 1;
    }
    Cow::Owned(out)
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Drops a `\r` that ends a line (before `\n` or at end of file); a CR inside a line stays.
fn strip_cr(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        if !(b == b'\r' && bytes.get(i + 1).is_none_or(|&n| n == b'\n')) {
            out.push(b);
        }
    }
    out
}

/// Every run of ASCII whitespace becomes one space and leading/trailing runs go, for `-w`.
fn collapse_whitespace(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut pending_space = false;
    for &b in bytes {
        if b.is_ascii_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(b' ');
                pending_space = false;
            }
            out.push(b);
        }
    }
    out
}

/// Like git, a file with a NUL byte near the start is binary. The exceptions are files that
/// something reads as text: scripts and the file types a JavaScript runtime loads as text.
/// bun also asks its parser whether a file of any other type is JavaScript; fnpm takes UTF-8
/// whose NUL bytes all sit in comments or string literals as such.
fn is_binary(path: &[u8], bytes: &[u8]) -> bool {
    if !bytes[..bytes.len().min(8192)].contains(&0) || bytes.starts_with(b"#!") || path.ends_with(b".sh") {
        return false;
    }
    let base = basename(path);
    let ext = base.iter().rposition(|&b| b == b'.').map(|dot| &base[dot + 1..]);
    match ext {
        Some(b"wasm" | b"node") => true,
        Some(
            b"js" | b"mjs" | b"cjs" | b"jsx" | b"ts" | b"mts" | b"cts" | b"tsx" | b"css" | b"json"
            | b"jsonc" | b"json5" | b"toml" | b"yaml" | b"yml" | b"txt" | b"text" | b"html"
            | b"htm" | b"md" | b"markdown" | b"sql" | b"csv",
        ) => false,
        _ => std::str::from_utf8(bytes).is_err() || !nul_only_in_comments_or_strings(bytes),
    }
}

/// Whether every NUL and other C0 control byte of `bytes` is inside a JavaScript comment or
/// string literal.
fn nul_only_in_comments_or_strings(bytes: &[u8]) -> bool {
    #[derive(PartialEq)]
    enum State {
        Code,
        Line,
        Block,
        Str(u8),
    }
    let mut state = State::Code;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match state {
            State::Code => match b {
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    state = State::Line;
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    state = State::Block;
                    i += 1;
                }
                b'"' | b'\'' | b'`' => state = State::Str(b),
                b if b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) => return false,
                _ => {}
            },
            State::Line if b == b'\n' => state = State::Code,
            State::Block if b == b'*' && bytes.get(i + 1) == Some(&b'/') => {
                state = State::Code;
                i += 1;
            }
            State::Str(_) if b == b'\\' => i += 1,
            State::Str(q) if b == q => state = State::Code,
            _ => {}
        }
        i += 1;
    }
    true
}

fn sha256_hex(bytes: &[u8]) -> String {
    Integrity::compute(HashAlgorithm::Sha256, bytes).to_hex()
}

/// `15 → 74 bytes, sha256 1a2b3c4d5e6f → 0f9e8d7c6b5a`.
fn binary_summary(c: &FileChange<'_>) -> String {
    let hash = |side: Option<&[u8]>| side.map_or_else(|| "none".to_owned(), |b| sha256_hex(b)[..12].to_owned());
    format!(
        "{} → {} bytes, sha256 {} → {}",
        c.old.map_or(0, <[u8]>::len),
        c.new.map_or(0, <[u8]>::len),
        hash(c.old),
        hash(c.new)
    )
}

fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        0
    } else {
        bytes.iter().filter(|&&b| b == b'\n').count() + usize::from(!bytes.ends_with(b"\n"))
    }
}

/// `prefix` + `path` the way a patch header names a file. A name that `git apply` or GNU patch
/// would misread is C-quoted, which both understand.
fn patch_name(prefix: &[u8], path: &[u8]) -> Vec<u8> {
    let needs_quotes = |b: &u8| matches!(*b, 0..=b' ' | 0x7f | b'"' | b'\\');
    if !path.iter().any(needs_quotes) {
        return [prefix, path].concat();
    }
    let mut out = [b"\"".as_slice(), prefix].concat();
    for &b in path {
        match b {
            b'"' | b'\\' => out.extend_from_slice(&[b'\\', b]),
            0..=0x1f | 0x7f => {
                let _ = std::io::Write::write_fmt(&mut out, format_args!("\\{b:03o}"));
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
    out
}

fn print_diff(left: &Tree, right: &Tree, flags: Flags, style: Style) -> Vec<u8> {
    let mut changes: Vec<FileChange> = Vec::new();
    let mut paths: Vec<&[u8]> = left
        .files
        .keys()
        .chain(right.files.keys())
        .map(Vec::as_slice)
        .collect();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        let old = left.files.get(path).map(Vec::as_slice);
        let new = right.files.get(path).map(Vec::as_slice);
        let mode_change = match (left.modes.get(path).copied(), right.modes.get(path).copied()) {
            (Some(o), Some(n)) if (o ^ n) & 0o111 != 0 => Some((o, n)),
            (None, Some(n)) if old.is_none() && n & 0o111 != 0 => Some((0, n)),
            _ => None,
        };
        if old == new && mode_change.is_none() {
            continue;
        }
        let mut change = FileChange {
            path,
            old,
            new,
            mode_change,
            ..Default::default()
        };
        if old == new {
            change.semantic = Semantic::ModeOnly;
            changes.push(change);
            continue;
        }
        change.binary = [old, new].iter().flatten().any(|side| is_binary(path, side));
        // Because of the colon, this prefix can only start JSON, never a JavaScript program.
        change.sourcemap =
            path.ends_with(b".map") && new.or(old).is_some_and(|b| b.starts_with(b"{\"version\":"));
        if change.binary || change.sourcemap {
            changes.push(change);
            continue;
        }
        if style.pretty
            && let (Some(o), Some(n)) = (old, new)
        {
            if (o.contains(&b'\r') || n.contains(&b'\r')) && strip_cr(o) == strip_cr(n) {
                change.semantic = Semantic::LineEndingsOnly;
                changes.push(change);
                continue;
            }
            // Like `formatting only`, a terminal-view summary; piped output stays a complete patch.
            if flags.ignore_space && collapse_whitespace(o) == collapse_whitespace(n) {
                change.semantic = Semantic::WhitespaceOnly;
                changes.push(change);
                continue;
            }
        }
        let (norm_old, norm_new) = if flags.raw {
            (None, None)
        } else {
            (
                old.and_then(|b| normalize(path, b)),
                new.and_then(|b| normalize(path, b)),
            )
        };
        let kind = kind_for(path);
        if !flags.raw && kind.is_some() {
            let too_big = |b: Option<&[u8]>| b.is_some_and(|b| b.len() > MAX_BYTES);
            if too_big(old) || too_big(new) {
                change.not_normalized = Some("too large to normalize");
            } else if kind == Some(Kind::Json)
                && ((old.is_some() && norm_old.is_none()) || (new.is_some() && norm_new.is_none()))
            {
                change.not_normalized = Some("not parsed");
            }
        }
        if let Some(n) = &norm_new {
            let old_imports: &[String] = norm_old.as_ref().map_or(&[], |o| o.imports.as_slice());
            for imp in &n.imports {
                if !old_imports.contains(imp) && !change.new_imports.contains(imp) {
                    change.new_imports.push(imp.clone());
                }
            }
            let before = norm_old
                .as_ref()
                .map_or([0; SIGNALS.len()], |o| count_signals(&o.text));
            let after = count_signals(&n.text);
            for (i, (_, label)) in SIGNALS.iter().enumerate() {
                if after[i] > before[i] {
                    change.signal_deltas.push((label, after[i] - before[i]));
                }
            }
        }
        let js_family = kind == Some(Kind::Js);
        let reprint_equal = !js_family
            && matches!((&norm_old, &norm_new), (Some(a), Some(b)) if a.text == b.text);
        change.formatting_only = reprint_equal;
        match (old, new, &norm_old, &norm_new) {
            (Some(o), Some(n), _, _) if js_family => {
                unified_hunks(o, n, flags.context, style, &mut change)
            }
            (Some(_), Some(_), Some(_), Some(_)) if style.pretty && reprint_equal => {
                // JSON: an identical re-print is formatting.
                change.semantic = Semantic::FormattingOnly;
            }
            (Some(_), Some(_), Some(no), Some(nn)) if style.pretty => {
                // JSON with a real change: the re-prints are what is compared and shown, so a
                // reformat around one edit is one edit.
                change.semantic = Semantic::Reprinted;
                unified_hunks(&no.text, &nn.text, flags.context, style, &mut change);
            }
            (None, Some(n), _, _) => change.added = count_lines(n),
            (Some(o), None, _, _) => change.removed = count_lines(o),
            (Some(o), Some(n), _, _) => unified_hunks(o, n, flags.context, style, &mut change),
            (None, None, _, _) => unreachable!("a path comes from one of the sides"),
        }
        if !flags.name_only
            && !flags.stat
            && change.hunks.is_empty()
            && change.semantic == Semantic::Text
            && (old.is_none() || new.is_none())
        {
            // A whole file added or removed: every line as + or -.
            let (op, body) = match (new, old) {
                (Some(n), _) => (Op::Insert, n),
                (None, Some(o)) => (Op::Delete, o),
                (None, None) => unreachable!("a path comes from one of the sides"),
            };
            let n = count_lines(body);
            // An empty file has no lines, and a hunk header without lines corrupts the patch.
            if n > 0 {
                let range = if new.is_some() { (0, 0, 1, n) } else { (1, n, 0, 0) };
                style.hunk_header(&mut change.hunks, range, true);
            }
            for (i, line) in Lines(body).enumerate() {
                style.line(&mut change.hunks, op, i + 1, i + 1, line, None);
            }
            if !style.pretty && !body.is_empty() && !body.ends_with(b"\n") {
                change.hunks.extend_from_slice(NO_NEWLINE_MARKER);
            }
        }
        changes.push(change);
    }

    let mut o: Vec<u8> = Vec::new();
    if flags.json {
        print_json(&mut o, left, right, &changes, flags);
        return o;
    }
    if style.pretty {
        o.push(b'\n');
    }
    print_header(&mut o, left, right, style);
    if changes.is_empty() {
        writeln_bytes(
            &mut o,
            &tags(&format!("<d>No differences ({} files)<r>", left.files.len()), style.pretty),
        );
        return o;
    }
    if !style.pretty || flags.name_only || flags.stat {
        o.push(b'\n');
    }
    let path_width = changes
        .iter()
        .map(|c| char_len(&defang(c.path)))
        .max()
        .unwrap_or(0)
        .min(60);
    let max_delta = changes
        .iter()
        .map(|c| c.added + c.removed)
        .max()
        .unwrap_or(1)
        .max(1);
    for c in &changes {
        let status = match (c.old, c.new) {
            (None, Some(_)) => "A",
            (Some(_), None) => "D",
            _ => "M",
        };
        if flags.name_only {
            o.extend_from_slice(status.as_bytes());
            o.push(b' ');
            o.extend_from_slice(&defang(c.path));
            o.push(b'\n');
            continue;
        }
        if flags.stat {
            // Bars scale to the biggest change so a 2-line edit next to a 500-line one still shows.
            let width = 40usize;
            let scale = |n: usize| {
                if max_delta <= width {
                    n
                } else {
                    (n * width).div_ceil(max_delta)
                }
            };
            let shown = lossy(&defang(c.path));
            let pad = path_width.saturating_sub(shown.chars().count());
            let padded = format!("{shown}{}", " ".repeat(pad));
            let sep = if style.pretty { "│" } else { "|" };
            let line = match c.semantic {
                Semantic::FormattingOnly
                | Semantic::WhitespaceOnly
                | Semantic::LineEndingsOnly
                | Semantic::ModeOnly => {
                    let what = match c.semantic {
                        Semantic::FormattingOnly => "formatting only".to_owned(),
                        Semantic::WhitespaceOnly => "whitespace only".to_owned(),
                        Semantic::LineEndingsOnly => "line endings only".to_owned(),
                        _ => c
                            .mode_change
                            .map_or(String::new(), |(o, n)| format!("mode {o:o} → {n:o}")),
                    };
                    format!(" {padded} <d>{sep}<r> <d>{what}<r>")
                }
                _ if c.binary || c.sourcemap => format!(
                    " {padded} <d>{sep}<r> <yellow>{}<r>   {} → {} bytes",
                    if c.sourcemap { "map" } else { "bin" },
                    c.old.map_or(0, <[u8]>::len),
                    c.new.map_or(0, <[u8]>::len)
                ),
                _ => format!(
                    " {padded} <d>{sep}<r> {:>5} <green>{}<r><red>{}<r>",
                    c.added + c.removed,
                    "+".repeat(scale(c.added)),
                    "-".repeat(scale(c.removed))
                ),
            };
            writeln_bytes(&mut o, &tags_keep(&line, style.pretty, &padded));
            continue;
        }
        if style.pretty {
            style.file_header(&mut o, c);
            o.extend_from_slice(&c.hunks);
            continue;
        }
        // A file whose contents the patch omits gets neither a `diff --git` header nor a mode
        // line, because GNU patch would apply the mode line to the previous file.
        let in_patch = !c.binary && !c.sourcemap;
        let header: &[u8] = if in_patch { b"diff --git " } else { b"diff --fnpm " };
        let (mut a, mut b) = (patch_name(b"a/", c.path), patch_name(b"b/", c.path));
        if style.escape {
            (a, b) = (defang(&a).into_owned(), defang(&b).into_owned());
        }
        for part in [header, &a, b" ", &b, b"\n"] {
            o.extend_from_slice(part);
        }
        match (c.old, c.new, c.mode_change) {
            (None, Some(_), _) if !in_patch => o.extend_from_slice(b"new file\n"),
            (Some(_), None, _) if !in_patch => o.extend_from_slice(b"deleted file\n"),
            (None, Some(_), mode) => {
                writeln_bytes(&mut o, &format!("new file mode 100{:o}", mode.map_or(0o644, |(_, n)| n)));
            }
            (Some(_), None, _) => {
                let mode = left.modes.get(c.path).copied().unwrap_or(0);
                writeln_bytes(
                    &mut o,
                    &format!("deleted file mode 100{}", if mode & 0o111 != 0 { 755 } else { 644 }),
                );
            }
            (_, _, Some((old_mode, new_mode))) if in_patch => {
                writeln_bytes(&mut o, &format!("old mode 100{old_mode:o}\nnew mode 100{new_mode:o}"));
            }
            _ => {}
        }
        if c.semantic == Semantic::ModeOnly {
            continue;
        }
        if c.sourcemap {
            writeln_bytes(
                &mut o,
                &format!(
                    "Source map files differ ({} → {} bytes)",
                    c.old.map_or(0, <[u8]>::len),
                    c.new.map_or(0, <[u8]>::len)
                ),
            );
            continue;
        }
        if c.binary {
            writeln_bytes(&mut o, &format!("Binary files differ ({})", binary_summary(c)));
            continue;
        }
        // An added or removed empty file is written the way git writes it: an `index` line with
        // the null object ID and the empty blob's ID, and no hunk.
        if c.hunks.is_empty() {
            writeln_bytes(
                &mut o,
                if c.new.is_some() { "index 0000000..e69de29" } else { "index e69de29..0000000" },
            );
            continue;
        }
        let old_name: &[u8] = if c.old.is_some() { &a } else { b"/dev/null" };
        let new_name: &[u8] = if c.new.is_some() { &b } else { b"/dev/null" };
        for part in [b"--- ".as_slice(), old_name, b"\n+++ ", new_name, b"\n", &c.hunks] {
            o.extend_from_slice(part);
        }
    }
    print_summary(&mut o, left, right, &changes, style);
    o
}

fn writeln_bytes(o: &mut Vec<u8>, text: &str) {
    o.extend_from_slice(text.as_bytes());
    o.push(b'\n');
}

fn char_len(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count()
}

/// bun's colour tags (`<b>`, `<d>`, `<r>`, `<red>`…) as SGR codes, or removed.
fn tags(template: &str, colors: bool) -> String {
    const TAGS: [(&str, &str); 9] = [
        ("<r>", "\x1b[0m"),
        ("<b>", "\x1b[1m"),
        ("<d>", "\x1b[2m"),
        ("<red>", "\x1b[31m"),
        ("<green>", "\x1b[32m"),
        ("<yellow>", "\x1b[33m"),
        ("<blue>", "\x1b[34m"),
        ("<magenta>", "\x1b[35m"),
        ("<cyan>", "\x1b[36m"),
    ];
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    'outer: while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        for (tag, code) in TAGS {
            if let Some(after) = rest.strip_prefix(tag) {
                if colors {
                    out.push_str(code);
                }
                rest = after;
                continue 'outer;
            }
        }
        out.push('<');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

/// [`tags`] for a template that contains `literal` (text from a package), which is not markup.
fn tags_keep(template: &str, colors: bool, literal: &str) -> String {
    match template.find(literal) {
        Some(at) if !literal.is_empty() => format!(
            "{}{literal}{}",
            tags(&template[..at], colors),
            tags(&template[at + literal.len()..], colors)
        ),
        _ => tags(template, colors),
    }
}

/// Renders a note template's colour tags and only then drops the (untrusted, defanged) args into
/// its `{}` slots, so package.json text is never read as markup.
fn note(colors: bool, template: &str, args: &[String]) -> String {
    let rendered = tags(template, colors);
    let mut out = String::with_capacity(rendered.len() + 32);
    let mut args = args.iter();
    let mut rest = rendered.as_str();
    while let Some(at) = rest.find("{}") {
        out.push_str(&rest[..at]);
        if let Some(a) = args.next() {
            out.push_str(&lossy(&defang(a.as_bytes())));
        }
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

/// `name@version` → (name, version), minding a leading scope `@`.
fn split_label(label: &str) -> Option<(&str, &str)> {
    let at = label.rfind('@')?;
    (at > 0).then(|| (&label[..at], &label[at + 1..]))
}

/// `react 18.2.0 → 19.0.0` when both sides are the same package, the two labels otherwise.
fn print_header(o: &mut Vec<u8>, left: &Tree, right: &Tree, style: Style) {
    let d = |s: &str| lossy(&defang(s.as_bytes()));
    let line = match (split_label(&left.label), split_label(&right.label)) {
        (Some((ln, lv)), Some((rn, rv))) if style.pretty && ln == rn => format!(
            "{}{}{} {}{}{} {} {}{}{}",
            "\x1b[1m",
            d(ln),
            "\x1b[0m",
            "\x1b[31m",
            d(lv),
            "\x1b[0m",
            "\x1b[2m→\x1b[0m",
            "\x1b[32m",
            d(rv),
            "\x1b[0m"
        ),
        _ if style.pretty => format!(
            "\x1b[1m{}\x1b[0m \x1b[2m→\x1b[0m \x1b[1m{}\x1b[0m",
            d(&left.label),
            d(&right.label)
        ),
        _ => format!("{} → {}", d(&left.label), d(&right.label)),
    };
    writeln_bytes(o, &line);
}

/// After the hunks: the end of the output is what stays on screen, so the totals and the changes
/// worth a second look go there.
fn print_summary(o: &mut Vec<u8>, left: &Tree, right: &Tree, changes: &[FileChange<'_>], style: Style) {
    let (mut files_added, mut files_removed, mut files_changed, mut lines_added, mut lines_removed) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for c in changes {
        match (c.old, c.new) {
            (None, Some(_)) => files_added += 1,
            (Some(_), None) => files_removed += 1,
            _ => files_changed += 1,
        }
        lines_added += c.added;
        lines_removed += c.removed;
    }
    o.push(b'\n');
    print_header(o, left, right, style);
    if style.pretty {
        let mut line = format!(
            "<d>{} file{}<r>  <green>+{}<r> <red>-{}<r>",
            changes.len(),
            if changes.len() == 1 { "" } else { "s" },
            lines_added,
            lines_removed
        );
        if files_added > 0 {
            let _ = write!(line, "  <d>·<r>  <green>{files_added} new<r>");
        }
        if files_removed > 0 {
            let _ = write!(line, "  <d>·<r>  <red>{files_removed} deleted<r>");
        }
        for (semantic, what) in [
            (Semantic::FormattingOnly, "formatting only"),
            (Semantic::WhitespaceOnly, "whitespace only"),
            (Semantic::LineEndingsOnly, "line endings only"),
        ] {
            let n = changes.iter().filter(|c| c.semantic == semantic).count();
            if n > 0 {
                let _ = write!(line, "  <d>·  {n} {what}<r>");
            }
        }
        writeln_bytes(o, &tags(&line, true));
    } else {
        writeln_bytes(
            o,
            &format!(
                "{} file{} changed, {} added, {} removed  (+{} -{} lines)",
                files_changed,
                if files_changed == 1 { "" } else { "s" },
                files_added,
                files_removed,
                lines_added,
                lines_removed
            ),
        );
    }
    let mark = if style.pretty { "  \x1b[33m▲\x1b[0m " } else { "  ! " };
    for n in collect_notes(left, right, changes, style.pretty) {
        writeln_bytes(o, &format!("{mark}{n}"));
    }
}

/// The changes worth a second look, as colour-tagged strings (package text already escaped).
fn collect_notes(left: &Tree, right: &Tree, changes: &[FileChange<'_>], colors: bool) -> Vec<String> {
    let mut notes = Vec::new();
    package_json_notes(left, right, &mut notes, colors);
    import_notes(changes, &mut notes, colors);
    for c in changes {
        let path = lossy(c.path);
        match c.mode_change {
            Some((0, n)) => notes.push(note(
                colors,
                "new executable file <b>{}<r> <d>({})<r>",
                &[path.clone(), format!("{n:o}")],
            )),
            Some((o, n)) if n & 0o111 != 0 => notes.push(note(
                colors,
                "now executable: <b>{}<r> <d>({} → {})<r>",
                &[path.clone(), format!("{o:o}"), format!("{n:o}")],
            )),
            _ => {}
        }
        if let Some(n) = c.new {
            let gained = |needle: fn(&[u8]) -> bool| needle(n) && !c.old.is_some_and(needle);
            if !c.binary && gained(has_terminal_escapes) {
                notes.push(note(
                    colors,
                    "terminal escape sequences in <b>{}<r> <d>(shown as ␛)<r>",
                    std::slice::from_ref(&path),
                ));
            }
            if !c.binary && gained(has_bidi_controls) {
                notes.push(note(
                    colors,
                    "bidirectional text controls in <b>{}<r> <d>(Trojan Source; shown as ‹U+202E›)<r>",
                    std::slice::from_ref(&path),
                ));
            }
        }
        if c.binary && c.old.is_none() {
            notes.push(note(
                colors,
                "new binary file <b>{}<r> ({} bytes)",
                &[path.clone(), c.new.map_or(0, <[u8]>::len).to_string()],
            ));
        }
        if c.new.is_some_and(|new| is_binary(c.path, new))
            && c.old.is_some_and(|old| !is_binary(c.path, old))
        {
            notes.push(note(
                colors,
                "<b>{}<r> was text and is now binary",
                std::slice::from_ref(&path),
            ));
        }
        if c.old.is_none()
            && (c.path.ends_with(b".node") || c.path.ends_with(b".sh") || c.path.ends_with(b".exe"))
            && !c.binary
        {
            notes.push(note(
                colors,
                "new executable-looking file <b>{}<r>",
                std::slice::from_ref(&path),
            ));
        }
    }
    notes
}

/// New imports of consequential builtins, other new module specifiers, and growth in risky API
/// use, aggregated across files.
fn import_notes(changes: &[FileChange<'_>], notes: &mut Vec<String>, colors: bool) {
    let mut builtins: Vec<(&str, &[u8])> = Vec::new();
    let mut packages: Vec<&str> = Vec::new();
    for c in changes {
        for imp in &c.new_imports {
            if notable_builtin(imp) {
                if !builtins.iter().any(|(b, _)| b == imp) {
                    builtins.push((imp, c.path));
                }
            } else if !imp.starts_with(['.', '/']) && !packages.contains(&imp.as_str()) {
                packages.push(imp);
            }
        }
    }
    for (b, path) in &builtins {
        notes.push(note(
            colors,
            "now imports <b><magenta>{}<r> <d>({})<r>",
            &[(*b).to_owned(), lossy(path)],
        ));
    }
    if !packages.is_empty() {
        let shown: Vec<String> = packages
            .iter()
            .take(6)
            .map(|p| note(colors, "<b>{}<r>", &[(*p).to_owned()]))
            .collect();
        let more = if packages.len() > 6 {
            note(colors, " <d>… and {} more<r>", &[(packages.len() - 6).to_string()])
        } else {
            String::new()
        };
        notes.push(format!("new module imports: {}{more}", shown.join(", ")));
    }
    let mut totals: Vec<(&'static str, usize, &[u8])> = Vec::new();
    for c in changes {
        for &(label, n) in &c.signal_deltas {
            match totals.iter_mut().find(|(l, _, _)| *l == label) {
                Some(t) => t.1 += n,
                None => totals.push((label, n, c.path)),
            }
        }
    }
    for (label, n, first_path) in totals {
        // URLs churn in license headers and doc strings; only call out the sharper tools.
        if label.ends_with("URL") && n < 3 {
            continue;
        }
        notes.push(note(
            colors,
            "<b>+{}<r> {} <d>({}{})<r>",
            &[
                n.to_string(),
                label.to_owned(),
                lossy(first_path),
                if n > 1 { ", …" } else { "" }.to_owned(),
            ],
        ));
    }
}

/// The fields `fnpm install` reads from the registry document's entry for a version, not from
/// the package.json in the tarball.
const INSTALL_FIELDS: [&str; 8] = [
    "dependencies",
    "optionalDependencies",
    "peerDependencies",
    "bin",
    "directories",
    "os",
    "cpu",
    "libc",
];

/// One side's package.json, each field read from wherever `fnpm install` reads it.
struct Manifest {
    file: Json,
    /// The registry document's entry for this version.
    entry: Option<Json>,
    /// The fields of `entry` whose values differ from those in `file`.
    overridden: Vec<&'static str>,
}

impl Manifest {
    fn get(&self, field: &str) -> Option<&Json> {
        match &self.entry {
            Some(entry) if self.overridden.contains(&field) => entry.get(field),
            _ => self.file.get(field),
        }
    }
}

/// The properties of the object `e` whose values are strings.
fn string_pairs(e: Option<&Json>) -> Vec<(String, String)> {
    e.and_then(Json::as_object)
        .map(|props| {
            props
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// `/` + `path` with `.` and `..` resolved.
fn normal_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

/// One of `INSTALL_FIELDS` as sorted pairs, normalized so that two spellings that install the
/// same thing compare equal. A registry rewrites some fields when a package is published.
fn install_view(json: &Json, field: &str) -> Vec<(String, String)> {
    let Some(value) = json.get(field) else {
        return Vec::new();
    };
    let mut pairs = string_pairs(Some(value));
    match field {
        "bin" => {
            // `"bin": "./cli.js"` is short for `{ "<name without its scope>": "cli.js" }`.
            if let (Some(path), Some(name)) = (value.as_str(), json.get("name").and_then(Json::as_str)) {
                pairs.push((name.to_owned(), path.to_owned()));
            }
            for (name, path) in &mut pairs {
                *name = name.rsplit('/').next().unwrap_or(name).to_owned();
                *path = normal_path(path);
            }
        }
        "directories" => pairs.retain(|(name, _)| name == "bin"),
        "os" | "cpu" | "libc" => {
            if let Some(only) = value.as_str() {
                pairs.push((only.to_owned(), String::new()));
            }
            if let Json::Array(items, _) = value {
                pairs.extend(items.iter().filter_map(|i| Some((i.as_str()?.to_owned(), String::new()))));
            }
        }
        // A package listed in both fields is optional, and some registry entries repeat the
        // optional dependencies under `dependencies`.
        "dependencies" => {
            let optional = install_view(json, "optionalDependencies");
            pairs.retain(|(name, _)| !optional.iter().any(|(other, _)| other == name));
        }
        _ => {}
    }
    pairs.sort_unstable();
    pairs
}

/// The properties of the top-level object of `text`, duplicates included, in order: each key
/// and the byte range of its value.
fn top_level_props(text: &[u8]) -> Vec<(String, std::ops::Range<usize>)> {
    let mut props = Vec::new();
    let mut open: Option<(String, usize)> = None;
    let (mut depth, mut i) = (0usize, 0usize);
    let mut close = |open: &mut Option<(String, usize)>, end: usize| {
        if let Some((key, start)) = open.take() {
            let value = &text[start..end];
            let trimmed = value.len() - value.iter().rev().take_while(|b| b.is_ascii_whitespace()).count();
            props.push((key, start..start + trimmed));
        }
    };
    while i < text.len() {
        match text[i] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                if depth == 1 {
                    close(&mut open, i);
                }
                depth = depth.saturating_sub(1);
            }
            b',' if depth == 1 => close(&mut open, i),
            b'"' => {
                let start = i;
                i += 1;
                while i < text.len() && text[i] != b'"' {
                    i += if text[i] == b'\\' { 2 } else { 1 };
                }
                let end = (i + 1).min(text.len());
                if depth == 1 && open.is_none() {
                    let rest = &text[end..];
                    let colon = rest.iter().position(|b| !b.is_ascii_whitespace());
                    if let Some(colon) = colon.filter(|&n| rest[n] == b':')
                        && let Ok(key) = serde_json::from_slice::<String>(&text[start..end])
                    {
                        let after = end + colon + 1;
                        let ws = text[after..].iter().take_while(|b| b.is_ascii_whitespace()).count();
                        open = Some((key, after + ws));
                        i = after + ws;
                        continue;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    props
}

/// package.json as bun's parser reads it for these notes: of two equal top-level keys, the first.
fn parse_first_wins(bytes: &[u8]) -> Option<Json> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut json = pj::parse(text).ok()?;
    let props = top_level_props(bytes);
    for (i, (key, range)) in props.iter().enumerate() {
        if props[..i].iter().any(|(k, _)| k == key) || !props[i + 1..].iter().any(|(k, _)| k == key) {
            continue;
        }
        if let (Ok(first), Some(slot)) = (pj::parse(&text[range.clone()]), json.get_mut(key)) {
            *slot = first;
        }
    }
    Some(json)
}

fn package_json_notes(left: &Tree, right: &Tree, notes: &mut Vec<String>, colors: bool) {
    let mut side = |tree: &Tree| -> Option<Manifest> {
        let file = parse_first_wins(tree.files.get(b"package.json".as_slice())?)?;
        let entry = tree.registry_entry.clone();
        let mut overridden = Vec::new();
        if let Some(entry) = &entry {
            for field in INSTALL_FIELDS {
                // The abbreviated registry document omits `libc`.
                if (field == "libc" && entry.get(field).is_none())
                    || install_view(entry, field) == install_view(&file, field)
                {
                    continue;
                }
                overridden.push(field);
                let text = |json: &Json| json.get(field).map_or_else(|| "nothing".to_owned(), expr_text);
                let (theirs, ours) = (text(entry), text(&file));
                let at = first_difference(theirs.as_bytes(), ours.as_bytes());
                notes.push(note(
                    colors,
                    "registry and package.json of {} disagree on <b>{}<r>: {} <d>vs<r> {} <d>(fnpm install uses the registry's)<r>",
                    &[tree.label.clone(), field.to_owned(), clip(&theirs, at), clip(&ours, at)],
                ));
            }
        }
        Some(Manifest {
            file,
            entry,
            overridden,
        })
    };
    let (Some(old), Some(new)) = (side(left), side(right)) else {
        return;
    };
    // When a key appears twice, which value wins depends on the tool that reads the file.
    if let Some(bytes) = right.files.get(b"package.json".as_slice()) {
        let keys: Vec<String> = top_level_props(bytes).into_iter().map(|(k, _)| k).collect();
        for (i, key) in keys.iter().enumerate() {
            if keys[..i].contains(key) {
                notes.push(note(
                    colors,
                    "<b>{}<r> listed more than once in package.json",
                    std::slice::from_ref(key),
                ));
            }
        }
    }
    const SCRIPT_KEYS: [&str; 7] = [
        "preinstall",
        "install",
        "postinstall",
        "prepare",
        "prepublish",
        "preuninstall",
        "postuninstall",
    ];
    let script = |m: &Manifest, key: &str| {
        m.file
            .get("scripts")
            .and_then(|s| s.get(key))
            .and_then(Json::as_str)
            .map(str::to_owned)
    };
    for key in SCRIPT_KEYS {
        match (script(&old, key), script(&new, key)) {
            (None, Some(n)) => notes.push(note(
                colors,
                "<b>{}<r> script added: <cyan>{}<r>",
                &[key.to_owned(), n],
            )),
            (Some(o), Some(n)) if o != n => notes.push(note(
                colors,
                "<b>{}<r> script changed: <cyan>{}<r>",
                &[key.to_owned(), n],
            )),
            _ => {}
        }
    }
    for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
        let o = string_pairs(old.get(field));
        let n = string_pairs(new.get(field));
        // A version bump goes from one range to another. A range that becomes a URL, a
        // repository or an alias of another package is not a bump.
        let is_range = |version: &str| {
            fnpm_types::spec::infer_tag(version) == fnpm_types::SpecTag::Npm
                && !version.starts_with("npm:")
        };
        let mut bumps: Vec<String> = Vec::new();
        for (name, ver) in &n {
            match o.iter().find(|(on, _)| on == name) {
                None => notes.push(note(
                    colors,
                    "{} added: <b>{}<r>@{}",
                    &[field.to_owned(), name.clone(), ver.clone()],
                )),
                Some((_, ov)) if ov != ver => {
                    let line = note(
                        colors,
                        "{} <b>{}<r>: {} → {}",
                        &[field.to_owned(), name.clone(), ov.clone(), ver.clone()],
                    );
                    if is_range(ov) && is_range(ver) {
                        bumps.push(line);
                    } else {
                        notes.push(line);
                    }
                }
                _ => {}
            }
        }
        // A release that bumps a family of packages in lockstep is one fact, not thirty.
        if bumps.len() > 4 {
            let rest = bumps.len() - 3;
            bumps.truncate(3);
            bumps.push(note(
                colors,
                "<d>… and {} more {} version changes<r>",
                &[rest.to_string(), field.to_owned()],
            ));
        }
        notes.append(&mut bumps);
        for (name, _) in &o {
            if !n.iter().any(|(nn, _)| nn == name) {
                notes.push(note(
                    colors,
                    "{} removed: <b>{}<r>",
                    &[field.to_owned(), name.clone()],
                ));
            }
        }
    }
    for field in [
        "main",
        "module",
        "types",
        "bin",
        "exports",
        "engines",
        "license",
        "type",
        "browser",
        "imports",
        "directories",
        "os",
        "cpu",
        "libc",
        "bundleDependencies",
        "bundledDependencies",
    ] {
        // Compare the full values; only the text shown in a note is cut short. The values
        // `fnpm install` uses come first, then the package.json files, which other tools read.
        let differ = |(o, n): &(Option<&Json>, Option<&Json>)| o.map(compact) != n.map(compact);
        let Some((o, n)) = [
            (old.get(field), new.get(field)),
            (old.file.get(field), new.file.get(field)),
        ]
        .into_iter()
        .find(differ) else {
            continue;
        };
        match (o, n) {
            (Some(o), Some(n)) => {
                let (o, n) = (expr_text(o), expr_text(n));
                let at = first_difference(o.as_bytes(), n.as_bytes());
                notes.push(note(
                    colors,
                    "<b>{}<r> changed: {} → {}",
                    &[field.to_owned(), clip(&o, at), clip(&n, at)],
                ));
            }
            (None, Some(n)) => notes.push(note(
                colors,
                "<b>{}<r> added: {}",
                &[field.to_owned(), clip(&expr_text(n), 0)],
            )),
            (Some(_), None) => notes.push(note(colors, "<b>{}<r> removed", &[field.to_owned()])),
            (None, None) => {}
        }
    }
}

/// `value` as JSON without whitespace, so the text does not depend on how the file was formatted.
fn compact(value: &Json) -> String {
    let mut out = String::new();
    compact_into(&mut out, value);
    out
}

fn compact_into(out: &mut String, value: &Json) {
    match value {
        Json::Array(items, _) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                compact_into(out, item);
            }
            out.push(']');
        }
        Json::Object(props) => {
            out.push('{');
            for (i, (k, v)) in props.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&pj::quote(k));
                out.push(':');
                compact_into(out, v);
            }
            out.push('}');
        }
        scalar => out.push_str(&pj::print(scalar, pj::Indent::default(), false)),
    }
}

/// `value` on one line, for a note: a string as it is, anything else as printed JSON with its
/// indentation collapsed.
fn expr_text(value: &Json) -> String {
    if let Some(s) = value.as_str() {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut last_space = false;
    for c in pj::print(value, pj::Indent::default(), false).chars() {
        let ws = matches!(c, '\n' | ' ' | '\t');
        if !(ws && last_space) {
            out.push(if ws { ' ' } else { c });
        }
        last_space = ws;
    }
    out
}

fn first_difference(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(a, b)| a == b).count()
}

/// At most about 120 bytes of `text`, in a window that includes the byte at `at`.
fn clip(text: &str, at: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= 120 {
        return text.to_owned();
    }
    let boundary = |mut i: usize| {
        while i > 0 && i < bytes.len() && !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let start = if at < 100 { 0 } else { boundary(at - 40) };
    let end = boundary((start + 117).min(bytes.len()));
    format!(
        "{}{}{}",
        if start > 0 { "..." } else { "" },
        &text[start..end],
        if end < bytes.len() { "..." } else { "" }
    )
}

/// `--json`: one document with both labels, per-file status/counts/patch text, the notes as
/// plain strings, and totals.
fn print_json(o: &mut Vec<u8>, left: &Tree, right: &Tree, changes: &[FileChange<'_>], flags: Flags) {
    let js = |s: &[u8]| serde_json::to_string(&String::from_utf8_lossy(s)).unwrap_or_default();
    let mut out = format!(
        "{{\n  \"from\": {},\n  \"to\": {},\n  \"files\": [",
        js(left.label.as_bytes()),
        js(right.label.as_bytes())
    );
    let (mut lines_added, mut lines_removed, mut added, mut deleted) = (0usize, 0usize, 0usize, 0usize);
    for (i, c) in changes.iter().enumerate() {
        let status = match (c.old, c.new) {
            (None, Some(_)) => {
                added += 1;
                "added"
            }
            (Some(_), None) => {
                deleted += 1;
                "deleted"
            }
            _ => "modified",
        };
        lines_added += c.added;
        lines_removed += c.removed;
        let _ = write!(
            out,
            "{}\n    {{ \"path\": {}, \"status\": \"{}\", \"binary\": {}, \"sourceMap\": {}, \"formattingOnly\": {}, \"linesAdded\": {}, \"linesRemoved\": {}, \"bytesBefore\": {}, \"bytesAfter\": {}{}",
            if i == 0 { "" } else { "," },
            js(c.path),
            status,
            c.binary,
            c.sourcemap,
            c.formatting_only,
            c.added,
            c.removed,
            c.old.map_or(0, <[u8]>::len),
            c.new.map_or(0, <[u8]>::len),
            c.mode_change.map_or(String::new(), |(o, n)| format!(
                ", \"modeBefore\": \"{o:o}\", \"modeAfter\": \"{n:o}\""
            )),
        );
        if !flags.name_only && !flags.stat && !c.binary && !c.sourcemap {
            let _ = write!(out, ", \"patch\": {}", js(&c.hunks));
        }
        if c.binary {
            for (key, side) in [("sha256Before", c.old), ("sha256After", c.new)] {
                if let Some(bytes) = side {
                    let _ = write!(out, ", \"{key}\": \"{}\"", sha256_hex(bytes));
                }
            }
        }
        out.push_str(" }");
    }
    let _ = write!(
        out,
        "{}],\n  \"notes\": [",
        if changes.is_empty() { "" } else { "\n  " }
    );
    let notes: Vec<String> = collect_notes(left, right, changes, false)
        .iter()
        .map(|n| js(n.as_bytes()))
        .collect();
    out.push_str(&notes.join(", "));
    let formatting_only = changes.iter().filter(|c| c.formatting_only).count();
    let _ = writeln!(
        out,
        "],\n  \"totals\": {{ \"files\": {}, \"added\": {}, \"deleted\": {}, \"linesAdded\": {}, \"linesRemoved\": {}, \"formattingOnly\": {} }}\n}}",
        changes.len(),
        added,
        deleted,
        lines_added,
        lines_removed,
        formatting_only
    );
    o.extend_from_slice(out.as_bytes());
}

/// The lines of `bytes`, without their `\n`.
struct Lines<'a>(&'a [u8]);

impl<'a> Iterator for Lines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.0.is_empty() {
            return None;
        }
        match self.0.iter().position(|&b| b == b'\n') {
            Some(i) => {
                let (line, rest) = self.0.split_at(i + 1);
                self.0 = rest;
                Some(&line[..i])
            }
            None => Some(std::mem::take(&mut self.0)),
        }
    }
}

/// Line tints for removed/added lines and the stronger tint for the differing span within them.
struct Palette {
    del: &'static str,
    del_strong: &'static str,
    ins: &'static str,
    ins_strong: &'static str,
}

/// GitHub's diff colours where the terminal can show them exactly, the nearest xterm-256 cells
/// otherwise: [dark 256, dark truecolor, light 256, light truecolor].
static PALETTES: [Palette; 4] = [
    Palette {
        del: "\x1b[48;5;52m",
        del_strong: "\x1b[48;5;88m",
        ins: "\x1b[48;5;22m",
        ins_strong: "\x1b[48;5;28m",
    },
    Palette {
        del: "\x1b[48;2;68;20;24m",
        del_strong: "\x1b[48;2;134;40;48m",
        ins: "\x1b[48;2;18;54;30m",
        ins_strong: "\x1b[48;2;30;104;52m",
    },
    Palette {
        del: "\x1b[48;5;224m",
        del_strong: "\x1b[48;5;217m",
        ins: "\x1b[48;5;194m",
        ins_strong: "\x1b[48;5;157m",
    },
    Palette {
        del: "\x1b[48;2;255;235;233m",
        del_strong: "\x1b[48;2;255;193;192m",
        ins: "\x1b[48;2;218;251;225m",
        ins_strong: "\x1b[48;2;172;238;187m",
    },
];

/// A plain patch, or (on a terminal in colour) a gutter with line numbers instead of `@@` headers.
#[derive(Clone, Copy)]
struct Style {
    pretty: bool,
    /// The output goes to a terminal, in colour or (with `NO_COLOR`) as a plain patch. Bytes from
    /// a package then go through `defang`.
    escape: bool,
    width: usize,
    palette: &'static Palette,
}

impl Style {
    fn detect(cx: &Ctx, out: &Out, flags: Flags) -> Style {
        let pretty = !flags.json && out.color_out;
        let width = cx
            .env
            .get("COLUMNS")
            .and_then(|c| c.trim().parse::<usize>().ok())
            .filter(|&c| c > 0)
            .unwrap_or(80)
            .clamp(40, 160);
        let truecolor = cx
            .env
            .get("COLORTERM")
            .is_some_and(|v| v == "truecolor" || v == "24bit");
        // `COLORFGBG` is `fg;bg` (rxvt, Konsole, iTerm2…); a light background is 7 or 9–15.
        let light = cx
            .env
            .get("COLORFGBG")
            .and_then(|v| v.rsplit(';').next())
            .and_then(|bg| bg.trim().parse::<u8>().ok())
            .is_some_and(|bg| bg == 7 || (9..=15).contains(&bg));
        Style {
            pretty,
            escape: pretty || (!flags.json && std::io::stdout().is_terminal()),
            width,
            palette: if pretty {
                &PALETTES[usize::from(light) * 2 + usize::from(truecolor)]
            } else {
                &PALETTES[0]
            },
        }
    }

    fn hunk_header(self, out: &mut Vec<u8>, (old_start, old_len, new_start, new_len): (usize, usize, usize, usize), first: bool) {
        if self.pretty {
            if !first {
                out.extend_from_slice("\x1b[2m    ⋮\x1b[0m\n".as_bytes());
            }
        } else {
            let _ = writeln!(Bytes(out), "@@ -{old_start},{old_len} +{new_start},{new_len} @@");
        }
    }

    /// `emph` is the byte range that differs from this line's -/+ partner; it gets a stronger
    /// tint and, on lines wider than the terminal, the shared prefix before it is elided.
    fn line(self, out: &mut Vec<u8>, op: Op, old_no: usize, new_no: usize, text: &[u8], emph: Option<(usize, usize)>) {
        let sign = match op {
            Op::Equal => b' ',
            Op::Delete => b'-',
            Op::Insert => b'+',
        };
        if !self.pretty {
            out.push(sign);
            if self.escape {
                out.extend_from_slice(&defang(text));
            } else {
                out.extend_from_slice(text);
            }
            out.push(b'\n');
            return;
        }
        // Nothing from the package may drive the terminal: CR, ESC and friends become visible.
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        let emph = emph.map(|(lo, hi)| {
            (
                defang(&text[..lo.min(text.len())]).len(),
                defang(&text[..hi.min(text.len())]).len(),
            )
        });
        let text: &[u8] = &defang(text);
        let (num, accent, bg, strong) = match op {
            Op::Equal => (new_no, "\x1b[2m", "", ""),
            Op::Delete => (old_no, "\x1b[31m", self.palette.del, self.palette.del_strong),
            Op::Insert => (new_no, "\x1b[32m", self.palette.ins, self.palette.ins_strong),
        };
        let gw = 5;
        let _ = write!(
            Bytes(out),
            "{accent}{num:>gw$} \x1b[0m\x1b[2m│\x1b[0m{bg}{accent}\x1b[1m{} \x1b[0m{bg}",
            sign as char
        );
        let budget = self.width.saturating_sub(gw + 4);
        match emph {
            Some((lo, hi)) if lo < hi || text.len() > budget => {
                let mut prefix = &text[..lo];
                // Keep ~24 columns of lead-in; drop the rest of a long shared prefix.
                if text.len() > budget && lo > 32 {
                    let mut cut = lo - 24;
                    // Land the cut between tokens rather than mid-identifier when one is near.
                    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b & 0x80 != 0;
                    for _ in 0..16 {
                        if cut == 0 || !ident(text[cut - 1]) || !ident(text[cut]) {
                            break;
                        }
                        cut -= 1;
                    }
                    while cut > 0 && text[cut] & 0xC0 == 0x80 {
                        cut -= 1;
                    }
                    out.extend_from_slice("\x1b[2m…\x1b[22m".as_bytes());
                    out.extend_from_slice(bg.as_bytes());
                    prefix = &text[cut..lo];
                }
                out.extend_from_slice(prefix);
                out.extend_from_slice(strong.as_bytes());
                out.extend_from_slice(&text[lo..hi]);
                out.extend_from_slice(bg.as_bytes());
                out.extend_from_slice(&text[hi..]);
            }
            _ if op == Op::Equal && text.len() > budget && budget > 16 => {
                // Context nobody will read to the end: one screen line is enough.
                let mut cut = budget - 1;
                while cut > 0 && text[cut] & 0xC0 == 0x80 {
                    cut -= 1;
                }
                out.extend_from_slice(&text[..cut]);
                out.extend_from_slice("\x1b[2m…\x1b[0m".as_bytes());
            }
            _ => out.extend_from_slice(text),
        }
        // Erase-to-EOL carries the tint to the edge before resetting.
        out.extend_from_slice(if bg.is_empty() { b"\x1b[0m\n".as_slice() } else { b"\x1b[K\x1b[0m\n".as_slice() });
    }

    /// `path ─────────────── +3 -1`, sized to the terminal.
    fn file_header(self, out: &mut Vec<u8>, c: &FileChange<'_>) {
        let note = match c.semantic {
            Semantic::FormattingOnly => "\x1b[2mformatting only\x1b[0m",
            Semantic::WhitespaceOnly => "\x1b[2mwhitespace only\x1b[0m",
            Semantic::LineEndingsOnly => "\x1b[2mline endings only\x1b[0m",
            Semantic::Reprinted => "\x1b[2mnormalized\x1b[0m ",
            Semantic::Text | Semantic::ModeOnly => "",
        };
        let mut badge = match (c.old, c.new, c.binary) {
            _ if matches!(
                c.semantic,
                Semantic::FormattingOnly | Semantic::WhitespaceOnly | Semantic::LineEndingsOnly
            ) =>
            {
                note.to_owned()
            }
            _ if c.sourcemap => format!(
                "\x1b[2msource map {} → {} bytes\x1b[0m",
                c.old.map_or(0, <[u8]>::len),
                c.new.map_or(0, <[u8]>::len)
            ),
            (_, _, true) => format!("\x1b[33mbinary\x1b[0m \x1b[2m{}\x1b[0m", binary_summary(c)),
            (None, Some(_), _) => format!("{note}\x1b[32mnew\x1b[0m \x1b[32m+{}\x1b[0m", c.added),
            (Some(_), None, _) => format!("{note}\x1b[31mdeleted\x1b[0m \x1b[31m-{}\x1b[0m", c.removed),
            _ => {
                let mut s = String::from(note);
                if c.added > 0 {
                    let _ = write!(s, "\x1b[32m+{}\x1b[0m", c.added);
                }
                if c.removed > 0 {
                    if !s.is_empty() {
                        s.push(' ');
                    }
                    let _ = write!(s, "\x1b[31m-{}\x1b[0m", c.removed);
                }
                s
            }
        };
        if let Some((o, n)) = c.mode_change {
            let mode = if o == 0 {
                format!("\x1b[33mexecutable\x1b[0m {n:o}")
            } else {
                format!("\x1b[33mmode\x1b[0m {o:o} → {n:o}")
            };
            badge = if c.semantic == Semantic::ModeOnly {
                mode
            } else {
                format!("{mode} {badge}")
            };
        }
        if let Some(why) = c.not_normalized {
            badge = format!("\x1b[2m{why}\x1b[0m {badge}");
        }
        let badge_width = strip_ansi_len(badge.as_bytes());
        let mut shown: Vec<u8> = defang(c.path).into_owned();
        // A path too long for the line keeps its tail: the file name is the part that matters.
        let room = self.width.saturating_sub(badge_width + 8);
        if strip_ansi_len(&shown) > room && room > 8 {
            let mut cut = shown.len() - shown.len().min(room - 1);
            while cut < shown.len() && (shown[cut] & 0xC0) == 0x80 {
                cut += 1;
            }
            shown = ["…".as_bytes(), &shown[cut..]].concat();
        }
        let rule = self
            .width
            .saturating_sub(strip_ansi_len(&shown) + badge_width + 4)
            .max(2);
        out.extend_from_slice(b"\n\x1b[1m");
        out.extend_from_slice(&shown);
        let _ = writeln!(Bytes(out), "\x1b[0m \x1b[2m{}\x1b[0m {badge}", "─".repeat(rule));
    }
}

/// `std::fmt::Write` into a byte buffer.
struct Bytes<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Bytes<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

fn strip_ansi_len(s: &[u8]) -> usize {
    let (mut n, mut in_esc) = (0, false);
    for &b in s {
        match (in_esc, b) {
            (false, 0x1b) => in_esc = true,
            (true, b'm') => in_esc = false,
            (true, _) => {}
            // Count UTF-8 scalar starts, not continuation bytes.
            (false, b) => n += usize::from(b & 0xC0 != 0x80),
        }
    }
    n
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// One displayed line. Numbers are 1-based; a side that did not advance keeps its last number.
#[derive(Clone, Copy)]
struct Line<'a> {
    kind: Op,
    old_no: usize,
    new_no: usize,
    text: &'a [u8],
}

/// The edit script between two lists of lines (Myers' greedy algorithm over the part between the
/// common prefix and suffix). A middle part too different to search is replaced whole.
fn edit_script(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let mut prefix = 0;
    while prefix < a.len().min(b.len()) && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (a.len() - prefix).min(b.len() - prefix)
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops = vec![Op::Equal; prefix];
    ops.extend(myers(am, bm));
    ops.extend(std::iter::repeat_n(Op::Equal, suffix));
    ops
}

fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    // The trace keeps one V per edit distance; past this many edits the middle is replaced whole.
    const MAX_EDITS: usize = 2048;
    let replace = || {
        let mut ops = vec![Op::Delete; a.len()];
        ops.extend(std::iter::repeat_n(Op::Insert, b.len()));
        ops
    };
    if n == 0 || m == 0 {
        return replace();
    }
    let offset = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'search: for d in 0..=max.min(MAX_EDITS) as isize {
        // V for the diagonals -d-1..=d+1, which is all that step d reads.
        trace.push(v[(offset - d - 1) as usize..=(offset + d + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let i = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) {
                v[i + 1]
            } else {
                v[i - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'search;
            }
            k += 2;
        }
    }
    let Some(d_end) = found else {
        return replace();
    };
    // Walk the trace back from (n, m).
    let mut ops = Vec::with_capacity((n + m) as usize);
    let (mut x, mut y) = (n, m);
    for d in (1..=d_end).rev() {
        let v = &trace[d as usize];
        let at = |k: isize| v[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        if x == prev_x {
            ops.push(Op::Insert);
        } else {
            ops.push(Op::Delete);
        }
        x = prev_x;
        y = prev_y;
    }
    while x > 0 && y > 0 {
        ops.push(Op::Equal);
        x -= 1;
        y -= 1;
    }
    ops.reverse();
    // Within a run of changes, show the deletions before the insertions.
    let mut i = 0;
    while i < ops.len() {
        if ops[i] == Op::Equal {
            i += 1;
            continue;
        }
        let start = i;
        while i < ops.len() && ops[i] != Op::Equal {
            i += 1;
        }
        ops[start..i].sort_by_key(|op| *op != Op::Delete);
    }
    ops
}

/// A plain line diff of two texts.
fn line_ops<'a>(old: &'a [u8], new: &'a [u8]) -> Vec<Line<'a>> {
    let (a, b): (Vec<&[u8]>, Vec<&[u8]>) = (Lines(old).collect(), Lines(new).collect());
    let script = edit_script(&a, &b);
    let mut out = Vec::with_capacity(script.len());
    let (mut i, mut j) = (0usize, 0usize);
    for op in script {
        let text = match op {
            Op::Equal => {
                i += 1;
                j += 1;
                a[i - 1]
            }
            Op::Delete => {
                i += 1;
                a[i - 1]
            }
            Op::Insert => {
                j += 1;
                b[j - 1]
            }
        };
        out.push(Line {
            kind: op,
            old_no: i,
            new_no: j,
            text,
        });
    }
    out
}

/// For a `-` line directly followed by exactly one `+` line (or the other way round), the byte
/// range where they differ.
fn pair_emphasis(ops: &[Line<'_>], k: usize) -> Option<(usize, usize)> {
    let (op, text) = (ops[k].kind, ops[k].text);
    let partner = match op {
        Op::Delete
            if k + 1 < ops.len()
                && ops[k + 1].kind == Op::Insert
                && ops.get(k + 2).is_none_or(|o| o.kind != Op::Insert)
                && (k == 0 || ops[k - 1].kind != Op::Delete) =>
        {
            ops[k + 1].text
        }
        Op::Insert
            if k > 0
                && ops[k - 1].kind == Op::Delete
                && ops.get(k + 1).is_none_or(|o| o.kind != Op::Insert)
                && (k < 2 || ops[k - 2].kind != Op::Delete) =>
        {
            ops[k - 1].text
        }
        _ => return None,
    };
    let mut lo = text.iter().zip(partner).take_while(|(a, b)| a == b).count();
    let max_suffix = text.len().min(partner.len()) - lo;
    let suffix = text
        .iter()
        .rev()
        .zip(partner.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let mut hi = text.len() - suffix;
    while lo > 0 && text.get(lo).is_some_and(|b| b & 0xC0 == 0x80) {
        lo -= 1;
    }
    while hi < text.len() && text[hi] & 0xC0 == 0x80 {
        hi += 1;
    }
    Some((lo, hi))
}

const NO_NEWLINE_MARKER: &[u8] = b"\\ No newline at end of file\n";

/// What the terminal view compares and shows for a text file: line-ending CRs dropped, and a
/// final newline (a missing one is the patch's business, not a -/+ pair of equal lines).
fn view_bytes(bytes: &[u8]) -> Cow<'_, [u8]> {
    let terminated = bytes.is_empty() || bytes.ends_with(b"\n");
    if bytes.contains(&b'\r') || !terminated {
        let mut v = strip_cr(bytes);
        if !v.is_empty() && !v.ends_with(b"\n") {
            v.push(b'\n');
        }
        Cow::Owned(v)
    } else {
        Cow::Borrowed(bytes)
    }
}

fn unified_hunks(old: &[u8], new: &[u8], context: usize, style: Style, change: &mut FileChange<'_>) {
    // The terminal view compares CRLF files as if they were LF.
    let (old, new) = if style.pretty {
        (view_bytes(old), view_bytes(new))
    } else {
        (Cow::Borrowed(old), Cow::Borrowed(new))
    };
    let ops = line_ops(&old, &new);
    // `patch`/`git apply` need to know when a side's last line had no terminator.
    let unterminated = (
        !old.is_empty() && !old.ends_with(b"\n"),
        !new.is_empty() && !new.ends_with(b"\n"),
    );
    render_ops(&ops, context, style, change, unterminated);
}

/// Windows of `context` lines around every change, rendered per the style.
fn render_ops(
    ops: &[Line<'_>],
    context: usize,
    style: Style,
    change: &mut FileChange<'_>,
    (old_unterminated, new_unterminated): (bool, bool),
) {
    for op in ops {
        match op.kind {
            Op::Insert => change.added += 1,
            Op::Delete => change.removed += 1,
            Op::Equal => {}
        }
    }
    let is_change = |op: &Line| op.kind != Op::Equal;
    let last_old = ops.iter().rposition(|op| op.kind != Op::Insert);
    let last_new = ops.iter().rposition(|op| op.kind != Op::Delete);
    let n = ops.len();
    let mut i = 0;
    let mut first = true;
    while i < n {
        if !is_change(&ops[i]) {
            i += 1;
            continue;
        }
        let mut start = i;
        while start > 0 && !is_change(&ops[start - 1]) && i - start < context {
            start -= 1;
        }
        // Extend until more than 2*context quiet lines in a row (or the end), then keep
        // `context` of them.
        let mut end = i;
        let mut quiet = 0;
        while end < n {
            if is_change(&ops[end]) {
                quiet = 0;
            } else {
                quiet += 1;
                if quiet > 2 * context {
                    break;
                }
            }
            end += 1;
        }
        let mut e = end;
        let mut trailing = 0;
        while e > start && !is_change(&ops[e - 1]) {
            trailing += 1;
            e -= 1;
        }
        let end = e + trailing.min(context);
        let window = &ops[start..end];
        let old_len = window.iter().filter(|op| op.kind != Op::Insert).count();
        let new_len = window.iter().filter(|op| op.kind != Op::Delete).count();
        // A line numbers what it consumed; a side that did not advance names the line before
        // the gap.
        let old_start = window
            .iter()
            .find(|op| op.kind != Op::Insert)
            .map_or(window[0].old_no, |op| op.old_no);
        let new_start = window
            .iter()
            .find(|op| op.kind != Op::Delete)
            .map_or(window[0].new_no, |op| op.new_no);
        style.hunk_header(&mut change.hunks, (old_start, old_len, new_start, new_len), first);
        first = false;
        for (k, op) in window.iter().enumerate() {
            let emph = pair_emphasis(window, k);
            style.line(&mut change.hunks, op.kind, op.old_no, op.new_no, op.text, emph);
            let idx = start + k;
            if !style.pretty
                && ((Some(idx) == last_old && old_unterminated)
                    || (Some(idx) == last_new && new_unterminated))
            {
                change.hunks.extend_from_slice(NO_NEWLINE_MARKER);
            }
        }
        i = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Style {
        Style {
            pretty: false,
            escape: false,
            width: 80,
            palette: &PALETTES[0],
        }
    }

    fn hunks(a: &[u8], b: &[u8], context: usize) -> (String, usize, usize) {
        let mut change = FileChange::default();
        unified_hunks(a, b, context, plain(), &mut change);
        (lossy(&change.hunks), change.added, change.removed)
    }

    #[test]
    fn hunks_handle_context_and_missing_newlines() {
        assert_eq!(
            hunks(b"one\ntwo\nthree\n", b"one\nnew\nthree\n", 1),
            ("@@ -1,3 +1,3 @@\n one\n-two\n+new\n three\n".into(), 1, 1)
        );
        assert_eq!(
            hunks(b"{\"a\":1}", b"{\"a\":2}\n", 3).0,
            "@@ -1,1 +1,1 @@\n-{\"a\":1}\n\\ No newline at end of file\n+{\"a\":2}\n"
        );
        assert_eq!(
            hunks(b"a\nb\nc\nd\ne\nf\ng\nh\n", b"a\nb\nc\nD\ne\nf\ng\nh\n", 1).0,
            "@@ -3,3 +3,3 @@\n c\n-d\n+D\n e\n"
        );
    }

    #[test]
    fn myers_finds_a_shortest_script() {
        let a: Vec<&[u8]> = vec![b"a", b"b", b"c", b"a", b"b", b"b", b"a"];
        let b: Vec<&[u8]> = vec![b"c", b"b", b"a", b"b", b"a", b"c"];
        let ops = edit_script(&a, &b);
        assert_eq!(ops.iter().filter(|o| **o != Op::Equal).count(), 5);
        assert_eq!(ops.iter().filter(|o| **o != Op::Insert).count(), a.len());
        assert_eq!(ops.iter().filter(|o| **o != Op::Delete).count(), b.len());
    }

    #[test]
    fn filters_names_and_quoting() {
        assert!(filter_matches("*.js", b"dist/a.js"));
        assert!(filter_matches("dist/", b"dist/a.js"));
        assert!(filter_matches("a.js", b"dist/a.js"));
        assert!(!filter_matches("dist/a.js", b"outside/a.js"));
        assert!(is_npm_name("@scope/name"));
        assert!(!is_npm_name("../../evil"));
        assert!(!is_npm_name("evil\x1b[2J"));
        assert_eq!(patch_name(b"a/", b"a\n.js"), b"\"a/a\\012.js\"");
        assert_eq!(split_filter("diffme@1.0.0:index.js"), Some(("diffme@1.0.0", "index.js")));
        assert_eq!(split_filter("./x:y"), None);
        assert_eq!(split_name_and_version("@s/n@1"), ("@s/n", Some("1")));
        assert_eq!(split_name_and_version("n"), ("n", None));
    }

    #[test]
    fn package_bytes_are_drawn_not_executed() {
        assert_eq!(lossy(&defang(b"a\x1b[2Kb\r")), "a␛[2Kb^M");
        assert_eq!(lossy(&defang("x\u{202e}y\u{9b}".as_bytes())), "x‹U+202E›y‹U+009B›");
        assert!(has_bidi_controls("\u{2066}".as_bytes()));
        assert!(!is_binary(b"a.js", b"/* \0 */"));
        assert!(is_binary(b"a.bin", b"\0\x01one"));
        assert!(!is_binary(b"a.bin", b"/* \0 */ x"));
        assert!(is_binary(b"a.wasm", b"\0asm1"));
        assert!(is_binary(b"logo.bin", b"\0a"));
        assert!(is_binary(b"tool.bin", b"\0"));
    }

    #[test]
    fn notes_read_package_json_like_install() {
        let file = pj::parse(r#"{"name":"p","bin":"./cli.js","os":"linux"}"#).unwrap();
        let entry = pj::parse(r#"{"name":"p","bin":{"p":"cli.js"},"os":["linux"]}"#).unwrap();
        for field in ["bin", "os"] {
            assert_eq!(install_view(&file, field), install_view(&entry, field));
        }
        let props = top_level_props(br#"{"a":{"a":1},"b":"a", "a" : [2] }"#);
        let keys: Vec<&str> = props.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["a", "b", "a"]);
        assert_eq!(props[2].1, 28..31);
        let first = parse_first_wins(br#"{"main":"./a.js","main":"./b.js"}"#).unwrap();
        assert_eq!(first.get("main").and_then(Json::as_str), Some("./a.js"));
        assert_eq!(expr_text(&pj::parse(r#"{"tool":"./cli.js"}"#).unwrap()), r#"{ "tool": "./cli.js" }"#);
        assert_eq!(tags("<b>x<r> <(y)", false), "x <(y)");
    }
}
