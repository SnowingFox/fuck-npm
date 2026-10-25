//! The peer pass (bun `PackageManagerEnqueue.rs` `settle_without_manifest` peer arm,
//! `installed_peer_target`, `peer_provided_from_above`, `ranges_of_every_asker`,
//! `version_every_peer_accepts`, and `processDependencyList.rs` `peer_batch_order`).
//!
//! Peers are resolved after every other dependency, in batches. A peer binds to what the
//! package that depends on its declarer provides (pnpm semantics), else to an installed version
//! its range (and the ranges of every other package that must agree with it) accepts, else a
//! version is installed from the registry. Optional peers are never installed.

use std::collections::{HashMap, HashSet};

use fnpm_registry::PackageManifest;
use fnpm_semver::{Range, Version};
use fnpm_types::{DependencySpec, PackageId, Resolution};

use super::bind::Cand;
use super::{Engine, Kind, Row};
use crate::pick::{self, AgeGate, Picked};

/// Required peer edges of packages that are in the tree without counting peers, grouped by name,
/// limited to the ones that have to agree on a version (bun `index_peer_askers`).
#[derive(Debug, Default)]
pub(crate) struct Askers {
    /// name -> (peer row, owner, nothing provides the peer for the owner anywhere).
    by_name: HashMap<String, Vec<(Row, PackageId, bool)>>,
}

/// The newest version every range accepts, among the versions each range would pick.
pub(crate) fn version_every_peer_accepts<'m>(
    manifest: &'m PackageManifest,
    ranges: &[(String, Range)],
    gate: Option<&AgeGate<'_>>,
) -> Option<Picked<'m>> {
    let mut best: Option<(Version, Picked<'m>)> = None;
    for (text, range) in ranges {
        let found = pick::find_for_dependency(
            manifest,
            pick::Wanted::Range {
                range,
                literal: text,
            },
            gate,
        );
        let Some(v) = found.ok() else { continue };
        if ranges.iter().all(|(_, r)| r.satisfies(&v.version))
            && best.as_ref().is_none_or(|(newest, _)| v.version > *newest)
        {
            best = Some((v.version.clone(), found));
        }
    }
    best.map(|(_, p)| p)
}

/// Reusable state for the per-name provider searches of [`Engine::compute_askers`]: generation
/// stamps instead of a fresh `Vec<bool>` (or a whole-graph traversal) per name.
struct Search {
    provider: Vec<u32>,
    visited: Vec<u32>,
    provider_gen: u32,
    visit_gen: u32,
    stack: Vec<PackageId>,
}

impl Search {
    fn new(n: usize) -> Self {
        Self {
            provider: vec![0; n],
            visited: vec![0; n],
            provider_gen: 0,
            visit_gen: 0,
            stack: Vec::new(),
        }
    }

    fn set_providers(&mut self, ids: &[PackageId]) {
        self.provider_gen += 1;
        for id in ids {
            self.provider[id.index()] = self.provider_gen;
        }
    }

    fn is_provider(&self, id: PackageId) -> bool {
        self.provider[id.index()] == self.provider_gen
    }

    fn start(&mut self, from: PackageId) {
        self.visit_gen += 1;
        self.stack.clear();
        self.stack.push(from);
        self.visited[from.index()] = self.visit_gen;
    }

    /// `id` is reached from a project through non-peer edges without passing a provider (the
    /// forward walk from the projects that stops at providers, done backwards from `id`).
    fn reached_without_provider(
        &mut self,
        id: PackageId,
        parents: &[Vec<(PackageId, bool)>],
        st: &[super::PState],
    ) -> bool {
        if self.is_provider(id) {
            return false;
        }
        self.start(id);
        while let Some(at) = self.stack.pop() {
            if st[at.index()].kind == Kind::Project {
                return true;
            }
            for &(parent, is_peer) in &parents[at.index()] {
                if !is_peer
                    && self.visited[parent.index()] != self.visit_gen
                    && !self.is_provider(parent)
                {
                    self.visited[parent.index()] = self.visit_gen;
                    self.stack.push(parent);
                }
            }
        }
        false
    }

    /// `id` is a provider or below one through non-peer edges.
    fn below_provider(&mut self, id: PackageId, parents: &[Vec<(PackageId, bool)>]) -> bool {
        if self.is_provider(id) {
            return true;
        }
        self.start(id);
        while let Some(at) = self.stack.pop() {
            for &(parent, is_peer) in &parents[at.index()] {
                if is_peer || self.visited[parent.index()] == self.visit_gen {
                    continue;
                }
                if self.is_provider(parent) {
                    return true;
                }
                self.visited[parent.index()] = self.visit_gen;
                self.stack.push(parent);
            }
        }
        false
    }
}

impl Engine<'_> {
    fn is_project(&self, id: PackageId) -> bool {
        self.st[id.index()].kind == Kind::Project
    }

    /// Edge of `owner` named `name` with the given peer-ness (not an optional peer) whose target
    /// really is a package of that name.
    fn named(&self, owner: PackageId, name: &str, is_peer: bool) -> Option<PackageId> {
        self.graph.packages[owner.index()]
            .dependencies
            .iter()
            .filter(|e| {
                e.name == name && e.behavior.is_peer() == is_peer && !e.behavior.is_optional_peer()
            })
            .filter_map(|e| e.target)
            .find(|t| self.graph.packages[t.index()].name == name)
    }

    fn project_ids(&self) -> Vec<PackageId> {
        let mut ids: Vec<PackageId> = self.graph.workspaces.values().copied().collect();
        ids.sort();
        ids
    }

    /// Packages reachable from the projects through non-peer edges.
    fn reachable_without_peers(&self) -> Vec<bool> {
        let mut seen = vec![false; self.graph.len()];
        let mut stack = self.project_ids();
        while let Some(id) = stack.pop() {
            if std::mem::replace(&mut seen[id.index()], true) {
                continue;
            }
            for e in &self.graph.packages[id.index()].dependencies {
                if let Some(t) = e.target
                    && !e.behavior.is_peer()
                    && !seen[t.index()]
                {
                    stack.push(t);
                }
            }
        }
        seen
    }

    fn compute_askers(&mut self) {
        self.ensure_parents();
        let known = self.reachable_without_peers();
        let mut grouped: HashMap<String, Vec<Row>> = HashMap::new();
        for (i, p) in self.graph.packages.iter().enumerate() {
            if !known[i] {
                continue;
            }
            for (j, e) in p.dependencies.iter().enumerate() {
                if e.behavior.is_peer() && !e.behavior.is_optional_peer() {
                    grouped.entry(e.name.clone()).or_default().push(Row {
                        pkg: PackageId(i as u32),
                        edge: j as u32,
                        install_peer: true,
                    });
                }
            }
        }
        grouped.retain(|_, rows| rows.len() >= 2);
        let mut askers = Askers::default();
        if grouped.is_empty() {
            self.peer_askers = Some(askers);
            return;
        }
        // Providers of every name that matters, found in one pass over the edges (bun
        // `peer_provider`), instead of one pass per name.
        let mut providers: HashMap<&str, Vec<PackageId>> =
            grouped.keys().map(|n| (n.as_str(), Vec::new())).collect();
        for (i, p) in self.graph.packages.iter().enumerate() {
            let id = PackageId(i as u32);
            let is_project = self.is_project(id);
            for e in &p.dependencies {
                let Some(list) = providers.get_mut(e.name.as_str()) else {
                    continue;
                };
                let provides = if e.behavior.is_peer() {
                    is_project && !e.behavior.is_optional_peer()
                } else {
                    e.target
                        .is_some_and(|t| self.graph.packages[t.index()].name == e.name)
                };
                if provides && list.last() != Some(&id) {
                    list.push(id);
                }
            }
        }
        let owned;
        let parents: &[Vec<(PackageId, bool)>] = match &self.parents {
            Some(p) => p,
            None => {
                owned = self.build_parents();
                &owned
            }
        };
        let mut search = Search::new(self.graph.len());
        for (name, rows) in &grouped {
            search.set_providers(&providers[name.as_str()]);
            let mut kept: Vec<(Row, PackageId, bool)> = Vec::new();
            for row in rows {
                if self.st[row.pkg.index()].kind == Kind::Registry
                    && search.reached_without_provider(row.pkg, parents, &self.st)
                {
                    let nowhere = !search.below_provider(row.pkg, parents);
                    kept.push((*row, row.pkg, nowhere));
                }
            }
            if !kept.is_empty() {
                askers.by_name.insert(name.clone(), kept);
            }
        }
        self.peer_askers = Some(askers);
    }

    /// The effective npm ranges of the packages that have to agree with `row` on `name`; empty
    /// when the range of `row` alone decides.
    pub(crate) fn ranges_of_every_asker(&mut self, row: Row, name: &str) -> Vec<(String, Range)> {
        if self.peer_askers.is_none() {
            self.compute_askers();
        }
        let askers: Vec<(Row, PackageId, bool)> = self
            .peer_askers
            .as_ref()
            .and_then(|a| a.by_name.get(name))
            .cloned()
            .unwrap_or_default();
        let mut ranges: Vec<(String, Range)> = Vec::new();
        let mut row_counts = false;
        for (asker, _, no_provider) in askers {
            let edge = self.edge(asker).clone();
            let Ok(Some(asked)) = self.asked(asker, &edge) else {
                continue;
            };
            let DependencySpec::Npm {
                range,
                is_alias: false,
                ..
            } = &asked.spec
            else {
                continue;
            };
            let Ok(parsed) = Range::parse(range) else {
                continue;
            };
            row_counts |= asker.pkg == row.pkg && asker.edge == row.edge && no_provider;
            if !ranges.iter().any(|(text, _)| text == &asked.literal) {
                ranges.push((asked.literal.clone(), parsed));
            }
        }
        if !row_counts || ranges.len() < 2 {
            return Vec::new();
        }
        ranges
    }

    /// The highest installed candidate that satisfies `range` and `accept`.
    fn package_satisfying(
        &mut self,
        name: &str,
        range: Option<&Range>,
        accept: &dyn Fn(&Self, Cand, &Version) -> bool,
    ) -> Option<PackageId> {
        let range = range?;
        let found = self.candidates(name).into_iter().find(|(c, v)| {
            v.as_ref()
                .is_some_and(|v| range.satisfies(v) && accept(self, *c, v))
        })?;
        self.take_cand(found.0)
    }

    /// bun `installed_peer_target`: an installed version in range, else (unless an override chose
    /// the version) what the packages above provide.
    pub(crate) fn installed_peer_target(
        &mut self,
        row: Row,
        name: &str,
        range: Option<&Range>,
    ) -> Option<PackageId> {
        if let Some(id) = self.package_satisfying(name, range, &|_, _, _| true) {
            return Some(id);
        }
        if self.overrides.has_rule_for(name) {
            let edge = self.edge(row).clone();
            if let Ok(Some(asked)) = self.asked(row, &edge)
                && asked.from_rule
            {
                return None;
            }
        }
        let provided = self.peer_provided_from_above(name, row)?;
        (!self.would_revive_leftover(row, Cand::New(provided))).then_some(provided)
    }

    /// The highest installed version that satisfies any of `ranges` (no version satisfies all).
    pub(crate) fn installed_version_in_any_range(
        &mut self,
        row: Row,
        name: &str,
        ranges: &[(String, Range)],
    ) -> Option<PackageId> {
        if ranges.is_empty() {
            return None;
        }
        let found = self.candidates(name).into_iter().find(|(c, v)| {
            v.as_ref()
                .is_some_and(|v| ranges.iter().any(|(_, r)| r.satisfies(v)))
                && !self.would_revive_leftover(row, *c)
        })?;
        self.take_cand(found.0)
    }

    /// Only leftover peers of other packages resolve to `c`: binding to it would revive it.
    fn would_revive_leftover(&self, row: Row, c: Cand) -> bool {
        let id = match c {
            Cand::Prev(_) => return true,
            Cand::New(id) => id,
        };
        if !self.st[id.index()].loaded {
            return false;
        }
        let any_holder = !self.is_project(row.pkg);
        if let Some(parents) = &self.parents {
            return !parents[id.index()]
                .iter()
                .any(|&(owner, is_peer)| any_holder || owner == PackageId::ROOT || !is_peer);
        }
        !self.graph.packages.iter().enumerate().any(|(owner, p)| {
            p.dependencies.iter().any(|e| {
                e.target == Some(id) && (any_holder || owner == 0 || !e.behavior.is_peer())
            })
        })
    }

    /// bun `peer_provided_from_above`: what the declarer, or the nearest package above it, loads
    /// under `name`.
    pub(crate) fn peer_provided_from_above(&self, name: &str, row: Row) -> Option<PackageId> {
        let owner = row.pkg;
        if let Some(own) = self.named(owner, name, false) {
            return Some(own);
        }
        // Only a package called `name` can be the answer: without one, skip walking every
        // ancestor (optional peers that nobody installs would make this quadratic).
        if !self.by_name.contains_key(name) && self.graph.root().name != name {
            return None;
        }
        let owned;
        let parents_of: &[Vec<(PackageId, bool)>] = match &self.parents {
            Some(p) => p,
            None => {
                owned = self.build_parents();
                &owned
            }
        };
        let mut seen: HashSet<PackageId> = HashSet::from([owner]);
        let mut level: HashSet<PackageId> = HashSet::from([owner]);
        loop {
            let mut above: Vec<PackageId> = Vec::new();
            for follows_peers in [false, true] {
                let mut candidates: Vec<PackageId> = level
                    .iter()
                    .flat_map(|t| parents_of[t.index()].iter())
                    .filter(|(parent, is_peer)| {
                        !seen.contains(parent)
                            && (follows_peers || !is_peer || self.is_project(*parent))
                    })
                    .map(|(parent, _)| *parent)
                    .collect();
                candidates.sort();
                candidates.dedup();
                for parent in candidates {
                    if seen.contains(&parent) {
                        continue;
                    }
                    if self.graph.packages[parent.index()].name == name && !self.is_project(parent)
                    {
                        return Some(parent);
                    }
                    if !follows_peers
                        && let Some(provided) = self
                            .named(parent, name, false)
                            .or_else(|| self.named(parent, name, true))
                    {
                        return Some(provided);
                    }
                    seen.insert(parent);
                    above.push(parent);
                }
                if !above.is_empty() {
                    break;
                }
            }
            if above.is_empty() {
                return None;
            }
            level = above.into_iter().collect();
        }
    }

    /// A `file:`, `link:`, tarball or git dependency of the declarer (or of the root) provides
    /// the peer; the linker binds it to the nearest copy.
    pub(crate) fn is_peer_provided_locally(&self, row: Row, name: &str) -> bool {
        let local_rows = |owner: PackageId| {
            self.graph.packages[owner.index()]
                .dependencies
                .iter()
                .any(|e| {
                    !e.behavior.is_peer()
                        && e.name == name
                        && e.target.is_some_and(|t| {
                            !matches!(
                                self.graph.packages[t.index()].resolution,
                                Resolution::Npm { .. }
                                    | Resolution::Workspace { .. }
                                    | Resolution::Root
                            )
                        })
                })
        };
        local_rows(row.pkg) || local_rows(PackageId::ROOT)
    }

    /// Every package depending on the declarer installs an `npm:` alias under the peer's name:
    /// those aliases provide it.
    pub(crate) fn alias_of_every_dependent(
        &mut self,
        row: Row,
        name: &str,
        range: Option<&Range>,
    ) -> Option<PackageId> {
        let mut dependents: Vec<PackageId> = match &self.parents {
            Some(parents) => parents[row.pkg.index()]
                .iter()
                .filter(|(_, is_peer)| !is_peer)
                .map(|(owner, _)| *owner)
                .collect(),
            None => self
                .graph
                .ids()
                .filter(|&id| {
                    self.graph.packages[id.index()]
                        .dependencies
                        .iter()
                        .any(|e| e.target == Some(row.pkg) && !e.behavior.is_peer())
                })
                .collect(),
        };
        dependents.sort_unstable();
        dependents.dedup();
        let any = !dependents.is_empty();
        let mut target: Option<PackageId> = None;
        for owner in dependents {
            let p = &self.graph.packages[owner.index()];
            let alias = p.dependencies.iter().find(|e| {
                e.name == name
                    && !e.behavior.is_peer()
                    && e.target.is_some()
                    && matches!(
                        DependencySpec::parse(&e.name, &e.literal),
                        Ok(DependencySpec::Npm { is_alias: true, .. })
                    )
            })?;
            target = target.or(alias.target);
        }
        if !any {
            return None;
        }
        let target = target?;
        let version = self.st[target.index()].version.as_ref()?;
        (matches!(
            self.graph.packages[target.index()].resolution,
            Resolution::Npm { .. }
        ) && range?.satisfies(version))
        .then_some(target)
    }

    /// No published version satisfies the peer: the highest installed one of the same kind.
    pub(crate) fn highest_peer_candidate(
        &mut self,
        name: &str,
        npm_spec: bool,
    ) -> Option<PackageId> {
        if !npm_spec {
            return None;
        }
        let first = self.candidates(name).into_iter().next()?;
        self.take_cand(first.0)
    }

    /// The installed package a peer settles on before any manifest is read.
    pub(crate) fn settle_peer(
        &mut self,
        row: Row,
        edge: &fnpm_types::DepEdge,
        asked: &super::Asked,
        range: Option<&Range>,
    ) -> Option<PackageId> {
        let ranges = if asked.replaced {
            Vec::new()
        } else {
            self.ranges_of_every_asker(row, &edge.name)
        };
        if ranges.is_empty() {
            return self.installed_peer_target(row, &edge.name, range);
        }
        let all = |_: &Self, _: Cand, v: &Version| ranges.iter().all(|(_, r)| r.satisfies(v));
        if let Some(id) = self.package_satisfying(&edge.name, range, &all) {
            return Some(id);
        }
        let disjoint = ranges
            .iter()
            .enumerate()
            .any(|(i, (_, a))| ranges[i + 1..].iter().any(|(_, b)| !a.intersects(b)));
        if !disjoint {
            return None;
        }
        self.installed_peer_target(row, &edge.name, range)
            .or_else(|| self.installed_version_in_any_range(row, &edge.name, &ranges))
    }

    /// Queues the deferred peers in bun's batch order: the project's own peers first, then by
    /// declarer name and version, then by peer name.
    pub(crate) fn start_peer_batch(&mut self) {
        let mut batch = std::mem::take(&mut self.deferred_peers);
        let rank = |s: &Self, id: PackageId| match s.graph.packages[id.index()].resolution {
            Resolution::Root => 0u8,
            Resolution::Workspace { .. } => 1,
            _ => 2,
        };
        batch.sort_by(|a, b| {
            let pa = &self.graph.packages[a.pkg.index()];
            let pb = &self.graph.packages[b.pkg.index()];
            let by_owner = if a.pkg == b.pkg {
                std::cmp::Ordering::Equal
            } else {
                rank(self, a.pkg)
                    .cmp(&rank(self, b.pkg))
                    .then_with(|| pa.name.cmp(&pb.name))
                    .then_with(|| {
                        match (
                            &self.st[a.pkg.index()].version,
                            &self.st[b.pkg.index()].version,
                        ) {
                            (Some(x), Some(y)) => x.cmp(y),
                            _ => pa.resolution.to_string().cmp(&pb.resolution.to_string()),
                        }
                    })
            };
            by_owner
                .then_with(|| self.edge(*a).name.cmp(&self.edge(*b).name))
                .then_with(|| (a.pkg, a.edge).cmp(&(b.pkg, b.edge)))
        });
        batch.dedup();
        self.ensure_parents();
        self.in_peer_pass = true;
        self.peer_askers = None;
        for mut row in batch {
            row.install_peer = true;
            self.queue.push_back(row);
            self.request_row_ahead(row);
        }
    }
}
