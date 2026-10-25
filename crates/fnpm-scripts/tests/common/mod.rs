//! Shared helpers: lock graphs built by hand and temporary projects on disk.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use fnpm_types::{Behavior, DepEdge, LockGraph, Package, PackageId, Resolution};

pub fn root(name: &str) -> LockGraph {
    LockGraph::new(Package::new(name, "1.0.0", Resolution::Root))
}

pub fn npm(name: &str, version: &str) -> Package {
    Package::new(name, version, Resolution::npm(version, None))
}

pub fn npm_url(name: &str, version: &str, url: &str) -> Package {
    Package::new(
        name,
        version,
        Resolution::Npm {
            version: version.into(),
            tarball_url: Some(url.into()),
            integrity: None,
        },
    )
}

pub fn with_scripts(mut p: Package) -> Package {
    p.meta.has_install_script = true;
    p
}

/// `from` depends on `to` under `alias`.
pub fn edge(g: &mut LockGraph, from: PackageId, alias: &str, to: PackageId) {
    edge_with(g, from, alias, to, Behavior::PROD);
}

pub fn edge_with(
    g: &mut LockGraph,
    from: PackageId,
    alias: &str,
    to: PackageId,
    behavior: Behavior,
) {
    let mut e = DepEdge::new(alias, "*", behavior);
    e.target = Some(to);
    g.packages[from.index()].dependencies.push(e);
}

pub fn add(g: &mut LockGraph, p: Package) -> PackageId {
    g.push(p)
}

pub fn write(path: &Path, contents: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

pub fn write_pkg(dir: &Path, json: serde_json::Value) -> PathBuf {
    let path = dir.join("package.json");
    write(&path, &serde_json::to_string_pretty(&json).unwrap());
    path
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}
