//! The JSON payloads the TS CLI sends (`packages/fuck-npm/src/core/types.ts`). Field names
//! are camelCase; unknown fields are ignored so the CLI can add flags first.

use std::path::PathBuf;

use fnpm_config::{Backend, CliOverrides, LinkerSetting, LogLevel};
use fnpm_types::DependencyGroup;
use serde::{Deserialize, Serialize};

/// `PmOptions`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PmOptions {
    pub cwd: PathBuf,
    /// A path, or `true` for a bare `--config`.
    pub config: Option<serde_json::Value>,
    pub yarn: bool,
    pub production: bool,
    pub save: bool,
    pub ca: Vec<String>,
    pub cafile: Option<PathBuf>,
    pub dry_run: bool,
    pub frozen_lockfile: bool,
    pub force: bool,
    pub cache_dir: Option<PathBuf>,
    pub no_cache: bool,
    pub log_level: LogLevel,
    pub progress: bool,
    pub summary: bool,
    pub verify: bool,
    pub offline: bool,
    pub prefer_offline: bool,
    pub ignore_scripts: bool,
    pub trust: bool,
    pub global: bool,
    pub env_file: Vec<PathBuf>,
    pub no_env_file: bool,
    pub backend: Option<Backend>,
    pub registry: Option<String>,
    pub concurrent_scripts: Option<usize>,
    pub network_concurrency: Option<u16>,
    pub save_text_lockfile: bool,
    pub omit: Vec<String>,
    pub lockfile_only: bool,
    pub linker: Option<LinkerSetting>,
    pub minimum_release_age_ms: Option<u64>,
    pub minimum_release_age_excludes: Option<Vec<String>>,
    pub cpu: Vec<String>,
    pub os: Vec<String>,
    pub libc: Vec<String>,
    pub ca_store: Option<String>,
}

impl PmOptions {
    /// The config-relevant subset. Booleans the CLI always sends become `Some` only when they
    /// differ from the CLI default, so file/env config still applies.
    pub fn to_cli_overrides(&self) -> CliOverrides {
        todo!()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallCommand {
    #[default]
    Install,
    Add,
    Remove,
    Update,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateGroups {
    pub dev: bool,
    pub prod: bool,
    pub no_optional: bool,
}

/// `InstallOptions`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstallOptions {
    #[serde(flatten)]
    pub pm: PmOptions,
    pub command: InstallCommand,
    pub packages: Vec<String>,
    pub group: Option<DependencyGroup>,
    pub save_prod: bool,
    pub exact: bool,
    pub filter: Vec<String>,
    pub analyze: bool,
    pub only_missing: bool,
    pub catalog: Option<String>,
    pub latest: bool,
    pub recursive: bool,
    pub update_groups: UpdateGroups,
    pub depth: Option<u32>,
}

/// `CoreRequest`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CoreRequest {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub positionals: Vec<String>,
    pub passthrough: Vec<String>,
    pub options: serde_json::Map<String, serde_json::Value>,
    pub json: bool,
}

/// `CoreResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreResult {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<String>>,
}
