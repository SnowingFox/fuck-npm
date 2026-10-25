//! `bundledDependencies` (bun `pack_command.rs` `iterate_bundled_deps`): each listed package
//! is found the way `require` finds it from the packed package, its files go below the
//! tarball's `node_modules`, and so do its own dependencies, nested where the installed layout
//! needs them so the unpacked package resolves the same copies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::json::{self, Json};
use super::manifests::{WorkspaceManifests, needs_workspace_manifests};
use super::out::{FResult, Out, show};
use super::walk::{self, DirInfo, IgnoreKind, Kind, PackQueue, Walker, bytes_path, join_sub};

/// A file to archive at `path` with the contents of `source` (or `edited`, a workspace's
/// package.json with its specs replaced).
pub(crate) struct BundledFile {
    pub path: Vec<u8>,
    pub source: PathBuf,
    pub edited: Option<Vec<u8>>,
}

struct BundledPackage {
    subpath: Vec<u8>,
    name_start: usize,
    real_path: PathBuf,
    parent: Option<usize>,
}

#[derive(Default)]
struct Bundle {
    packages: Vec<BundledPackage>,
    placed: HashMap<Vec<u8>, usize>,
    pending: Vec<usize>,
    files: Vec<BundledFile>,
    project_path: PathBuf,
}

impl Bundle {
    fn subpath_in(&self, parent: Option<usize>, name: &str) -> Vec<u8> {
        match parent {
            Some(p) => [
                &self.packages[p].subpath[..],
                b"/node_modules/",
                name.as_bytes(),
            ]
            .concat(),
            None => [b"node_modules/".as_slice(), name.as_bytes()].concat(),
        }
    }

    fn place(&mut self, parent: Option<usize>, name: &str, real_path: PathBuf) -> usize {
        let subpath = self.subpath_in(parent, name);
        let index = self.packages.len();
        self.placed.insert(subpath.clone(), index);
        self.packages.push(BundledPackage {
            name_start: subpath.len() - name.len(),
            subpath,
            real_path,
            parent,
        });
        index
    }

    /// What `require(name)` in bundled package `from` resolves to once unpacked.
    fn visible_from(&self, from: usize, name: &str) -> Option<&BundledPackage> {
        let mut level = Some(from);
        loop {
            if let Some(&index) = self.placed.get(&self.subpath_in(level, name)) {
                return Some(&self.packages[index]);
            }
            level = self.packages[level?].parent;
        }
    }
}

/// bun `alias_is_safe_install_target`: the name is one path entry below `node_modules`.
pub(crate) fn alias_is_safe_install_target(alias: &str) -> bool {
    let b = alias.as_bytes();
    if b.is_empty() || b.len() > 511 {
        return false;
    }
    let mut components = 0;
    for component in b.split(|&c| c == b'/') {
        components += 1;
        if component.len() > 255 || matches!(component, b"" | b"." | b"..") {
            return false;
        }
    }
    if b.iter()
        .any(|&c| c == b'\\' || c == b':' || c.is_ascii_control())
        || b.windows(2)
            .any(|w| w[0] == 0xc2 && (0x80..=0x9f).contains(&w[1]))
    {
        return false;
    }
    let name = match (components, alias.split_once('/')) {
        (1, None) if b[0] != b'@' => alias,
        (2, Some((scope, name))) if scope.len() > 1 && b[0] == b'@' => name,
        _ => return false,
    };
    !name.starts_with('.') && !name.eq_ignore_ascii_case("node_modules")
}

const NODE_MODULES_NEEDLE: &str = "/node_modules/";

/// The folder `require(name)` resolves to from `from`, with symlinks resolved, and whether
/// it is in `from`'s own `node_modules`. The search does not go above `project`.
fn resolve_installed(
    out: &Out,
    project: &Path,
    from: &Path,
    name: &str,
) -> FResult<Option<(PathBuf, bool)>> {
    let from_s = from.to_string_lossy();
    let last: PathBuf = from
        .ancestors()
        .find(|dir| *dir == project)
        .map(Path::to_path_buf)
        .or_else(|| {
            from_s
                .find(NODE_MODULES_NEEDLE)
                .map(|at| PathBuf::from(&from_s[..at]))
        })
        .unwrap_or_else(|| from.to_path_buf());
    let last_len = last.as_os_str().len();
    for dir in from.ancestors() {
        if dir.as_os_str().len() < last_len {
            break;
        }
        if dir.file_name().is_some_and(|n| n == "node_modules") {
            continue;
        }
        let candidate = dir.join("node_modules").join(name);
        match std::fs::metadata(&candidate) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => continue,
            Err(e) if walk::is_absent(&e) => continue,
            Err(e) => {
                return Err(out.named(
                    &super::out::errno_name(&e),
                    &format!(
                        "failed to open directory \"{}\" for packing",
                        candidate.display()
                    ),
                ));
            }
        }
        let real = std::fs::canonicalize(&candidate).map_err(|e| {
            out.named(
                &super::out::errno_name(&e),
                &format!(
                    "failed to open directory \"{}\" for packing",
                    candidate.display()
                ),
            )
        })?;
        return Ok(Some((
            real,
            dir.as_os_str().len() == from.as_os_str().len(),
        )));
    }
    Ok(None)
}

/// The bundled packages to list (`bundled <name>`), and their files sorted by tarball path.
pub(crate) struct Bundled {
    pub names: Vec<String>,
    pub files: Vec<BundledFile>,
    pub count: usize,
}

pub(crate) struct BundleInput<'a> {
    pub walker: &'a Walker<'a>,
    pub names: Vec<String>,
    pub package_json: &'a Json,
    /// The packed package, symlinks resolved.
    pub root_path: &'a Path,
    /// The workspace root (the package itself outside a workspace), symlinks resolved.
    pub project_path: &'a Path,
    pub manifests: &'a mut Option<WorkspaceManifests>,
    /// Where the manifests are loaded from.
    pub project_dir: &'a Path,
}

pub(crate) fn iterate_bundled_deps(input: BundleInput<'_>) -> FResult<Bundled> {
    let out = input.walker.out;
    let mut bundle = Bundle {
        project_path: input.project_path.to_path_buf(),
        ..Default::default()
    };
    let mut names = Vec::new();
    if input.names.is_empty() {
        return Ok(Bundled {
            names,
            files: Vec::new(),
            count: 0,
        });
    }
    let is_in = |group: &str, name: &str| {
        input
            .package_json
            .get(group)
            .and_then(Json::as_object)
            .is_some_and(|deps| deps.iter().any(|(k, _)| k == name))
    };
    for dep in input.names {
        let found = if alias_is_safe_install_target(&dep) {
            resolve_installed(out, &bundle.project_path, input.root_path, &dep)?
        } else {
            None
        };
        let Some((real_path, _)) = found else {
            // Like npm, skip a package an install may omit: an optional dependency for another
            // platform, a peer, a devDependency, or a name in no dependency group.
            if !is_in("dependencies", &dep) || is_in("optionalDependencies", &dep) {
                continue;
            }
            return Err(out.error(&format!("bundled dependency \"{dep}\" is not installed")));
        };
        let subpath = bundle.subpath_in(None, &dep);
        if !bundle.placed.contains_key(&subpath) {
            bundle.place(None, &dep, real_path);
        }
        names.push(dep);
    }
    let mut count = 0;
    let mut cx = AddCx {
        walker: input.walker,
        manifests: input.manifests,
        project_dir: input.project_dir,
        root_path: input.root_path,
    };
    for index in 0..bundle.packages.len() {
        count += 1;
        cx.add(&mut bundle, index)?;
    }
    while let Some(index) = bundle.pending.pop() {
        let package = &bundle.packages[index];
        names.push(String::from_utf8_lossy(&package.subpath[package.name_start..]).into_owned());
        count += 1;
        cx.add(&mut bundle, index)?;
    }
    bundle.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Bundled {
        names,
        files: bundle.files,
        count,
    })
}

struct AddCx<'a> {
    walker: &'a Walker<'a>,
    manifests: &'a mut Option<WorkspaceManifests>,
    project_dir: &'a Path,
    root_path: &'a Path,
}

impl AddCx<'_> {
    /// Adds the files of bundled package `index`, and gives its dependencies a place.
    fn add(&mut self, bundle: &mut Bundle, index: usize) -> FResult<()> {
        let out = self.walker.out;
        let subpath = bundle.packages[index].subpath.clone();
        let real_path = bundle.packages[index].real_path.clone();
        let source = match std::fs::read(real_path.join("package.json")) {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) if walk::is_absent(&e) || is_eisdir(&e) => None,
            Err(e) => {
                return Err(out.sys(
                    &e,
                    "read",
                    &format!(
                        "failed to read package.json: \"{}/package.json\"",
                        show(&subpath)
                    ),
                ));
            }
        };
        let json = source.as_deref().and_then(|s| json::parse(s).ok());
        let real_s = real_path.to_string_lossy().into_owned();
        let project_s = bundle.project_path.to_string_lossy().into_owned();
        let below_project = real_s.strip_prefix(project_s.as_str()).unwrap_or(&real_s);
        let is_own_package = !below_project.contains(NODE_MODULES_NEEDLE);
        if is_own_package
            && self.manifests.is_none()
            && json.as_ref().is_some_and(needs_workspace_manifests)
        {
            *self.manifests = Some(WorkspaceManifests::load(out, self.project_dir)?);
        }
        if let Some(json) = &json {
            self.find_dependencies_to_bundle(bundle, index, json)?;
        }
        if is_own_package {
            return self.add_own_package(bundle, &subpath, &real_path, source.as_deref().zip(json));
        }
        const PACKAGE_DEPTH: usize = 2;
        let mut dirs = vec![(real_path.clone(), subpath.clone(), PACKAGE_DEPTH)];
        while let Some((dir, dir_subpath, depth)) = dirs.pop() {
            for entry in walk::read_dir(out, &dir, &dir_subpath)? {
                if entry.kind == Kind::Other {
                    continue;
                }
                if depth == PACKAGE_DEPTH && entry.name == b"node_modules" {
                    continue;
                }
                let entry_subpath = join_sub(&dir_subpath, &entry.name);
                if let Some(pattern) = walk::default_ignore(&entry.name) {
                    self.walker.print_ignored(
                        IgnoreKind::Default,
                        pattern,
                        &entry_subpath,
                        entry.kind,
                    );
                    continue;
                }
                if entry.kind == Kind::File {
                    let in_package = &entry_subpath[subpath.len() + 1..];
                    bundle.files.push(BundledFile {
                        source: real_path.join(bytes_path(in_package)),
                        path: entry_subpath,
                        edited: None,
                    });
                } else {
                    dirs.push((dir.join(bytes_path(&entry.name)), entry_subpath, depth + 1));
                }
            }
        }
        Ok(())
    }

    /// A bundled workspace or linked folder: packed as a package if it has a package.json
    /// entry, else as a directory.
    fn add_own_package(
        &mut self,
        bundle: &mut Bundle,
        subpath: &[u8],
        real_path: &Path,
        manifest: Option<(&str, Json)>,
    ) -> FResult<()> {
        let out = self.walker.out;
        let mut files: Vec<BundledFile> = Vec::new();
        let mut add_file = |file: &[u8], edited: Option<Vec<u8>>| {
            files.push(BundledFile {
                path: join_sub(subpath, file),
                source: real_path.join(bytes_path(file)),
                edited,
            });
        };
        let json = match manifest {
            Some((source, mut json)) => {
                let edited = if needs_workspace_manifests(&json) {
                    let indent = json::guess_indentation(source.as_bytes());
                    super::manifests::substitute(
                        out,
                        self.manifests.as_ref(),
                        real_path,
                        &mut json,
                    )?;
                    Some(json::print(&json, indent, source.ends_with('\n')).into_bytes())
                } else {
                    None
                };
                add_file(b"package.json", edited);
                json
            }
            None => {
                match walk::bin_kind_without_following_symlinks(out, real_path, b"package.json")? {
                    Kind::Other => {
                        let mut queue = PackQueue::new();
                        let depth = subpath.iter().filter(|&&b| b == b'/').count() + 2;
                        self.walker.project_tree(
                            &mut queue,
                            &[],
                            Some(&[]),
                            DirInfo {
                                abs: real_path.to_path_buf(),
                                subpath: subpath.to_vec(),
                                depth,
                            },
                        )?;
                        for file in queue {
                            add_file(&file[subpath.len() + 1..], None);
                        }
                        bundle.files.extend(files);
                        return Ok(());
                    }
                    kind => {
                        if kind == Kind::File {
                            add_file(b"package.json", None);
                        }
                        Json::Object(Vec::new())
                    }
                }
            }
        };
        let (published, _) = self.walker.published_files(real_path, &json)?;
        for file in published {
            add_file(&file, None);
        }
        bundle.files.extend(files);
        Ok(())
    }

    fn find_dependencies_to_bundle(
        &mut self,
        bundle: &mut Bundle,
        index: usize,
        json: &Json,
    ) -> FResult<()> {
        let out = self.walker.out;
        for group in ["dependencies", "optionalDependencies"] {
            let Some(Json::Object(deps)) = json.get(group) else {
                continue;
            };
            for (dep_name, _) in deps {
                if !alias_is_safe_install_target(dep_name) {
                    continue;
                }
                let from = bundle.packages[index].real_path.clone();
                let Some((real_path, is_nested)) =
                    resolve_installed(out, &bundle.project_path, &from, dep_name)?
                else {
                    continue;
                };
                if real_path == self.root_path {
                    continue;
                }
                let parent = match bundle.visible_from(index, dep_name) {
                    Some(visible) if visible.real_path == real_path => continue,
                    Some(_) => Some(index),
                    None => {
                        let other_at_the_top = || -> FResult<bool> {
                            Ok(resolve_installed(
                                out,
                                &bundle.project_path,
                                self.root_path,
                                dep_name,
                            )?
                            .is_some_and(|(other, _)| other != real_path))
                        };
                        if is_nested || other_at_the_top()? {
                            Some(index)
                        } else {
                            None
                        }
                    }
                };
                let copies_above =
                    std::iter::successors(parent, |&above| bundle.packages[above].parent)
                        .filter(|&above| bundle.packages[above].real_path == real_path)
                        .count();
                if copies_above > 1 {
                    return Err(out.error(&format!(
                        "bundled dependencies form a cycle: \"{}\"",
                        show(&bundle.subpath_in(parent, dep_name))
                    )));
                }
                let placed = bundle.place(parent, dep_name, real_path);
                bundle.pending.push(placed);
            }
        }
        Ok(())
    }
}

fn is_eisdir(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EISDIR)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

/// The names `bundledDependencies` (or `bundleDependencies`) lists: an array of names, or
/// `true` for every `dependencies` entry.
pub(crate) fn get_bundled_deps(out: &Out, json: &Json) -> FResult<Vec<String>> {
    for field in ["bundledDependencies", "bundleDependencies"] {
        let Some(value) = json.get(field) else {
            continue;
        };
        let names: Option<Vec<String>> = match value {
            Json::Array(items, _) => items
                .iter()
                .map(|i| i.as_str().map(str::to_owned))
                .collect(),
            Json::Bool(all) => Some(match json.get("dependencies") {
                Some(Json::Object(deps)) if *all => deps.iter().map(|(k, _)| k.clone()).collect(),
                _ => Vec::new(),
            }),
            _ => None,
        };
        return names.ok_or_else(|| {
            out.error(&format!(
                "expected `{field}` to be a boolean or an array of strings"
            ))
        });
    }
    Ok(Vec::new())
}
