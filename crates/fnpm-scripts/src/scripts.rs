//! A package's lifecycle scripts and which of them run (bun `lockfile/Package/Scripts.rs`).

use std::path::Path;

use fnpm_types::{LockGraph, PackageId, Resolution};

use crate::LIFECYCLE;

/// What a package is to the installer, which decides the scripts it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptSource {
    /// The project: all six scripts.
    Root,
    /// A workspace member: preinstall, install, postinstall and prepare.
    Workspace,
    /// A git or GitHub dependency: all six scripts (it is built from source).
    Git,
    /// Any other dependency (registry, tarball, folder, link): preinstall, install, postinstall.
    Dependency,
}

impl ScriptSource {
    /// From a resolution (bun switches on `Resolution::tag`).
    pub fn from_resolution(resolution: &Resolution) -> Self {
        match resolution {
            Resolution::Root => Self::Root,
            Resolution::Workspace { .. } => Self::Workspace,
            Resolution::Git { .. } | Resolution::Github { .. } => Self::Git,
            _ => Self::Dependency,
        }
    }

    /// Not the project's or a workspace's own scripts (bun `List::is_dependency`).
    pub fn is_dependency(self) -> bool {
        matches!(self, Self::Git | Self::Dependency)
    }
}

/// The six lifecycle scripts of a package.json, in [`LIFECYCLE`] order. Empty strings count as
/// absent, like in bun.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageScripts {
    pub hooks: [Option<String>; 6],
    /// `gypfile` is not literally `false`.
    pub gypfile: bool,
}

const PREINSTALL: usize = 0;
const INSTALL: usize = 1;
const POSTINSTALL: usize = 2;
const PREPARE: usize = 4;

/// The default script of a package with a `binding.gyp` and no `install`/`preinstall` script.
pub const NODE_GYP_REBUILD: &str = "node-gyp rebuild";

impl PackageScripts {
    /// Reads the `scripts` object (string values only) and `gypfile` of a parsed package.json.
    pub fn from_json(json: &serde_json::Value) -> Self {
        let mut hooks: [Option<String>; 6] = Default::default();
        if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
            for (slot, name) in hooks.iter_mut().zip(LIFECYCLE) {
                *slot = scripts
                    .get(name)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
        }
        Self {
            hooks,
            gypfile: json.get("gypfile").and_then(|v| v.as_bool()) != Some(false),
        }
    }

    /// Reads `<dir>/package.json`. An empty file has no scripts (the install read it as `{}`).
    pub fn read(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join("package.json");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
            return Ok(Self::from_json(&serde_json::Value::Object(
                Default::default(),
            )));
        }
        Ok(Self::from_json(&crate::json::read_package_json(&path)?))
    }

    /// Any of the six is set.
    pub fn has_any(&self) -> bool {
        self.hooks.iter().any(Option::is_some)
    }

    /// The script for `event`, if set.
    pub fn get(&self, event: &str) -> Option<&str> {
        LIFECYCLE
            .iter()
            .position(|n| *n == event)
            .and_then(|i| self.hooks[i].as_deref())
    }

    /// npm's rule for the default `node-gyp rebuild` script: no `install` or `preinstall`, a
    /// `binding.gyp` in `dir`, and `gypfile` not `false`.
    pub fn wants_default_node_gyp(&self, dir: &Path) -> bool {
        self.hooks[INSTALL].is_none()
            && self.hooks[PREINSTALL].is_none()
            && self.gypfile
            && dir.join("binding.gyp").exists()
    }

    /// The `(event, command)` pairs that run, in order (bun `get_script_entries`). With
    /// `node_gyp`, `node-gyp rebuild` runs as the `install` script, followed by `postinstall`.
    pub fn entries(&self, source: ScriptSource, node_gyp: bool) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut push = |i: usize, cmd: &str| out.push((LIFECYCLE[i].to_owned(), cmd.to_owned()));
        if node_gyp {
            push(INSTALL, NODE_GYP_REBUILD);
            if let Some(s) = &self.hooks[POSTINSTALL] {
                push(POSTINSTALL, s);
            }
        } else {
            for i in PREINSTALL..=POSTINSTALL {
                if let Some(s) = &self.hooks[i] {
                    push(i, s);
                }
            }
        }
        match source {
            ScriptSource::Root | ScriptSource::Git => {
                for i in 3..6 {
                    if let Some(s) = &self.hooks[i] {
                        push(i, s);
                    }
                }
            }
            ScriptSource::Workspace => {
                if let Some(s) = &self.hooks[PREPARE] {
                    push(PREPARE, s);
                }
            }
            ScriptSource::Dependency => {}
        }
        out
    }

    /// The events an untrusted package was blocked from running (bun
    /// `get_installed_package_scripts_count`; git packages count their prepare scripts too).
    pub fn blocked_events(&self, source: ScriptSource, dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = match source {
            ScriptSource::Root | ScriptSource::Git => LIFECYCLE
                .iter()
                .zip(&self.hooks)
                .filter(|(_, h)| h.is_some())
                .map(|(n, _)| (*n).to_owned())
                .collect(),
            _ => LIFECYCLE[..3]
                .iter()
                .zip(&self.hooks)
                .filter(|(_, h)| h.is_some())
                .map(|(n, _)| (*n).to_owned())
                .collect(),
        };
        if self.wants_default_node_gyp(dir) {
            out.insert(
                out.iter()
                    .position(|e| e != "preinstall")
                    .unwrap_or(out.len()),
                "install".to_owned(),
            );
        }
        out
    }
}

/// The scripts a package in `dir` runs once it is trusted, read from its package.json
/// (bun `Scripts::create_from_package_json`). `trusted: false` leaves out the implicit
/// `node-gyp rebuild` (bun only adds it for packages that will run).
pub fn job_scripts(
    dir: &Path,
    source: ScriptSource,
    trusted: bool,
) -> std::io::Result<Vec<(String, String)>> {
    let scripts = PackageScripts::read(dir)?;
    let node_gyp = trusted && scripts.wants_default_node_gyp(dir);
    Ok(scripts.entries(source, node_gyp))
}

/// The scripts of package `id` installed at `dir`, as a [`crate::Job`] (empty `scripts` when
/// it has none). `optional` comes from the edges that reach it (all optional).
pub fn job_for(
    graph: &LockGraph,
    id: PackageId,
    dir: &Path,
    optional: bool,
) -> std::io::Result<crate::Job> {
    let pkg = graph
        .get(id)
        .ok_or_else(|| std::io::Error::other("unknown package id"))?;
    let source = ScriptSource::from_resolution(&pkg.resolution);
    let scripts = match job_scripts(dir, source, true) {
        Ok(s) => s,
        // A `link:` dependency may point to a directory without a package.json.
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && matches!(pkg.resolution, Resolution::Symlink { .. }) =>
        {
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    Ok(crate::Job {
        package: id,
        name: pkg.name.clone(),
        version: pkg.version.clone(),
        dir: dir.to_path_buf(),
        package_json: dir.join("package.json"),
        scripts,
        optional,
        // Only the root runs in the foreground; workspaces are buffered like dependencies.
        foreground: source == ScriptSource::Root,
    })
}
