//! Read-only views of the lockfile: `why`, `ls`, `pm bin`, `pm hash*`, `pm licenses`, and
//! `fnpm-lock verify | diff | why | print`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use fnpm_lockfile::Format;
use fnpm_types::{Behavior, LockGraph, PackageId, Resolution};
use serde_json::{Value, json};

use super::{Ctx, exit, ok, table};
use crate::install::name_matches;
use crate::{CoreResult, Error, Result};

/// The version shown for a package.
fn shown_version(graph: &LockGraph, id: PackageId) -> String {
    let Some(p) = graph.get(id) else {
        return String::new();
    };
    match &p.resolution {
        Resolution::Root => String::new(),
        Resolution::Npm { version, .. } => version.clone(),
        Resolution::Workspace { .. } => p.version.clone(),
        r => r.to_string(),
    }
}

fn display_name(graph: &LockGraph, id: PackageId) -> String {
    let p = &graph.packages[id.index()];
    if p.name.is_empty() {
        if id == PackageId::ROOT {
            "(root)".into()
        } else {
            p.resolution.to_string()
        }
    } else {
        p.name.clone()
    }
}

fn dep_type(b: Behavior) -> &'static str {
    if b.is_dev() {
        "dev "
    } else if b.is_optional_peer() {
        "optional peer "
    } else if b.is_peer() {
        "peer "
    } else if b.is_optional() {
        "optional "
    } else {
        ""
    }
}

/// `pattern` with an optional `@range`: matches a package name (glob) and version.
fn matches_pattern(pattern: &str, name: &str, version: &str) -> bool {
    let (name_pat, range) = fnpm_types::name::split_name_and_maybe_version(pattern);
    if !name_matches(name_pat, name) {
        return false;
    }
    match range {
        None => true,
        Some(r) => fnpm_semver::satisfies(version, r),
    }
}

struct Dependent {
    from: PackageId,
    spec: String,
    behavior: Behavior,
    alias: Option<String>,
}

fn dependents(graph: &LockGraph) -> HashMap<PackageId, Vec<Dependent>> {
    let mut map: HashMap<PackageId, Vec<Dependent>> = HashMap::new();
    for (i, p) in graph.packages.iter().enumerate() {
        for e in &p.dependencies {
            let Some(t) = e.target else { continue };
            let target_name = &graph.packages[t.index()].name;
            map.entry(t).or_default().push(Dependent {
                from: PackageId(i as u32),
                spec: e.literal.clone(),
                behavior: e.behavior,
                alias: (e.name != *target_name).then(|| e.name.clone()),
            });
        }
    }
    for list in map.values_mut() {
        list.sort_by(|a, b| {
            let la = graph.packages[a.from.index()].resolution.is_local_tree();
            let lb = graph.packages[b.from.index()].resolution.is_local_tree();
            lb.cmp(&la).then_with(|| {
                graph.packages[a.from.index()]
                    .name
                    .cmp(&graph.packages[b.from.index()].name)
            })
        });
    }
    map
}

fn dependent_line(graph: &LockGraph, d: &Dependent) -> String {
    let p = &graph.packages[d.from.index()];
    let mut s = String::from(dep_type(d.behavior));
    s.push_str(&display_name(graph, d.from));
    match &p.resolution {
        Resolution::Workspace { .. } => s.push_str("@workspace"),
        Resolution::Root => {}
        _ => {
            s.push('@');
            s.push_str(&shown_version(graph, d.from));
        }
    }
    if !d.spec.is_empty() {
        match &d.alias {
            Some(a) => s.push_str(&format!(" (requires {a}@{})", d.spec)),
            None => s.push_str(&format!(" (requires {})", d.spec)),
        }
    }
    s
}

#[allow(clippy::too_many_arguments)]
fn why_tree(
    graph: &LockGraph,
    deps: &HashMap<PackageId, Vec<Dependent>>,
    id: PackageId,
    prefix: &str,
    depth: usize,
    max_depth: usize,
    path: &mut Vec<PackageId>,
    expanded: &mut HashSet<PackageId>,
    lines: &mut Vec<String>,
) -> Value {
    let Some(list) = deps.get(&id) else {
        return json!([]);
    };
    let mut out = Vec::new();
    if depth >= max_depth {
        lines.push(format!("{prefix}└─ (deeper dependencies hidden)"));
        return json!([]);
    }
    for (i, d) in list.iter().enumerate() {
        let last = i + 1 == list.len();
        let (branch, cont) = if last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        lines.push(format!("{prefix}{branch}{}", dependent_line(graph, d)));
        let mut node = json!({
            "name": display_name(graph, d.from),
            "version": shown_version(graph, d.from),
            "spec": d.spec,
            "type": dep_type(d.behavior).trim(),
        });
        let child_prefix = format!("{prefix}{cont}");
        if path.contains(&d.from) {
            lines.push(format!("{child_prefix}└─ *circular"));
        } else if !graph.packages[d.from.index()].resolution.is_local_tree() {
            if !expanded.insert(d.from) {
                if deps.contains_key(&d.from) {
                    lines.push(format!("{child_prefix}└─ *deduped"));
                }
            } else {
                path.push(d.from);
                node["dependents"] = why_tree(
                    graph,
                    deps,
                    d.from,
                    &child_prefix,
                    depth + 1,
                    max_depth,
                    path,
                    expanded,
                    lines,
                );
                path.pop();
            }
        }
        out.push(node);
    }
    Value::Array(out)
}

/// `fnpm why <pattern>` / `pm why` / `fnpm-lock why`.
pub(crate) fn why(cx: &Ctx) -> Result<CoreResult> {
    let Some(pattern) = cx.req.positionals.first().cloned() else {
        return Err(Error::usage(
            "Missing package name. Usage: fnpm why <package>",
        ));
    };
    let (_, _, graph) = cx.load_locked("nothing to explain")?;
    let max_depth = cx
        .number("depth")
        .map_or(usize::MAX, |d| usize::try_from(d).unwrap_or(usize::MAX));
    let top = cx.flag("top");
    let deps = dependents(&graph);
    let mut lines = Vec::new();
    let mut packages = Vec::new();
    for id in graph.ids().skip(1) {
        let p = &graph.packages[id.index()];
        if p.resolution.is_local_tree() && !matches!(p.resolution, Resolution::Workspace { .. }) {
            continue;
        }
        let version = shown_version(&graph, id);
        if !matches_pattern(&pattern, &p.name, &version) {
            continue;
        }
        lines.push(format!("{}@{version}", p.name));
        let tree = if !deps.contains_key(&id) {
            lines.push("  └─ No dependents found".into());
            json!([])
        } else if top || max_depth == 0 {
            let list = &deps[&id];
            if max_depth == 0 {
                lines.push("  └─ (deeper dependencies hidden)".into());
            } else {
                for (i, d) in list.iter().enumerate() {
                    let branch = if i + 1 == list.len() {
                        "  └─ "
                    } else {
                        "  ├─ "
                    };
                    lines.push(format!("{branch}{}", dependent_line(&graph, d)));
                }
            }
            json!([])
        } else {
            let mut path = vec![id];
            let mut expanded = HashSet::new();
            why_tree(
                &graph,
                &deps,
                id,
                "  ",
                0,
                max_depth,
                &mut path,
                &mut expanded,
                &mut lines,
            )
        };
        lines.push(String::new());
        packages.push(json!({ "name": p.name, "version": version, "dependents": tree }));
    }
    if packages.is_empty() {
        return Ok(exit(
            1,
            json!({ "packages": [] }),
            vec![format!(
                "error: No packages matching '{pattern}' found in lockfile"
            )],
        ));
    }
    Ok(ok(json!({ "packages": packages }), lines))
}

fn ls_tree(
    graph: &LockGraph,
    id: PackageId,
    prefix: &str,
    seen: &mut HashSet<PackageId>,
    lines: &mut Vec<String>,
    depth: usize,
    max_depth: usize,
) -> Value {
    let p = &graph.packages[id.index()];
    let edges: Vec<_> = p
        .dependencies
        .iter()
        .filter(|e| e.target.is_some() && !e.behavior.is_peer())
        .collect();
    let mut out = Vec::new();
    for (i, e) in edges.iter().enumerate() {
        let t = e.target.expect("filtered");
        let last = i + 1 == edges.len();
        let (branch, cont) = if last {
            ("└── ", "    ")
        } else {
            ("├── ", "│   ")
        };
        let version = shown_version(graph, t);
        lines.push(format!("{prefix}{branch}{}@{version}", e.name));
        let mut node = json!({ "name": e.name, "version": version });
        if depth + 1 < max_depth && seen.insert(t) {
            let children = ls_tree(
                graph,
                t,
                &format!("{prefix}{cont}"),
                seen,
                lines,
                depth + 1,
                max_depth,
            );
            if children.as_array().is_some_and(|a| !a.is_empty()) {
                node["dependencies"] = children;
            }
        }
        out.push(node);
    }
    Value::Array(out)
}

/// `fnpm ls [--all] [--depth n] [names]`.
pub(crate) fn ls(cx: &Ctx) -> Result<CoreResult> {
    let (project, _, graph) = cx.load_locked_with("nothing to list", false)?;
    let all = cx.flag("all");
    let max_depth = cx
        .number("depth")
        .map_or(if all { usize::MAX } else { 1 }, |d| {
            usize::try_from(d).unwrap_or(usize::MAX).saturating_add(1)
        });
    let names = &cx.req.positionals;
    let reachable = graph.reachable(
        Behavior::PROD | Behavior::DEV | Behavior::OPTIONAL | Behavior::PEER,
        None,
    );
    let installed = graph
        .ids()
        .filter(|id| {
            reachable[id.index()]
                && !graph.packages[id.index()].resolution.is_local_tree()
                && !graph.packages[id.index()].is_disabled(&project.config.platform)
        })
        .count();
    // bun names the project root, also when run in a member.
    let mut lines = vec![format!(
        "{} node_modules ({installed} installed)",
        project.root_dir.display()
    )];
    if !names.is_empty() {
        // A flat list of the selected packages, wherever they are.
        let mut found = Vec::new();
        for id in graph.ids().skip(1) {
            let p = &graph.packages[id.index()];
            if names.iter().any(|n| name_matches(n, &p.name)) {
                found.push((p.name.clone(), shown_version(&graph, id)));
            }
        }
        found.sort();
        found.dedup();
        for (i, (n, v)) in found.iter().enumerate() {
            let branch = if i + 1 == found.len() {
                "└── "
            } else {
                "├── "
            };
            lines.push(format!("{branch}{n}@{v}"));
        }
        let data: Vec<Value> = found
            .iter()
            .map(|(n, v)| json!({ "name": n, "version": v }))
            .collect();
        return Ok(ok(
            json!({ "dependencies": data, "installed": installed }),
            lines,
        ));
    }
    let mut seen = HashSet::new();
    let tree = ls_tree(
        &graph,
        PackageId::ROOT,
        "",
        &mut seen,
        &mut lines,
        0,
        max_depth,
    );
    Ok(ok(
        json!({ "dependencies": tree, "installed": installed }),
        lines,
    ))
}

/// `fnpm pm bin [-g]`.
pub(crate) fn bin(cx: &Ctx) -> Result<CoreResult> {
    let project = if cx.pm.global {
        cx.project_anywhere()?
    } else {
        cx.project()?
    };
    let dir: PathBuf = if cx.pm.global {
        project.config.global_bin_dir.clone()
    } else {
        project.root_dir.join("node_modules").join(".bin")
    };
    let s = dir.to_string_lossy().into_owned();
    Ok(ok(json!({ "path": s }), vec![s]))
}

/// Which `pm hash*` command.
pub(crate) enum Hash {
    /// `pm hash`.
    Digest,
    /// `pm hash-string`.
    String,
    /// `pm hash-print`.
    Print,
}

/// `fnpm pm hash | hash-string | hash-print`.
pub(crate) fn hash(cx: &Ctx, which: Hash) -> Result<CoreResult> {
    let (_, _, graph) = cx.load_locked("nothing to hash")?;
    let text = match which {
        Hash::String => fnpm_lockfile::meta_hash_input(&graph),
        Hash::Digest | Hash::Print => {
            fnpm_lockfile::hash::format_meta_hash(&fnpm_lockfile::meta_hash(&graph))
        }
    };
    Ok(ok(json!({ "hash": text }), vec![text]))
}

/// `fnpm pm licenses`: the license of every installed registry package, from the store.
pub(crate) fn licenses(cx: &Ctx) -> Result<CoreResult> {
    let (project, _, graph) = cx.load_locked("nothing to list")?;
    let store = project.store()?;
    let mut by_license: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in graph.packages.iter().skip(1) {
        let Some(key) = crate::fetch::store_key(p) else {
            continue;
        };
        let license = p.meta.license.clone().or_else(|| {
            let entry = store.get(&key).ok().flatten()?;
            let pj: Value = serde_json::from_str(entry.index.package_json.as_deref()?).ok()?;
            match pj.get("license")? {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("type").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            }
        });
        by_license
            .entry(license.unwrap_or_else(|| "UNKNOWN".into()))
            .or_default()
            .push(format!("{}@{}", p.name, p.version));
    }
    let rows: Vec<Vec<String>> = by_license
        .iter()
        .map(|(l, pkgs)| vec![l.clone(), pkgs.len().to_string()])
        .collect();
    let lines = table::render(&["License", "Packages"], &rows, cx.unicode());
    Ok(ok(json!({ "licenses": by_license }), lines))
}

/// `fnpm-lock verify [--integrity]`.
pub(crate) fn verify(cx: &Ctx) -> Result<CoreResult> {
    let (project, ws, graph) = cx.load_locked("nothing to verify")?;
    let mut problems = fnpm_lockfile::out_of_sync(&graph, &ws);
    for p in graph.packages.iter().skip(1) {
        let needs = matches!(
            p.resolution,
            Resolution::Npm { .. }
                | Resolution::RemoteTarball { .. }
                | Resolution::Github { .. }
                | Resolution::Git { .. }
        );
        if needs && p.resolution.integrity().is_none() {
            problems.push(format!("{}: no integrity", p.key()));
        }
    }
    if let Err(e) = graph.validate() {
        problems.push(e.to_string());
    }
    let mut missing = Vec::new();
    if cx.flag("integrity") {
        let store = project.store()?;
        for p in graph.packages.iter().skip(1) {
            let Some(key) = crate::fetch::store_key(p) else {
                continue;
            };
            match store.get(&key) {
                Ok(Some(entry)) => {
                    for f in &entry.index.files {
                        let path = entry.root.join(&f.path);
                        let ok_file = std::fs::metadata(&path).is_ok_and(|m| m.len() == f.size);
                        if !ok_file {
                            problems.push(format!(
                                "{}: {} is damaged in the store",
                                p.key(),
                                f.path
                            ));
                        }
                    }
                }
                _ => missing.push(p.key()),
            }
        }
    }
    let mut lines: Vec<String> = problems.iter().map(|p| format!("error: {p}")).collect();
    for m in &missing {
        lines.push(format!("note: {m} is not in the store"));
    }
    if problems.is_empty() {
        lines.push(format!(
            "{} packages verified",
            graph.len().saturating_sub(1)
        ));
    }
    let data = json!({ "ok": problems.is_empty(), "problems": problems, "notInStore": missing });
    Ok(exit(i32::from(!problems.is_empty()), data, lines))
}

/// Reads a lockfile given as a path, else as a git revision of the project's fnpm.lock.
fn read_lock_arg(
    root: &Path,
    cwd: &Path,
    arg: &str,
    ws: &fnpm_workspace::Workspace,
) -> Result<LockGraph> {
    let path = cwd.join(arg);
    let (name, bytes) = if path.is_file() {
        let bytes = std::fs::read(&path).map_err(Error::io(&path))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (name, bytes)
    } else {
        if arg.starts_with('-') {
            return Err(Error::usage(format!(
                "{arg} is neither a lockfile nor a git revision"
            )));
        }
        let out = std::process::Command::new("git")
            .arg("show")
            .arg(format!("{arg}:./{}", fnpm_lockfile::FNPM_LOCK))
            .current_dir(root)
            .output()
            .map_err(Error::io(root))?;
        if !out.status.success() {
            return Err(Error::usage(format!(
                "{arg} is neither a lockfile nor a git revision with an fnpm.lock: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        (fnpm_lockfile::FNPM_LOCK.to_owned(), out.stdout)
    };
    let format = Format::detect(&name, &bytes).unwrap_or(Format::Fnpm);
    Ok(fnpm_lockfile::parse(format, &bytes, ws)?)
}

/// `fnpm-lock diff <a> [b]`.
pub(crate) fn diff(cx: &Ctx) -> Result<CoreResult> {
    let project = cx.project()?;
    let ws = project.workspace(&*cx.events)?;
    let Some(a) = cx.req.positionals.first() else {
        return Err(Error::usage("Missing lockfile or git revision to compare"));
    };
    let ga = read_lock_arg(&project.root_dir, &cx.req.cwd, a, &ws)?;
    let gb = match cx.req.positionals.get(1) {
        Some(b) => read_lock_arg(&project.root_dir, &cx.req.cwd, b, &ws)?,
        None => read_lock_arg(
            &project.root_dir,
            &project.root_dir,
            fnpm_lockfile::FNPM_LOCK,
            &ws,
        )?,
    };
    let d = fnpm_lockfile::diff(&ga, &gb);
    let name_only = cx.flag("nameOnly");
    let mut lines = Vec::new();
    let name_of = |s: &str| {
        fnpm_types::name::split_name_and_maybe_version(s)
            .0
            .to_owned()
    };
    for x in &d.added {
        lines.push(if name_only {
            name_of(x)
        } else {
            format!("+ {x}")
        });
    }
    for x in &d.removed {
        lines.push(if name_only {
            name_of(x)
        } else {
            format!("- {x}")
        });
    }
    for (n, from, to) in &d.changed {
        lines.push(if name_only {
            n.clone()
        } else {
            format!("~ {n} {from} -> {to}")
        });
    }
    if lines.is_empty() {
        lines.push("No changes".into());
    }
    let data = json!({
        "added": d.added,
        "removed": d.removed,
        "changed": d.changed.iter().map(|(n, f, t)| json!({ "name": n, "from": f, "to": t })).collect::<Vec<_>>(),
    });
    Ok(ok(data, lines))
}

/// `fnpm-lock print [--yarn]`.
pub(crate) fn print(cx: &Ctx) -> Result<CoreResult> {
    let (project, _, graph) = cx.load_locked("nothing to print")?;
    let text = if cx.flag("yarn") {
        fnpm_lockfile::print::yarn_v1(
            &graph,
            &fnpm_lockfile::print::YarnOptions {
                registry: project.config.registry.url.clone(),
                ..Default::default()
            },
        )
    } else {
        String::from_utf8(fnpm_lockfile::serialize(Format::Fnpm, &graph)?).unwrap_or_default()
    };
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    Ok(ok(json!({ "text": text }), lines))
}

#[cfg(test)]
mod tests {
    use super::matches_pattern;

    #[test]
    fn why_patterns() {
        assert!(matches_pattern("react", "react", "18.0.0"));
        assert!(matches_pattern("@types/*", "@types/node", "20.0.0"));
        assert!(matches_pattern("react@^18", "react", "18.2.0"));
        assert!(!matches_pattern("react@^17", "react", "18.2.0"));
        assert!(!matches_pattern("react", "react-dom", "18.2.0"));
    }
}
