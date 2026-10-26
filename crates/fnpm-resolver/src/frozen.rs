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
        // Same-name edges pair up by group: package.json and the lockfile need not list a
        // name's groups in the same order (`dependencies` and `devDependencies`).
        fn by_group<'a>(v: &[&'a DepEdge]) -> Vec<&'a DepEdge> {
            let mut v = v.to_vec();
            v.sort_by_key(|e| e.behavior.group());
            v
        }
        // `optionalDependencies` replaces the same name in `dependencies` (as in
        // `package_json_edges`); a lockfile from Bun 1.4 still lists both.
        let (a, b) = (
            by_group(&collapse_optional(&now[i..i_end])),
            by_group(&collapse_optional(&before[j..j_end])),
        );
        let (a, b) = (a.as_slice(), b.as_slice());
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

/// Same-name edges as package.json declares them: without a `dependencies` one that an
/// `optionalDependencies` one replaces, or the `*` optional peer that Bun 1.4 wrote next to the
/// declared peer range.
fn collapse_optional<'a>(edges: &[&'a DepEdge]) -> Vec<&'a DepEdge> {
    let optional = edges.iter().any(|e| {
        !e.behavior.is_peer() && e.behavior.group() == fnpm_types::DependencyGroup::Optional
    });
    let ranged_peer = edges
        .iter()
        .any(|e| e.behavior.is_peer() && e.literal != "*");
    edges
        .iter()
        .copied()
        .filter(|e| !optional || !shadowed_by_optional(e))
        .filter(|e| !(ranged_peer && e.behavior.is_peer() && e.literal == "*"))
        .collect()
}

/// A `dependencies` edge (not a dev, optional or peer one).
fn shadowed_by_optional(e: &DepEdge) -> bool {
    e.behavior.group() == fnpm_types::DependencyGroup::Prod
}

/// Bun 1.4 and older left a workspace out of the lockfile when a root dependency on a registry
/// version of its name took its entry: a tag, or a range that the workspace's version does not
/// satisfy, under its own name or by an `npm:` alias of it. Such a lockfile is not missing the
/// workspace for a frozen install. A plain one keeps it too while a range under the workspace's
/// own name (`own_name_only`) has the workspace's folder in node_modules.
fn dropped_by_bun_1_4(
    root: &[DepEdge],
    member: &fnpm_workspace::Member,
    own_name_only: bool,
) -> bool {
    use fnpm_semver::{Range, Version};
    let Ok(version) = Version::parse(&member.version) else {
        return false;
    };
    root.iter().any(|e| {
        if e.behavior.is_workspace() || (own_name_only && e.name != member.name) {
            return false;
        }
        match fnpm_types::DependencySpec::parse(&e.name, &e.literal) {
            // A plain install links the workspace for a tag of its name.
            Ok(fnpm_types::DependencySpec::DistTag { name, .. }) => {
                !own_name_only && name == member.name
            }
            Ok(fnpm_types::DependencySpec::Npm { name, range, .. }) => {
                name == member.name
                    && matches!(Range::parse(&range), Ok(r) if !r.satisfies(&version))
            }
            _ => false,
        }
    })
}

/// The root fields whose change bun names in its frozen note, in its order (`catalogs` is
/// worded `the catalog`).
pub(crate) const SECTIONS: [&str; 4] =
    ["overrides", "resolutions", "catalogs", "packageExtensions"];

/// `overrides changed`, `catalogs changed`, ...
pub(crate) fn is_section_change(change: &str) -> bool {
    change
        .strip_suffix(" changed")
        .is_some_and(|s| SECTIONS.contains(&s))
}

/// A change [`edge_changes`] words (`a was added`, `a changed from ^1 to ^2 in packages/x`).
pub(crate) fn is_dependency_change(change: &str) -> bool {
    let Some((_, rest)) = change.split_once(' ') else {
        return false;
    };
    !change.starts_with("workspace ")
        && !change.starts_with("version of workspace ")
        && !change.starts_with("lockfile is ")
        && [
            "was added",
            "was removed",
            "is in different dependency groups",
            "changed in peerDependenciesMeta",
            "changed from ",
        ]
        .iter()
        .any(|m| rest.starts_with(m))
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
    Ok(classified(ws, prev, false)?.0)
}

/// [`changes`], and how many of them (at the front) are dependency edits: an edited range, an
/// added, removed or moved dependency. A resolve may still reproduce `prev` after those (a
/// loosened range that the locked version satisfies); the later ones (workspaces, overrides,
/// catalogs, patches, trust) change the lockfile itself. A frozen install, like bun's, does not
/// compare `patchedDependencies` and `trustedDependencies`: it applies what package.json says.
pub(crate) fn classified(
    ws: &Workspace,
    prev: &LockGraph,
    frozen: bool,
) -> Result<(Vec<String>, usize)> {
    let mut out = Vec::new();
    let root_edges = declared_edges(&ws.root, LocalKind::Root)?;
    edge_changes("", &root_edges, &prev.root().dependencies, &mut out);
    for member in &ws.members {
        if let Some(prev_id) = prev.workspaces.get(&member.path) {
            let edges = declared_edges(&member.manifest, LocalKind::Workspace)?;
            let p = &prev.packages[prev_id.index()];
            edge_changes(&member.path, &edges, &p.dependencies, &mut out);
            // Release tooling bumps versions without an install; the ranges on it decide.
            if p.name == member.name && p.version != member.version {
                out.push(format!(
                    "version of workspace {} changed from {} to {}",
                    member.path, p.version, member.version
                ));
            }
        }
    }
    let edges = out.len();
    let mut paths: Vec<&str> = ws.members.iter().map(|m| m.path.as_str()).collect();
    for path in prev.workspaces.keys() {
        if !path.is_empty() && !paths.contains(&path.as_str()) {
            out.push(format!("workspace {path} was removed"));
        }
    }
    paths.sort_unstable();
    for member in &ws.members {
        let Some(prev_id) = prev.workspaces.get(&member.path) else {
            if !dropped_by_bun_1_4(&root_edges, member, !frozen) {
                out.push(format!("workspace {} was added", member.path));
            }
            continue;
        };
        let p = &prev.packages[prev_id.index()];
        if p.name != member.name {
            out.push(format!(
                "workspace {} was renamed from {} to {}",
                member.path, p.name, member.name
            ));
        }
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
    let catalogs = crate::catalogs::parse_catalogs(root, &ws.settings.catalogs)?;
    if catalogs != prev.catalogs && !(frozen && skipped_catalogs(ws, prev, &catalogs).is_some()) {
        out.push("catalogs changed".into());
    }
    let (rules, extensions, _) = crate::extensions::parse_package_extensions(root);
    if extensions != prev.package_extensions && !crate::extensions::applied_alike(&rules, prev) {
        out.push("packageExtensions changed".into());
    }
    if frozen {
        return Ok((out, edges));
    }
    // A frozen install warns about an edited `file:` folder; a plain one reads it again.
    for path in folder_edits(ws, prev) {
        out.push(format!("folder {path} changed"));
    }
    // A frozen install links a workspace's bin from its package.json; a plain one records it.
    for member in &ws.members {
        if let Some(id) = prev.workspaces.get(&member.path)
            && prev.packages[id.index()].bin != member.manifest.bin
        {
            out.push(format!("bin of workspace {} changed", member.path));
        }
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
        .map(fnpm_types::Package::patch_key)
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
    Ok((out, edges))
}

/// The `file:` folders (paths relative to the root, as fnpm.lock has them) whose package.json
/// now declares other dependencies or bins than `prev` records for them.
pub(crate) fn folder_edits(ws: &Workspace, prev: &LockGraph) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in &prev.packages {
        let fnpm_types::Resolution::Folder { path } = &p.resolution else {
            continue;
        };
        if out.contains(path) {
            continue;
        }
        let Ok(pj) = PackageJson::read(&ws.root_dir.join(path).join("package.json")) else {
            continue;
        };
        let Ok(edges) = declared_edges(&pj, LocalKind::Folder) else {
            continue;
        };
        let mut changes = Vec::new();
        edge_changes("", &edges, &p.dependencies, &mut changes);
        if !changes.is_empty() || pj.bin != p.bin {
            out.push(path.clone());
        }
    }
    out
}

/// The workspaces `prev` lists whose package.json is not on disk (a pruned checkout: `turbo
/// prune`, a Docker layer with some workspaces), as `(path, id)`. A frozen install keeps the
/// lockfile as it is and installs the others.
pub(crate) fn pruned(ws: &Workspace, prev: &LockGraph) -> Vec<(String, PackageId)> {
    prev.workspaces
        .iter()
        .filter(|(path, _)| {
            !path.is_empty()
                && !ws.members.iter().any(|m| &m.path == *path)
                && !ws
                    .root_dir
                    .join(path.as_str())
                    .join("package.json")
                    .exists()
        })
        .map(|(path, id)| (path.clone(), *id))
        .collect()
}

/// `prev` without the `pruned` workspaces: they are not projects of this install, and the root no
/// longer lists them. A peer or optional dependency of a package bound to one is left out
/// (bun `skips_link_to_pruned_workspace`). Their packages stay in the arena, unreachable, so
/// nothing installs them.
pub(crate) fn without_pruned(prev: &LockGraph, pruned: &[(String, PackageId)]) -> LockGraph {
    let mut graph = prev.clone();
    for (path, id) in pruned {
        graph.workspaces.remove(path);
        graph.packages[0]
            .dependencies
            .retain(|e| !(e.behavior.contains(Behavior::WORKSPACE) && e.target == Some(*id)));
    }
    for p in &mut graph.packages {
        if p.resolution.is_local_tree() {
            continue;
        }
        for e in &mut p.dependencies {
            if (e.behavior.is_peer() || e.behavior.is_optional())
                && e.target
                    .is_some_and(|t| pruned.iter().any(|(_, id)| *id == t))
            {
                e.target = None;
            }
        }
    }
    graph
}

/// bun's errors for a remaining project that needs a pruned workspace: a `workspace:`
/// dependency on its name, or (`recorded`, a frozen install, which never asks the registry for
/// a workspace's name) a dependency that `prev` bound to it.
pub(crate) fn pruned_dependents(
    ws: &Workspace,
    prev: &LockGraph,
    pruned: &[(String, PackageId)],
    recorded_bindings: bool,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let projects = std::iter::once((None, &ws.root, PackageId::ROOT, LocalKind::Root)).chain(
        ws.members.iter().filter_map(|m| {
            prev.workspaces.get(&m.path).map(|id| {
                (
                    Some(m.name.as_str()),
                    &m.manifest,
                    *id,
                    LocalKind::Workspace,
                )
            })
        }),
    );
    for (name, manifest, id, kind) in projects {
        let recorded = &prev.packages[id.index()].dependencies;
        let mut reported: Vec<PackageId> = Vec::new();
        for edge in declared_edges(manifest, kind)? {
            if edge.behavior.contains(Behavior::WORKSPACE) {
                continue;
            }
            let by_protocol = edge
                .literal
                .trim_start()
                .starts_with("workspace:")
                .then(|| {
                    pruned
                        .iter()
                        .find(|(_, t)| prev.packages[t.index()].name == edge.name)
                })
                .flatten();
            let bound = || {
                if !recorded_bindings {
                    return None;
                }
                recorded
                    .iter()
                    .find(|r| r.name == edge.name && r.behavior.group() == edge.behavior.group())
                    .and_then(|r| r.target)
                    .and_then(|t| pruned.iter().find(|(_, p)| *p == t))
            };
            let Some((path, target)) = by_protocol.or_else(bound) else {
                continue;
            };
            if reported.contains(target) {
                continue;
            }
            reported.push(*target);
            let target_name = &prev.packages[target.index()].name;
            out.push(match name {
                None => format!(
                    "the root package depends on workspace \"{target_name}\" ({path}), which is listed in fnpm.lock but not on disk"
                ),
                Some(name) => format!(
                    "workspace \"{name}\" depends on workspace \"{target_name}\" ({path}), which is listed in fnpm.lock but not on disk"
                ),
            });
        }
    }
    Ok(out)
}

/// A project's registry dependency that `prev` pins to a version outside its range (no
/// override, peer or unresolved optional explains it).
pub(crate) fn stale_pin(prev: &LockGraph) -> bool {
    use fnpm_semver::{Range, Version};
    prev.workspaces.values().any(|id| {
        prev.packages[id.index()].dependencies.iter().any(|e| {
            let Some(t) = e.target else { return false };
            let target = &prev.packages[t.index()];
            // Another edge of the name decides the package (an optional one replacing it, or
            // one in another group, a tag of Bun 1.4's, on the same package).
            let replaced = prev.packages[id.index()].dependencies.iter().any(|o| {
                !std::ptr::eq(o, e)
                    && o.name == e.name
                    && !o.behavior.is_peer()
                    && (o.target == e.target
                        || (shadowed_by_optional(e)
                            && o.behavior.group() == fnpm_types::DependencyGroup::Optional))
            });
            if e.behavior.is_peer()
                || replaced
                || !matches!(target.resolution, fnpm_types::Resolution::Npm { .. })
                || prev.overrides.iter().any(|r| {
                    r.selector.target.name == e.name || r.selector.target.name == target.name
                })
            {
                return false;
            }
            let Ok(fnpm_types::DependencySpec::Npm { range, .. }) =
                fnpm_types::DependencySpec::parse(&e.name, &e.literal)
            else {
                return false;
            };
            if range.trim().is_empty() || range.trim() == "*" {
                return false;
            }
            matches!(
                (Range::parse(&range), Version::parse(&target.version)),
                (Ok(r), Ok(v)) if !r.satisfies(&v)
            )
        })
    })
}

/// Catalog entries as `(catalog, name) -> range`; `catalogs.default` is the default catalog.
fn catalog_entries(c: &fnpm_types::Catalogs) -> BTreeMap<(&str, &str), &str> {
    let mut out = BTreeMap::new();
    for (name, range) in &c.default {
        out.insert(("", name.as_str()), range.as_str());
    }
    for (group, entries) in &c.named {
        let group = if group == "default" {
            ""
        } else {
            group.as_str()
        };
        for (name, range) in entries {
            out.entry((group, name.as_str())).or_insert(range.as_str());
        }
    }
    out
}

/// bun's pruned-checkout rule for catalogs: the lockfile may lack catalog entries that no
/// project on disk (and no override) uses, as long as every entry it has is unchanged. The
/// names it lacks, sorted (empty when the catalogs agree), or `None` when that is not the case.
pub(crate) fn skipped_catalogs(
    ws: &Workspace,
    prev: &LockGraph,
    declared: &fnpm_types::Catalogs,
) -> Option<Vec<String>> {
    let (declared, locked) = (catalog_entries(declared), catalog_entries(&prev.catalogs));
    if locked.iter().any(|(k, v)| declared.get(k) != Some(v)) {
        return None;
    }
    fn group_of(literal: &str) -> Option<&str> {
        literal.trim().strip_prefix("catalog:").map(|g| {
            let g = g.trim();
            if g == "default" { "" } else { g }
        })
    }
    let projects = std::iter::once(&ws.root).chain(ws.members.iter().map(|m| &m.manifest));
    let mut used: Vec<(String, String)> = Vec::new();
    for pj in projects {
        for group in fnpm_types::DependencyGroup::ALL {
            for (name, literal) in pj.group(group) {
                if let Some(g) = group_of(literal) {
                    used.push((g.to_owned(), name.clone()));
                }
            }
        }
    }
    for r in &prev.overrides {
        if let Some(g) = group_of(&r.value) {
            used.push((g.to_owned(), r.selector.target.name.clone()));
        }
    }
    let mut skipped = Vec::new();
    for (group, name) in declared.keys() {
        if locked.contains_key(&(*group, *name)) {
            continue;
        }
        if used.iter().any(|(g, n)| g == group && n == name) {
            return None;
        }
        skipped.push((*name).to_owned());
    }
    skipped.sort();
    Some(skipped)
}

/// bun `warn_orphaned_patches`: a `patchedDependencies` key that names no installed version,
/// as `<path> no longer applies (<name> is no longer installed)` or `(... is now <versions>)`.
pub(crate) fn orphaned_patches(ws: &Workspace, prev: &LockGraph) -> Vec<String> {
    let mut out = Vec::new();
    for (key, patch) in declared_patches(ws) {
        let Some(at) = key.get(1..).and_then(|k| k.find('@')).map(|i| i + 1) else {
            continue;
        };
        let (name, version) = (&key[..at], &key[at + 1..]);
        let installed: Vec<&fnpm_types::Package> =
            prev.packages.iter().filter(|p| p.name == name).collect();
        // `file:` folders, `link:` dependencies and workspaces are never patched.
        if installed.iter().any(|p| p.resolution.is_local_tree()) {
            continue;
        }
        if installed.iter().any(|p| p.version == version) {
            continue;
        }
        let now: Vec<&str> = installed.iter().map(|p| p.version.as_str()).collect();
        out.push(if now.is_empty() {
            format!(
                "{} no longer applies ({name} is no longer installed)",
                patch.path
            )
        } else {
            format!(
                "{} no longer applies ({name} is now {})",
                patch.path,
                now.join(", ")
            )
        });
    }
    out
}

/// The `patchedDependencies` of the root package.json, for an install that keeps the lockfile.
pub(crate) fn declared_patches(ws: &Workspace) -> BTreeMap<String, fnpm_types::PatchedDependency> {
    ws.root
        .raw
        .get("patchedDependencies")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| {
                    v.as_str().map(|path| {
                        (
                            k.clone(),
                            fnpm_types::PatchedDependency {
                                path: path.to_owned(),
                                hash: None,
                            },
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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

/// `after`, the resolve of edited package.json files against `before`, installs the same
/// packages (bun's `Lockfile.eql` on a frozen install): the same set of packages, the same
/// edges below the projects, and every dependency a project kept still bound to the same
/// package. Ranges, groups and dependencies a project dropped or gained without moving a
/// package do not count.
pub(crate) fn same_packages(before: &LockGraph, after: &LockGraph) -> bool {
    if before.len() != after.len() {
        return false;
    }
    let keys = |g: &LockGraph| -> Vec<String> { g.packages.iter().map(|p| p.key()).collect() };
    let (before_keys, after_keys) = (keys(before), keys(after));
    let index: BTreeMap<&str, usize> = before_keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let targets = |keys: &[String], p: &fnpm_types::Package| {
        let mut v: Vec<(String, Option<String>)> = p
            .dependencies
            .iter()
            .map(|e| (e.name.clone(), e.target.map(|t| keys[t.index()].clone())))
            .collect();
        v.sort();
        v
    };
    for (i, p) in after.packages.iter().enumerate() {
        let Some(&j) = index.get(after_keys[i].as_str()) else {
            return false;
        };
        let q = &before.packages[j];
        let (now, then) = (targets(&after_keys, p), targets(&before_keys, q));
        if !p.resolution.is_local_tree() {
            if now != then {
                return false;
            }
            continue;
        }
        let then: BTreeMap<&String, &Option<String>> = then.iter().map(|(n, t)| (n, t)).collect();
        if now.iter().any(|(n, t)| {
            t.is_some()
                && then
                    .get(n)
                    .is_some_and(|before| before.is_some() && *before != t)
        }) {
            return false;
        }
    }
    true
}

/// The two graphs describe the same install (package order aside).
pub(crate) fn equivalent(a: &LockGraph, b: &LockGraph) -> bool {
    a.len() == b.len() && canonical(a) == canonical(b)
}
