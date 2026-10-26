//! bun's `fetch_necessary_package_metadata_after_yarn_or_pnpm_migration`. yarn.lock and
//! pnpm-lock.yaml do not record bins or whether a package has install scripts (yarn v1 not its
//! os/cpu or peer dependencies either, yarn berry not the tarball's integrity), so after
//! migrating one, the registry manifests of the registry packages fill them in. Best effort,
//! like bun: a manifest that cannot be fetched leaves its packages as migrated.
//!
//! The peers go beyond bun: without them the hoister can place a package where its peer
//! resolves to another major (webpack's `ajv-keywords@5`, peer `ajv@^8`, next to a hoisted
//! `ajv@6`), and the package fails to load.

use std::collections::HashMap;
use std::sync::Arc;

use fnpm_config::Config;
use fnpm_lockfile::Format;
use fnpm_registry::{ManifestRequest, PackageManifest, RegistryClient};
use fnpm_types::{
    Behavior, CpuSet, DepEdge, LibcSet, LockGraph, OsSet, PackageId, PeerMeta, Resolution,
};
use futures_util::StreamExt;

/// What a migrated lockfile lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fill {
    /// os/cpu/libc of packages the lockfile left unconstrained.
    pub os_cpu: bool,
    /// The tarball integrity of registry packages that have none.
    pub integrity: bool,
    /// `peerDependencies` (and their `peerDependenciesMeta`), which yarn v1 does not record.
    pub peers: bool,
}

/// The backfill a migration from `format` needs (bun's arguments per migrator).
pub(crate) fn needed(format: Option<Format>) -> Option<Fill> {
    match format? {
        Format::YarnV1 => Some(Fill {
            os_cpu: true,
            integrity: false,
            peers: true,
        }),
        Format::YarnBerry => Some(Fill {
            os_cpu: true,
            integrity: true,
            peers: false,
        }),
        Format::Pnpm => Some(Fill {
            os_cpu: false,
            integrity: false,
            peers: false,
        }),
        _ => None,
    }
}

const CONCURRENCY: usize = 64;

/// Fetches the manifests of every registry package of `graph` and fills in what `fill` asks for
/// plus bins and the install-script flag. Returns how many packages were completed.
pub(crate) async fn run(
    graph: &mut LockGraph,
    client: &RegistryClient,
    config: &Config,
    fill: Fill,
) -> usize {
    let mut names: Vec<String> = graph
        .packages
        .iter()
        .filter(|p| matches!(p.resolution, Resolution::Npm { .. }))
        .map(|p| p.name.clone())
        .collect();
    names.sort_unstable();
    names.dedup();
    let manifests: HashMap<String, Arc<PackageManifest>> = futures_util::stream::iter(names)
        .map(|name| {
            let client = client.clone();
            async move {
                let manifest = client.manifest(&name, ManifestRequest::default()).await;
                (name, manifest.ok())
            }
        })
        .buffer_unordered(CONCURRENCY)
        .filter_map(|(name, m)| async move { m.map(|m| (name, m)) })
        .collect()
        .await;
    apply(graph, &manifests, config, fill)
}

/// The part of [`run`] after the fetches.
pub(crate) fn apply(
    graph: &mut LockGraph,
    manifests: &HashMap<String, Arc<PackageManifest>>,
    config: &Config,
    fill: Fill,
) -> usize {
    let mut filled = 0;
    // Versions in the graph by name, for the target of a filled-in peer.
    let mut by_name: HashMap<String, Vec<(fnpm_semver::Version, PackageId)>> = HashMap::new();
    if fill.peers {
        for (i, p) in graph.packages.iter().enumerate() {
            if let Resolution::Npm { version, .. } = &p.resolution
                && let Ok(v) = fnpm_semver::Version::parse(version)
            {
                by_name
                    .entry(p.name.clone())
                    .or_default()
                    .push((v, PackageId(i as u32)));
            }
        }
    }
    let mut added_peers = false;
    for p in &mut graph.packages {
        let Resolution::Npm {
            version,
            tarball_url,
            integrity,
        } = &mut p.resolution
        else {
            continue;
        };
        let Some(manifest) = manifests.get(&p.name) else {
            continue;
        };
        let Some(found) = fnpm_semver::Version::parse(version)
            .ok()
            .and_then(|v| manifest.find(&v))
        else {
            continue;
        };
        // The manifest only describes the locked tarball if it comes from the registry, or if
        // the hashes agree (a mirror serving the registry's path from another host otherwise).
        let registry = config.registry_for(&p.name).url.as_str();
        let from_registry = tarball_url.as_deref().is_none_or(|url| {
            under_registry(url, registry)
                || (!found.dist.tarball.is_empty() && url == found.dist.tarball)
        });
        let described = from_registry
            || match (integrity.as_ref(), found.dist.integrity.as_ref()) {
                (Some(locked), Some(published)) if locked.algorithm() == published.algorithm() => {
                    locked == published
                }
                _ => tarball_url
                    .as_deref()
                    .is_some_and(|url| url.ends_with(&default_tarball_path(&p.name, version))),
            };
        if !described {
            continue;
        }
        p.bin = found.bin.clone();
        p.meta.has_install_script = found.has_install_script;
        if fill.os_cpu {
            if p.platform.os == OsSet::ALL {
                p.platform.os = found.platform.os;
            }
            if p.platform.cpu == CpuSet::ALL {
                p.platform.cpu = found.platform.cpu;
            }
            if p.platform.libc == LibcSet::NONE {
                p.platform.libc = found.platform.libc;
            }
        }
        if fill.integrity && from_registry && integrity.is_none() {
            *integrity = found.dist.integrity;
        }
        if fill.peers {
            for (name, range) in &found.peer_dependencies {
                // A dependency of the same name (some packages list both) already decides it.
                if p.dependencies.iter().any(|e| &e.name == name) {
                    continue;
                }
                let optional = found
                    .peer_dependencies_meta
                    .get(name)
                    .is_some_and(|m| m.optional);
                let target = fnpm_semver::Range::parse(range).ok().and_then(|r| {
                    by_name.get(name).and_then(|versions| {
                        versions
                            .iter()
                            .filter(|(v, _)| r.satisfies(v))
                            .max_by(|a, b| a.0.cmp(&b.0))
                            .map(|&(_, id)| id)
                    })
                });
                let mut edge = DepEdge::new(
                    name.clone(),
                    range.clone(),
                    Behavior::PEER.with(Behavior::OPTIONAL, optional),
                );
                edge.target = target;
                p.dependencies.push(edge);
                if optional {
                    p.peer_meta
                        .insert(name.clone(), PeerMeta { optional: true });
                }
                added_peers = true;
            }
        }
        filled += 1;
    }
    if added_peers {
        graph.sort_dependencies();
    }
    filled
}

/// `true` if `url` is `registry` or below it.
fn under_registry(url: &str, registry: &str) -> bool {
    let registry = registry.trim_end_matches('/');
    url.starts_with(registry)
        && (url.len() == registry.len() || url.as_bytes()[registry.len()] == b'/')
}

/// `/<name>/-/<unscoped>-<version>.tgz`.
fn default_tarball_path(name: &str, version: &str) -> String {
    let unscoped = name.rsplit_once('/').map_or(name, |(_, n)| n);
    format!("/{name}/-/{unscoped}-{version}.tgz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnpm_config::{CliOverrides, Env, LoadOptions};
    use fnpm_types::Package;

    fn npm(name: &str, version: &str, deps: Vec<DepEdge>) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            resolution: Resolution::Npm {
                version: version.into(),
                tarball_url: None,
                integrity: None,
            },
            dependencies: deps,
            peer_meta: Default::default(),
            bin: Default::default(),
            platform: Default::default(),
            meta: Default::default(),
        }
    }

    fn dep(name: &str, range: &str, target: u32) -> DepEdge {
        let mut edge = DepEdge::new(name, range, Behavior::PROD);
        edge.target = Some(PackageId(target));
        edge
    }

    #[test]
    fn yarn_v1_peers_come_from_the_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&LoadOptions {
            cwd: dir.path().to_owned(),
            cli: CliOverrides::default(),
            env: Env::default(),
            project_dir: Some(dir.path().to_owned()),
            node_version: None,
        })
        .unwrap();
        // webpack's yarn.lock: schema-utils@4 > ajv@8 + ajv-keywords@5; ajv@6 elsewhere.
        let mut graph = LockGraph::new(Package {
            name: "root".into(),
            version: String::new(),
            resolution: Resolution::Root,
            dependencies: vec![dep("ajv", "^6.12.5", 2), dep("schema-utils", "^4.0.0", 1)],
            peer_meta: Default::default(),
            bin: Default::default(),
            platform: Default::default(),
            meta: Default::default(),
        });
        graph.packages.extend([
            npm(
                "schema-utils",
                "4.8.1",
                vec![dep("ajv", "^8.20.0", 3), dep("ajv-keywords", "^5.1.0", 4)],
            ),
            npm("ajv", "6.15.0", vec![]),
            npm("ajv", "8.20.0", vec![]),
            npm(
                "ajv-keywords",
                "5.1.0",
                vec![dep("fast-deep-equal", "^3.1.3", 5)],
            ),
            npm("fast-deep-equal", "3.1.3", vec![]),
        ]);
        let manifest = |name: &str, json: &str| {
            (
                name.to_owned(),
                Arc::new(PackageManifest::parse(name, json.as_bytes()).unwrap()),
            )
        };
        let manifests: HashMap<_, _> = [manifest(
            "ajv-keywords",
            r#"{"name":"ajv-keywords","dist-tags":{"latest":"5.1.0"},"versions":{"5.1.0":{
                "name":"ajv-keywords","version":"5.1.0",
                "dependencies":{"fast-deep-equal":"^3.1.3"},
                "peerDependencies":{"ajv":"^8.8.2","missing":"^1.0.0","maybe":"^2.0.0"},
                "peerDependenciesMeta":{"maybe":{"optional":true}},
                "dist":{"tarball":"https://registry.npmjs.org/ajv-keywords/-/ajv-keywords-5.1.0.tgz"}}}}"#,
        )]
        .into_iter()
        .collect();
        let fill = needed(Some(Format::YarnV1)).unwrap();
        assert_eq!(apply(&mut graph, &manifests, &config, fill), 1);
        let keywords = &graph.packages[4];
        let edges: Vec<_> = keywords
            .dependencies
            .iter()
            .map(|e| (e.name.as_str(), e.literal.as_str(), e.behavior, e.target))
            .collect();
        assert_eq!(
            edges,
            [
                ("ajv", "^8.8.2", Behavior::PEER, Some(PackageId(3))),
                (
                    "fast-deep-equal",
                    "^3.1.3",
                    Behavior::PROD,
                    Some(PackageId(5))
                ),
                (
                    "maybe",
                    "^2.0.0",
                    Behavior::PEER.with(Behavior::OPTIONAL, true),
                    None
                ),
                ("missing", "^1.0.0", Behavior::PEER, None),
            ]
        );
        assert!(keywords.peer_meta["maybe"].optional);

        // The hoister keeps ajv-keywords next to the ajv@8 its peer needs.
        let tree = fnpm_lockfile::tree::hoist(
            &graph,
            &fnpm_lockfile::tree::HoistOptions::all(fnpm_types::Platform::current()),
        );
        let paths: Vec<String> = tree.nodes.iter().map(|n| n.path.clone()).collect();
        assert!(
            paths.iter().any(|p| p == "schema-utils/ajv-keywords"),
            "{paths:?}"
        );

        // Other formats record peers themselves.
        assert!(!needed(Some(Format::YarnBerry)).unwrap().peers);
        assert!(!needed(Some(Format::Pnpm)).unwrap().peers);
    }
}
