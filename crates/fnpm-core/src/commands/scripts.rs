//! Scripts and trust: `run`, `exec`, `pm untrusted`, `pm trust`, `pm default-trusted`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use fnpm_config::LoadOptions;
use fnpm_linker::Layout;
use fnpm_scripts::multi::{MultiOptions, MultiPackage};
use fnpm_scripts::pm::Candidate;
use fnpm_scripts::run::{RunOutcome, RunRequest};
use fnpm_scripts::{RunOptions, TrustPolicy};
use fnpm_types::{LockGraph, Resolution};
use fnpm_workspace::RunPackages;
use serde_json::json;

use super::maintenance::current_layout;
use super::{Ctx, exit, ok};
use crate::project::Project;
use crate::{CoreResult, Error, Result};

/// The variables `.env` files (or `--env-file`) add on top of the process environment.
fn dotenv_vars(cx: &Ctx, project_dir: Option<PathBuf>) -> Vec<(String, String)> {
    let opts = LoadOptions {
        cwd: cx.req.cwd.clone(),
        cli: cx.pm.to_cli_overrides(),
        env: cx.env.clone(),
        project_dir,
        node_version: None,
    };
    let Ok(effective) = opts.effective_env() else {
        return Vec::new();
    };
    effective
        .iter()
        .filter(|(k, v)| cx.env.get(k) != Some(*v))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

pub(crate) fn run_options(cx: &Ctx, project_dir: &Path) -> RunOptions {
    let mut o = RunOptions::new(project_dir);
    o.env = dotenv_vars(cx, Some(project_dir.to_path_buf()));
    o.silent = cx.flag("silent");
    o.shell = cx
        .string("shell")
        .filter(|s| !matches!(s.as_str(), "" | "system" | "fnpm" | "bun"));
    o
}

fn scripts_error(e: fnpm_scripts::Error) -> Error {
    match e {
        fnpm_scripts::Error::Message(m) => Error::usage(m),
        other => Error::Scripts(other),
    }
}

/// `fnpm run [script] [args]` (with `--filter`, `--workspaces`, `--parallel`,
/// `--sequential`, `--if-present`).
pub(crate) async fn run(cx: &Ctx) -> Result<CoreResult> {
    let filters = cx.strings("filter");
    let workspaces = cx.flag("workspaces");
    let parallel = cx.flag("parallel");
    let sequential = cx.flag("sequential");
    let if_present = cx.flag("ifPresent");
    let mo = MultiOptions {
        sequential,
        no_exit_on_error: cx.flag("noExitOnError"),
        if_present: if_present || !filters.is_empty(),
        workspaces,
    };
    let names: Vec<String> = cx.req.positionals.clone();
    let root =
        fnpm_workspace::Workspace::find_root(&cx.req.cwd).unwrap_or_else(|_| cx.req.cwd.clone());
    let opts = run_options(cx, &root);

    if !filters.is_empty() || workspaces {
        let rp = RunPackages::discover(&cx.req.cwd, &filters, workspaces)?;
        for w in &rp.warnings {
            cx.events.emit(fnpm_types::Event::warn(w.clone(), None));
        }
        if rp.packages.is_empty() {
            return Err(Error::usage(RunPackages::nothing_selected_message(
                &filters, workspaces,
            )));
        }
        let script_names: Vec<String> = if parallel || sequential {
            names.clone()
        } else {
            names.first().cloned().into_iter().collect()
        };
        let (levels, _) = rp.topo_levels();
        let mut code = 0;
        for level in levels {
            let packages: Vec<MultiPackage> = level
                .iter()
                .map(|&i| {
                    let p = &rp.packages[i];
                    MultiPackage {
                        label_prefix: Some(p.display_name(&rp.root_dir)),
                        dir: p.dir.clone(),
                        package_json: Some(p.package_json_path.clone()),
                        scripts: p
                            .manifest
                            .scripts
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    }
                })
                .collect();
            let planned = match fnpm_scripts::multi::plan(&script_names, &packages, mo) {
                Ok(p) => p,
                Err(fnpm_scripts::Error::Message(m))
                    if m.contains("No scripts") || mo.if_present =>
                {
                    let _ = m;
                    continue;
                }
                Err(e) => return Err(scripts_error(e)),
            };
            if planned.is_empty() {
                continue;
            }
            let c = fnpm_scripts::multi::run(planned, &opts, mo)
                .await
                .map_err(scripts_error)?;
            if c != 0 {
                code = c;
                if !mo.no_exit_on_error {
                    break;
                }
            }
        }
        return Ok(exit(code, json!({ "exitCode": code }), Vec::new()));
    }

    if parallel || sequential {
        let Some((pj, json)) =
            fnpm_scripts::run::find_package_json(&cx.req.cwd).map_err(scripts_error)?
        else {
            return Err(Error::usage("No package.json found"));
        };
        let package = MultiPackage {
            label_prefix: None,
            dir: pj.parent().unwrap_or(&cx.req.cwd).to_path_buf(),
            package_json: Some(pj.clone()),
            scripts: fnpm_scripts::run::scripts_of(&json),
        };
        let planned = fnpm_scripts::multi::plan(&names, &[package], mo).map_err(scripts_error)?;
        let code = fnpm_scripts::multi::run(planned, &opts, mo)
            .await
            .map_err(scripts_error)?;
        return Ok(exit(code, json!({ "exitCode": code }), Vec::new()));
    }

    let Some(name) = names.first() else {
        // `fnpm run` alone lists the scripts.
        let found = fnpm_scripts::run::find_package_json(&cx.req.cwd).map_err(scripts_error)?;
        let scripts = found
            .as_ref()
            .map(|(_, j)| fnpm_scripts::run::scripts_of(j))
            .unwrap_or_default();
        let mut lines = Vec::new();
        if scripts.is_empty() {
            lines.push("No \"scripts\" in package.json".into());
        } else {
            lines.push(format!("{} scripts:", scripts.len()));
            for (k, v) in &scripts {
                lines.push(String::new());
                lines.push(format!(" fnpm run {k}"));
                lines.push(format!("   {v}"));
            }
        }
        let data: serde_json::Map<String, serde_json::Value> = scripts
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();
        return Ok(ok(json!({ "scripts": data }), lines));
    };
    let mut args: Vec<String> = names[1..].to_vec();
    args.extend(cx.req.passthrough.iter().cloned());
    let req = RunRequest {
        cwd: &cx.req.cwd,
        name,
        args: &args,
        npm_command: "run-script",
    };
    let outcome = fnpm_scripts::run::run_script(&req, &opts)
        .await
        .map_err(scripts_error)?;
    let code = outcome.exit_code(if_present);
    if outcome == RunOutcome::NotFound && !if_present {
        return Ok(exit(
            1,
            json!({ "exitCode": 1 }),
            vec![format!("error: Script not found \"{name}\"")],
        ));
    }
    Ok(exit(code, json!({ "exitCode": code }), Vec::new()))
}

/// `fnpm exec <command> [args]`.
pub(crate) async fn exec(cx: &Ctx) -> Result<CoreResult> {
    let Some(command) = cx.req.positionals.first() else {
        return Err(Error::usage("Missing a command to run"));
    };
    let root =
        fnpm_workspace::Workspace::find_root(&cx.req.cwd).unwrap_or_else(|_| cx.req.cwd.clone());
    let opts = run_options(cx, &root);
    let mut args: Vec<String> = cx.req.positionals[1..].to_vec();
    args.extend(cx.req.passthrough.iter().cloned());
    let code = fnpm_scripts::run_command(&cx.req.cwd, "exec", command, &args, &opts)
        .await
        .map_err(scripts_error)?;
    Ok(exit(code, json!({ "exitCode": code }), Vec::new()))
}

/// Where each installed package is, from the planned layout (hoisted tree or isolated
/// entries), with its `node_modules` depth.
pub(crate) fn installed(project: &Project, graph: &LockGraph) -> Result<Vec<Candidate>> {
    let (_, layout) = current_layout(project, graph)?;
    let root = &project.root_dir;
    let nm = root.join("node_modules");
    let mut out = Vec::new();
    match &layout {
        Layout::Hoisted(tree) => {
            let mut ws_base: HashMap<u32, PathBuf> = HashMap::new();
            for (ws, nodes) in &tree.workspace_roots {
                let Some(dir) = graph.get(*ws).and_then(|p| match &p.resolution {
                    Resolution::Workspace { path } => Some(root.join(path)),
                    _ => None,
                }) else {
                    continue;
                };
                for n in nodes {
                    ws_base.insert(*n, dir.join("node_modules"));
                }
            }
            let mut dirs: Vec<(PathBuf, usize)> = Vec::with_capacity(tree.nodes.len());
            for (i, node) in tree.nodes.iter().enumerate() {
                let parent = node.parent.and_then(|p| dirs.get(p as usize));
                let (base, depth) = match parent {
                    Some((dir, depth)) => (dir.join("node_modules"), depth + 1),
                    None => (
                        ws_base
                            .get(&(i as u32))
                            .cloned()
                            .unwrap_or_else(|| nm.clone()),
                        0,
                    ),
                };
                let dir = fnpm_linker::fsutil::join_name(&base, &node.name);
                dirs.push((dir.clone(), depth));
                out.push(Candidate {
                    package: node.package,
                    alias: node.name.clone(),
                    dir,
                    depth,
                    in_global_store: false,
                });
            }
        }
        Layout::Isolated(l) => {
            let fnpm = nm.join(".fnpm");
            // Depth: breadth-first from the importers.
            let mut depth: Vec<Option<usize>> = vec![None; l.entries.len()];
            let mut alias: Vec<Option<String>> = vec![None; l.entries.len()];
            let mut queue = VecDeque::new();
            for (_, deps) in &l.roots {
                for (a, e) in deps {
                    let e = *e as usize;
                    if depth[e].is_none() {
                        depth[e] = Some(0);
                        alias[e] = Some(a.clone());
                        queue.push_back(e);
                    }
                }
            }
            while let Some(e) = queue.pop_front() {
                for (a, d) in &l.entries[e].deps {
                    let d = *d as usize;
                    if depth[d].is_none() {
                        depth[d] = Some(depth[e].unwrap_or(0) + 1);
                        alias[d] = Some(a.clone());
                        queue.push_back(d);
                    }
                }
            }
            for (i, e) in l.entries.iter().enumerate() {
                let Some(p) = graph.get(e.package) else {
                    continue;
                };
                if p.resolution.is_local_tree() {
                    continue;
                }
                let dir = fnpm_linker::fsutil::join_name(
                    &fnpm.join(&e.key).join("node_modules"),
                    &p.name,
                );
                out.push(Candidate {
                    package: e.package,
                    alias: alias[i].clone().unwrap_or_else(|| p.name.clone()),
                    dir,
                    depth: depth[i].unwrap_or(0),
                    in_global_store: e.global_hash.is_some() && project.config.global_store,
                });
            }
        }
    }
    out.retain(|c| c.dir.exists());
    Ok(out)
}

fn untrusted_list(
    project: &Project,
    graph: &LockGraph,
) -> Result<Vec<fnpm_scripts::pm::Untrusted>> {
    let candidates = installed(project, graph)?;
    let policy = TrustPolicy::from_graph(graph, Vec::new());
    let config = project.config.clone();
    let rf = |n: &str| config.registry_for(n).url.as_str();
    fnpm_scripts::pm::find_untrusted(graph, &policy, candidates, &rf).map_err(scripts_error)
}

/// bun's `bun pm trust v<version>` header on stderr, printed once the project is found.
fn header(cx: &Ctx, command: &str, gap: bool) {
    if cx.req.json || cx.pm.log_level == fnpm_config::LogLevel::Silent {
        return;
    }
    let out = super::pack::out::Out::new();
    let version = cx
        .string("cliVersion")
        .unwrap_or_else(|| crate::version().to_owned());
    out.eprint(&format!(
        "{}{}\n{}",
        out.paint_err("1", &format!("fnpm {command} ")),
        out.paint_err("2", &format!("v{version}")),
        if gap { "\n" } else { "" }
    ));
}

/// `fnpm pm untrusted`.
pub(crate) fn untrusted(cx: &Ctx) -> Result<CoreResult> {
    cx.project()?;
    // bun leaves a blank line under this one.
    header(cx, "pm untrusted", true);
    let (project, _, graph) = cx.load_locked("nothing to check")?;
    let list = untrusted_list(&project, &graph)?;
    let lines = fnpm_scripts::pm::render_untrusted(&list, cx.pm.global);
    let data: Vec<serde_json::Value> = list
        .iter()
        .map(|u| {
            json!({
                "name": u.alias,
                "trustedName": u.trusted_name,
                "resolution": u.resolution,
                "scripts": u.scripts.iter().map(|(e, c)| json!({ "event": e, "command": c })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(ok(json!({ "untrusted": data }), lines))
}

/// `fnpm pm trust <names> | --all [--dry-run]`.
pub(crate) async fn trust(cx: &Ctx) -> Result<CoreResult> {
    let names = cx.req.positionals.clone();
    let all = cx.flag("all");
    cx.project()?;
    header(cx, "pm trust", false);
    let (project, ws, mut graph) = cx.load_locked("nothing to trust")?;
    let config = project.config.clone();
    fnpm_scripts::pm::check_trust_command(
        &names,
        all,
        config.ignore_scripts,
        config.frozen_lockfile,
        cx.pm.dry_run,
    )
    .map_err(scripts_error)?;
    let list = untrusted_list(&project, &graph)?;
    let plan = fnpm_scripts::pm::plan_trust(&list, &names, all).map_err(scripts_error)?;
    if cx.pm.dry_run {
        let lines = fnpm_scripts::pm::render_dry_run(&list, &plan);
        return Ok(ok(json!({ "names": plan.names_to_add }), lines));
    }
    // bun opens package.json for writing before it runs anything.
    if !plan.names_to_add.is_empty() {
        let path = project.root_dir.join("package.json");
        if let Err(e) = std::fs::OpenOptions::new().append(true).open(&path) {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                return Err(Error::usage(format!(
                    "EACCES: Permission denied: could not open \"{}\" for writing",
                    path.display()
                )));
            }
            return Err(Error::io(&path)(e));
        }
    }
    let started = std::time::Instant::now();
    let jobs = fnpm_scripts::pm::trust_jobs(&graph, &list, &plan);
    let mut opts = run_options(cx, &project.root_dir);
    opts.concurrency = config.concurrent_scripts;
    fnpm_scripts::run_jobs(jobs, &opts, &*cx.events)
        .await
        .map_err(scripts_error)?;
    if !plan.names_to_add.is_empty() {
        let path = project.root_dir.join("package.json");
        let text = std::fs::read_to_string(&path).map_err(Error::io(&path))?;
        let out = crate::install::edit_package_json_text(
            &text,
            &[crate::install::Edit::Trust {
                names: plan.names_to_add.clone(),
            }],
        )?;
        if out != text {
            crate::project::write_atomic(&path, out.as_bytes())?;
        }
        graph
            .trusted_dependencies
            .get_or_insert_with(Default::default)
            .extend(plan.names_to_add.iter().cloned());
        if config.lockfile.save {
            fnpm_lockfile::save_keeping_format(&project.root_dir, &graph, None)?;
        }
        crate::fast_path::remove(&project.root_dir, &project.meta_dir());
    }
    let _ = ws;
    let lines = fnpm_scripts::pm::render_trust_result(&list, &plan, started.elapsed());
    Ok(ok(json!({ "trusted": plan.names_to_add }), lines))
}

/// `fnpm pm default-trusted`.
pub(crate) fn default_trusted() -> Result<CoreResult> {
    let list = fnpm_scripts::trust::default_trusted();
    Ok(ok(
        json!({ "packages": list }),
        fnpm_scripts::pm::render_default_trusted(),
    ))
}
