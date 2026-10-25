//! `--filter` / `-F` selectors (bun `workspace_selection.rs`, pnpm grammar).
//!
//! A selector is `!`* (an odd count negates), then an optional `...` / `...^` prefix
//! (dependents, `^` excluding the matched packages themselves) and/or `...` / `^...` suffix
//! (dependencies), around one of: `*` or `**` (every package; the root too unless the root is
//! explicit-only), `{dir}` (every package at or under `dir`, resolved from the cwd), `./path`
//! or any spelling starting with `.` (a directory glob resolved from the cwd), or a name glob
//! (`pkg`, `@scope/*`). Positive selectors are unioned, then negative ones subtracted; with no
//! positive selector the start is every package.

use std::path::Path;

use fnpm_types::{DependencyGroup, DependencySpec};

use crate::discover::abs_posix;
use crate::glob::{Dots, Glob, escape_literal, strip_negations};
use crate::graph::WorkspaceGraph;
use crate::paths::{clean_relative, join_posix};
use crate::{Error, Result, Workspace};

/// What a selector matches before relations are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `*` / `**`: every package with a name (and the root, see [`RootSelection`]).
    All,
    /// Glob over package names (`@scope/*`).
    Name(String),
    /// Directory glob relative to the cwd (`./packages/*`, `../x`, `.`).
    Path(String),
    /// `{dir}`: every package whose directory is `dir` (relative to the cwd) or below it.
    Subtree(String),
}

/// A parsed `--filter` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub target: Target,
    /// `!pattern`: subtract instead of add.
    pub exclude: bool,
    /// `pattern...`: add the workspaces it depends on (transitively).
    pub include_dependencies: bool,
    /// `...pattern`: add the workspaces that depend on it (transitively).
    pub include_dependents: bool,
    /// `^` next to `...`: leave out the packages `target` itself matched.
    pub exclude_self: bool,
}

impl Selector {
    /// Parses one pattern. A relation without a name or path (`...`) is an error with bun's
    /// message: `--filter "..." is missing a workspace name or path`.
    pub fn parse(raw: &str) -> Result<Self> {
        let (mut remain, exclude) = strip_negations(raw);
        let mut include_dependencies = false;
        let mut include_dependents = false;
        let mut exclude_self = false;
        if let Some(rest) = remain.strip_prefix("...") {
            include_dependents = true;
            remain = rest;
            if let Some(rest) = remain.strip_prefix('^') {
                exclude_self = true;
                remain = rest;
            }
        }
        if let Some(rest) = remain.strip_suffix("...") {
            include_dependencies = true;
            remain = rest;
            if let Some(rest) = remain.strip_suffix('^') {
                exclude_self = true;
                remain = rest;
            }
        }
        if (include_dependencies || include_dependents) && remain.is_empty() {
            return Err(Error::BadFilter {
                filter: raw.to_owned(),
                message: "is missing a workspace name or path".to_owned(),
            });
        }
        let target = if remain == "*" || remain == "**" {
            Target::All
        } else if remain.len() >= 2 && remain.starts_with('{') && remain.ends_with('}') {
            Target::Subtree(remain[1..remain.len() - 1].to_owned())
        } else if remain.starts_with('.') {
            Target::Path(remain.to_owned())
        } else {
            Target::Name(remain.to_owned())
        };
        Ok(Selector {
            target,
            exclude,
            include_dependencies,
            include_dependents,
            exclude_self,
        })
    }

    /// Parses every pattern.
    pub fn parse_all<S: AsRef<str>>(patterns: &[S]) -> Result<Vec<Self>> {
        patterns.iter().map(|p| Self::parse(p.as_ref())).collect()
    }

    /// Whether this selector follows the dependency graph.
    pub fn is_relational(&self) -> bool {
        self.include_dependencies || self.include_dependents
    }
}

/// Whether the root package can be selected without being named (bun `RootSelection`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RootSelection {
    /// install / update / outdated: `*` and a filter set without positive patterns include
    /// the root.
    #[default]
    Implicit,
    /// add / remove: the root only when matched by name or path; `*` excludes it, and
    /// relations never reach it (it still seeds a relation when named).
    ExplicitOnly,
}

/// Options for [`select_with`].
#[derive(Debug, Clone, Copy)]
pub struct SelectOptions {
    pub root: RootSelection,
    /// `install.linkWorkspacePackages`: an npm range on a member's name is an edge to it when
    /// the member's version satisfies it.
    pub link_workspace_packages: bool,
    /// `satisfies(version, range)`, used for npm ranges in the relation graph.
    pub satisfies: fn(&str, &str) -> bool,
}

impl Default for SelectOptions {
    fn default() -> Self {
        SelectOptions {
            root: RootSelection::Implicit,
            link_workspace_packages: true,
            satisfies: fnpm_semver::satisfies,
        }
    }
}

/// One selectable package.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// Empty for a package.json without a name (selectable by path only).
    pub name: &'a str,
    /// Absolute, `/`-separated, no trailing slash.
    pub dir: &'a str,
    pub is_root: bool,
}

/// The outcome of a selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Selected indices, ascending. From [`select_with`] these index `Workspace::members`
    /// with `usize::MAX` (listed first) for the root; from [`select_candidates`] they index the
    /// candidates.
    pub selected: Vec<usize>,
    /// Indices of positive selectors that matched nothing (bun warns about each).
    pub unmatched_patterns: Vec<usize>,
}

enum Base {
    All,
    Name(Glob),
    Path(Glob),
    Subtree(Glob),
}

/// The cwd is escaped so a directory named like `app[1]` matches itself; a trailing `**` needs
/// a segment, as in bun's matcher (`./packages/**` does not select `packages` itself).
fn base_of(target: &Target, cwd: &str) -> Base {
    let path = |p: &str| Glob::new(&join_posix(cwd, p)).trailing_globstar_needs_segment();
    match target {
        Target::All => Base::All,
        Target::Name(n) => Base::Name(Glob::new(n).trailing_globstar_needs_segment()),
        Target::Path(p) => Base::Path(path(p)),
        Target::Subtree(p) => Base::Subtree(path(p)),
    }
}

fn base_matches(base: &Base, c: &Candidate<'_>, explicit_root_only: bool) -> bool {
    match base {
        Base::All => {
            if c.is_root {
                !explicit_root_only
            } else {
                !c.name.is_empty()
            }
        }
        Base::Name(g) => g.matches(c.name, Dots::Match),
        Base::Path(g) => g.matches(c.dir, Dots::Match),
        Base::Subtree(g) => {
            let mut dir = c.dir;
            loop {
                if g.matches(dir, Dots::Match) {
                    return true;
                }
                match dir.rfind('/') {
                    Some(i) if i > 0 => dir = &dir[..i],
                    Some(0) if dir.len() > 1 => dir = "/",
                    _ => return false,
                }
            }
        }
    }
}

/// bun `walk`: `unreachable` (the explicit-only root) seeds a relation but is never reached.
fn walk(
    graph: &WorkspaceGraph,
    sel: &Selector,
    base: &[bool],
    unreachable: Option<usize>,
) -> Vec<bool> {
    let n = base.len();
    let mut reached = vec![false; n];
    for adjacency in [
        sel.include_dependencies.then_some(&graph.dependencies),
        sel.include_dependents.then_some(&graph.dependents),
    ]
    .into_iter()
    .flatten()
    {
        let mut visited = base.to_vec();
        if let Some(i) = unreachable {
            visited[i] = true;
        }
        let mut queue: std::collections::VecDeque<usize> = (0..n).filter(|&i| base[i]).collect();
        while let Some(u) = queue.pop_front() {
            for &v in &adjacency[u] {
                let v = v as usize;
                if !visited[v] {
                    visited[v] = true;
                    queue.push_back(v);
                }
            }
        }
        for i in 0..n {
            reached[i] |= visited[i];
        }
    }
    for i in 0..n {
        if sel.exclude_self {
            reached[i] &= !base[i];
        } else {
            reached[i] |= base[i];
        }
    }
    if let Some(i) = unreachable.filter(|&i| !base[i]) {
        reached[i] = false;
    }
    reached
}

/// bun `select` over arbitrary candidates. `cwd` is absolute; `graph` is required when a
/// selector is relational (without it relations select only their base).
pub fn select_candidates(
    selectors: &[Selector],
    cwd: &Path,
    candidates: &[Candidate<'_>],
    graph: Option<&WorkspaceGraph>,
    root: RootSelection,
) -> Selection {
    let n = candidates.len();
    let cwd = escape_literal(&abs_posix(&crate::paths::absolute(cwd)));
    let explicit_root_only = root == RootSelection::ExplicitOnly && n > 1;
    let root_index = if explicit_root_only {
        candidates.iter().position(|c| c.is_root)
    } else {
        None
    };
    let mut include = vec![false; n];
    let mut exclude = vec![false; n];
    let mut any_positive = false;
    let mut unmatched_patterns = Vec::new();
    for (index, sel) in selectors.iter().enumerate() {
        let base = base_of(&sel.target, &cwd);
        let mut set: Vec<bool> = candidates
            .iter()
            .map(|c| base_matches(&base, c, explicit_root_only))
            .collect();
        if sel.is_relational()
            && let Some(graph) = graph
        {
            set = walk(graph, sel, &set, root_index);
        }
        if sel.exclude {
            for i in 0..n {
                exclude[i] |= set[i];
            }
        } else {
            any_positive = true;
            if !set.iter().any(|&s| s) {
                unmatched_patterns.push(index);
            }
            for i in 0..n {
                include[i] |= set[i];
            }
        }
    }
    if !any_positive {
        for (i, c) in candidates.iter().enumerate() {
            include[i] = base_matches(&Base::All, c, explicit_root_only);
        }
    }
    let selected = (0..n).filter(|&i| include[i] && !exclude[i]).collect();
    Selection {
        selected,
        unmatched_patterns,
    }
}

/// The relation graph over `[root, members...]` from the manifests (bun
/// `workspace_manifests.rs` `relation_graph`): every dependency group; `catalog:` is resolved
/// first; `workspace:<path>` points at the member at that path, other `workspace:` specs at
/// the member with the dependency's name; an npm range on a member's name is an edge when
/// `link_workspace_packages` is on and the range is `*`/empty or the member's version
/// satisfies it. The root has outgoing edges only.
pub(crate) fn workspace_graph(ws: &Workspace, options: &SelectOptions) -> WorkspaceGraph {
    let n = ws.members.len() + 1;
    let mut by_name: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(n);
    let mut by_path: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(n);
    for (i, m) in ws.members.iter().enumerate() {
        by_name.entry(&m.name).or_insert(i + 1);
        by_path.entry(&m.path).or_insert(i + 1);
    }
    let mut edges = Vec::new();
    for from in 0..n {
        let manifest = if from == 0 {
            &ws.root
        } else {
            &ws.members[from - 1].manifest
        };
        for group in DependencyGroup::ALL {
            for (name, literal) in manifest.group(group) {
                let literal = match literal.trim_start().strip_prefix("catalog:") {
                    Some(catalog) => match ws.settings.catalogs.get(catalog.trim(), name) {
                        Some(l) => l,
                        None => continue,
                    },
                    None => literal.as_str(),
                };
                let Ok(spec) = DependencySpec::parse(name, literal) else {
                    continue;
                };
                let to = match &spec {
                    DependencySpec::Workspace { spec } => {
                        let raw = spec.to_raw();
                        // A root-relative path (bun keeps `workspace:<path>` as a lockfile path).
                        let rooted = (!matches!(raw.trim(), "" | "*" | "^" | "~"))
                            .then(|| by_path.get(clean_relative(raw).as_str()).copied())
                            .flatten();
                        let from_dir = if from == 0 {
                            &ws.root_dir
                        } else {
                            &ws.members[from - 1].dir
                        };
                        rooted.or_else(|| {
                            match ws.resolve_workspace_spec(
                                from_dir,
                                name,
                                literal,
                                options.satisfies,
                            ) {
                                Ok(Some(usize::MAX)) => Some(0),
                                Ok(Some(i)) => Some(i + 1),
                                _ => None,
                            }
                        })
                    }
                    DependencySpec::Npm {
                        name: target,
                        range,
                        ..
                    } if options.link_workspace_packages => {
                        by_name.get(target.as_str()).copied().filter(|&i| {
                            let range = range.trim();
                            if matches!(range, "" | "*" | "x" | "X") {
                                return true;
                            }
                            let version = &ws.members[i - 1].version;
                            !version.is_empty() && (options.satisfies)(version, range)
                        })
                    }
                    _ => None,
                };
                // Like bun, nothing depends on the root: it is not a workspace by name or path.
                if let Some(to) = to.filter(|&to| to != from && to != 0) {
                    edges.push((from as u32, to as u32));
                }
            }
        }
    }
    WorkspaceGraph::from_edges(n, edges)
}

/// The candidates of a workspace: `[root, members...]`, plus their posix directories.
fn candidate_dirs(ws: &Workspace) -> Vec<String> {
    std::iter::once(abs_posix(&ws.root_dir))
        .chain(ws.members.iter().map(|m| abs_posix(&m.dir)))
        .collect()
}

/// Selects workspace packages (install, update, outdated, publish, ...). Returns indices into
/// `ws.members`, ascending, with `usize::MAX` first for the root. No selectors select
/// everything. Nothing selected is [`Error::NoMatch`].
pub fn select(ws: &Workspace, selectors: &[Selector], cwd: &Path) -> Result<Vec<usize>> {
    let selection = select_with(ws, selectors, cwd, &SelectOptions::default());
    if selection.selected.is_empty() {
        Err(Error::NoMatch)
    } else {
        Ok(selection.selected)
    }
}

/// [`select`] with options, keeping the unmatched-pattern report and an empty result.
pub fn select_with(
    ws: &Workspace,
    selectors: &[Selector],
    cwd: &Path,
    options: &SelectOptions,
) -> Selection {
    let dirs = candidate_dirs(ws);
    let root_name = ws.root.name.as_deref().unwrap_or("");
    let candidates: Vec<Candidate<'_>> = std::iter::once(Candidate {
        name: root_name,
        dir: &dirs[0],
        is_root: true,
    })
    .chain(ws.members.iter().zip(&dirs[1..]).map(|(m, d)| Candidate {
        name: &m.name,
        dir: d,
        is_root: false,
    }))
    .collect();
    let graph = selectors
        .iter()
        .any(Selector::is_relational)
        .then(|| workspace_graph(ws, options));
    let s = select_candidates(selectors, cwd, &candidates, graph.as_ref(), options.root);
    Selection {
        // Candidate 0 is the root, so the order stays ascending with the root first.
        selected: s
            .selected
            .into_iter()
            .map(|i| if i == 0 { usize::MAX } else { i - 1 })
            .collect(),
        unmatched_patterns: s.unmatched_patterns,
    }
}

/// `"a", "b"`.
pub fn quote_patterns<S: AsRef<str>>(patterns: &[S]) -> String {
    patterns
        .iter()
        .map(|p| format!("\"{}\"", p.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `No workspace packages matched the filter "a"` / `... the filters "a", "b"`: the sentence
/// every `--filter` command prints (as a warning per unmatched pattern, or as the error when
/// nothing was selected).
pub fn unmatched_message<S: AsRef<str>>(patterns: &[S]) -> String {
    let lead = if patterns.len() == 1 {
        "No workspace packages matched the filter "
    } else {
        "No workspace packages matched the filters "
    };
    format!("{lead}{}", quote_patterns(patterns))
}
