//! The hoisted `node_modules` tree (port of bun `lockfile/Tree.rs`).
//!
//! fnpm.lock and bun.lock key packages by their hoisted install path, so serialisation needs
//! the tree; the hoisted linker materialises the same tree. One algorithm, one place.

use fnpm_types::{Behavior, LockGraph, PackageId, Platform};

/// A directory `node_modules/<...>/node_modules/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub package: PackageId,
    /// The folder name (the edge alias).
    pub name: String,
    /// `None` for the top level (`node_modules/<name>`).
    pub parent: Option<u32>,
    /// `a/b` for `node_modules/a/node_modules/b`; the key used in fnpm.lock.
    pub path: String,
}

/// Every installed directory. Node order: by depth, then path; ids are indices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    pub nodes: Vec<TreeNode>,
    /// Workspaces get their own `node_modules` for deps that conflict with the root's.
    /// `(workspace package id, node ids directly under it)`.
    pub workspace_roots: Vec<(PackageId, Vec<u32>)>,
}

#[derive(Debug, Clone)]
pub struct HoistOptions {
    /// Which edges are installed (`--omit`, `--production`).
    pub include: Behavior,
    /// Disabled packages (platform mismatch) are not placed.
    pub platform: Platform,
    /// Limit to these workspaces (`--filter`); `None` is all.
    pub workspaces: Option<Vec<PackageId>>,
}

/// npm-style hoisting with peer handling: a dependency goes as high as possible without
/// conflicting with an existing name or shadowing a peer's resolution for a dependent.
pub fn hoist(_graph: &LockGraph, _opts: &HoistOptions) -> Tree {
    todo!()
}

/// Reverse of [`hoist`]: resolve each edge of the package at `path` by node's lookup (own
/// `node_modules`, then each ancestor's) over a set of install paths. Used when reading
/// fnpm.lock, bun.lock and package-lock.json.
pub fn resolve_paths(_paths: &[(String, PackageId)], _from: &str, _name: &str) -> Option<PackageId> {
    todo!()
}
