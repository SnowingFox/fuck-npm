//! Dependency-order gating: a package's scripts run after the scripts of every package it
//! (transitively) depends on, and the root's run last (bun: the hoisted installer waits for
//! the trees below, the isolated installer for each entry's dependencies, and the root's
//! scripts run after the install).

use std::collections::HashMap;

use fnpm_types::{LockGraph, PackageId, Resolution};

use crate::Job;

/// For each job, the indices of the jobs it waits for: the nearest jobs reachable through
/// dependency edges (packages without scripts are looked through). Jobs in one dependency
/// cycle do not wait for each other. Root jobs wait for every other job, and workspace jobs for
/// every job of a registry, folder or other non-project package that does not wait for them
/// (bun runs the scripts of the members after those of the packages).
pub fn dependencies(graph: &LockGraph, jobs: &[Job]) -> Vec<Vec<usize>> {
    let mut jobs_of: HashMap<PackageId, Vec<usize>> = HashMap::new();
    for (i, job) in jobs.iter().enumerate() {
        if job.package.index() < graph.len() {
            jobs_of.entry(job.package).or_default().push(i);
        }
    }
    let scc = strongly_connected(graph);
    let mut stamp = vec![u32::MAX; graph.len()];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); jobs.len()];
    let mut stack: Vec<PackageId> = Vec::new();

    for (i, job) in jobs.iter().enumerate() {
        let start = job.package;
        if start.index() >= graph.len() {
            continue;
        }
        if matches!(graph.packages[start.index()].resolution, Resolution::Root) {
            out[i] = (0..jobs.len())
                .filter(|&j| {
                    j != i
                        && !matches!(
                            graph.get(jobs[j].package).map(|p| &p.resolution),
                            Some(Resolution::Root)
                        )
                })
                .collect();
            continue;
        }
        let mark = i as u32;
        stamp[start.index()] = mark;
        stack.clear();
        stack.push(start);
        let mut deps: Vec<usize> = Vec::new();
        while let Some(p) = stack.pop() {
            for edge in &graph.packages[p.index()].dependencies {
                let Some(t) = edge.target else { continue };
                if t.index() >= graph.len() || stamp[t.index()] == mark {
                    continue;
                }
                stamp[t.index()] = mark;
                if let Some(js) = jobs_of.get(&t)
                    && scc[t.index()] != scc[start.index()]
                {
                    deps.extend(js.iter().copied().filter(|&j| j != i));
                    continue;
                }
                stack.push(t);
            }
        }
        deps.sort_unstable();
        deps.dedup();
        out[i] = deps;
    }
    let is_project = |j: usize| {
        matches!(
            graph.get(jobs[j].package).map(|p| &p.resolution),
            Some(Resolution::Root | Resolution::Workspace { .. })
        )
    };
    let members: Vec<usize> = (0..jobs.len())
        .filter(|&i| {
            matches!(
                graph.get(jobs[i].package).map(|p| &p.resolution),
                Some(Resolution::Workspace { .. })
            )
        })
        .collect();
    if !members.is_empty() && members.len() < jobs.len() {
        let mut waiters: Vec<Vec<usize>> = vec![Vec::new(); jobs.len()];
        for (j, deps) in out.iter().enumerate() {
            for &d in deps {
                waiters[d].push(j);
            }
        }
        for &i in &members {
            // Every job that already waits for this member, directly or not.
            let mut waits = vec![false; jobs.len()];
            let mut todo = vec![i];
            while let Some(x) = todo.pop() {
                for &w in &waiters[x] {
                    if !waits[w] {
                        waits[w] = true;
                        todo.push(w);
                    }
                }
            }
            let extra: Vec<usize> = (0..jobs.len())
                .filter(|&j| j != i && !is_project(j) && !waits[j])
                .collect();
            for &j in &extra {
                waiters[j].push(i);
            }
            out[i].extend(extra);
            out[i].sort_unstable();
            out[i].dedup();
        }
    }
    out
}

/// Tarjan's algorithm, iterative. Returns the component id of each package.
fn strongly_connected(graph: &LockGraph) -> Vec<u32> {
    let n = graph.len();
    const UNSET: u32 = u32::MAX;
    let mut index = vec![UNSET; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut comp = vec![UNSET; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0u32;
    let mut next_comp = 0u32;
    // (node, next edge position)
    let mut call: Vec<(usize, usize)> = Vec::new();
    let targets = |v: usize| -> Vec<usize> {
        graph.packages[v]
            .dependencies
            .iter()
            .filter_map(|e| e.target)
            .map(PackageId::index)
            .filter(|&t| t < n)
            .collect()
    };
    let adjacency: Vec<Vec<usize>> = (0..n).map(targets).collect();

    for root in 0..n {
        if index[root] != UNSET {
            continue;
        }
        call.push((root, 0));
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(top) = call.last_mut() {
            let v = top.0;
            if let Some(&w) = adjacency[v].get(top.1) {
                top.1 += 1;
                if index[w] == UNSET {
                    index[w] = next_index;
                    low[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    call.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            call.pop();
            if let Some(&(parent, _)) = call.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    comp[w] = next_comp;
                    if w == v {
                        break;
                    }
                }
                next_comp += 1;
            }
        }
    }
    comp
}

/// Groups jobs into levels from `deps` (for callers that want [`crate::run_jobs`]): a job's
/// level is one more than the highest level among the jobs it waits for. Cycles in `deps`
/// are broken arbitrarily.
pub fn levels(jobs: Vec<Job>, deps: &[Vec<usize>]) -> Vec<Vec<Job>> {
    let n = jobs.len();
    let mut level: Vec<Option<usize>> = vec![None; n];
    let mut visiting = vec![false; n];
    fn visit(
        i: usize,
        deps: &[Vec<usize>],
        level: &mut [Option<usize>],
        visiting: &mut [bool],
    ) -> usize {
        if let Some(l) = level[i] {
            return l;
        }
        if visiting[i] {
            return 0;
        }
        visiting[i] = true;
        let n = level.len();
        let mut l = 0;
        for &d in deps.get(i).map_or(&[][..], Vec::as_slice) {
            if d < n {
                l = l.max(visit(d, deps, level, visiting) + 1);
            }
        }
        visiting[i] = false;
        level[i] = Some(l);
        l
    }
    for i in 0..n {
        visit(i, deps, &mut level, &mut visiting);
    }
    let max = level.iter().flatten().copied().max().map_or(0, |m| m + 1);
    let mut out: Vec<Vec<Job>> = (0..max).map(|_| Vec::new()).collect();
    for (i, job) in jobs.into_iter().enumerate() {
        out[level[i].unwrap_or(0)].push(job);
    }
    out
}

/// Inputs of [`plan_install`] beyond the graph.
pub struct PlanOptions<'a> {
    /// `--ignore-scripts` (any source): no job at all, nothing reported as blocked.
    pub ignore_scripts: bool,
    pub platform: fnpm_types::Platform,
    /// Packages `--trust <names>` trusts for this install ([`crate::trust::trusted_from_requests`]).
    pub trusted_from_requests: &'a std::collections::BTreeSet<PackageId>,
    /// The registry URL serving a package name (default-list trust compares tarball URLs).
    pub registry_for: &'a dyn Fn(&str) -> &'a str,
    /// Packages that are installed (`LockGraph::reachable` after omit/platform filtering).
    /// `None`: every package.
    pub installed: Option<&'a [bool]>,
    /// The workspaces this install links as members (a `--filter` selection). `None`: all of
    /// them. Another member is only a dependency, which an optional edge may leave out.
    pub members: Option<&'a [PackageId]>,
}

/// The jobs of an install and what was left out.
#[derive(Debug, Default)]
pub struct InstallPlan {
    pub jobs: Vec<Job>,
    /// For [`crate::run_dag`].
    pub deps: Vec<Vec<usize>>,
    /// Untrusted packages with scripts (`blockedScripts`, `fnpm pm untrusted`).
    pub blocked: Vec<crate::trust::Blocked>,
    /// Trusted packages whose scripts the optimizer skips.
    pub ignored: Vec<PackageId>,
    /// `(package, platform package)` pairs whose bins the linker points at the platform package.
    pub native_binlinks: Vec<(PackageId, PackageId)>,
}

impl InstallPlan {
    /// The number of blocked scripts, for `InstallSummary::blocked_scripts`.
    pub fn blocked_scripts(&self) -> u32 {
        self.blocked.iter().map(|b| b.scripts.len() as u32).sum()
    }
}

/// Selects the jobs of an install: the root's and the workspaces' own scripts, and those of
/// trusted dependencies that the optimizer does not skip. `locate` returns where a package
/// is installed (the project directory for the root); packages it returns `None` for are not
/// installed. Scripts are read from each package's package.json on disk.
pub fn plan_install(
    graph: &LockGraph,
    policy: &crate::TrustPolicy,
    optimizer: &crate::Optimizer,
    opts: &PlanOptions<'_>,
    locate: &dyn Fn(PackageId) -> Option<std::path::PathBuf>,
) -> std::io::Result<InstallPlan> {
    use crate::optimizer::Decision;
    use crate::scripts::{PackageScripts, ScriptSource};

    let mut plan = InstallPlan::default();
    if opts.ignore_scripts || graph.is_empty() {
        return Ok(plan);
    }
    let optional = optional_only(graph, opts.members);
    for id in graph.ids() {
        if opts
            .installed
            .is_some_and(|inst| !inst.get(id.index()).copied().unwrap_or(false))
        {
            continue;
        }
        let pkg = &graph.packages[id.index()];
        let source = ScriptSource::from_resolution(&pkg.resolution);
        // Registry manifests say whether there are install scripts; local and git packages
        // are read from disk.
        if matches!(pkg.resolution, Resolution::Npm { .. }) && !pkg.meta.needs_scripts() {
            continue;
        }
        let Some(dir) = locate(id) else { continue };
        let trusted = opts.trusted_from_requests.contains(&id)
            || policy.is_trusted_with_registry(graph, id, opts.registry_for);
        if !trusted {
            if matches!(pkg.resolution, Resolution::Folder { .. })
                && !crate::trust::is_trusted_folder_package(graph, id)
            {
                continue;
            }
            match PackageScripts::read(&dir) {
                Ok(scripts) => {
                    let events = scripts.blocked_events(source, &dir);
                    if !events.is_empty() {
                        plan.blocked.push(crate::trust::Blocked {
                            package: id,
                            name: pkg.name.clone(),
                            version: pkg.version.clone(),
                            scripts: events,
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            continue;
        }
        if source.is_dependency() {
            match optimizer.decide(graph, id, &opts.platform) {
                Decision::Run => {}
                Decision::Skip => {
                    plan.ignored.push(id);
                    continue;
                }
                Decision::NativeBinlink { platform_package } => {
                    plan.ignored.push(id);
                    plan.native_binlinks.push((id, platform_package));
                    continue;
                }
            }
        }
        let scripts = match PackageScripts::read(&dir) {
            Ok(s) => s,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && !matches!(pkg.resolution, Resolution::Root) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        // Only a workspace gets here without being trusted by name.
        let gyp_allowed = source != ScriptSource::Workspace
            || policy.has_trusted_dependency(graph, id, opts.registry_for)
            || opts.trusted_from_requests.contains(&id);
        let entries = scripts.entries(source, gyp_allowed && scripts.wants_default_node_gyp(&dir));
        if entries.is_empty() {
            continue;
        }
        plan.jobs.push(Job {
            package: id,
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            package_json: dir.join("package.json"),
            dir,
            scripts: entries,
            optional: optional[id.index()],
            // bun runs only the root's scripts in the foreground (after the install).
            foreground: source == ScriptSource::Root,
        });
    }
    plan.deps = dependencies(graph, &plan.jobs);
    Ok(plan)
}

/// Packages reached only through optional dependency edges. A workspace counts only when it is
/// not one of `members`.
fn optional_only(graph: &LockGraph, members: Option<&[PackageId]>) -> Vec<bool> {
    let mut any_edge = vec![false; graph.len()];
    let mut required = vec![false; graph.len()];
    for pkg in &graph.packages {
        for edge in &pkg.dependencies {
            if let Some(t) = edge.target.filter(|t| t.index() < graph.len()) {
                any_edge[t.index()] = true;
                if !edge.behavior.is_optional() && !edge.behavior.is_optional_peer() {
                    required[t.index()] = true;
                }
            }
        }
    }
    (0..graph.len())
        .map(|i| {
            any_edge[i]
                && !required[i]
                && match graph.packages[i].resolution {
                    Resolution::Root => false,
                    Resolution::Workspace { .. } => {
                        members.is_some_and(|m| !m.contains(&PackageId(i as u32)))
                    }
                    _ => true,
                }
        })
        .collect()
}
