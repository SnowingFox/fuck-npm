//! The hoisted `node_modules` tree (port of bun `lockfile/Tree.rs`).
//!
//! fnpm.lock and bun.lock key packages by their hoisted install path, so serialisation needs
//! the tree; the hoisted linker materialises the same tree. One algorithm, one place.
//!
//! The root's links to workspace members are implicit in a [`LockGraph`]: every entry of
//! `graph.workspaces` gets a row named after the member in the root `node_modules`, as in
//! bun. Packages placed under a workspace's row (`<workspace name>/<dep>`) are installed in
//! that workspace's own `node_modules`; [`Tree::workspace_roots`] lists them.

use std::collections::VecDeque;

use crate::fxhash::{FxHashMap as HashMap, map};

use fnpm_types::{Behavior, LockGraph, PackageId, Platform, Resolution};

use crate::flat::{Flat, INVALID, VTag};

/// A directory `node_modules/<...>/node_modules/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// The package installed in this directory.
    pub package: PackageId,
    /// The folder name (the edge alias).
    pub name: String,
    /// `None` for the top level (`node_modules/<name>`).
    pub parent: Option<u32>,
    /// `a/b` for `node_modules/a/node_modules/b`; the key used in fnpm.lock.
    pub path: String,
}

/// Every installed directory. Node order: by depth, then path; ids are indices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// The directories.
    pub nodes: Vec<TreeNode>,
    /// Workspaces get their own `node_modules` for deps that conflict with the root's.
    /// `(workspace package id, node ids directly under it)`.
    pub workspace_roots: Vec<(PackageId, Vec<u32>)>,
}

/// What an install places.
#[derive(Debug, Clone)]
pub struct HoistOptions {
    /// Which edges are installed (`--omit`, `--production`).
    pub include: Behavior,
    /// Disabled packages (platform mismatch) are not placed.
    pub platform: Platform,
    /// Limit to these workspaces (`--filter`); `None` is all.
    pub workspaces: Option<Vec<PackageId>>,
}

impl HoistOptions {
    /// Every dependency kind, every workspace, for `platform`.
    pub fn all(platform: Platform) -> Self {
        Self {
            include: Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER,
            platform,
            workspaces: None,
        }
    }
}

/// A dependency cycle that would nest forever (`a > b > a > b ...`, each needing its own
/// copy of the next). bun aborts the install with this message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot install dependency cycle into node_modules: {cycle}")]
pub struct HoistError {
    /// `a@1.0.0 > b@2.0.0 > a@1.0.0`.
    pub cycle: String,
    /// The tree built without the cycle's last nesting.
    pub partial: Tree,
}

/// npm-style hoisting with peer handling: a dependency goes as high as possible without
/// conflicting with an existing name or shadowing a peer's resolution for a dependent.
///
/// The placement is computed over every dependency (bun's saved tree), then reduced to what
/// `opts` installs, so where a package lands does not depend on the install's options.
/// Bundled dependencies ship inside their parent's tarball and are not part of the result.
pub fn hoist(graph: &LockGraph, opts: &HoistOptions) -> Tree {
    try_hoist(graph, opts).unwrap_or_else(|e| e.partial)
}

/// [`hoist`] that reports an endless nesting cycle instead of breaking it silently.
pub fn try_hoist(graph: &LockGraph, opts: &HoistOptions) -> Result<Tree, HoistError> {
    let mut flat = Flat::new(graph);
    let hoisted = resolve(&mut flat);
    let tree = install_tree(&flat, &hoisted, opts);
    match hoisted.cycle {
        Some(cycle) => Err(HoistError {
            cycle,
            partial: tree,
        }),
        None => Ok(tree),
    }
}

/// Reverse of [`hoist`]: resolve each edge of the package at `path` by node's lookup (own
/// `node_modules`, then each ancestor's) over a set of install paths. Used when reading
/// fnpm.lock, bun.lock and package-lock.json.
///
/// `paths` must be sorted by path (`sort_by(|a, b| a.0.cmp(&b.0))`); `from` is the path of
/// the package whose dependency `name` is looked up (`""` for the root). Scoped segments
/// (`@scope/name`) count as one level, like bun's `PkgMap::find_resolution`.
pub fn resolve_paths(paths: &[(String, PackageId)], from: &str, name: &str) -> Option<PackageId> {
    let find = |p: &str| {
        paths
            .binary_search_by(|(k, _)| k.as_str().cmp(p))
            .ok()
            .map(|i| paths[i].1)
    };
    let mut base = from;
    let mut buf = String::with_capacity(from.len() + name.len() + 1);
    loop {
        buf.clear();
        buf.push_str(base);
        if !base.is_empty() {
            buf.push('/');
        }
        buf.push_str(name);
        if let Some(id) = find(&buf) {
            return Some(id);
        }
        if base.is_empty() {
            return None;
        }
        base = parent_path(base)?;
    }
}

/// The path whose `node_modules` is searched after `path`'s (bun `PkgMap::find_resolution`):
/// `a/b` gives `a`, `a` gives `""`, `x/@s/b` gives `x`. `None` for a malformed path.
pub(crate) fn parent_path(path: &str) -> Option<&str> {
    let Some(slash) = path.rfind('/') else {
        return Some("");
    };
    let Some(at) = path.rfind('@') else {
        return Some(&path[..slash]);
    };
    if at > slash {
        return None;
    }
    match path[..slash].rfind('/') {
        None => (at == 0).then_some(""),
        Some(ns) if ns > at => Some(&path[..slash]),
        Some(ns) if ns + 1 != at => None,
        Some(ns) => Some(&path[..ns]),
    }
}

// ─── the builder ────────────────────────────────────────────────────────────────────────────

const ROOT_DEP_ID: u32 = u32::MAX - 1;
const INVALID_ID: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub(crate) struct BTree {
    pub id: u32,
    pub dependency_id: u32,
    pub parent: u32,
}

#[derive(Clone, Copy)]
struct FillItem {
    tree_id: u32,
    dependency_id: u32,
    hoist_root_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoistResult {
    DependencyLoop,
    Hoisted,
    Resolve(u32),
    ResolveReplace { id: u32, dep_id: u32 },
    ResolveLater,
    Rebind(u32),
    Served(u32),
    Placement { id: u32, bundled: bool },
}

#[derive(Clone, Copy)]
enum OwnerCopy {
    None,
    FirstAbove,
    Package(u32),
}

/// The result of hoisting: each `node_modules` folder and the rows (dependency ids) in it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Hoisted {
    pub trees: Vec<BTree>,
    pub rows: Vec<Vec<u32>>,
    pub late_bound_peer: bool,
    pub cycle: Option<String>,
}

struct Builder<'a, 'g> {
    flat: &'a Flat<'g>,
    resolutions: &'a mut Vec<u32>,
    trees: Vec<BTree>,
    rows: Vec<Vec<u32>>,
    queue: VecDeque<FillItem>,
    pending_peers: HashMap<u32, Vec<u32>>,
    late_bound_peer: bool,
    bundle_placed: bool,
    walked: Vec<bool>,
    served: Vec<(u32, u32)>,
    sort_buf: Vec<u32>,
    probes: Vec<(u32, bool, OwnerCopy)>,
    with_peers: Option<Vec<bool>>,
    regular_targets: Option<Vec<bool>>,
    placed: HashMap<(u32, u32), u32>,
    cycle: Option<String>,
}

/// Hoists until no pass binds or rebinds a peer, then binds the peers whose target is not
/// printed anywhere (bun `Lockfile::resolve`).
pub(crate) fn resolve(flat: &mut Flat<'_>) -> Hoisted {
    let mut resolutions = std::mem::take(&mut flat.resolutions);
    let mut last;
    let mut passes = 0;
    loop {
        last = hoist_pass(flat, &mut resolutions);
        passes += 1;
        // bun loops until stable; a cap guards against oscillation on malformed input.
        if !last.late_bound_peer || passes >= 16 {
            break;
        }
    }
    rebind_peers_to_printed_packages(flat, &mut resolutions, &last);
    flat.resolutions = resolutions;
    last
}

fn hoist_pass(flat: &Flat<'_>, resolutions: &mut Vec<u32>) -> Hoisted {
    let n = flat.graph.packages.len();
    let mut b = Builder {
        flat,
        resolutions,
        trees: Vec::new(),
        rows: Vec::new(),
        queue: VecDeque::new(),
        pending_peers: map(0),
        late_bound_peer: false,
        bundle_placed: false,
        walked: vec![false; n],
        served: Vec::new(),
        sort_buf: Vec::new(),
        probes: Vec::new(),
        with_peers: None,
        regular_targets: None,
        placed: map(n * 2),
        cycle: None,
    };
    let root = BTree {
        id: INVALID_ID,
        dependency_id: u32::MAX,
        parent: INVALID_ID,
    };
    b.process_subtree(root, ROOT_DEP_ID, INVALID_ID);
    while let Some(item) = b.queue.pop_front() {
        let t = b.trees[item.tree_id as usize];
        b.process_subtree(t, item.dependency_id, item.hoist_root_id);
    }
    b.bind_served_peers();
    // `clean`: rows of peers that never resolved are dropped.
    let res = &*b.resolutions;
    let rows: Vec<Vec<u32>> = b
        .rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .filter(|&d| res[d as usize] != INVALID)
                .collect()
        })
        .collect();
    Hoisted {
        trees: b.trees,
        rows,
        late_bound_peer: b.late_bound_peer,
        cycle: b.cycle,
    }
}

/// bun `rebind_peers_to_printed_packages`.
fn rebind_peers_to_printed_packages(flat: &Flat<'_>, resolutions: &mut [u32], h: &Hoisted) {
    let n = flat.graph.packages.len();
    let mut printed = vec![false; n];
    for (i, p) in flat.graph.packages.iter().enumerate() {
        if matches!(
            p.resolution,
            Resolution::Root | Resolution::Workspace { .. }
        ) {
            printed[i] = true;
        }
    }
    for rows in &h.rows {
        for &d in rows {
            let t = resolutions[d as usize];
            if (t as usize) < n {
                printed[t as usize] = true;
            }
        }
    }
    for (dep_id, dep) in flat.deps.iter().enumerate() {
        let t = resolutions[dep_id];
        if t as usize >= n || printed[t as usize] || !dep.behavior.is_peer() {
            continue;
        }
        if let Some(p) = flat.peer_by_version(dep_id as u32, |id| printed[id as usize]) {
            resolutions[dep_id] = p;
        }
    }
}

#[inline]
fn contains(range: (u32, u32), id: u32) -> bool {
    range.0 <= id && id < range.1
}

impl<'a, 'g> Builder<'a, 'g> {
    fn dep(&self, id: u32) -> &'a crate::flat::FDep<'g> {
        &self.flat.deps[id as usize]
    }

    fn bind_pending_peers(&mut self, name: u32, pkg: u32) {
        if let Some(peers) = self.pending_peers.remove(&name) {
            for d in peers {
                self.resolutions[d as usize] = pkg;
            }
        }
    }

    fn bind_peer(&mut self, dep_id: u32) -> bool {
        if !self.dep(dep_id).behavior.is_peer() {
            return false;
        }
        match self.flat.peer_by_version(dep_id, |_| true) {
            Some(p) => {
                self.resolutions[dep_id as usize] = p;
                true
            }
            None => false,
        }
    }

    fn pkg_tag_is(&self, pkg: u32, f: impl Fn(&Resolution) -> bool) -> bool {
        (pkg as usize) < self.flat.graph.packages.len() && f(self.flat.res(pkg))
    }

    fn bind_served_peers(&mut self) {
        if self.served.is_empty() {
            return;
        }
        let mut placed: Vec<u32> = self
            .rows
            .iter()
            .flatten()
            .copied()
            .filter(|&r| self.dep(r).behavior.is_peer())
            .collect();
        placed.sort_unstable();
        let served = std::mem::take(&mut self.served);
        for (dep_id, pkg) in served {
            if placed.binary_search(&dep_id).is_err() {
                self.resolutions[dep_id as usize] = pkg;
            }
        }
    }

    /// bun `repeating_period`.
    fn repeating_period(&self, parent: u32, dependency_id: u32) -> Option<usize> {
        let mut period = 1usize;
        let mut round_start = parent;
        while round_start != INVALID_ID {
            let above = self.trees[round_start as usize].parent;
            if self.trees[round_start as usize].dependency_id == dependency_id {
                let (mut lower, mut upper) = (parent, above);
                let mut same = 0usize;
                while same < period
                    && upper != INVALID_ID
                    && self.trees[lower as usize].dependency_id
                        == self.trees[upper as usize].dependency_id
                    && self.rows[upper as usize].starts_with(&self.rows[lower as usize])
                {
                    lower = self.trees[lower as usize].parent;
                    upper = self.trees[upper as usize].parent;
                    same += 1;
                }
                if same == period {
                    return Some(period);
                }
            }
            round_start = above;
            period += 1;
        }
        None
    }

    fn describe_cycle(&self, parent: u32, dependency_id: u32, period: usize) -> String {
        let mut round = vec![dependency_id];
        let mut t = parent;
        for _ in 0..period {
            round.push(self.trees[t as usize].dependency_id);
            t = self.trees[t as usize].parent;
        }
        let mut out = String::new();
        for &d in round.iter().rev() {
            let p = self.resolutions.get(d as usize).copied().unwrap_or(INVALID);
            if !out.is_empty() {
                out.push_str(" > ");
            }
            match self.flat.graph.packages.get(p as usize) {
                Some(pkg) => out.push_str(&pkg.key()),
                None => out.push_str(self.dep(d).name),
            }
        }
        out
    }

    fn process_subtree(&mut self, this: BTree, dependency_id: u32, hoist_root_id: u32) {
        let parent_pkg = if dependency_id == ROOT_DEP_ID {
            0
        } else {
            self.resolutions[dependency_id as usize]
        };
        if parent_pkg as usize >= self.flat.lists.len() {
            return;
        }
        let list = self.flat.lists[parent_pkg as usize];
        if list.0 == list.1 {
            return;
        }

        // A dependency that links back to a workspace from inside that workspace's own tree.
        if dependency_id != ROOT_DEP_ID
            && !self.dep(dependency_id).behavior.is_workspace()
            && matches!(self.flat.res(parent_pkg), Resolution::Workspace { .. })
        {
            let mut t = this.id;
            while t != INVALID_ID {
                let tree = self.trees[t as usize];
                if self.resolutions.get(tree.dependency_id as usize) == Some(&parent_pkg) {
                    return;
                }
                t = tree.parent;
            }
        }

        if this.id != INVALID_ID
            && let Some(period) = self.repeating_period(this.id, dependency_id)
        {
            if self.cycle.is_none() {
                self.cycle = Some(self.describe_cycle(this.id, dependency_id, period));
            }
            return;
        }

        let next_id = self.trees.len() as u32;
        self.trees.push(BTree {
            id: next_id,
            dependency_id,
            parent: this.id,
        });
        self.rows.push(Vec::new());

        let first_walk = !self.walked[parent_pkg as usize];
        self.walked[parent_pkg as usize] = true;

        let mut sort_buf = std::mem::take(&mut self.sort_buf);
        sort_buf.clear();
        sort_buf.extend(list.0..list.1);
        let deps = &self.flat.deps;
        sort_buf.sort_unstable_by(|&a, &b| {
            crate::flat::dependency_cmp(&deps[a as usize], &deps[b as usize])
        });

        for &dep_id in &sort_buf {
            let bound_by_version = self.bind_peer(dep_id);
            let pkg_id = self.resolutions[dep_id as usize];
            let dependency = self.dep(dep_id);

            let hoisted = 'hoisted: {
                if dependency.behavior.is_bundled() {
                    self.bundle_placed = true;
                    break 'hoisted HoistResult::Placement {
                        id: next_id,
                        bundled: true,
                    };
                }
                if pkg_id == INVALID {
                    if !dependency.behavior.is_peer() {
                        break 'hoisted HoistResult::DependencyLoop;
                    }
                } else if self.pkg_tag_is(pkg_id, |r| matches!(r, Resolution::Folder { .. })) {
                    if dependency.behavior.is_peer() {
                        let r = self.hoist_dependency(
                            true,
                            next_id,
                            hoist_root_id,
                            pkg_id,
                            dep_id,
                            list,
                        );
                        if matches!(r, HoistResult::Hoisted | HoistResult::Served(_)) {
                            break 'hoisted r;
                        }
                    }
                    // Folder packages never hoist, so a cycle between them would nest forever.
                    let mut t = next_id;
                    let mut cyclic = false;
                    while t != INVALID_ID {
                        let tree = self.trees[t as usize];
                        let a = tree.dependency_id;
                        if (a as usize) < self.flat.deps.len()
                            && self.resolutions[a as usize] == pkg_id
                            && self.flat.deps[a as usize].name_id == dependency.name_id
                        {
                            cyclic = true;
                            break;
                        }
                        t = tree.parent;
                    }
                    if cyclic {
                        break 'hoisted HoistResult::DependencyLoop;
                    }
                    break 'hoisted self
                        .hoist_dependency(true, next_id, next_id, pkg_id, dep_id, list);
                }
                self.hoist_dependency(true, next_id, hoist_root_id, pkg_id, dep_id, list)
            };

            match hoisted {
                HoistResult::DependencyLoop | HoistResult::Hoisted => {}
                HoistResult::Served(res_id) => {
                    if first_walk && bound_by_version {
                        self.served.push((dep_id, res_id));
                    }
                }
                HoistResult::Resolve(res_id) => {
                    if first_walk || !dependency.behavior.is_optional_peer() {
                        self.resolutions[dep_id as usize] = res_id;
                        if !self.stays_on(first_walk, next_id, hoist_root_id, res_id, dep_id, list)
                        {
                            self.late_bound_peer = true;
                        }
                    }
                }
                HoistResult::ResolveReplace {
                    id,
                    dep_id: replaced,
                } => {
                    if !self.dep(replaced).behavior.is_workspace() {
                        self.late_bound_peer = true;
                        self.resolutions[replaced as usize] = pkg_id;
                        self.bind_pending_peers(dependency.name_id, pkg_id);
                    }
                    for r in &mut self.rows[id as usize] {
                        if *r == replaced {
                            *r = dep_id;
                        }
                    }
                    if let Some(p) = self.placed.get_mut(&(id, dependency.name_id))
                        && *p == replaced
                    {
                        *p = dep_id;
                    }
                    if self.list_len(pkg_id) > 0 {
                        self.queue.push_back(FillItem {
                            tree_id: id,
                            dependency_id: dep_id,
                            hoist_root_id,
                        });
                    }
                }
                HoistResult::Rebind(res_id) => {
                    if first_walk {
                        self.resolutions[dep_id as usize] = res_id;
                        if !self.stays_on(first_walk, next_id, hoist_root_id, res_id, dep_id, list)
                        {
                            self.late_bound_peer = true;
                        }
                    }
                }
                HoistResult::ResolveLater => {
                    let e = self.pending_peers.entry(dependency.name_id).or_default();
                    if !e.contains(&dep_id) {
                        e.push(dep_id);
                    }
                }
                HoistResult::Placement { id, bundled } => {
                    if dependency.behavior.is_peer() && !bundled {
                        let own = (list.0..list.1).find(|&own| {
                            let o = self.dep(own);
                            o.name_id == dependency.name_id
                                && !o.behavior.is_peer()
                                && self.resolutions[own as usize] != INVALID
                        });
                        if let Some(own) = own {
                            let own_pkg = self.resolutions[own as usize];
                            let ob = self.dep(own).behavior;
                            if first_walk
                                && bound_by_version
                                && !ob.is_optional()
                                && !ob.is_bundled()
                                && self.flat.satisfies(own_pkg, &dependency.resolved)
                            {
                                self.served.push((dep_id, own_pkg));
                            }
                            continue;
                        }
                    }
                    if pkg_id != INVALID
                        && !dependency.name.is_empty()
                        && !fnpm_types::name::is_safe_install_folder_name(dependency.name)
                    {
                        continue;
                    }
                    self.rows[id as usize].push(dep_id);
                    self.placed
                        .entry((id, dependency.name_id))
                        .or_insert(dep_id);
                    if pkg_id != INVALID && self.list_len(pkg_id) > 0 {
                        self.queue.push_back(FillItem {
                            tree_id: id,
                            dependency_id: dep_id,
                            hoist_root_id: if bundled { id } else { hoist_root_id },
                        });
                    }
                }
            }
        }
        self.sort_buf = sort_buf;

        if self.rows[next_id as usize].is_empty() {
            debug_assert_eq!(self.trees.len(), next_id as usize + 1);
            self.trees.pop();
            self.rows.pop();
        }
    }

    fn list_len(&self, pkg: u32) -> u32 {
        self.flat.lists.get(pkg as usize).map_or(0, |l| l.1 - l.0)
    }

    fn stays_on(
        &mut self,
        first_walk: bool,
        next_id: u32,
        hoist_root_id: u32,
        res_id: u32,
        dep_id: u32,
        list: (u32, u32),
    ) -> bool {
        first_walk
            && !self.bundle_placed
            && self.hoist_dependency(true, next_id, hoist_root_id, res_id, dep_id, list)
                == HoistResult::Hoisted
    }

    fn is_in_peer_range(&self, dependency: &crate::flat::FDep<'_>, res_id: u32) -> bool {
        dependency.resolved.tag == VTag::Npm
            && matches!(self.flat.res(res_id), Resolution::Npm { .. })
            && self.flat.satisfies(res_id, &dependency.resolved)
    }

    #[allow(clippy::too_many_arguments)]
    fn hoist_dependency(
        &mut self,
        as_defined: bool,
        self_id: u32,
        hoist_root_id: u32,
        package_id: u32,
        input_dep_id: u32,
        input_range: (u32, u32),
    ) -> HoistResult {
        let dependency = self.dep(input_dep_id);
        let this = self.trees[self_id as usize];
        if !as_defined && !self.probes.is_empty() && self.is_hidden_by(this.id) {
            return HoistResult::DependencyLoop;
        }
        if let Some(&dep_id) = self.placed.get(&(this.id, dependency.name_id)) {
            let dep = self.dep(dep_id);
            let res_id = self.resolutions[dep_id as usize];

            if res_id == INVALID && dep.behavior.is_bundled() {
                return HoistResult::DependencyLoop;
            }
            if res_id == INVALID && package_id == INVALID {
                return HoistResult::ResolveLater;
            }
            if res_id == INVALID {
                return HoistResult::ResolveReplace {
                    id: this.id,
                    dep_id,
                };
            }
            if package_id == INVALID {
                return HoistResult::Resolve(res_id);
            }
            if res_id == package_id {
                return HoistResult::Hoisted;
            }
            let always_linked = |b: Behavior| !b.is_optional() && !b.is_bundled();

            if contains(input_range, dep_id) {
                if dep.behavior.is_workspace()
                    && !(input_range.0..input_dep_id).any(|earlier| {
                        let row = self.dep(earlier);
                        row.name_id == dependency.name_id
                            && !row.behavior.is_workspace()
                            && !row.behavior.is_peer()
                            && self.resolutions[earlier as usize] == res_id
                    })
                {
                    return HoistResult::ResolveReplace {
                        id: this.id,
                        dep_id,
                    };
                }
                if dependency.behavior.is_peer()
                    && !dependency.behavior.is_optional_peer()
                    && always_linked(dep.behavior)
                    && self.is_in_peer_range(dependency, res_id)
                {
                    return HoistResult::Served(res_id);
                }
                return HoistResult::Hoisted;
            }

            if dependency.behavior.is_peer() {
                if self.flat.pkg_name_id[res_id as usize]
                    != self.flat.pkg_name_id[package_id as usize]
                    && self.flat.is_overridden_dependency(input_dep_id)
                {
                    return HoistResult::DependencyLoop;
                }
                let dedupe = if dependency.behavior.is_optional_peer() {
                    HoistResult::Rebind(res_id)
                } else {
                    HoistResult::Hoisted
                };
                let is_root_dependency = contains(self.flat.lists[0], dep_id);
                if self.is_in_peer_range(dependency, res_id) {
                    let own_shares = !dependency.behavior.is_optional_peer()
                        && (input_range.0..input_range.1).any(|own| {
                            let o = self.dep(own);
                            o.name_id == dependency.name_id
                                && !o.behavior.is_peer()
                                && always_linked(o.behavior)
                                && self.resolutions[own as usize] == res_id
                        });
                    if !dependency.behavior.is_optional_peer()
                        && ((is_root_dependency && always_linked(dep.behavior)) || own_shares)
                    {
                        return HoistResult::Served(res_id);
                    }
                    return dedupe;
                }
                if is_root_dependency {
                    return dedupe;
                }
            }
            return HoistResult::DependencyLoop;
        }

        if this.parent != INVALID_ID && this.id != hoist_root_id {
            if as_defined {
                self.probes_begin(input_range, package_id, dependency.behavior.is_peer());
            }
            let id = self.hoist_dependency(
                false,
                this.parent,
                hoist_root_id,
                package_id,
                input_dep_id,
                input_range,
            );
            if !as_defined || id != HoistResult::DependencyLoop {
                return id;
            }
        }

        if this.id == hoist_root_id {
            if !as_defined
                && self.hoist_root_resolves_elsewhere(this.id, package_id, dependency.name_id)
            {
                return HoistResult::DependencyLoop;
            }
            if (this.dependency_id as usize) < self.flat.deps.len()
                && self.resolutions[this.dependency_id as usize] == package_id
                && self.flat.deps[this.dependency_id as usize].name_id == dependency.name_id
            {
                return HoistResult::Hoisted;
            }
        }

        HoistResult::Placement {
            id: this.id,
            bundled: false,
        }
    }

    /// bun `PeerProbes::begin`.
    fn probes_begin(&mut self, owner_deps: (u32, u32), package_id: u32, is_for_peer: bool) {
        self.probes.clear();
        if package_id == INVALID {
            return;
        }
        let flat = self.flat;
        let is_probed =
            |d: &crate::flat::FDep<'_>| d.behavior.is_peer() && d.version.tag == VTag::Npm;
        let with_peers = self.with_peers.get_or_insert_with(|| {
            flat.lists
                .iter()
                .map(|&(b, e)| (b..e).any(|d| is_probed(&flat.deps[d as usize])))
                .collect()
        });
        if !with_peers
            .get(package_id as usize)
            .copied()
            .unwrap_or(false)
        {
            return;
        }
        if matches!(
            flat.res(package_id),
            Resolution::Workspace { .. } | Resolution::Symlink { .. }
        ) {
            return;
        }
        let pkg_deps = flat.lists[package_id as usize];
        for peer_dep_id in pkg_deps.0..pkg_deps.1 {
            let peer = &flat.deps[peer_dep_id as usize];
            if !is_probed(peer) {
                continue;
            }
            if (pkg_deps.0..pkg_deps.1).any(|d| {
                let x = &flat.deps[d as usize];
                x.name_id == peer.name_id && !x.behavior.is_peer()
            }) {
                continue;
            }
            let owner_copy = (owner_deps.0..owner_deps.1)
                .find(|&d| {
                    let x = &flat.deps[d as usize];
                    x.name_id == peer.name_id
                        && !x.behavior.is_peer()
                        && self.resolutions[d as usize] != INVALID
                })
                .map(|d| self.resolutions[d as usize]);
            let is_listed = match owner_copy {
                Some(id) => flat.satisfies(id, &peer.version),
                None => !peer.behavior.is_optional_peer(),
            };
            if is_listed {
                let has_same_peer = (owner_deps.0..owner_deps.1).any(|d| {
                    let x = &flat.deps[d as usize];
                    x.name_id == peer.name_id && !x.behavior.is_optional_peer()
                });
                let used_by_owner = match owner_copy {
                    _ if is_for_peer => OwnerCopy::None,
                    Some(copy) => OwnerCopy::Package(copy),
                    None if has_same_peer => OwnerCopy::FirstAbove,
                    None => OwnerCopy::None,
                };
                self.probes
                    .push((peer_dep_id, owner_copy.is_some(), used_by_owner));
            }
        }
    }

    /// bun `PeerProbes::is_hidden_by`.
    fn is_hidden_by(&mut self, tree_id: u32) -> bool {
        let flat = self.flat;
        let mut i = 0;
        while i < self.probes.len() {
            let (peer_dep_id, provided, owner_copy) = self.probes[i];
            let peer = &flat.deps[peer_dep_id as usize];
            let held = self
                .placed
                .get(&(tree_id, peer.name_id))
                .map(|&row| self.resolutions[row as usize])
                .filter(|&r| r != INVALID);
            let Some(held) = held else {
                i += 1;
                continue;
            };
            let is_optional = peer.behavior.is_optional_peer();
            if flat.satisfies(held, &peer.version) {
                match owner_copy {
                    OwnerCopy::Package(copy) if copy != held => {
                        let res = &*self.resolutions;
                        let regular = self.regular_targets.get_or_insert_with(|| {
                            let mut set = vec![false; flat.graph.packages.len()];
                            for (d, &t) in flat.deps.iter().zip(res.iter()) {
                                if !d.behavior.is_peer() && (t as usize) < set.len() {
                                    set[t as usize] = true;
                                }
                            }
                            set
                        });
                        if !regular[held as usize] {
                            return true;
                        }
                        self.probes[i].2 = OwnerCopy::None;
                    }
                    OwnerCopy::FirstAbove => self.probes[i].2 = OwnerCopy::Package(held),
                    _ => {}
                }
                self.probes[i].1 |= !is_optional;
                i += 1;
            } else if provided && !is_optional {
                return true;
            } else {
                self.probes.swap_remove(i);
            }
        }
        false
    }

    /// bun `Tree::hoist_root_resolves_elsewhere`.
    fn hoist_root_resolves_elsewhere(&self, hoist_root: u32, package_id: u32, name: u32) -> bool {
        if package_id == INVALID {
            return false;
        }
        if self.trees[hoist_root as usize].parent == INVALID_ID {
            return false;
        }
        let flat = self.flat;
        let res = &*self.resolutions;
        let resolves_elsewhere = |pkg: u32| -> bool {
            let Some(&(b, e)) = flat.lists.get(pkg as usize) else {
                return false;
            };
            (b..e).any(|d| {
                let dep = &flat.deps[d as usize];
                dep.name_id == name
                    && !dep.behavior.is_bundled()
                    && flat.peer_by_version(d, |_| true).unwrap_or(res[d as usize]) != package_id
            })
        };
        let nested_in = |tree_id: u32, owner: u32| -> Vec<u32> {
            let Some(&own) = flat.lists.get(owner as usize) else {
                return Vec::new();
            };
            self.rows[tree_id as usize]
                .iter()
                .filter(|&&d| contains(own, d) && !flat.deps[d as usize].behavior.is_bundled())
                .map(|&d| res[d as usize])
                .filter(|&p| p != INVALID)
                .collect()
        };
        let owner = res[self.trees[hoist_root as usize].dependency_id as usize];
        if resolves_elsewhere(owner) {
            return true;
        }
        let nested = nested_in(hoist_root, owner);
        for &p in &nested {
            if resolves_elsewhere(p) {
                return true;
            }
        }
        if nested.is_empty() {
            return false;
        }
        let mut scope: Vec<(u32, u32)> = vec![(hoist_root, owner)];
        for (id, tree) in self.trees.iter().enumerate().skip(hoist_root as usize + 1) {
            let Some(&(_, parent_owner)) = scope.iter().find(|(s, _)| *s == tree.parent) else {
                continue;
            };
            let Some(&po) = flat.lists.get(parent_owner as usize) else {
                continue;
            };
            if !contains(po, tree.dependency_id)
                || flat.deps[tree.dependency_id as usize].behavior.is_bundled()
            {
                continue;
            }
            let tree_owner = res[tree.dependency_id as usize];
            if nested_in(id as u32, tree_owner)
                .into_iter()
                .any(resolves_elsewhere)
            {
                return true;
            }
            scope.push((id as u32, tree_owner));
        }
        false
    }
}

// ─── reading the result ─────────────────────────────────────────────────────────────────────

/// One `node_modules` folder in output order.
#[derive(Debug, Clone)]
pub(crate) struct Folder {
    pub tree_id: u32,
    /// `""` for the root, `a/b` for `node_modules/a/node_modules/b/node_modules`, `./ws` for
    /// a workspace that has no link in the root `node_modules`.
    pub path: String,
    pub depth: usize,
    /// Rows (dependency ids) sorted by name.
    pub rows: Vec<u32>,
}

impl Folder {
    /// The lockfile key of a row in this folder.
    pub fn key(&self, name: &str) -> String {
        let mut k = String::with_capacity(self.path.len() + 1 + name.len());
        k.push_str(&self.path);
        if self.depth != 0 {
            k.push('/');
        }
        k.push_str(name);
        k
    }
}

/// bun `Tree::is_unlinked_member`: a workspace tree whose link lost its root row to a root
/// dependency of the same name.
pub(crate) fn is_unlinked_member(flat: &Flat<'_>, h: &Hoisted, t: u32) -> bool {
    let tree = h.trees[t as usize];
    if tree.parent == INVALID_ID {
        return false;
    }
    flat.deps
        .get(tree.dependency_id as usize)
        .is_some_and(|d| d.behavior.is_workspace() && d.version.tag == VTag::Workspace)
        && !h.rows[tree.parent as usize].contains(&tree.dependency_id)
}

/// The folders in bun's write order: by depth, then path; rows by name.
pub(crate) fn folders(flat: &Flat<'_>, h: &Hoisted) -> Vec<Folder> {
    let mut out: Vec<Folder> = Vec::with_capacity(h.trees.len());
    let mut chain: Vec<u32> = Vec::new();
    for (t, rows) in h.rows.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        chain.clear();
        let mut p = h.trees[t].id;
        while p > 0 && (p as usize) < h.trees.len() {
            chain.push(p);
            p = h.trees[p as usize].parent;
        }
        let depth = chain.len();
        let mut path = String::new();
        for (i, &id) in chain.iter().rev().enumerate() {
            if i != 0 {
                path.push('/');
            }
            let name = flat
                .deps
                .get(h.trees[id as usize].dependency_id as usize)
                .map_or("", |d| d.name);
            if is_unlinked_member(flat, h, id) {
                path.clear();
                path.push_str("./");
            }
            path.push_str(name);
        }
        let mut rows = rows.clone();
        rows.sort_by(|&a, &b| {
            flat.deps[a as usize]
                .name
                .as_bytes()
                .cmp(flat.deps[b as usize].name.as_bytes())
        });
        out.push(Folder {
            tree_id: t as u32,
            path,
            depth,
            rows,
        });
    }
    out.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.path.as_bytes().cmp(b.path.as_bytes()))
    });
    out
}

/// bun's dependency features: which edges an install places.
fn is_enabled(b: Behavior, include: Behavior, local_owner: bool) -> bool {
    b.is_prod()
        || (include.intersects(Behavior::OPTIONAL) && b.is_optional())
        || (include.intersects(Behavior::DEV) && local_owner && b.is_dev())
        || (include.intersects(Behavior::PEER) && b.is_peer())
        || b.is_workspace()
}

/// Reduces the saved tree to what `opts` installs (bun's `Filter` tree): the rows that the
/// root reaches by node's lookup through dependencies the install does not skip.
pub(crate) fn install_tree(flat: &Flat<'_>, h: &Hoisted, opts: &HoistOptions) -> Tree {
    let n = flat.graph.packages.len();
    let res = &flat.resolutions;
    let mut placed: HashMap<(u32, u32), u32> = map(flat.deps.len());
    for (t, rows) in h.rows.iter().enumerate() {
        for &r in rows {
            placed
                .entry((t as u32, flat.deps[r as usize].name_id))
                .or_insert(r);
        }
    }
    let mut own_folder: HashMap<(u32, u32), u32> = map(h.trees.len());
    for t in h.trees.iter().skip(1) {
        own_folder.insert((t.parent, t.dependency_id), t.id);
    }
    let selected = |p: u32| {
        opts.workspaces
            .as_ref()
            .is_none_or(|w| w.contains(&PackageId(p)))
    };

    let mut left_out = vec![true; flat.deps.len()];
    for pkg in 0..n as u32 {
        let local = matches!(
            flat.res(pkg),
            Resolution::Root | Resolution::Workspace { .. } | Resolution::Folder { .. }
        );
        for d in flat.list(pkg) {
            let dep = &flat.deps[d as usize];
            let t = res[d as usize];
            if t == INVALID || t as usize >= n || dep.behavior.is_bundled() {
                continue;
            }
            let target = &flat.graph.packages[t as usize];
            if !target
                .platform
                .matches_for_edge(&opts.platform, dep.behavior.is_optional())
            {
                continue;
            }
            if !is_enabled(dep.behavior, opts.include, local) {
                continue;
            }
            if pkg == 0 {
                let link = dep.behavior.is_workspace() && dep.version.tag == VTag::Workspace;
                if (link && !selected(t)) || (!link && !selected(0)) {
                    continue;
                }
            }
            left_out[d as usize] = false;
        }
    }

    let mut kept: HashMap<(u32, u32), ()> = map(0);
    let mut seen: HashMap<(u32, u32), ()> = map(0);
    let mut todo: Vec<(u32, u32)> = if h.trees.is_empty() {
        Vec::new()
    } else {
        vec![(0, 0)]
    };
    while let Some((from, pkg)) = todo.pop() {
        if seen.insert((from, pkg), ()).is_some() {
            continue;
        }
        let list = flat.lists[pkg as usize];
        'dep: for d in list.0..list.1 {
            let dep = &flat.deps[d as usize];
            let target = res[d as usize];
            if target == INVALID || left_out[d as usize] {
                continue;
            }
            if pkg == 0 && dep.behavior.is_workspace() && placed.get(&(0, dep.name_id)) != Some(&d)
            {
                let own = own_folder.get(&(0, d)).copied().unwrap_or(0);
                todo.push((own, target));
                continue;
            }
            let mut t = from;
            let row = loop {
                if let Some(&row) = placed.get(&(t, dep.name_id))
                    && (res[row as usize] == target || !contains(list, row))
                {
                    break row;
                }
                t = h.trees[t as usize].parent;
                if t == INVALID_ID {
                    continue 'dep;
                }
            };
            if res[row as usize] != target && left_out[row as usize] {
                continue;
            }
            if kept.insert((t, row), ()).is_none() {
                let p = res[row as usize];
                if p != INVALID {
                    todo.push((own_folder.get(&(t, row)).copied().unwrap_or(t), p));
                }
            }
        }
    }

    let mut tree = Tree::default();
    let mut node_of: HashMap<(u32, u32), u32> = map(kept.len());
    let mut ws_index: HashMap<u32, usize> = map(0);
    for folder in folders(flat, h) {
        let bt = h.trees[folder.tree_id as usize];
        let parent = if folder.tree_id == 0 {
            None
        } else {
            node_of.get(&(bt.parent, bt.dependency_id)).copied()
        };
        let ws_pkg = (folder.tree_id != 0)
            .then(|| flat.deps.get(bt.dependency_id as usize))
            .flatten()
            .filter(|d| d.behavior.is_workspace() && d.version.tag == VTag::Workspace)
            .map(|_| res[bt.dependency_id as usize])
            .filter(|&p| p != INVALID);
        for &r in &folder.rows {
            if !kept.contains_key(&(folder.tree_id, r)) {
                continue;
            }
            let dep = &flat.deps[r as usize];
            let id = tree.nodes.len() as u32;
            node_of.insert((folder.tree_id, r), id);
            tree.nodes.push(TreeNode {
                package: PackageId(res[r as usize]),
                name: dep.name.to_owned(),
                parent,
                path: folder.key(dep.name),
            });
            if let Some(ws) = ws_pkg {
                let i = *ws_index.entry(ws).or_insert_with(|| {
                    tree.workspace_roots.push((PackageId(ws), Vec::new()));
                    tree.workspace_roots.len() - 1
                });
                tree.workspace_roots[i].1.push(id);
            }
        }
    }
    tree
}

#[cfg(test)]
mod tests;
