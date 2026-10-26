//! Deduplication (bun `install/dedupe.rs`): re-point dependency edges onto the fewest versions
//! of each package that satisfy every edge's effective range, preferring newer versions.
//!
//! Each package name with two or more registry versions is a group. For a group, every edge is a
//! row and every live version a column of a satisfaction matrix; a greedy set cover keeps the
//! fewest versions, and only edges whose version is dropped move. Patched versions are pinned,
//! unmet peers follow the others, packages the pass itself removes do not vote (pnpm/pnpm#9213),
//! and a version only reachable through edges that moved onto it is closed and the pass rerun.
//! [`dedupe`] is `fnpm dedupe`; [`collapse_appended`] runs after every install / update and only
//! moves the edges of packages appended in this session.

use std::cmp::Ordering;

use fnpm_semver::{Range, Version};
use fnpm_types::{DependencySpec, LockGraph, PackageId, Resolution};

use crate::overrides::{OverrideSet, Owner};

/// One removed version in a [`DedupeReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupeRow {
    /// The package name.
    pub name: String,
    /// The removed version.
    pub from: String,
    /// The surviving versions its dependents moved to, ascending; empty when dropped outright.
    pub to: Vec<String>,
    /// Every survivor is a lower major than `from`.
    pub downgrade: bool,
}

/// What a dedupe changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DedupeReport {
    /// The removed versions, by name then version.
    pub rows: Vec<DedupeRow>,
    /// `name@version (needed to reach patched …)` lines for versions kept for patched packages.
    pub kept: Vec<String>,
    /// How many packages were checked.
    pub checked: usize,
}

struct Dep {
    owner: PackageId,
    index: usize,
    range: Option<Range>,
    bundled: bool,
    peer: bool,
}

struct Ctx<'g> {
    graph: &'g LockGraph,
    deps: Vec<Dep>,
    /// First dep index of each package.
    starts: Vec<usize>,
    versions: Vec<Option<Version>>,
    groups: Vec<Vec<PackageId>>,
    group_of: Vec<Option<usize>>,
    pinned: Vec<bool>,
    closed: Vec<bool>,
    /// `Some` for `collapse_appended`: packages that came from the lockfile.
    loaded: Option<&'g [bool]>,
    direct: Vec<bool>,
    roots: Vec<PackageId>,
}

fn npm_version(p: &fnpm_types::Package) -> Option<Version> {
    match &p.resolution {
        Resolution::Npm { version, .. } => Version::parse(version).ok(),
        _ => None,
    }
}

/// Whether a locked registry package is outside the range that an override rule gives its
/// edge (a lockfile written before the rule, or by a merge). bun resolves such an entry again
/// on a normal install, while a frozen install keeps it.
pub(crate) fn overrides_unmet(graph: &LockGraph) -> bool {
    if graph.overrides.is_empty() {
        return false;
    }
    let set = OverrideSet::new(&graph.overrides);
    graph.packages.iter().enumerate().any(|(i, p)| {
        let owner = PackageId(i as u32);
        let owner_version = crate::engine::package_version(p);
        p.dependencies.iter().any(|e| {
            let Some(target) = e.target.and_then(|t| graph.get(t)) else {
                return false;
            };
            let Some(version) = npm_version(target) else {
                return false;
            };
            let Ok(declared) = DependencySpec::parse(&e.name, &e.literal) else {
                return false;
            };
            if matches!(declared, DependencySpec::Workspace { .. })
                || set
                    .lookup(
                        Some(Owner {
                            name: &p.name,
                            version: owner_version.as_ref(),
                        }),
                        &e.name,
                        &declared,
                    )
                    .is_none()
            {
                return false;
            }
            effective_npm_range(
                graph,
                &set,
                owner,
                owner_version.as_ref(),
                &e.name,
                &e.literal,
            )
            .is_some_and(|range| !range.satisfies(&version))
        })
    })
}

/// The effective npm range of an edge: its declared spec after overrides and catalogs.
pub(crate) fn effective_npm_range(
    graph: &LockGraph,
    set: &OverrideSet,
    owner: PackageId,
    owner_version: Option<&Version>,
    name: &str,
    literal: &str,
) -> Option<Range> {
    let declared = DependencySpec::parse(name, literal).ok()?;
    let owner_name = &graph.packages[owner.index()].name;
    let (mut spec_name, mut value) = (name.to_owned(), literal.to_owned());
    if !matches!(declared, DependencySpec::Workspace { .. })
        && let Some(rule) = set.lookup(
            Some(Owner {
                name: owner_name,
                version: owner_version,
            }),
            name,
            &declared,
        )
    {
        spec_name = rule.selector.target.name.clone();
        value = rule.value.clone();
    }
    let mut spec = DependencySpec::parse(&spec_name, &value).ok()?;
    if let DependencySpec::Catalog { catalog } = &spec {
        let entry = crate::catalogs::lookup(&graph.catalogs, catalog, &spec_name)?;
        spec = DependencySpec::parse(&spec_name, entry).ok()?;
    }
    match spec {
        DependencySpec::Npm { range, .. } => Range::parse(&range).ok(),
        _ => None,
    }
}

impl<'g> Ctx<'g> {
    fn new(graph: &'g LockGraph, loaded: Option<&'g [bool]>) -> Self {
        let versions: Vec<Option<Version>> = graph.packages.iter().map(npm_version).collect();
        let set = OverrideSet::new(&graph.overrides);
        let owner_versions: Vec<Option<Version>> = graph
            .packages
            .iter()
            .map(crate::engine::package_version)
            .collect();
        let mut deps = Vec::new();
        let mut starts = Vec::with_capacity(graph.len() + 1);
        for (i, p) in graph.packages.iter().enumerate() {
            starts.push(deps.len());
            let owner = PackageId(i as u32);
            for (j, e) in p.dependencies.iter().enumerate() {
                let range = effective_npm_range(
                    graph,
                    &set,
                    owner,
                    owner_versions[i].as_ref(),
                    &e.name,
                    &e.literal,
                );
                deps.push(Dep {
                    owner,
                    index: j,
                    range,
                    bundled: e.behavior.is_bundled(),
                    peer: e.behavior.is_peer(),
                });
            }
        }
        starts.push(deps.len());
        let direct: Vec<bool> = graph
            .packages
            .iter()
            .map(|p| {
                matches!(
                    p.resolution,
                    Resolution::Root | Resolution::Workspace { .. }
                )
            })
            .collect();
        let mut pinned = vec![false; graph.len()];
        for (i, p) in graph.packages.iter().enumerate() {
            if versions[i].is_some()
                && graph
                    .patched_dependencies
                    .contains_key(&format!("{}@{}", p.name, p.version))
            {
                pinned[i] = true;
            }
        }
        let mut by_name: std::collections::BTreeMap<&str, Vec<PackageId>> =
            std::collections::BTreeMap::new();
        for (i, p) in graph.packages.iter().enumerate() {
            if versions[i].is_some() {
                by_name
                    .entry(p.name.as_str())
                    .or_default()
                    .push(PackageId(i as u32));
            }
        }
        let mut groups = Vec::new();
        let mut group_of = vec![None; graph.len()];
        for (_, mut ids) in by_name {
            if ids.len() < 2 || loaded.is_some_and(|l| ids.iter().all(|id| l[id.index()])) {
                continue;
            }
            ids.sort_by(|a, b| {
                versions[b.index()]
                    .as_ref()
                    .cmp(&versions[a.index()].as_ref())
                    .then(a.cmp(b))
            });
            for id in &ids {
                group_of[id.index()] = Some(groups.len());
            }
            groups.push(ids);
        }
        let mut roots: Vec<PackageId> = graph.workspaces.values().copied().collect();
        roots.sort();
        Self {
            graph,
            deps,
            starts,
            versions,
            groups,
            group_of,
            pinned,
            closed: vec![false; graph.len()],
            loaded,
            direct,
            roots,
        }
    }

    fn original(&self) -> Vec<Option<PackageId>> {
        self.deps
            .iter()
            .map(|d| self.graph.packages[d.owner.index()].dependencies[d.index].target)
            .collect()
    }

    fn reachable(&self, cur: &[Option<PackageId>]) -> Vec<bool> {
        self.reachable_from(cur, &self.roots, self.loaded.is_some())
    }

    fn reachable_from(
        &self,
        cur: &[Option<PackageId>],
        roots: &[PackageId],
        skip_bundled: bool,
    ) -> Vec<bool> {
        let mut seen = vec![false; self.graph.len()];
        let mut stack: Vec<PackageId> = roots.to_vec();
        while let Some(id) = stack.pop() {
            if std::mem::replace(&mut seen[id.index()], true) {
                continue;
            }
            #[allow(clippy::needless_range_loop)]
            for d in self.starts[id.index()]..self.starts[id.index() + 1] {
                if skip_bundled && self.deps[d].bundled && !self.direct[id.index()] {
                    continue;
                }
                if let Some(t) = cur[d]
                    && !seen[t.index()]
                {
                    stack.push(t);
                }
            }
        }
        seen
    }

    fn is_loaded(&self, id: PackageId) -> bool {
        self.loaded.is_some_and(|l| l[id.index()])
    }

    fn counts(&self, d: usize) -> bool {
        self.loaded.is_none() || !self.deps[d].bundled
    }
}

struct Matrix {
    n: usize,
    m: usize,
    sat: Vec<bool>,
    required: Vec<bool>,
    chosen: Vec<bool>,
    survivors: Vec<bool>,
}

impl Matrix {
    fn sat(&self, e: usize, c: usize) -> bool {
        self.sat[e * self.n + c]
    }

    fn cover(&mut self) -> usize {
        let (n, m) = (self.n, self.m);
        self.chosen = self.required.clone();
        let mut covered = vec![false; m];
        let mut count = vec![0u32; n];
        for e in 0..m {
            for (c, slot) in count.iter_mut().enumerate() {
                *slot += u32::from(self.sat(e, c));
            }
        }
        let mut uncovered = m;
        let mut size = 0;
        let mark = |c: usize,
                    covered: &mut Vec<bool>,
                    count: &mut Vec<u32>,
                    uncovered: &mut usize,
                    sat: &[bool]| {
            for e in 0..m {
                if !covered[e] && sat[e * n + c] {
                    covered[e] = true;
                    *uncovered -= 1;
                    for (other, slot) in count.iter_mut().enumerate() {
                        *slot -= u32::from(sat[e * n + other]);
                    }
                }
            }
        };
        for c in 0..n {
            if self.chosen[c] {
                size += 1;
                mark(c, &mut covered, &mut count, &mut uncovered, &self.sat);
            }
        }
        while uncovered > 0 {
            let mut best: Option<usize> = None;
            for c in 0..n {
                if self.chosen[c] || count[c] == 0 {
                    continue;
                }
                if best.is_none_or(|b| count[c] > count[b]) {
                    best = Some(c);
                }
            }
            let Some(b) = best else { break };
            self.chosen[b] = true;
            size += 1;
            mark(b, &mut covered, &mut count, &mut uncovered, &self.sat);
        }
        size
    }

    fn placement(&self, e: usize) -> Option<usize> {
        (0..self.n).find(|&c| self.survivors[c] && self.sat(e, c))
    }

    fn anchored_placement(&self, e: usize) -> Option<usize> {
        (0..self.n)
            .find(|&c| self.required[c] && self.sat(e, c))
            .or_else(|| self.placement(e))
    }
}

impl Ctx<'_> {
    /// Keeps the fewest versions that satisfy every group edge owned by `voters`; re-points
    /// only edges whose version is dropped. Returns the new targets and who voted.
    fn vote(
        &self,
        cur: &[Option<PackageId>],
        live: &[bool],
        voters: &[bool],
    ) -> (Vec<Option<PackageId>>, Vec<bool>) {
        let mut edges: Vec<Vec<(usize, PackageId)>> = vec![Vec::new(); self.groups.len()];
        let mut voted = vec![false; self.graph.len()];
        for pkg in 0..self.graph.len() {
            if !voters[pkg] {
                continue;
            }
            #[allow(clippy::needless_range_loop)]
            for d in self.starts[pkg]..self.starts[pkg + 1] {
                let Some(target) = cur[d] else { continue };
                if !self.counts(d) {
                    continue;
                }
                let Some(g) = self.group_of[target.index()] else {
                    continue;
                };
                edges[g].push((d, PackageId(pkg as u32)));
                voted[pkg] = true;
            }
        }
        let mut next = cur.to_vec();
        for (g, group) in self.groups.iter().enumerate() {
            if edges[g].is_empty() {
                continue;
            }
            let candidates: Vec<PackageId> = group
                .iter()
                .copied()
                .filter(|id| live[id.index()])
                .collect();
            let n = candidates.len();
            if n < 2 {
                continue;
            }
            let rows = &edges[g];
            let m = rows.len();
            let mut mx = Matrix {
                n,
                m,
                sat: vec![false; m * n],
                required: vec![false; n],
                chosen: vec![false; n],
                survivors: vec![false; n],
            };
            let mut movable = vec![false; m];
            let mut cur_c = vec![usize::MAX; m];
            let mut can_move = false;
            for (e, &(d, owner)) in rows.iter().enumerate() {
                let dep = &self.deps[d];
                let Some(target) = cur[d] else { continue };
                let Some(cc) = candidates.iter().position(|&c| c == target) else {
                    continue;
                };
                cur_c[e] = cc;
                let frozen =
                    self.loaded.is_some() && (self.direct[owner.index()] || self.is_loaded(target));
                let range = if self.pinned[target.index()] || frozen || dep.bundled {
                    None
                } else {
                    dep.range.as_ref()
                };
                match range {
                    None => {
                        mx.sat[e * n + cc] = true;
                        mx.required[cc] = true;
                    }
                    Some(range) => {
                        let exact = range.as_exact().map(|v| (v.major, v.minor, v.patch));
                        for (c, id) in candidates.iter().enumerate() {
                            let Some(v) = self.versions[id.index()].as_ref() else {
                                continue;
                            };
                            if exact.is_some_and(|x| x != (v.major, v.minor, v.patch)) {
                                continue;
                            }
                            if (c == cc || !self.closed[id.index()]) && range.satisfies(v) {
                                mx.sat[e * n + c] = true;
                                movable[e] = true;
                                can_move |= c != cc;
                            }
                        }
                        if !movable[e] && dep.peer {
                            movable[e] = true;
                            can_move = true;
                            for (c, id) in candidates.iter().enumerate() {
                                if c == cc || !self.closed[id.index()] {
                                    mx.sat[e * n + c] = true;
                                }
                            }
                        } else if !movable[e] {
                            mx.sat[e * n + cc] = true;
                            mx.required[cc] = true;
                        }
                    }
                }
            }
            if !can_move {
                continue;
            }
            let base = mx.cover();
            mx.survivors = mx.chosen.clone();
            let mut protected: Vec<usize> = Vec::new();
            for (e, &(_, owner)) in rows.iter().enumerate() {
                if cur_c[e] == usize::MAX
                    || !self.direct[owner.index()]
                    || !movable[e]
                    || mx.survivors[cur_c[e]]
                {
                    continue;
                }
                if mx.placement(e).is_some_and(|p| p > cur_c[e]) {
                    protected.push(cur_c[e]);
                }
            }
            protected.sort_unstable();
            protected.dedup();
            for k in protected {
                if mx.survivors[k] {
                    continue;
                }
                mx.required[k] = true;
                if mx.cover() == base {
                    mx.survivors = mx.chosen.clone();
                } else {
                    mx.required[k] = false;
                }
            }
            for (e, &(d, owner)) in rows.iter().enumerate() {
                if !movable[e] || cur_c[e] == usize::MAX {
                    continue;
                }
                let placed_early = self.loaded.is_some()
                    && !self.is_loaded(owner)
                    && !self.direct[owner.index()]
                    && !self.deps[d].peer;
                let placement = if placed_early {
                    mx.anchored_placement(e)
                } else if mx.survivors[cur_c[e]] {
                    continue;
                } else {
                    mx.placement(e)
                };
                if let Some(p) = placement {
                    next[d] = Some(candidates[p]);
                }
            }
        }
        (next, voted)
    }

    fn settle(&self, cur: &[Option<PackageId>], live: &[bool]) -> Vec<Option<PackageId>> {
        let mut voters = live.to_vec();
        let mut best: Option<Vec<Option<PackageId>>> = None;
        loop {
            let (next, mut voted) = self.vote(cur, live, &voters);
            let after = self.reachable(&next);
            if after.iter().zip(&voters).any(|(a, v)| *a && !*v) {
                return best.unwrap_or(next);
            }
            for (v, a) in voted.iter_mut().zip(&after) {
                *v &= !*a;
            }
            if !voted.iter().any(|v| *v) {
                return next;
            }
            for (voter, v) in voters.iter_mut().zip(&voted) {
                *voter &= !*v;
            }
            best = Some(next);
        }
    }

    fn label(&self, id: PackageId) -> String {
        let p = &self.graph.packages[id.index()];
        format!("{}@{}", p.name, p.version)
    }

    fn run(&mut self) -> (Vec<Option<PackageId>>, DedupeReport) {
        let original = self.original();
        let checked = self.graph.len();
        if self.groups.is_empty() {
            return (
                original,
                DedupeReport {
                    checked,
                    ..Default::default()
                },
            );
        }
        let initial = self.reachable(&original);
        let patched: Vec<PackageId> = (0..self.graph.len())
            .filter(|&p| self.pinned[p] && initial[p])
            .map(|p| PackageId(p as u32))
            .collect();
        let mut kept: Vec<(PackageId, Vec<PackageId>)> = Vec::new();
        let (cur, live) = loop {
            let mut live = initial.clone();
            let mut cur = original.clone();
            loop {
                cur = self.settle(&cur, &live);
                let after = self.reachable(&cur);
                if after == live {
                    break;
                }
                live = after;
            }
            // A version only held by edges that moved onto it won on the vote of a removed package.
            let mut held = vec![false; self.graph.len()];
            for (d, dep) in self.deps.iter().enumerate() {
                if !live[dep.owner.index()] {
                    continue;
                }
                if let Some(t) = original[d]
                    && !(dep.bundled && self.loaded.is_some())
                {
                    held[t.index()] = true;
                }
            }
            let before = self.closed.iter().filter(|c| **c).count();
            for v in self.groups.iter().flatten() {
                if live[v.index()] && !held[v.index()] {
                    self.closed[v.index()] = true;
                }
            }
            if self.closed.iter().filter(|c| **c).count() != before {
                continue;
            }
            let orphaned: Vec<PackageId> = patched
                .iter()
                .copied()
                .filter(|p| !live[p.index()])
                .collect();
            if orphaned.is_empty() {
                break (cur, live);
            }
            let kept_before = kept.len();
            let all: Vec<PackageId> = self.groups.iter().flatten().copied().collect();
            for v in all {
                let i = v.index();
                if !initial[i] || live[i] || self.pinned[i] {
                    continue;
                }
                let leads = self.reachable_from(&original, &[v], false);
                let needed: Vec<PackageId> = orphaned
                    .iter()
                    .copied()
                    .filter(|p| leads[p.index()])
                    .collect();
                if !needed.is_empty() {
                    self.pinned[i] = true;
                    kept.push((v, needed));
                }
            }
            if kept.len() == kept_before {
                break (cur, live);
            }
        };
        let order = |a: &PackageId, b: &PackageId| {
            let (pa, pb) = (
                &self.graph.packages[a.index()],
                &self.graph.packages[b.index()],
            );
            pa.name
                .cmp(&pb.name)
                .then_with(|| self.versions[a.index()].cmp(&self.versions[b.index()]))
        };
        kept.sort_by(|a, b| order(&a.0, &b.0));
        let kept_lines: Vec<String> = kept
            .into_iter()
            .map(|(v, mut needed)| {
                needed.sort_by(order);
                let list: Vec<String> = needed.iter().map(|p| self.label(*p)).collect();
                format!(
                    "{} (needed to reach patched {})",
                    self.label(v),
                    list.join(", ")
                )
            })
            .collect();
        let mut removed: Vec<PackageId> = self
            .groups
            .iter()
            .flatten()
            .copied()
            .filter(|id| initial[id.index()] && !live[id.index()])
            .collect();
        removed.sort_by(order);
        let mut rows = Vec::new();
        for id in removed {
            let mut to: Vec<PackageId> = Vec::new();
            for (d, dep) in self.deps.iter().enumerate() {
                if live[dep.owner.index()]
                    && original[d] == Some(id)
                    && let Some(t) = cur[d]
                    && !to.contains(&t)
                {
                    to.push(t);
                }
            }
            to.sort_by(|a, b| self.versions[a.index()].cmp(&self.versions[b.index()]));
            let from = self.versions[id.index()].clone();
            let downgrade = match (
                to.last().and_then(|t| self.versions[t.index()].as_ref()),
                &from,
            ) {
                (Some(last), Some(from)) => last.major < from.major,
                _ => false,
            };
            rows.push(DedupeRow {
                name: self.graph.packages[id.index()].name.clone(),
                from: self.graph.packages[id.index()].version.clone(),
                to: to
                    .iter()
                    .map(|t| self.graph.packages[t.index()].version.clone())
                    .collect(),
                downgrade,
            });
        }
        (
            cur,
            DedupeReport {
                rows,
                kept: kept_lines,
                checked,
            },
        )
    }
}

fn apply(graph: &mut LockGraph, ctx_deps: &[(PackageId, usize)], cur: &[Option<PackageId>]) {
    for (&(owner, index), target) in ctx_deps.iter().zip(cur) {
        graph.packages[owner.index()].dependencies[index].target = *target;
    }
}

fn run_on(graph: &mut LockGraph, loaded: Option<&[bool]>) -> DedupeReport {
    let (cur, report, deps) = {
        let mut ctx = Ctx::new(graph, loaded);
        let (cur, report) = ctx.run();
        let deps: Vec<(PackageId, usize)> = ctx.deps.iter().map(|d| (d.owner, d.index)).collect();
        (cur, report, deps)
    };
    apply(graph, &deps, &cur);
    report
}

/// `fnpm dedupe`: dedupes the whole graph and drops the packages nothing reaches any more.
pub fn dedupe(graph: &mut LockGraph) -> DedupeReport {
    let report = run_on(graph, None);
    prune_unreachable(graph);
    report
}

/// After a resolve: collapses the versions appended in this session onto the fewest versions
/// that satisfy their dependents; edges of `loaded` packages (from the lockfile) and of the root
/// and workspaces do not move. The caller prunes afterwards.
pub(crate) fn collapse_appended(graph: &mut LockGraph, loaded: &[bool]) -> DedupeReport {
    if loaded.iter().all(|l| *l) {
        return DedupeReport {
            checked: graph.len(),
            ..Default::default()
        };
    }
    run_on(graph, Some(loaded))
}

/// Removes packages that no project reaches, renumbering the rest in order. Returns the map from
/// old to new ids.
pub(crate) fn prune_unreachable(graph: &mut LockGraph) -> Vec<Option<PackageId>> {
    let n = graph.len();
    let mut seen = vec![false; n];
    let mut stack: Vec<PackageId> = graph.workspaces.values().copied().collect();
    stack.push(PackageId::ROOT);
    while let Some(id) = stack.pop() {
        if id.index() >= n || std::mem::replace(&mut seen[id.index()], true) {
            continue;
        }
        for e in &graph.packages[id.index()].dependencies {
            if let Some(t) = e.target
                && !seen[t.index()]
            {
                stack.push(t);
            }
        }
    }
    let mut map: Vec<Option<PackageId>> = vec![None; n];
    let mut next = 0u32;
    for (i, keep) in seen.iter().enumerate() {
        if *keep {
            map[i] = Some(PackageId(next));
            next += 1;
        }
    }
    if next as usize == n {
        return map;
    }
    let old = std::mem::take(&mut graph.packages);
    for (i, mut p) in old.into_iter().enumerate() {
        if map[i].is_none() {
            continue;
        }
        for e in &mut p.dependencies {
            e.target = e.target.and_then(|t| map[t.index()]);
        }
        graph.packages.push(p);
    }
    for id in graph.workspaces.values_mut() {
        if let Some(new) = map[id.index()] {
            *id = new;
        }
    }
    map
}

impl PartialOrd for DedupeRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DedupeRow {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| self.from.cmp(&other.from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnpm_types::{Behavior, DepEdge, Package};

    #[test]
    fn prune_renumbers_and_keeps_workspaces() {
        let mut g = LockGraph::new(Package::new("app", "", Resolution::Root));
        let ws = g.add_workspace(Package::new(
            "w",
            "1.0.0",
            Resolution::Workspace { path: "w".into() },
        ));
        let dead = g.push(Package::new(
            "dead",
            "1.0.0",
            Resolution::npm("1.0.0", None),
        ));
        let live = g.push(Package::new(
            "live",
            "1.0.0",
            Resolution::npm("1.0.0", None),
        ));
        let mut e = DepEdge::new("live", "1", Behavior::PROD);
        e.target = Some(live);
        g.packages[ws.index()].dependencies.push(e);
        let map = prune_unreachable(&mut g);
        assert_eq!(map[dead.index()], None);
        assert_eq!(map[live.index()], Some(PackageId(2)));
        assert_eq!(g.packages.len(), 3);
        assert_eq!(g.packages[1].dependencies[0].target, Some(PackageId(2)));
        g.validate().unwrap();
    }
}
