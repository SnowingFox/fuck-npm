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

/// The terminal resolution: registry version, source URL, or local path without `file:`.
pub(crate) fn version_of(p: &fnpm_types::Package) -> String {
    match &p.resolution {
        Resolution::Folder { path } | Resolution::LocalTarball { path, .. } => path.clone(),
        resolution => fnpm_registry::redact_url(&resolution.to_string()),
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

/// bun's `+` rows: the direct dependencies of the workspace at `ws_path` whose package this
/// run wrote (`written`, per package id), membership edges excluded, by name.
pub(crate) fn installed_direct(
    graph: &LockGraph,
    ws_path: &str,
    written: &[bool],
) -> Vec<AddedPackage> {
    let mut out: Vec<AddedPackage> = Vec::new();
    let Some(pkg) = graph.workspaces.get(ws_path).and_then(|id| graph.get(*id)) else {
        return out;
    };
    let mut edges: Vec<_> = pkg.dependencies.iter().collect();
    // bun prints them in its dependency order: workspace, dev, optional, prod, peer; each by
    // name.
    let rank = |b: fnpm_types::Behavior| {
        if b.is_workspace() {
            0
        } else if b.is_dev() {
            1
        } else if b.is_optional() && !b.is_peer() {
            2
        } else if b.is_peer() {
            4
        } else {
            3
        }
    };
    edges.sort_by(|a, b| {
        rank(a.behavior)
            .cmp(&rank(b.behavior))
            .then_with(|| a.name.cmp(&b.name))
    });
    for edge in edges {
        let Some(id) = edge.target else { continue };
        // bun skips the root's membership edges (`link_view`), not a declared `workspace:`
        // dependency: that one prints as `+ bar@workspace:packages/bar` when its link is new.
        if graph.workspaces.get(&edge.literal) == Some(&id) {
            continue;
        }
        if !written.get(id.index()).copied().unwrap_or(false) {
            continue;
        }
        let Some(t) = graph.get(id) else { continue };
        if matches!(t.resolution, Resolution::Root) {
            continue;
        }
        let entry = AddedPackage {
            name: edge.name.clone(),
            version: version_of(t),
            alias: (edge.name != t.name).then(|| edge.name.clone()),
            available_version: None,
        };
        if !out.iter().any(|e| e.name == entry.name) {
            out.push(entry);
        }
    }
    out
}

/// bun's `(v1.2.4 available)`: the `latest` of a manifest this run loaded, when it is newer
/// than the registry release that was picked (prereleases get no hint).
pub(crate) fn later_version(
    graph: &LockGraph,
    client: &fnpm_registry::RegistryClient,
    config: &fnpm_config::Config,
    name: &str,
    version: &str,
) -> Option<String> {
    let installed = fnpm_semver::Version::parse(version).ok()?;
    if installed.is_prerelease() {
        return None;
    }
    let npm = graph.packages.iter().any(|p| {
        p.name == name && p.version == version && matches!(p.resolution, Resolution::Npm { .. })
    });
    if !npm {
        return None;
    }
    let manifest = client.cached_manifest(name)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let excludes = config
        .minimum_release_age_ms
        .filter(|ms| *ms > 0)
        .map(|ms| {
            let (excludes, _) = fnpm_resolver::options::ReleaseAgeExcludes::parse(
                &config.minimum_release_age_excludes,
            );
            (ms, excludes)
        });
    let gate = excludes
        .as_ref()
        .map(|(ms, excludes)| fnpm_resolver::pick::AgeGate {
            min_age_ms: *ms,
            now_ms,
            excludes,
        });
    let latest =
        match fnpm_resolver::pick::find_by_dist_tag_with_filter(&manifest, "latest", gate.as_ref())
        {
            fnpm_resolver::pick::Picked::Found(v)
            | fnpm_resolver::pick::Picked::FoundWithFilter { result: v, .. } => v,
            fnpm_resolver::pick::Picked::Err(_) => return None,
        };
    (latest.version.cmp(&installed) == std::cmp::Ordering::Greater)
        .then(|| latest.version.to_string())
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
            real_name: (p.name != req.name && matches!(p.resolution, Resolution::Npm { .. }))
                .then(|| p.name.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_resolutions_redact_url_credentials() {
        let mut p = fnpm_types::Package::new("root", "1.0.0", Resolution::Root);
        p.resolution = Resolution::RemoteTarball {
            url: "https://user:secret@example.com/pkg.tgz?token=secret".into(),
            integrity: None,
        };
        assert_eq!(version_of(&p), "https://***@example.com/pkg.tgz?token=***");
        p.resolution = Resolution::Git {
            url: "https://user:secret@example.com/repo.git".into(),
            committish: String::new(),
            resolved_commit: "abcdef".into(),
            integrity: None,
        };
        assert_eq!(
            version_of(&p),
            "git+https://***@example.com/repo.git#abcdef"
        );
    }
}
