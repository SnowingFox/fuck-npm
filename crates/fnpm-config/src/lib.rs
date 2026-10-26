//! Configuration: `.npmrc`, `bunfig.toml [install]`, `fnpm.toml`, `package.json#fnpm`,
//! environment variables and CLI flags, merged into one [`Config`].
//!
//! Precedence, lowest first (bun's order with fnpm's files inserted after bunfig):
//! user `.npmrc` < project `.npmrc` < global bunfig < project bunfig < global `fnpm.toml`
//! < project `fnpm.toml` < `package.json#fnpm` < environment < CLI.
//! See `docs/ARCHITECTURE.md#fnpm-config`.

#![warn(missing_docs)]

pub mod auth;
pub mod dotenv;
pub mod env;
pub mod files;
pub mod ini;
mod load;
pub mod npmrc;
pub mod publish;
pub mod redact;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use fnpm_types::{NodeLinker, Platform};
use serde::{Deserialize, Serialize};

pub use auth::{RegistryKey, RegistrySource};
pub use env::Env;
pub use files::{ConfigFile, FileKind, InstallFile, RunFile};
pub use npmrc::Npmrc;
pub use redact::redact_url;

/// The store layout version: the directory appended to the store dir (`~/.fnpm/store/v2`).
/// Bumped whenever an older fnpm could not read what a newer one writes (v2: no `files/` blob
/// directory, `index.bin` format 2).
pub const STORE_LAYOUT: &str = "v2";

/// Errors of config loading.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A config file that does not parse, or holds a value of the wrong type.
    #[error("{path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
    /// A flag or environment value that cannot be used.
    #[error("invalid value for {key}: {message}")]
    InvalidValue {
        /// The flag or variable.
        key: String,
        /// What is wrong.
        message: String,
    },
    /// A config file value bun reports by itself, followed by where it is (`\n    at <file>:<line>:<col>`).
    #[error("{0}")]
    Located(String),
    /// A credential that cannot be sent: a CR, LF or NUL would break the header line.
    #[error("{0}")]
    Credential(String),
    /// A config file named with `--config` that cannot be read.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The read error.
        #[source]
        source: std::io::Error,
    },
}

/// `Result` with [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The npm registry.
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/";

/// Credentials for one registry. `Debug` never prints the secret.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Auth {
    /// No credentials.
    #[default]
    None,
    /// `_authToken` / `token`: `Authorization: Bearer <token>`.
    Token {
        /// The token.
        token: String,
    },
    /// `username` + `_password` (base64 in .npmrc): `Authorization: Basic`.
    Basic {
        /// The user name.
        username: String,
        /// The password, in plain text.
        password: String,
    },
    /// `_auth`: already base64 `user:pass`, sent verbatim.
    Legacy {
        /// The base64 value.
        auth: String,
    },
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Token { .. } => f.write_str("Token { token: *** }"),
            Self::Basic { username, .. } => {
                write!(f, "Basic {{ username: {username:?}, password: *** }}")
            }
            Self::Legacy { .. } => f.write_str("Legacy { auth: *** }"),
        }
    }
}

impl Auth {
    /// The same kind of credential with every secret replaced by `***` (for config echoes).
    pub fn redacted(&self) -> Self {
        let mask = |s: &str| redact::redact_secret(s).to_owned();
        match self {
            Self::None => Self::None,
            Self::Token { token } => Self::Token { token: mask(token) },
            Self::Basic { username, password } => Self::Basic {
                username: username.clone(),
                password: mask(password),
            },
            Self::Legacy { auth } => Self::Legacy { auth: mask(auth) },
        }
    }
}

/// A registry and its credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryScope {
    /// Always ends with `/`.
    pub url: String,
    #[serde(default)]
    /// The credentials sent to this registry.
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// `//<key>:email` of the registry's key.
    pub email: Option<String>,
    /// Client certificate / key paths (`certfile` / `keyfile`). Like bun, fnpm has no mTLS:
    /// `.npmrc` lines for them are warned about and these stay `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// See [`cert_file`](Self::cert_file).
    pub key_file: Option<PathBuf>,
}

impl RegistryScope {
    /// A registry without credentials. A missing trailing `/` is added.
    pub fn new(url: &str) -> Self {
        Self {
            url: with_trailing_slash(url),
            auth: Auth::None,
            email: None,
            cert_file: None,
            key_file: None,
        }
    }
}

fn with_trailing_slash(url: &str) -> String {
    if url.ends_with('/') || url.contains('?') {
        url.to_owned()
    } else {
        format!("{url}/")
    }
}

/// The `node_modules` layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkerSetting {
    /// Isolated for workspaces with configVersion 1 or an existing `node_modules/.fnpm`,
    /// hoisted otherwise (bun's `resolve_node_linker`).
    #[default]
    Auto,
    /// npm-style flat `node_modules`.
    Hoisted,
    /// pnpm-style `node_modules/.fnpm` with symlinks.
    Isolated,
}

impl LinkerSetting {
    /// The linker if it does not depend on the project.
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
    /// Plain copies.
    Copyfile,
    /// Symlinks into the store.
    Symlink,
}

/// Network access.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OfflineMode {
    #[default]
    /// Use the network normally.
    Online,
    /// `--prefer-offline`: use any cached manifest regardless of age.
    PreferOffline,
    /// `--offline`: never touch the network.
    Offline,
}

/// Use of the on-disk manifest cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManifestCacheMode {
    /// Never read or write the manifest cache.
    Disabled,
    /// Cache but always revalidate (ETag / Last-Modified).
    NoCacheControl,
    /// Honour `Cache-Control: max-age` (300 s default for the npm registry).
    #[default]
    CacheControl,
}

/// Output verbosity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Errors only, no summary.
    Silent,
    /// No progress bar.
    Quiet,
    #[default]
    /// The normal output.
    Default,
    /// Extra diagnostics.
    Verbose,
}

/// `--omit` (and `--production`, which omits dev).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omit {
    /// Skip devDependencies of local packages.
    pub dev: bool,
    /// Skip optionalDependencies.
    pub optional: bool,
    /// Skip peerDependencies.
    pub peer: bool,
}

/// HTTP proxies. `Debug` masks the credentials in the proxy URLs.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    /// Proxy for `http://` requests.
    pub http: Option<String>,
    /// Proxy for `https://` requests.
    pub https: Option<String>,
    /// `NO_PROXY` entries.
    pub no_proxy: Vec<String>,
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("http", &self.http.as_deref().map(redact_url))
            .field("https", &self.https.as_deref().map(redact_url))
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

/// TLS settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    /// Inline PEM certificates (`ca=` / `install.ca` / `--ca`).
    pub ca: Vec<String>,
    /// `cafile=` / `install.cafile` / `--cafile`.
    pub ca_file: Option<PathBuf>,
    /// `false` for `strict-ssl=false` or `NODE_TLS_REJECT_UNAUTHORIZED=0`.
    pub reject_unauthorized: bool,
    /// `--ca-store system|openssl|bundled` (`NODE_USE_SYSTEM_CA=1` is `system`).
    pub ca_store: Option<String>,
    /// `NODE_EXTRA_CA_CERTS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_ca_certs: Option<PathBuf>,
    /// `SSL_CERT_FILE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssl_cert_file: Option<PathBuf>,
    /// `SSL_CERT_DIR`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssl_cert_dir: Option<PathBuf>,
}

/// HTTP client settings.
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
    /// Network access.
    pub offline: OfflineMode,
    /// Use of the manifest cache.
    pub manifest_cache: ManifestCacheMode,
    /// Proxies.
    pub proxy: ProxyConfig,
    /// TLS settings.
    pub tls: TlsConfig,
    /// `user-agent` header.
    pub user_agent: String,
}

/// Lockfile reading and writing.
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
    /// Save even when nothing changed (`install.lockfile.save` set, `--force`).
    #[serde(default)]
    pub force_save: bool,
}

/// `[run]` settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunConfig {
    /// Always `true`: fnpm has no embedded shell.
    pub use_system_shell: bool,
    /// `run.silent`: no command echo.
    pub silent: bool,
    /// `run.elide-lines`: lines shown per package in the `--filter` TUI.
    pub elide_lines: Option<usize>,
    /// `run.bun`.
    pub bun: bool,
    /// `run.noOrphans`: parent-death watchdog.
    pub no_orphans: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            use_system_shell: true,
            silent: false,
            elide_lines: None,
            bun: false,
            no_orphans: false,
        }
    }
}

/// `FNPM_FEATURE_FLAG_*` / `BUN_FEATURE_FLAG_*` switches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureFlags {
    /// Do not write or use the package index.
    pub disable_install_index: bool,
    /// Extract tarballs only after they downloaded completely.
    pub disable_streaming_install: bool,
    /// Do not use `copy_file_range`.
    pub disable_copy_file_range: bool,
    /// Do not use the `FICLONERANGE` ioctl.
    pub disable_ficlonerange: bool,
    /// Do not use the native dependency linker.
    pub disable_native_dependency_linker: bool,
    /// Run lifecycle scripts the optimizer would skip.
    pub disable_ignore_scripts_optimizer: bool,
    /// Use junctions instead of symlinks on Windows.
    pub force_windows_junctions: bool,
}

/// The merged configuration for one invocation. Every field has a resolved value; nothing
/// downstream re-reads files or env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Where fnpm was started (absolute).
    pub cwd: PathBuf,
    /// The workspace root (or the nearest package.json directory); the global dir with `-g`.
    pub project_dir: PathBuf,
    /// `~/.fnpm` (`FNPM_HOME`).
    pub home_dir: PathBuf,
    /// `~/.fnpm/store/v2` (`FNPM_STORE_DIR` overrides the parent: `$FNPM_STORE_DIR/v2`).
    pub store_dir: PathBuf,
    /// `~/.fnpm/cache` (`FNPM_CACHE_DIR`, `BUN_INSTALL_CACHE_DIR`, `install.cache.dir`).
    pub cache_dir: PathBuf,
    /// `-g` installs: `~/.fnpm/global` (`FNPM_GLOBAL_DIR`, `BUN_INSTALL_GLOBAL_DIR`).
    pub global_dir: PathBuf,
    /// `-g` bins: `~/.fnpm/bin` (`FNPM_GLOBAL_BIN_DIR`, `BUN_INSTALL_BIN`).
    pub global_bin_dir: PathBuf,
    /// The default registry.
    pub registry: RegistryScope,
    /// `@scope` (with the `@`) to registry.
    pub scopes: BTreeMap<String, RegistryScope>,
    /// `//host/path/:_authToken=` style entries keyed by npm's normalised key (`host/path/`),
    /// matched by walking the request URL like npm's `getAuth`.
    pub url_auth: Vec<(String, Auth)>,
    /// HTTP client settings.
    pub network: NetworkConfig,
    /// The `node_modules` layout.
    pub linker: LinkerSetting,
    /// `None`: platform default (clonefile on macOS, hardlink elsewhere).
    pub backend: Option<Backend>,
    /// Isolated linker shares `<store>/links` when enabled. Default: false, like bun.
    pub global_store: bool,
    /// Isolated linker: `node_modules/.fnpm/node_modules` hidden hoist (pnpm `hoist`).
    pub hoist: bool,
    /// Default `["*"]` (everything is hidden-hoisted).
    pub hoist_pattern: Vec<String>,
    /// Default empty (nothing is publicly hoisted).
    pub public_hoist_pattern: Vec<String>,
    /// An npm range satisfied by a workspace links it.
    pub link_workspace_packages: bool,
    /// Error when the lockfile would change; never save it.
    pub frozen_lockfile: bool,
    /// `--production` / `install.production`.
    pub production: bool,
    /// Resolve and report without writing anything.
    pub dry_run: bool,
    /// Ignore manifest freshness and reinstall everything.
    pub force: bool,
    /// Save exact versions (no `^`).
    pub exact: bool,
    /// Run no lifecycle scripts.
    pub ignore_scripts: bool,
    /// Check tarball integrity.
    pub verify_integrity: bool,
    /// Write the text lockfile (`fnpm.lock`).
    pub save_text_lockfile: bool,
    /// Lockfile reading and writing.
    pub lockfile: LockfileConfig,
    /// Dependency groups not to install.
    pub omit: Omit,
    /// Install peer dependencies automatically (bun `install.peer`, default true).
    pub auto_install_peers: bool,
    /// Default `2 * cpus`.
    pub concurrent_scripts: usize,
    /// Only install versions published at least this long ago.
    pub minimum_release_age_ms: Option<u64>,
    /// `name` or `name@1.2.3 || 1.2.4` entries exempt from the age gate.
    pub minimum_release_age_excludes: Vec<String>,
    /// The install target (`--cpu/--os/--libc`, default the running machine).
    pub platform: Platform,
    /// `install.security.scanner`.
    pub security_scanner: Option<String>,
    /// Output verbosity.
    pub log_level: LogLevel,
    /// `CI` and friends.
    pub ci: bool,
    /// `-g`.
    #[serde(default)]
    pub global: bool,
    /// Progress bar allowed by the environment (`FNPM_PROGRESS=0` and CI turn it off; the CLI
    /// still checks for a TTY).
    #[serde(default)]
    pub progress: bool,
    /// `false` with `FNPM_CONFIG_SKIP_LOAD_LOCKFILE`.
    #[serde(default)]
    pub load_lockfile: bool,
    /// `false` with `FNPM_CONFIG_SKIP_INSTALL_PACKAGES` and `--dry-run`.
    #[serde(default)]
    pub install_packages: bool,
    /// `false` with `--no-save` and `--dry-run`.
    #[serde(default)]
    pub save_package_json: bool,
    /// `--production`: stop at the first failed package.
    #[serde(default)]
    pub fail_early: bool,
    /// The whole package cache is off (`install.cache = false`, `cache=false`).
    #[serde(default)]
    pub cache_disabled: bool,
    /// npm `provenance` from the environment (`NPM_CONFIG_PROVENANCE`), else `.npmrc` /
    /// config files. The CLI flag and `publishConfig` take precedence in the publish command.
    #[serde(default)]
    pub provenance: Option<bool>,
    /// `[run]`.
    #[serde(default)]
    pub run: RunConfig,
    #[serde(default)]
    /// Feature-flag switches.
    pub feature_flags: FeatureFlags,
    /// `FNPM_TMPDIR` / `BUN_TMPDIR`.
    #[serde(default)]
    pub tmp_dir: Option<PathBuf>,
    /// `GITHUB_API_URL`, default `https://api.github.com`.
    #[serde(default)]
    pub github_api_url: String,
    /// Non-fatal diagnostics from loading (ignored keys, unset `$VAR`s). Not part of the
    /// config hash.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

/// CLI flags as the TS layer passes them (the `PmOptions` subset that affects config).
/// `None` / empty means "not given".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CliOverrides {
    /// `-c, --config <path>` (relative to `cwd`).
    pub config: Option<PathBuf>,
    /// `--registry`.
    pub registry: Option<String>,
    /// `--cache-dir` (relative to `cwd`).
    pub cache_dir: Option<PathBuf>,
    /// `--no-cache`.
    pub no_cache: bool,
    /// `--linker`.
    pub linker: Option<LinkerSetting>,
    /// `--backend`.
    pub backend: Option<Backend>,
    /// `--frozen-lockfile`.
    pub frozen_lockfile: Option<bool>,
    /// `-p, --production`.
    pub production: Option<bool>,
    /// `--dry-run`.
    pub dry_run: Option<bool>,
    /// `-f, --force`.
    pub force: Option<bool>,
    /// `-E, --exact`.
    pub exact: Option<bool>,
    /// `--ignore-scripts`.
    pub ignore_scripts: Option<bool>,
    /// `--no-verify` is `Some(false)`.
    pub verify: Option<bool>,
    /// `--no-save` is `Some(false)`.
    pub save: Option<bool>,
    /// `--save-text-lockfile`.
    pub save_text_lockfile: Option<bool>,
    /// `-y, --yarn`.
    pub yarn: Option<bool>,
    /// `--offline`.
    pub offline: Option<bool>,
    /// `--prefer-offline`.
    pub prefer_offline: Option<bool>,
    /// `--network-concurrency`.
    pub network_concurrency: Option<u16>,
    /// `--concurrent-scripts` (0 keeps the default).
    pub concurrent_scripts: Option<usize>,
    /// `--omit dev|optional|peer`.
    pub omit: Vec<String>,
    /// `--ca`.
    pub ca: Vec<String>,
    /// `--cafile` (relative to `cwd`).
    pub cafile: Option<PathBuf>,
    /// `--ca-store`.
    pub ca_store: Option<String>,
    /// `--minimum-release-age`, in milliseconds.
    pub minimum_release_age_ms: Option<u64>,
    /// `--minimum-release-age-excludes` (comma-separated, repeatable).
    pub minimum_release_age_excludes: Vec<String>,
    /// `--cpu`.
    pub cpu: Vec<String>,
    /// `--os`.
    pub os: Vec<String>,
    /// `--libc`.
    pub libc: Vec<String>,
    /// `--silent` / `--quiet` / `--verbose`.
    pub log_level: Option<LogLevel>,
    /// `-g, --global`.
    pub global: bool,
    /// `fnpm update`: never read the manifest cache.
    pub update: bool,
    /// `--env-file <f>...`: read only these `.env` files (relative to `cwd`).
    pub env_file: Vec<PathBuf>,
    /// `--no-env-file`: read no default `.env` file.
    pub no_env_file: bool,
}

/// Inputs of [`Config::load`].
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// Where fnpm was started.
    pub cwd: PathBuf,
    /// The command-line flags.
    pub cli: CliOverrides,
    /// A snapshot of the environment ([`Env::from_process`] in production).
    pub env: Env,
    /// The project root when the caller already knows it (the workspace root). `None`: the
    /// nearest `package.json` directory, or the workspace root above it.
    pub project_dir: Option<PathBuf>,
    /// `process.version` of the JS host, for the user agent (`v22.11.0`).
    pub node_version: Option<String>,
}

impl LoadOptions {
    /// The environment config loading and lifecycle scripts see: [`env`](Self::env) plus the
    /// project's `.env` files (or the `--env-file` ones). Variables already set win.
    pub fn effective_env(&self) -> Result<Env> {
        load::effective_env(self).map(|(env, _)| env)
    }
}

impl Config {
    /// Reads every source and merges them. Missing files are fine; malformed config files
    /// are errors (`.npmrc` is never an error, like npm and bun).
    pub fn load(opts: &LoadOptions) -> Result<Self> {
        load::load(opts)
    }

    /// Defaults for `cwd` with no files and an empty environment (tests).
    pub fn defaults(cwd: &Path) -> Self {
        load::defaults(cwd)
    }

    /// The registry a package name is fetched from (`@scope:registry`, else the default).
    pub fn registry_for(&self, package_name: &str) -> &RegistryScope {
        if self.scopes.is_empty() || !package_name.starts_with('@') {
            return &self.registry;
        }
        // The `@scope` prefix of the name is the map key, so no key has to be built.
        package_name
            .find('/')
            .and_then(|slash| self.scopes.get(&package_name[..slash]))
            .unwrap_or(&self.registry)
    }

    /// The deepest `url_auth` entry covering `url` (npm's `regFromURI` walk), http(s) only.
    fn own_url_auth(&self, url: &str) -> Option<&(String, Auth)> {
        auth::find_entry(&self.url_auth, url)
    }

    /// npm's `getAuth` for a request to `url` made on behalf of `scope`: the `.npmrc` line for
    /// the URL's own key, or else the registry's credentials if the URL is on the registry's
    /// origin. Never sends credentials to another origin, and `.npmrc` lines only apply to
    /// another URL over https.
    fn auth_with_scope<'a>(&'a self, scope: &'a RegistryScope, url: &str) -> Option<&'a Auth> {
        let own = if auth::is_https(url) {
            self.own_url_auth(url)
        } else {
            None
        };
        if scope.auth.is_some() && auth::same_origin(url, &scope.url) {
            let scope_key = self.own_url_auth(&scope.url).map(|(k, _)| k);
            return match own {
                Some((key, auth)) if scope_key != Some(key) => Some(auth),
                _ => Some(&scope.auth),
            };
        }
        own.map(|(_, auth)| auth)
    }

    /// Credentials for a request URL: the registry's auth if the URL is on that registry's
    /// origin (the registry whose path covers the URL best), else the deepest matching
    /// `url_auth` key. Never sends credentials to another host.
    pub fn auth_for_url(&self, url: &str) -> Option<&Auth> {
        let segments = auth::path_segments(url);
        let candidates = std::iter::once(&self.registry)
            .chain(self.scopes.values())
            .filter(|s| s.auth.is_some() && auth::same_origin(url, &s.url));
        let mut best: Option<(&RegistryScope, usize)> = None;
        for scope in candidates {
            let base = auth::path_segments(&scope.url);
            let depth = if segments.starts_with(&base) {
                base.len() + 1
            } else {
                0
            };
            if best.is_none_or(|(_, d)| depth > d) {
                best = Some((scope, depth));
            }
        }
        match best {
            Some((scope, _)) => self.auth_with_scope(scope, url),
            None => self.auth_with_scope(&self.registry, url),
        }
    }

    /// Credentials for a request made for `package_name` (manifest or tarball): bun's
    /// `tarball_credentials` with the package's own registry.
    pub fn auth_for_package_url(&self, package_name: &str, url: &str) -> Option<&Auth> {
        self.auth_with_scope(self.registry_for(package_name), url)
    }

    /// `Authorization` header value for a URL (`None` for non-http(s) URLs).
    pub fn authorization_header(&self, url: &str) -> Option<String> {
        if !url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
            return None;
        }
        self.auth_for_url(url).and_then(Auth::header_value)
    }

    /// A copy safe to print or log: credentials masked, userinfo and tokens in URLs masked.
    pub fn redacted(&self) -> Self {
        let scope = |s: &RegistryScope| RegistryScope {
            url: redact_url(&s.url),
            auth: s.auth.redacted(),
            email: s.email.as_ref().map(|_| "***".to_owned()),
            ..s.clone()
        };
        let mut out = self.clone();
        out.registry = scope(&self.registry);
        out.scopes = self
            .scopes
            .iter()
            .map(|(k, v)| (k.clone(), scope(v)))
            .collect();
        out.url_auth = self
            .url_auth
            .iter()
            .map(|(k, a)| (redact_url(k), a.redacted()))
            .collect();
        let proxy = &mut out.network.proxy;
        proxy.http = proxy.http.as_deref().map(redact_url);
        proxy.https = proxy.https.as_deref().map(redact_url);
        out
    }
}
