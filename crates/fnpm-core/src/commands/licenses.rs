//! `fnpm pm licenses [list]` (bun `pm_licenses_command.rs`): the license of every package
//! fnpm.lock installs, read from the package.json in `node_modules`, grouped by license.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use fnpm_config::LogLevel;
use fnpm_linker::Layout;
use fnpm_types::{Behavior, LockGraph, PackageId, Platform, Resolution};
use fnpm_workspace::{RootSelection, SelectOptions, Selector};
use serde_json::Value;

use super::json_text;
use super::{Ctx, exit};
use crate::{CoreResult, Error, Result};

const UNKNOWN: &str = "Unknown";
const MAX_SCAN_DEPTH: usize = 64;

/// What one installed package.json says.
struct PackageInfo {
    name: Option<String>,
    version: Option<String>,
    license: String,
    homepage: Option<String>,
    author: Option<String>,
    description: Option<String>,
    path: PathBuf,
}

struct Entry {
    license: String,
    name: String,
    version: String,
    path: PathBuf,
    semver: Option<fnpm_semver::Version>,
    homepage: Option<String>,
    author: Option<String>,
    description: Option<String>,
    dev_only: bool,
}

/// Which edges a walk follows (bun `reachable::Options`).
#[derive(Clone, Copy)]
pub(super) struct Walk<'a> {
    pub(super) graph: &'a LockGraph,
    pub(super) dev: bool,
    pub(super) optional: bool,
    pub(super) peer: bool,
    pub(super) optional_peer: bool,
    /// Skip packages disabled for this platform (`None`: every package counts).
    pub(super) platform: Option<&'a Platform>,
    pub(super) follow_workspace_edges: bool,
}

impl Walk<'_> {
    fn group_enabled(&self, b: Behavior) -> bool {
        if b.is_optional_peer() {
            self.optional_peer
        } else if b.is_peer() {
            self.peer
        } else if b.is_optional() {
            self.optional
        } else if b.is_dev() {
            self.dev
        } else {
            true
        }
    }

    /// An optional peer whose package only an omitted group of the same package asked for.
    fn omits_optional_peer(&self, parent: PackageId, edge: &fnpm_types::DepEdge) -> bool {
        if !edge.behavior.is_optional_peer() || (self.dev && self.optional) {
            return false;
        }
        let mut lender_omitted = false;
        for sibling in &self.graph.packages[parent.index()].dependencies {
            if sibling.name == edge.name && !sibling.behavior.is_peer() {
                if self.group_enabled(sibling.behavior) {
                    return false;
                }
                lender_omitted = true;
            }
        }
        lender_omitted
    }

    fn follows(&self, b: Behavior) -> bool {
        !(b.is_workspace() && !self.follow_workspace_edges) && self.group_enabled(b)
    }

    fn admit(&self, target: PackageId, b: Behavior, seen: &mut [bool], work: &mut Vec<PackageId>) {
        let Some(slot) = seen.get_mut(target.index()) else {
            return;
        };
        if *slot {
            return;
        }
        let p = &self.graph.packages[target.index()];
        if let Some(platform) = self.platform
            && !p.platform.matches_for_edge(platform, b.is_optional())
        {
            return;
        }
        *slot = true;
        work.push(target);
    }

    fn drain(&self, seen: &mut [bool], work: &mut Vec<PackageId>) {
        while let Some(parent) = work.pop() {
            let p = &self.graph.packages[parent.index()];
            let local = p.resolution.is_local_tree();
            for e in &p.dependencies {
                let Some(t) = e.target else { continue };
                // Only local packages have devDependencies that install.
                if e.behavior.is_dev() && !local {
                    continue;
                }
                if self.follows(e.behavior) && !self.omits_optional_peer(parent, e) {
                    self.admit(t, e.behavior, seen, work);
                }
            }
        }
    }

    /// Packages reachable from `roots`.
    pub(super) fn packages_from(&self, roots: &[PackageId]) -> Vec<bool> {
        let mut seen = vec![false; self.graph.packages.len()];
        let mut work = Vec::new();
        for &r in roots {
            if !seen[r.index()] {
                seen[r.index()] = true;
                work.push(r);
            }
        }
        self.drain(&mut seen, &mut work);
        seen
    }

    /// What the devDependencies of `roots` (and of the workspaces they own) pull in.
    fn dev_packages_from(&self, roots: &[PackageId]) -> Vec<bool> {
        let mut seen = vec![false; self.graph.packages.len()];
        let mut importers = Vec::new();
        for &r in roots {
            if !seen[r.index()] {
                seen[r.index()] = true;
                importers.push(r);
            }
        }
        let mut work = Vec::new();
        while let Some(importer) = importers.pop() {
            for e in &self.graph.packages[importer.index()].dependencies {
                let Some(t) = e.target else { continue };
                if e.behavior.is_workspace() {
                    if self.follow_workspace_edges && !seen[t.index()] {
                        seen[t.index()] = true;
                        importers.push(t);
                    }
                    continue;
                }
                if e.behavior.is_dev() && self.follows(e.behavior) {
                    self.admit(t, e.behavior, &mut seen, &mut work);
                }
            }
        }
        self.drain(&mut seen, &mut work);
        seen
    }
}

fn str_of(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn string_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(str_of)
}

fn string_or_type(v: &Value) -> Option<String> {
    str_of(v)
        .or_else(|| string_field(v, "type"))
        .or_else(|| string_field(v, "name"))
}

fn parse_license_field(field: &Value) -> Option<String> {
    if let Some(value) = string_or_type(field) {
        return Some(value);
    }
    let mut collected: Vec<String> = field
        .as_array()?
        .iter()
        .filter_map(string_or_type)
        .collect();
    match collected.len() {
        0 => None,
        1 => collected.pop(),
        _ => Some(format!("({})", collected.join(" OR "))),
    }
}

fn license_of(json: &Value) -> String {
    json.get("license")
        .and_then(parse_license_field)
        .or_else(|| json.get("licenses").and_then(parse_license_field))
        .unwrap_or_else(|| UNKNOWN.to_owned())
}

fn repository_of(json: &Value) -> Option<String> {
    let repository = json.get("repository")?;
    str_of(repository).or_else(|| string_field(repository, "url"))
}

fn author_of(json: &Value) -> Option<String> {
    let author = json.get("author")?;
    if let Some(s) = str_of(author) {
        return Some(s);
    }
    let mut out = String::new();
    if let Some(name) = string_field(author, "name") {
        out.push_str(&name);
    }
    if let Some(email) = string_field(author, "email") {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("<{email}>"));
    }
    if let Some(url) = string_field(author, "url") {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("({url})"));
    }
    (!out.is_empty()).then_some(out)
}

/// The package.json in `dir`; `None` when it cannot be read. One that does not parse is a
/// package with an Unknown license.
fn read_package_info(dir: &Path) -> Option<PackageInfo> {
    let bytes = std::fs::read(dir.join("package.json")).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    Some(match json_text::parse_package_json(&text) {
        Ok(json) => PackageInfo {
            name: string_field(&json, "name"),
            version: string_field(&json, "version"),
            license: license_of(&json),
            homepage: string_field(&json, "homepage").or_else(|| repository_of(&json)),
            author: author_of(&json),
            description: string_field(&json, "description"),
            path: dir.to_path_buf(),
        },
        Err(_) => PackageInfo {
            name: None,
            version: None,
            license: UNKNOWN.to_owned(),
            homepage: None,
            author: None,
            description: None,
            path: dir.to_path_buf(),
        },
    })
}

/// The sorted entry names of `node_modules/.fnpm`, read the first time a hoisted install has a
/// package somewhere else than its tree location.
struct StoreDir {
    entries: Option<Vec<String>>,
}

impl StoreDir {
    /// The entry for `key`, with or without a peer hash suffix.
    fn lookup(&mut self, root: &Path, key: &str) -> Option<String> {
        let entries = self.entries.get_or_insert_with(|| {
            sorted_dir(&root.join("node_modules").join(".fnpm"))
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        });
        let i = entries.partition_point(|e| e.as_str() < key);
        let entry = entries.get(i)?;
        let peer_suffixed = entry.len() > key.len()
            && entry.starts_with(key)
            && entry.as_bytes()[key.len()] == b'+';
        (entry == key || peer_suffixed).then(|| entry.clone())
    }
}

/// Every package.json under `node_modules`, by `name@version`; the first copy (in name order)
/// wins. Built the first time a package is not where the lockfile puts it.
struct DiskIndex {
    entries: Option<HashMap<String, PackageInfo>>,
}

fn sorted_dir(path: &Path) -> Vec<(String, bool)> {
    let Ok(rd) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut out: Vec<(String, bool)> = rd
        .flatten()
        .map(|e| {
            let link = e.file_type().is_ok_and(|t| t.is_symlink());
            (e.file_name().to_string_lossy().into_owned(), link)
        })
        .collect();
    out.sort();
    out
}

impl DiskIndex {
    fn take(&mut self, root: &Path, name: &str, version: &str) -> Option<PackageInfo> {
        let entries = self.entries.get_or_insert_with(|| {
            let mut out = HashMap::new();
            Self::scan_node_modules(&root.join("node_modules"), 0, &mut out);
            out
        });
        entries.remove(&format!("{name}@{version}"))
    }

    fn scan_node_modules(nm: &Path, depth: usize, out: &mut HashMap<String, PackageInfo>) {
        for (name, link) in sorted_dir(nm) {
            if name.starts_with('.') || (depth > 0 && link) {
                continue;
            }
            let path = nm.join(&name);
            if !name.starts_with('@') {
                Self::scan_package(&path, depth, out);
                continue;
            }
            for (scoped, scoped_link) in sorted_dir(&path) {
                if scoped.starts_with('.') || (depth > 0 && scoped_link) {
                    continue;
                }
                Self::scan_package(&path.join(scoped), depth, out);
            }
        }
    }

    fn scan_package(dir: &Path, depth: usize, out: &mut HashMap<String, PackageInfo>) {
        if let Some(info) = read_package_info(dir)
            && let (Some(name), Some(version)) = (&info.name, &info.version)
        {
            out.entry(format!("{name}@{version}")).or_insert(info);
        }
        if depth < MAX_SCAN_DEPTH {
            Self::scan_node_modules(&dir.join("node_modules"), depth + 1, out);
        }
    }
}

/// Unknown last; otherwise case-insensitive ignoring a leading `(`, so identical licenses stay
/// adjacent for grouping.
fn license_order(a: &str, b: &str) -> Ordering {
    (a == UNKNOWN)
        .cmp(&(b == UNKNOWN))
        .then_with(|| {
            let ak = a.strip_prefix('(').unwrap_or(a).to_ascii_lowercase();
            let bk = b.strip_prefix('(').unwrap_or(b).to_ascii_lowercase();
            ak.cmp(&bk)
        })
        .then_with(|| a.cmp(b))
}

/// The version column: the npm version, a local path as written, or the resolution.
fn version_of(resolution: &Resolution) -> String {
    match resolution {
        Resolution::LocalTarball { path, .. } | Resolution::Folder { path } => path.clone(),
        r => r.to_string(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn elapsed(start: Instant) -> String {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    if ms >= 1000.0 {
        format!("[{:.2}s]", ms / 1000.0)
    } else {
        format!("[{ms:.2}ms]")
    }
}

/// Prints `error: <message>` (unless `--silent`) and fails.
fn fail(loud: bool, message: &str, note: Option<&str>) -> Error {
    if loud {
        eprintln!("error: {message}");
        if let Some(note) = note {
            eprintln!("note: {note}");
        }
    }
    Error::Reported
}

/// `fnpm pm licenses [list|ls] [--json] [--long] [--dev | --prod] [--filter <pattern>]`.
pub(crate) fn run(cx: &Ctx) -> Result<CoreResult> {
    let start = Instant::now();
    let loud = cx.pm.log_level != LogLevel::Silent;
    let args = &cx.req.positionals;
    if let Some(sub) = args.first() {
        if sub != "list" && sub != "ls" {
            return Err(fail(
                loud,
                &format!("unknown subcommand \"{sub}\" for fnpm pm licenses"),
                Some("did you mean 'fnpm pm licenses list'?"),
            ));
        }
        if args.len() > 1 {
            return Err(fail(
                loud,
                &format!("fnpm pm licenses {sub} does not take arguments"),
                None,
            ));
        }
    }

    let (project, ws, graph) = match cx.load() {
        Ok((p, ws, Some(g))) => (p, ws, g),
        Ok(_) => {
            return Err(fail(
                loud,
                "missing lockfile, nothing to list",
                Some("run 'fnpm install' first"),
            ));
        }
        Err(e) => return Err(e),
    };
    let config = &project.config;
    let json = cx.req.json;
    let dev_only = cx.flag("dev");
    let dev = !config.production && !config.omit.dev;
    if dev_only && !dev {
        return Err(fail(
            loud,
            "--dev cannot be combined with --prod or --omit=dev",
            None,
        ));
    }
    let root = &project.root_dir;
    if !root.join("node_modules").exists() {
        return Err(fail(
            loud,
            "node_modules not found, nothing to list",
            Some("run 'fnpm install' first"),
        ));
    }

    let filters = cx.strings("filter");
    let (roots, follow_workspace_edges): (Vec<PackageId>, bool) = if filters.is_empty() {
        let here = ws
            .member_for_dir(&cx.req.cwd)
            .map(|m| m.path.clone())
            .unwrap_or_default();
        let id = graph
            .workspaces
            .get(&here)
            .copied()
            .unwrap_or(PackageId::ROOT);
        (vec![id], true)
    } else {
        let selectors = Selector::parse_all(&filters)?;
        let selection = fnpm_workspace::select_with(
            &ws,
            &selectors,
            &cx.req.cwd,
            &SelectOptions {
                root: RootSelection::Implicit,
                ..SelectOptions::default()
            },
        );
        let ids: Vec<PackageId> = selection
            .selected
            .iter()
            .filter_map(|&i| {
                let path = if i == usize::MAX {
                    String::new()
                } else {
                    ws.members[i].path.clone()
                };
                graph.workspaces.get(&path).copied()
            })
            .collect();
        if ids.is_empty() {
            return Err(fail(
                loud,
                &fnpm_workspace::unmatched_message(&filters),
                None,
            ));
        }
        if loud && !selection.unmatched_patterns.is_empty() {
            let unmatched: Vec<&String> = selection
                .unmatched_patterns
                .iter()
                .filter_map(|&i| filters.get(i))
                .collect();
            eprintln!("warn: {}", fnpm_workspace::unmatched_message(&unmatched));
        }
        (ids, false)
    };

    let summary = loud && cx.pm.summary;
    let mut lines: Vec<String> = Vec::new();
    if summary && !json {
        lines.push(format!(
            "fnpm pm licenses v{}",
            cx.string("cliVersion")
                .unwrap_or_else(|| crate::version().to_owned())
        ));
        lines.push(String::new());
    }

    let view = crate::install::link_view(&graph);
    let walk = Walk {
        graph: &view,
        dev,
        optional: !config.omit.optional,
        peer: !config.omit.peer,
        optional_peer: !config.omit.peer,
        platform: Some(&config.platform),
        follow_workspace_edges,
    };
    let wanted = if dev_only {
        walk.dev_packages_from(&roots)
    } else {
        walk.packages_from(&roots)
    };
    let production = (!json && dev).then(|| Walk { dev: false, ..walk }.packages_from(&roots));

    let (_, layout) = super::maintenance::current_layout(&project, &graph)?;
    let mut tree_location: Vec<Option<PathBuf>> = vec![None; view.packages.len()];
    let mut store_location: Vec<Option<PathBuf>> = vec![None; view.packages.len()];
    let isolated = matches!(layout, Layout::Isolated(_));
    match &layout {
        Layout::Hoisted(tree) => {
            for node in &tree.nodes {
                let slot = &mut tree_location[node.package.index()];
                if slot.is_none() {
                    *slot = Some(nested_dir(root, &node.path));
                }
            }
        }
        Layout::Isolated(iso) => {
            for e in iso.entries.iter().skip(1) {
                let slot = &mut store_location[e.package.index()];
                if slot.is_none() {
                    let name = &view.packages[e.package.index()].name;
                    *slot = Some(
                        root.join("node_modules")
                            .join(".fnpm")
                            .join(&e.key)
                            .join("node_modules")
                            .join(name),
                    );
                }
            }
        }
    }

    // Bundled dependencies unpack inside their parent's directory.
    for _ in 0..2 {
        for (index, p) in view.packages.iter().enumerate() {
            let Some(parent) = tree_location[index]
                .clone()
                .or_else(|| store_location[index].clone())
            else {
                continue;
            };
            for e in &p.dependencies {
                if let Some(t) = e.target
                    && e.behavior.is_bundled()
                    && tree_location[t.index()].is_none()
                {
                    tree_location[t.index()] = Some(parent.join("node_modules").join(&e.name));
                }
            }
        }
    }
    let mut store = StoreDir { entries: None };
    let basename = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut disk = DiskIndex { entries: None };
    let mut entries: Vec<Entry> = Vec::new();
    // Packages whose installed copy is not the version in fnpm.lock: listed under the version
    // in the installed package.json.
    let mut other_versions: Vec<Entry> = Vec::new();
    let mut missing = 0usize;
    let mut checked = 0usize;
    for (index, p) in view.packages.iter().enumerate() {
        if !wanted[index] {
            continue;
        }
        if matches!(
            p.resolution,
            Resolution::Root | Resolution::Workspace { .. } | Resolution::Symlink { .. }
        ) {
            continue;
        }
        checked += 1;
        let mut version = version_of(&p.resolution);
        let is_npm = matches!(p.resolution, Resolution::Npm { .. });

        let mut info = if isolated {
            store_location[index].as_deref().and_then(read_package_info)
        } else {
            None
        };
        let mut other_version = None;
        if info.is_none() {
            if let Some(dir) = &tree_location[index] {
                info = read_package_info(dir);
            }
            if is_npm {
                other_version =
                    info.take_if(|i| i.version.as_deref().is_some_and(|v| v != version));
            }
        }
        if info.is_none() && !isolated {
            let key = fnpm_linker::store_key::store_key(&p.name, &p.resolution, &basename);
            info = store.lookup(root, &key).and_then(|entry| {
                read_package_info(
                    &root
                        .join("node_modules")
                        .join(".fnpm")
                        .join(entry)
                        .join("node_modules")
                        .join(&p.name),
                )
            });
        }
        if info.is_none() && is_npm {
            info = disk.take(root, &p.name, &version);
        }
        let (info, list) = match (info, other_version) {
            (Some(info), _) => (info, &mut entries),
            (None, Some(info)) => {
                if let Some(installed) = &info.version {
                    version = installed.clone();
                }
                (info, &mut other_versions)
            }
            (None, None) => {
                missing += 1;
                continue;
            }
        };
        list.push(Entry {
            license: info.license,
            name: p.name.clone(),
            semver: if is_npm {
                fnpm_semver::Version::parse(&version).ok()
            } else {
                None
            },
            version,
            path: info.path,
            homepage: info.homepage,
            author: info.author,
            description: info.description,
            dev_only: production.as_ref().is_some_and(|prod| !prod[index]),
        });
    }
    // A copy already listed for the package it matches is not listed twice; the package it
    // was found for counts as missing.
    for entry in other_versions {
        if entries.iter().any(|listed| listed.path == entry.path) {
            missing += 1;
        } else {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| {
        license_order(&a.license, &b.license)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| match (&a.semver, &b.semver) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then_with(|| a.version.cmp(&b.version))
    });

    if json {
        lines.extend(print_json(&entries).lines().map(str::to_owned));
    } else {
        print_text(
            &mut lines,
            &entries,
            cx.flag("long"),
            checked,
            summary,
            start,
        );
    }
    if missing > 0 && loud {
        if missing == 1 {
            eprintln!("warn: 1 package in fnpm.lock is not installed and was skipped");
        } else {
            eprintln!("warn: {missing} packages in fnpm.lock are not installed and were skipped");
        }
        eprintln!("note: run 'fnpm install' first");
    }
    Ok(exit(0, Value::Null, lines))
}

/// `node_modules/a/node_modules/b` for the tree path `a/b` (`@s/a/b` keeps the scope).
fn nested_dir(root: &Path, tree_path: &str) -> PathBuf {
    let mut dir = root.join("node_modules");
    let mut parts = tree_path.split('/').peekable();
    let mut first = true;
    while let Some(part) = parts.next() {
        if !first {
            dir = dir.join("node_modules");
        }
        first = false;
        dir = dir.join(part);
        if part.starts_with('@')
            && let Some(name) = parts.next()
        {
            dir = dir.join(name);
        }
    }
    dir
}

fn escape(s: &str) -> String {
    fnpm_types::text::escape_control_chars(s)
}

fn print_text(
    lines: &mut Vec<String>,
    entries: &[Entry],
    long: bool,
    checked: usize,
    summary: bool,
    start: Instant,
) {
    if entries.is_empty() {
        lines.push(if summary {
            format!(
                "No packages to list (checked {checked} package{} in fnpm.lock) {}",
                plural(checked),
                elapsed(start)
            )
        } else {
            "No packages to list".to_owned()
        });
        return;
    }
    let mut licenses = 0;
    for group in entries.chunk_by(|a, b| a.license == b.license) {
        if licenses > 0 {
            lines.push(String::new());
        }
        licenses += 1;
        lines.push(format!("{} ({})", escape(&group[0].license), group.len()));
        for (i, entry) in group.iter().enumerate() {
            let last = i + 1 == group.len();
            let mut line = format!(
                "{} {}@{}",
                if last { "└──" } else { "├──" },
                escape(&entry.name),
                escape(&entry.version)
            );
            if entry.dev_only {
                line.push_str(" (dev)");
            }
            lines.push(line);
            if long {
                for field in [&entry.author, &entry.description, &entry.homepage]
                    .into_iter()
                    .flatten()
                {
                    let lead = if last { "    " } else { "│   " };
                    lines.push(format!("{lead}{}", escape(field)));
                }
            }
        }
    }
    if summary {
        lines.push(String::new());
        lines.push(format!(
            "{} package{} across {licenses} license{} (checked {checked} package{} in fnpm.lock) {}",
            entries.len(),
            plural(entries.len()),
            plural(licenses),
            plural(checked),
            elapsed(start)
        ));
    }
}

fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn print_json(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return "{}".to_owned();
    }
    let mut out = String::from("{\n");
    for (license_index, same_license) in entries.chunk_by(|a, b| a.license == b.license).enumerate()
    {
        let license = &same_license[0].license;
        if license_index > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!("  {}: [", js(license)));
        for (name_index, same_name) in same_license.chunk_by(|a, b| a.name == b.name).enumerate() {
            if name_index > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n    {{\n      \"name\": {}",
                js(&same_name[0].name)
            ));
            let mut kept: Vec<&Entry> = Vec::new();
            for entry in same_name {
                if kept
                    .last()
                    .is_none_or(|previous| previous.version != entry.version)
                {
                    kept.push(entry);
                }
            }
            let versions: Vec<String> = kept.iter().map(|e| js(&e.version)).collect();
            let paths: Vec<String> = kept.iter().map(|e| js(&e.path.to_string_lossy())).collect();
            out.push_str(&format!(",\n      \"versions\": [{}]", versions.join(", ")));
            out.push_str(&format!(",\n      \"paths\": [{}]", paths.join(", ")));
            out.push_str(&format!(",\n      \"license\": {}", js(license)));
            let newest = &same_name[same_name.len() - 1];
            for (key, value) in [
                ("author", &newest.author),
                ("description", &newest.description),
                ("homepage", &newest.homepage),
            ] {
                if let Some(value) = value {
                    out.push_str(&format!(",\n      \"{key}\": {}", js(value)));
                }
            }
            out.push_str("\n    }");
        }
        out.push_str("\n  ]");
    }
    out.push_str("\n}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn licenses_from_manifests() {
        let of = |v: Value| license_of(&v);
        assert_eq!(of(serde_json::json!({"license": "MIT"})), "MIT");
        assert_eq!(
            of(serde_json::json!({"licenses": [{"type": "MIT"}, {"type": "Apache-2.0"}]})),
            "(MIT OR Apache-2.0)"
        );
        assert_eq!(
            of(serde_json::json!({"license": "", "licenses": [{"type": "ISC"}]})),
            "ISC"
        );
        assert_eq!(of(serde_json::json!({"license": 42})), UNKNOWN);
        assert_eq!(
            author_of(&serde_json::json!({"author": {"name": "A", "email": "a@x", "url": "u"}})),
            Some("A <a@x> (u)".into())
        );
        assert_eq!(license_order("mit", "MIT"), Ordering::Greater);
        assert_eq!(license_order("Unknown", "zlib"), Ordering::Greater);
        assert_eq!(license_order("(MIT OR X)", "SEE"), Ordering::Less);
        assert_eq!(
            nested_dir(Path::new("/p"), "a/@s/b/c"),
            Path::new("/p/node_modules/a/node_modules/@s/b/node_modules/c")
        );
    }
}
