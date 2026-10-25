//! `fnpm.toml`, `bunfig.toml` (`[install]`) and `package.json#fnpm`.
//!
//! All three share one schema, [`InstallFile`], which is bunfig's `[install]` table. In
//! `fnpm.toml` the keys live under `[install]` too, so a bunfig can be renamed to fnpm.toml.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScopeEntry {
    pub url: Option<String>,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// A registry value is a URL string or a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RegistryValue {
    Url(String),
    Table(ScopeEntry),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LockfileTable {
    pub save: Option<bool>,
    pub print: Option<String>,
    pub path: Option<PathBuf>,
    pub save_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CacheTable {
    pub dir: Option<PathBuf>,
    pub disable: Option<bool>,
    pub disable_manifest: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecurityTable {
    pub scanner: Option<String>,
}

/// bunfig `[install]`. Unknown keys are ignored; a value of the wrong type is ignored like bun
/// (the loader records a warning).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstallFile {
    pub registry: Option<RegistryValue>,
    pub scopes: BTreeMap<String, RegistryValue>,
    pub ca: Option<StringOrList>,
    pub cafile: Option<PathBuf>,
    pub exact: Option<bool>,
    pub dry_run: Option<bool>,
    pub production: Option<bool>,
    pub frozen_lockfile: Option<bool>,
    pub save_text_lockfile: Option<bool>,
    pub ignore_scripts: Option<bool>,
    pub global_store: Option<bool>,
    pub optional: Option<bool>,
    pub peer: Option<bool>,
    pub dev: Option<bool>,
    pub link_workspace_packages: Option<bool>,
    pub hoist: Option<bool>,
    pub offline: Option<bool>,
    pub concurrent_scripts: Option<usize>,
    pub linker: Option<String>,
    pub backend: Option<String>,
    pub lockfile: Option<LockfileTable>,
    pub global_dir: Option<PathBuf>,
    pub global_bin_dir: Option<PathBuf>,
    pub cache: Option<CacheTable>,
    pub security: Option<SecurityTable>,
    /// Seconds.
    pub minimum_release_age: Option<u64>,
    pub minimum_release_age_excludes: Vec<String>,
    pub public_hoist_pattern: Option<StringOrList>,
    pub hoist_pattern: Option<StringOrList>,
    pub network_concurrency: Option<u16>,
    pub log_level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    One(String),
    Many(Vec<String>),
}

/// Reads `[install]` from a TOML file (`bunfig.toml`, `fnpm.toml`); a non-`.toml` path is read
/// as JSONC like bun.
pub fn load_install_table(_path: &Path) -> Result<Option<InstallFile>> {
    todo!()
}

/// The global and project candidates in load order.
pub fn config_file_candidates(_cwd: &Path, _home: &Path, _xdg_config: Option<&Path>) -> Vec<PathBuf> {
    todo!("$XDG_CONFIG_HOME/.bunfig.toml | ~/.bunfig.toml, ./bunfig.toml, ~/.fnpm/fnpm.toml, ./fnpm.toml")
}
