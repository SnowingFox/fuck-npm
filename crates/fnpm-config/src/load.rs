//! The merge algorithm of [`Config::load`]: bun's `PackageManager::init` config phase and
//! `Options::load`, with fnpm's files and environment names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fnpm_types::{CpuSet, LibcSet, OsSet, Platform};

use crate::auth::{self, CredentialSite, RegistrySource, Scope, breaks_header_line, build_scope};
use crate::env::string_is_truthy;
use crate::files::{self, ConfigFile, FileKind, InstallFile, RunFile, normalize, resolve_path};
use crate::npmrc::{self, npm_config_bool};
use crate::{
    Auth, Config, DEFAULT_REGISTRY, Env, Error, FeatureFlags, LinkerSetting, LoadOptions,
    LockfileConfig, LogLevel, ManifestCacheMode, NetworkConfig, OfflineMode, Omit, ProxyConfig,
    RegistryScope, Result, RunConfig, TlsConfig,
};

use crate::STORE_LAYOUT;

const DEFAULT_CONCURRENCY: u16 = 64;
const DEFAULT_RETRIES: u16 = 5;
const DEFAULT_BACKOFF_MS: u64 = 50;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;
const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";

/// `2 * available_parallelism`, computed once (on Linux it reads cgroup files).
fn default_concurrent_scripts() -> usize {
    fnpm_types::available_parallelism() * 2
}

/// `cwd` made absolute against the process working directory.
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return normalize(p);
    }
    match std::env::current_dir() {
        Ok(dir) => normalize(&dir.join(p)),
        Err(_) => p.to_path_buf(),
    }
}

/// A path from an environment variable or a flag: `~/` expanded, relative to `base`.
fn env_path(base: &Path, value: &str, home: Option<&Path>) -> PathBuf {
    resolve_path(base, value, home)
}

fn first_env<'a>(env: &'a Env, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| env.get_not_empty(k))
}

/// `FNPM_HOME`, else `$HOME/.fnpm`, else `<cwd>/.fnpm`.
fn fnpm_home(env: &Env, home: Option<&Path>, cwd: &Path) -> PathBuf {
    match env.get_not_empty(crate::env::names::HOME) {
        Some(v) => env_path(cwd, v, home),
        None => home.map_or_else(|| cwd.join(".fnpm"), |h| h.join(".fnpm")),
    }
}

/// Simple glob for workspace patterns: `*` and `?` within a segment, `**` across segments.
fn glob_match(pattern: &str, path: &str) -> bool {
    fn seg_match(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => seg_match(&p[1..], s) || (!s.is_empty() && seg_match(p, &s[1..])),
            (Some(b'?'), Some(_)) => seg_match(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => seg_match(&p[1..], &s[1..]),
            _ => false,
        }
    }
    fn segs_match(p: &[&str], s: &[&str]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(&"**") => segs_match(&p[1..], s) || (!s.is_empty() && segs_match(p, &s[1..])),
            Some(first) => {
                !s.is_empty()
                    && seg_match(first.as_bytes(), s[0].as_bytes())
                    && segs_match(&p[1..], &s[1..])
            }
        }
    }
    let clean = |s: &str| -> Vec<String> {
        s.trim_start_matches("./")
            .split('/')
            .filter(|x| !x.is_empty() && *x != ".")
            .map(str::to_owned)
            .collect()
    };
    let p = clean(pattern);
    let s = clean(path);
    let p: Vec<&str> = p.iter().map(String::as_str).collect();
    let s: Vec<&str> = s.iter().map(String::as_str).collect();
    segs_match(&p, &s)
}

fn workspace_patterns(json: &serde_json::Value) -> Option<Vec<String>> {
    let ws = json.get("workspaces")?;
    let list = match ws {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(o) => o.get("packages")?.as_array()?,
        _ => return None,
    };
    Some(
        list.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
    )
}

/// The nearest directory with a `package.json` at or above `cwd`, or the workspace root above
/// it whose `workspaces` patterns include it. `cwd` when there is none.
pub(crate) fn find_project_dir(cwd: &Path) -> PathBuf {
    let Some(nearest) = cwd.ancestors().find(|d| d.join("package.json").is_file()) else {
        return cwd.to_path_buf();
    };
    for dir in nearest.ancestors().skip(1) {
        let Ok(bytes) = std::fs::read(dir.join("package.json")) else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let Some(patterns) = workspace_patterns(&json) else {
            continue;
        };
        let Ok(rel) = nearest.strip_prefix(dir) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let mut included = false;
        for p in &patterns {
            match p.strip_prefix('!') {
                Some(neg) if glob_match(neg, &rel) => included = false,
                Some(_) => {}
                None if glob_match(p, &rel) => included = true,
                None => {}
            }
        }
        if included {
            return dir.to_path_buf();
        }
    }
    nearest.to_path_buf()
}

/// The user's `.npmrc`: `$NPM_CONFIG_USERCONFIG` (relative to where fnpm started), else
/// `$XDG_CONFIG_HOME/.npmrc` if it exists, else `$HOME/.npmrc`.
fn user_npmrc(env: &Env, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(userconfig) = ["NPM_CONFIG_USERCONFIG", "npm_config_userconfig"]
        .iter()
        .find_map(|k| env.get_not_empty(k))
    {
        return Some(resolve_path(cwd, userconfig, home));
    }
    if let Some(xdg) = env.xdg_config_home() {
        let p = xdg.join(".npmrc");
        if p.exists() {
            return Some(p);
        }
    }
    home.map(|h| h.join(".npmrc"))
}

/// The environment's `ignore-scripts`. It can only turn scripts off.
fn env_ignores_scripts(env: &Env) -> bool {
    [
        "FNPM_CONFIG_IGNORE_SCRIPTS",
        "BUN_CONFIG_IGNORE_SCRIPTS",
        "NPM_CONFIG_IGNORE_SCRIPTS",
        "npm_config_ignore_scripts",
    ]
    .iter()
    .filter_map(|k| env.get(k))
    .any(|v| !v.is_empty() && npm_config_bool(v))
}

/// The `.npmrc` keys fnpm also reads from `FNPM_CONFIG_<KEY>` / `BUN_CONFIG_<KEY>` /
/// `NPM_CONFIG_<KEY>` / `npm_config_<key>`, as a layer over the files.
fn env_layer(env: &Env, cwd: &Path, home: Option<&Path>) -> InstallFile {
    let mut layer = InstallFile::default();
    let list = |v: &str| -> Vec<String> {
        v.split(['\n', ','])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let flag = |k: &str| env.config_var(k).map(npm_config_bool);
    layer.dry_run = flag("dry-run");
    layer.strict_ssl = flag("strict-ssl");
    layer.link_workspace_packages = flag("link-workspace-packages");
    layer.exact = flag("save-exact");
    layer.hoist = flag("hoist");
    if let Some(v) = env.config_var("ca") {
        layer.ca = Some(vec![v.to_owned()]);
    }
    if let Some(v) = env.config_var("cafile") {
        layer.cafile = Some(resolve_path(cwd, v, home));
    }
    for (key, save) in [("omit", false), ("include", true)] {
        for item in env.config_var(key).map(list).unwrap_or_default() {
            match item.as_str() {
                "dev" => layer.dev = Some(save),
                "peer" => layer.peer = Some(save),
                "optional" => layer.optional = Some(save),
                _ => {}
            }
        }
    }
    if let Some(v) = env.config_var("install-strategy") {
        match v {
            "hoisted" => layer.linker = Some(LinkerSetting::Hoisted),
            "linked" => layer.linker = Some(LinkerSetting::Isolated),
            _ => {}
        }
    }
    if let Some(v) = env.config_var("node-linker") {
        match v {
            "pnpm" | "isolated" => layer.linker = Some(LinkerSetting::Isolated),
            "node-modules" | "hoisted" => layer.linker = Some(LinkerSetting::Hoisted),
            _ => {}
        }
    }
    layer.public_hoist_pattern = env.config_var("public-hoist-pattern").map(list);
    layer.hoist_pattern = env.config_var("hoist-pattern").map(list);
    layer.proxy = env.config_var("proxy").map(str::to_owned);
    layer.https_proxy = env.config_var("https-proxy").map(str::to_owned);
    layer.noproxy = env.config_var("noproxy").map(list);
    if let Some(m) = env
        .config_var("minimum-release-age")
        .map(npmrc::js_string_to_number)
        && m.is_finite()
        && m >= 0.0
    {
        layer.minimum_release_age_ms = Some((m * 60_000.0).round() as u64);
    }
    layer
}

/// bun `MinimumReleaseAgeExcludes::parse`: `name` or `name@1.2.3 || 1.2.4`. A malformed entry
/// (`foo@`, `foo@^1`) is dropped with a warning, never widened to the name.
fn validate_excludes(entries: Vec<String>, warnings: &mut Vec<String>) -> Vec<String> {
    fn is_exact_version(v: &str) -> bool {
        let v = v.trim();
        let (core, rest) = match v.find(['-', '+']) {
            Some(i) => (&v[..i], &v[i..]),
            None => (v, ""),
        };
        let parts: Vec<&str> = core.split('.').collect();
        let core_ok = parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        let rest_ok = rest.is_empty()
            || (rest.len() > 1
                && rest[1..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')));
        core_ok && rest_ok
    }
    let mut out = Vec::new();
    for entry in entries {
        let name_start = usize::from(entry.starts_with('@'));
        let Some(at) = entry[name_start..].find('@').map(|i| i + name_start) else {
            out.push(entry);
            continue;
        };
        let name = &entry[..at];
        if entry[at + 1..].split("||").all(is_exact_version) {
            out.push(entry);
        } else {
            warnings.push(format!(
                "ignoring minimumReleaseAgeExcludes entry {entry:?}: expected a package name or exact versions, e.g. \"{name}@1.2.3\" or \"{name}@1.2.3 || 1.2.4\""
            ));
        }
    }
    out
}

/// The registries being resolved (bun `Options::{scope, registries, url_auth}`).
struct Registries {
    scope: Scope,
    /// Scope name without `@`.
    registries: BTreeMap<String, Scope>,
    url_auth: Vec<(String, Auth)>,
}

fn url_hash_eq(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn copy_credentials(to: &mut Scope, from: &Scope) {
    to.auth = from.auth.clone();
    to.declared_in_npmrc = from.declared_in_npmrc;
}

impl Registries {
    /// A registry at or under `old` keeps its credentials unless `.npmrc` has a deeper line
    /// for it, and never across an https -> http downgrade.
    fn inherits_credentials(&self, old: &Scope, new_url: &str) -> bool {
        let old_segments = auth::path_segments(&old.url);
        let new_segments = auth::path_segments(new_url);
        auth::lenient_host(new_url) == auth::lenient_host(&old.url)
            && (auth::is_https(new_url) || !auth::is_https(&old.url))
            && new_segments.starts_with(&old_segments)
            && match auth::find_entry(&self.url_auth, new_url) {
                Some((own, _)) => {
                    auth::find_entry(&self.url_auth, &old.url).is_some_and(|(k, _)| k == own)
                }
                None => true,
            }
    }

    /// The scope for `url` once it replaces `current`. `.npmrc` lines are applied after.
    fn scope_for_registry_url(
        &self,
        site: CredentialSite<'_>,
        current: &Scope,
        url: &str,
        env: &Env,
        warnings: &mut Vec<String>,
    ) -> Result<Scope> {
        let registry = RegistrySource::from_url(url);
        let has_userinfo = registry.has_credentials();
        let mut scope = build_scope(site, registry, env, true, warnings)?;
        if has_userinfo && scope.auth.is_some() {
            return Ok(scope);
        }
        if !scope.auth.is_some() {
            let configured = std::iter::once(&self.scope)
                .chain(self.registries.values())
                .find(|c| url_hash_eq(&c.url, &scope.url) && c.auth.is_some());
            if let Some(configured) = configured {
                copy_credentials(&mut scope, configured);
                return Ok(scope);
            }
        }
        if self.inherits_credentials(current, &scope.url) {
            if scope.auth.is_some() {
                // A credential embedded in the path of `url` is trusted as little as one in the
                // line it replaces.
                scope.declared_in_npmrc = current.declared_in_npmrc;
            } else {
                copy_credentials(&mut scope, current);
            }
        }
        Ok(scope)
    }

    /// `.npmrc`'s credentials for the URL of `scope` yield to the config files, the
    /// environment and flags.
    fn fill_from_url_auth(url_auth: &[(String, Auth)], scope: &mut Scope) {
        let own = auth::find_entry(url_auth, &scope.url);
        scope.url_auth_key = own.map(|(k, _)| k.clone());
        if let Some((_, credentials)) = own
            && (!scope.auth.is_some() || scope.declared_in_npmrc)
        {
            scope.auth = credentials.clone();
            scope.declared_in_npmrc = false;
        }
    }
}

fn to_registry_scope(scope: &Scope, emails: &[(String, String)]) -> RegistryScope {
    let mut out = RegistryScope::new(&scope.url);
    out.auth = scope.auth.clone();
    out.email = auth::find_entry(emails, &scope.url).map(|(_, e)| e.clone());
    out
}

fn user_agent(env: &Env, node_version: Option<&str>) -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let mut ua = format!("fnpm/{} npm/?", env!("CARGO_PKG_VERSION"));
    if let Some(v) = node_version.filter(|v| !v.is_empty()) {
        ua.push_str(" node/");
        if !v.starts_with('v') {
            ua.push('v');
        }
        ua.push_str(v);
    }
    ua.push(' ');
    ua.push_str(os);
    ua.push(' ');
    ua.push_str(arch);
    if let Some(ci) = env.ci_name() {
        ua.push_str(" ci/");
        ua.push_str(ci);
    }
    ua
}

fn parse_platform(cli: &crate::CliOverrides) -> Result<Platform> {
    let invalid = |key: &str, e: fnpm_types::PlatformError| Error::InvalidValue {
        key: key.to_owned(),
        message: e.to_string(),
    };
    let current = Platform::current();
    Ok(Platform {
        os: if cli.os.is_empty() {
            current.os
        } else {
            OsSet::parse_cli(&cli.os).map_err(|e| invalid("--os", e))?
        },
        cpu: if cli.cpu.is_empty() {
            current.cpu
        } else {
            CpuSet::parse_cli(&cli.cpu).map_err(|e| invalid("--cpu", e))?
        },
        libc: if cli.libc.is_empty() {
            current.libc
        } else {
            LibcSet::parse_cli(&cli.libc).map_err(|e| invalid("--libc", e))?
        },
    })
}

/// `FNPM_CONFIG_<NAME>` then `BUN_CONFIG_<NAME>`, with the name of the one that was set.
fn tuning_var<'a>(env: &'a Env, name: &str) -> Option<(String, &'a str)> {
    [format!("FNPM_CONFIG_{name}"), format!("BUN_CONFIG_{name}")]
        .into_iter()
        .find_map(|k| env.get(&k).map(|v| (k.clone(), v)))
}

fn feature_flags(env: &Env) -> FeatureFlags {
    let on = |names: &[&str]| names.iter().any(|k| env.truthy(k) == Some(true));
    FeatureFlags {
        disable_install_index: on(&[
            "FNPM_FEATURE_FLAG_DISABLE_INSTALL_INDEX",
            "BUN_FEATURE_FLAG_DISABLE_INSTALL_INDEX",
        ]),
        disable_streaming_install: on(&[
            "FNPM_FEATURE_FLAG_DISABLE_STREAMING_INSTALL",
            "BUN_FEATURE_FLAG_DISABLE_STREAMING_INSTALL",
        ]),
        disable_copy_file_range: on(&[
            "FNPM_CONFIG_DISABLE_COPY_FILE_RANGE",
            "BUN_CONFIG_DISABLE_COPY_FILE_RANGE",
        ]),
        disable_ficlonerange: on(&[
            "FNPM_CONFIG_DISABLE_IOCTL_FICLONERANGE",
            "BUN_CONFIG_DISABLE_ioctl_ficlonerange",
        ]),
        disable_native_dependency_linker: on(&[
            "FNPM_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER",
            "BUN_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER",
        ]),
        disable_ignore_scripts_optimizer: on(&[
            "FNPM_FEATURE_FLAG_DISABLE_IGNORE_SCRIPTS",
            "BUN_FEATURE_FLAG_DISABLE_IGNORE_SCRIPTS",
        ]),
        force_windows_junctions: on(&[
            "FNPM_FEATURE_FLAG_FORCE_WINDOWS_JUNCTIONS",
            "BUN_FEATURE_FLAG_FORCE_WINDOWS_JUNCTIONS",
        ]),
    }
}

/// Everything read from files, before the options are derived.
struct Layers {
    env: Env,
    cwd: PathBuf,
    home: Option<PathBuf>,
    fnpm_home: PathBuf,
    project_dir: PathBuf,
    global_dir: PathBuf,
    global_bin_dir: PathBuf,
    merged: InstallFile,
    run: RunFile,
    warnings: Vec<String>,
}

fn global_dirs(
    env: &Env,
    cwd: &Path,
    home: Option<&Path>,
    fnpm_home: &Path,
    layer: &InstallFile,
) -> (PathBuf, PathBuf) {
    let fnpm_home_set = env.get_not_empty(crate::env::names::HOME).is_some();
    let bun_install = env
        .get_not_empty("BUN_INSTALL")
        .map(|v| env_path(cwd, v, home));
    let pick = |env_keys: &[&str],
                configured: &Option<PathBuf>,
                under_bun: &[&str],
                under_fnpm: &str|
     -> PathBuf {
        if let Some(v) = first_env(env, env_keys) {
            return env_path(cwd, v, home);
        }
        if let Some(p) = configured.as_ref().filter(|p| !p.as_os_str().is_empty()) {
            return p.clone();
        }
        if !fnpm_home_set && let Some(root) = &bun_install {
            return under_bun.iter().fold(root.clone(), |p, s| p.join(s));
        }
        fnpm_home.join(under_fnpm)
    };
    (
        pick(
            &["FNPM_GLOBAL_DIR", "BUN_INSTALL_GLOBAL_DIR"],
            &layer.global_dir,
            &["install", "global"],
            "global",
        ),
        pick(
            &["FNPM_GLOBAL_BIN_DIR", "BUN_INSTALL_BIN"],
            &layer.global_bin_dir,
            &["bin"],
            "bin",
        ),
    )
}

/// The project directory (`None` with `-g`) and the environment with its `.env` files.
pub(crate) fn effective_env(opts: &LoadOptions) -> Result<(Env, Option<PathBuf>)> {
    let cwd = absolute(&opts.cwd);
    let project_dir = if opts.cli.global {
        None
    } else {
        Some(
            opts.project_dir
                .as_deref()
                .map_or_else(|| find_project_dir(&cwd), absolute),
        )
    };
    let env = crate::dotenv::with_env_files(
        &opts.env,
        project_dir.as_deref(),
        &cwd,
        &opts.cli.env_file,
        opts.cli.no_env_file,
    )?;
    Ok((env, project_dir))
}

fn read_layers(opts: &LoadOptions) -> Result<Layers> {
    let (env_owned, project_dir) = effective_env(opts)?;
    let env = &env_owned;
    let cli = &opts.cli;
    let cwd = absolute(&opts.cwd);
    let home = env.home();
    let xdg = env.xdg_config_home();
    let fnpm_home = fnpm_home(env, home.as_deref(), &cwd);
    let mut warnings = Vec::new();

    let mut files_layer = InstallFile::default();
    let mut run = RunFile::default();
    let mut apply = |file: Option<ConfigFile>, warnings: &mut Vec<String>| {
        if let Some(file) = file {
            warnings.extend(file.warnings);
            if let Some(install) = file.install {
                files_layer.overlay(install);
            }
            run.overlay(file.run);
        }
    };
    let h = home.as_deref();
    let named = cli
        .config
        .as_ref()
        .map(|p| resolve_path(&cwd, &p.to_string_lossy(), h));
    let user_dir = xdg.as_deref().or(h);

    // 1. The user's bunfig.
    if let Some(dir) = user_dir {
        apply(
            files::load_config_file(&dir.join(".bunfig.toml"), FileKind::User, h)?,
            &mut warnings,
        );
    }
    // 2. The project's bunfig.
    if let (Some(dir), None) = (&project_dir, &named) {
        apply(
            files::load_config_file(&dir.join("bunfig.toml"), FileKind::Project, h)?,
            &mut warnings,
        );
    }
    // 3. The user's fnpm.toml files.
    if let Some(dir) = user_dir {
        apply(
            files::load_config_file(&dir.join(".fnpm.toml"), FileKind::User, h)?,
            &mut warnings,
        );
    }
    apply(
        files::load_config_file(&fnpm_home.join("fnpm.toml"), FileKind::User, h)?,
        &mut warnings,
    );
    // 4. `--config`, or the project's fnpm.toml.
    match (&named, &project_dir) {
        (Some(path), _) => apply(
            files::load_config_file(path, FileKind::Named, h)?,
            &mut warnings,
        ),
        (None, Some(dir)) => apply(
            files::load_config_file(&dir.join("fnpm.toml"), FileKind::Project, h)?,
            &mut warnings,
        ),
        (None, None) => {}
    }
    // 5. package.json#fnpm.
    if let Some(dir) = &project_dir {
        apply(files::load_package_json_config(dir, h)?, &mut warnings);
    }

    let (global_dir, global_bin_dir) = global_dirs(env, &cwd, h, &fnpm_home, &files_layer);
    // With `-g` the project is the global directory.
    let project_dir = project_dir.unwrap_or_else(|| global_dir.clone());

    let mut npmrc_paths: Vec<PathBuf> = user_npmrc(env, &cwd, h).into_iter().collect();
    npmrc_paths.push(project_dir.join(".npmrc"));
    let mut merged = npmrc::load_npmrc_files(&npmrc_paths, env, &files_layer, h, &mut warnings);
    merged.overlay(files_layer);
    merged.overlay(env_layer(env, &cwd, h));
    if env_ignores_scripts(env) {
        merged.ignore_scripts = Some(true);
    }

    Ok(Layers {
        env: env_owned,
        cwd,
        home,
        fnpm_home,
        project_dir,
        global_dir,
        global_bin_dir,
        merged,
        run,
        warnings,
    })
}

pub(crate) fn load(opts: &LoadOptions) -> Result<Config> {
    let layers = read_layers(opts)?;
    resolve(opts, layers)
}

pub(crate) fn defaults(cwd: &Path) -> Config {
    let cwd = absolute(cwd);
    let opts = LoadOptions {
        cwd: cwd.clone(),
        project_dir: Some(cwd.clone()),
        ..Default::default()
    };
    let fnpm_home = cwd.join(".fnpm");
    let layers = Layers {
        env: Env::default(),
        cwd: cwd.clone(),
        home: None,
        global_dir: fnpm_home.join("global"),
        global_bin_dir: fnpm_home.join("bin"),
        fnpm_home,
        project_dir: cwd,
        merged: InstallFile::default(),
        run: RunFile::default(),
        warnings: Vec::new(),
    };
    resolve(&opts, layers).expect("defaults never fail: no credentials, no flags")
}

/// bun `env_loader`: `<scheme>_proxy` before `<SCHEME>_PROXY` (an empty lowercase value falls
/// through to the uppercase one); `""` and `''` count as unset.
fn proxy_var<'a>(env: &'a Env, lower: &str, upper: &str) -> Option<&'a str> {
    let value = match env.get(lower) {
        Some(v) if !v.is_empty() => Some(v),
        _ => env.get(upper),
    };
    value.filter(|v| !is_emptyish(v))
}

fn is_emptyish(v: &str) -> bool {
    matches!(v, "" | "\"\"" | "''")
}

/// `all_proxy` / `ALL_PROXY`, only when it names an HTTP proxy (no scheme, `http:`, `https:`):
/// it commonly points at a SOCKS proxy, which the HTTP client cannot speak.
fn all_proxy(env: &Env) -> Option<&str> {
    let value = proxy_var(env, "all_proxy", "ALL_PROXY")?;
    match value.split_once("://") {
        None => Some(value),
        Some((scheme, _))
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            Some(value)
        }
        Some(_) => None,
    }
}

/// The http and https proxies and the `NO_PROXY` list. `.npmrc` `proxy` / `https-proxy` win
/// over the environment (an empty value turns one off) and, like npm, `proxy` also serves https
/// when neither `https-proxy` nor an https variable is set. The variables follow bun: the http
/// ones never serve https requests, and `ALL_PROXY` fills either.
fn proxies(env: &Env, m: &InstallFile) -> (Option<String>, Option<String>, Vec<String>) {
    let configured = |v: &Option<String>| v.as_ref().map(|s| (!s.is_empty()).then(|| s.clone()));
    let http = configured(&m.proxy).unwrap_or_else(|| {
        proxy_var(env, "http_proxy", "HTTP_PROXY")
            .or_else(|| all_proxy(env))
            .map(str::to_owned)
    });
    let https = configured(&m.https_proxy).unwrap_or_else(|| {
        proxy_var(env, "https_proxy", "HTTPS_PROXY")
            .map(str::to_owned)
            .or_else(|| configured(&m.proxy).flatten())
            .or_else(|| all_proxy(env).map(str::to_owned))
    });
    let no_proxy = match &m.noproxy {
        Some(list) => list.clone(),
        None => ["no_proxy", "NO_PROXY"]
            .iter()
            .filter_map(|k| env.get(k))
            .find(|v| !is_emptyish(v))
            .map(|v| {
                v.split([',', ' '])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    };
    (http, https, no_proxy)
}

fn resolve(opts: &LoadOptions, layers: Layers) -> Result<Config> {
    let cli = &opts.cli;
    let Layers {
        env,
        cwd,
        home,
        fnpm_home,
        project_dir,
        global_dir,
        global_bin_dir,
        merged: m,
        run,
        mut warnings,
    } = layers;
    let env = &env;
    let h = home.as_deref();

    // ── manifest cache ──
    let mut manifest_cache = true;
    let mut manifest_cache_control = true;
    if let Some(v) = ["FNPM_MANIFEST_CACHE", "BUN_MANIFEST_CACHE"]
        .iter()
        .find_map(|k| env.get(k))
    {
        manifest_cache = matches!(v, "1" | "2");
        manifest_cache_control = v == "2";
    }

    // ── registries ──
    let mut base = m.registry.clone().unwrap_or_default();
    if base.url.is_empty() {
        base.url = DEFAULT_REGISTRY.to_owned();
    }
    let scope = build_scope(CredentialSite::Registry(""), base, env, true, &mut warnings)?;
    let mut regs = Registries {
        scope,
        registries: BTreeMap::new(),
        url_auth: Vec::new(),
    };
    // Scopes with credentials but no URL use the default registry, once its URL is final.
    let mut at_default: Vec<(String, RegistrySource)> = Vec::new();
    for (name, registry) in m.scopes.iter().flatten() {
        at_default.retain(|(n, _)| n != name);
        if registry.url.is_empty() {
            regs.registries.remove(name);
            if registry.has_credentials() {
                at_default.push((name.clone(), registry.clone()));
            }
            continue;
        }
        let built = build_scope(
            CredentialSite::Registry(name),
            registry.clone(),
            env,
            true,
            &mut warnings,
        )?;
        regs.registries.insert(name.clone(), built);
    }
    for (key, credentials) in &m.url_auth {
        let built = build_scope(
            CredentialSite::NpmrcLine(key),
            credentials.clone(),
            env,
            false,
            &mut warnings,
        )?;
        if built.auth.is_some() {
            regs.url_auth.push((key.clone(), built.auth));
        }
    }

    // ── per-file settings ──
    let mut offline = if m.offline == Some(true) {
        OfflineMode::Offline
    } else {
        m.prefer.unwrap_or_default()
    };
    let mut dry_run = m.dry_run == Some(true);
    let mut cache_disabled = false;
    if m.disable_cache == Some(true) {
        cache_disabled = true;
        manifest_cache = false;
    }
    if m.disable_manifest_cache == Some(true) {
        manifest_cache = false;
    }
    let mut force = false;
    if m.force == Some(true) {
        force = true;
        manifest_cache_control = false;
    }
    let mut print_yarn = m.save_yarn_lockfile == Some(true);
    let mut save_lockfile = true;
    let mut force_save = false;
    if let Some(save) = m.save_lockfile {
        save_lockfile = save;
        force_save = true;
    }
    let mut local_dev = m.dev.unwrap_or(true);
    let mut optional = m.optional.unwrap_or(true);
    let mut peer = m.peer.unwrap_or(true);
    let mut exact = m.exact.unwrap_or(false);
    let mut fail_early = false;
    let mut frozen = false;
    let mut production = false;
    if m.production == Some(true) {
        production = true;
        local_dev = false;
        fail_early = true;
        frozen = true;
        force_save = false;
    }
    if m.frozen_lockfile == Some(true) {
        frozen = true;
    }
    let mut save_text_lockfile = m.save_text_lockfile.unwrap_or(true);
    let mut concurrent_scripts = m
        .concurrent_scripts
        .map_or_else(default_concurrent_scripts, |n| n as usize);
    let mut ignore_scripts = m.ignore_scripts == Some(true);
    let mut minimum_release_age_ms = m.minimum_release_age_ms;
    let mut minimum_release_age_excludes = m
        .minimum_release_age_excludes
        .clone()
        .map(|l| validate_excludes(l, &mut warnings))
        .unwrap_or_default();
    let mut linker = m.linker.unwrap_or_default();
    let mut global_store = m.global_store.unwrap_or(true);
    let mut backend = m.backend;
    let mut log_level = m.log_level.unwrap_or_default();

    if let Some(v) = ["FNPM_GLOBAL_STORE", "BUN_INSTALL_GLOBAL_STORE"]
        .iter()
        .find_map(|k| env.get(k))
    {
        global_store = string_is_truthy(v);
    }
    let progress = match ["FNPM_PROGRESS", "BUN_INSTALL_PROGRESS"]
        .iter()
        .find_map(|k| env.get(k))
    {
        Some(v) => v != "0",
        None => !env.is_ci(),
    };
    if ["FNPM_VERBOSE", "BUN_INSTALL_VERBOSE"]
        .iter()
        .any(|k| env.get(k).is_some())
    {
        log_level = LogLevel::Verbose;
    }

    // ── registry from the environment and the command line ──
    let configured = regs.scope.clone();
    for key in [
        "FNPM_REGISTRY",
        "FNPM_CONFIG_REGISTRY",
        "BUN_CONFIG_REGISTRY",
        "NPM_CONFIG_REGISTRY",
        "npm_config_registry",
    ] {
        let Some(value) = env.get(key) else { continue };
        // An unusable value fails when it is used; it never falls through to the next layer.
        let value = value.trim_matches([' ', '\t', '\r', '\n']);
        if !value.is_empty() {
            let current = regs.scope.clone();
            regs.scope = regs.scope_for_registry_url(
                CredentialSite::Registry(""),
                &current,
                value,
                env,
                &mut warnings,
            )?;
            break;
        }
    }
    if let Some(url) = cli.registry.as_deref().filter(|u| !u.is_empty()) {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(Error::InvalidValue {
                key: "--registry".to_owned(),
                message: format!(
                    "Registry URL must start with 'https://' or 'http://': {:?}",
                    crate::redact_url(url)
                ),
            });
        }
        let current = regs.scope.clone();
        regs.scope = regs.scope_for_registry_url(
            CredentialSite::Registry(""),
            &current,
            url,
            env,
            &mut warnings,
        )?;
    }

    // Scopes without a URL follow the default registry's credentials only when they move.
    let follows = url_hash_eq(&regs.scope.url, &configured.url)
        || regs.inherits_credentials(&configured, &regs.scope.url);
    let home_url = if follows {
        regs.scope.url.clone()
    } else {
        configured.url.clone()
    };
    for (name, registry) in at_default {
        let registry = RegistrySource {
            url: home_url.clone(),
            ..registry
        };
        let built = build_scope(
            CredentialSite::Registry(&name),
            registry,
            env,
            true,
            &mut warnings,
        )?;
        regs.registries.insert(name, built);
    }

    for key in [
        "FNPM_CONFIG_TOKEN",
        "BUN_CONFIG_TOKEN",
        "NPM_CONFIG_TOKEN",
        "npm_config_token",
    ] {
        let Some(token) = env.get_not_empty(key) else {
            continue;
        };
        if breaks_header_line(token) {
            return Err(Error::InvalidValue {
                key: key.to_owned(),
                message: format!("${key} contains a newline or NUL byte"),
            });
        }
        regs.scope.auth = Auth::Token {
            token: token.to_owned(),
        };
        regs.scope.declared_in_npmrc = false;
        break;
    }

    // ── tuning environment ──
    if tuning_var(env, "YARN_LOCKFILE").is_some() {
        print_yarn = true;
    }
    let mut retries = DEFAULT_RETRIES;
    if let Some((_, v)) = tuning_var(env, "HTTP_RETRY_COUNT")
        && let Ok(n) = v.trim().parse::<u16>()
    {
        retries = n;
    }
    let mut retry_backoff_ms = DEFAULT_BACKOFF_MS;
    if let Some((_, v)) = tuning_var(env, "HTTP_RETRY_BACKOFF")
        && let Ok(n) = v.trim().parse::<u64>()
    {
        retry_backoff_ms = n;
    }
    let mut idle_timeout_secs = DEFAULT_IDLE_TIMEOUT_SECS;
    if let Some((_, v)) = tuning_var(env, "HTTP_IDLE_TIMEOUT")
        && let Ok(n) = v.trim().parse::<u64>()
    {
        idle_timeout_secs = n;
    }
    let mut concurrency = m.network_concurrency.unwrap_or(DEFAULT_CONCURRENCY);
    if let Some((key, v)) = tuning_var(env, "MAX_HTTP_REQUESTS") {
        match v.trim().parse::<u16>() {
            Ok(0) => warnings.push(format!("{key} value must be a number between 1 and 65535")),
            Ok(n) => concurrency = n,
            Err(_) => {
                return Err(Error::InvalidValue {
                    key,
                    message: format!("value {v:?} is not a valid integer between 1 and 65535"),
                });
            }
        }
    }
    let mut load_lockfile = true;
    let mut install_packages = true;
    if let Some((_, v)) = tuning_var(env, "SKIP_SAVE_LOCKFILE") {
        save_lockfile = v == "0";
    }
    if let Some((_, v)) = tuning_var(env, "SKIP_LOAD_LOCKFILE") {
        load_lockfile = v == "0";
    }
    if let Some((_, v)) = tuning_var(env, "SKIP_INSTALL_PACKAGES") {
        install_packages = v == "0";
    }
    let mut verify_integrity = !matches!(tuning_var(env, "NO_VERIFY"), Some((_, "1" | "true")));

    if cli.update {
        manifest_cache = false;
        manifest_cache_control = false;
    }

    // ── command line ──
    let mut cache_dir_flag = None;
    if let Some(dir) = &cli.cache_dir {
        cache_dir_flag = Some(resolve_path(&cwd, &dir.to_string_lossy(), h));
    }
    if let Some(v) = cli.exact {
        exact = v;
    }
    let mut save_package_json = true;
    if cli.save == Some(false) {
        save_lockfile = false;
        save_package_json = false;
    }
    if let Some(v) = cli.dry_run {
        dry_run = v;
    }
    if cli.no_cache {
        manifest_cache = false;
        manifest_cache_control = false;
    }
    for item in &cli.omit {
        match item.as_str() {
            "dev" => local_dev = false,
            "optional" => optional = false,
            "peer" => peer = false,
            other => {
                return Err(Error::InvalidValue {
                    key: "--omit".to_owned(),
                    message: format!("invalid `omit` value: '{other}'"),
                });
            }
        }
    }
    if let Some(v) = cli.ignore_scripts {
        ignore_scripts = v;
    }
    // `NPM_CONFIG_IGNORE_SCRIPTS` and friends can only turn scripts off, even under a flag.
    if env_ignores_scripts(env) {
        ignore_scripts = true;
    }
    if let Some(v) = cli.save_text_lockfile {
        save_text_lockfile = v;
    }
    if let Some(n) = cli.concurrent_scripts.filter(|&n| n > 0) {
        concurrent_scripts = n;
    }
    if let Some(ms) = cli.minimum_release_age_ms {
        minimum_release_age_ms = Some(ms);
    }
    if !cli.minimum_release_age_excludes.is_empty() {
        let entries = cli
            .minimum_release_age_excludes
            .iter()
            .flat_map(|l| l.split(','))
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect();
        minimum_release_age_excludes = validate_excludes(entries, &mut warnings);
    }
    if let Some(l) = cli.linker {
        linker = l;
    }
    if let Some(l) = cli.log_level {
        log_level = l;
    }
    if cli.verify == Some(false) {
        verify_integrity = false;
    }
    if cli.offline == Some(true) {
        offline = OfflineMode::Offline;
    } else if cli.prefer_offline == Some(true) && offline == OfflineMode::Online {
        offline = OfflineMode::PreferOffline;
    }
    if cli.yarn == Some(true) {
        print_yarn = true;
    }
    if cli.backend.is_some() {
        backend = cli.backend;
    }
    let platform = parse_platform(cli)?;
    if let Some(v) = cli.frozen_lockfile {
        frozen = v;
    }
    // After `--frozen-lockfile`: like bun, `--production` always freezes the lockfile.
    if cli.production == Some(true) {
        production = true;
        local_dev = false;
        fail_early = true;
        frozen = true;
    } else if cli.production == Some(false) {
        production = false;
    }
    if cli.force == Some(true) {
        force = true;
        manifest_cache_control = false;
        force_save = true;
    }
    if let Some(n) = cli.network_concurrency {
        concurrency = n.max(1);
    }

    // `--ca` / `--cafile` replace the config's `ca` + `cafile` as a whole.
    let (mut ca, mut ca_file) = (m.ca.clone().unwrap_or_default(), m.cafile.clone());
    if !cli.ca.is_empty() || cli.cafile.is_some() {
        ca = cli.ca.clone();
        ca_file = cli
            .cafile
            .as_ref()
            .map(|p| resolve_path(&cwd, &p.to_string_lossy(), h));
    }

    // ── derived rules ──
    if frozen {
        save_lockfile = false;
        force_save = false;
    }
    if dry_run {
        install_packages = false;
        save_package_json = false;
        save_lockfile = false;
        print_yarn = false;
    }
    for scope in std::iter::once(&mut regs.scope).chain(regs.registries.values_mut()) {
        Registries::fill_from_url_auth(&regs.url_auth, scope);
    }
    // The manifest cache is the data source for --prefer-offline / --offline.
    if offline != OfflineMode::Online {
        manifest_cache = true;
    }
    let minimum_release_age_ms = minimum_release_age_ms.filter(|&ms| ms > 0);

    // ── directories ──
    let store_dir = match env.get_not_empty(crate::env::names::STORE_DIR) {
        Some(v) => env_path(&cwd, v, h).join(STORE_LAYOUT),
        None => fnpm_home.join("store").join(STORE_LAYOUT),
    };
    let cache_dir = if let Some(dir) = cache_dir_flag {
        dir
    } else if let Some(v) = env.get_not_empty(crate::env::names::CACHE_DIR) {
        env_path(&cwd, v, h)
    } else if let Some(v) = env.get_not_empty("BUN_INSTALL_CACHE_DIR") {
        // CI sets it once for the whole repository.
        env_path(&project_dir, v, h)
    } else if let Some(v) = env.config_var("cache") {
        env_path(&project_dir, v, h)
    } else if let Some(dir) = m.cache_dir.clone().filter(|d| !d.as_os_str().is_empty()) {
        dir
    } else if h.is_some() || env.get_not_empty(crate::env::names::HOME).is_some() {
        fnpm_home.join("cache")
    } else {
        project_dir.join("node_modules").join(".fnpm-cache")
    };

    // ── network ──
    let (http_proxy, https_proxy, no_proxy) = proxies(env, &m);
    let reject_unauthorized =
        m.strict_ssl != Some(false) && env.get("NODE_TLS_REJECT_UNAUTHORIZED") != Some("0");
    let ca_store = cli
        .ca_store
        .clone()
        .or_else(|| (env.get("NODE_USE_SYSTEM_CA") == Some("1")).then(|| "system".to_owned()));
    let path_env = |k: &str| env.get_not_empty(k).map(PathBuf::from);
    let manifest_cache = match (manifest_cache, manifest_cache_control) {
        (false, _) => ManifestCacheMode::Disabled,
        (true, false) => ManifestCacheMode::NoCacheControl,
        (true, true) => ManifestCacheMode::CacheControl,
    };
    let network = NetworkConfig {
        concurrency,
        retries,
        retry_backoff_ms,
        idle_timeout_secs,
        offline,
        manifest_cache,
        proxy: ProxyConfig {
            http: http_proxy,
            https: https_proxy,
            no_proxy,
        },
        tls: TlsConfig {
            ca,
            ca_file,
            reject_unauthorized,
            ca_store,
            extra_ca_certs: path_env("NODE_EXTRA_CA_CERTS"),
            ssl_cert_file: path_env("SSL_CERT_FILE"),
            ssl_cert_dir: path_env("SSL_CERT_DIR"),
        },
        user_agent: user_agent(env, opts.node_version.as_deref()),
    };

    let provenance = ["NPM_CONFIG_PROVENANCE", "npm_config_provenance"]
        .iter()
        .filter_map(|k| env.get_not_empty(k))
        .map(npm_config_bool)
        .max()
        .or(m.provenance);

    let registry = to_registry_scope(&regs.scope, &m.emails);
    let scopes = regs
        .registries
        .iter()
        .map(|(name, s)| (format!("@{name}"), to_registry_scope(s, &m.emails)))
        .collect();

    let cwd_for_tmp = cwd.clone();
    Ok(Config {
        cwd,
        project_dir,
        home_dir: fnpm_home,
        store_dir,
        cache_dir,
        global_dir,
        global_bin_dir,
        registry,
        scopes,
        url_auth: regs.url_auth,
        network,
        linker,
        backend,
        global_store,
        hoist: m.hoist.unwrap_or(true),
        hoist_pattern: m
            .hoist_pattern
            .clone()
            .unwrap_or_else(|| vec!["*".to_owned()]),
        public_hoist_pattern: m.public_hoist_pattern.clone().unwrap_or_default(),
        link_workspace_packages: m.link_workspace_packages.unwrap_or(true),
        frozen_lockfile: frozen,
        production,
        dry_run,
        force,
        exact,
        ignore_scripts,
        verify_integrity,
        save_text_lockfile,
        lockfile: LockfileConfig {
            save: save_lockfile,
            print_yarn,
            path: m.lockfile_path.clone(),
            save_path: m.save_lockfile_path.clone(),
            force_save,
        },
        omit: Omit {
            dev: !local_dev,
            optional: !optional,
            peer: !peer,
        },
        auto_install_peers: peer,
        concurrent_scripts,
        minimum_release_age_ms,
        minimum_release_age_excludes,
        platform,
        security_scanner: m.security_scanner.clone(),
        log_level,
        ci: env.is_ci(),
        global: cli.global,
        progress,
        load_lockfile,
        install_packages,
        save_package_json,
        fail_early,
        cache_disabled,
        provenance,
        run: RunConfig {
            use_system_shell: run.use_system_shell.unwrap_or(true),
            silent: run.silent.unwrap_or(false),
            elide_lines: run.elide_lines,
            bun: run.bun.unwrap_or(false),
            no_orphans: run.no_orphans.unwrap_or(false),
        },
        feature_flags: feature_flags(env),
        tmp_dir: first_env(env, &["FNPM_TMPDIR", "BUN_TMPDIR"])
            .map(|v| env_path(&cwd_for_tmp, v, h)),
        github_api_url: env
            .get_not_empty("GITHUB_API_URL")
            .unwrap_or(DEFAULT_GITHUB_API_URL)
            .to_owned(),
        warnings,
    })
}
