//! The dependency graph between workspace packages: relation selectors (`foo...`), publish
//! order (bun `WorkspaceGraph`) and `run --filter` levels (bun `filter_run.rs`).

use std::collections::{HashMap, VecDeque};

use crate::PackageJson;

/// Adjacency lists over candidate indices. `dependencies[i]` are the packages `i` depends on;
/// `dependents` is the reverse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceGraph {
    pub dependencies: Vec<Vec<u32>>,
    pub dependents: Vec<Vec<u32>>,
}

impl WorkspaceGraph {
    /// A graph over `n` nodes from `(from, to)` edges ("`from` depends on `to`").
    pub fn from_edges(n: usize, edges: impl IntoIterator<Item = (u32, u32)>) -> Self {
        let mut graph = WorkspaceGraph {
            dependencies: vec![Vec::new(); n],
            dependents: vec![Vec::new(); n],
        };
        for (from, to) in edges {
            graph.dependencies[from as usize].push(to);
            graph.dependents[to as usize].push(from);
        }
        graph
    }

    /// Edges from dependency names: a name maps to the first node with that name; empty names
    /// and self-references are ignored (bun `from_dependency_names`).
    pub fn from_dependency_names<'a, D, I>(names: &[&str], mut dependency_names: D) -> Self
    where
        D: FnMut(usize) -> I,
        I: IntoIterator<Item = &'a str>,
    {
        let mut by_name: HashMap<&str, u32> = HashMap::with_capacity(names.len());
        for (i, &name) in names.iter().enumerate() {
            if !name.is_empty() {
                by_name.entry(name).or_insert(i as u32);
            }
        }
        let mut edges = Vec::new();
        for from in 0..names.len() {
            for dep in dependency_names(from) {
                if let Some(&to) = by_name.get(dep)
                    && to as usize != from
                {
                    edges.push((from as u32, to));
                }
            }
        }
        Self::from_edges(names.len(), edges)
    }

    /// The number of nodes.
    pub fn len(&self) -> usize {
        self.dependencies.len()
    }

    /// Whether the graph has no nodes.
    pub fn is_empty(&self) -> bool {
        self.dependencies.is_empty()
    }

    /// The graph induced on `nodes`, renumbered by position in `nodes`.
    pub fn subgraph(&self, nodes: &[usize]) -> Self {
        let mut pos: HashMap<usize, u32> = HashMap::with_capacity(nodes.len());
        for (k, &n) in nodes.iter().enumerate() {
            pos.entry(n).or_insert(k as u32);
        }
        let mut edges = Vec::new();
        for (k, &n) in nodes.iter().enumerate() {
            for &d in &self.dependencies[n] {
                if let Some(&to) = pos.get(&(d as usize)) {
                    edges.push((k as u32, to));
                }
            }
        }
        Self::from_edges(nodes.len(), edges)
    }

    /// Node indices, each after those it depends on. A cycle is broken at one of its members:
    /// follow unmet dependencies from the first unplaced node until one repeats (bun
    /// `dependency_order`).
    pub fn dependency_order(&self) -> Vec<u32> {
        let n = self.len();
        let mut remaining: Vec<usize> = self.dependencies.iter().map(Vec::len).collect();
        let mut order = Vec::with_capacity(n);
        let mut queued = vec![false; n];
        let mut queue = VecDeque::new();
        for i in 0..n {
            if remaining[i] == 0 {
                queued[i] = true;
                queue.push_back(i as u32);
            }
        }
        loop {
            while let Some(u) = queue.pop_front() {
                order.push(u);
                for &v in &self.dependents[u as usize] {
                    if queued[v as usize] {
                        continue;
                    }
                    remaining[v as usize] -= 1;
                    if remaining[v as usize] == 0 {
                        queued[v as usize] = true;
                        queue.push_back(v);
                    }
                }
            }
            if order.len() == n {
                return order;
            }
            let mut on_walk = vec![false; n];
            let Some(mut cur) = (0..n).find(|&i| !queued[i]) else {
                return order;
            };
            while !on_walk[cur] {
                on_walk[cur] = true;
                match self.dependencies[cur]
                    .iter()
                    .map(|&d| d as usize)
                    .find(|&d| !queued[d])
                {
                    Some(next) => cur = next,
                    None => break,
                }
            }
            queued[cur] = true;
            queue.push_back(cur as u32);
        }
    }

    /// Strongly connected components that form cycles (more than one node, or a node that
    /// depends on itself), each sorted ascending, ordered by their smallest node.
    pub fn cycles(&self) -> Vec<Vec<usize>> {
        let mut out: Vec<Vec<usize>> = tarjan(&self.dependencies)
            .into_iter()
            .filter(|c| c.len() > 1 || self.dependencies[c[0]].contains(&(c[0] as u32)))
            .map(|mut c| {
                c.sort_unstable();
                c
            })
            .collect();
        out.sort();
        out
    }

    /// Kahn levels: level 0 has no dependencies, level k depends only on earlier levels. Nodes
    /// keep their index order inside a level. `None` if there is a cycle.
    pub fn levels(&self) -> Option<Vec<Vec<usize>>> {
        let n = self.len();
        let mut remaining: Vec<usize> = self.dependencies.iter().map(Vec::len).collect();
        let mut current: Vec<usize> = (0..n).filter(|&i| remaining[i] == 0).collect();
        let mut levels = Vec::new();
        let mut placed = 0;
        while !current.is_empty() {
            placed += current.len();
            let mut next = Vec::new();
            for &u in &current {
                for &v in &self.dependents[u] {
                    remaining[v as usize] -= 1;
                    if remaining[v as usize] == 0 {
                        next.push(v as usize);
                    }
                }
            }
            next.sort_unstable();
            levels.push(std::mem::replace(&mut current, next));
        }
        (placed == n).then_some(levels)
    }
}

/// Iterative Tarjan SCC.
fn tarjan(adj: &[Vec<u32>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut next_index = 0;
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        let mut call: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(top) = call.len().checked_sub(1) {
            let (v, ei) = call[top];
            if ei < adj[v].len() {
                let w = adj[v][ei] as usize;
                call[top].1 += 1;
                if index[w] == usize::MAX {
                    index[w] = next_index;
                    low[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    call.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                call.pop();
                if let Some(&(parent, _)) = call.last() {
                    low[parent] = low[parent].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    out.push(comp);
                }
            }
        }
    }
    out
}

/// The names `run --filter` orders by: keys of `dependencies`, `devDependencies` and
/// `optionalDependencies` (bun reads these three groups for the run-side package.json).
pub fn run_dependency_names(pkg: &PackageJson) -> impl Iterator<Item = &str> {
    pkg.dependencies
        .iter()
        .chain(&pkg.dev_dependencies)
        .chain(&pkg.optional_dependencies)
        .map(|(n, _)| n.as_str())
}

/// `run --filter` ordering (bun `filter_run.rs`): a package waits for every package of the
/// set whose name is one of its dependency names (all packages with that name). If the
/// resulting graph has a cycle, ordering is dropped: one level with every package. The second
/// value lists the cycles.
pub fn topo_levels_by_names<'a, D, I>(
    names: &[&str],
    mut dependency_names: D,
) -> (Vec<Vec<usize>>, Vec<Vec<usize>>)
where
    D: FnMut(usize) -> I,
    I: IntoIterator<Item = &'a str>,
{
    let n = names.len();
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let mut by_name: HashMap<&str, Vec<u32>> = HashMap::with_capacity(n);
    for (i, &name) in names.iter().enumerate() {
        by_name.entry(name).or_default().push(i as u32);
    }
    let mut edges = Vec::new();
    for from in 0..n {
        for dep in dependency_names(from) {
            if let Some(targets) = by_name.get(dep) {
                edges.extend(targets.iter().map(|&to| (from as u32, to)));
            }
        }
    }
    let graph = WorkspaceGraph::from_edges(n, edges);
    let cycles = graph.cycles();
    if !cycles.is_empty() {
        return (vec![(0..n).collect()], cycles);
    }
    (
        graph.levels().unwrap_or_else(|| vec![(0..n).collect()]),
        Vec::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_and_levels() {
        // 0 -> 1 -> 2, 3 alone
        let g = WorkspaceGraph::from_edges(4, [(0, 1), (1, 2)]);
        assert_eq!(g.dependency_order(), vec![2, 3, 1, 0]);
        assert_eq!(g.levels(), Some(vec![vec![2, 3], vec![1], vec![0]]));
        assert!(g.cycles().is_empty());
    }

    #[test]
    fn cycle_is_broken() {
        // 0 <-> 1, 2 -> 0
        let g = WorkspaceGraph::from_edges(3, [(0, 1), (1, 0), (2, 0)]);
        let order = g.dependency_order();
        assert_eq!(order.len(), 3);
        assert_eq!(*order.last().unwrap(), 2);
        assert_eq!(g.cycles(), vec![vec![0, 1]]);
        assert_eq!(g.levels(), None);
        // a cycle of length 3 (bun "detect cycle of length > 2")
        let g = WorkspaceGraph::from_edges(3, [(0, 1), (1, 2), (2, 0)]);
        assert_eq!(g.cycles(), vec![vec![0, 1, 2]]);
        assert_eq!(g.dependency_order().len(), 3);
        // self loop
        let g = WorkspaceGraph::from_edges(2, [(1, 1)]);
        assert_eq!(g.cycles(), vec![vec![1]]);
    }

    #[test]
    fn names() {
        let g = WorkspaceGraph::from_dependency_names(&["a", "b", "", "a"], |i| match i {
            0 => vec!["b", "a", "zzz"],
            1 => vec![""],
            _ => vec!["a"],
        });
        // The second "a" maps to the first one, which is not itself.
        assert_eq!(g.dependencies, vec![vec![1], vec![], vec![0], vec![0]]);
        assert_eq!(g.subgraph(&[2, 0]).dependencies, vec![vec![1], vec![]]);
    }

    #[test]
    fn run_levels() {
        // bun "respect dependency order": dep1 depends on dep0
        let (levels, cycles) = topo_levels_by_names(&["dep1", "dep0"], |i| {
            if i == 0 { vec!["dep0"] } else { vec![] }
        });
        assert_eq!(levels, vec![vec![1], vec![0]]);
        assert!(cycles.is_empty());
        // bun "detect cycle of length > 2": two packages named dep1
        let (levels, cycles) = topo_levels_by_names(&["dep0", "dep1", "dep1"], |i| match i {
            0 => vec!["dep1"],
            1 => vec!["dep2"],
            _ => vec!["dep0"],
        });
        assert_eq!(levels, vec![vec![0, 1, 2]]);
        assert_eq!(cycles, vec![vec![0, 2]]);
        assert_eq!(
            topo_levels_by_names(&[], |_| Vec::<&str>::new()),
            (vec![], vec![])
        );
    }
}
