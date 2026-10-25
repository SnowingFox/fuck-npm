//! package.json: a typed view plus a format-preserving editor.

use std::collections::BTreeMap;
use std::path::Path;

use fnpm_types::{Behavior, Bin, Dependency, DependencyGroup, PeerMeta, PlatformConstraints, SpecError};
use serde_json::Value;

use crate::Result;

/// The fields fnpm reads. Built from the raw JSON; `raw` keeps everything (key order
/// preserved, `serde_json` has `preserve_order`).
#[derive(Debug, Clone, Default)]
pub struct PackageJson {
    pub name: Option<String>,
    pub version: Option<String>,
    /// Per group, in file order.
    pub dependencies: Vec<(String, String)>,
    pub dev_dependencies: Vec<(String, String)>,
    pub optional_dependencies: Vec<(String, String)>,
    pub peer_dependencies: Vec<(String, String)>,
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    pub bundle_dependencies: Vec<String>,
    /// `workspaces` as an array or `{ packages, catalog, catalogs, nohoist }`.
    pub workspaces: Option<Vec<String>>,
    pub bin: Bin,
    pub scripts: BTreeMap<String, String>,
    pub platform: PlatformConstraints,
    pub engines: BTreeMap<String, String>,
    pub raw: Value,
}

impl PackageJson {
    pub fn parse(_text: &str) -> Result<Self> {
        todo!("JSONC-tolerant like bun (comments, trailing commas)")
    }

    pub fn read(_path: &Path) -> Result<Self> {
        todo!()
    }

    pub fn group(&self, group: DependencyGroup) -> &[(String, String)] {
        match group {
            DependencyGroup::Prod => &self.dependencies,
            DependencyGroup::Dev => &self.dev_dependencies,
            DependencyGroup::Optional => &self.optional_dependencies,
            DependencyGroup::Peer => &self.peer_dependencies,
        }
    }

    /// All declared edges with behaviors, deduplicated by name with npm's precedence
    /// (`optionalDependencies` overrides `dependencies`; a dev entry that is also prod is prod
    /// plus dev). Peers that are also dev are installed as dev.
    pub fn dependency_list(&self, _include_dev: bool) -> Result<Vec<Dependency>, SpecError> {
        let _ = Behavior::PROD;
        todo!()
    }

    /// `raw[key]` for root-only fields: overrides, resolutions, catalog(s), patchedDependencies,
    /// trustedDependencies, packageExtensions, nativeDependencies, ignoreScripts, fnpm.
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }
}

/// Edits package.json text while keeping indentation, key order, line endings and the
/// trailing newline (bun `PackageJSONEditor.rs`).
pub struct PackageJsonEditor {
    pub text: String,
}

impl PackageJsonEditor {
    pub fn new(text: String) -> Self {
        Self { text }
    }

    /// Adds or replaces `name` in `group`, removing it from the other groups (bun's
    /// move-to-group rule). New keys are inserted in sorted position if the group is sorted.
    pub fn set_dependency(&mut self, _group: DependencyGroup, _name: &str, _spec: &str) {
        todo!()
    }

    /// Removes `name` from every group; returns whether anything changed.
    pub fn remove_dependency(&mut self, _name: &str) -> bool {
        todo!()
    }

    /// Sets a top-level or nested (`a.b.c`) key (`fnpm pkg set`, trustedDependencies).
    pub fn set_path(&mut self, _path: &str, _value: Value) {
        todo!()
    }

    pub fn delete_path(&mut self, _path: &str) -> bool {
        todo!()
    }

    pub fn finish(self) -> String {
        self.text
    }
}
