//! Read-only views of the lockfile: `pm bin`, `pm hash*`, and
//! `fnpm-lock verify | diff | why | print`.

use std::path::{Path, PathBuf};

use fnpm_lockfile::Format;
use fnpm_types::{LockGraph, Resolution};
use serde_json::json;

use super::{Ctx, exit, ok};
use crate::{CoreResult, Error, Result};

/// `fnpm pm bin [-g]`.
pub(crate) fn bin(cx: &Ctx) -> Result<CoreResult> {
    let dir: PathBuf = if cx.pm.global {
        // bun opens the global directory, then the global bin directory, creating both; a
        // query starts no project in them.
        // Loading the global project creates the global directory.
        let config = cx.project_anywhere()?.config;
        let bin = config.global_bin_dir.clone();
        crate::project::make_global_dir(&bin)?;
        // bun warns, in colour only, when the folder is not on $PATH.
        let out = super::pack::out::Out::new();
        let shown = bin.to_string_lossy();
        let dir = shown.trim_end_matches(['/', '\\']);
        let on_path = cx.env.get("PATH").is_none_or(|path| {
            std::env::split_paths(std::ffi::OsStr::new(path)).any(|entry| {
                let entry = entry.to_string_lossy();
                let entry = entry.trim_end_matches(['/', '\\']);
                if cfg!(windows) {
                    entry.eq_ignore_ascii_case(dir)
                } else {
                    entry == dir
                }
            })
        });
        if out.color_err && !on_path {
            out.eprint(&format!(
                "\n{}: not in $PATH\n\n",
                out.paint_err("33", "warn")
            ));
        }
        bin
    } else {
        cx.project()?.root_dir.join("node_modules").join(".bin")
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
