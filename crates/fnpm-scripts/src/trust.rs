//! Who may run lifecycle scripts (bun `lockfile.rs:1007-1093,3620-3863`,
//! `PackageInstaller.rs`, `PackageManagerLifecycle.rs:415-582`).
//!
//! Three modes, as in bun: no `trustedDependencies` field anywhere (the built-in default list
//! applies, to npm packages fetched from their canonical registry tarball URL only), an explicit
//! list (replaces the default list), and an explicit empty list (trusts nothing). The root and
//! the workspaces always run their own scripts.

use std::collections::BTreeSet;

use fnpm_types::{LockGraph, PackageId, Resolution};

use crate::phf_hash::{phf_hash, phf_slot, phf_split};

mod table {
    include!(concat!(env!("OUT_DIR"), "/default_trusted.rs"));
}

/// Built-in list: `bun/src/install/default-trusted-dependencies.txt`, copied to
/// `crates/fnpm-scripts/data/default-trusted-dependencies.txt` and compiled into a static
/// perfect-hash set by `build.rs`. Sorted, like `bun pm default-trusted` prints it.
pub fn default_trusted() -> &'static [&'static str] {
    &table::SORTED
}

/// Whether `name` is on the built-in default trusted list. One hash and one string compare.
#[inline]
pub fn is_default_trusted(name: &str) -> bool {
    let (g, f1, f2) = phf_split(phf_hash(name.as_bytes(), table::PHF_SEED));
    let (d1, d2) = table::PHF_DISPS[g as usize % table::PHF_DISPS.len()];
    let slot = phf_slot(f1, f2, d1, d2, table::PHF_KEYS.len() as u32);
    table::PHF_KEYS[slot as usize] == name
}

/// The registry's canonical tarball URL of `name@version` (bun `registry_tarball_url`):
/// `<registry>/<name>/-/<unscoped name>-<version>.tgz`.
pub fn canonical_tarball_url(registry: &str, name: &str, version: &str) -> String {
    let registry = registry.trim_end_matches('/');
    let unscoped = match name.strip_prefix('@').and_then(|rest| rest.split_once('/')) {
        Some((_, unscoped)) => unscoped,
        None => name,
    };
    format!("{registry}/{name}/-/{unscoped}-{version}.tgz")
}

/// The trust decision for one install.
pub struct TrustPolicy {
    /// `None`: the default list applies (npm resolutions from the default registry only).
    pub explicit: Option<BTreeSet<String>>,
    /// `--trust` / `fnpm pm trust` names for this run.
    pub extra: BTreeSet<String>,
}

/// A dependency whose lifecycle scripts were not run because it is not trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub package: PackageId,
    pub name: String,
    pub version: String,
    /// The script events that would run (`preinstall`, `install`, ...), in order.
    pub scripts: Vec<String>,
}

impl TrustPolicy {
    /// The policy recorded in the graph (`trustedDependencies` of the root and workspaces) plus
    /// names trusted for this run.
    pub fn from_graph(graph: &LockGraph, extra: impl IntoIterator<Item = String>) -> Self {
        Self {
            explicit: graph.trusted_dependencies.clone(),
            extra: extra.into_iter().collect(),
        }
    }

    /// Root and workspaces always run their own scripts. Others: by real name for npm
    /// resolutions; by alias for non-npm resolutions only when the root or a workspace
    /// declares that exact resolution; `file:` folders inside their declarer are never trusted.
    ///
    /// Default-list trust compares a recorded tarball URL against the default registry
    /// ([`fnpm_config::DEFAULT_REGISTRY`]); use [`TrustPolicy::is_trusted_with_registry`] when
    /// scoped registries are configured.
    pub fn is_trusted(&self, graph: &LockGraph, id: PackageId) -> bool {
        self.is_trusted_with_registry(graph, id, &|_| fnpm_config::DEFAULT_REGISTRY)
    }

    /// [`TrustPolicy::is_trusted`] with the registry URL that serves each package name
    /// (`Config::registry_for(name).url`).
    pub fn is_trusted_with_registry<'r>(
        &self,
        graph: &LockGraph,
        id: PackageId,
        registry_for: &dyn Fn(&str) -> &'r str,
    ) -> bool {
        let Some(pkg) = graph.get(id) else {
            return false;
        };
        match pkg.resolution {
            Resolution::Root | Resolution::Workspace { .. } => return true,
            Resolution::Folder { .. } if !is_trusted_folder_package(graph, id) => return false,
            _ => {}
        }
        self.has_trusted_dependency(graph, id, registry_for)
    }

    /// bun `Lockfile::has_trusted_dependency` at package granularity: whether a dependency
    /// resolving to `id` is trusted, ignoring the root/workspace and folder rules.
    pub fn has_trusted_dependency<'r>(
        &self,
        graph: &LockGraph,
        id: PackageId,
        registry_for: &dyn Fn(&str) -> &'r str,
    ) -> bool {
        let Some(pkg) = graph.get(id) else {
            return false;
        };
        if let Resolution::Npm {
            version,
            tarball_url,
            ..
        } = &pkg.resolution
        {
            if self.extra.contains(&pkg.name) {
                return true;
            }
            return match &self.explicit {
                Some(names) => names.contains(&pkg.name),
                None => {
                    is_default_trusted(&pkg.name)
                        && tarball_url.as_deref().is_none_or(|url| {
                            url.is_empty()
                                || url
                                    == canonical_tarball_url(
                                        registry_for(&pkg.name),
                                        &pkg.name,
                                        version,
                                    )
                        })
                }
            };
        }
        // Non-npm: trusted by the alias a root or workspace declares for this exact package.
        declared_aliases(graph, id).any(|alias| {
            self.extra.contains(alias)
                || self
                    .explicit
                    .as_ref()
                    .is_some_and(|names| names.contains(alias))
        })
    }

    /// Every package with install scripts that this policy blocks, for the summary's
    /// `blockedScripts` count. `scripts_of` returns the script events a package would run
    /// once trusted (read from its installed package.json, see
    /// [`crate::scripts::PackageScripts::blocked_events`]); `None` skips the package.
    pub fn blocked(
        &self,
        graph: &LockGraph,
        mut scripts_of: impl FnMut(PackageId) -> Option<Vec<String>>,
    ) -> Vec<Blocked> {
        let mut out = Vec::new();
        for id in graph.ids() {
            let pkg = &graph.packages[id.index()];
            if matches!(
                pkg.resolution,
                Resolution::Root | Resolution::Workspace { .. }
            ) || !pkg.meta.needs_scripts()
                || self.is_trusted(graph, id)
            {
                continue;
            }
            if let Some(scripts) = scripts_of(id).filter(|s| !s.is_empty()) {
                out.push(Blocked {
                    package: id,
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    scripts,
                });
            }
        }
        out
    }
}

/// The name `trustedDependencies` knows a package by (bun `Resolution::trusted_name`): its
/// own name for npm resolutions, else the alias it is declared under.
pub fn trusted_name<'a>(alias: &'a str, package_name: &'a str, resolution: &Resolution) -> &'a str {
    if matches!(resolution, Resolution::Npm { .. }) {
        package_name
    } else {
        alias
    }
}

/// bun `Lockfile::can_trust_by_name`: whether listing `alias` in `trustedDependencies` can
/// trust the package `id` (always for npm; otherwise only when the root or a workspace
/// declares the exact resolution under that alias).
pub fn can_trust_by_name(graph: &LockGraph, alias: &str, id: PackageId) -> bool {
    let Some(pkg) = graph.get(id) else {
        return false;
    };
    matches!(pkg.resolution, Resolution::Npm { .. })
        || declared_aliases(graph, id).any(|a| a == alias)
}

/// The names under which the root or a workspace depends directly on `id`.
fn declared_aliases(graph: &LockGraph, id: PackageId) -> impl Iterator<Item = &str> {
    graph
        .packages
        .iter()
        .filter(|p| {
            matches!(
                p.resolution,
                Resolution::Root | Resolution::Workspace { .. }
            )
        })
        .flat_map(|p| p.dependencies.iter())
        .filter(move |edge| edge.target == Some(id))
        .map(|edge| edge.name.as_str())
}

/// bun `Lockfile::is_local_declarer`: the root, a workspace, or a `file:` folder that one of
/// them depends on or that an override selects.
fn is_local_declarer(graph: &LockGraph, id: PackageId) -> bool {
    let Some(pkg) = graph.get(id) else {
        return false;
    };
    match pkg.resolution {
        Resolution::Root | Resolution::Workspace { .. } => true,
        Resolution::Folder { .. } => {
            declared_aliases(graph, id).next().is_some() || override_selects(graph, &pkg.name)
        }
        _ => false,
    }
}

/// Whether an override supplies a `file:` / path spec for `name` (only the root package.json
/// can declare overrides, so such a path is user authored).
fn override_selects(graph: &LockGraph, name: &str) -> bool {
    graph.overrides.iter().any(|rule| {
        rule.selector.target.name == name
            && matches!(
                fnpm_types::infer_tag(&rule.value),
                fnpm_types::SpecTag::Folder
            )
    })
}

/// bun `Lockfile::is_trusted_folder_package`: some dependency edge resolving to the folder
/// `id` is declared by a local package or named by an override. A `file:` folder that a
/// registry package ships and points into itself is not.
pub fn is_trusted_folder_package(graph: &LockGraph, id: PackageId) -> bool {
    graph.ids().any(|parent| {
        graph.packages[parent.index()]
            .dependencies
            .iter()
            .any(|edge| {
                edge.target == Some(id)
                    && (is_local_declarer(graph, parent)
                        || graph
                            .overrides
                            .iter()
                            .any(|r| r.selector.target.name == edge.name))
            })
    })
}

/// bun `find_trusted_dependencies_from_update_requests` + `add_package_to_set`: the packages
/// `--trust <names>` trusts for this install. Each root dependency whose name is in `requested`
/// is trusted, with every package below it that a name can trust (see [`can_trust_by_name`]).
pub fn trusted_from_requests(graph: &LockGraph, requested: &[&str]) -> BTreeSet<PackageId> {
    let mut set = BTreeSet::new();
    if requested.is_empty() || graph.is_empty() {
        return set;
    }
    let mut visited = vec![false; graph.len()];
    for edge in &graph.root().dependencies {
        let Some(target) = edge.target else { continue };
        if !requested.contains(&edge.name.as_str())
            || !set.insert(target)
            || visited[target.index()]
        {
            continue;
        }
        visited[target.index()] = true;
        let mut stack = vec![target];
        while let Some(current) = stack.pop() {
            for dep in &graph.packages[current.index()].dependencies {
                let Some(dep_id) = dep.target else { continue };
                if dep_id.index() >= graph.len() {
                    continue;
                }
                if !std::mem::replace(&mut visited[dep_id.index()], true) {
                    stack.push(dep_id);
                }
                if can_trust_by_name(graph, &dep.name, dep_id) {
                    set.insert(dep_id);
                }
            }
        }
    }
    set
}
