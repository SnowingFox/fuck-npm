//! Speculative manifest requests (bun `PackageManagerEnqueue.rs` `request_ahead`,
//! `look_ahead`, `follow_likely_version`): every queued row asks for its manifest right away,
//! and when a manifest arrives, the dependencies of the version the row most likely resolves to
//! are requested before the queue reaches them. Nothing here resolves anything, so results do not
//! depend on what arrives first.

use fnpm_semver::Range;
use fnpm_types::name::is_npm_package_name;
use fnpm_types::spec::trim_literal;
use fnpm_types::{DepEdge, DependencySpec};

use super::{Asked, Engine, MState, Row};
use crate::pick::{self, Wanted};

impl Engine<'_> {
    /// Starts the manifest request a queued row will need when its turn comes.
    pub(crate) fn request_row_ahead(&mut self, row: Row) {
        let edge = self.edge(row).clone();
        if edge.behavior.is_optional_peer() || (edge.behavior.is_peer() && !row.install_peer) {
            return;
        }
        if edge.target.is_some() {
            return;
        }
        let Ok(Some(asked)) = self.asked(row, &edge) else {
            return;
        };
        if !asked.spec.tag().is_npm() || !is_npm_package_name(&asked.name) {
            return;
        }
        if self.manifest_state.contains_key(&asked.name) {
            return;
        }
        if self.settles_without_manifest(row, &edge, &asked) {
            return;
        }
        let revalidate = self.should_update(row, &edge, &asked);
        self.request_manifest(&asked.name, revalidate, true);
        self.lookahead_waiting
            .entry(asked.name.clone())
            .or_default()
            .push((edge.name.clone(), asked.literal.clone(), row.install_peer));
    }

    /// The row will be bound from the previous graph, a workspace or an installed version.
    fn settles_without_manifest(&self, row: Row, edge: &DepEdge, asked: &Asked) -> bool {
        if self.linked_workspace(asked).is_some() {
            return true;
        }
        if row.install_peer && edge.behavior.is_peer() {
            if self.peer_provided_from_above(&edge.name, row).is_some()
                || self.is_peer_provided_locally(row, &edge.name)
            {
                return true;
            }
            if !self.cfg.auto_install_peers {
                return true;
            }
        }
        if !self.should_update(row, edge, asked)
            && let (Some(prev), Some(counterpart)) = (self.prev, self.st[row.pkg.index()].prev)
            && let Some(prev_edge) = prev.packages.get(counterpart.index()).and_then(|p| {
                p.dependencies
                    .iter()
                    .find(|e| e.name == edge.name && e.behavior.group() == edge.behavior.group())
            })
            && prev_edge.target.is_some()
            && prev_edge.literal == edge.literal
        {
            return true;
        }
        if let DependencySpec::Npm { range, .. } = &asked.spec
            && let Ok(range) = Range::parse(range)
            && let Some((c, _)) = self
                .candidates(&asked.name)
                .into_iter()
                .find(|(_, v)| v.as_ref().is_some_and(|v| range.satisfies(v)))
        {
            return self.cand_loaded(c) || self.cand_pinned(c);
        }
        false
    }

    /// A manifest arrived: follow the likely versions of the rows that asked for it.
    pub(crate) fn on_manifest(&mut self, name: &str) {
        let Some(wanted) = self.lookahead_waiting.remove(name) else {
            return;
        };
        for (alias, literal, follows_peers) in wanted {
            self.follow_likely_version(name, &alias, &literal, follows_peers);
        }
    }

    fn follow_likely_version(
        &mut self,
        name: &str,
        alias: &str,
        literal: &str,
        follows_peers: bool,
    ) {
        let Ok(spec) = DependencySpec::parse(alias, literal) else {
            return;
        };
        let range = match &spec {
            DependencySpec::Npm { range, .. } => match Range::parse(range) {
                Ok(r) => Some(r),
                Err(_) => return,
            },
            DependencySpec::DistTag { .. } => None,
            _ => return,
        };
        // A version the graphs already have most likely serves it, and its dependencies are
        // queued already.
        if let Some(r) = &range
            && self
                .candidates(name)
                .iter()
                .any(|(_, v)| v.as_ref().is_some_and(|v| r.satisfies(v)))
        {
            return;
        }
        let Some(MState::Ready(manifest)) = self.manifest_state.get(name) else {
            return;
        };
        let manifest = manifest.clone();
        let gate = self.age_gate();
        let wanted = match (&range, &spec) {
            (Some(r), DependencySpec::Npm { range: text, .. }) => Wanted::Range {
                range: r,
                literal: text,
            },
            (None, DependencySpec::DistTag { tag, .. }) => Wanted::Tag { tag, literal },
            _ => return,
        };
        let Some(vm) = pick::find_for_dependency(&manifest, wanted, gate.as_ref()).ok() else {
            return;
        };
        let vm = vm.clone();
        let optional_peers: Vec<&String> = vm
            .peer_dependencies_meta
            .iter()
            .filter(|(_, m)| m.optional)
            .map(|(n, _)| n)
            .collect();
        let groups: [(&[(String, String)], bool); 3] = [
            (&vm.optional_dependencies, false),
            (&vm.dependencies, false),
            (&vm.peer_dependencies, true),
        ];
        let mut next: Vec<(String, String, bool)> = Vec::new();
        for (group, is_peer) in groups {
            if is_peer && !follows_peers {
                break;
            }
            for (dep, range) in group {
                if vm.bundle_dependencies.iter().any(|b| b == dep)
                    || (is_peer && optional_peers.contains(&dep))
                {
                    continue;
                }
                next.push((dep.clone(), range.clone(), is_peer));
            }
        }
        for (dep, range, is_peer) in next {
            self.follow_dependency(&dep, &range, is_peer, follows_peers);
        }
    }

    /// Requests the manifest of `alias@range` if it is worth following.
    fn follow_dependency(&mut self, alias: &str, range: &str, is_peer: bool, follows_peers: bool) {
        let is_alias = trim_literal(range).starts_with("npm:");
        if !is_alias && self.lookahead_seen.contains(alias) {
            return;
        }
        // Names the project aliases, overrides or has a workspace for depend on who declares them.
        if self.ws.member(alias).is_some()
            || self.overrides.has_flat(alias)
            || (is_peer && (alias == "bun" || self.by_name.contains_key(alias)))
        {
            return;
        }
        let name = match DependencySpec::parse(alias, range) {
            Ok(DependencySpec::Npm { name, .. } | DependencySpec::DistTag { name, .. }) => name,
            _ => return,
        };
        if !is_npm_package_name(&name) || self.manifest_state.contains_key(&name) {
            self.lookahead_seen.insert(name);
            return;
        }
        // A locked or pinned version will serve the range without its manifest (bun asks anyway).
        if !is_peer
            && let Ok(DependencySpec::Npm { range, .. }) = DependencySpec::parse(alias, range)
            && let Ok(range) = Range::parse(&range)
            && let Some((c, _)) = self
                .candidates(&name)
                .into_iter()
                .find(|(_, v)| v.as_ref().is_some_and(|v| range.satisfies(v)))
            && (self.cand_loaded(c) || self.cand_pinned(c))
        {
            return;
        }
        self.lookahead_seen.insert(name.clone());
        self.request_manifest(&name, false, true);
        self.lookahead_waiting.entry(name).or_default().push((
            alias.to_owned(),
            range.to_owned(),
            follows_peers,
        ));
    }
}
