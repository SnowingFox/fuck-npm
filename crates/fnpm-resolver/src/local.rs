//! Building [`Package`]s and their edges from a `package.json` (root, workspace members, folders,
//! git / tarball packages) and from registry [`VersionManifest`]s, with bun's rules for
//! duplicates, optional peers, `peerDependenciesMeta`-only peers, bundled dependencies and
//! `catalog:` outside the project (bun `lockfile/Package.rs` `parse_with_json`, `from_npm`).

use std::cmp::Ordering;

use fnpm_registry::VersionManifest;
use fnpm_types::meta::BundledDependencies;
use fnpm_types::spec::{nameless_npm_alias, trim_literal};
use fnpm_types::{
    Behavior, Bin, DepEdge, DependencyGroup, Package, PackageMeta, PeerMeta, Resolution,
    ScriptFlags, SpecTag,
};
use fnpm_workspace::PackageJson;
use serde_json::Value;

use crate::{Error, Result};

/// Which kind of package a package.json belongs to; decides the groups read and the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalKind {
    Root,
    Workspace,
    /// `file:` folder.
    Folder,
    /// `link:` (no dependencies are installed for a symlink).
    Link,
    /// A git, GitHub or tarball package.
    Remote,
}

impl LocalKind {
    fn is_project(self) -> bool {
        matches!(self, Self::Root | Self::Workspace)
    }
}

/// bun's `Behavior::cmp`: workspace, dev, optional, prod, peer.
pub(crate) fn behavior_order(a: Behavior, b: Behavior) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    for flag in [Behavior::WORKSPACE, Behavior::DEV] {
        if a.contains(flag) != b.contains(flag) {
            return if a.contains(flag) {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
    }
    if a.is_optional() != b.is_optional() {
        return if a.is_optional() {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    for flag in [Behavior::PROD, Behavior::PEER] {
        if a.contains(flag) != b.contains(flag) {
            return if a.contains(flag) {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
    }
    Ordering::Equal
}

fn bundled_from_raw(raw: &Value, list: &[String]) -> BundledDependencies {
    let field = raw
        .get("bundleDependencies")
        .or_else(|| raw.get("bundledDependencies"));
    match field {
        Some(Value::Bool(b)) => BundledDependencies::All(*b),
        _ if !list.is_empty() => BundledDependencies::List(list.to_vec()),
        Some(Value::Array(items)) => BundledDependencies::List(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ),
        _ => BundledDependencies::None,
    }
}

fn catalog_outside_project(literal: &str) -> bool {
    fnpm_types::infer_tag(trim_literal(literal)) == SpecTag::Catalog
}

/// The edges a `package.json` declares, in bun's processing order (behavior, then name).
pub(crate) fn package_json_edges(
    pj: &PackageJson,
    kind: LocalKind,
) -> Result<(Vec<DepEdge>, Vec<String>)> {
    let mut edges: Vec<DepEdge> = Vec::new();
    let mut warnings = Vec::new();
    if kind == LocalKind::Link {
        return Ok((edges, warnings));
    }
    let bundled = bundled_from_raw(&pj.raw, &pj.bundle_dependencies);
    let mut optional_peers: Vec<&str> = pj
        .peer_dependencies_meta
        .iter()
        .filter(|(_, m)| m.optional)
        .map(|(n, _)| n.as_str())
        .collect();
    let groups: &[DependencyGroup] = match kind {
        LocalKind::Remote => &[
            DependencyGroup::Prod,
            DependencyGroup::Optional,
            DependencyGroup::Peer,
        ],
        _ => &DependencyGroup::ALL,
    };
    for &group in groups {
        for (name, literal) in pj.group(group) {
            if name.is_empty() || nameless_npm_alias(literal).is_some() {
                if !kind.is_project() {
                    continue;
                }
                let message = match nameless_npm_alias(literal) {
                    Some(alias) if !name.is_empty() => {
                        format!("invalid package name \"{alias}\" in dependency \"{literal}\"")
                    }
                    _ => format!("empty dependency name (in \"{}\")", group.field()),
                };
                return Err(Error::InvalidPackageJson {
                    path: "package.json".into(),
                    message,
                });
            }
            let mut behavior = group.behavior();
            if group == DependencyGroup::Peer {
                if let Some(i) = optional_peers.iter().position(|n| n == name) {
                    optional_peers.remove(i);
                    behavior |= Behavior::OPTIONAL;
                } else if !kind.is_project() && name == "bun" {
                    behavior |= Behavior::OPTIONAL;
                }
            }
            if bundled.contains(name) {
                behavior |= Behavior::BUNDLED;
            }
            if !kind.is_project()
                && group == DependencyGroup::Peer
                && catalog_outside_project(literal)
            {
                behavior |= Behavior::OPTIONAL;
            }
            let edge = DepEdge::new(name.clone(), trim_literal(literal), behavior);
            if group != DependencyGroup::Peer
                && let Some(existing) = edges.iter_mut().find(|e| {
                    e.name == edge.name && !e.behavior.is_peer() && !e.behavior.is_workspace()
                })
            {
                if group == DependencyGroup::Optional {
                    *existing = edge;
                    continue;
                }
                if kind.is_project() {
                    warnings.push(format!(
                        "Duplicate dependency: \"{name}\" specified in package.json"
                    ));
                }
            }
            edges.push(edge);
        }
    }
    for name in optional_peers {
        if kind == LocalKind::Remote || kind.is_project() || kind == LocalKind::Folder {
            edges.push(DepEdge::new(name, "*", Behavior::PEER | Behavior::OPTIONAL));
        }
    }
    edges.sort_by(|a, b| behavior_order(a.behavior, b.behavior).then_with(|| a.name.cmp(&b.name)));
    Ok((edges, warnings))
}

fn script_flags(scripts: &std::collections::BTreeMap<String, String>) -> ScriptFlags {
    ScriptFlags::from_names(scripts.keys().map(String::as_str))
}

/// A package (without edges) from a `package.json`.
pub(crate) fn package_from_json(
    pj: &PackageJson,
    fallback_name: &str,
    resolution: Resolution,
) -> Package {
    let name = pj
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name.to_owned());
    let mut package = Package::new(name, pj.version.clone().unwrap_or_default(), resolution);
    package.bin = pj.bin.clone();
    package.platform = pj.platform;
    let scripts = script_flags(&pj.scripts);
    package.meta = PackageMeta {
        scripts,
        has_install_script: scripts.has_install_script(),
        deprecated: None,
        engines: pj.engines.clone(),
        bundled_dependencies: bundled_from_raw(&pj.raw, &pj.bundle_dependencies),
        bundled: false,
        license: pj
            .raw
            .get("license")
            .and_then(Value::as_str)
            .map(str::to_owned),
        man: Vec::new(),
    };
    package.peer_meta = pj.peer_dependencies_meta.clone();
    package
}

/// A registry package with its edges (bun `Package::from_npm`): `dependencies`, then
/// `optionalDependencies` (replacing a same-named dependency), then peers not declared above.
pub(crate) fn package_from_manifest(
    name: &str,
    vm: &VersionManifest,
    tarball_url: Option<String>,
) -> Package {
    let version = vm.version.to_string();
    let resolution = Resolution::Npm {
        version: version.clone(),
        tarball_url,
        integrity: vm.dist.integrity,
    };
    let mut package = Package::new(name, version, resolution);
    let bundled = &vm.bundle_dependencies;
    let mut edges: Vec<DepEdge> = Vec::with_capacity(
        vm.dependencies.len() + vm.optional_dependencies.len() + vm.peer_dependencies.len(),
    );
    let usable = |n: &str, lit: &str| !n.is_empty() && nameless_npm_alias(lit).is_none();
    for (n, lit) in &vm.dependencies {
        if usable(n, lit) {
            let mut b = Behavior::PROD;
            if bundled.iter().any(|x| x == n) {
                b |= Behavior::BUNDLED;
            }
            edges.push(DepEdge::new(n.clone(), trim_literal(lit), b));
        }
    }
    for (n, lit) in &vm.optional_dependencies {
        if !usable(n, lit) {
            continue;
        }
        let mut b = Behavior::OPTIONAL;
        if bundled.iter().any(|x| x == n) {
            b |= Behavior::BUNDLED;
        }
        let edge = DepEdge::new(n.clone(), trim_literal(lit), b);
        if let Some(i) = edges.iter().position(|e| &e.name == n) {
            edges.remove(i);
        }
        edges.push(edge);
    }
    let mut meta_optional: Vec<&String> = vm
        .peer_dependencies_meta
        .iter()
        .filter(|(_, m)| m.optional)
        .map(|(n, _)| n)
        .collect();
    for (n, lit) in &vm.peer_dependencies {
        if !usable(n, lit) || edges.iter().any(|e| &e.name == n) {
            continue;
        }
        let mut b = Behavior::PEER;
        if let Some(i) = meta_optional.iter().position(|m| *m == n) {
            meta_optional.remove(i);
            b |= Behavior::OPTIONAL;
        } else if n == "bun" {
            b |= Behavior::OPTIONAL;
        }
        if catalog_outside_project(lit) {
            b |= Behavior::OPTIONAL;
        }
        if bundled.iter().any(|x| x == n) {
            b |= Behavior::BUNDLED;
        }
        edges.push(DepEdge::new(n.clone(), trim_literal(lit), b));
    }
    for n in meta_optional {
        if !edges.iter().any(|e| &e.name == n) {
            edges.push(DepEdge::new(
                n.clone(),
                "*",
                Behavior::PEER | Behavior::OPTIONAL,
            ));
        }
    }
    package.dependencies = edges;
    package.peer_meta = vm
        .peer_dependencies_meta
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                PeerMeta {
                    optional: v.optional,
                },
            )
        })
        .collect();
    package.bin = match &vm.bin {
        Bin::None => Bin::None,
        other => other.clone(),
    };
    package.platform = vm.platform;
    let scripts = script_flags(&vm.scripts);
    package.meta = PackageMeta {
        scripts,
        has_install_script: vm.has_install_script || scripts.has_install_script(),
        deprecated: vm.deprecated.clone().filter(|d| !d.is_empty()),
        engines: vm.engines.clone(),
        bundled_dependencies: if bundled.is_empty() {
            BundledDependencies::None
        } else {
            BundledDependencies::List(bundled.clone())
        },
        bundled: false,
        license: vm.license.clone(),
        man: Vec::new(),
    };
    package
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn pj(
        deps: &[(&str, &str)],
        dev: &[(&str, &str)],
        opt: &[(&str, &str)],
        peer: &[(&str, &str)],
    ) -> PackageJson {
        let own = |l: &[(&str, &str)]| {
            l.iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect()
        };
        PackageJson {
            dependencies: own(deps),
            dev_dependencies: own(dev),
            optional_dependencies: own(opt),
            peer_dependencies: own(peer),
            peer_dependencies_meta: BTreeMap::from([
                ("m".to_owned(), PeerMeta { optional: true }),
                ("p".to_owned(), PeerMeta { optional: true }),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn duplicates_optional_peers_and_order() {
        let p = pj(
            &[("a", "1"), ("o", "1")],
            &[("a", "2"), ("d", "1")],
            &[("o", "2")],
            &[("p", "*"), ("q", "^1")],
        );
        let (edges, warnings) = package_json_edges(&p, LocalKind::Root).unwrap();
        let summary: Vec<(&str, &str, DependencyGroup)> = edges
            .iter()
            .map(|e| (e.name.as_str(), e.literal.as_str(), e.behavior.group()))
            .collect();
        assert_eq!(
            summary,
            [
                ("a", "2", DependencyGroup::Dev),
                ("d", "1", DependencyGroup::Dev),
                ("o", "2", DependencyGroup::Optional),
                ("a", "1", DependencyGroup::Prod),
                ("m", "*", DependencyGroup::Peer),
                ("p", "*", DependencyGroup::Peer),
                ("q", "^1", DependencyGroup::Peer),
            ]
        );
        assert!(
            edges
                .iter()
                .find(|e| e.name == "p")
                .unwrap()
                .behavior
                .is_optional_peer()
        );
        assert!(
            edges
                .iter()
                .find(|e| e.name == "m")
                .unwrap()
                .behavior
                .is_optional_peer()
        );
        assert!(
            !edges
                .iter()
                .find(|e| e.name == "q")
                .unwrap()
                .behavior
                .is_optional_peer()
        );
        assert_eq!(
            warnings,
            ["Duplicate dependency: \"a\" specified in package.json"]
        );
    }

    #[test]
    fn remote_packages_skip_dev_and_tolerate_unnamed() {
        let p = pj(
            &[("", "1"), ("x", "npm:@1")],
            &[("d", "1")],
            &[],
            &[("bun", "*")],
        );
        let (edges, _) = package_json_edges(&p, LocalKind::Remote).unwrap();
        let names: Vec<&str> = edges.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["bun", "m", "p"]);
        assert!(edges[0].behavior.is_optional_peer());
        assert!(package_json_edges(&p, LocalKind::Root).is_err());
        assert!(
            package_json_edges(&p, LocalKind::Link)
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn behavior_order_matches_bun() {
        let mut list = [
            Behavior::PEER,
            Behavior::PROD,
            Behavior::OPTIONAL,
            Behavior::DEV,
            Behavior::PROD | Behavior::WORKSPACE,
        ];
        list.sort_by(|a, b| behavior_order(*a, *b));
        assert_eq!(
            list,
            [
                Behavior::PROD | Behavior::WORKSPACE,
                Behavior::DEV,
                Behavior::OPTIONAL,
                Behavior::PROD,
                Behavior::PEER
            ]
        );
    }
}
