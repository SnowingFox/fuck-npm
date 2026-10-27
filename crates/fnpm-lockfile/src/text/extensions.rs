//! `packageExtensions` in bun.lock records which edges extensions added to which installed
//! packages (bun `package_extensions.rs` `package_extension_lines` / `parse_applied`):
//! `"name@version": { "dependencies": [...], "peerDependencies": [...], "optionalPeers": [...] }`.
//!
//! In the graph, [`LockGraph::package_extensions`] holds the rules. A record read from a
//! lockfile becomes a rule keyed by the exact `name@version` whose entries are the edges'
//! own specifiers, so writing it again gives the same record.

use std::collections::BTreeMap;

use fnpm_types::name::split_name_and_maybe_version;
use fnpm_types::{LockGraph, PackageExtension, PeerMeta, Resolution};

use crate::flat::{Flat, INVALID};
use crate::json_out::quoted;
use crate::tree::{Folder, Hoisted};

/// A `packageExtensions` record: `(name@version, dependencies, peerDependencies, optionalPeers)`.
pub(crate) type ExtRecord = (String, Vec<String>, Vec<String>, Vec<String>);

/// `(key, applies to peer rows, rule test)`.
type Kind<'a> = (&'static str, bool, &'a dyn Fn(&str) -> bool);

/// The rules that apply to package `name@version`.
fn rules_for<'g>(
    graph: &'g LockGraph,
    name: &str,
    version: &str,
) -> impl Iterator<Item = &'g PackageExtension> {
    graph
        .package_extensions
        .iter()
        .filter_map(move |(key, ext)| {
            let (n, range) = split_name_and_maybe_version(key);
            // Prereleases match as the install matches the rule (node-semver
            // `includePrerelease`).
            let ok = n == name
                && range.is_none_or(|r| {
                    r == version
                        || matches!(
                            (
                                fnpm_semver::Range::parse_including_prerelease(r),
                                fnpm_semver::Version::parse(version),
                            ),
                            (Ok(r), Ok(v)) if r.satisfies_with(
                                &v,
                                fnpm_semver::SatisfyOptions { include_prerelease: true },
                            )
                        )
                });
            ok.then_some(ext)
        })
}

/// The `packageExtensions` lines (each ends with `",\n"`), sorted like bun's.
pub(crate) fn lines(flat: &Flat<'_>, h: &Hoisted, folders: &[Folder]) -> Vec<String> {
    let graph = flat.graph;
    if graph.package_extensions.is_empty() {
        return Vec::new();
    }
    let _ = h;
    let mut seen = vec![false; graph.packages.len()];
    let mut out = Vec::new();
    for folder in folders {
        for &d in &folder.rows {
            let pkg = flat.resolutions[d as usize];
            if pkg == INVALID || std::mem::replace(&mut seen[pkg as usize], true) {
                continue;
            }
            let p = &graph.packages[pkg as usize];
            let Resolution::Npm { version, .. } = &p.resolution else {
                continue;
            };
            let rules: Vec<&PackageExtension> = rules_for(graph, &p.name, version).collect();
            if rules.is_empty() {
                continue;
            }
            let rows: Vec<_> = flat.list(pkg).map(|i| &flat.deps[i as usize]).collect();
            let mut line = String::new();
            let kinds: [Kind<'_>; 3] = [
                ("dependencies", false, &|n| {
                    rules.iter().any(|r| {
                        r.dependencies.contains_key(n) || r.optional_dependencies.contains_key(n)
                    })
                }),
                ("peerDependencies", true, &|n| {
                    rules.iter().any(|r| r.peer_dependencies.contains_key(n))
                }),
                ("optionalPeers", true, &|n| {
                    rules
                        .iter()
                        .any(|r| r.peer_dependencies_meta.get(n).is_some_and(|m| m.optional))
                }),
            ];
            for (key, peer, applied) in kinds {
                let mut names: Vec<&str> = rows
                    .iter()
                    .filter(|r| r.behavior.is_peer() == peer && applied(r.name))
                    .map(|r| r.name)
                    .collect();
                if names.is_empty() {
                    continue;
                }
                names.sort_unstable();
                names.dedup();
                if line.is_empty() {
                    quoted(&mut line, &format!("{}@{}", p.name, version));
                    line.push_str(": {");
                } else {
                    line.push(',');
                }
                line.push_str(" \"");
                line.push_str(key);
                line.push_str("\": [");
                for (i, n) in names.iter().enumerate() {
                    if i != 0 {
                        line.push_str(", ");
                    }
                    quoted(&mut line, n);
                }
                line.push(']');
            }
            if !line.is_empty() {
                line.push_str(" },\n");
                out.push(line);
            }
        }
    }
    out.sort_unstable();
    out
}

/// Turns a parsed `packageExtensions` record into rules, taking each specifier from the
/// package's own edge of that name.
pub(crate) fn rules_from_records(graph: &mut LockGraph, records: &[ExtRecord]) {
    for (label, deps, peers, optional_peers) in records {
        let (name, version) = split_name_and_maybe_version(label);
        let Some(pkg) = graph
            .packages
            .iter()
            .find(|p| p.name == name && matches!(&p.resolution, Resolution::Npm { version: v, .. } if Some(v.as_str()) == version))
        else {
            // A record for a package that is no longer installed is dropped, as bun drops it
            // on the next save.
            continue;
        };
        let literal = |n: &str, peer: bool| {
            pkg.dependencies
                .iter()
                .find(|e| e.name == n && e.behavior.is_peer() == peer)
                .map_or_else(String::new, |e| e.literal.clone())
        };
        let mut ext = PackageExtension::default();
        for n in deps {
            let edge = pkg
                .dependencies
                .iter()
                .find(|e| e.name == *n && !e.behavior.is_peer());
            match edge {
                Some(e) if e.behavior.is_optional() => {
                    ext.optional_dependencies
                        .insert(n.clone(), e.literal.clone());
                }
                _ => {
                    ext.dependencies.insert(n.clone(), literal(n, false));
                }
            }
        }
        for n in peers {
            ext.peer_dependencies.insert(n.clone(), literal(n, true));
        }
        for n in optional_peers {
            ext.peer_dependencies_meta
                .insert(n.clone(), PeerMeta { optional: true });
        }
        merge(&mut graph.package_extensions, label.clone(), ext);
    }
}

fn merge(map: &mut BTreeMap<String, PackageExtension>, key: String, ext: PackageExtension) {
    let e = map.entry(key).or_default();
    e.dependencies.extend(ext.dependencies);
    e.optional_dependencies.extend(ext.optional_dependencies);
    e.peer_dependencies.extend(ext.peer_dependencies);
    e.peer_dependencies_meta.extend(ext.peer_dependencies_meta);
}
