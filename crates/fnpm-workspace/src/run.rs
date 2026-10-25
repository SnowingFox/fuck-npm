//! Run-side discovery for `fnpm run --filter` / `--workspaces` (bun `runtime/cli/filter_arg.rs`).
//!
//! Unlike install, this walks up from the cwd to the first package.json with `workspaces`
//! and globs `<entry>/package.json` from there; without such a root it globs
//! `**/package.json` under the cwd. A package.json without a name is kept (selectable by path
//! only) and one that fails to parse is skipped with a warning.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde_json::Value;

use crate::discover::{abs_posix, expand_glob_opts};
use crate::filter::{
    Candidate, RootSelection, Selector, quote_patterns, select_candidates, unmatched_message,
};
use crate::glob::{self, Dots, NegatedWorkspaces};
use crate::graph::{WorkspaceGraph, run_dependency_names, topo_levels_by_names};
use crate::paths::{self, relative_posix};
use crate::{Error, PackageJson, Result};

/// A package found for `run --filter`.
#[derive(Debug, Clone)]
pub struct RunPackage {
    pub package_json_path: PathBuf,
    /// Absolute, lexically normalised.
    pub dir: PathBuf,
    /// Empty when the package.json has no name.
    pub name: String,
    pub manifest: PackageJson,
}

impl RunPackage {
    /// The name, or the directory relative to `root_dir` for a package without one.
    pub fn display_name(&self, root_dir: &Path) -> String {
        if self.name.is_empty() {
            relative_posix(root_dir, &self.dir)
        } else {
            self.name.clone()
        }
    }
}

/// The packages `run --filter` / `--workspaces` selected.
#[derive(Debug, Clone)]
pub struct RunPackages {
    /// The workspace root, or the cwd when there is none.
    pub root_dir: PathBuf,
    /// Selected packages in discovery order.
    pub packages: Vec<RunPackage>,
    /// Patterns that selected nothing while others did (bun warns
    /// `No workspace packages matched the filter "x"`).
    pub unmatched: Vec<String>,
    /// Other warnings, e.g. `Failed to read "<path>", skipping this workspace package`.
    pub warnings: Vec<String>,
}

const SKIP_LIST: [&str; 2] = ["node_modules", ".git"];

impl RunPackages {
    /// Discovers packages under `cwd` and applies `filters` (or `*` minus the root when
    /// `workspaces` is set).
    pub fn discover<S: AsRef<str>>(cwd: &Path, filters: &[S], workspaces: bool) -> Result<Self> {
        let cwd = paths::absolute(cwd);
        let patterns: Vec<String> = if workspaces {
            vec!["*".to_owned()]
        } else {
            filters.iter().map(|f| f.as_ref().to_owned()).collect()
        };
        let selectors = Selector::parse_all(&patterns)?;
        let (root_dir, entries) = candidate_patterns(&cwd)?;
        let mut negated = NegatedWorkspaces::default();
        let mut globs: Vec<String> = Vec::new();
        for e in &entries {
            if !negated.take(e) {
                globs.push(e.clone());
            }
        }
        let root_posix = abs_posix(&root_dir);
        let mut warnings = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for g in &globs {
            let mut found = match expand_run_pattern(&root_posix, g) {
                Ok(found) => found,
                // bun prints the walk error and goes on with the other patterns.
                Err(e @ Error::GlobFailed { .. }) => {
                    warnings.push(format!("Error: {e}"));
                    Vec::new()
                }
                Err(e) => return Err(e),
            };
            found.sort();
            if glob::detect_glob_syntax(g) {
                found.retain(|d| !negated.excludes(&relative_posix(&root_dir, d)));
            }
            for d in found {
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
        }
        if workspaces {
            dirs.retain(|d| *d != root_dir);
        }
        let read: Vec<std::result::Result<RunPackage, String>> = dirs
            .par_iter()
            .map(|dir| {
                let pj = dir.join("package.json");
                match PackageJson::read(&pj) {
                    Ok(manifest) => Ok(RunPackage {
                        name: manifest.name.clone().unwrap_or_default(),
                        package_json_path: pj,
                        dir: dir.clone(),
                        manifest,
                    }),
                    Err(_) => Err(format!(
                        "Failed to read {}, skipping this workspace package",
                        serde_json::to_string(&pj.to_string_lossy()).unwrap_or_default()
                    )),
                }
            })
            .collect();
        let mut discovered = Vec::new();
        for r in read {
            match r {
                Ok(p) => discovered.push(p),
                Err(w) => warnings.push(w),
            }
        }
        let posix: Vec<String> = discovered.iter().map(|p| abs_posix(&p.dir)).collect();
        let candidates: Vec<Candidate<'_>> = discovered
            .iter()
            .zip(&posix)
            .map(|(p, d)| Candidate {
                name: &p.name,
                dir: d,
                is_root: false,
            })
            .collect();
        let graph = selectors.iter().any(Selector::is_relational).then(|| {
            let names: Vec<&str> = discovered.iter().map(|p| p.name.as_str()).collect();
            WorkspaceGraph::from_dependency_names(&names, |i| {
                run_dependency_names(&discovered[i].manifest)
            })
        });
        let selection = select_candidates(
            &selectors,
            &cwd,
            &candidates,
            graph.as_ref(),
            RootSelection::Implicit,
        );
        let unmatched = selection
            .unmatched_patterns
            .iter()
            .map(|&i| patterns[i].clone())
            .collect();
        let mut keep = vec![false; discovered.len()];
        for i in selection.selected {
            keep[i] = true;
        }
        let packages = discovered
            .into_iter()
            .zip(keep)
            .filter(|(_, k)| *k)
            .map(|(p, _)| p)
            .collect();
        Ok(RunPackages {
            root_dir,
            packages,
            unmatched,
            warnings,
        })
    }

    /// bun's error when nothing was selected: `No workspace packages found` for
    /// `--workspaces`, else `No workspace packages matched the filter(s) ...`.
    pub fn nothing_selected_message<S: AsRef<str>>(filters: &[S], workspaces: bool) -> String {
        if workspaces {
            "No workspace packages found".to_owned()
        } else {
            unmatched_message(filters)
        }
    }

    /// bun's error when the script is in none of the packages.
    pub fn script_not_found_message<S: AsRef<str>>(
        &self,
        script: &str,
        filters: &[S],
        workspaces: bool,
    ) -> String {
        let quoted = quote_patterns(&[script]);
        match self.packages.len() {
            1 => format!(
                "Script {quoted} not found in package \"{}\"",
                self.packages[0].display_name(&self.root_dir)
            ),
            n if workspaces => format!("Script {quoted} not found in {n} workspace packages"),
            n => format!(
                "Script {quoted} not found in {n} packages matching {}",
                quote_patterns(filters)
            ),
        }
    }

    /// Run order (bun `filter_run.rs`): levels of packages whose workspace dependencies ran in
    /// earlier levels; with any cycle, one level with every package (second value: the
    /// cycles). Indices are into [`RunPackages::packages`].
    pub fn topo_levels(&self) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
        let names: Vec<&str> = self.packages.iter().map(|p| p.name.as_str()).collect();
        topo_levels_by_names(&names, |i| run_dependency_names(&self.packages[i].manifest))
    }
}

/// `<pattern>/package.json` globbed from the root, keeping the directories that have a
/// package.json file. The run walk matches dot-directories and only skips `node_modules` and
/// `.git` (bun `glob_ignore_fn`).
fn expand_run_pattern(root_posix: &str, pattern: &str) -> Result<Vec<PathBuf>> {
    let mut dirs = if glob::detect_glob_syntax(pattern) {
        expand_glob_opts(root_posix, pattern, &SKIP_LIST, Dots::Match)?
    } else {
        vec![PathBuf::from(crate::paths::join_posix(root_posix, pattern))]
    };
    dirs.retain(|d| std::fs::metadata(d.join("package.json")).is_ok_and(|m| m.is_file()));
    Ok(dirs)
}

/// Walks up from `cwd` to the first package.json with `workspaces`; returns the root and its
/// entries, or `(cwd, ["**"])`.
fn candidate_patterns(cwd: &Path) -> Result<(PathBuf, Vec<String>)> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let pj = d.join("package.json");
        let text = match std::fs::read_to_string(&pj) {
            Ok(t) => Some(t),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::PermissionDenied
                ) =>
            {
                None
            }
            Err(source) => return Err(Error::Io { path: pj, source }),
        };
        if let Some(text) = text {
            let value =
                crate::jsonc::parse_value(&text).map_err(|message| Error::InvalidPackageJson {
                    path: pj.clone(),
                    message,
                })?;
            if let Some(ws) = value.get("workspaces") {
                let arr = match ws {
                    Value::Array(a) => a,
                    Value::Object(o) => match o.get("packages") {
                        Some(Value::Array(a)) => a,
                        _ => break,
                    },
                    _ => break,
                };
                let entries = arr
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| Error::InvalidPackageJson {
                        path: pj.clone(),
                        message:
                            "Failed to parse \"workspaces\" property: all items must be strings"
                                .to_owned(),
                    })?;
                return Ok((d.to_path_buf(), entries));
            }
        }
        dir = d.parent();
    }
    Ok((cwd.to_path_buf(), vec!["**".to_owned()]))
}
