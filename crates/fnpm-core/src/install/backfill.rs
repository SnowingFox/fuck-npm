//! bun's `fetch_necessary_package_metadata_after_yarn_or_pnpm_migration`. yarn.lock and
//! pnpm-lock.yaml do not record bins or whether a package has install scripts (yarn v1 not its
//! os/cpu either, yarn berry not the tarball's integrity), so after migrating one, the registry
//! manifests of the registry packages fill them in. Best effort, like bun: a manifest that cannot
//! be fetched leaves its packages as migrated.

use std::collections::HashMap;
use std::sync::Arc;

use fnpm_config::Config;
use fnpm_lockfile::Format;
use fnpm_registry::{ManifestRequest, PackageManifest, RegistryClient};
use fnpm_types::{CpuSet, LibcSet, LockGraph, OsSet, Resolution};
use futures_util::StreamExt;

/// What a migrated lockfile lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fill {
    /// os/cpu/libc of packages the lockfile left unconstrained.
    pub os_cpu: bool,
    /// The tarball integrity of registry packages that have none.
    pub integrity: bool,
}

/// The backfill a migration from `format` needs (bun's arguments per migrator).
pub(crate) fn needed(format: Option<Format>) -> Option<Fill> {
    match format? {
        Format::YarnV1 => Some(Fill {
            os_cpu: true,
            integrity: false,
        }),
        Format::YarnBerry => Some(Fill {
            os_cpu: true,
            integrity: true,
        }),
        Format::Pnpm => Some(Fill {
            os_cpu: false,
            integrity: false,
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
        filled += 1;
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
