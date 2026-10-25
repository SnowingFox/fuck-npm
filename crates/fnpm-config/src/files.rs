//! `fnpm.toml`, `bunfig.toml` (`[install]`, `[run]`) and `package.json#fnpm`.
//!
//! All three share one schema, bunfig's `[install]` table, read into an [`InstallFile`]. In
//! `fnpm.toml` the keys live under `[install]` too, so a bunfig can be renamed to fnpm.toml.
//! `.npmrc` settings are read into the same type, so every source overlays the same way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::auth::RegistrySource;
use crate::{Backend, Error, LinkerSetting, LogLevel, OfflineMode, Result};

/// One configuration layer: the settings one source (or one merged group of sources) sets.
/// Every field is "not set" by default; [`InstallFile::overlay`] applies a later layer.
/// This is bun's `Api::BunInstall`, plus fnpm-only keys and the npm network keys.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstallFile {
    /// `registry`.
    pub registry: Option<RegistrySource>,
    /// `scopes` / `@scope:registry`, keyed by scope name without the `@`.
    pub scopes: Option<BTreeMap<String, RegistrySource>>,
    /// `.npmrc` `//<key>:<opt>` credentials: npm key and the credential fields.
    pub url_auth: Vec<(String, RegistrySource)>,
    /// `.npmrc` `//<key>:email`.
    pub emails: Vec<(String, String)>,
    /// `install.lockfile.path`: the lockfile to read.
    pub lockfile_path: Option<PathBuf>,
    /// `install.lockfile.savePath`: where the lockfile is written.
    pub save_lockfile_path: Option<PathBuf>,
    /// `install.cache.dir` / `.npmrc` `cache`.
    pub cache_dir: Option<PathBuf>,
    /// `install.dryRun` / `dry-run`.
    pub dry_run: Option<bool>,
    /// `install.force`.
    pub force: Option<bool>,
    /// `install.dev`, `omit=dev` / `include=dev`.
    pub dev: Option<bool>,
    /// `install.optional` / `omit=optional`.
    pub optional: Option<bool>,
    /// `install.peer` / `omit=peer`.
    pub peer: Option<bool>,
    /// `install.lockfile.save`.
    pub save_lockfile: Option<bool>,
    /// `install.production`.
    pub production: Option<bool>,
    /// `install.lockfile.print = "yarn"` / `install.saveYarnLockfile`.
    pub save_yarn_lockfile: Option<bool>,
    /// `install.cache.disable` / `cache = false`.
    pub disable_cache: Option<bool>,
    /// `install.cache.disableManifest`.
    pub disable_manifest_cache: Option<bool>,
    /// `install.globalDir` (user files and `--config` only).
    pub global_dir: Option<PathBuf>,
    /// `install.globalBinDir` (user files and `--config` only).
    pub global_bin_dir: Option<PathBuf>,
    /// `install.frozenLockfile`.
    pub frozen_lockfile: Option<bool>,
    /// `install.exact` / `save-exact`.
    pub exact: Option<bool>,
    /// `None` for 0 (keeps the default).
    pub concurrent_scripts: Option<u32>,
    /// Absolute (resolved against the file's directory).
    pub cafile: Option<PathBuf>,
    /// Inline PEM certificates.
    pub ca: Option<Vec<String>>,
    /// `install.saveTextLockfile`.
    pub save_text_lockfile: Option<bool>,
    /// `install.ignoreScripts` / `ignore-scripts`.
    pub ignore_scripts: Option<bool>,
    /// `provenance`.
    pub provenance: Option<bool>,
    /// `install.linkWorkspacePackages` / `link-workspace-packages`.
    pub link_workspace_packages: Option<bool>,
    /// `install.linker` / `node-linker` / `install-strategy`.
    pub linker: Option<LinkerSetting>,
    /// `install.globalStore`.
    pub global_store: Option<bool>,
    /// `install.security.scanner`.
    pub security_scanner: Option<String>,
    /// `install.minimumReleaseAge`, in milliseconds.
    pub minimum_release_age_ms: Option<u64>,
    /// `install.minimumReleaseAgeExcludes`, unvalidated.
    pub minimum_release_age_excludes: Option<Vec<String>>,
    /// `install.publicHoistPattern` / `public-hoist-pattern`.
    pub public_hoist_pattern: Option<Vec<String>>,
    /// `install.hoistPattern` / `hoist-pattern`.
    pub hoist_pattern: Option<Vec<String>>,
    /// `install.hoist` / `hoist`.
    pub hoist: Option<bool>,
    /// `install.offline`.
    pub offline: Option<bool>,
    /// `install.prefer`.
    pub prefer: Option<OfflineMode>,
    /// `install.logLevel`.
    pub log_level: Option<LogLevel>,
    /// fnpm-only: `install.backend`.
    pub backend: Option<Backend>,
    /// fnpm-only: `install.networkConcurrency`.
    pub network_concurrency: Option<u16>,
    /// `.npmrc` `strict-ssl`.
    pub strict_ssl: Option<bool>,
    /// `.npmrc` `proxy`; empty turns the proxy off.
    pub proxy: Option<String>,
    /// `.npmrc` `https-proxy`; empty turns the proxy off.
    pub https_proxy: Option<String>,
    /// `.npmrc` `noproxy`.
    pub noproxy: Option<Vec<String>>,
}

macro_rules! overlay_fields {
    ($dst:ident, $src:ident; $($f:ident),* $(,)?) => {
        $( if $src.$f.is_some() { $dst.$f = $src.$f; } )*
    };
}

impl InstallFile {
    /// Applies `later` over `self` (bun `overlay_bunfig_install`): a set field replaces, scopes
    /// merge per name, `url_auth` appends, and `ca` + `cafile` are one setting.
    pub fn overlay(&mut self, later: InstallFile) {
        if later.registry.is_some() {
            self.registry = later.registry;
        }
        if let Some(scopes) = later.scopes {
            match self.scopes.as_mut().filter(|s| !s.is_empty()) {
                None => self.scopes = Some(scopes),
                Some(existing) => existing.extend(scopes),
            }
        }
        self.url_auth.extend(later.url_auth);
        self.emails.extend(later.emails);
        if later.ca.is_some() || later.cafile.is_some() {
            self.ca = later.ca;
            self.cafile = later.cafile;
        }
        overlay_fields!(self, later;
            lockfile_path, save_lockfile_path, cache_dir, dry_run, force, dev, optional, peer,
            save_lockfile, production, save_yarn_lockfile, disable_cache, disable_manifest_cache,
            global_dir, global_bin_dir, frozen_lockfile, exact, concurrent_scripts,
            save_text_lockfile, ignore_scripts, provenance, link_workspace_packages, linker,
            global_store, security_scanner, minimum_release_age_ms, minimum_release_age_excludes,
            public_hoist_pattern, hoist_pattern, hoist, offline, prefer, log_level, backend,
            network_concurrency, strict_ssl, proxy, https_proxy, noproxy,
        );
    }
}

/// `[run]` of a bunfig / fnpm.toml.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFile {
    /// `run.shell`: `true` for `"system"`, `false` for `"bun"`.
    pub use_system_shell: Option<bool>,
    /// `run.silent`.
    pub silent: Option<bool>,
    /// `run.elide-lines`.
    pub elide_lines: Option<usize>,
    /// `run.bun`.
    pub bun: Option<bool>,
    /// `run.noOrphans`.
    pub no_orphans: Option<bool>,
}

impl RunFile {
    /// Applies `later` over `self`.
    pub fn overlay(&mut self, later: RunFile) {
        overlay_fields!(self, later; use_system_shell, silent, elide_lines, bun, no_orphans);
    }
}

/// What a config file contributes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigFile {
    /// `None` when the file has no `[install]` table.
    pub install: Option<InstallFile>,
    /// `[run]`.
    pub run: RunFile,
    /// Non-fatal diagnostics (ignored keys).
    pub warnings: Vec<String>,
}

/// How a config file is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// The user's own file (`~/.bunfig.toml`, `~/.fnpm/fnpm.toml`).
    User,
    /// Comes with the project (`./bunfig.toml`, `./fnpm.toml`, `package.json#fnpm`):
    /// `globalDir`/`globalBinDir` are ignored with a warning.
    Project,
    /// Named with `--config`.
    Named,
}

/// Resolves `path` against `dir` (the directory of the file that sets it). As in npm, `~` and
/// `~/<rest>` expand to the home directory; `~user` does not.
pub fn resolve_path(dir: &Path, path: &str, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (path.strip_prefix('~'), home) {
        if rest.is_empty() {
            return home.to_path_buf();
        }
        if rest.starts_with('/') || rest.starts_with(std::path::MAIN_SEPARATOR) {
            let mut s = home.as_os_str().to_owned();
            s.push(rest);
            return PathBuf::from(s);
        }
    }
    let p = Path::new(path);
    if path.is_empty() || p.is_absolute() || !dir.is_absolute() {
        return p.to_path_buf();
    }
    normalize(&dir.join(p))
}

/// Lexically removes `.` and `..` components.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// bun's `is_package_path`: not absolute and not `./`, `../`, `.` or `..`.
fn is_package_path(p: &str) -> bool {
    if Path::new(p).is_absolute() || p.starts_with('/') {
        return false;
    }
    let dot_relative = p.starts_with("./") || p.starts_with("../") || p == "." || p == "..";
    #[cfg(windows)]
    let dot_relative = dot_relative || p.starts_with(".\\") || p.starts_with("..\\");
    !dot_relative
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// bun's `fmt::parse_seconds`: a number of seconds, or digits followed by a unit (`90m`,
/// `3d`, `2 weeks`). `None` for negative, non-finite or unrecognised values.
pub fn parse_seconds(s: &str) -> Option<f64> {
    fn parse_f64(s: &str) -> Option<f64> {
        let ok = !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
            && s.bytes().any(|b| b.is_ascii_digit());
        ok.then(|| s.parse::<f64>().ok()).flatten()
    }
    let seconds = parse_f64(s).or_else(|| {
        let digits = s
            .bytes()
            .take_while(|c| c.is_ascii_digit() || *c == b'.')
            .count();
        let unit = match s[digits..].trim_start() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
            "d" | "day" | "days" => 86_400.0,
            "w" | "week" | "weeks" => 604_800.0,
            _ => return None,
        };
        Some(parse_f64(&s[..digits])? * unit)
    })?;
    (seconds >= 0.0 && seconds.is_finite()).then_some(seconds)
}

struct Parser<'a> {
    path: &'a Path,
    dir: PathBuf,
    kind: FileKind,
    home: Option<&'a Path>,
    warnings: Vec<String>,
}

impl Parser<'_> {
    fn err(&self, message: impl Into<String>) -> Error {
        Error::Parse {
            path: self.path.to_path_buf(),
            message: message.into(),
        }
    }

    fn expect_str<'v>(&self, key: &str, v: &'v Value) -> Result<&'v str> {
        v.as_str().ok_or_else(|| {
            self.err(format!(
                "{key}: expected string but received {}",
                type_name(v)
            ))
        })
    }

    fn install_path(&self, p: &str) -> PathBuf {
        resolve_path(&self.dir, p, self.home)
    }

    fn registry_object(
        &self,
        key: &str,
        obj: &serde_json::Map<String, Value>,
    ) -> Result<RegistrySource> {
        // `user:pass@` / `:token@` in the URL are credentials, as in the string form.
        let mut registry = match obj.get("url") {
            Some(url) => RegistrySource::from_url(self.expect_str(key, url)?),
            None => RegistrySource::default(),
        };
        let username = obj.get("username");
        let password = obj.get("password");
        let token = obj.get("token");
        // Keys replace the URL's credentials as a set.
        if username.is_some() || password.is_some() || token.is_some() {
            registry = RegistrySource {
                url: registry.url,
                ..Default::default()
            };
        }
        for (value, field) in [
            (username, &mut registry.username),
            (password, &mut registry.password),
            (token, &mut registry.token),
        ] {
            if let Some(value) = value {
                *field = self.expect_str(key, value)?.to_owned();
            }
        }
        Ok(registry)
    }

    fn registry(&self, key: &str, v: &Value) -> Result<RegistrySource> {
        let source = match v {
            Value::String(s) => RegistrySource::from_url(s),
            Value::Object(o) => self.registry_object(key, o)?,
            _ => {
                return Err(self.err(format!(
                    "{key}: Expected registry to be a URL string or an object"
                )));
            }
        };
        // A newline in a header is refused when the scope is built; reject it right here too
        // so the error names the file.
        let fields = [
            &source.url,
            &source.username,
            &source.password,
            &source.token,
        ];
        if fields.iter().any(|f| f.contains(['\r', '\n', '\0'])) {
            return Err(self.err(format!("{key} contains a newline or NUL byte")));
        }
        Ok(source)
    }

    fn patterns(&self, key: &str, v: &Value) -> Result<Vec<String>> {
        let items: Vec<&Value> = match v {
            Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        items
            .into_iter()
            .map(|i| i.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| self.err(format!("{key}: Expected a string or an array of strings")))
    }

    fn install(&mut self, obj: &serde_json::Map<String, Value>) -> Result<InstallFile> {
        let mut install = InstallFile::default();
        let bool_of = |k: &str| obj.get(k).and_then(Value::as_bool);

        let cafile = obj.get("cafile");
        let ca = obj.get("ca");
        if let Some(cafile) = cafile {
            let s = cafile
                .as_str()
                .ok_or_else(|| self.err("Invalid cafile. Expected a string."))?;
            install.cafile = Some(self.install_path(s));
        }
        if let Some(ca) = ca {
            install.ca = Some(match ca {
                Value::Array(items) => items
                    .iter()
                    .map(|i| i.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| self.err("Invalid CA. Expected a string."))?,
                Value::String(s) => vec![s.clone()],
                _ => return Err(self.err("Invalid CA. Expected a string or an array of strings.")),
            });
        }

        install.exact = bool_of("exact");
        if let Some(v) = obj.get("registry") {
            install.registry = Some(self.registry("registry", v)?);
        }
        if let Some(scopes) = obj.get("scopes") {
            let Value::Object(map) = scopes else {
                return Err(self.err(format!(
                    "scopes: expected object but received {}",
                    type_name(scopes)
                )));
            };
            let mut out = BTreeMap::new();
            for (name, value) in map {
                if name.is_empty() {
                    continue;
                }
                let name = name.strip_prefix('@').unwrap_or(name);
                out.insert(
                    name.to_owned(),
                    self.registry(&format!("scopes.{name}"), value)?,
                );
            }
            install.scopes = Some(out);
        }
        install.dry_run = bool_of("dryRun");
        install.production = bool_of("production");
        install.frozen_lockfile = bool_of("frozenLockfile");
        install.save_text_lockfile = bool_of("saveTextLockfile");
        if let Some(n) = obj.get("concurrentScripts").and_then(Value::as_f64) {
            let n = n as u32;
            install.concurrent_scripts = (n != 0).then_some(n);
        }
        install.ignore_scripts = bool_of("ignoreScripts");
        if let Some(v) = obj.get("linker") {
            install.linker = Some(match self.expect_str("linker", v)? {
                "isolated" => LinkerSetting::Isolated,
                "hoisted" => LinkerSetting::Hoisted,
                _ => return Err(self.err("linker: Expected one of \"isolated\" or \"hoisted\"")),
            });
        }
        install.global_store = bool_of("globalStore");

        if let Some(lockfile) = obj.get("lockfile") {
            if let Some(print) = lockfile.get("print") {
                let value = self.expect_str("lockfile.print", print)?;
                if value != "bun" && value != "fnpm" {
                    if value != "yarn" {
                        return Err(
                            self.err("Invalid lockfile format, only 'yarn' output is implemented")
                        );
                    }
                    install.save_yarn_lockfile = Some(true);
                }
            }
            install.save_lockfile = lockfile.get("save").and_then(Value::as_bool);
            install.lockfile_path = lockfile
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            install.save_lockfile_path = lockfile
                .get("savePath")
                .and_then(Value::as_str)
                .map(PathBuf::from);
        }

        install.optional = bool_of("optional");
        install.peer = bool_of("peer");
        install.dev = bool_of("dev");

        for (key, env_var) in [
            ("globalDir", "FNPM_GLOBAL_DIR"),
            ("globalBinDir", "FNPM_GLOBAL_BIN_DIR"),
        ] {
            let Some(v) = obj.get(key) else { continue };
            if self.kind == FileKind::Project {
                let name = self
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.warnings.push(format!(
                    "ignoring \"{key}\" in project {name}, set it in $HOME/.fnpm/fnpm.toml or ${env_var}"
                ));
                continue;
            }
            if let Some(s) = v.as_str() {
                let p = Some(self.install_path(s));
                if key == "globalDir" {
                    install.global_dir = p;
                } else {
                    install.global_bin_dir = p;
                }
            }
        }

        if let Some(cache) = obj.get("cache") {
            match cache {
                Value::Bool(b) => {
                    if !b {
                        install.disable_cache = Some(true);
                        install.disable_manifest_cache = Some(true);
                    }
                }
                Value::String(s) => install.cache_dir = Some(self.install_path(s)),
                Value::Object(c) => {
                    install.disable_cache = c.get("disable").and_then(Value::as_bool);
                    install.disable_manifest_cache =
                        c.get("disableManifest").and_then(Value::as_bool);
                    install.cache_dir = c
                        .get("dir")
                        .and_then(Value::as_str)
                        .map(|s| self.install_path(s));
                }
                _ => {}
            }
        }

        install.link_workspace_packages = bool_of("linkWorkspacePackages");

        if let Some(security) = obj.get("security") {
            let Value::Object(security) = security else {
                return Err(self.err("Invalid security config, expected an object"));
            };
            if let Some(scanner) = security.get("scanner") {
                let name = self.expect_str("security.scanner", scanner)?;
                install.security_scanner =
                    Some(if self.kind == FileKind::Project || is_package_path(name) {
                        name.to_owned()
                    } else {
                        self.install_path(name).to_string_lossy().replace('\\', "/")
                    });
            }
        }

        if let Some(age) = obj.get("minimumReleaseAge") {
            let seconds = match age {
                Value::Number(n) => {
                    let s = n.as_f64().unwrap_or(f64::NAN);
                    if !(s >= 0.0 && s.is_finite()) {
                        return Err(
                            self.err("Expected positive number of seconds for minimumReleaseAge")
                        );
                    }
                    s
                }
                Value::String(s) => parse_seconds(s)
                    .ok_or_else(|| self.err("Expected number of seconds for minimumReleaseAge"))?,
                _ => return Err(self.err("Expected number of seconds for minimumReleaseAge")),
            };
            install.minimum_release_age_ms = Some((seconds * 1000.0).round() as u64);
        }

        if let Some(excludes) = obj.get("minimumReleaseAgeExcludes") {
            let Value::Array(items) = excludes else {
                return Err(self.err("Expected array for minimumReleaseAgeExcludes"));
            };
            if !items.is_empty() {
                let list = items
                    .iter()
                    .map(|i| {
                        self.expect_str("minimumReleaseAgeExcludes", i)
                            .map(str::to_owned)
                    })
                    .collect::<Result<Vec<_>>>()?;
                install.minimum_release_age_excludes = Some(list);
            }
        }

        if let Some(v) = obj.get("publicHoistPattern") {
            install.public_hoist_pattern = Some(self.patterns("publicHoistPattern", v)?);
        }
        if let Some(v) = obj.get("hoistPattern") {
            install.hoist_pattern = Some(self.patterns("hoistPattern", v)?);
        }
        install.hoist = bool_of("hoist");
        install.offline = bool_of("offline");

        if let Some(v) = obj.get("prefer") {
            install.prefer = Some(match self.expect_str("prefer", v)? {
                "offline" => OfflineMode::PreferOffline,
                "online" | "latest" => OfflineMode::Online,
                _ => {
                    return Err(
                        self.err("Invalid prefer setting, must be one of online or offline")
                    );
                }
            });
        }
        if let Some(v) = obj.get("logLevel") {
            install.log_level = Some(match self.expect_str("logLevel", v)? {
                "debug" => LogLevel::Verbose,
                "info" | "warn" => LogLevel::Default,
                "error" => LogLevel::Quiet,
                _ => {
                    return Err(self.err("Invalid log level, must be one of debug, error, or warn"));
                }
            });
        }

        // Keys bun reads only from the CLI or elsewhere; fnpm accepts them in the file too.
        install.force = bool_of("force");
        install.save_yarn_lockfile = bool_of("saveYarnLockfile").or(install.save_yarn_lockfile);
        install.provenance = bool_of("provenance");

        if let Some(v) = obj.get("backend") {
            install.backend = Some(match self.expect_str("backend", v)? {
                "clonefile" | "clonefile_each_dir" => Backend::Clonefile,
                "hardlink" => Backend::Hardlink,
                "copyfile" => Backend::Copyfile,
                "symlink" => Backend::Symlink,
                _ => {
                    return Err(
                        self.err("backend: Expected one of \"clonefile\", \"hardlink\", \"copyfile\" or \"symlink\"")
                    )
                }
            });
        }
        if let Some(v) = obj.get("networkConcurrency") {
            let n = v
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 65535.0)
                .ok_or_else(|| {
                    self.err("networkConcurrency: Expected a number between 0 and 65535")
                })?;
            install.network_concurrency = Some((n as u16).max(1));
        }
        Ok(install)
    }

    fn run(&mut self, run: &Value) -> Result<RunFile> {
        let mut out = RunFile::default();
        let get = |k: &str| run.get(k);
        if let Some(v) = get("silent") {
            out.silent = Some(
                v.as_bool()
                    .ok_or_else(|| self.err("run.silent: Expected boolean"))?,
            );
        }
        if let Some(v) = get("elide-lines") {
            let n = v
                .as_f64()
                .ok_or_else(|| self.err("run.elide-lines: Expected number"))?;
            out.elide_lines = Some(if n.is_nan() || n < 0.0 { 0 } else { n as usize });
        }
        if let Some(v) = get("shell") {
            let s = v
                .as_str()
                .ok_or_else(|| self.err("run.shell: Expected string"))?;
            out.use_system_shell = Some(match s {
                "system" => true,
                "bun" => {
                    self.warnings.push(
                        "run.shell = \"bun\": fnpm has no embedded shell; scripts run in the system shell".to_owned(),
                    );
                    true
                }
                _ => return Err(self.err("Invalid shell, only 'bun' and 'system' are supported")),
            });
        }
        if let Some(v) = get("bun") {
            out.bun = Some(
                v.as_bool()
                    .ok_or_else(|| self.err("run.bun: Expected boolean"))?,
            );
        }
        if let Some(v) = get("noOrphans") {
            out.no_orphans = Some(
                v.as_bool()
                    .ok_or_else(|| self.err("run.noOrphans: Expected boolean"))?,
            );
        }
        Ok(out)
    }
}

/// Removes `//` and `/* */` comments and trailing commas outside strings (tsconfig-style
/// JSONC, what bun's `parse_ts_config` accepts).
pub(crate) fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut start = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                out.push_str(&text[start..i]);
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                start = i;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.push_str(&text[start..i]);
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                out.push(' ');
                start = i;
            }
            b',' => {
                let mut j = i + 1;
                // Skip whitespace and comments to see whether a closing bracket follows.
                loop {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'/') {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                    } else if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'*') {
                        j += 2;
                        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                            j += 1;
                        }
                        j = (j + 2).min(bytes.len());
                    } else {
                        break;
                    }
                }
                if matches!(bytes.get(j), Some(b'}' | b']')) {
                    out.push_str(&text[start..i]);
                    start = i + 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[start..]);
    out
}

fn toml_to_json(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}

/// Parses a config document: TOML for a `.toml` path, JSONC otherwise.
fn parse_document(path: &Path, text: &str) -> Result<Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let is_toml = path.extension().is_some_and(|e| e == "toml");
    let err = |message: String| Error::Parse {
        path: path.to_path_buf(),
        message,
    };
    let root = if is_toml {
        let table: toml::Table =
            toml::from_str(text).map_err(|e| err(format!("Failed to parse: {}", e.message())))?;
        toml_to_json(toml::Value::Table(table))
    } else {
        serde_json::from_str(&strip_jsonc(text))
            .map_err(|e| err(format!("Failed to parse: {e}")))?
    };
    if !root.is_object() {
        return Err(err("bunfig expects an object { } at the root".to_owned()));
    }
    Ok(root)
}

/// Parses the text of a config file. `path` gives the format (`.toml` or JSONC) and the
/// directory relative paths resolve against.
pub fn parse_config_file(
    path: &Path,
    text: &str,
    kind: FileKind,
    home: Option<&Path>,
) -> Result<ConfigFile> {
    let root = parse_document(path, text)?;
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut parser = Parser {
        path,
        dir,
        kind,
        home,
        warnings: Vec::new(),
    };
    let install = match root.get("install") {
        Some(Value::Object(obj)) => Some(parser.install(obj)?),
        _ => None,
    };
    let run = match root.get("run") {
        Some(run) => parser.run(run)?,
        None => RunFile::default(),
    };
    Ok(ConfigFile {
        install,
        run,
        warnings: parser.warnings,
    })
}

/// Reads a config file. A missing file is `Ok(None)`; an automatically found file that cannot
/// be read is skipped like npm (`Ok(None)`), while a [`FileKind::Named`] one is an error.
/// Malformed content is always an error.
pub fn load_config_file(
    path: &Path,
    kind: FileKind,
    home: Option<&Path>,
) -> Result<Option<ConfigFile>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            parse_config_file(path, &text, kind, home).map(Some)
        }
        Err(e) if kind == FileKind::Named => Err(Error::Io {
            path: path.to_path_buf(),
            source: e,
        }),
        Err(_) => Ok(None),
    }
}

/// Reads `[install]` from a TOML file (`bunfig.toml`, `fnpm.toml`); a non-`.toml` path is read
/// as JSONC like bun. `Ok(None)` when the file is missing or has no `[install]`.
pub fn load_install_table(path: &Path) -> Result<Option<InstallFile>> {
    let home = crate::Env::from_process().home();
    Ok(load_config_file(path, FileKind::User, home.as_deref())?.and_then(|f| f.install))
}

/// `package.json#fnpm`: the same keys as `[install]`, camelCase JSON. A package.json that is
/// missing, unreadable or not JSON contributes nothing here (the workspace loader reports it).
pub fn load_package_json_config(
    project_dir: &Path,
    home: Option<&Path>,
) -> Result<Option<ConfigFile>> {
    let path = project_dir.join("package.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(None);
    };
    let Ok(json) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(None);
    };
    let Some(Value::Object(obj)) = json.get("fnpm") else {
        return Ok(None);
    };
    let mut parser = Parser {
        path: &path,
        dir: project_dir.to_path_buf(),
        kind: FileKind::Project,
        home,
        warnings: Vec::new(),
    };
    let install = parser.install(obj)?;
    Ok(Some(ConfigFile {
        install: Some(install),
        run: RunFile::default(),
        warnings: parser.warnings,
    }))
}

/// The user's global config files, lowest precedence first: bun's `$XDG_CONFIG_HOME/.bunfig.toml`
/// (or `$HOME/.bunfig.toml`), then `$XDG_CONFIG_HOME/.fnpm.toml` (or `$HOME/.fnpm.toml`), then
/// `<fnpm home>/fnpm.toml`.
pub fn global_config_candidates(
    home: Option<&Path>,
    xdg_config: Option<&Path>,
    fnpm_home: &Path,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = xdg_config
        .filter(|d| d.is_absolute())
        .or(home.filter(|d| d.is_absolute()))
    {
        out.push(dir.join(".bunfig.toml"));
        out.push(dir.join(".fnpm.toml"));
    }
    out.push(fnpm_home.join("fnpm.toml"));
    out
}

/// The global and project candidates in load order:
/// `$XDG_CONFIG_HOME/.bunfig.toml | ~/.bunfig.toml`, `./bunfig.toml`, then
/// `$XDG_CONFIG_HOME/.fnpm.toml | ~/.fnpm.toml`, `~/.fnpm/fnpm.toml`, `./fnpm.toml`.
pub fn config_file_candidates(cwd: &Path, home: &Path, xdg_config: Option<&Path>) -> Vec<PathBuf> {
    let base = xdg_config.filter(|d| d.is_absolute()).unwrap_or(home);
    vec![
        base.join(".bunfig.toml"),
        cwd.join("bunfig.toml"),
        base.join(".fnpm.toml"),
        home.join(".fnpm").join("fnpm.toml"),
        cwd.join("fnpm.toml"),
    ]
}

#[cfg(test)]
#[path = "tests/files.rs"]
mod tests;
