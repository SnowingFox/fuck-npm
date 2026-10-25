//! Configuration: `.npmrc`, `bunfig.toml [install]`, `fnpm.toml`, `package.json#fnpm`,
//! environment variables and CLI flags, merged into one [`Config`].
//!
//! Precedence, lowest first (bun's order with fnpm's files inserted after bunfig):
//! user `.npmrc` < project `.npmrc` < global bunfig < project bunfig < global `fnpm.toml`
//! < project `fnpm.toml` < `package.json#fnpm` < environment < CLI.
//! See `docs/ARCHITECTURE.md#fnpm-config`.

pub mod env;
pub mod files;
pub mod npmrc;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fnpm_types::{NodeLinker, Platform};
use serde::{Deserialize, Serialize};

pub use env::Env;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid value for {key}: {message}")]
    InvalidValue { key: String, message: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/";

/// Credentials for one registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Auth {
    #[default]
    None,
    /// `_authToken` / `token`: `Authorization: Bearer <token>`.
    Token { token: String },
    /// `username` + `_password` (base64 in .npmrc): `Authorization: Basic`.
    Basic { username: String, password: String },
    /// `_auth`: already base64 `user:pass`.
    Legacy { auth: String },
}

/// A registry and its credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryScope {
    /// Always ends with `/`.
    pub url: String,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Client certificate / key paths (`certfile` / `keyfile`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkerSetting {
    /// Isolated for workspaces with configVersion 1 or an existing `node_modules/.fnpm`,
    /// hoisted otherwise (bun's `resolve_node_linker`).
    #[default]
    Auto,
    Hoisted,
    Isolated,
}

impl LinkerSetting {
    pub fn fixed(self) -> Option<NodeLinker> {
        match self {
            Self::Auto => None,
            Self::Hoisted => Some(NodeLinker::Hoisted),
            Self::Isolated => Some(NodeLinker::Isolated),
        }
    }
}

/// How files get from the store into `node_modules`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// APFS `clonefile` / Linux `FICLONE`; the macOS default.
    Clonefile,
    /// The Linux and Windows default.
    Hardlink,
    Copyfile,
    Symlink,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OfflineMode {
    #[default]
    Online,
    /// `--prefer-offline`: use any cached manifest regardless of age.
    PreferOffline,
    /// `--offline`: never touch the network.
    Offline,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManifestCacheMode {
    Disabled,
    /// Cache but always revalidate (ETag / Last-Modified).
    NoCacheControl,
    /// Honour `Cache-Control: max-age` (300 s default for the npm registry).
    #[default]
    CacheControl,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Silent,
    Quiet,
    #[default]
    Default,
    Verbose,
}

/// `--omit` (and `--production`, which omits dev).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omit {
    pub dev: bool,
    pub optional: bool,
    pub peer: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    pub http: Option<String>,
    pub https: Option<String>,
    /// `NO_PROXY` entries.
    pub no_proxy: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    /// Inline PEM certificates (`ca=` / `install.ca` / `--ca`).
    pub ca: Vec<String>,
    pub ca_file: Option<PathBuf>,
    /// `strict-ssl=false` or `NODE_TLS_REJECT_UNAUTHORIZED=0`.
    pub reject_unauthorized: bool,
    /// `--ca-store system|openssl|bundled` (`NODE_USE_SYSTEM_CA`).
    pub ca_store: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfig {
    /// Max concurrent HTTP requests (`--network-concurrency`, `FNPM_CONFIG_MAX_HTTP_REQUESTS`,
    /// `BUN_CONFIG_MAX_HTTP_REQUESTS`). Default 64; with HTTP/2 these are streams.
    pub concurrency: u16,
    /// Default 5.
    pub retries: u16,
    /// Base backoff in ms, default 50.
    pub retry_backoff_ms: u64,
    /// Default 300 s.
    pub idle_timeout_secs: u64,
    pub offline: OfflineMode,
    pub manifest_cache: ManifestCacheMode,
    pub proxy: ProxyConfig,
    pub tls: TlsConfig,
    /// `user-agent` header.
    pub user_agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockfileConfig {
    /// `install.lockfile.save` / `--no-save`.
    pub save: bool,
    /// Also write `yarn.lock` (`--yarn`, `install.lockfile.print = "yarn"`).
    pub print_yarn: bool,
    /// Read path override (`install.lockfile.path`).
    pub path: Option<PathBuf>,
    /// Write path override (`install.lockfile.savePath`).
    pub save_path: Option<PathBuf>,
}

/// The merged configuration for one invocation. Every field has a resolved value; nothing
/// downstream re-reads files or env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub cwd: PathBuf,
    /// The workspace root (or the nearest package.json directory).
    pub project_dir: PathBuf,
    /// `~/.fnpm` (`FNPM_HOME`).
    pub home_dir: PathBuf,
    /// `~/.fnpm/store/v1` (`FNPM_STORE_DIR` overrides the parent: `$FNPM_STORE_DIR/v1`).
    pub store_dir: PathBuf,
    /// `~/.fnpm/cache` (`FNPM_CACHE_DIR`, `BUN_INSTALL_CACHE_DIR`, `install.cache.dir`).
    pub cache_dir: PathBuf,
    /// `-g` installs: `~/.fnpm/global` (`FNPM_GLOBAL_DIR`, `BUN_INSTALL_GLOBAL_DIR`).
    pub global_dir: PathBuf,
    /// `-g` bins: `~/.fnpm/bin` (`FNPM_GLOBAL_BIN_DIR`, `BUN_INSTALL_BIN`).
    pub global_bin_dir: PathBuf,
    pub registry: RegistryScope,
    /// `@scope` (with the `@`) to registry.
    pub scopes: BTreeMap<String, RegistryScope>,
    /// `//host/path/:_authToken=` style entries, matched by URL prefix.
    pub url_auth: Vec<(String, Auth)>,
    pub network: NetworkConfig,
    pub linker: LinkerSetting,
    /// `None`: platform default (clonefile on macOS, hardlink elsewhere).
    pub backend: Option<Backend>,
    /// Isolated linker shares `<store>/links`. fnpm default: true (bun: false).
    pub global_store: bool,
    pub hoist: bool,
    pub hoist_pattern: Vec<String>,
    pub public_hoist_pattern: Vec<String>,
    pub link_workspace_packages: bool,
    pub frozen_lockfile: bool,
    pub production: bool,
    pub dry_run: bool,
    pub force: bool,
    pub exact: bool,
    pub ignore_scripts: bool,
    pub verify_integrity: bool,
    pub save_text_lockfile: bool,
    pub lockfile: LockfileConfig,
    pub omit: Omit,
    /// Install peer dependencies automatically (bun `install.peer`, default true).
    pub auto_install_peers: bool,
    /// Default `2 * cpus`.
    pub concurrent_scripts: usize,
    pub minimum_release_age_ms: Option<u64>,
    pub minimum_release_age_excludes: Vec<String>,
    /// The install target (`--cpu/--os/--libc`, default the running machine).
    pub platform: Platform,
    /// `install.security.scanner`.
    pub security_scanner: Option<String>,
    pub log_level: LogLevel,
    /// `CI` and friends.
    pub ci: bool,
}

/// CLI flags as the TS layer passes them (the `PmOptions` subset that affects config).
/// `None` / empty means "not given".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CliOverrides {
    pub config: Option<PathBuf>,
    pub registry: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub no_cache: bool,
    pub linker: Option<LinkerSetting>,
    pub backend: Option<Backend>,
    pub frozen_lockfile: Option<bool>,
    pub production: Option<bool>,
    pub dry_run: Option<bool>,
    pub force: Option<bool>,
    pub exact: Option<bool>,
    pub ignore_scripts: Option<bool>,
    pub verify: Option<bool>,
    pub save: Option<bool>,
    pub save_text_lockfile: Option<bool>,
    pub yarn: Option<bool>,
    pub offline: Option<bool>,
    pub prefer_offline: Option<bool>,
    pub network_concurrency: Option<u16>,
    pub concurrent_scripts: Option<usize>,
    pub omit: Vec<String>,
    pub ca: Vec<String>,
    pub cafile: Option<PathBuf>,
    pub ca_store: Option<String>,
    pub minimum_release_age_ms: Option<u64>,
    pub minimum_release_age_excludes: Vec<String>,
    pub cpu: Vec<String>,
    pub os: Vec<String>,
    pub libc: Vec<String>,
    pub log_level: Option<LogLevel>,
    pub global: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    pub cwd: PathBuf,
    pub cli: CliOverrides,
    /// A snapshot of the environment ([`Env::from_process`] in production).
    pub env: Env,
}

impl Config {
    /// Reads every source and merges them. Missing files are fine; malformed ones are errors
    /// (bun prints and continues for `.npmrc`; we return `Error::Parse` and the caller
    /// decides).
    pub fn load(_opts: &LoadOptions) -> Result<Self> {
        todo!("see ARCHITECTURE.md#fnpm-config for the merge algorithm")
    }

    /// Defaults for `cwd` with no files and an empty environment (tests).
    pub fn defaults(_cwd: &Path) -> Self {
        todo!()
    }

    /// The registry a package name is fetched from (`@scope:registry`, else the default).
    pub fn registry_for(&self, package_name: &str) -> &RegistryScope {
        package_name
            .strip_prefix('@')
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(scope, _)| self.scopes.get(&format!("@{scope}")))
            .unwrap_or(&self.registry)
    }

    /// Credentials for a request URL: the scope's auth if the URL is on that registry, else
    /// the longest matching `url_auth` prefix. Never sends credentials to another host.
    pub fn auth_for_url(&self, _url: &str) -> Option<&Auth> {
        todo!()
    }

    /// `Authorization` header value for a URL.
    pub fn authorization_header(&self, _url: &str) -> Option<String> {
        todo!()
    }
}
