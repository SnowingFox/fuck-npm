//! `fnpm why <package>` / `fnpm pm why` / `fnpm-lock why` (bun `why_command.rs`): every
//! version of the matching packages, and the chain of dependents that brings each one in.

use std::cmp::Ordering;
use std::collections::HashMap;

use fnpm_linker::Layout;
use fnpm_types::{Behavior, LockGraph, PackageId, Resolution};
use serde_json::{Value, json};

use super::{Ctx, exit};
use crate::{CoreResult, Error, Result};

const PREFIX_LAST: &str = "  └─ ";
const PREFIX_MIDDLE: &str = "  ├─ ";
const PREFIX_CONTINUE: &str = "  │  ";
const PREFIX_SPACE: &str = "     ";

/// The label of a root package.json with no "name". It has spaces, so it is never a package
/// name.
const UNNAMED_ROOT: &str = "the root package";

#[derive(Clone, Copy, PartialEq, Eq)]
enum DependencyType {
    Dev,
    Prod,
    Peer,
    Optional,
    OptionalPeer,
}

impl DependencyType {
    fn of(b: Behavior) -> Self {
        if b.contains(Behavior::DEV) {
            Self::Dev
        } else if b.contains(Behavior::OPTIONAL) && b.contains(Behavior::PEER) {
            Self::OptionalPeer
        } else if b.contains(Behavior::OPTIONAL) {
            Self::Optional
        } else if b.contains(Behavior::PEER) {
            Self::Peer
        } else {
            Self::Prod
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Prod => 4,
            Self::Peer => 3,
            Self::OptionalPeer => 2,
            Self::Optional => 1,
            Self::Dev => 0,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Peer => "peer",
            Self::Optional => "optional",
            Self::OptionalPeer => "optional peer",
            Self::Prod => "",
        }
    }
}

#[derive(Clone)]
struct DependentInfo {
    name: String,
    version: String,
    /// The name this dependent uses for the package, if it is not the package's own name.
    alias: Option<String>,
    spec: String,
    dep_type: DependencyType,
    pkg_id: PackageId,
    workspace: bool,
}

fn specificity(spec: &str) -> u8 {
    let Some(first) = spec.bytes().next() else {
        return 9;
    };
    if first == b'*' {
        1
    } else if spec.contains(".x") {
        5
    } else if spec.contains(['<', '>', '=']) {
        6
    } else if first == b'~' {
        7
    } else if first == b'^' {
        8
    } else if spec.contains("workspace:") {
        9
    } else if first.is_ascii_digit() {
        10
    } else {
        3
    }
}

/// Most specific range first, then by dependency type, then by name.
fn cmp_dependents(a: &DependentInfo, b: &DependentInfo) -> Ordering {
    specificity(&b.spec)
        .cmp(&specificity(&a.spec))
        .then_with(|| b.dep_type.priority().cmp(&a.dep_type.priority()))
        .then_with(|| a.name.cmp(&b.name))
}

enum NamePattern<'a> {
    Exact(&'a str),
    Prefix(&'a str),
    Suffix(&'a str),
    Middle(&'a str, &'a str),
    Contains(&'a str),
    Invalid,
}

struct GlobPattern<'a> {
    name: NamePattern<'a>,
    version_pattern: &'a str,
    version_query: Option<fnpm_semver::Range>,
}

impl<'a> GlobPattern<'a> {
    fn new(pattern: &'a str) -> Self {
        let (name, version) = fnpm_types::name::split_name_and_maybe_version(pattern);
        let version_pattern = version.unwrap_or("");
        // Something that is not a range (`beta`, `workspace:packages/a`) is matched against
        // the start of the printed resolution.
        // (bun's range parser reads those as an empty range).
        let version_query = version_pattern
            .contains(|c: char| c.is_ascii_digit())
            .then(|| fnpm_semver::Range::parse(version_pattern).ok())
            .flatten();
        GlobPattern {
            name: Self::name_pattern(name),
            version_pattern,
            version_query,
        }
    }

    fn name_pattern(pattern: &'a str) -> NamePattern<'a> {
        let Some(wildcard) = pattern.find('*') else {
            return NamePattern::Exact(pattern);
        };
        if pattern.len() >= 3 && pattern.starts_with('*') && pattern.ends_with('*') {
            let inner = &pattern[1..pattern.len() - 1];
            if !inner.is_empty() && !inner.contains('*') {
                return NamePattern::Contains(inner);
            }
        }
        if wildcard == pattern.len() - 1 {
            return NamePattern::Prefix(&pattern[..wildcard]);
        }
        if wildcard == 0 {
            return NamePattern::Suffix(&pattern[1..]);
        }
        if pattern[wildcard + 1..].contains('*') {
            return NamePattern::Invalid;
        }
        NamePattern::Middle(&pattern[..wildcard], &pattern[wildcard + 1..])
    }

    fn matches_name(&self, name: &str) -> bool {
        match self.name {
            NamePattern::Exact(exact) => name == exact,
            NamePattern::Prefix(p) => name.starts_with(p),
            NamePattern::Suffix(s) => name.ends_with(s),
            NamePattern::Middle(p, s) => name.starts_with(p) && name.ends_with(s),
            NamePattern::Contains(c) => name.contains(c),
            NamePattern::Invalid => false,
        }
    }

    fn matches_version(&self, version: Option<&str>, resolution: &str) -> bool {
        if matches!(self.version_pattern, "" | "latest" | "*") {
            return true;
        }
        if let (Some(range), Some(version)) = (&self.version_query, version)
            && let Ok(v) = fnpm_semver::Version::parse(version)
        {
            return range.satisfies(&v);
        }
        resolution.starts_with(self.version_pattern)
    }
}

/// bun's printed resolution: nothing for the root, the version for npm, the path of a local
/// folder or tarball, `workspace:<path>`, otherwise the resolution itself.
fn resolution_label(r: &Resolution) -> String {
    match r {
        Resolution::Root => String::new(),
        Resolution::LocalTarball { path, .. } | Resolution::Folder { path } => path.clone(),
        Resolution::Workspace { path } => format!("workspace:{path}"),
        r => r.to_string(),
    }
}

fn escape(s: &str) -> String {
    fnpm_types::text::escape_control_chars(s)
}

fn usage(version: &str) -> Vec<String> {
    let text = format!(
        "fnpm why v{version}
Explain why a package is installed

Arguments:
  <package>     The package name or alias to explain (supports glob patterns like '@org/*')

Options:
  --top         Show only the top dependency tree instead of nested ones
  --depth <NUM> Maximum depth of the dependency tree to display

Examples:
  $ fnpm why react
  $ fnpm why \"@types/*\" --depth 2
  $ fnpm why \"*-lodash\" --top"
    );
    text.lines().map(str::to_owned).collect()
}

fn package_line(prefix: &str, d: &DependentInfo) -> String {
    let mut s = String::from(prefix);
    let label = d.dep_type.label();
    if !label.is_empty() {
        s.push_str(label);
        s.push(' ');
    }
    s.push_str(&escape(&d.name));
    if !d.version.is_empty() {
        s.push('@');
        s.push_str(&if d.workspace {
            "workspace".to_owned()
        } else {
            escape(&d.version)
        });
    }
    if !d.spec.is_empty() {
        match &d.alias {
            Some(alias) => s.push_str(&format!(
                " (requires {}@{})",
                escape(alias),
                escape(&d.spec)
            )),
            None => s.push_str(&format!(" (requires {})", escape(&d.spec))),
        }
    }
    s
}

fn node_json(d: &DependentInfo, dependents: Vec<Value>) -> Value {
    json!({
        "name": d.name,
        "version": d.version,
        "spec": d.spec,
        "type": d.dep_type.label(),
        "dependents": dependents,
    })
}

struct TreeContext<'a> {
    all_dependents: &'a HashMap<PackageId, Vec<DependentInfo>>,
    max_depth: usize,
    /// The packages being expanded right now; the value records whether a `*circular` pointed
    /// at it.
    path_tracker: HashMap<PackageId, bool>,
    /// The `(depth, cutoff)` of each printed subtree. A repeat that is not shallower than
    /// `cutoff` is printed as `*deduped`.
    expanded: HashMap<PackageId, (usize, usize)>,
    lines: Vec<String>,
}

/// Returns `(lines printed, cutoff, json)`. See `TreeContext::expanded`.
fn print_dependency_tree(
    cx: &mut TreeContext<'_>,
    current: PackageId,
    prefix: &str,
    depth: usize,
    printed_break_line: bool,
    parent_is_workspace: bool,
) -> (usize, usize, Vec<Value>) {
    if let Some(pointed_at) = cx.path_tracker.get_mut(&current) {
        *pointed_at = true;
        cx.lines.push(format!("{prefix}└─ *circular"));
        return (1, 0, Vec::new());
    }
    if let Some(&(_, cutoff)) = cx.expanded.get(&current).filter(|e| depth >= e.1) {
        cx.lines.push(format!("{prefix}└─ *deduped"));
        return (1, cutoff, Vec::new());
    }
    cx.path_tracker.insert(current, false);
    let (mut lines, mut cutoff) = (0usize, 0usize);
    let mut nodes = Vec::new();
    if let Some(dependents) = cx.all_dependents.get(&current) {
        // The root depends on every workspace, so it is left out of a workspace's dependents.
        let mut sorted: Vec<DependentInfo> = dependents
            .iter()
            .filter(|d| !(parent_is_workspace && d.version.is_empty()))
            .cloned()
            .collect();
        sorted.sort_by(cmp_dependents);
        let len = sorted.len();
        for (i, d) in sorted.iter().enumerate() {
            if depth >= cx.max_depth {
                cx.lines
                    .push(format!("{prefix}└─ (deeper dependencies hidden)"));
                lines += 1;
                cutoff = depth;
                break;
            }
            let last = i + 1 == len;
            let (branch, next) = if last {
                ("└─ ", "   ")
            } else {
                ("├─ ", "│  ")
            };
            cx.lines.push(package_line(&format!("{prefix}{branch}"), d));
            lines += 1;
            let print_break_line = last && len > 1 && !printed_break_line;
            let (sub_lines, sub_cutoff, sub) = print_dependency_tree(
                cx,
                d.pkg_id,
                &format!("{prefix}{next}"),
                depth + 1,
                printed_break_line || print_break_line,
                d.workspace,
            );
            lines += sub_lines;
            // A dependent repeats one level deeper than this package does.
            cutoff = cutoff.max(sub_cutoff.saturating_sub(1));
            nodes.push(node_json(d, sub));
            if print_break_line {
                cx.lines.push(prefix.to_owned());
            }
        }
    }
    if cx.path_tracker.remove(&current) == Some(true) && cutoff > 0 {
        // Their `*circular` replaced this cut-off subtree, and a repeat reaches it one level
        // further down.
        for (their_depth, their_cutoff) in cx.expanded.values_mut() {
            if *their_depth > depth {
                *their_cutoff = (*their_cutoff).max(cutoff - 1);
            }
        }
    }
    // A one-line subtree is no longer than the `*deduped` marker, so it is repeated instead.
    if lines > 1 {
        cx.expanded.insert(current, (depth, cutoff));
    }
    (lines, cutoff, nodes)
}

/// A peer dependency leads to the copy that each placement of its owner loads, whichever one
/// the resolver bound it to: `(package index, edge index)` to those copies.
fn served_peers(view: &LockGraph, layout: &Layout) -> HashMap<(usize, usize), Vec<PackageId>> {
    let mut out: HashMap<(usize, usize), Vec<PackageId>> = HashMap::new();
    let mut add = |owner: PackageId, edge: usize, copy: PackageId| {
        let copies = out.entry((owner.index(), edge)).or_default();
        if !copies.contains(&copy) {
            copies.push(copy);
        }
    };
    match layout {
        Layout::Hoisted(tree) => {
            let mut paths: Vec<(String, PackageId)> = tree
                .nodes
                .iter()
                .map(|n| (n.path.clone(), n.package))
                .collect();
            paths.sort_by(|a, b| a.0.cmp(&b.0));
            for node in &tree.nodes {
                let p = &view.packages[node.package.index()];
                for (edge, e) in p.dependencies.iter().enumerate() {
                    if e.behavior.is_peer()
                        && let Some(copy) =
                            fnpm_lockfile::tree::resolve_paths(&paths, &node.path, &e.name)
                    {
                        add(node.package, edge, copy);
                    }
                }
            }
        }
        Layout::Isolated(iso) => {
            for entry in &iso.entries {
                let p = &view.packages[entry.package.index()];
                for (edge, e) in p.dependencies.iter().enumerate() {
                    if !e.behavior.is_peer() {
                        continue;
                    }
                    for (alias, linked) in &entry.deps {
                        if *alias == e.name
                            && let Some(copy) = iso.entries.get(*linked as usize)
                        {
                            add(entry.package, edge, copy.package);
                        }
                    }
                }
            }
        }
    }
    out
}

/// `fnpm why <pattern> [--top] [--depth N]`.
pub(crate) fn why(cx: &Ctx) -> Result<CoreResult> {
    let version = cx
        .string("cliVersion")
        .unwrap_or_else(|| crate::version().to_owned());
    let Some(pattern) = cx.req.positionals.first().cloned() else {
        return Ok(exit(1, Value::Null, usage(&version)));
    };
    let (project, _, graph) = cx.load_locked("nothing to explain")?;
    let top_only = cx.flag("top");
    let max_depth = if top_only {
        1
    } else {
        cx.number("depth")
            .map_or(100, |d| usize::try_from(d).unwrap_or(usize::MAX))
    };
    let view = crate::install::link_view(&graph);
    let glob = GlobPattern::new(&pattern);
    let served = super::maintenance::current_layout(&project, &graph)
        .map(|(_, layout)| served_peers(&view, &layout))
        .unwrap_or_default();

    let mut dependents: HashMap<PackageId, Vec<DependentInfo>> = HashMap::new();
    // The pattern also selects a package through the name of a dependency that leads to it, so
    // an alias finds the package behind it.
    let mut matched_by_dep_name = vec![false; view.packages.len()];
    for (index, p) in view.packages.iter().enumerate() {
        let dependent_name = if p.name.is_empty() && matches!(p.resolution, Resolution::Root) {
            UNNAMED_ROOT.to_owned()
        } else {
            p.name.clone()
        };
        let dependent_version = resolution_label(&p.resolution);
        let workspace = dependent_version.starts_with("workspace:") || dependent_version.is_empty();
        for (edge, e) in p.dependencies.iter().enumerate() {
            let targets: Vec<PackageId> = match served.get(&(index, edge)) {
                Some(copies) => copies.clone(),
                None => e.target.into_iter().collect(),
            };
            for target in targets {
                let Some(t) = view.packages.get(target.index()) else {
                    continue;
                };
                matched_by_dep_name[target.index()] |= glob.matches_name(&e.name);
                // The root's membership edges have no specifier of their own.
                let membership = index == 0
                    && e.behavior.is_workspace()
                    && view.workspaces.get(&e.literal) == Some(&target);
                dependents.entry(target).or_default().push(DependentInfo {
                    name: dependent_name.clone(),
                    version: dependent_version.clone(),
                    alias: (e.name != t.name).then(|| e.name.clone()),
                    spec: if membership {
                        String::new()
                    } else {
                        e.literal.clone()
                    },
                    dep_type: DependencyType::of(e.behavior),
                    pkg_id: PackageId(index as u32),
                    workspace,
                });
            }
        }
    }

    let mut targets: Vec<(PackageId, String)> = Vec::new();
    for (index, p) in view.packages.iter().enumerate() {
        if p.name.is_empty() || !(glob.matches_name(&p.name) || matched_by_dep_name[index]) {
            continue;
        }
        let printed = resolution_label(&p.resolution);
        let semver = match &p.resolution {
            Resolution::Npm { version, .. } => Some(version.as_str()),
            Resolution::Workspace { .. } => Some(p.version.as_str()),
            _ => None,
        };
        if glob.matches_version(semver, &printed) {
            targets.push((PackageId(index as u32), printed));
        }
    }
    if targets.is_empty() {
        eprintln!("error: No packages matching '{pattern}' found in lockfile");
        return Err(Error::Reported);
    }

    let mut lines = Vec::new();
    let mut packages = Vec::new();
    for (id, printed) in &targets {
        let name = &view.packages[id.index()].name;
        lines.push(format!("{}@{}", escape(name), escape(printed)));
        let mut tree = Vec::new();
        match dependents.get(id) {
            None => lines.push("  └─ No dependents found".to_owned()),
            Some(_) if max_depth == 0 => lines.push("  └─ (deeper dependencies hidden)".to_owned()),
            Some(list) => {
                let mut sorted = list.clone();
                sorted.sort_by(cmp_dependents);
                let mut tcx = TreeContext {
                    all_dependents: &dependents,
                    max_depth,
                    path_tracker: HashMap::from([(*id, false)]),
                    expanded: HashMap::new(),
                    lines: Vec::new(),
                };
                let len = sorted.len();
                for (i, d) in sorted.iter().enumerate() {
                    let last = i + 1 == len;
                    tcx.lines.push(package_line(
                        if last { PREFIX_LAST } else { PREFIX_MIDDLE },
                        d,
                    ));
                    let sub = if top_only {
                        Vec::new()
                    } else {
                        print_dependency_tree(
                            &mut tcx,
                            d.pkg_id,
                            if last { PREFIX_SPACE } else { PREFIX_CONTINUE },
                            1,
                            last,
                            d.workspace,
                        )
                        .2
                    };
                    tree.push(node_json(d, sub));
                }
                lines.append(&mut tcx.lines);
            }
        }
        lines.push(String::new());
        packages.push(json!({ "name": name, "version": printed, "dependents": tree }));
    }
    Ok(exit(0, json!({ "packages": packages }), lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_patterns() {
        let m = |p: &str, n: &str| GlobPattern::new(p).matches_name(n);
        assert!(m("@types/*", "@types/node"));
        assert!(m("*-lodash", "my-lodash"));
        assert!(m("*lo*", "hello"));
        assert!(m("a*c", "abc"));
        assert!(!m("a*b*c", "abc"));
        assert!(GlobPattern::new("x@^1").matches_version(Some("1.2.0"), "1.2.0"));
        assert!(GlobPattern::new("x@beta").matches_version(Some("1.2.0"), "beta-1"));
        assert!(!GlobPattern::new("x@^3").matches_version(None, "github:o/r#abc"));
    }
}
