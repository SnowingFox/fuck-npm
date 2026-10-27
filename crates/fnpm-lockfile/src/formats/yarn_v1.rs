//! yarn.lock v1: migration (bun `install/yarn.rs`, including its hand-written parser) and
//! export (bun `lockfile/printer/Yarn.rs`).

use std::collections::HashMap;

use fnpm_types::spec::{
    infer_tag, is_github_tarball_path, link_path_for_lockfile, without_npm_protocol,
};
use fnpm_types::{
    Behavior, ConfigVersion, CpuSet, DepEdge, Integrity, LockGraph, OsSet, Package, PackageId,
    PlatformConstraints, Resolution, SpecTag,
};
use fnpm_workspace::Workspace;

use super::foreign::{self, DiskFiles, MigrateOptions, Migration, semver};
use crate::{Format, Result};

const FILE: &str = "yarn.lock";

/// Which yarn wrote `bytes`: [`Format::YarnV1`] when the `# yarn lockfile v1` marker is in the
/// comment lines at the top (that is how yarn itself decides), [`Format::YarnBerry`] for a
/// `__metadata:` mapping, `None` for neither.
pub fn detect(bytes: &[u8]) -> Option<Format> {
    let is_berry = memchr::memmem::find(bytes, b"\n__metadata:").is_some()
        || bytes.starts_with(b"__metadata:");
    let v1_marker = match memchr::memmem::find(bytes, b"# yarn lockfile v1") {
        Some(at) if is_berry => bytes[..at]
            .split(|&b| b == b'\n')
            .all(|line| line.is_empty() || line.starts_with(b"#") || line == b"\r"),
        Some(_) => true,
        None => false,
    };
    if v1_marker {
        Some(Format::YarnV1)
    } else if is_berry {
        Some(Format::YarnBerry)
    } else {
        None
    }
}

/// Migrates a yarn.lock with the process environment and the real filesystem. A berry
/// lockfile is handed to [`super::yarn_berry`], as bun does.
pub fn parse(bytes: &[u8], workspace: &Workspace) -> Result<LockGraph> {
    let files = DiskFiles;
    Ok(migrate(bytes, workspace, &MigrateOptions::from_process(&files))?.graph)
}

/// The yarn v1 migration. Resolved versions, tarball URLs and integrity are kept exactly; a
/// dependency of the root or a workspace that yarn.lock does not lock is dropped from the
/// graph so the install resolves it (bun `bind_importer_dependencies`).
pub fn migrate(bytes: &[u8], ws: &Workspace, options: &MigrateOptions<'_>) -> Result<Migration> {
    match detect(bytes) {
        Some(Format::YarnV1) => {}
        Some(_) => return super::yarn_berry::migrate(bytes, ws, options),
        None => {
            return Err(foreign::parse_error(
                ws,
                FILE,
                "unsupported yarn.lock version (no \"# yarn lockfile v1\" marker or __metadata)",
            ));
        }
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| foreign::parse_error(ws, FILE, "yarn.lock is not UTF-8"))?;
    let lock = YarnLock::parse(text);
    build(&lock, ws, options)
}

/// In `DEPENDENCY_SECTIONS` order.
const SECTIONS: [(&str, Behavior); 4] = [
    ("dependencies:", Behavior::PROD),
    ("optionalDependencies:", Behavior::OPTIONAL),
    ("peerDependencies:", Behavior::PEER),
    ("devDependencies:", Behavior::DEV),
];

#[derive(Debug, Default, Clone)]
pub(crate) struct Entry<'a> {
    pub(crate) specs: Vec<&'a str>,
    /// The name of the package the entry resolves to (see [`Entry::package_name_of`]).
    pub(crate) name: &'a str,
    pub(crate) version: &'a str,
    pub(crate) resolved: Option<String>,
    pub(crate) integrity: Option<&'a str>,
    /// In [`SECTIONS`] order; a later line for the same name replaces the earlier one.
    pub(crate) dependencies: [Option<Vec<(&'a str, &'a str)>>; 4],
    pub(crate) commit: Option<String>,
    pub(crate) workspace: bool,
    pub(crate) file: Option<&'a str>,
    /// The directory a `link:` spec points to, relative to yarn.lock.
    pub(crate) link: Option<String>,
    pub(crate) os: Vec<&'a str>,
    pub(crate) cpu: Vec<&'a str>,
    pub(crate) git_repo_name: Option<String>,
}

fn trim_chars<'a>(s: &'a str, chars: &[char]) -> &'a str {
    s.trim_matches(|c| chars.contains(&c))
}

impl<'a> Entry<'a> {
    /// An `npm:` alias spec may follow plain specs of the same package on yarn's key line.
    pub(crate) fn package_name_of(specs: &[&'a str]) -> &'a str {
        let first = specs.first().map_or("", |s| Self::name_from_spec(s));
        specs
            .iter()
            .find_map(|s| Self::npm_alias_target(s))
            .unwrap_or(first)
    }

    /// `<name>` for `alias@npm:<name>@<range>`, `None` for any other spec.
    pub(crate) fn npm_alias_target(spec: &'a str) -> Option<&'a str> {
        let alias_end = spec.find("@npm:")?;
        let range = without_npm_protocol(&spec[alias_end + 1..]);
        let (name, _) = Self::parse_npm_alias(range.strip_prefix("npm:")?);
        (!name.is_empty()).then_some(name)
    }

    pub(crate) fn has_direct_url_spec(&self) -> bool {
        self.specs
            .iter()
            .any(|s| s.contains("@https://") || s.contains("@http://"))
    }

    /// The identity consolidation and the package pass both use; git entries are identified by
    /// repository.
    pub(crate) fn dedupe_name(&self) -> &str {
        match &self.git_repo_name {
            Some(repo) if !self.has_direct_url_spec() => repo,
            _ => self.name,
        }
    }

    pub(crate) fn name_from_spec(spec: &'a str) -> &'a str {
        let unquoted = if spec.len() >= 2 && spec.starts_with('"') && spec.ends_with('"') {
            &spec[1..spec.len() - 1]
        } else {
            spec
        };
        if let Some(scoped) = unquoted.strip_prefix('@') {
            if let Some(second) = scoped.find('@') {
                return &unquoted[..second + 1];
            }
            return unquoted;
        }
        for marker in ["@npm:", "@https://", "@git+", "@github:", "@file:", "@"] {
            if let Some(i) = unquoted.find(marker) {
                return &unquoted[..i];
            }
        }
        unquoted
    }

    pub(crate) fn is_git_dependency(version: &str) -> bool {
        if let Some(path) = version.strip_prefix("https://github.com/") {
            // An archive download's `#` is yarn's tarball hash, not a commit.
            return !is_github_tarball_path(Self::url_without_hash(path));
        }
        version.starts_with("git+")
            || version.starts_with("git://")
            || version.starts_with("github:")
    }

    /// Did a spec (a range, a dist-tag, or an `npm:` alias of one) ask a registry for it?
    pub(crate) fn is_registry_entry(&self) -> bool {
        self.specs.iter().any(|spec| {
            let name = Self::name_from_spec(spec);
            let range = spec.get(name.len() + 1..).unwrap_or("");
            let target = match range.strip_prefix("npm:") {
                Some(alias) => Self::parse_npm_alias(alias).1,
                None => range,
            };
            infer_tag(target).is_npm()
        })
    }

    pub(crate) fn is_remote_tarball(version: &str) -> bool {
        version.starts_with("https://") && version.ends_with(".tgz")
    }

    /// yarn writes tarball `resolved` fields as `<url>#<sha1 of the tarball>`.
    pub(crate) fn url_without_hash(resolved: &str) -> &str {
        resolved.split_once('#').map_or(resolved, |(url, _)| url)
    }

    /// The `integrity` line, or without one the sha1 after the `#` of a tarball's `resolved`.
    fn tarball_integrity(&self) -> Option<Integrity> {
        let integrity = self.integrity.and_then(foreign::parse_integrity);
        // In a git entry the `#` is followed by a commit, not by the tarball's sha1.
        if integrity.is_some() || self.commit.is_some() {
            return integrity;
        }
        match self.resolved.as_deref().and_then(|r| r.split_once('#')) {
            Some((_, sha1)) if sha1.len() == 40 => Integrity::parse_sha1_hex(sha1).ok(),
            _ => None,
        }
    }

    pub(crate) fn file_path_from_spec(spec: &str) -> &str {
        let mut path = spec.strip_prefix("file:").unwrap_or(spec);
        while let Some(rest) = path.strip_prefix("./") {
            path = rest;
        }
        path.strip_suffix('/').unwrap_or(path)
    }

    /// `https://registry.npmjs.org/@scope/name/-/name-1.0.0.tgz` gives `@scope/name`.
    pub(crate) fn package_name_from_default_registry_url(url: &str) -> Option<&str> {
        let host_and_path = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;
        let path = host_and_path
            .strip_prefix("registry.npmjs.org/")
            .or_else(|| host_and_path.strip_prefix("registry.yarnpkg.com/"))?;
        // `-` is a valid package name, so the first `/-/` can be inside `@scope/-`.
        let scope_len = if path.starts_with('@') {
            let scope_end = path.find('/')?;
            if scope_end == 1 {
                return None;
            }
            scope_end + 1
        } else {
            0
        };
        let name_len = scope_len + path[scope_len..].find('/')?;
        let (name, rest) = path.split_at(name_len);
        (rest.starts_with("/-/") && fnpm_types::name::is_safe_install_folder_name(name))
            .then_some(name)
    }

    /// A `github:` shorthand resolves to its `https://github.com/` URL.
    fn set_git_source(&mut self, version: &str) {
        let url = version.strip_prefix("git+").unwrap_or(version);
        self.commit = url.split_once('#').map(|(_, c)| c.to_owned());
        let url = Self::url_without_hash(url);
        if let Some(github_path) = version.strip_prefix("github:") {
            let path = Self::url_without_hash(github_path);
            if let Some((_, repo)) = path.split_once('/') {
                self.git_repo_name = Some(repo.to_owned());
            }
            self.resolved = Some(format!("https://github.com/{path}"));
            return;
        }
        if url.contains("github.com") {
            let remaining = url
                .find("github.com/")
                .map_or(url, |i| &url[i + "github.com/".len()..]);
            if let Some((_, repo)) = remaining.split_once('/') {
                self.git_repo_name = Some(repo.strip_suffix(".git").unwrap_or(repo).to_owned());
            }
        }
        self.resolved = Some(url.to_owned());
    }

    /// `npm:<name>@<range>` into (name, range); the `@` of a scope is not the separator.
    pub(crate) fn parse_npm_alias(version: &str) -> (&str, &str) {
        let target = version.strip_prefix("npm:").unwrap_or(version);
        let scope_len = usize::from(target.starts_with('@'));
        let Some(at) = target[scope_len..].find('@') else {
            return (target, "*");
        };
        let (name, range) = target.split_at(scope_len + at);
        let range = &range[1..];
        (name, if range.is_empty() { "*" } else { range })
    }
}

#[derive(Debug, Default)]
pub(crate) struct YarnLock<'a> {
    pub(crate) entries: Vec<Entry<'a>>,
    by_key: HashMap<String, usize>,
}

impl<'a> YarnLock<'a> {
    pub(crate) fn parse(content: &'a str) -> Self {
        let mut lock = YarnLock::default();
        let mut current: Option<Entry<'a>> = None;
        let mut dep_type: Option<usize> = None;
        for raw_line in content.split('\n') {
            let line = raw_line.trim_end_matches([' ', '\r', '\t']);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let indent = line.bytes().take_while(|&b| b == b' ').count();
            let trimmed = trim_chars(&line[indent..], &[' ', '\r', '\t']);
            if trimmed.is_empty() {
                continue;
            }
            if indent == 0 && trimmed.ends_with(':') {
                if let Some(entry) = current.take() {
                    lock.consolidate(entry);
                }
                let specs: Vec<&str> = trimmed[..trimmed.len() - 1]
                    .split(',')
                    .map(|s| trim_chars(s, &[' ', '"']))
                    .filter(|s| !s.is_empty())
                    .collect();
                let mut entry = Entry {
                    name: Entry::package_name_of(&specs),
                    ..Default::default()
                };
                for &spec in &specs {
                    let name = Entry::name_from_spec(spec);
                    let range = spec.get(name.len() + 1..).unwrap_or("");
                    if let Some(path) = range.strip_prefix("link:") {
                        entry.link = Some(link_path_for_lockfile(path).into_owned());
                        break;
                    }
                    // yarn treats a bare relative path like a `file:` path.
                    if range.starts_with("file:")
                        || range.starts_with("./")
                        || range.starts_with("../")
                    {
                        entry.file = Some(Entry::file_path_from_spec(range));
                        break;
                    }
                }
                entry.specs = specs;
                current = Some(entry);
                dep_type = None;
                continue;
            }
            let Some(entry) = current.as_mut() else {
                continue;
            };
            if indent == 0 {
                continue;
            }
            if let Some(i) = SECTIONS.iter().position(|(s, _)| *s == trimmed) {
                dep_type = Some(i);
                entry.dependencies[i] = Some(Vec::new());
                continue;
            }
            if let Some(i) = dep_type {
                if let Some(space) = trimmed.find(' ') {
                    let key = trim_chars(&trimmed[..space], &[' ', '"']);
                    let value = trim_chars(&trimmed[space + 1..], &[' ', '"']);
                    let map = entry.dependencies[i].get_or_insert_with(Vec::new);
                    match map.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = value,
                        None => map.push((key, value)),
                    }
                }
                continue;
            }
            let Some(space) = trimmed.find(' ') else {
                continue;
            };
            let key = trim_chars(&trimmed[..space], &[' ']);
            let value = trim_chars(&trimmed[space + 1..], &[' ', '"']);
            match key {
                "version" => {
                    entry.version = value;
                    if value.starts_with("workspace:") || value == "*" {
                        entry.workspace = true;
                    } else if value.starts_with("file:")
                        || value.starts_with("./")
                        || value.starts_with("../")
                    {
                        entry.file = Some(Entry::file_path_from_spec(value));
                    } else if Entry::is_git_dependency(value) {
                        entry.set_git_source(value);
                    } else if value.starts_with("npm:") {
                        entry.version = Entry::parse_npm_alias(value).1;
                    } else if Entry::is_remote_tarball(value) {
                        entry.resolved = Some(value.to_owned());
                    }
                }
                "resolved" => {
                    entry.resolved = Some(value.to_owned());
                    if Entry::is_git_dependency(value) {
                        entry.set_git_source(value);
                    }
                }
                "integrity" => entry.integrity = Some(value),
                "os" | "cpu" if value.starts_with('[') && value.ends_with(']') => {
                    let items = value[1..value.len() - 1]
                        .split(',')
                        .map(|i| trim_chars(i, &[' ', '"']));
                    if key == "os" {
                        entry.os.extend(items)
                    } else {
                        entry.cpu.extend(items)
                    }
                }
                _ => {}
            }
        }
        if let Some(entry) = current.take() {
            lock.consolidate(entry);
        }
        lock
    }

    fn consolidate(&mut self, entry: Entry<'a>) {
        if entry.specs.is_empty() {
            return;
        }
        let link = entry.link.as_deref().unwrap_or("");
        // A name and a version identify a package only when it comes from a registry.
        let (file, resolved, commit) = if entry.is_registry_entry() {
            ("", "", "")
        } else {
            (
                entry.file.unwrap_or(""),
                entry.resolved.as_deref().unwrap_or(""),
                entry.commit.as_deref().unwrap_or(""),
            )
        };
        let key = format!(
            "{}\n{}\n{link}\n{file}\n{resolved}\n{commit}",
            entry.dedupe_name(),
            entry.version
        );
        match self.by_key.get(&key) {
            Some(&i) => self.entries[i].specs.extend(entry.specs),
            None => {
                self.by_key.insert(key, self.entries.len());
                self.entries.push(entry);
            }
        }
    }
}
fn version_text(version: &str) -> Option<String> {
    let (parsed, wildcard) = semver::parse_version(version)?;
    if wildcard {
        return Some(parsed.to_string());
    }
    let v = version.trim();
    let v = v.strip_prefix('=').unwrap_or(v).trim_start();
    Some(v.strip_prefix(['v', 'V']).unwrap_or(v).to_owned())
}

/// Builds the resolution of an entry, or the reason it is skipped.
fn entry_resolution(
    e: &Entry<'_>,
    options: &MigrateOptions<'_>,
    name: &str,
) -> std::result::Result<Resolution, String> {
    let integrity = e.tarball_integrity();
    if let Some(file) = e.file {
        if file.ends_with(".tgz") || file.ends_with(".tar.gz") {
            return Ok(Resolution::LocalTarball {
                path: file.to_owned(),
                integrity,
            });
        }
        return Ok(Resolution::Folder {
            path: file.to_owned(),
        });
    }
    if let Some(link) = &e.link {
        return Ok(Resolution::Symlink { path: link.clone() });
    }
    if let Some(commit) = &e.commit {
        let Some(resolved) = e.resolved.as_deref() else {
            return Err("missing \"resolved\" field".into());
        };
        let (mut owner, mut repo) = ("", resolved);
        if let Some(i) = resolved.find("github.com/")
            && let Some((o, r)) = resolved[i + "github.com/".len()..].split_once('/')
        {
            owner = o;
            repo = r.strip_suffix(".git").unwrap_or(r);
        }
        if !owner.is_empty() && !repo.is_empty() {
            return Ok(Resolution::Github {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                committish: short_commit(commit).to_owned(),
                resolved_commit: commit.clone(),
                integrity,
            });
        }
        return Ok(Resolution::Git {
            url: resolved.to_owned(),
            committish: commit.clone(),
            resolved_commit: commit.clone(),
            integrity,
        });
    }
    let resolved_url = e.resolved.as_deref().map(Entry::url_without_hash);
    let is_registry_entry = e.is_registry_entry();
    match resolved_url {
        // yarn locks a spec like `owner/repo` to the host's tarball URL for the commit.
        Some(url) if e.has_direct_url_spec() || !is_registry_entry => {
            return Ok(Resolution::RemoteTarball {
                url: url.to_owned(),
                integrity,
            });
        }
        None if !is_registry_entry => return Err("missing \"resolved\" field".into()),
        _ => {}
    }
    let Some(version) = version_text(e.version) else {
        // An off-registry tarball (codeload and the like) installs from its URL.
        if let Some(url) = resolved_url
            && Entry::is_remote_tarball(url)
            && Entry::package_name_from_default_registry_url(url).is_none()
            && !url.starts_with(options.registry_for(name).trim_end_matches('/'))
        {
            return Ok(Resolution::RemoteTarball {
                url: url.to_owned(),
                integrity,
            });
        }
        return Err(if e.version.is_empty() {
            "missing \"version\" field".into()
        } else {
            format!(
                "invalid version \"{}\"",
                fnpm_types::text::escape_control_chars(e.version)
            )
        });
    };
    // The bare URL a fresh install records; `None` is the configured registry's location.
    let tarball_url = resolved_url
        .filter(|url| {
            !(url.starts_with("https://registry.yarnpkg.com/")
                || url.starts_with("https://registry.npmjs.org/"))
        })
        .map(str::to_owned);
    Ok(Resolution::Npm {
        version,
        tarball_url,
        integrity,
    })
}

/// The first 7 bytes of a commit (bun's abbreviated committish), cut at a character boundary.
fn short_commit(commit: &str) -> &str {
    let mut end = commit.len().min(7);
    while !commit.is_char_boundary(end) {
        end -= 1;
    }
    &commit[..end]
}

enum Bound {
    To(Option<PackageId>),
    Unlocked,
}

fn build(lock: &YarnLock<'_>, ws: &Workspace, options: &MigrateOptions<'_>) -> Result<Migration> {
    let mut warnings = Vec::new();
    let mut graph = LockGraph::new(foreign::root_package(ws));
    let mut index = graph.index();
    for member in &ws.members {
        foreign::intern(&mut graph, &mut index, foreign::workspace_package(member));
    }
    foreign::apply_root_settings(&mut graph, &ws.root.raw);
    let importer_count = graph.len();
    let workspace_named = |graph: &LockGraph, name: &str| -> Option<PackageId> {
        graph
            .workspaces
            .values()
            .copied()
            .find(|id| graph.packages[id.index()].name == name)
    };

    let mut entry_ids: Vec<Option<PackageId>> = vec![None; lock.entries.len()];
    let mut owns_deps: Vec<bool> = vec![false; lock.entries.len()];
    for (i, e) in lock.entries.iter().enumerate() {
        if e.workspace {
            // yarn writes no entry for a linked workspace; one that is there names it.
            entry_ids[i] = workspace_named(&graph, e.name);
            continue;
        }
        let resolved_url = e.resolved.as_deref().map(Entry::url_without_hash);
        let name = match (&e.commit, &e.git_repo_name) {
            (Some(_), Some(repo)) => repo.as_str(),
            _ => resolved_url
                .filter(|_| e.has_direct_url_spec())
                .and_then(Entry::package_name_from_default_registry_url)
                .unwrap_or(e.name),
        };
        let resolution = match entry_resolution(e, options, name) {
            Ok(r) => r,
            Err(reason) => {
                warnings.push(format!(
                    "skipping \"{}\" from yarn.lock: {reason}",
                    e.specs.join(", ")
                ));
                continue;
            }
        };
        let mut pkg = Package::new(name, e.version, resolution);
        if !e.os.is_empty() || !e.cpu.is_empty() {
            pkg.platform = PlatformConstraints {
                os: if e.os.is_empty() {
                    OsSet::ALL
                } else {
                    OsSet::from_list(&e.os)
                },
                cpu: if e.cpu.is_empty() {
                    CpuSet::ALL
                } else {
                    CpuSet::from_list(&e.cpu)
                },
                ..PlatformConstraints::ANY
            };
        }
        // yarn.lock records no bins; a `file:` folder or `link:` has its package.json on disk.
        if let Some(folder) = e.file.or(e.link.as_deref())
            && matches!(
                pkg.resolution,
                Resolution::Folder { .. } | Resolution::Symlink { .. }
            )
        {
            pkg.bin = foreign::local_bin(ws, options, folder);
        }
        let before = graph.len();
        let id = foreign::intern(&mut graph, &mut index, pkg);
        owns_deps[i] = graph.len() > before;
        entry_ids[i] = Some(id);
    }

    let mut spec_to_id: HashMap<String, PackageId> = HashMap::new();
    for (e, id) in lock.entries.iter().zip(&entry_ids) {
        let Some(id) = *id else { continue };
        for spec in &e.specs {
            spec_to_id.insert((*spec).to_owned(), id);
        }
        // yarn.lock spells the path as package.json does, with or without `./`.
        if let Some(file) = e.file {
            spec_to_id.insert(format!("{}@file:{file}", e.name), id);
        } else if let Some(link) = &e.link {
            spec_to_id.insert(format!("{}@link:{link}", e.name), id);
        }
    }

    // -- root and workspaces: only the exact `name@range` key binds ----------
    for pkg_index in 0..importer_count {
        let importer_path = match &graph.packages[pkg_index].resolution {
            Resolution::Workspace { path } => path.clone(),
            _ => String::new(),
        };
        let edges = std::mem::take(&mut graph.packages[pkg_index].dependencies);
        let mut kept = Vec::with_capacity(edges.len());
        for mut edge in edges {
            match bind_importer_edge(
                &graph,
                &spec_to_id,
                &importer_path,
                &mut edge,
                options.frozen,
            ) {
                Bound::To(target) => {
                    edge.target = target;
                    kept.push(edge);
                }
                Bound::Unlocked => {
                    let spec = edge.spec().ok();
                    let is_workspace =
                        matches!(spec, Some(fnpm_types::DependencySpec::Workspace { .. }))
                            || workspace_named(&graph, &edge.name).is_some();
                    if !edge.behavior.is_peer() && !edge.behavior.is_optional() && !is_workspace {
                        warnings.push(format!(
                            "yarn.lock missing \"{}@{}\", resolving it",
                            edge.name, edge.literal
                        ));
                    }
                }
            }
        }
        graph.packages[pkg_index].dependencies = kept;
    }

    // -- package dependencies ---------------------------------------------------
    for (i, e) in lock.entries.iter().enumerate() {
        let (Some(id), true) = (entry_ids[i], owns_deps[i]) else {
            continue;
        };
        let mut edges = Vec::new();
        for (deps, (_, behavior)) in e.dependencies.iter().zip(SECTIONS) {
            let Some(deps) = deps else { continue };
            for &(name, literal) in deps {
                let mut edge = DepEdge::new(name, literal, behavior);
                edge.target = spec_to_id.get(&format!("{name}@{literal}")).copied();
                // When a workspace's version satisfies the range, yarn links the workspace and
                // writes no yarn.lock entry for it.
                if edge.target.is_none()
                    && !behavior.is_peer()
                    && infer_tag(literal) == SpecTag::Npm
                    && let Some(ws_id) = foreign::linked_workspace(
                        &graph,
                        options.link_workspace_packages,
                        name,
                        literal,
                    )
                {
                    edge.target = Some(ws_id);
                    edge.behavior |= Behavior::WORKSPACE;
                }
                edges.push(edge);
            }
        }
        graph.packages[id.index()].dependencies = edges;
    }

    foreign::finish(&mut graph, ConfigVersion::V0);
    Ok(Migration {
        graph,
        write_back: Vec::new(),
        warnings,
    })
}

/// bun `bind_importer_dependency`.
fn bind_importer_edge(
    graph: &LockGraph,
    spec_to_id: &HashMap<String, PackageId>,
    importer_path: &str,
    edge: &mut DepEdge,
    frozen: bool,
) -> Bound {
    use fnpm_types::DependencySpec as Spec;
    use fnpm_types::TarballUri;
    let spec = edge.spec().ok();
    if let Some(Spec::Workspace { spec }) = &spec {
        let target = match spec {
            fnpm_types::WorkspaceSpec::Path(p) => {
                let path = foreign::join_posix(importer_path, p);
                graph.workspaces.get(&path).copied()
            }
            _ => graph
                .workspaces
                .values()
                .copied()
                .find(|id| graph.packages[id.index()].name == edge.name),
        };
        return match target {
            Some(t) => {
                edge.behavior |= Behavior::WORKSPACE;
                Bound::To(Some(t))
            }
            None => Bound::Unlocked,
        };
    }
    // Yarn never locked these peers, so they are bound the way loading a bun.lock binds them.
    if edge.behavior.is_peer() {
        if edge.behavior.is_optional_peer() {
            return Bound::To(None);
        }
        let range =
            foreign::resolve_catalog(graph, &edge.name, &edge.literal).unwrap_or(&edge.literal);
        // Overrides are not consulted: yarn already applied them to the entries.
        let bound = foreign::bind_peer_by_range(graph, &edge.name, range)
            .or_else(|| foreign::newest(graph, &foreign::peer_target(&edge.name, range).0));
        return match bound {
            Some(t) => Bound::To(Some(t)),
            None if frozen => Bound::To(None),
            None => Bound::Unlocked,
        };
    }
    let mut keys = vec![format!("{}@{}", edge.name, edge.literal)];
    // yarn.lock spells a `file:`/`link:` path relative to yarn.lock; a workspace's package.json
    // spells it relative to the workspace.
    match &spec {
        Some(Spec::Folder { path })
        | Some(Spec::Tarball {
            uri: TarballUri::Local(path),
        }) => {
            keys.push(format!(
                "{}@file:{}",
                edge.name,
                Entry::file_path_from_spec(&foreign::join_posix(importer_path, path))
            ));
        }
        Some(Spec::Symlink { path }) => {
            let joined = foreign::join_posix(importer_path, path);
            keys.push(format!(
                "{}@link:{}",
                edge.name,
                link_path_for_lockfile(&joined)
            ));
        }
        _ => {}
    }
    match keys.iter().find_map(|k| spec_to_id.get(k)) {
        Some(&id) => Bound::To(Some(id)),
        None => Bound::Unlocked,
    }
}

// -- export ---------------------------------------------------------------------

/// bun's `Behavior::cmp`: workspace edges first, then dev, optional, prod, peer.
fn behavior_rank(b: Behavior) -> (bool, bool, bool, bool, bool) {
    (
        !b.is_workspace(),
        !b.is_dev(),
        !b.is_optional(),
        !b.contains(Behavior::PROD),
        !b.is_peer(),
    )
}

/// bun `must_escape_yaml_string`.
fn must_escape(s: &str) -> bool {
    let Some(&first) = s.as_bytes().first() else {
        return true;
    };
    if !first.is_ascii_alphabetic() {
        return true;
    }
    s.starts_with("Yes")
        || s.starts_with("No")
        || s.starts_with("true")
        || s.starts_with("false")
        || s.as_bytes()[1..]
            .iter()
            .any(|b| b": \t\r\n\x0B\x0C\\\",[]".contains(b))
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn json_inner(s: &str) -> String {
    let quoted = json_string(s);
    quoted[1..quoted.len() - 1].to_owned()
}

/// The `resolved` URL bun prints (`Resolution::fmt_url`).
fn resolved_url(p: &Package) -> String {
    match &p.resolution {
        Resolution::Npm {
            version,
            tarball_url,
            ..
        } => tarball_url.clone().unwrap_or_else(|| {
            foreign::default_tarball_url(foreign::DEFAULT_REGISTRY, &p.name, version)
        }),
        Resolution::LocalTarball { path, .. } | Resolution::Folder { path } => path.clone(),
        Resolution::RemoteTarball { url, .. } => url.clone(),
        Resolution::Git {
            url,
            resolved_commit,
            ..
        } => format!("git+{url}#{resolved_commit}"),
        Resolution::Github {
            owner,
            repo,
            resolved_commit,
            ..
        } => format!("github:{owner}/{repo}#{resolved_commit}"),
        Resolution::Workspace { path } => format!("workspace:{path}"),
        Resolution::Symlink { path } => format!("link:{path}"),
        Resolution::Root => String::new(),
    }
}

/// yarn.lock v1 (bun `printer/Yarn.rs`): one entry per package, keyed by every spec that
/// requested it, with `version`, `resolved`, `integrity` and the dependency groups yarn knows.
/// The header carries an xxh3 of the sorted `name@resolution` list instead of bun's SHA-512/256
/// meta hash, and entries are keyed by the dependency's alias even for git/URL/file
/// dependencies (as yarn does), so the export migrates back to the same graph.
pub fn serialize(graph: &LockGraph) -> String {
    let n = graph.packages.len();
    // dependency edges (package, edge index) that resolved to each package
    let mut requested: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (pi, p) in graph.packages.iter().enumerate() {
        for (ei, e) in p.dependencies.iter().enumerate() {
            if let Some(t) = e.target
                && t.index() < n
            {
                requested[t.index()].push((pi, ei));
            }
        }
    }
    let edge = |(pi, ei): (usize, usize)| &graph.packages[pi].dependencies[ei];
    // yarn keys an entry by the dependency's name in package.json: the alias of an `npm:`
    // alias, and the alias of a git/URL/file dependency too (bun prints the package's name for
    // the latter, which a migration back cannot find).
    let requested_name = |r: (usize, usize)| -> &str { &edge(r).name };
    for list in &mut requested {
        list.sort_by(|&a, &b| {
            requested_name(a)
                .cmp(requested_name(b))
                .then_with(|| infer_tag(&edge(a).literal).cmp(&infer_tag(&edge(b).literal)))
                .then_with(|| edge(a).literal.cmp(&edge(b).literal))
        });
    }
    let versions: Vec<String> = graph
        .packages
        .iter()
        .map(|p| p.resolution.to_string())
        .collect();
    let mut order: Vec<usize> = (1..n).collect();
    order.sort_unstable_by(|&a, &b| {
        let (pa, pb) = (&graph.packages[a], &graph.packages[b]);
        pa.name
            .cmp(&pb.name)
            .then_with(|| pa.resolution.tag().cmp(&pb.resolution.tag()))
            .then_with(|| versions[a].cmp(&versions[b]))
            .then(a.cmp(&b))
    });

    let mut keys: Vec<String> = order.iter().map(|&i| graph.packages[i].key()).collect();
    keys.sort();
    let hash = xxhash_rust::xxh3::xxh3_64(keys.join("\n").as_bytes());
    let mut out = String::with_capacity(n * 160 + 128);
    out.push_str(
        "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n# yarn lockfile v1\n",
    );
    out.push_str(&format!("# fnpm --hash: {hash:016X}\n\n"));

    for &i in &order {
        let p = &graph.packages[i];
        let version = &versions[i];
        out.push('\n');
        let mut written: Vec<(&str, &str)> = Vec::new();
        for &r in &requested[i] {
            let (name, as_) = (requested_name(r), edge(r).literal.as_str());
            if written.contains(&(name, as_)) {
                continue;
            }
            if !written.is_empty() {
                out.push_str(", ");
            }
            written.push((name, as_));
            let needs_quote = must_escape(name)
                || as_.contains([
                    ' ', '|', '\t', '-', '/', '!', ':', '"', '\\', ',', '\n', '\r',
                ])
                || as_.starts_with("npm:");
            if needs_quote {
                out.push('"');
                out.push_str(&json_inner(name));
            } else {
                out.push_str(name);
            }
            out.push('@');
            if as_.is_empty() {
                out.push('^');
                out.push_str(version);
            } else if needs_quote {
                out.push_str(&json_inner(as_));
            } else {
                out.push_str(as_);
            }
            if needs_quote {
                out.push('"');
            }
        }
        if written.is_empty() {
            // nothing depends on it; key it by its own resolution
            out.push_str(&json_string(&p.key()));
        }
        out.push_str(":\n");
        out.push_str(&format!("  version {}\n", json_string(version)));
        out.push_str(&format!("  resolved {}\n", json_string(&resolved_url(p))));
        if let Some(integrity) = p.resolution.integrity() {
            out.push_str(&format!("  integrity {}\n", integrity.to_sri()));
        }
        let mut deps: Vec<&DepEdge> = p.dependencies.iter().collect();
        deps.sort_by(|a, b| {
            behavior_rank(a.behavior)
                .cmp(&behavior_rank(b.behavior))
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut group = "";
        for d in deps {
            let header = if d.behavior.is_optional() {
                "  optionalDependencies:\n"
            } else if d.behavior.contains(Behavior::PROD) {
                "  dependencies:\n"
            } else if d.behavior.is_dev() {
                "  devDependencies:\n"
            } else {
                continue;
            };
            if header != group {
                out.push_str(header);
                group = header;
            }
            out.push_str("    ");
            if must_escape(&d.name) {
                out.push_str(&json_string(&d.name));
            } else {
                out.push_str(&d.name);
            }
            out.push(' ');
            out.push_str(&json_string(&d.literal));
            out.push('\n');
        }
    }
    out
}
