//! The install summary, filled the way bun prints it (`install_with_manager.rs:1264-1530`).

use std::collections::{BTreeMap, BTreeSet};

use fnpm_resolver::Request;
use fnpm_types::event::{AddedPackage, RequestedPackage, UpdatedPackage};
use fnpm_types::{LockGraph, PackageId, Platform, Resolution};

/// `(workspace path, dependency name)` to `(package name, version)` for every resolved direct
/// dependency of the root and the workspaces, workspace links excluded.
pub(crate) fn direct(graph: &LockGraph) -> BTreeMap<(String, String), (String, String)> {
    let mut out = BTreeMap::new();
    for (path, id) in &graph.workspaces {
        let Some(pkg) = graph.get(*id) else { continue };
        for edge in &pkg.dependencies {
            let Some(t) = edge.target.and_then(|t| graph.get(t)) else {
                continue;
            };
            if matches!(
                t.resolution,
                Resolution::Workspace { .. } | Resolution::Root
            ) {
                continue;
            }
            out.insert(
                (path.clone(), edge.name.clone()),
                (t.name.clone(), version_of(t)),
            );
        }
    }
    out
}

/// The version shown for a package (the resolution for packages without one).
pub(crate) fn version_of(p: &fnpm_types::Package) -> String {
    if p.version.is_empty() {
        p.resolution.to_string()
    } else {
        p.version.clone()
    }
}

/// `added`, `updated` and `removed` between the previous and the new graph.
pub(crate) fn changes(
    previous: Option<&LockGraph>,
    next: &LockGraph,
) -> (Vec<AddedPackage>, Vec<UpdatedPackage>, Vec<String>) {
    let before = previous.map(direct).unwrap_or_default();
    let after = direct(next);
    let mut added: Vec<AddedPackage> = Vec::new();
    let mut updated = Vec::new();
    let mut seen_updated = BTreeSet::new();
    for ((ws, alias), (name, version)) in &after {
        match before.get(&(ws.clone(), alias.clone())) {
            None => {
                let entry = AddedPackage {
                    name: name.clone(),
                    version: version.clone(),
                    alias: (alias != name).then(|| alias.clone()),
                    available_version: None,
                };
                if !added.contains(&entry) {
                    added.push(entry);
                }
            }
            Some((_, old)) if old != version => {
                if seen_updated.insert((alias.clone(), old.clone(), version.clone())) {
                    updated.push(UpdatedPackage {
                        name: alias.clone(),
                        from: old.clone(),
                        to: version.clone(),
                        available_version: None,
                    });
                }
            }
            Some(_) => {}
        }
    }
    let mut removed: Vec<String> = before
        .keys()
        .filter(|k| !after.contains_key(*k))
        .map(|(_, alias)| alias.clone())
        .collect();
    removed.sort();
    removed.dedup();
    (added, updated, removed)
}

/// `requested`: what each `add` / `update` request resolved to, with its bins.
pub(crate) fn requested(
    graph: &LockGraph,
    requests: &[(Request, PackageId)],
    platform: &Platform,
) -> Vec<RequestedPackage> {
    let mut out: Vec<RequestedPackage> = Vec::new();
    for (req, id) in requests {
        let Some(p) = graph.get(*id) else { continue };
        let entry = RequestedPackage {
            name: req.name.clone(),
            version: version_of(p),
            real_name: (p.name != req.name).then(|| p.name.clone()),
            bins: p
                .bin
                .entries(&p.name)
                .into_iter()
                .map(|(n, _)| n.to_owned())
                .collect(),
            skipped: p.is_disabled(platform).then_some(true),
        };
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}
