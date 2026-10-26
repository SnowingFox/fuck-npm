//! A flat view of a [`LockGraph`] laid out like bun's lockfile buffers: one dependency array,
//! each package owning a contiguous, sorted slice of it, and a parallel resolution array.
//!
//! The hoisting algorithm (`tree`) and the bun.lock reader/writer are ports of bun code that
//! index into those buffers, so keeping the same layout keeps the port faithful. Building it
//! from a graph is linear in the number of edges.

use crate::fxhash::{FxHashMap, map};
use std::borrow::Cow;
use std::cmp::Ordering;

use fnpm_types::spec::{split_npm_alias, trim_literal};
use fnpm_types::{Behavior, LockGraph, OverrideRule, Resolution, SpecTag, infer_tag};

use crate::semver;

pub(crate) const INVALID: u32 = u32::MAX;

/// bun's `Dependency.Version.Tag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VTag {
    Uninitialized,
    Npm,
    DistTag,
    Tarball,
    Folder,
    Symlink,
    Workspace,
    Git,
    Github,
    Catalog,
}

impl From<SpecTag> for VTag {
    fn from(t: SpecTag) -> Self {
        match t {
            SpecTag::Npm => VTag::Npm,
            SpecTag::DistTag => VTag::DistTag,
            SpecTag::Tarball => VTag::Tarball,
            SpecTag::Folder => VTag::Folder,
            SpecTag::Symlink => VTag::Symlink,
            SpecTag::Workspace => VTag::Workspace,
            SpecTag::Git => VTag::Git,
            SpecTag::Github => VTag::Github,
            SpecTag::Catalog => VTag::Catalog,
        }
    }
}

/// A parsed dependency version, borrowing from the literal it came from.
#[derive(Debug, Clone)]
pub(crate) struct Spec<'g> {
    pub tag: VTag,
    /// The registry name for `npm`/dist-tag specs (the alias target), else empty.
    pub npm_name: &'g str,
    /// The semver range for `npm` specs.
    pub range: &'g str,
    pub is_alias: bool,
    /// `catalog:<name>` name, workspace path, folder path... depending on the tag.
    pub value: &'g str,
}

impl<'g> Spec<'g> {
    pub fn parse(literal: &'g str) -> Spec<'g> {
        let lit = trim_literal(literal);
        let tag: VTag = infer_tag(lit).into();
        let mut s = Spec {
            tag,
            npm_name: "",
            range: "",
            is_alias: false,
            value: lit,
        };
        match tag {
            VTag::Npm | VTag::DistTag => {
                if let Some(target) = lit.strip_prefix("npm:") {
                    let (n, version) = split_npm_alias(target);
                    if version.is_none() && tag == VTag::Npm && is_range_after_npm_protocol(target)
                    {
                        s.range = target;
                    } else {
                        s.is_alias = true;
                        s.npm_name = n;
                        s.range = version.unwrap_or("");
                    }
                } else {
                    s.range = lit;
                }
                if tag == VTag::Npm && s.range.len() > 1 && s.range.starts_with('v') {
                    s.range = &s.range[1..];
                }
            }
            VTag::Workspace => s.value = lit.strip_prefix("workspace:").unwrap_or(lit),
            VTag::Catalog => s.value = lit.strip_prefix("catalog:").unwrap_or("").trim(),
            _ => {}
        }
        s
    }
}

/// `npm:^1.0.0` is a range, `npm:foo` is an alias (bun `Dependency::parse`).
fn is_range_after_npm_protocol(target: &str) -> bool {
    target
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'^' | b'~' | b'<' | b'>' | b'=' | b'*'))
}

#[derive(Debug, Clone)]
pub(crate) struct FDep<'g> {
    pub name: &'g str,
    pub name_id: u32,
    pub literal: &'g str,
    pub behavior: Behavior,
    /// The version as declared, after `tag_workspace_links`.
    pub version: Spec<'g>,
    /// `version` with `catalog:` replaced by the catalog's entry (bun `CatalogMap::resolve_range`).
    pub resolved: Spec<'g>,
    /// Workspace path when `version.tag == Workspace` (set by `tag_workspace_links`).
    pub workspace_path: Option<Cow<'g, str>>,
    /// `(package, index into its dependencies)`; `None` for the root's synthesized links to
    /// workspace members.
    pub edge: Option<(u32, u32)>,
}

pub(crate) struct Flat<'g> {
    pub graph: &'g LockGraph,
    pub deps: Vec<FDep<'g>>,
    pub resolutions: Vec<u32>,
    /// Per package, `[begin, end)` into `deps`.
    pub lists: Vec<(u32, u32)>,
    pub pkg_name_id: Vec<u32>,
    names: FxHashMap<&'g str, u32>,
    /// Name id to package ids, highest resolution first (bun `package_index`).
    pub package_index: FxHashMap<u32, Vec<u32>>,
    /// Workspace name id to `(path, version)`.
    pub workspaces_by_name: FxHashMap<u32, (&'g str, &'g str)>,
    pub overridden_names: Vec<u32>,
}

impl<'g> Flat<'g> {
    fn intern(names: &mut FxHashMap<&'g str, u32>, name: &'g str) -> u32 {
        let n = names.len() as u32;
        *names.entry(name).or_insert(n)
    }

    /// Builds the flat view. Root edges that are pure workspace links are replaced by one
    /// synthesized link per workspace member, as bun's root package has.
    pub fn new(graph: &'g LockGraph) -> Flat<'g> {
        let mut names: FxHashMap<&'g str, u32> = map(graph.packages.len() * 2);
        let pkg_name_id: Vec<u32> = graph
            .packages
            .iter()
            .map(|p| Self::intern(&mut names, &p.name))
            .collect();
        let edge_count: usize = graph.packages.iter().map(|p| p.dependencies.len()).sum();
        let mut deps: Vec<FDep<'g>> = Vec::with_capacity(edge_count + graph.workspaces.len());
        let mut resolutions: Vec<u32> = Vec::with_capacity(deps.capacity());
        let mut lists = Vec::with_capacity(graph.packages.len());
        let n = graph.packages.len() as u32;

        let mut scratch: Vec<(FDep<'g>, u32)> = Vec::new();
        for (pid, pkg) in graph.packages.iter().enumerate() {
            scratch.clear();
            for (ei, e) in pkg.dependencies.iter().enumerate() {
                let group_bits =
                    Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER;
                // A link edge (also the `WORKSPACE | PROD` membership edges of the linker's
                // view, whose literal is the member's path); the synthesized ones below
                // replace it.
                if pid == 0
                    && e.behavior.is_workspace()
                    && (!e.behavior.intersects(group_bits)
                        || (e.target.is_some()
                            && graph.workspaces.get(&e.literal) == e.target.as_ref()))
                {
                    continue;
                }
                let behavior = if e.behavior.intersects(group_bits) {
                    e.behavior.with(Behavior::WORKSPACE, false)
                } else {
                    e.behavior
                };
                let mut version = Spec::parse(&e.literal);
                // `catalog:` only applies to the root and workspaces; elsewhere bun strips the
                // reference and the edge stays unresolved (`CatalogMap::strip_reference`).
                if version.tag == VTag::Catalog
                    && !matches!(
                        pkg.resolution,
                        Resolution::Root | Resolution::Workspace { .. }
                    )
                {
                    version.tag = VTag::Uninitialized;
                }
                let resolved = resolve_catalog(graph, &e.name, &version);
                let target = e
                    .target
                    .map_or(INVALID, |t| if t.0 < n { t.0 } else { INVALID });
                scratch.push((
                    FDep {
                        name: &e.name,
                        name_id: Self::intern(&mut names, &e.name),
                        literal: &e.literal,
                        behavior,
                        version,
                        resolved,
                        workspace_path: None,
                        edge: Some((pid as u32, ei as u32)),
                    },
                    target,
                ));
            }
            if pid == 0 {
                for (path, id) in &graph.workspaces {
                    if path.is_empty() {
                        continue;
                    }
                    let Some(member) = graph.get(*id) else {
                        continue;
                    };
                    let spec = Spec {
                        tag: VTag::Workspace,
                        npm_name: "",
                        range: "",
                        is_alias: false,
                        value: path,
                    };
                    scratch.push((
                        FDep {
                            name: &member.name,
                            name_id: Self::intern(&mut names, &member.name),
                            literal: "",
                            behavior: Behavior::WORKSPACE,
                            version: spec.clone(),
                            resolved: spec,
                            workspace_path: Some(Cow::Borrowed(path.as_str())),
                            edge: None,
                        },
                        id.0,
                    ));
                }
            }
            scratch.sort_by(|a, b| dependency_cmp(&a.0, &b.0));
            let begin = deps.len() as u32;
            for (d, t) in scratch.drain(..) {
                deps.push(d);
                resolutions.push(t);
            }
            lists.push((begin, deps.len() as u32));
        }

        let mut package_index: FxHashMap<u32, Vec<u32>> = map(graph.packages.len());
        for (id, &name) in pkg_name_id.iter().enumerate() {
            let list = package_index.entry(name).or_default();
            let at = list
                .iter()
                .position(|&e| {
                    resolution_order(
                        &graph.packages[id].resolution,
                        &graph.packages[e as usize].resolution,
                    ) == Ordering::Greater
                })
                .unwrap_or(list.len());
            list.insert(at, id as u32);
        }

        let mut workspaces_by_name = map(graph.workspaces.len());
        for (path, id) in &graph.workspaces {
            if path.is_empty() {
                continue;
            }
            if let Some(p) = graph.get(*id) {
                workspaces_by_name.insert(
                    Self::intern(&mut names, &p.name),
                    (path.as_str(), p.version.as_str()),
                );
            }
        }

        let mut overridden_names: Vec<u32> = graph
            .overrides
            .iter()
            .map(|r: &OverrideRule| Self::intern(&mut names, &r.selector.target.name))
            .collect();
        overridden_names.sort_unstable();
        overridden_names.dedup();

        let mut flat = Flat {
            graph,
            deps,
            resolutions,
            lists,
            pkg_name_id,
            names,
            package_index,
            workspaces_by_name,
            overridden_names,
        };
        flat.tag_workspace_links();
        flat
    }

    #[inline]
    pub fn list(&self, pkg: u32) -> std::ops::Range<u32> {
        let (b, e) = self.lists[pkg as usize];
        b..e
    }

    #[inline]
    pub fn res(&self, pkg: u32) -> &'g Resolution {
        &self.graph.packages[pkg as usize].resolution
    }

    pub fn has_override_for(&self, name_id: u32) -> bool {
        self.overridden_names.binary_search(&name_id).is_ok()
    }

    /// bun `Lockfile::is_overridden_dependency`: an override rule applies to this edge.
    pub fn is_overridden_dependency(&self, dep_id: u32) -> bool {
        let d = &self.deps[dep_id as usize];
        if d.behavior.is_workspace() || (d.version.tag == VTag::Npm && d.version.is_alias) {
            return false;
        }
        if !self.has_override_for(d.name_id) {
            return false;
        }
        let owner = d.edge.map(|(p, _)| &self.graph.packages[p as usize]);
        self.graph.overrides.iter().any(|r| {
            if r.selector.target.name != d.name {
                return false;
            }
            if !r.selector.target.range.is_empty() && r.selector.target.range != d.version.range {
                let min = min_of_range(d.version.range);
                if !min.is_some_and(|m| semver::satisfies(&r.selector.target.range, m)) {
                    return false;
                }
            }
            match (&r.selector.parent, owner) {
                (None, _) => true,
                (Some(parent), Some(owner)) => {
                    owner.name == parent.name
                        && (parent.range.is_empty()
                            || semver::satisfies(&parent.range, &owner.version))
                }
                (Some(_), None) => false,
            }
        })
    }

    /// bun `Resolution::satisfies_dependency_version`.
    pub fn satisfies(&self, pkg: u32, spec: &Spec<'_>) -> bool {
        match (self.res(pkg), spec.tag) {
            (Resolution::Npm { version, .. }, VTag::Npm) => semver::satisfies(spec.range, version),
            (Resolution::Git { url, .. }, VTag::Git) => {
                let s = spec.value.strip_prefix("git+").unwrap_or(spec.value);
                s.rsplit_once('#').map_or(s, |(u, _)| u) == url
            }
            (Resolution::Github { owner, repo, .. }, VTag::Github) => {
                let s = spec.value.strip_prefix("github:").unwrap_or(spec.value);
                let s = s.split('#').next().unwrap_or(s);
                s.rsplit_once('/').is_some_and(|(o, r)| {
                    o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo)
                })
            }
            _ => false,
        }
    }

    /// The name id that `package_index` is searched with for a peer (bun
    /// `peer_candidate_name_hash`): the alias target for `npm:` specs.
    pub fn peer_candidate_name(&self, dep: &FDep<'_>) -> u32 {
        match dep.resolved.tag {
            VTag::Npm | VTag::DistTag if dep.resolved.is_alias => self
                .names
                .get(dep.resolved.npm_name)
                .copied()
                .unwrap_or(INVALID),
            _ => dep.name_id,
        }
    }

    /// bun `version_based_peer_range`: `Some(name id)` for a required, non-bundled peer whose
    /// binding is chosen by version rather than by tree path.
    pub fn version_based_peer(&self, dep_id: u32) -> Option<u32> {
        let dep = &self.deps[dep_id as usize];
        if !dep.behavior.is_peer() || dep.behavior.is_optional_peer() || dep.behavior.is_bundled() {
            return None;
        }
        let name = self.peer_candidate_name(dep);
        let overridable =
            !dep.behavior.is_workspace() && (dep.version.tag != VTag::Npm || !dep.version.is_alias);
        let override_name = if dep.version.tag == VTag::Catalog {
            dep.name_id
        } else {
            name
        };
        if overridable && self.has_override_for(override_name) {
            return None;
        }
        Some(name)
    }

    /// bun `resolve_peer_dep_version_based`.
    pub fn peer_by_version(&self, dep_id: u32, is_candidate: impl Fn(u32) -> bool) -> Option<u32> {
        let name = self.version_based_peer(dep_id)?;
        let dep = &self.deps[dep_id as usize];
        self.package_index
            .get(&name)?
            .iter()
            .copied()
            .find(|&id| self.satisfies(id, &dep.resolved) && is_candidate(id))
    }

    /// bun `linked_workspace_path`: the workspace an npm range links to.
    pub fn linked_workspace(&self, name_id: u32, range: &str) -> Option<&'g str> {
        let (path, version) = *self.workspaces_by_name.get(&name_id)?;
        if semver::is_star(range) {
            return Some(path);
        }
        if version.is_empty() {
            return None;
        }
        semver::satisfies(range, version).then_some(path)
    }

    fn is_local_row(&self, dep_id: usize) -> bool {
        self.deps[dep_id].edge.is_none_or(|(p, _)| {
            matches!(self.res(p), Resolution::Root | Resolution::Workspace { .. })
        })
    }

    /// bun `Lockfile::tag_workspace_links` (with `link_workspace_packages`).
    pub fn tag_workspace_links(&mut self) {
        let has_local = self.graph.packages.iter().skip(1).any(|p| {
            matches!(
                p.resolution,
                Resolution::Root | Resolution::Workspace { .. }
            )
        }) || self.resolutions.contains(&0);
        if !has_local {
            return;
        }
        let n = self.graph.packages.len();
        for dep_id in 0..self.deps.len() {
            let pkg = self.resolutions[dep_id];
            if pkg as usize >= n {
                continue;
            }
            let res = self.res(pkg);
            if matches!(res, Resolution::Root) && self.deps[dep_id].version.tag == VTag::Workspace {
                self.deps[dep_id].workspace_path = Some(Cow::Borrowed("."));
            }
            let Resolution::Workspace { path } = res else {
                continue;
            };
            if self.deps[dep_id].version.tag == VTag::Workspace
                && self.deps[dep_id].behavior.is_peer()
                && !self.deps[dep_id].behavior.is_optional_peer()
                && !self.is_local_row(dep_id)
            {
                let reparsed = Spec::parse(self.deps[dep_id].literal);
                if reparsed.tag == VTag::Npm {
                    self.deps[dep_id].version = reparsed;
                }
            }
            let d = &self.deps[dep_id];
            let linked = match d.version.tag {
                VTag::Workspace => true,
                VTag::Npm => {
                    let name = if d.version.is_alias {
                        self.names.get(d.version.npm_name).copied()
                    } else {
                        Some(d.name_id)
                    };
                    name.and_then(|name| self.linked_workspace(name, d.version.range))
                        .is_some()
                        && (!d.behavior.is_peer()
                            || self.is_local_row(dep_id)
                            || (!d.behavior.is_optional_peer()
                                && self.peer_by_version(dep_id as u32, |_| true).is_none()))
                }
                VTag::Folder => self.list(0).contains(&(dep_id as u32)),
                VTag::DistTag => {
                    self.list(0).contains(&(dep_id as u32))
                        && self.workspaces_by_name.contains_key(&d.name_id)
                        && (!d.version.is_alias || d.version.npm_name == d.name)
                }
                _ => false,
            };
            if linked {
                let d = &mut self.deps[dep_id];
                d.version.tag = VTag::Workspace;
                d.workspace_path = Some(Cow::Borrowed(path.as_str()));
            }
        }
    }
}

/// `catalog:` replaced by its entry; other specs are returned unchanged.
fn resolve_catalog<'g>(graph: &'g LockGraph, name: &str, spec: &Spec<'g>) -> Spec<'g> {
    if spec.tag != VTag::Catalog {
        return spec.clone();
    }
    match graph.catalogs.get(spec.value, name) {
        Some(lit) => Spec::parse(lit),
        None => Spec {
            tag: VTag::Uninitialized,
            ..spec.clone()
        },
    }
}

/// The lowest version a simple range names (`^1.2.3` gives `1.2.3`), for override matching.
fn min_of_range(range: &str) -> Option<&str> {
    let r = range.trim().trim_start_matches(['^', '~', '=', '>', 'v']);
    let r = r.split_whitespace().next()?;
    semver::is_version(r).then_some(r)
}

/// bun `Behavior::cmp`: workspace, dev, optional, prod, peer.
pub(crate) fn behavior_cmp(a: Behavior, b: Behavior) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let first = |x: bool, y: bool| {
        if x {
            Ordering::Less
        } else if y {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    };
    for (x, y) in [
        (a.is_workspace(), b.is_workspace()),
        (a.is_dev(), b.is_dev()),
        (a.is_optional(), b.is_optional()),
        (a.is_prod(), b.is_prod()),
        (a.is_peer(), b.is_peer()),
    ] {
        if x != y {
            return first(x, y);
        }
    }
    Ordering::Equal
}

/// bun `Dependency::cmp`: behavior, then name.
pub(crate) fn dependency_cmp(a: &FDep<'_>, b: &FDep<'_>) -> Ordering {
    behavior_cmp(a.behavior, b.behavior).then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
}

/// bun's resolution tag numbers, which order resolutions of different kinds.
pub(crate) fn resolution_tag_number(r: &Resolution) -> u8 {
    match r {
        Resolution::Root => 1,
        Resolution::Npm { .. } => 2,
        Resolution::Folder { .. } => 4,
        Resolution::LocalTarball { .. } => 8,
        Resolution::Github { .. } => 16,
        Resolution::Git { .. } => 32,
        Resolution::Symlink { .. } => 64,
        Resolution::Workspace { .. } => 72,
        Resolution::RemoteTarball { .. } => 80,
    }
}

/// bun `Resolution::order`.
pub(crate) fn resolution_order(a: &Resolution, b: &Resolution) -> Ordering {
    let (ta, tb) = (resolution_tag_number(a), resolution_tag_number(b));
    if ta != tb {
        return ta.cmp(&tb);
    }
    match (a, b) {
        (Resolution::Npm { version: x, .. }, Resolution::Npm { version: y, .. }) => {
            semver::cmp(x, y)
        }
        (Resolution::LocalTarball { path: x, .. }, Resolution::LocalTarball { path: y, .. })
        | (Resolution::Folder { path: x }, Resolution::Folder { path: y })
        | (Resolution::Workspace { path: x }, Resolution::Workspace { path: y })
        | (Resolution::Symlink { path: x }, Resolution::Symlink { path: y })
        | (Resolution::RemoteTarball { url: x, .. }, Resolution::RemoteTarball { url: y, .. }) => {
            x.cmp(y)
        }
        (
            Resolution::Git {
                url: x,
                committish: cx,
                resolved_commit: rx,
                ..
            },
            Resolution::Git {
                url: y,
                committish: cy,
                resolved_commit: ry,
                ..
            },
        ) => x.cmp(y).then(cx.cmp(cy)).then(rx.cmp(ry)),
        (
            Resolution::Github {
                owner: ox,
                repo: x,
                committish: cx,
                resolved_commit: rx,
                ..
            },
            Resolution::Github {
                owner: oy,
                repo: y,
                committish: cy,
                resolved_commit: ry,
                ..
            },
        ) => ox.cmp(oy).then(x.cmp(y)).then(cx.cmp(cy)).then(rx.cmp(ry)),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parsing() {
        let s = Spec::parse("npm:@scope/real@^1.2.0");
        assert_eq!(
            (s.tag, s.npm_name, s.range, s.is_alias),
            (VTag::Npm, "@scope/real", "^1.2.0", true)
        );
        let s = Spec::parse("npm:^1.0.0");
        assert_eq!((s.tag, s.range, s.is_alias), (VTag::Npm, "^1.0.0", false));
        assert_eq!(Spec::parse("v1.0.0").range, "1.0.0");
        assert_eq!(Spec::parse("workspace:*").value, "*");
        let c = Spec::parse("catalog:react17");
        assert_eq!((c.tag, c.value), (VTag::Catalog, "react17"));
        assert_eq!(Spec::parse("catalog:").value, "");
        assert_eq!(Spec::parse("latest").tag, VTag::DistTag);
    }

    #[test]
    fn behavior_order() {
        let mut v = vec![
            Behavior::PEER,
            Behavior::PROD,
            Behavior::OPTIONAL,
            Behavior::DEV,
            Behavior::WORKSPACE,
        ];
        v.sort_by(|a, b| behavior_cmp(*a, *b));
        assert_eq!(
            v,
            vec![
                Behavior::WORKSPACE,
                Behavior::DEV,
                Behavior::OPTIONAL,
                Behavior::PROD,
                Behavior::PEER
            ]
        );
        assert_eq!(
            behavior_cmp(Behavior::PEER | Behavior::OPTIONAL, Behavior::PEER),
            Ordering::Equal
        );
    }
}
