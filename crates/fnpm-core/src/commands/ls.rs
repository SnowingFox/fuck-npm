//! `fnpm ls` / `fnpm pm ls` / `fnpm list` (bun `package_manager_command.rs`, `ls`): what
//! `node_modules` has of what fnpm.lock installs, in the shape of the hoisted tree.

use std::path::Path;

use fnpm_config::LogLevel;
use fnpm_types::{LockGraph, PackageId, Resolution};
use serde_json::{Value, json};

use super::{Ctx, exit};
use crate::{CoreResult, Error, Result};

/// One `node_modules` folder of the hoisted tree.
struct Folder {
    /// `node_modules`, `node_modules/a/node_modules`, ...
    relative_path: String,
    /// `(alias, package)` of each directory in it.
    deps: Vec<(String, PackageId)>,
}

/// bun's printed resolution: the version for npm, `workspace:<path>`, the path of a local
/// folder or tarball, otherwise the resolution itself.
fn label(r: &Resolution) -> String {
    match r {
        Resolution::Root => String::new(),
        Resolution::LocalTarball { path, .. } | Resolution::Folder { path } => path.clone(),
        Resolution::Workspace { path } => format!("workspace:{path}"),
        r => r.to_string(),
    }
}

fn escape(s: &str) -> String {
    fnpm_types::text::escape_control_chars(s)
}

/// Whether a dependency is on disk (bun `InstalledFilter`). `or_in_store`: an isolated install
/// only links direct dependencies into a `node_modules`, so a tree entry may only be in
/// `node_modules/.fnpm`.
struct OnDisk<'a> {
    root: &'a Path,
    store: Option<Vec<String>>,
    basename: String,
}

impl OnDisk<'_> {
    fn store_has(&mut self, key: &str) -> bool {
        let root = self.root;
        let entries = self.store.get_or_insert_with(|| {
            let mut names: Vec<String> = std::fs::read_dir(root.join("node_modules").join(".fnpm"))
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        });
        let i = entries.partition_point(|e| e.as_str() < key);
        entries.get(i).is_some_and(|e| {
            e == key
                || (e.len() > key.len() && e.starts_with(key) && e.as_bytes()[key.len()] == b'+')
        })
    }

    fn is_installed(
        &mut self,
        graph: &LockGraph,
        dir: &str,
        alias: &str,
        id: PackageId,
        or_in_store: bool,
    ) -> bool {
        let p = &graph.packages[id.index()];
        match &p.resolution {
            // Local sources live outside of node_modules.
            Resolution::Root | Resolution::Symlink { .. } => return true,
            // A pruned checkout is missing some of the lockfile's workspaces.
            Resolution::Workspace { path } => return self.root.join(path).exists(),
            _ => {}
        }
        let mut path = self.root.to_path_buf();
        for part in dir.split('/') {
            path.push(part);
        }
        path.push(alias);
        if std::fs::symlink_metadata(&path).is_ok() && path.exists() {
            return true;
        }
        or_in_store && {
            let key = fnpm_linker::store_key::store_key(&p.name, &p.resolution, &self.basename);
            self.store_has(&key)
        }
    }
}

/// The folders of the hoisted tree, the root's first.
fn folders(tree: &fnpm_lockfile::tree::Tree) -> Vec<Folder> {
    let in_workspace: std::collections::HashSet<u32> = tree
        .workspace_roots
        .iter()
        .flat_map(|(_, ids)| ids.iter().copied())
        .collect();
    let mut out = vec![Folder {
        relative_path: "node_modules".into(),
        deps: Vec::new(),
    }];
    let mut folder_of: Vec<Option<usize>> = vec![None; tree.nodes.len()];
    for (i, node) in tree.nodes.iter().enumerate() {
        let folder = match node.parent {
            None if in_workspace.contains(&(i as u32)) => continue,
            None => 0,
            Some(parent) => match folder_of[parent as usize] {
                Some(f) => f,
                None => {
                    out.push(Folder {
                        relative_path: nested_path(&tree.nodes[parent as usize].path),
                        deps: Vec::new(),
                    });
                    folder_of[parent as usize] = Some(out.len() - 1);
                    out.len() - 1
                }
            },
        };
        out[folder].deps.push((node.name.clone(), node.package));
    }
    out
}

/// `node_modules/a/node_modules/b/node_modules` for the tree path `a/b`.
fn nested_path(tree_path: &str) -> String {
    let mut out = String::from("node_modules");
    let mut parts = tree_path.split('/').peekable();
    while let Some(part) = parts.next() {
        out.push('/');
        out.push_str(part);
        if part.starts_with('@')
            && let Some(name) = parts.next()
        {
            out.push('/');
            out.push_str(name);
        }
        out.push_str("/node_modules");
    }
    out
}

fn flat_lines(graph: &LockGraph, deps: &[(String, PackageId)], lines: &mut Vec<String>) {
    for (i, (alias, id)) in deps.iter().enumerate() {
        let branch = if i + 1 < deps.len() {
            "├──"
        } else {
            "└──"
        };
        lines.push(format!(
            "{branch} {}@{}",
            escape(alias),
            escape(&label(&graph.packages[id.index()].resolution))
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn print_folder_structure(
    graph: &LockGraph,
    directory: &Folder,
    package: Option<PackageId>,
    depth: usize,
    directories: &mut Vec<Folder>,
    more: &mut Vec<bool>,
    cwd: &str,
    lines: &mut Vec<String>,
) {
    let mut line = String::new();
    for (i, flag) in more.iter().enumerate().take(depth) {
        if i == depth - 1 {
            line.push_str(if *flag { "├──" } else { "└──" });
        } else {
            line.push_str(if *flag { "│   " } else { "    " });
        }
    }
    match package {
        Some(id) => {
            let mut path = directory.relative_path.as_str();
            if depth != 0 {
                line.push(' ');
                for _ in 0..depth {
                    if let Some(j) = path.find("node_modules") {
                        path = &path[(j + "node_modules".len() + 1).min(path.len())..];
                    }
                }
            }
            if let Some(j) = path.find("node_modules") {
                path = &path[..j.saturating_sub(1)];
            }
            line.push_str(&format!(
                "{}@{}",
                escape(path),
                escape(&label(&graph.packages[id.index()].resolution))
            ));
            lines.push(line);
        }
        None => lines.push(format!("{cwd} node_modules")),
    }

    let mut sorted = directory.deps.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let len = sorted.len();
    for (index, (alias, id)) in sorted.iter().enumerate() {
        let possible = format!("{}/{alias}/node_modules", directory.relative_path);
        if index + 1 == len {
            more[depth] = false;
        }
        let mut found = false;
        let mut i = 0;
        while i < directories.len() {
            // A node_modules is removed from the list before it is printed.
            if directories[i].relative_path == possible {
                found = true;
                let next = directories.remove(i);
                let new_depth = possible.matches("node_modules").count() - 1;
                if more.len() <= new_depth {
                    more.resize(new_depth + 1, false);
                }
                more[new_depth] = true;
                print_folder_structure(
                    graph,
                    &next,
                    Some(*id),
                    new_depth,
                    directories,
                    more,
                    cwd,
                    lines,
                );
            } else {
                i += 1;
            }
        }
        if found {
            continue;
        }
        let mut line = String::new();
        for flag in more.iter().take(depth) {
            line.push_str(if *flag { "│   " } else { "    " });
        }
        line.push_str(if more[depth] {
            "├── "
        } else {
            "└── "
        });
        line.push_str(&format!(
            "{}@{}",
            escape(alias),
            escape(&label(&graph.packages[id.index()].resolution))
        ));
        lines.push(line);
    }
}

/// `fnpm pm ls [names] [--all] [--trusted] [--depth 0]`.
pub(crate) fn ls(cx: &Ctx) -> Result<CoreResult> {
    let loud = cx.pm.log_level != LogLevel::Silent;
    let depth = cx.number("depth");
    if depth.is_some_and(|d| d != 0) {
        if loud {
            eprintln!(
                "error: fnpm pm ls --depth only accepts 0 (direct dependencies, the default). --all lists every installed package"
            );
        }
        return Err(Error::Reported);
    }
    let names = &cx.req.positionals;
    // As in `npm ls`, a name is searched for among all installed packages, and `--depth`
    // takes precedence over `--all`.
    let all = depth.is_none() && (!names.is_empty() || cx.flag("all"));
    let trusted_only = cx.flag("trusted");
    // A global listing before the first global install, or after the last one was removed:
    // the global directory with nothing in it.
    let empty_global = |dir: &std::path::Path| {
        let dir = dir.display();
        let line = if all {
            format!("{dir} node_modules")
        } else {
            format!("{dir} node_modules (0 installed)")
        };
        Ok(exit(0, json!({ "dependencies": [] }), vec![line]))
    };
    let project = match cx.project() {
        Err(Error::Workspace(fnpm_workspace::Error::NoProject(dir))) if cx.pm.global => {
            return empty_global(&dir);
        }
        other => other?,
    };
    let ws = project.workspace(&*cx.events)?;
    let lock = crate::install::load_lock(&project.config, &ws)?;
    // pm ls names only the failure.
    if let Some(failure) = &lock.parse_failure {
        return Err(failure.read_error(false));
    }
    cx.report_lock(&lock);
    let loaded = (project, ws, lock.graph);
    let (project, _, graph) = match loaded {
        (p, _, None) if cx.pm.global => return empty_global(&p.root_dir),
        (p, ws, Some(g)) => (p, ws, g),
        _ => {
            if loud {
                eprintln!("error: missing lockfile, nothing to list");
                eprintln!("note: run 'fnpm install' first");
            }
            return Err(Error::Reported);
        }
    };
    let config = &project.config;
    let view = crate::install::link_view(&graph);
    let dev = !config.production && !config.omit.dev;
    let optional = !config.omit.optional;
    let peer = !config.omit.peer;
    let enabled = |b: fnpm_types::Behavior| {
        if b.is_peer() {
            peer
        } else if b.is_optional() {
            optional
        } else if b.is_dev() {
            dev
        } else {
            true
        }
    };
    // `--production` and `--omit` exclude a dependency group, along with every package that
    // only that group needs.
    let mut include = fnpm_types::Behavior::PROD;
    for (on, b) in [
        (dev, fnpm_types::Behavior::DEV),
        (optional, fnpm_types::Behavior::OPTIONAL),
        (peer, fnpm_types::Behavior::PEER),
    ] {
        if on {
            include |= b;
        }
    }
    let wanted = view.reachable(include, None);
    let tree = fnpm_lockfile::tree::hoist(
        &view,
        &fnpm_lockfile::tree::HoistOptions::all(config.platform),
    );
    let root = project.root_dir.clone();
    let has_node_modules = root.join("node_modules").is_dir();
    let mut disk = OnDisk {
        root: &root,
        store: None,
        basename: root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let mut is_installed = |dir: &str, alias: &str, id: PackageId, or_in_store: bool| {
        !has_node_modules || disk.is_installed(&view, dir, alias, id, or_in_store)
    };
    let policy = fnpm_scripts::trust::TrustPolicy::from_graph(&view, std::iter::empty());
    let registry_for = |name: &str| config.registry_for(name).url.as_str();
    let selected = |alias: &str, id: PackageId| {
        (!trusted_only || policy.has_trusted_dependency(&view, id, &registry_for))
            && (names.is_empty() || names.iter().any(|n| n == alias))
    };

    let mut directories = folders(&tree);
    let mut installed = 0usize;
    let mut max_depth = 0usize;
    for folder in &mut directories {
        let dir = folder.relative_path.clone();
        folder.deps.retain(|(alias, id)| {
            wanted.get(id.index()).copied().unwrap_or(false) && is_installed(&dir, alias, *id, true)
        });
        installed += folder.deps.len();
        max_depth = max_depth.max(dir.matches("node_modules").count());
    }
    // bun names the project root (the global directory with -g), also when run in a member.
    let cwd = project.root_dir.display().to_string();
    let mut lines = Vec::new();
    let first = directories.remove(0);
    let listed: Vec<(String, PackageId)>;
    let found = if all {
        if trusted_only || !names.is_empty() {
            // `--trusted` and names select by name, not by position in the tree: a selected
            // package is listed even when its parent is not.
            lines.push(format!("{cwd} node_modules"));
            let mut seen = vec![false; view.packages.len()];
            let mut picked = Vec::new();
            for folder in std::iter::once(&first).chain(directories.iter()) {
                for (alias, id) in &folder.deps {
                    if !seen[id.index()] && selected(alias, *id) {
                        seen[id.index()] = true;
                        picked.push((alias.clone(), *id));
                    }
                }
            }
            picked.sort_by(|a, b| a.0.cmp(&b.0));
            flat_lines(&view, &picked, &mut lines);
            listed = picked;
            !listed.is_empty()
        } else {
            let mut more = vec![false; max_depth + 1];
            if first.deps.len() > 1 {
                more[0] = true;
            }
            print_folder_structure(
                &view,
                &first,
                None,
                0,
                &mut directories,
                &mut more,
                &cwd,
                &mut lines,
            );
            listed = first.deps.clone();
            true
        }
    } else {
        lines.push(format!("{cwd} node_modules ({installed} installed)"));
        let mut direct: Vec<(String, PackageId, bool)> = view.packages[0]
            .dependencies
            .iter()
            .filter(|e| enabled(e.behavior))
            .filter_map(|e| Some((e.name.clone(), e.target?, e.behavior.is_workspace())))
            .collect();
        direct.sort_by(|a, b| a.0.cmp(&b.0));
        // The root lists a workspace it also declares once; if it uses a member's name for
        // another package, both are listed.
        direct.dedup_by(|a, b| a.0 == b.0 && (a.1 == b.1 || !(a.2 || b.2)));
        let picked: Vec<(String, PackageId)> = direct
            .into_iter()
            .filter(|(alias, id, _)| {
                is_installed("node_modules", alias, *id, false) && selected(alias, *id)
            })
            .map(|(alias, id, _)| (alias, id))
            .collect();
        flat_lines(&view, &picked, &mut lines);
        listed = picked;
        !listed.is_empty()
    };
    let data = json!({
        "dependencies": listed
            .iter()
            .map(|(alias, id)| json!({ "name": alias, "version": label(&view.packages[id.index()].resolution) }))
            .collect::<Vec<Value>>(),
        "installed": installed,
    });
    if !found && !names.is_empty() {
        if loud {
            eprintln!("error: no packages match: {}", names.join(", "));
        }
        return Ok(exit(1, data, lines));
    }
    Ok(exit(0, data, lines))
}
