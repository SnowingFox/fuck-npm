//! [`LockGraph`]: the one in-memory model of a resolved install.
//!
//! Every lockfile format converts to and from it (`fnpm-lockfile`), the resolver produces it
//! (`fnpm-resolver`) and the linker consumes it (`fnpm-linker`). It is a graph, not a tree:
//! each package appears once per `(name, resolution)` and dependency edges point at package
//! ids. Hoisted and isolated layouts are derived from it deterministically by the linker.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::behavior::{Behavior, PeerMeta};
use crate::bin::Bin;
use crate::meta::PackageMeta;
use crate::overrides::OverrideRule;
use crate::platform::{Platform, PlatformConstraints};
use crate::resolution::Resolution;
use crate::spec::{Dependency, DependencySpec, SpecError};

/// Index into [`LockGraph::packages`]. The root is always `PackageId::ROOT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageId(pub u32);

impl PackageId {
    pub const ROOT: PackageId = PackageId(0);

    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// bun's `ConfigVersion`: decides defaults that changed over time (the linker for new
/// workspaces). New lockfiles get [`ConfigVersion::CURRENT`]; migrated npm/yarn lockfiles V0.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(into = "u8", try_from = "u8")]
pub enum ConfigVersion {
    V0,
    #[default]
    V1,
}

impl ConfigVersion {
    pub const CURRENT: Self = Self::V1;
}

impl From<ConfigVersion> for u8 {
    fn from(v: ConfigVersion) -> u8 {
        match v {
            ConfigVersion::V0 => 0,
            ConfigVersion::V1 => 1,
        }
    }
}

impl TryFrom<u8> for ConfigVersion {
    type Error = String;
    /// Values above the current version clamp to it, like bun.
    fn try_from(n: u8) -> Result<Self, String> {
        Ok(if n == 0 { Self::V0 } else { Self::V1 })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeLinker {
    /// npm-style flat `node_modules`.
    Hoisted,
    /// pnpm-style `node_modules/.fnpm/<key>/node_modules/<name>` with symlinks.
    Isolated,
}

/// A dependency edge of a package.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DepEdge {
    /// The key in package.json; the folder name under `node_modules` (an alias for `npm:`).
    pub name: String,
    /// The specifier as declared (after overrides and catalogs are applied it is still the
    /// declared text; the resolver records the effective spec only through `target`).
    pub literal: String,
    pub behavior: Behavior,
    /// `None`: unresolved. That is an optional dependency that failed or does not match the
    /// platform, a peer that nobody provides, or a dependency filtered out by `--omit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PackageId>,
}

impl DepEdge {
    pub fn new(name: impl Into<String>, literal: impl Into<String>, behavior: Behavior) -> Self {
        Self {
            name: name.into(),
            literal: literal.into(),
            behavior,
            target: None,
        }
    }

    pub fn spec(&self) -> Result<DependencySpec, SpecError> {
        DependencySpec::parse(&self.name, &self.literal)
    }

    pub fn to_dependency(&self) -> Result<Dependency, SpecError> {
        Dependency::parse(&self.name, &self.literal, self.behavior)
    }
}

/// A resolved package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    /// The package's own name (from its package.json or the registry). For unnamed local
    /// packages, [`crate::name::fallback_package_name`].
    pub name: String,
    /// The package.json `version`. Equal to the npm resolution version for registry packages;
    /// may be empty for local packages.
    #[serde(default)]
    pub version: String,
    pub resolution: Resolution,
    /// Sorted by `name`, then by group (prod, dev, optional, peer). Root and workspace
    /// packages carry their dev dependencies; registry packages never do.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<DepEdge>,
    /// `peerDependenciesMeta`. Optional peers also have `Behavior::OPTIONAL` on their edge.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_meta: BTreeMap<String, PeerMeta>,
    #[serde(default, skip_serializing_if = "Bin::is_none")]
    pub bin: Bin,
    #[serde(default, skip_serializing_if = "PlatformConstraints::is_any")]
    pub platform: PlatformConstraints,
    #[serde(default)]
    pub meta: PackageMeta,
}

impl Package {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        resolution: Resolution,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            resolution,
            dependencies: Vec::new(),
            peer_meta: BTreeMap::new(),
            bin: Bin::None,
            platform: PlatformConstraints::ANY,
            meta: PackageMeta::default(),
        }
    }

    pub fn dependency(&self, name: &str) -> Option<&DepEdge> {
        self.dependencies.iter().find(|e| e.name == name)
    }

    /// Does not match the target platform, so it is neither fetched nor linked but stays in the
    /// lockfile.
    pub fn is_disabled(&self, platform: &Platform) -> bool {
        !self.platform.matches(platform)
    }

    /// The key of this package in patchedDependencies.
    pub fn patch_key(&self) -> String {
        let label = match &self.resolution {
            Resolution::Workspace { .. } => self.version.clone(),
            resolution => resolution.patch_label(),
        };
        format!("{}@{label}", self.name)
    }

    /// `name@resolution`, the key bun.lock and the isolated store use.
    pub fn key(&self) -> String {
        format!("{}@{}", self.name, self.resolution)
    }
}

/// `catalog` and `catalogs` from the root package.json (or `pnpm-workspace.yaml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalogs {
    /// `catalog:` (the default catalog).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub default: BTreeMap<String, String>,
    /// `catalog:<name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub named: BTreeMap<String, BTreeMap<String, String>>,
}

impl Catalogs {
    /// The specifier `catalog:<catalog>` stands for, for dependency `name`.
    pub fn get(&self, catalog: &str, name: &str) -> Option<&str> {
        let map = if catalog.is_empty() || catalog == "default" {
            &self.default
        } else {
            self.named.get(catalog)?
        };
        map.get(name).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.default.is_empty() && self.named.is_empty()
    }
}

/// `patchedDependencies["name@version"]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PatchedDependency {
    /// Path of the `.patch` file relative to the project root.
    pub path: String,
    /// Hash of the patch file contents (hex), part of the store key of the patched package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// `packageExtensions["name@range"]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageExtension {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    #[error("the lock graph has no root package")]
    MissingRoot,
    #[error("package 0 must have a root resolution, found {0}")]
    RootNotFirst(String),
    #[error("{package}: dependency {dependency} points at missing package id {target}")]
    DanglingEdge {
        package: String,
        dependency: String,
        target: u32,
    },
    #[error("workspace {path:?} points at package id {target}, which is not a workspace")]
    BadWorkspace { path: String, target: u32 },
    #[error("duplicate package {0}")]
    Duplicate(String),
}

/// The resolved install. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockGraph {
    /// fnpm.lock format version.
    pub lockfile_version: u32,
    pub config_version: ConfigVersion,
    /// A linker recorded by the lockfile (`fnpm install --linker` with `--save-linker`) or by
    /// migration. `None` lets the config and auto-selection decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_linker: Option<NodeLinker>,
    /// The arena. `packages[0]` is the root.
    pub packages: Vec<Package>,
    /// Workspace path (relative, `/`-separated; `""` is the root) to package id.
    pub workspaces: BTreeMap<String, PackageId>,
    #[serde(default, skip_serializing_if = "Catalogs::is_empty")]
    pub catalogs: Catalogs,
    /// In declaration order; the resolver applies precedence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<OverrideRule>,
    /// Keyed by `name@version`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub patched_dependencies: BTreeMap<String, PatchedDependency>,
    /// `None`: no `trustedDependencies` field, so the built-in default list applies.
    /// `Some(empty)`: explicitly no packages are trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_dependencies: Option<BTreeSet<String>>,
    /// Keyed by `name` or `name@range`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_extensions: BTreeMap<String, PackageExtension>,
}

impl LockGraph {
    pub const LOCKFILE_VERSION: u32 = 1;

    /// A graph containing only `root` (whose resolution is forced to [`Resolution::Root`]).
    pub fn new(mut root: Package) -> Self {
        root.resolution = Resolution::Root;
        let mut workspaces = BTreeMap::new();
        workspaces.insert(String::new(), PackageId::ROOT);
        Self {
            lockfile_version: Self::LOCKFILE_VERSION,
            config_version: ConfigVersion::CURRENT,
            node_linker: None,
            packages: vec![root],
            workspaces,
            catalogs: Catalogs::default(),
            overrides: Vec::new(),
            patched_dependencies: BTreeMap::new(),
            trusted_dependencies: None,
            package_extensions: BTreeMap::new(),
        }
    }

    #[inline]
    pub fn root(&self) -> &Package {
        &self.packages[0]
    }

    #[inline]
    pub fn root_mut(&mut self) -> &mut Package {
        &mut self.packages[0]
    }

    #[inline]
    pub fn get(&self, id: PackageId) -> Option<&Package> {
        self.packages.get(id.index())
    }

    #[inline]
    pub fn get_mut(&mut self, id: PackageId) -> Option<&mut Package> {
        self.packages.get_mut(id.index())
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.packages.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    pub fn ids(&self) -> impl Iterator<Item = PackageId> + '_ {
        (0..self.packages.len() as u32).map(PackageId)
    }

    /// Appends without dedupe. Use [`LockGraph::intern`] to reuse an equal package.
    pub fn push(&mut self, package: Package) -> PackageId {
        let id = PackageId(self.packages.len() as u32);
        self.packages.push(package);
        id
    }

    /// Adds a workspace member (resolution must be [`Resolution::Workspace`]).
    pub fn add_workspace(&mut self, package: Package) -> PackageId {
        let path = match &package.resolution {
            Resolution::Workspace { path } => path.clone(),
            _ => panic!("add_workspace needs a workspace resolution"),
        };
        let id = self.push(package);
        self.workspaces.insert(path, id);
        id
    }

    /// Linear lookup by `(name, resolution)`. Build a [`GraphIndex`] for repeated lookups.
    pub fn find(&self, name: &str, resolution: &Resolution) -> Option<PackageId> {
        self.packages
            .iter()
            .position(|p| p.name == name && &p.resolution == resolution)
            .map(|i| PackageId(i as u32))
    }

    /// All packages with this name.
    pub fn by_name<'a>(&'a self, name: &'a str) -> impl Iterator<Item = PackageId> + 'a {
        self.packages
            .iter()
            .enumerate()
            .filter(move |(_, p)| p.name == name)
            .map(|(i, _)| PackageId(i as u32))
    }

    pub fn index(&self) -> GraphIndex {
        let mut by_key = HashMap::with_capacity(self.packages.len());
        for (i, p) in self.packages.iter().enumerate() {
            by_key
                .entry((p.name.clone(), p.resolution.to_string()))
                .or_insert(PackageId(i as u32));
        }
        GraphIndex { by_key }
    }

    /// `(edge, target)` pairs of resolved edges.
    pub fn resolved_edges(
        &self,
        id: PackageId,
    ) -> impl Iterator<Item = (&DepEdge, PackageId)> + '_ {
        self.packages[id.index()]
            .dependencies
            .iter()
            .filter_map(|e| e.target.map(|t| (e, t)))
    }

    pub fn has_workspaces(&self) -> bool {
        self.workspaces.len() > 1
    }

    pub fn is_trusted(&self, name: &str, default_list: &dyn Fn(&str) -> bool) -> bool {
        match &self.trusted_dependencies {
            Some(set) => set.contains(name),
            None => default_list(name),
        }
    }

    /// Packages reachable from the root and workspaces through edges whose behavior intersects
    /// `include` (for example `PROD | OPTIONAL | PEER` for `--production`). Dev edges are only
    /// followed from root/workspace packages, as with npm. Workspaces are roots unless
    /// `workspace_roots` restricts them (`--filter`).
    pub fn reachable(&self, include: Behavior, workspace_roots: Option<&[PackageId]>) -> Vec<bool> {
        let mut seen = vec![false; self.packages.len()];
        let mut stack: Vec<PackageId> = match workspace_roots {
            Some(r) => r.to_vec(),
            None => self.workspaces.values().copied().collect(),
        };
        while let Some(id) = stack.pop() {
            let Some(slot) = seen.get_mut(id.index()) else {
                continue;
            };
            if *slot {
                continue;
            }
            *slot = true;
            let local = self.packages[id.index()].resolution.is_local_tree();
            for e in &self.packages[id.index()].dependencies {
                let Some(t) = e.target else { continue };
                let b = if local {
                    e.behavior
                } else {
                    e.behavior.with(Behavior::DEV, false)
                };
                let follows = b.intersects(include)
                    || (b.is_workspace() && include.intersects(Behavior::PROD));
                if follows && !seen.get(t.index()).copied().unwrap_or(true) {
                    stack.push(t);
                }
            }
        }
        seen
    }

    /// Sorts every package's edges into the canonical order.
    pub fn sort_dependencies(&mut self) {
        for p in &mut self.packages {
            p.dependencies.sort_by(|a, b| {
                a.name
                    .cmp(&b.name)
                    .then(a.behavior.group().cmp(&b.behavior.group()))
            });
        }
    }

    /// Checks the structural invariants the linker and serializers rely on.
    pub fn validate(&self) -> Result<(), GraphError> {
        let root = self.packages.first().ok_or(GraphError::MissingRoot)?;
        if root.resolution != Resolution::Root {
            return Err(GraphError::RootNotFirst(root.resolution.to_string()));
        }
        let n = self.packages.len() as u32;
        for p in &self.packages {
            for e in &p.dependencies {
                if let Some(t) = e.target
                    && t.0 >= n
                {
                    return Err(GraphError::DanglingEdge {
                        package: p.key(),
                        dependency: e.name.clone(),
                        target: t.0,
                    });
                }
            }
        }
        for (path, id) in &self.workspaces {
            let ok = match self.get(*id).map(|p| &p.resolution) {
                Some(Resolution::Root) => path.is_empty(),
                Some(Resolution::Workspace { path: p }) => p == path,
                _ => false,
            };
            if !ok {
                return Err(GraphError::BadWorkspace {
                    path: path.clone(),
                    target: id.0,
                });
            }
        }
        let mut keys = BTreeSet::new();
        for p in &self.packages {
            if !keys.insert((p.name.as_str(), p.resolution.to_string())) {
                return Err(GraphError::Duplicate(p.key()));
            }
        }
        Ok(())
    }
}

/// Hash lookup of `(name, resolution)` built by [`LockGraph::index`].
#[derive(Debug, Clone, Default)]
pub struct GraphIndex {
    by_key: HashMap<(String, String), PackageId>,
}

impl GraphIndex {
    pub fn get(&self, name: &str, resolution: &Resolution) -> Option<PackageId> {
        self.by_key
            .get(&(name.to_owned(), resolution.to_string()))
            .copied()
    }

    pub fn insert(&mut self, name: &str, resolution: &Resolution, id: PackageId) {
        self.by_key
            .insert((name.to_owned(), resolution.to_string()), id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::{HashAlgorithm, Integrity};

    fn npm(name: &str, v: &str) -> Package {
        let i = Integrity::compute(HashAlgorithm::Sha512, format!("{name}{v}").as_bytes());
        Package::new(name, v, Resolution::npm(v, Some(i)))
    }

    fn sample() -> LockGraph {
        let mut g = LockGraph::new(Package::new("app", "1.0.0", Resolution::Root));
        let ws = g.add_workspace(Package::new(
            "lib",
            "0.1.0",
            Resolution::Workspace {
                path: "packages/lib".into(),
            },
        ));
        let a = g.push(npm("a", "1.0.0"));
        let b = g.push(npm("b", "2.0.0"));
        let d = g.push(npm("devonly", "3.0.0"));
        let mut e = DepEdge::new("a", "^1.0.0", Behavior::PROD);
        e.target = Some(a);
        let mut dv = DepEdge::new("devonly", "^3", Behavior::DEV);
        dv.target = Some(d);
        let mut w = DepEdge::new("lib", "workspace:*", Behavior::PROD | Behavior::WORKSPACE);
        w.target = Some(ws);
        g.root_mut().dependencies = vec![e, dv, w];
        let mut ab = DepEdge::new("b", "^2", Behavior::PROD);
        ab.target = Some(b);
        g.packages[a.index()].dependencies.push(ab);
        let mut bd = DepEdge::new("devonly", "^3", Behavior::DEV);
        bd.target = Some(d);
        g.packages[b.index()].dependencies.push(bd);
        g
    }

    #[test]
    fn build_and_validate() {
        let g = sample();
        g.validate().unwrap();
        assert!(g.has_workspaces());
        assert_eq!(g.find("b", &g.packages[3].resolution), Some(PackageId(3)));
        let idx = g.index();
        assert_eq!(idx.get("a", &g.packages[2].resolution), Some(PackageId(2)));
        assert_eq!(g.root().key(), "app@root:");
        assert_eq!(g.resolved_edges(PackageId::ROOT).count(), 3);
    }

    #[test]
    fn reachability() {
        let g = sample();
        let prod = g.reachable(Behavior::PROD | Behavior::OPTIONAL | Behavior::PEER, None);
        assert_eq!(prod, vec![true, true, true, true, false]);
        let all = g.reachable(
            Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER,
            None,
        );
        assert_eq!(all, vec![true; 5]);
    }

    #[test]
    fn validation_errors() {
        let mut g = sample();
        g.packages[2].dependencies[0].target = Some(PackageId(99));
        assert!(matches!(g.validate(), Err(GraphError::DanglingEdge { .. })));
        let mut g = sample();
        g.push(npm("a", "1.0.0"));
        assert!(matches!(g.validate(), Err(GraphError::Duplicate(_))));
        let mut g = sample();
        g.workspaces.insert("nope".into(), PackageId(2));
        assert!(matches!(g.validate(), Err(GraphError::BadWorkspace { .. })));
    }

    #[test]
    fn serde_roundtrip() {
        let mut g = sample();
        g.catalogs.default.insert("react".into(), "^18".into());
        g.trusted_dependencies = Some(["esbuild".to_owned()].into());
        g.overrides.push(OverrideRule {
            selector: crate::overrides::OverrideSelector::parse("a>b")
                .unwrap()
                .unwrap(),
            value: "2.0.0".into(),
        });
        let j = serde_json::to_string(&g).unwrap();
        let back: LockGraph = serde_json::from_str(&j).unwrap();
        assert_eq!(back, g);
        assert_eq!(g.catalogs.get("", "react"), Some("^18"));
        assert!(g.is_trusted("esbuild", &|_| false));
        assert!(!g.is_trusted("x", &|_| true));
    }

    #[test]
    fn sorting_and_config_version() {
        let mut g = sample();
        g.root_mut().dependencies.reverse();
        g.sort_dependencies();
        let names: Vec<_> = g
            .root()
            .dependencies
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, ["a", "devonly", "lib"]);
        assert_eq!(ConfigVersion::try_from(7u8).unwrap(), ConfigVersion::V1);
        assert_eq!(serde_json::to_string(&ConfigVersion::V0).unwrap(), "0");
    }
}
