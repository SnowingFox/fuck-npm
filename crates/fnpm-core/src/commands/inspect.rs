//! Read-only views of the lockfile: `ls`, `pm bin`, `pm hash*`, and
//! `fnpm-lock verify | diff | why | print`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use fnpm_lockfile::Format;
use fnpm_types::{Behavior, LockGraph, PackageId, Resolution};
use serde_json::{Value, json};

use super::{Ctx, exit, ok};
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
    let (project, _, graph) = cx.load_locked("nothing to list")?;
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
    let mut lines = vec![format!(
        "{} node_modules ({installed} installed)",
        cx.req.cwd.display()
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
