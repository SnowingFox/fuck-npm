//! Turns `InstallOptions` into what the resolver gets: the mode, `add` requests, `remove`
//! removals and the `update` targets (bun `UpdateRequest.rs`, `add_remove_with_filter.rs`,
//! `updatePackageJSONAndInstall.rs`).

use std::collections::HashSet;
use std::path::Path;

use fnpm_config::Config;
use fnpm_resolver::{Mode, Request};
use fnpm_types::{DependencyGroup, Event, EventSink, LockGraph, PackageId};
use fnpm_workspace::{PackageJson, RootSelection, SelectOptions, Selector, Workspace};

use crate::options::{InstallCommand, InstallOptions};
use crate::spec::Positional;
use crate::{Error, Result};

use super::edits;

/// An `add` positional and the request it became.
#[derive(Debug, Clone)]
pub(crate) struct Add {
    pub positional: Positional,
    /// Index into [`Planned::requests`].
    pub request: usize,
    /// `--catalog`: the catalog to record the version in (`""` is the default one).
    pub catalog: Option<String>,
    /// The catalog already had an entry (it is kept).
    pub catalog_existing: Option<String>,
}

/// A direct dependency `fnpm update` may rewrite in package.json.
#[derive(Debug, Clone)]
pub(crate) struct UpdateTarget {
    pub workspace: String,
    pub name: String,
    /// The declared literal before the update.
    pub original: String,
}

/// The resolver input of one command.
#[derive(Debug)]
pub(crate) struct Planned {
    pub mode: Mode,
    pub requests: Vec<Request>,
    pub removals: Vec<(String, String)>,
    pub adds: Vec<Add>,
    pub updates: Vec<UpdateTarget>,
    /// `update --latest`.
    pub latest: bool,
    /// `--filter` / `-r`: the selected workspace paths (`None`: no selection).
    pub selected: Option<Vec<String>>,
    /// `update <name|pattern>`: the ones no selected package.json declares. The install looks
    /// them up in the lockfile (a transitive dependency is updated in place there).
    pub unmatched: Vec<String>,
    /// `update`: the workspace paths it targets, and whether `-r` / `--filter` chose them.
    pub scope: (Vec<String>, bool),
}

fn manifest_of<'a>(ws: &'a Workspace, path: &str) -> &'a PackageJson {
    if path.is_empty() {
        &ws.root
    } else {
        ws.member_by_path(path).map_or(&ws.root, |m| &m.manifest)
    }
}

/// The group that declares `name`, preferring a non-peer group.
fn declared_group(pkg: &PackageJson, name: &str) -> Option<DependencyGroup> {
    let has = |g: DependencyGroup| pkg.group(g).iter().any(|(n, _)| n == name);
    [
        DependencyGroup::Prod,
        DependencyGroup::Dev,
        DependencyGroup::Optional,
        DependencyGroup::Peer,
    ]
    .into_iter()
    .find(|g| has(*g))
}

fn declared_literal<'a>(pkg: &'a PackageJson, name: &str) -> Option<&'a str> {
    DependencyGroup::ALL.into_iter().find_map(|g| {
        pkg.group(g)
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    })
}

/// `*` glob over package names (bun `update` patterns).
pub(crate) fn name_matches(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = name;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else {
                return false;
            };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    rest.is_empty()
}

/// The workspace paths a command targets: `--filter` (or `-r` for update), else the member
/// containing `cwd`, else the root.
fn targets(
    opts: &InstallOptions,
    ws: &Workspace,
    cwd: &Path,
    events: &dyn EventSink,
) -> Result<(Vec<String>, bool)> {
    let path_of = |i: usize| {
        if i == usize::MAX {
            String::new()
        } else {
            ws.members[i].path.clone()
        }
    };
    if !opts.filter.is_empty() {
        let selectors = Selector::parse_all(&opts.filter)?;
        let root = match opts.command {
            InstallCommand::Add | InstallCommand::Remove => RootSelection::ExplicitOnly,
            _ => RootSelection::Implicit,
        };
        let sel = fnpm_workspace::select_with(
            ws,
            &selectors,
            cwd,
            &SelectOptions {
                root,
                ..SelectOptions::default()
            },
        );
        // Add/remove/update need an edit target. A plain install still resolves the whole
        // lockfile, warns, and links nothing when its selection is empty.
        let install_only = opts.command == InstallCommand::Install && opts.packages.is_empty();
        if sel.selected.is_empty() && !install_only {
            return Err(Error::usage(fnpm_workspace::unmatched_message(
                &opts.filter,
            )));
        }
        let unmatched: Vec<&str> = if sel.selected.is_empty() {
            opts.filter.iter().map(String::as_str).collect()
        } else {
            sel.unmatched_patterns
                .iter()
                .filter_map(|&i| opts.filter.get(i).map(String::as_str))
                .collect()
        };
        if !unmatched.is_empty() {
            events.emit(Event::warn(
                fnpm_workspace::unmatched_message(&unmatched),
                None,
            ));
        }
        return Ok((sel.selected.into_iter().map(path_of).collect(), true));
    }
    if opts.command == InstallCommand::Update && opts.recursive {
        let mut all = vec![String::new()];
        all.extend(ws.members.iter().map(|m| m.path.clone()));
        return Ok((all, true));
    }
    let here = ws
        .member_for_dir(cwd)
        .map(|m| m.path.clone())
        .unwrap_or_default();
    Ok((vec![here], false))
}

/// Builds the resolver input.
pub(crate) fn plan(
    opts: &InstallOptions,
    config: &Config,
    ws: &Workspace,
    cwd: &Path,
    events: &dyn EventSink,
) -> Result<Planned> {
    let mut planned = Planned {
        mode: if config.frozen_lockfile {
            Mode::Frozen
        } else {
            Mode::Normal
        },
        requests: Vec::new(),
        removals: Vec::new(),
        adds: Vec::new(),
        updates: Vec::new(),
        latest: opts.latest,
        selected: None,
        unmatched: Vec::new(),
        scope: (Vec::new(), false),
    };
    let is_add = opts.command == InstallCommand::Add
        || (opts.command == InstallCommand::Install && !opts.packages.is_empty());
    match opts.command {
        _ if is_add => {
            let (targets, filtered) = targets(opts, ws, cwd, events)?;
            if filtered {
                planned.selected = Some(targets.clone());
            }
            if opts.catalog.is_some() && !edits::catalog_supported(&ws.root.raw) {
                return Err(Error::usage(
                    "--catalog requires a \"workspaces\" field in the root package.json",
                ));
            }
            for input in &opts.packages {
                let positional = Positional::parse(input)?;
                for target in &targets {
                    let positional = if opts.pm.global {
                        positional.absolute_from(cwd)
                    } else {
                        positional.relative_to(cwd, &ws.root_dir.join(target))
                    };
                    let manifest = manifest_of(ws, target);
                    let name = positional.request_name();
                    if opts.only_missing && declared_group(manifest, &name).is_some() {
                        continue;
                    }
                    let group = opts
                        .group
                        .or_else(|| {
                            declared_group(manifest, &name).filter(|g| *g != DependencyGroup::Peer)
                        })
                        .unwrap_or(DependencyGroup::Prod);
                    let mut catalog_existing = None;
                    let spec = match &opts.catalog {
                        Some(cat) if !positional.has_version() => {
                            catalog_existing = edits::catalog_entry(&ws.root.raw, cat, &name);
                            catalog_existing
                                .clone()
                                .unwrap_or_else(|| positional.resolve_literal())
                        }
                        _ if positional.literal.is_empty() => String::new(),
                        _ => positional.resolve_literal(),
                    };
                    planned.adds.push(Add {
                        positional: positional.clone(),
                        request: planned.requests.len(),
                        catalog: opts.catalog.clone(),
                        catalog_existing,
                    });
                    planned.requests.push(Request {
                        workspace: target.clone(),
                        name,
                        spec,
                        group,
                    });
                }
            }
        }
        InstallCommand::Remove => {
            let (targets, filtered) = targets(opts, ws, cwd, events)?;
            if filtered {
                planned.selected = Some(targets.clone());
            }
            for input in &opts.packages {
                let name = input.trim();
                let mut found = false;
                for target in &targets {
                    if declared_group(manifest_of(ws, target), name).is_some() {
                        planned.removals.push((target.clone(), name.to_owned()));
                        found = true;
                    }
                }
                if !found {
                    events.emit(Event::warn(
                        format!("\"{name}\" is not in a package.json file"),
                        Some("NOT_A_DEPENDENCY"),
                    ));
                }
            }
        }
        InstallCommand::Update => {
            let (targets, selected) = targets(opts, ws, cwd, events)?;
            planned.scope = (targets.clone(), selected);
            if selected {
                planned.selected = Some(targets.clone());
            }
            let groups: Vec<DependencyGroup> = {
                let g = opts.update_groups;
                DependencyGroup::ALL
                    .into_iter()
                    .filter(|x| *x != DependencyGroup::Peer)
                    .filter(|x| match x {
                        DependencyGroup::Dev => !g.prod || g.dev,
                        DependencyGroup::Prod => !g.dev || g.prod,
                        DependencyGroup::Optional => !g.no_optional && !(g.dev && !g.prod),
                        DependencyGroup::Peer => false,
                    })
                    .collect()
            };
            let mut names: Vec<String> = Vec::new();
            let mut patterns: Vec<String> = Vec::new();
            // `!name` / `!glob` leave matching packages out of the update.
            let mut negations: Vec<String> = Vec::new();
            for input in &opts.packages {
                let input = input.trim();
                if let Some(negated) = input.strip_prefix('!') {
                    negations.push(negated.to_owned());
                    continue;
                }
                if input.contains('*') {
                    let scoped = input.strip_prefix('@').map_or(input, |rest| {
                        rest.split_once('/').map_or(rest, |(_, name)| name)
                    });
                    if scoped.contains('@') {
                        return Err(Error::usage(format!(
                            "a version cannot be combined with a pattern: {input}"
                        )));
                    }
                    patterns.push(input.to_owned());
                    continue;
                }
                let p = Positional::parse(input)?;
                match (&p.name, p.has_version()) {
                    (Some(name), true) => {
                        for target in &targets {
                            let m = manifest_of(ws, target);
                            if let Some(group) = declared_group(m, name) {
                                planned.requests.push(Request {
                                    workspace: target.clone(),
                                    name: name.clone(),
                                    spec: p.literal.clone(),
                                    group,
                                });
                                planned.adds.push(Add {
                                    positional: p.clone(),
                                    request: planned.requests.len() - 1,
                                    catalog: None,
                                    catalog_existing: None,
                                });
                            }
                        }
                    }
                    _ => patterns.push(p.input.clone()),
                }
            }
            let excluded = |name: &str| negations.iter().any(|pat| name_matches(pat, name));
            for target in &targets {
                let m = manifest_of(ws, target);
                for g in &groups {
                    for (name, literal) in m.group(*g) {
                        let wanted = if patterns.is_empty() {
                            opts.packages.len() == negations.len()
                        } else {
                            patterns.iter().any(|pat| name_matches(pat, name))
                        };
                        if !wanted || excluded(name) {
                            continue;
                        }
                        if !names.contains(name) {
                            names.push(name.clone());
                        }
                        planned.updates.push(UpdateTarget {
                            workspace: target.clone(),
                            name: name.clone(),
                            original: literal.clone(),
                        });
                    }
                }
            }
            for pat in &patterns {
                // A name declared only in peerDependencies is left as it is.
                let peer = targets.iter().any(|t| {
                    manifest_of(ws, t)
                        .group(DependencyGroup::Peer)
                        .iter()
                        .any(|(n, _)| name_matches(pat, n))
                });
                if !peer && !names.iter().any(|n| name_matches(pat, n)) {
                    planned.unmatched.push(pat.clone());
                }
            }
            if !negations.is_empty() && patterns.is_empty() && names.is_empty() {
                // Everything was left out: an update of nothing.
                names.push(String::new());
            }
            // An explicit name list keeps transitive dependencies locked (`--depth 0` and
            // patterns); a bare update re-resolves every direct dependency.
            let names = if patterns.is_empty()
                && opts.packages.is_empty()
                && opts.depth.is_none()
                && !selected
                && groups.len() == 3
            {
                Vec::new()
            } else {
                names
            };
            planned.mode = Mode::Update {
                names,
                latest: opts.latest,
            };
        }
        InstallCommand::Install => {
            if !opts.filter.is_empty() {
                let (targets, _) = targets(opts, ws, cwd, events)?;
                planned.selected = Some(targets);
            }
        }
        InstallCommand::Add => unreachable!("handled above"),
    }
    Ok(planned)
}

/// `update <name|pattern>` for what no targeted package.json declares: the packages of that
/// name in the lockfile that the targeted workspaces reach move within their dependents'
/// ranges (bun updates a transitive dependency in place). A name the lockfile lacks, or one
/// only other workspaces reach, is an error that names the fix.
pub(crate) fn update_from_lockfile(
    planned: &mut Planned,
    graph: Option<&LockGraph>,
    lockfile: &str,
    ws: &Workspace,
) -> Result<()> {
    if planned.unmatched.is_empty() {
        return Ok(());
    }
    let reach = |path: &str| -> HashSet<String> {
        let mut names = HashSet::new();
        let Some(graph) = graph else { return names };
        let start = if path.is_empty() {
            Some(PackageId::ROOT)
        } else {
            graph.workspaces.get(path).copied()
        };
        let importers: HashSet<PackageId> = graph.workspaces.values().copied().collect();
        let mut seen = HashSet::new();
        let mut stack: Vec<PackageId> = start.into_iter().collect();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(p) = graph.get(id) else { continue };
            for e in &p.dependencies {
                let Some(t) = e.target else { continue };
                if importers.contains(&t) || t == PackageId::ROOT {
                    continue;
                }
                if let Some(target) = graph.get(t) {
                    names.insert(target.name.clone());
                }
                stack.push(t);
            }
        }
        names
    };
    let (targets, by_flag) = planned.scope.clone();
    let mut scoped: HashSet<String> = HashSet::new();
    for t in &targets {
        scoped.extend(reach(t));
    }
    let mut extra: Vec<String> = Vec::new();
    for pat in std::mem::take(&mut planned.unmatched) {
        let in_lock: Vec<String> = graph
            .map(|g| {
                let mut v: Vec<String> = g
                    .packages
                    .iter()
                    .skip(1)
                    .filter(|p| {
                        !matches!(p.resolution, fnpm_types::Resolution::Workspace { .. })
                            && name_matches(&pat, &p.name)
                    })
                    .map(|p| p.name.clone())
                    .collect();
                v.sort();
                v.dedup();
                v
            })
            .unwrap_or_default();
        if in_lock.is_empty() {
            return Err(Error::usage(if pat.contains('*') {
                format!("no packages in {lockfile} match \"{pat}\"")
            } else {
                format!("\"{pat}\" is not in {lockfile}\n    fnpm add {pat}")
            }));
        }
        let hits: Vec<String> = in_lock.into_iter().filter(|n| scoped.contains(n)).collect();
        if hits.is_empty() {
            let scope = if by_flag {
                "the selected workspaces"
            } else {
                "this workspace"
            };
            let mut message =
                format!("\"{pat}\" is not a dependency of {scope}\n    fnpm update -r {pat}");
            let mut others: Vec<(String, String)> = Vec::new();
            others.push((String::new(), ws.root.name.clone().unwrap_or_default()));
            for m in &ws.members {
                others.push((m.path.clone(), m.manifest.name.clone().unwrap_or_default()));
            }
            for (path, name) in others {
                if targets.contains(&path) || !reach(&path).iter().any(|n| name_matches(&pat, n)) {
                    continue;
                }
                let label = if name.is_empty() { path } else { name };
                message.push_str(&format!("\n    fnpm update --filter {label} {pat}"));
            }
            return Err(Error::usage(message));
        }
        extra.extend(hits);
    }
    if let Mode::Update { names, .. } = &mut planned.mode {
        for n in extra {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    Ok(())
}

/// The declared literal of `name` in the package at `workspace`.
pub(crate) fn declared(ws: &Workspace, workspace: &str, name: &str) -> Option<String> {
    declared_literal(manifest_of(ws, workspace), name).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::name_matches;

    #[test]
    fn globs() {
        assert!(name_matches("react", "react"));
        assert!(!name_matches("react", "react-dom"));
        assert!(name_matches("react*", "react-dom"));
        assert!(name_matches("@types/*", "@types/node"));
        assert!(!name_matches("@types/*", "types"));
        assert!(name_matches("*-loader", "css-loader"));
        assert!(name_matches("a*b*c", "axxbyyc"));
        assert!(!name_matches("a*b*c", "axxbyy"));
    }
}
