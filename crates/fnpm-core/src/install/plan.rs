//! Turns `InstallOptions` into what the resolver gets: the mode, `add` requests, `remove`
//! removals and the `update` targets (bun `UpdateRequest.rs`, `add_remove_with_filter.rs`,
//! `updatePackageJSONAndInstall.rs`).

use std::collections::HashSet;
use std::path::Path;

use fnpm_config::Config;
use fnpm_resolver::{Mode, Request};
use fnpm_types::spec::DependencySpec;
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
    /// The range a target declared, which seeds a new catalog entry as written.
    pub catalog_seed: Option<String>,
    /// The request was resolved again with the range it writes, which links the workspace
    /// of that name.
    pub relinked: bool,
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
    /// A bare `update` limited to some workspaces: what they reach in the lockfile moves too.
    pub bare_scope: bool,
    /// `update <glob>` and `update !<name|glob>`: matching transitive packages move (or stay).
    pub globs: Vec<String>,
    pub negations: Vec<String>,
    /// `update` whose selectors left no entry: `(entries checked, what left them out)`, for
    /// bun's `Checked N dependencies, none selected by --dev (no changes)`.
    pub none_selected: Option<(u32, String)>,
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

/// Why `--catalog` cannot record `p`: a workspace package (by name or `workspace:`) or a
/// local path, which the catalog would read from the workspace root.
fn catalog_refusal(p: &Positional, ws: &Workspace) -> Result<()> {
    let refuse = |why: String| Err(Error::usage(format!("--catalog cannot add {why}")));
    match &p.spec {
        DependencySpec::Workspace { .. } => {
            refuse(format!("a workspace package, but got \"{}\"", p.input))
        }
        // An absolute path means the same from the root: it is cataloged as written.
        DependencySpec::Folder { path } | DependencySpec::Symlink { path }
            if !Path::new(path.trim_start_matches("file:")).is_absolute() =>
        {
            refuse(format!(
                "\"{}\": a local path in the catalog would resolve from the workspace root, not from the package that added it",
                p.input
            ))
        }
        DependencySpec::Npm {
            is_alias: false, ..
        }
        | DependencySpec::DistTag { .. } => {
            let name = p.request_name();
            if let Some(member) = ws.member(&name) {
                refuse(format!(
                    "a workspace package, but \"{name}\" is the workspace at {}",
                    member.path
                ))
            } else if ws.root.name.as_deref() == Some(name.as_str()) {
                refuse(format!(
                    "a workspace package, but \"{name}\" is the workspace root"
                ))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
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
        bare_scope: false,
        globs: Vec::new(),
        negations: Vec::new(),
        none_selected: None,
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
            if opts.catalog.is_some() {
                // bun refuses the whole add before it resolves anything.
                for input in &opts.packages {
                    catalog_refusal(&Positional::parse(input)?, ws)?;
                }
            }
            for input in &opts.packages {
                let positional = Positional::parse(input)?;
                // `--catalog` without a version: the first selected target that declares the
                // name with a range seeds a new entry with that range, as written.
                let seed = opts
                    .catalog
                    .as_ref()
                    .filter(|_| !positional.has_version() && !opts.only_missing)
                    .and_then(|_| {
                        let name = positional.request_name();
                        targets.iter().find_map(|t| {
                            declared_literal(manifest_of(ws, t), &name)
                                .filter(|l| {
                                    !l.starts_with("catalog:") && !l.starts_with("workspace:")
                                })
                                .map(str::to_owned)
                        })
                    });
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
                    // pnpm's rule for a bare `--catalog` add over a range the target declares.
                    if let Some(cat) = opts.catalog.as_ref().filter(|_| !positional.has_version())
                        && let Some(declared) = declared_literal(manifest, &name)
                            .filter(|l| !l.starts_with("catalog:") && !l.starts_with("workspace:"))
                    {
                        let existing = edits::catalog_entry(&ws.root.raw, cat, &name);
                        let entry = existing.clone().or_else(|| seed.clone());
                        if let Some(entry) = entry.filter(|e| e != declared) {
                            let who = manifest
                                .name
                                .clone()
                                .unwrap_or_else(|| "package.json".into());
                            let follows = existing.is_some()
                                || matches!(
                                    (
                                        fnpm_semver::Range::parse(&entry),
                                        fnpm_semver::Version::parse(declared),
                                    ),
                                    (Ok(r), Ok(v)) if r.satisfies(&v)
                                );
                            let note = if follows {
                                format!(
                                    "note: {name} in {who} now follows the catalog entry \"{entry}\" instead of \"{declared}\""
                                )
                            } else {
                                format!(
                                    "note: {name} in {who} keeps \"{declared}\" because the catalog entry is \"{entry}\""
                                )
                            };
                            if config.log_level != fnpm_config::LogLevel::Silent {
                                events.emit(Event::notice(note));
                            }
                            if !follows {
                                continue;
                            }
                        }
                    }
                    let group = opts
                        .group
                        .or_else(|| {
                            declared_group(manifest, &name).filter(|g| *g != DependencyGroup::Peer)
                        })
                        .unwrap_or(DependencyGroup::Prod);
                    // A target that already reads the name from a catalog keeps that one.
                    let referenced = declared_literal(manifest, &name)
                        .and_then(|l| l.trim().strip_prefix("catalog:"))
                        .map(|c| match c.trim() {
                            "default" => String::new(),
                            c => c.to_owned(),
                        });
                    // Without `--catalog`, a bare name (or the entry's own text) that the
                    // default catalog has is added as `catalog:` (bun).
                    let default_entry = (opts.catalog.is_none()
                        && edits::catalog_supported(&ws.root.raw)
                        && matches!(
                            positional.spec,
                            DependencySpec::Npm {
                                is_alias: false,
                                ..
                            } | DependencySpec::DistTag { .. }
                        ))
                    .then(|| edits::catalog_entry(&ws.root.raw, "", &name))
                    .flatten()
                    .filter(|e| !positional.has_version() || positional.literal == *e);
                    let catalog = match (&opts.catalog, &referenced) {
                        (Some(_), Some(r)) => Some(r.clone()),
                        (Some(c), None) => Some(c.clone()),
                        (None, _) if default_entry.is_some() => Some(String::new()),
                        (None, _) => None,
                    };
                    let mut catalog_existing = None;
                    let spec = match &catalog {
                        Some(cat) if !positional.has_version() || default_entry.is_some() => {
                            catalog_existing = edits::catalog_entry(&ws.root.raw, cat, &name);
                            catalog_existing
                                .clone()
                                .or_else(|| seed.clone())
                                .unwrap_or_else(|| positional.resolve_literal())
                        }
                        _ if positional.literal.is_empty() => String::new(),
                        _ => positional.resolve_literal(),
                    };
                    planned.adds.push(Add {
                        positional: positional.clone(),
                        request: planned.requests.len(),
                        catalog,
                        catalog_seed: seed.clone().filter(|_| catalog_existing.is_none()),
                        catalog_existing,
                        relinked: false,
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
                    .filter(|x| match x {
                        DependencyGroup::Dev => !g.prod || g.dev,
                        DependencyGroup::Prod => !g.dev || g.prod,
                        DependencyGroup::Optional => !g.no_optional && !(g.dev && !g.prod),
                        // `--latest` moves a peerDependencies range too (bun).
                        DependencyGroup::Peer => opts.latest && !g.dev && !g.prod,
                    })
                    .collect()
            };
            let g = opts.update_groups;
            let selector = if g.dev {
                Some("--dev")
            } else if g.prod {
                Some("--prod")
            } else if g.no_optional {
                Some("--no-optional")
            } else if opts.depth.is_some() {
                Some("--depth 0")
            } else {
                None
            };
            if selector.is_some()
                && let Some(input) = opts.packages.iter().find(|i| {
                    Positional::parse(i.trim()).is_ok_and(|p| p.name.is_some() && p.has_version())
                })
            {
                return Err(Error::usage(format!(
                    "a version cannot be combined with --dev, --prod, --no-optional or --depth 0: {}",
                    input.trim()
                )));
            }
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
                                    catalog_seed: None,
                                    relinked: false,
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
            // The root owns the catalogs: an update there also moves the entries that members
            // read through `catalog:` (bun).
            if targets.iter().any(String::is_empty) {
                for member in &ws.members {
                    if targets.contains(&member.path) {
                        continue;
                    }
                    for g in &groups {
                        for (name, literal) in member.manifest.group(*g) {
                            let wanted = if patterns.is_empty() {
                                opts.packages.len() == negations.len()
                            } else {
                                patterns.iter().any(|pat| name_matches(pat, name))
                            };
                            if !wanted || excluded(name) || !literal.starts_with("catalog:") {
                                continue;
                            }
                            if !names.contains(name) {
                                names.push(name.clone());
                            }
                            planned.updates.push(UpdateTarget {
                                workspace: member.path.clone(),
                                name: name.clone(),
                                original: literal.clone(),
                            });
                        }
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
            // Entries a selector or an exclusion could have picked, for the no-op line.
            let checked: u32 = targets
                .iter()
                .map(|t| {
                    DependencyGroup::ALL
                        .into_iter()
                        .flat_map(|g| manifest_of(ws, t).group(g))
                        .filter(|(_, literal)| !literal.starts_with("workspace:"))
                        .count() as u32
                })
                .sum();
            let excluded_all = !negations.is_empty() && patterns.is_empty() && names.is_empty();
            if names.is_empty() && patterns.is_empty() && (excluded_all || selector.is_some()) {
                let by = if excluded_all {
                    let quoted: Vec<String> = opts
                        .packages
                        .iter()
                        .map(|p| format!("\"{}\"", p.trim()))
                        .collect();
                    format!("match {}", quoted.join(" "))
                } else {
                    format!("selected by {}", selector.unwrap_or_default())
                };
                planned.none_selected = Some((checked, by));
            }
            planned.globs = patterns
                .iter()
                .filter(|p| p.contains('*'))
                .cloned()
                .collect();
            planned.negations = negations.clone();
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
                && targets.iter().all(|t| t.is_empty())
                && groups.len() >= 3
            {
                Vec::new()
            } else {
                names
            };
            planned.bare_scope = opts.packages.is_empty()
                && opts.depth.is_none()
                && groups.len() >= 3
                && !names.is_empty();
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
    if planned.unmatched.is_empty()
        && !planned.bare_scope
        && planned.globs.is_empty()
        && planned.negations.is_empty()
    {
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
    let negated = |n: &str| planned.negations.iter().any(|p| name_matches(p, n));
    let mut reached: Vec<&String> = scoped.iter().collect();
    reached.sort();
    // A glob reaches the transitive packages it matches; negations alone select the rest.
    for glob in &planned.globs {
        extra.extend(
            reached
                .iter()
                .filter(|n| name_matches(glob, n) && !negated(n))
                .map(|n| (*n).clone()),
        );
    }
    if !planned.negations.is_empty() && planned.globs.is_empty() {
        extra.extend(reached.iter().filter(|n| !negated(n)).map(|n| (*n).clone()));
    }
    if planned.bare_scope {
        let mut reached: Vec<String> = scoped.iter().cloned().collect();
        reached.sort();
        extra.extend(reached);
    }
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
        if !extra.is_empty() {
            names.retain(|n| !n.is_empty());
        }
        for n in extra {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    Ok(())
}

/// Whether the package at `workspace` declares `name` in peerDependencies only.
pub(crate) fn declared_as_peer_only(ws: &Workspace, workspace: &str, name: &str) -> bool {
    declared_group(manifest_of(ws, workspace), name) == Some(DependencyGroup::Peer)
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
