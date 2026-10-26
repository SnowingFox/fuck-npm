//! bun's `incorrect peer dependency` warnings (`PackageManager::warn_incorrect_peers`): the copy
//! of a peer that the layout gives a dependent, when it is outside the range the dependent asks
//! for. The layout decides, so the check runs on the layout that is linked.

use std::collections::HashSet;

use fnpm_linker::Layout;
use fnpm_semver::{Range, Version};
use fnpm_types::{DepEdge, LockGraph, PackageId, Resolution};

/// Every `(dependent, peer edge index, copy)` of the layout.
fn served(view: &LockGraph, layout: &Layout) -> Vec<(PackageId, usize, PackageId)> {
    let mut out = Vec::new();
    match layout {
        Layout::Hoisted(tree) => {
            let mut paths: Vec<(String, PackageId)> = tree
                .nodes
                .iter()
                .map(|n| (n.path.clone(), n.package))
                .collect();
            paths.sort_by(|a, b| a.0.cmp(&b.0));
            // The importers load from their own folder: the root from the top, a workspace from
            // its node.
            let mut from: Vec<(PackageId, String)> = vec![(PackageId::ROOT, String::new())];
            for (ws, _) in &tree.workspace_roots {
                if let Some(node) = tree
                    .nodes
                    .iter()
                    .find(|n| n.package == *ws && !n.path.contains('/'))
                {
                    from.push((*ws, node.path.clone()));
                }
            }
            for node in &tree.nodes {
                if !from.iter().any(|(id, _)| *id == node.package) {
                    from.push((node.package, node.path.clone()));
                }
            }
            let mut seen = HashSet::new();
            for (id, path) in from {
                let Some(p) = view.packages.get(id.index()) else {
                    continue;
                };
                for (i, e) in p.dependencies.iter().enumerate() {
                    if !e.behavior.is_peer() {
                        continue;
                    }
                    if let Some(copy) = fnpm_lockfile::tree::resolve_paths(&paths, &path, &e.name)
                        && seen.insert((id, i, copy))
                    {
                        out.push((id, i, copy));
                    }
                }
            }
        }
        Layout::Isolated(iso) => {
            let mut seen = HashSet::new();
            // What is linked: the importers' node_modules, and the entries reachable from them.
            // An importer has one node_modules, so an entry of an importer in another context
            // is never linked.
            let importer = |id: PackageId| iso.roots.iter().any(|(r, _)| *r == id);
            let mut reached = vec![false; iso.entries.len()];
            let mut stack: Vec<u32> = iso
                .roots
                .iter()
                .flat_map(|(_, deps)| deps.iter().map(|(_, e)| *e))
                .collect();
            while let Some(i) = stack.pop() {
                let Some(entry) = iso.entries.get(i as usize) else {
                    continue;
                };
                if std::mem::replace(&mut reached[i as usize], true) || importer(entry.package) {
                    continue;
                }
                stack.extend(entry.deps.iter().map(|(_, e)| *e));
            }
            let linked = iso
                .entries
                .iter()
                .enumerate()
                .filter(|(i, e)| reached[*i] && !importer(e.package))
                .map(|(_, e)| (e.package, e.deps.as_slice()))
                .chain(
                    iso.roots
                        .iter()
                        .enumerate()
                        .filter(|(i, (id, _))| !iso.roots[..*i].iter().any(|(r, _)| r == id))
                        .map(|(_, (id, deps))| (*id, deps.as_slice())),
                );
            for (id, deps) in linked {
                let Some(p) = view.packages.get(id.index()) else {
                    continue;
                };
                for (i, e) in p.dependencies.iter().enumerate() {
                    if !e.behavior.is_peer() {
                        continue;
                    }
                    for (alias, entry) in deps {
                        if *alias == e.name
                            && let Some(copy) = iso.entries.get(*entry as usize)
                            && seen.insert((id, i, copy.package))
                        {
                            out.push((id, i, copy.package));
                        }
                    }
                }
            }
        }
    }
    out
}

/// The range the resolver reads for a peer edge: after `overrides` and catalogs.
fn effective_range(view: &LockGraph, owner: PackageId, e: &DepEdge) -> Option<(Range, String)> {
    let parent = view.packages.get(owner.index());
    let rule = fnpm_resolver::overrides::find(&view.overrides, parent, &e.name, &e.literal);
    let (name, mut literal) = match rule {
        Some(r) => (r.selector.target.name.as_str(), r.value.as_str()),
        None => (e.name.as_str(), e.literal.as_str()),
    };
    if let Some(catalog) = literal.trim().strip_prefix("catalog:")
        && let Some(entry) = view.catalogs.get(catalog.trim(), name)
    {
        literal = entry;
    }
    let range = crate::spec::split_npm_alias(literal).map_or(literal, |(_, r)| r);
    if fnpm_types::spec::infer_tag(range) != fnpm_types::SpecTag::Npm {
        return None;
    }
    Some((Range::parse(range).ok()?, literal.to_owned()))
}

fn version_of(view: &LockGraph, id: PackageId) -> Option<Version> {
    let p = view.packages.get(id.index())?;
    match &p.resolution {
        Resolution::Npm { version, .. } => Version::parse(version).ok(),
        Resolution::Workspace { .. } => Version::parse(&p.version).ok(),
        _ => None,
    }
}

/// The warnings, in layout order, each once.
pub(crate) fn incorrect_peers(view: &LockGraph, layout: &Layout) -> Vec<String> {
    let esc = |s: &str| crate::security::escape_control(s, false);
    let mut out: Vec<String> = Vec::new();
    for (dependent, index, copy) in served(view, layout) {
        let e = &view.packages[dependent.index()].dependencies[index];
        if e.behavior.is_optional_peer() {
            continue;
        }
        let (Some((range, literal)), Some(version)) = (
            effective_range(view, dependent, e),
            version_of(view, copy),
        ) else {
            continue;
        };
        if range.satisfies(&version) {
            continue;
        }
        let c = &view.packages[copy.index()];
        let d = &view.packages[dependent.index()];
        // The root's resolution prints as nothing.
        let dependent_resolution = if dependent == PackageId::ROOT {
            "root".to_owned()
        } else {
            esc(&d.resolution.to_string())
        };
        let line = format!(
            "incorrect peer dependency \"{}@{}\": \"{}@{dependent_resolution}\" requires \"{}@{}\"",
            c.name,
            esc(&c.resolution.to_string()),
            d.name,
            e.name,
            esc(&literal),
        );
        if !out.contains(&line) {
            out.push(line);
        }
    }
    out
}
