//! The isolated layout (bun `isolated_install.rs` `build_store`, `isolated_install/Store.rs`):
//!
//! ```text
//! node_modules/<name>                      -> .fnpm/<key>/node_modules/<name>
//! node_modules/.fnpm/<key>/node_modules/<name>/...   the package (clone/hardlink of the store)
//! node_modules/.fnpm/<key>/node_modules/<dep>        -> ../../<dep key>/node_modules/<dep>
//! node_modules/.fnpm/node_modules/<name>   hidden hoist (install.hoist / hoistPattern)
//! ```
//! With the global virtual store, `.fnpm/<key>` is a symlink to
//! `<store>/links/<key>-<closure hash16>`.
//!
//! Planning is two passes, as in bun. The first builds a tree of nodes, one per dependency
//! position, resolving every peer dependency against the ancestors (a package may satisfy its
//! own peers; an unresolved peer gets the version the lockfile chose). Positions whose leaking
//! peers resolve the same way are merged early, so the tree stays linear in practice. The
//! second pass walks the tree breadth first and turns nodes into entries, sharing one entry
//! between nodes with the same package and the same set of resolved peers.
//!
//! Entries for the root, workspaces and `link:` packages are link targets only: nothing is
//! materialised for them under `.fnpm`, dependents link straight to their folders.

use std::collections::{HashMap, HashSet};

use fnpm_types::{Behavior, LockGraph, PackageId, Platform, Resolution};
use xxhash_rust::xxh3::Xxh3;

use crate::matcher::PnpmMatcher;
use crate::store_key;

/// One `node_modules/.fnpm/<key>` entry: a package in one peer context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The package.
    pub package: PackageId,
    /// `name@resolution` escaped (`/`, `\`, `:`, `#`, `?` -> `+`), resolution part capped at
    /// 80 bytes (63 + `+` + 16 hex hash), plus `+<peer hash16>` when the entry has peers (or
    /// is a tarball, whose pinned integrity stands in).
    pub key: String,
    /// `(alias, entry index)` for each linked dependency, sorted by alias.
    pub deps: Vec<(String, u32)>,
    /// The closure hash for the global virtual store; `None` when ineligible (trusted,
    /// patched, local, or depends on an ineligible entry), or when the global store is off.
    pub global_hash: Option<u64>,
}

/// The planned isolated layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IsolatedLayout {
    /// Every entry; index 0 is the root.
    pub entries: Vec<Entry>,
    /// Root and each workspace: `(package id, [(alias, entry index)])` linked into its
    /// `node_modules`.
    pub roots: Vec<(PackageId, Vec<(String, u32)>)>,
    /// Names hoisted into `node_modules/.fnpm/node_modules`.
    pub hidden_hoist: Vec<(String, u32)>,
    /// Names hoisted into the root `node_modules` (`publicHoistPattern`).
    pub public_hoist: Vec<(String, u32)>,
}

impl IsolatedLayout {
    /// `.fnpm/<key>` for an entry, or with the global store `<key>-<hash16>` (the directory
    /// name under `<store>/links`).
    pub fn global_dir_name(&self, entry: u32) -> Option<String> {
        let e = self.entries.get(entry as usize)?;
        e.global_hash.map(|h| format!("{}-{h:016x}", e.key))
    }
}

/// Planner inputs that come from the configuration.
pub struct PlanOptions<'a> {
    /// `install.hoist`: fill `.fnpm/node_modules` (default true).
    pub hoist: bool,
    /// Which names go into `.fnpm/node_modules`; empty means all.
    pub hoist_pattern: &'a [String],
    /// Which transitive names are linked into the root `node_modules`; empty means none.
    pub public_hoist_pattern: &'a [String],
    /// Compute global virtual store hashes.
    pub global_store: bool,
    /// Packages whose scripts will run (never shared through the global store).
    pub trusted: &'a dyn Fn(PackageId) -> bool,
}

/// Which edges are installed. [`plan`] uses [`PlanFilter::default`].
#[derive(Debug, Clone)]
pub struct PlanFilter {
    /// Edge kinds to follow (`--omit`, `--production`): `PROD | DEV | OPTIONAL | PEER` by
    /// default. Dev edges are only followed from the root, workspaces and local folders.
    pub include: Behavior,
    /// Packages that do not match are not installed.
    pub platform: Platform,
    /// `--filter`: the workspaces whose dependencies are installed; `None` is all.
    pub workspaces: Option<Vec<PackageId>>,
    /// `--no-verify` is part of the global store hash, so an unverified build is never shared
    /// with verified installs.
    pub no_verify: bool,
    /// The project directory's name, for an unnamed root package's key.
    pub project_basename: String,
}

impl Default for PlanFilter {
    fn default() -> Self {
        Self {
            include: Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER,
            platform: Platform::current(),
            workspaces: None,
            no_verify: false,
            project_basename: String::new(),
        }
    }
}

/// Plans with the default filter (everything, current platform).
pub fn plan(graph: &LockGraph, opts: &PlanOptions<'_>) -> IsolatedLayout {
    plan_filtered(graph, opts, &PlanFilter::default())
}

/// A dependency edge: `(owner package, index in its dependencies)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EdgeRef {
    owner: u32,
    idx: u32,
}

#[derive(Debug, Clone, Copy)]
struct Peer {
    edge: EdgeRef,
    pkg: u32,
    auto_installed: bool,
}

struct Node {
    pkg: u32,
    dep: Option<EdgeRef>,
    parent: Option<u32>,
    /// Wired dependencies (including resolved peers): `(edge, package)`.
    deps: Vec<(EdgeRef, u32)>,
    /// Transitive peers resolved above this node; sorted by package name then id.
    peers: Vec<Peer>,
    /// Children: each becomes a link in the entry's `node_modules`.
    nodes: Vec<u32>,
}

struct Ctx<'g> {
    graph: &'g LockGraph,
    filter: &'g PlanFilter,
    /// Interned edge names: `names[pkg][edge idx]`.
    edge_name: Vec<Vec<u32>>,
    disabled: Vec<bool>,
    root_declares_workspace: HashSet<u32>,
    /// Names the root declares without the workspace flag (they shadow a member of that name).
    root_plain_names: HashSet<u32>,
}

impl<'g> Ctx<'g> {
    fn edge(&self, e: EdgeRef) -> &'g fnpm_types::DepEdge {
        &self.graph.packages[e.owner as usize].dependencies[e.idx as usize]
    }

    fn name_id(&self, e: EdgeRef) -> u32 {
        self.edge_name[e.owner as usize][e.idx as usize]
    }

    fn is_workspace_literal(&self, e: EdgeRef) -> bool {
        self.edge(e).literal.starts_with("workspace:")
    }

    fn is_local(&self, pkg: u32) -> bool {
        self.graph.packages[pkg as usize].resolution.is_local_tree()
    }

    fn is_omitted_peer(&self, e: EdgeRef) -> bool {
        let b = self.edge(e).behavior;
        b.is_peer() && !self.filter.include.intersects(Behavior::PEER)
    }

    /// bun `is_filtered_dependency_or_workspace`.
    fn is_filtered(&self, e: EdgeRef) -> bool {
        let edge = self.edge(e);
        let b = edge.behavior;
        let Some(target) = edge.target else {
            return !b.is_optional_peer();
        };
        if target.index() >= self.graph.packages.len() {
            return !b.is_optional_peer();
        }
        if self.disabled[target.index()] {
            return true;
        }
        if b.is_bundled() || self.graph.packages[target.index()].meta.bundled {
            return true;
        }
        // is_placed
        if b.is_peer() {
            if !self.filter.include.intersects(Behavior::PEER) {
                return true;
            }
        } else if b.is_dev() && !b.is_prod() {
            if !self.is_local(e.owner) || !self.filter.include.intersects(Behavior::DEV) {
                return true;
            }
        } else if (b.is_optional() && !self.filter.include.intersects(Behavior::OPTIONAL))
            || (!b.intersects(Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER)
                && !b.is_workspace())
        {
            return true;
        }
        if e.owner != 0 {
            return false;
        }
        match &self.filter.workspaces {
            Some(sel) => !sel.contains(&if b.is_workspace() {
                target
            } else {
                PackageId::ROOT
            }),
            None => false,
        }
    }

    fn is_unlinked_member(&self, e: EdgeRef) -> bool {
        self.edge(e).behavior.is_workspace() && self.root_plain_names.contains(&self.name_id(e))
    }

    fn pkg_name(&self, pkg: u32) -> &'g str {
        &self.graph.packages[pkg as usize].name
    }
}

/// bun's `Behavior::cmp`: workspace, dev, optional, prod, peer.
fn behavior_rank(b: Behavior) -> [bool; 5] {
    [
        !b.is_workspace(),
        !b.is_dev(),
        !b.contains(Behavior::OPTIONAL),
        !b.is_prod(),
        !b.is_peer(),
    ]
}
/// A fixed-width bitset per package over the universe of peer names.
struct Bits {
    words: usize,
    data: Vec<u64>,
}

impl Bits {
    fn new(rows: usize, bits: usize) -> Self {
        let words = bits.div_ceil(64).max(1);
        Self {
            words,
            data: vec![0; rows * words],
        }
    }
    fn row(&self, r: usize) -> &[u64] {
        &self.data[r * self.words..(r + 1) * self.words]
    }
    fn set(&mut self, r: usize, bit: usize) {
        self.data[r * self.words + bit / 64] |= 1 << (bit % 64);
    }
}

/// Plans the isolated layout of `graph`.
pub fn plan_filtered(
    graph: &LockGraph,
    opts: &PlanOptions<'_>,
    filter: &PlanFilter,
) -> IsolatedLayout {
    let n_pkgs = graph.packages.len();
    if n_pkgs == 0 {
        return IsolatedLayout::default();
    }

    // Intern edge names.
    let mut interner: HashMap<&str, u32> = HashMap::new();
    let mut edge_name: Vec<Vec<u32>> = Vec::with_capacity(n_pkgs);
    for p in &graph.packages {
        let mut v = Vec::with_capacity(p.dependencies.len());
        for e in &p.dependencies {
            let next = interner.len() as u32;
            v.push(*interner.entry(e.name.as_str()).or_insert(next));
        }
        edge_name.push(v);
    }
    let disabled: Vec<bool> = graph
        .packages
        .iter()
        .map(|p| {
            !p.resolution.is_local_tree() && !p.platform.matches_for_edge(&filter.platform, true)
        })
        .collect();

    let mut ctx = Ctx {
        graph,
        filter,
        edge_name,
        disabled,
        root_declares_workspace: HashSet::new(),
        root_plain_names: HashSet::new(),
    };
    for (i, e) in graph.packages[0].dependencies.iter().enumerate() {
        let er = EdgeRef {
            owner: 0,
            idx: i as u32,
        };
        if !e.behavior.is_workspace() {
            ctx.root_plain_names.insert(ctx.name_id(er));
            continue;
        }
        if let Some(t) = e.target
            && !ctx.is_filtered(er)
        {
            ctx.root_declares_workspace.insert(t.0);
        }
    }

    // Peer names and leaking peers (fixpoint), bun `build_store`.
    let mut peer_bit: HashMap<u32, usize> = HashMap::new();
    for (p, pkg) in graph.packages.iter().enumerate() {
        for (i, e) in pkg.dependencies.iter().enumerate() {
            if e.behavior.is_peer() {
                let id = ctx.edge_name[p][i];
                let next = peer_bit.len();
                peer_bit.entry(id).or_insert(next);
            }
        }
    }
    let peer_count = peer_bit.len();
    let mut leaking = Bits::new(n_pkgs, peer_count);
    if peer_count != 0 {
        let mut peer_targets: Vec<Vec<u32>> = vec![Vec::new(); peer_count];
        let mut own = Bits::new(n_pkgs, peer_count);
        let mut provides = Bits::new(n_pkgs, peer_count);
        for (p, pkg) in graph.packages.iter().enumerate() {
            for (i, e) in pkg.dependencies.iter().enumerate() {
                let Some(&bit) = peer_bit.get(&ctx.edge_name[p][i]) else {
                    continue;
                };
                if let Some(t) = e.target
                    && t.index() < n_pkgs
                    && !peer_targets[bit].contains(&t.0)
                {
                    peer_targets[bit].push(t.0);
                }
                let er = EdgeRef {
                    owner: p as u32,
                    idx: i as u32,
                };
                if e.behavior.is_peer() {
                    own.set(p, bit);
                } else if !ctx.is_filtered(er) {
                    provides.set(p, bit);
                }
            }
        }
        let words = leaking.words;
        let mut scratch = vec![0u64; words];
        let mut changed = true;
        while changed {
            changed = false;
            for p in 0..n_pkgs {
                scratch.copy_from_slice(own.row(p));
                for (i, e) in graph.packages[p].dependencies.iter().enumerate() {
                    if e.behavior.is_peer() {
                        if let Some(&bit) = peer_bit.get(&ctx.edge_name[p][i]) {
                            for &child in &peer_targets[bit] {
                                for (w, s) in scratch.iter_mut().enumerate() {
                                    *s |= leaking.data[child as usize * words + w];
                                }
                            }
                        }
                    } else if let Some(t) = e.target
                        && t.index() < n_pkgs
                    {
                        for (w, s) in scratch.iter_mut().enumerate() {
                            *s |= leaking.data[t.index() * words + w];
                        }
                    }
                }
                for (w, s) in scratch.iter_mut().enumerate() {
                    *s &= !provides.data[p * words + w];
                }
                if leaking.row(p) != scratch.as_slice() {
                    leaking.data[p * words..(p + 1) * words].copy_from_slice(&scratch);
                    changed = true;
                }
            }
        }
    }
    let mut bit_names: Vec<u32> = vec![0; peer_count];
    for (&name, &bit) in &peer_bit {
        bit_names[bit] = name;
    }

    // First pass: the full tree with resolved peers (DFS).
    let mut nodes: Vec<Node> = Vec::new();
    struct Queued {
        parent: Option<u32>,
        dep: Option<EdgeRef>,
        pkg: u32,
    }
    let mut queue: Vec<Queued> = vec![Queued {
        parent: None,
        dep: None,
        pkg: 0,
    }];
    let mut early: HashMap<(u32, u64), u32> = HashMap::new();
    let mut sort_buf: Vec<u32> = Vec::new();
    let mut peer_edges: Vec<EdgeRef> = Vec::new();
    let mut visited: Vec<u32> = Vec::new();

    let name_of = |ctx: &Ctx<'_>, e: EdgeRef| ctx.name_id(e);

    'next_node: while let Some(q) = queue.pop() {
        // Cycles: reuse the ancestor's node.
        'check_cycle: {
            let mut cur = q.parent;
            while let Some(c) = cur {
                let cn = &nodes[c as usize];
                if cn.pkg == q.pkg {
                    match (cn.dep, q.dep) {
                        (None, None) => {
                            let p = q.parent.expect("has parent");
                            nodes[p as usize].nodes.push(c);
                            continue 'next_node;
                        }
                        (None, _) | (_, None) => break 'check_cycle,
                        (Some(a), Some(b)) => {
                            if name_of(&ctx, a) == name_of(&ctx, b)
                                && ctx.edge(a).behavior.is_workspace()
                                    == ctx.edge(b).behavior.is_workspace()
                            {
                                let p = q.parent.expect("has parent");
                                nodes[p as usize].nodes.push(c);
                                continue 'next_node;
                            }
                        }
                    }
                }
                cur = cn.parent;
            }
        }

        let node_id = nodes.len() as u32;
        let mut skip_deps = q.pkg == 0 && q.dep.is_some();

        if let Some(dep) = q.dep {
            let dep_is_ws_literal = ctx.is_workspace_literal(dep);
            if dep_is_ws_literal
                && !ctx.edge(dep).behavior.is_workspace()
                && ctx.root_declares_workspace.contains(&q.pkg)
            {
                skip_deps = true;
            }
            let ctx_hash = if dep_is_ws_literal || peer_count == 0 {
                0
            } else {
                let row = leaking.row(q.pkg as usize);
                if row.iter().all(|w| *w == 0) {
                    0
                } else {
                    let mut h = Xxh3::new();
                    for bit in 0..peer_count {
                        if row[bit / 64] & (1 << (bit % 64)) == 0 {
                            continue;
                        }
                        let name = bit_names[bit];
                        let mut resolved = u32::MAX;
                        let mut cur = q.parent;
                        'resolve: while let Some(c) = cur {
                            let cn = &nodes[c as usize];
                            for (e, p) in &cn.deps {
                                if ctx.name_id(*e) == name {
                                    resolved = *p;
                                    break 'resolve;
                                }
                            }
                            for pr in &cn.peers {
                                if !pr.auto_installed && ctx.name_id(pr.edge) == name {
                                    resolved = pr.pkg;
                                    break 'resolve;
                                }
                            }
                            cur = cn.parent;
                        }
                        h.update(&name.to_le_bytes());
                        h.update(&resolved.to_le_bytes());
                    }
                    h.digest()
                }
            };
            'dont_dedupe: {
                if let Some(&dn) = early.get(&(q.pkg, ctx_hash)) {
                    let Some(dd) = nodes[dn as usize].dep else {
                        break 'dont_dedupe;
                    };
                    if name_of(&ctx, dd) != name_of(&ctx, dep) {
                        break 'dont_dedupe;
                    }
                    let a_ws = ctx.is_workspace_literal(dd);
                    if a_ws != dep_is_ws_literal {
                        break 'dont_dedupe;
                    }
                    if a_ws
                        && ctx.edge(dd).behavior.is_workspace()
                            != ctx.edge(dep).behavior.is_workspace()
                    {
                        break 'dont_dedupe;
                    }
                    let dedupe_peers = nodes[dn as usize].peers.clone();
                    for peer in dedupe_peers {
                        let pname = ctx.name_id(peer.edge);
                        let mut cur = q.parent;
                        'walk: while let Some(c) = cur {
                            if nodes[c as usize]
                                .deps
                                .iter()
                                .any(|(e, _)| ctx.name_id(*e) == pname)
                            {
                                break 'walk;
                            }
                            if nodes[c as usize].pkg != peer.pkg {
                                insert_peer(&ctx, &mut nodes[c as usize].peers, peer);
                            }
                            cur = nodes[c as usize].parent;
                        }
                    }
                    let p = q.parent.expect("dependency has a parent");
                    nodes[p as usize].nodes.push(dn);
                    continue 'next_node;
                }
                early.insert((q.pkg, ctx_hash), node_id);
            }
        }

        nodes.push(Node {
            pkg: q.pkg,
            dep: q.dep,
            parent: q.parent,
            deps: Vec::new(),
            peers: Vec::new(),
            nodes: Vec::new(),
        });
        if let Some(p) = q.parent {
            nodes[p as usize].nodes.push(node_id);
        }
        if skip_deps {
            continue;
        }
        let queue_mark = queue.len();
        let pkg = &graph.packages[q.pkg as usize];
        sort_buf.clear();
        sort_buf.extend(0..pkg.dependencies.len() as u32);
        sort_buf.sort_by(|&a, &b| {
            let ea = &pkg.dependencies[a as usize];
            let eb = &pkg.dependencies[b as usize];
            behavior_rank(ea.behavior)
                .cmp(&behavior_rank(eb.behavior))
                .then_with(|| ea.name.cmp(&eb.name))
        });
        peer_edges.clear();
        for &i in &sort_buf {
            let er = EdgeRef {
                owner: q.pkg,
                idx: i,
            };
            if !ctx.is_omitted_peer(er) && ctx.is_filtered(er) {
                continue;
            }
            let edge = ctx.edge(er);
            if !edge.behavior.is_peer() {
                let Some(t) = edge.target else { continue };
                if !ctx.is_unlinked_member(er) {
                    nodes[node_id as usize].deps.push((er, t.0));
                }
                queue.push(Queued {
                    parent: Some(node_id),
                    dep: Some(er),
                    pkg: t.0,
                });
                continue;
            }
            peer_edges.push(er);
        }

        for &pe in &peer_edges {
            let pname = ctx.name_id(pe);
            let edge = ctx.edge(pe);
            let best = if ctx.is_omitted_peer(pe) {
                u32::MAX
            } else {
                edge.target.map_or(u32::MAX, |t| {
                    if ctx.disabled.get(t.index()).copied().unwrap_or(true) {
                        u32::MAX
                    } else {
                        t.0
                    }
                })
            };
            visited.clear();
            let (resolved, auto) = 'resolved: {
                let mut cur = Some(node_id);
                while let Some(c) = cur {
                    let cn = &nodes[c as usize];
                    for (e, p) in &cn.deps {
                        if ctx.name_id(*e) == pname {
                            break 'resolved (*p, false);
                        }
                    }
                    for pr in &cn.peers {
                        if ctx.name_id(pr.edge) != pname {
                            continue;
                        }
                        if !pr.auto_installed {
                            break 'resolved (pr.pkg, false);
                        }
                        if best == u32::MAX {
                            break 'resolved (u32::MAX, true);
                        }
                        if best == pr.pkg {
                            break 'resolved (pr.pkg, true);
                        }
                        let mut up = Some(c);
                        while let Some(u) = up {
                            visited.push(u);
                            up = nodes[u as usize].parent;
                        }
                        break 'resolved (best, true);
                    }
                    visited.push(c);
                    cur = cn.parent;
                }
                (best, true)
            };
            if resolved == u32::MAX {
                continue;
            }
            for &v in &visited {
                if nodes[v as usize].pkg == resolved {
                    continue;
                }
                insert_peer(
                    &ctx,
                    &mut nodes[v as usize].peers,
                    Peer {
                        edge: pe,
                        pkg: resolved,
                        auto_installed: auto,
                    },
                );
            }
            if !visited.is_empty() {
                nodes[node_id as usize].deps.push((pe, resolved));
                queue.push(Queued {
                    parent: Some(node_id),
                    dep: Some(pe),
                    pkg: resolved,
                });
            }
        }
        queue[queue_mark..].reverse();
    }

    // Second pass: entries (BFS), deduplicated by package and peer set.
    struct DedupeInfo {
        entry: u32,
        dep: Option<EdgeRef>,
        peers: Vec<u32>,
    }
    struct RawEntry {
        node: u32,
        deps: Vec<(u32 /* name id */, u32 /* entry */, EdgeRef)>,
        public: Vec<(u32, u32, EdgeRef)>,
    }
    let mut entries: Vec<RawEntry> = Vec::new();
    let mut entry_hoisted: Vec<bool> = Vec::new();
    let mut entry_peer_hash: Vec<u64> = Vec::new();
    let mut dedupe: HashMap<u32, Vec<DedupeInfo>> = HashMap::new();
    let mut public_hoisted: HashSet<u32> = HashSet::new();
    let mut hidden_hoisted: HashSet<u32> = HashSet::new();
    let mut hidden_keys: Vec<(u32, EdgeRef)> = Vec::new();
    let hoist_matcher = PnpmMatcher::new(opts.hoist_pattern);
    let public_matcher = PnpmMatcher::new(opts.public_hoist_pattern);
    fn alias_of<'g>(ctx: &Ctx<'g>, e: EdgeRef) -> &'g str {
        ctx.edge(e).name.as_str()
    }
    let claim_hidden = |ctx: &Ctx<'_>, set: &mut HashSet<u32>, e: EdgeRef| -> bool {
        opts.hoist
            && (hoist_matcher.is_empty() || hoist_matcher.is_match(alias_of(ctx, e)))
            && set.insert(ctx.name_id(e))
    };
    let storable = |pkg: u32| {
        matches!(
            graph.packages[pkg as usize].resolution,
            Resolution::Npm { .. }
                | Resolution::Git { .. }
                | Resolution::Github { .. }
                | Resolution::LocalTarball { .. }
                | Resolution::RemoteTarball { .. }
        )
    };
    let insert_dep =
        |list: &mut Vec<(u32, u32, EdgeRef)>, ctx: &Ctx<'_>, item: (u32, u32, EdgeRef)| {
            let name = ctx.edge(item.2).name.as_str();
            if list.iter().any(|(n, e, _)| *n == item.0 && *e == item.1) {
                return;
            }
            let pos = list.partition_point(|(_, _, er)| ctx.edge(*er).name.as_str() <= name);
            list.insert(pos, item);
        };

    let mut equeue: std::collections::VecDeque<(u32, Option<u32>)> =
        std::collections::VecDeque::new();
    equeue.push_back((0, None));
    'next_entry: while let Some((node_id, parent_entry)) = equeue.pop_front() {
        let node = &nodes[node_id as usize];
        let pkg = node.pkg;
        let cur_peers: Vec<u32> = node.peers.iter().map(|p| p.pkg).collect();
        let cur_dep = node.dep;
        let found_existing = dedupe.contains_key(&pkg);
        if let Some(infos) = dedupe.get(&pkg) {
            for info in infos {
                if (info.dep.is_none() || cur_dep.is_none())
                    && info.dep.is_some() != cur_dep.is_some()
                {
                    continue;
                }
                if let (Some(a), Some(b)) = (info.dep, cur_dep)
                    && ctx.is_workspace_literal(a)
                    && ctx.is_workspace_literal(b)
                    && ctx.edge(a).behavior.is_workspace() != ctx.edge(b).behavior.is_workspace()
                {
                    continue;
                }
                if info.peers != cur_peers {
                    continue;
                }
                let entry = info.entry;
                if let (Some(cd), Some(id)) = (cur_dep, info.dep)
                    && storable(pkg)
                    && ctx.name_id(cd) != ctx.name_id(id)
                    && claim_hidden(&ctx, &mut hidden_hoisted, cd)
                {
                    hidden_keys.push((entry, cd));
                }
                if let (Some(pe), Some(cd)) = (parent_entry, cur_dep)
                    && !ctx.edge(cd).behavior.is_workspace()
                {
                    insert_dep(
                        &mut entries[pe as usize].deps,
                        &ctx,
                        (ctx.name_id(cd), entry, cd),
                    );
                }
                continue 'next_entry;
            }
        }

        let pinned: Vec<u8> = match &graph.packages[pkg as usize].resolution {
            Resolution::LocalTarball {
                integrity: Some(i), ..
            }
            | Resolution::RemoteTarball {
                integrity: Some(i), ..
            } => i.digest().to_vec(),
            _ => Vec::new(),
        };
        let peer_hash = if node.peers.is_empty() {
            if pinned.len() >= 8 {
                let mut b = [0u8; 8];
                b.copy_from_slice(&pinned[..8]);
                u64::from_be_bytes(b)
            } else {
                0
            }
        } else {
            let mut h = Xxh3::new();
            h.update(&pinned);
            for p in &node.peers {
                let pk = &graph.packages[p.pkg as usize];
                h.update(pk.name.as_bytes());
                h.update(pk.resolution.to_string().as_bytes());
            }
            h.digest()
        };

        let new_id = entries.len() as u32;
        let hoisted = match cur_dep {
            Some(cd) => !ctx.is_unlinked_member(cd) && claim_hidden(&ctx, &mut hidden_hoisted, cd),
            None => false,
        };
        entries.push(RawEntry {
            node: node_id,
            deps: Vec::new(),
            public: Vec::new(),
        });
        entry_hoisted.push(hoisted);
        entry_peer_hash.push(peer_hash);
        let _ = found_existing;

        if let (Some(pe), Some(cd)) = (parent_entry, cur_dep)
            && !ctx.edge(cd).behavior.is_workspace()
        {
            insert_dep(
                &mut entries[pe as usize].deps,
                &ctx,
                (ctx.name_id(cd), new_id, cd),
            );
            if pe == 0 {
                public_hoisted.insert(ctx.name_id(cd));
            } else if !public_matcher.is_empty()
                && public_matcher.is_match(alias_of(&ctx, cd))
                && public_hoisted.insert(ctx.name_id(cd))
            {
                insert_dep(&mut entries[0].public, &ctx, (ctx.name_id(cd), new_id, cd));
            }
        }
        dedupe.entry(pkg).or_default().push(DedupeInfo {
            entry: new_id,
            dep: cur_dep,
            peers: cur_peers,
        });
        for &child in &nodes[node_id as usize].nodes {
            equeue.push_back((child, Some(new_id)));
        }
    }
    // Keys.
    let keys: Vec<String> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let p = &graph.packages[nodes[e.node as usize].pkg as usize];
            store_key::with_peer_hash(
                store_key::store_key(&p.name, &p.resolution, &filter.project_basename),
                entry_peer_hash[i],
            )
        })
        .collect();

    // Global virtual store hashes (bun `install_isolated_packages`).
    let n = entries.len();
    let mut hashes: Vec<u64> = vec![0; n];
    if opts.global_store {
        let types_names: HashSet<&str> = graph
            .packages
            .iter()
            .map(|p| p.name.as_str())
            .filter(|n| n.starts_with("@types/"))
            .collect();
        let mut trusted_entries: Vec<usize> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            let node = &nodes[e.node as usize];
            let pkg = &graph.packages[node.pkg as usize];
            let trusted = (opts.trusted)(PackageId(node.pkg));
            let eligible = match &pkg.resolution {
                Resolution::Npm { .. }
                | Resolution::Git { .. }
                | Resolution::Github { .. }
                | Resolution::LocalTarball { .. }
                | Resolution::RemoteTarball { .. } => 'eligible: {
                    if trusted {
                        trusted_entries.push(i);
                        break 'eligible false;
                    }
                    if !graph.patched_dependencies.is_empty()
                        && graph
                            .patched_dependencies
                            .contains_key(&format!("{}@{}", pkg.name, pkg.resolution))
                    {
                        break 'eligible false;
                    }
                    if !types_names.is_empty() {
                        for peer in &node.peers {
                            let tn = types_package_name(&ctx.edge(peer.edge).name);
                            if types_names.contains(tn.as_str())
                                && !e.deps.iter().any(|(_, _, er)| ctx.edge(*er).name == tn)
                            {
                                break 'eligible false;
                            }
                        }
                    }
                    true
                }
                Resolution::Folder { .. } => {
                    if trusted {
                        trusted_entries.push(i);
                    }
                    false
                }
                _ => false,
            };
            hashes[i] = u64::from(eligible);
        }
        let mut seen = vec![false; n];
        while let Some(i) = trusted_entries.pop() {
            for (_, d, _) in &entries[i].deps {
                let d = *d as usize;
                if !seen[d] {
                    seen[d] = true;
                    hashes[d] = 0;
                    trusted_entries.push(d);
                }
            }
        }

        let hash_entry = |seed: u64, id: usize| -> Xxh3 {
            let mut h = Xxh3::with_seed(seed);
            h.update(&[GLOBAL_STORE_LAYOUT, u8::from(filter.no_verify)]);
            h.update(keys[id].as_bytes());
            h.update(&[0]);
            match graph.packages[nodes[entries[id].node as usize].pkg as usize]
                .resolution
                .integrity()
            {
                Some(i) => {
                    h.update(&[i.algorithm() as u8 + 1]);
                    h.update(i.digest());
                }
                None => h.update(&[0]),
            }
            h
        };
        let dep_name = |er: EdgeRef| ctx.edge(er).name.as_bytes();

        // Tarjan, iterative.
        let mut index = vec![u32::MAX; n];
        let mut low = vec![0u32; n];
        let mut on_stack = vec![false; n];
        let mut stack: Vec<u32> = Vec::new();
        let mut work: Vec<(u32, usize)> = Vec::new();
        let mut counter = 0u32;
        for root in 0..n {
            if index[root] != u32::MAX {
                continue;
            }
            work.push((root as u32, 0));
            while let Some(&(v, child)) = work.last() {
                let vi = v as usize;
                if child == 0 && index[vi] == u32::MAX {
                    index[vi] = counter;
                    low[vi] = counter;
                    counter += 1;
                    stack.push(v);
                    on_stack[vi] = true;
                }
                let deps = &entries[vi].deps;
                let mut c = child;
                let mut recursed = false;
                while c < deps.len() {
                    let w = deps[c].1 as usize;
                    c += 1;
                    if index[w] == u32::MAX {
                        if let Some(top) = work.last_mut() {
                            top.1 = c;
                        }
                        work.push((w as u32, 0));
                        recursed = true;
                        break;
                    } else if on_stack[w] {
                        low[vi] = low[vi].min(index[w]);
                    }
                }
                if recursed {
                    continue;
                }
                if low[vi] == index[vi] {
                    let start = stack
                        .iter()
                        .rposition(|&m| m == v)
                        .expect("v is on the stack");
                    let members: Vec<u32> = stack[start..].to_vec();
                    for &m in &members {
                        on_stack[m as usize] = false;
                    }
                    if members.len() == 1 {
                        let m = members[0] as usize;
                        if hashes[m] != 0 {
                            let mut sub = hash_entry(0x9E37_79B9_7F4A_7C15, m);
                            let mut poisoned = false;
                            for (_, d, er) in &entries[m].deps {
                                let dh = hashes[*d as usize];
                                if dh == 0 {
                                    poisoned = true;
                                    break;
                                }
                                sub.update(dep_name(*er));
                                sub.update(&[0]);
                                sub.update(&dh.to_le_bytes());
                            }
                            hashes[m] = if poisoned { 0 } else { sub.digest().max(1) };
                        }
                    } else {
                        let own: Vec<u64> = members
                            .iter()
                            .map(|&m| hash_entry(0, m as usize).digest())
                            .collect();
                        let mut subs: Vec<u64> = Vec::with_capacity(members.len());
                        let mut any_ineligible = false;
                        for (k, &m) in members.iter().enumerate() {
                            if hashes[m as usize] == 0 {
                                any_ineligible = true;
                            }
                            let mut sub = Xxh3::new();
                            sub.update(&own[k].to_le_bytes());
                            for (_, d, er) in &entries[m as usize].deps {
                                let in_scc = members.iter().position(|&o| o == *d);
                                let linked = match in_scc {
                                    Some(o) => own[o],
                                    None => hashes[*d as usize],
                                };
                                if linked == 0 {
                                    any_ineligible = true;
                                }
                                sub.update(dep_name(*er));
                                sub.update(&[0, u8::from(in_scc.is_some())]);
                                sub.update(&linked.to_le_bytes());
                            }
                            subs.push(sub.digest());
                        }
                        subs.sort_unstable();
                        let mut h = Xxh3::with_seed(0x42A7_C15F_9E37_79B9);
                        for s in &subs {
                            h.update(&s.to_le_bytes());
                        }
                        let fin = if any_ineligible { 0 } else { h.digest().max(1) };
                        for &m in &members {
                            hashes[m as usize] = fin;
                        }
                    }
                    stack.truncate(start);
                }
                work.pop();
                if let Some(&(p, _)) = work.last() {
                    let pi = p as usize;
                    low[pi] = low[pi].min(low[vi]);
                }
            }
        }
    }

    // Output.
    let out_entries: Vec<Entry> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| Entry {
            package: PackageId(nodes[e.node as usize].pkg),
            key: keys[i].clone(),
            deps: e
                .deps
                .iter()
                .map(|(_, d, er)| (ctx.edge(*er).name.clone(), *d))
                .collect(),
            global_hash: (opts.global_store && hashes[i] != 0).then_some(hashes[i]),
        })
        .collect();
    let mut roots = Vec::new();
    let mut seen_roots: HashSet<u32> = HashSet::new();
    for e in &out_entries {
        let r = &graph.packages[e.package.index()].resolution;
        if matches!(r, Resolution::Root | Resolution::Workspace { .. })
            && seen_roots.insert(e.package.0)
        {
            roots.push((e.package, e.deps.clone()));
        }
    }
    let mut hidden_hoist: Vec<(String, u32)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let node = &nodes[e.node as usize];
        if entry_hoisted[i]
            && storable(node.pkg)
            && let Some(d) = node.dep
        {
            hidden_hoist.push((ctx.edge(d).name.clone(), i as u32));
        }
    }
    for (entry, er) in &hidden_keys {
        hidden_hoist.push((ctx.edge(*er).name.clone(), *entry));
    }
    hidden_hoist.sort();
    let public_hoist = entries
        .first()
        .map(|r| {
            r.public
                .iter()
                .map(|(_, d, er)| (ctx.edge(*er).name.clone(), *d))
                .collect()
        })
        .unwrap_or_default();
    IsolatedLayout {
        entries: out_entries,
        roots,
        hidden_hoist,
        public_hoist,
    }
}

/// Part of every global-store hash; bump it when what an entry contains changes.
const GLOBAL_STORE_LAYOUT: u8 = 1;

/// The DefinitelyTyped name: `react` -> `@types/react`, `@s/p` -> `@types/s__p`.
fn types_package_name(name: &str) -> String {
    let bare = name.strip_prefix('@').unwrap_or(name);
    format!("@types/{}", bare.replace('/', "__"))
}

fn insert_peer(ctx: &Ctx<'_>, peers: &mut Vec<Peer>, peer: Peer) {
    if peers.iter().any(|p| p.pkg == peer.pkg) {
        return;
    }
    let name = ctx.pkg_name(peer.pkg);
    let pos = peers.partition_point(|p| {
        let n = ctx.pkg_name(p.pkg);
        n < name || (n == name && p.pkg < peer.pkg)
    });
    peers.insert(pos, peer);
}
