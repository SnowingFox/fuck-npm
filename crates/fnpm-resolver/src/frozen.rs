//! Comparing the declared state (package.json files and root fields) with a previous graph:
//! the `--frozen-lockfile` check, the no-op fast path, and `changed` detection.

use std::collections::BTreeMap;

use fnpm_config::Config;
use fnpm_types::{Behavior, DepEdge, LockGraph, PackageId};
use fnpm_workspace::{PackageJson, Workspace};
use serde_json::Value;

use crate::local::{self, LocalKind};
use crate::{Error, Resolved, Result};

/// bun `DependencyChange` messages: `name was added`, `name was removed`,
/// `name is in different dependency groups`, `name changed in peerDependenciesMeta`,
/// `name changed from A to B`, each followed by ` in <workspace>` for members.
fn edge_changes(workspace: &str, now: &[DepEdge], before: &[DepEdge], out: &mut Vec<String>) {
    let suffix = if workspace.is_empty() {
        String::new()
    } else {
        format!(" in {workspace}")
    };
    // Both sides sorted by name (stable, so same-name edges keep their order), then walked
    // together: linear in the number of edges rather than names x edges.
    fn by_name(edges: &[DepEdge]) -> Vec<&DepEdge> {
        let mut v: Vec<&DepEdge> = edges.iter().collect();
        v.sort_by(|x, y| x.name.cmp(&y.name));
        v
    }
    let (now, before) = (by_name(now), by_name(before));
    let (mut i, mut j) = (0, 0);
    while i < now.len() || j < before.len() {
        let name = match (now.get(i), before.get(j)) {
            (Some(x), Some(y)) => x.name.as_str().min(y.name.as_str()),
            (Some(x), None) => x.name.as_str(),
            (None, Some(y)) => y.name.as_str(),
            (None, None) => break,
        };
        let i_end = i + now[i..].iter().take_while(|e| e.name == name).count();
        let j_end = j + before[j..].iter().take_while(|e| e.name == name).count();
        let (a, b) = (&now[i..i_end], &before[j..j_end]);
        (i, j) = (i_end, j_end);
        let message = match (a.is_empty(), b.is_empty()) {
            (false, true) => Some("was added".to_owned()),
            (true, false) => Some("was removed".to_owned()),
            _ => {
                let groups = |v: &[&DepEdge]| {
                    let mut g: Vec<_> = v.iter().map(|e| e.behavior.group()).collect();
                    g.sort();
                    g
                };
                if groups(a) != groups(b) {
                    Some("is in different dependency groups".to_owned())
                } else if let Some((x, y)) = a.iter().zip(b).find(|(x, y)| x.literal != y.literal) {
                    Some(format!("changed from {} to {}", y.literal, x.literal))
                } else if a
                    .iter()
                    .zip(b)
                    .any(|(x, y)| x.behavior.is_optional_peer() != y.behavior.is_optional_peer())
                {
                    Some("changed in peerDependenciesMeta".to_owned())
                } else {
                    None
                }
            }
        };
        if let Some(m) = message {
            out.push(format!("{name} {m}{suffix}"));
        }
    }
}

fn declared_edges(pj: &PackageJson, kind: LocalKind) -> Result<Vec<DepEdge>> {
    Ok(local::package_json_edges(pj, kind)?.0)
}

/// The explicit lifecycle trust policy from the root, workspaces and pnpm settings.
pub(crate) fn trusted(ws: &Workspace) -> Result<Option<std::collections::BTreeSet<String>>> {
    let mut set: Option<std::collections::BTreeSet<String>> = None;
    let sources = std::iter::once(&ws.root.raw).chain(ws.members.iter().map(|m| &m.manifest.raw));
    for raw in sources {
        let Some(field) = raw.get("trustedDependencies") else {
            continue;
        };
        let Some(items) = field.as_array() else {
            return Err(Error::InvalidPackageJson {
                path: "package.json".into(),
                message: "Expected \"trustedDependencies\" to be an array of strings".into(),
            });
        };
        let set = set.get_or_insert_with(Default::default);
        for item in items {
            let Some(name) = item.as_str() else {
                return Err(Error::InvalidPackageJson {
                    path: "package.json".into(),
                    message: "Expected \"trustedDependencies\" to be an array of strings".into(),
                });
            };
            set.insert(name.to_owned());
        }
    }
    if let Some(only) = &ws.settings.only_built_dependencies {
        set.get_or_insert_with(Default::default)
            .extend(only.iter().cloned());
    }
    Ok(set)
}

/// Everything that differs between the workspace and the previous graph, in bun's words. Empty
/// means a resolve would reproduce `prev` without any manifest.
pub(crate) fn changes(ws: &Workspace, prev: &LockGraph) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let root_edges = declared_edges(&ws.root, LocalKind::Root)?;
    edge_changes("", &root_edges, &prev.root().dependencies, &mut out);
    let mut paths: Vec<&str> = ws.members.iter().map(|m| m.path.as_str()).collect();
    for path in prev.workspaces.keys() {
        if !path.is_empty() && !paths.contains(&path.as_str()) {
            out.push(format!("workspace {path} was removed"));
        }
    }
    paths.sort_unstable();
    for member in &ws.members {
        let Some(prev_id) = prev.workspaces.get(&member.path) else {
            out.push(format!("workspace {} was added", member.path));
            continue;
        };
        let p = &prev.packages[prev_id.index()];
        if p.name != member.name {
            out.push(format!(
                "workspace {} was renamed from {} to {}",
                member.path, p.name, member.name
            ));
        }
        if p.version != member.version {
            out.push(format!(
                "version of workspace {} changed from {} to {}",
                member.path, p.version, member.version
            ));
        }
        let edges = declared_edges(&member.manifest, LocalKind::Workspace)?;
        edge_changes(&member.path, &edges, &p.dependencies, &mut out);
    }
    let root = &ws.root.raw;
    let members: Vec<&PackageJson> = ws.members.iter().map(|m| &m.manifest).collect();
    let (rules, _) =
        crate::overrides::parse_overrides_with(root, &members, &ws.settings.overrides)?;
    let sorted = |mut v: Vec<fnpm_types::OverrideRule>| {
        v.sort_by(|a, b| {
            a.selector
                .cmp(&b.selector)
                .then_with(|| a.value.cmp(&b.value))
        });
        v
    };
    if sorted(rules) != sorted(prev.overrides.clone()) {
        out.push(format!(
            "{} changed",
            if root.get("overrides").is_none() && root.get("resolutions").is_some() {
                "resolutions"
            } else {
                "overrides"
            }
        ));
    }
    if crate::catalogs::parse_catalogs(root, &ws.settings.catalogs)? != prev.catalogs {
        out.push("catalogs changed".into());
    }
    let (_, extensions, _) = crate::extensions::parse_package_extensions(root);
    if extensions != prev.package_extensions {
        out.push("packageExtensions changed".into());
    }
    let mut patched: BTreeMap<String, String> = root
        .get("patchedDependencies")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let prev_patched: BTreeMap<String, String> = prev
        .patched_dependencies
        .iter()
        .map(|(k, v)| (k.clone(), v.path.clone()))
        .collect();
    // The text lockfile only records trust and patches for packages present in its tree.
    // Ignore declarations the writer omitted, as bun does on a frozen reinstall.
    let patch_keys: std::collections::BTreeSet<String> = prev
        .packages
        .iter()
        .map(|p| {
            if matches!(p.resolution, fnpm_types::Resolution::Workspace { .. }) {
                format!("{}@{}", p.name, p.version)
            } else {
                p.key()
            }
        })
        .collect();
    patched.retain(|key, _| patch_keys.contains(key) || prev_patched.contains_key(key));
    if patched != prev_patched {
        out.push("patchedDependencies changed".into());
    }
    let mut trusted = trusted(ws)?;
    if let Some(names) = &mut trusted
        && !names.is_empty()
    {
        let mut present: std::collections::BTreeSet<&str> = prev
            .packages
            .iter()
            .skip(1)
            .map(|p| p.name.as_str())
            .collect();
        if let Some(recorded) = &prev.trusted_dependencies {
            present.extend(recorded.iter().map(String::as_str));
        }
        for p in &prev.packages {
            for e in &p.dependencies {
                if e.target.is_some_and(|t| {
                    !matches!(
                        prev.packages[t.index()].resolution,
                        fnpm_types::Resolution::Npm { .. }
                    )
                }) {
                    present.insert(&e.name);
                }
            }
        }
        names.retain(|name| present.contains(name.as_str()));
        if names.is_empty() {
            trusted = None;
        }
    }
    if trusted != prev.trusted_dependencies {
        out.push("trustedDependencies changed".into());
    }
    Ok(out)
}

/// Every required, non-peer edge of `prev` that a resolve would bind is bound.
pub(crate) fn complete(prev: &LockGraph) -> bool {
    prev.packages.iter().all(|p| {
        p.dependencies.iter().all(|e| {
            e.target.is_some()
                || e.behavior.is_peer()
                || e.behavior.is_optional()
                || (!p.resolution.is_local_tree() && e.literal.trim_start().starts_with("catalog:"))
        })
    })
}

/// Packages the fetch phase needs: reachable from the projects through edges that `--omit`
/// keeps, not disabled by the platform and not shipped inside another package's tarball.
pub(crate) fn needed(graph: &LockGraph, cfg: &Config) -> Vec<bool> {
    let mut seen = vec![false; graph.len()];
    let mut stack: Vec<PackageId> = graph.workspaces.values().copied().collect();
    while let Some(id) = stack.pop() {
        if std::mem::replace(&mut seen[id.index()], true) {
            continue;
        }
        let p = &graph.packages[id.index()];
        let local = p.resolution.is_local_tree();
        for e in &p.dependencies {
            let Some(t) = e.target else { continue };
            let b = e.behavior;
            if (cfg.omit.dev && b.is_dev())
                || (cfg.omit.optional && b.is_optional())
                || (cfg.omit.peer && b.is_peer())
            {
                continue;
            }
            if b.is_bundled() && !local {
                continue;
            }
            if !graph.packages[t.index()]
                .platform
                .matches_for_edge(&cfg.platform, b.is_optional())
            {
                continue;
            }
            if !seen[t.index()] {
                stack.push(t);
            }
        }
    }
    seen
}

/// Sends a [`Resolved::Package`] for every needed, storable package.
pub(crate) fn emit_all(
    graph: &LockGraph,
    cfg: &Config,
    tx: Option<&tokio::sync::mpsc::UnboundedSender<Resolved>>,
) {
    let Some(tx) = tx else { return };
    for (i, keep) in needed(graph, cfg).into_iter().enumerate() {
        let p = &graph.packages[i];
        if keep && p.resolution.is_storable() && !p.meta.bundled {
            let _ = tx.send(Resolved::of(PackageId(i as u32), p));
        }
    }
}

/// A form of a graph that ignores package order, for `changed`.
fn canonical(graph: &LockGraph) -> Value {
    let keys: Vec<String> = graph.packages.iter().map(|p| p.key()).collect();
    let mut packages: BTreeMap<String, Value> = BTreeMap::new();
    for (i, p) in graph.packages.iter().enumerate() {
        let mut p = p.clone();
        let mut edges: Vec<Value> = p
            .dependencies
            .iter()
            .map(|e| {
                serde_json::json!([
                    e.name,
                    e.literal,
                    (e.behavior & !Behavior::WORKSPACE).bits(),
                    e.target.map(|t| keys[t.index()].clone())
                ])
            })
            .collect();
        edges.sort_by_key(|v| v.to_string());
        p.dependencies.clear();
        // Project packages are rebuilt from package.json every time; the lockfile only records
        // their name, version and edges.
        let mut v = if p.resolution.is_local_tree() {
            serde_json::json!({ "name": p.name, "version": p.version })
        } else {
            serde_json::to_value(&p).unwrap_or(Value::Null)
        };
        v["edges"] = Value::Array(edges);
        packages.insert(keys[i].clone(), v);
    }
    let workspaces: BTreeMap<&String, &String> = graph
        .workspaces
        .iter()
        .map(|(k, id)| (k, &keys[id.index()]))
        .collect();
    let mut overrides = graph.overrides.clone();
    overrides.sort_by(|a, b| a.selector.cmp(&b.selector));
    serde_json::json!({
        "packages": packages,
        "workspaces": serde_json::to_value(workspaces).unwrap_or(Value::Null),
        "catalogs": serde_json::to_value(&graph.catalogs).unwrap_or(Value::Null),
        "overrides": serde_json::to_value(&overrides).unwrap_or(Value::Null),
        "patched": serde_json::to_value(graph.patched_dependencies.iter().map(|(k, v)| (k, &v.path)).collect::<BTreeMap<_, _>>()).unwrap_or(Value::Null),
        "trusted": serde_json::to_value(&graph.trusted_dependencies).unwrap_or(Value::Null),
        "extensions": serde_json::to_value(&graph.package_extensions).unwrap_or(Value::Null),
        "linker": serde_json::to_value(graph.node_linker).unwrap_or(Value::Null),
    })
}

/// The two graphs describe the same install (package order aside).
pub(crate) fn equivalent(a: &LockGraph, b: &LockGraph) -> bool {
    a.len() == b.len() && canonical(a) == canonical(b)
}
