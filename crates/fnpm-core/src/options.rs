//! The JSON payloads the TS CLI sends (`packages/fuck-npm/src/core/types.ts`). Field names
//! are camelCase; unknown fields are ignored so the CLI can add flags first.

use std::path::PathBuf;

use fnpm_config::{Backend, CliOverrides, LinkerSetting, LogLevel};
use fnpm_types::DependencyGroup;
use serde::{Deserialize, Serialize};

/// `PmOptions`: bun's shared install flags. Missing fields take the CLI defaults
/// ([`PmOptions::default`]: `save`, `verify`, `progress` and `summary` on, everything else off).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PmOptions {
    /// Where the command runs (`--cwd`, absolute).
    pub cwd: PathBuf,
    /// A path, or `true` for a bare `--config`.
    pub config: Option<serde_json::Value>,
    /// `--yarn`: also write `yarn.lock`.
    pub yarn: bool,
    /// `--production`: no devDependencies, frozen lockfile.
    pub production: bool,
    /// `false` with `--no-save`.
    pub save: bool,
    /// `--ca` certificates (PEM).
    pub ca: Vec<String>,
    /// `--cafile`.
    pub cafile: Option<PathBuf>,
    /// `--dry-run`: resolve and report, write nothing.
    pub dry_run: bool,
    /// `--frozen-lockfile` (and `ci`).
    pub frozen_lockfile: bool,
    /// `--force`: re-resolve and reinstall everything.
    pub force: bool,
    /// `--cache-dir`.
    pub cache_dir: Option<PathBuf>,
    /// `--no-cache`: ignore the manifest cache.
    pub no_cache: bool,
    /// `--silent` / `--quiet` / `--verbose`.
    pub log_level: LogLevel,
    /// `false` with `--no-progress`.
    pub progress: bool,
    /// `false` with `--no-summary`.
    pub summary: bool,
    /// `false` with `--no-verify` (skip integrity checks).
    pub verify: bool,
    /// `--offline`.
    pub offline: bool,
    /// `--prefer-offline`.
    pub prefer_offline: bool,
    /// `--ignore-scripts`.
    pub ignore_scripts: bool,
    /// `--trust`: trust and run the scripts of the added packages.
    pub trust: bool,
    /// `-g, --global`.
    pub global: bool,
    /// `--env-file <f>`.
    pub env_file: Vec<PathBuf>,
    /// `--no-env-file`.
    pub no_env_file: bool,
    /// `--backend`.
    pub backend: Option<Backend>,
    /// `--registry`.
    pub registry: Option<String>,
    /// `--concurrent-scripts`.
    pub concurrent_scripts: Option<usize>,
    /// `--network-concurrency`.
    pub network_concurrency: Option<u16>,
    /// `--save-text-lockfile`.
    pub save_text_lockfile: bool,
    /// `--omit dev|optional|peer`.
    pub omit: Vec<String>,
    /// `--lockfile-only`: resolve and write the lockfile, install nothing.
    pub lockfile_only: bool,
    /// `--linker hoisted|isolated`.
    pub linker: Option<LinkerSetting>,
    /// `--minimum-release-age`, in milliseconds.
    pub minimum_release_age_ms: Option<u64>,
    /// `--minimum-release-age-excludes`.
    pub minimum_release_age_excludes: Option<Vec<String>>,
    /// `--cpu`.
    pub cpu: Vec<String>,
    /// `--os`.
    pub os: Vec<String>,
    /// `--libc`.
    pub libc: Vec<String>,
    /// `--use-system-ca` and friends.
    pub ca_store: Option<String>,
}

impl Default for PmOptions {
    fn default() -> Self {
        Self {
            cwd: PathBuf::new(),
            config: None,
            yarn: false,
            production: false,
            save: true,
            ca: Vec::new(),
            cafile: None,
            dry_run: false,
            frozen_lockfile: false,
            force: false,
            cache_dir: None,
            no_cache: false,
            log_level: LogLevel::Default,
            progress: true,
            summary: true,
            verify: true,
            offline: false,
            prefer_offline: false,
            ignore_scripts: false,
            trust: false,
            global: false,
            env_file: Vec::new(),
            no_env_file: false,
            backend: None,
            registry: None,
            concurrent_scripts: None,
            network_concurrency: None,
            save_text_lockfile: false,
            omit: Vec::new(),
            lockfile_only: false,
            linker: None,
            minimum_release_age_ms: None,
            minimum_release_age_excludes: None,
            cpu: Vec::new(),
            os: Vec::new(),
            libc: Vec::new(),
            ca_store: None,
        }
    }
}

impl PmOptions {
    /// The config-relevant subset. Booleans the CLI always sends become `Some` only when they
    /// differ from the CLI default, so file/env config still applies.
    pub fn to_cli_overrides(&self) -> CliOverrides {
        let on = |b: bool| b.then_some(true);
        CliOverrides {
            config: match &self.config {
                Some(serde_json::Value::String(p)) if !p.is_empty() => Some(PathBuf::from(p)),
                _ => None,
            },
            registry: self.registry.clone().filter(|r| !r.is_empty()),
            cache_dir: self.cache_dir.clone(),
            no_cache: self.no_cache,
            linker: self.linker,
            backend: self.backend,
            frozen_lockfile: on(self.frozen_lockfile),
            production: on(self.production),
            dry_run: on(self.dry_run),
            force: on(self.force),
            exact: None,
            ignore_scripts: on(self.ignore_scripts),
            verify: (!self.verify).then_some(false),
            save: (!self.save).then_some(false),
            save_text_lockfile: on(self.save_text_lockfile),
            yarn: on(self.yarn),
            offline: on(self.offline),
            prefer_offline: on(self.prefer_offline),
            network_concurrency: self.network_concurrency.filter(|&n| n > 0),
            concurrent_scripts: self.concurrent_scripts.filter(|&n| n > 0),
            omit: self.omit.clone(),
            ca: self.ca.clone(),
            cafile: self.cafile.clone(),
            ca_store: self.ca_store.clone(),
            minimum_release_age_ms: self.minimum_release_age_ms,
            minimum_release_age_excludes: self
                .minimum_release_age_excludes
                .clone()
                .unwrap_or_default(),
            cpu: self.cpu.clone(),
            os: self.os.clone(),
            libc: self.libc.clone(),
            log_level: (self.log_level != LogLevel::Default).then_some(self.log_level),
            global: self.global,
            update: false,
            env_file: self.env_file.clone(),
            no_env_file: self.no_env_file,
        }
    }
}

/// The install-pipeline command (`ci` is `install` with `frozenLockfile`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallCommand {
    /// `fnpm install` / `i` / `ci`.
    #[default]
    Install,
    /// `fnpm add`.
    Add,
    /// `fnpm remove`.
    Remove,
    /// `fnpm update`.
    Update,
}

/// Which declared groups `fnpm update` touches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateGroups {
    /// Only devDependencies.
    pub dev: bool,
    /// Only dependencies.
    pub prod: bool,
    /// Skip optionalDependencies.
    pub no_optional: bool,
}

/// `InstallOptions`: the payload of [`crate::install`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstallOptions {
    /// The shared flags.
    #[serde(flatten)]
    pub pm: PmOptions,
    /// Which install command.
    pub command: InstallCommand,
    /// Dependency specs (add/update) or names (remove).
    pub packages: Vec<String>,
    /// `add`: the group to write to.
    pub group: Option<DependencyGroup>,
    /// `--save-prod`.
    pub save_prod: bool,
    /// `-E, --exact`.
    pub exact: bool,
    /// `--filter` patterns.
    pub filter: Vec<String>,
    /// `--analyze`.
    pub analyze: bool,
    /// `--only-missing`: skip packages that are already declared.
    pub only_missing: bool,
    /// `--catalog [name]` (`""` is the default catalog).
    pub catalog: Option<String>,
    /// `update --latest`.
    pub latest: bool,
    /// `update -r`.
    pub recursive: bool,
    /// `update --dev / --production / --no-optional`.
    pub update_groups: UpdateGroups,
    /// `update --depth 0`.
    pub depth: Option<u32>,
}

/// `CoreRequest`: the payload of [`crate::invoke`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CoreRequest {
    /// Which install command.
    pub command: Vec<String>,
    /// Where the command runs (`--cwd`, absolute).
    pub cwd: PathBuf,
    /// Positional arguments.
    pub positionals: Vec<String>,
    /// Arguments for a child process (`run`, `exec`, `x`), verbatim.
    pub passthrough: Vec<String>,
    /// Flags, keyed by camelCase name.
    pub options: serde_json::Map<String, serde_json::Value>,
    /// `--json` output mode.
    pub json: bool,
}

/// `CoreResult`: what [`crate::invoke`] returns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreResult {
    /// The process exit code.
    pub exit_code: i32,
    /// The `--json` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Pre-rendered human output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<String>>,
}
