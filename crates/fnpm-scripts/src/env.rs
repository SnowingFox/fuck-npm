//! Script environment (bun `PackageManagerLifecycle.rs`, `install/lib.rs`
//! `configure_env_for_run` / `seed_script_env`, `run_command.rs` `path_for_run`).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// The fnpm version reported in `npm_config_user_agent`.
pub const FNPM_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Reported when the caller does not say which Node.js runs fnpm (`FNPM_NODE_VERSION`).
/// bun also reports a fixed version rather than running `node --version`.
pub const DEFAULT_NODE_VERSION: &str = "v24.11.0";

/// Everything one lifecycle script needs to know about its package.
pub struct ScriptContext<'a> {
    pub project_dir: &'a Path,
    pub package_dir: &'a Path,
    pub package_json: &'a Path,
    pub package_name: &'a str,
    pub package_version: &'a str,
    /// `config` object string values (`npm_package_config_<key>`).
    pub config: &'a [(String, String)],
    pub event: &'a str,
    pub script: &'a str,
}

/// The full environment for a child: inherited env minus `npm_package_*` / `npm_lifecycle_*`,
/// plus `npm_lifecycle_event`, `npm_lifecycle_script`, `npm_package_json`,
/// `npm_package_name`, `npm_package_version`, `npm_package_config_*`,
/// `npm_config_local_prefix`, `npm_config_user_agent` (`fnpm/<ver> npm/? node/<ver> <os>
/// <arch>`), `npm_execpath`, `npm_node_execpath`, `INIT_CWD`, and PATH with every ancestor
/// `node_modules/.bin` from `package_dir` up (inherited PATH last for dependencies).
///
/// A package is a dependency (its own `.bin` chain is searched before the inherited PATH,
/// which comes before the project's bins) when `package_dir` has a `node_modules` component.
pub fn build(
    ctx: &ScriptContext<'_>,
    inherited: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let base = BaseEnv::lifecycle(ctx.project_dir, inherited.iter().cloned(), &[], None);
    let package = PackageVars {
        dir: ctx.package_dir,
        package_json: ctx.package_json,
        name: ctx.package_name,
        version: ctx.package_version,
        config: ctx.config,
    };
    let mut env = base.for_package(&package, is_dependency_dir(ctx.package_dir));
    env.set_lifecycle(ctx.event, ctx.script);
    env.into_pairs()
}

/// A package directory inside some `node_modules` (or the global store) belongs to a
/// dependency; the project and its workspaces live outside.
pub fn is_dependency_dir(dir: &Path) -> bool {
    dir.components()
        .any(|c| c == Component::Normal(OsStr::new("node_modules")))
        || in_global_store(dir)
}

/// An environment under construction. Sorted keys keep the child environment deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvMap {
    vars: BTreeMap<OsString, OsString>,
}

impl EnvMap {
    /// From `(key, value)` pairs; later pairs win.
    pub fn from_pairs<K: Into<OsString>, V: Into<OsString>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// The current process environment.
    pub fn from_process() -> Self {
        Self::from_pairs(std::env::vars_os())
    }

    pub fn get(&self, key: &str) -> Option<&OsStr> {
        self.vars.get(OsStr::new(key)).map(OsString::as_os_str)
    }

    pub fn set(&mut self, key: impl Into<OsString>, value: impl Into<OsString>) {
        self.vars.insert(key.into(), value.into());
    }

    /// Sets `key` unless it already has a value (bun `put_default`).
    pub fn set_default(&mut self, key: &str, value: impl FnOnce() -> Option<OsString>) {
        if !self.vars.contains_key(OsStr::new(key))
            && let Some(v) = value()
        {
            self.vars.insert(key.into(), v);
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.vars.remove(OsStr::new(key));
    }

    /// Drops every key starting with one of `prefixes`.
    pub fn remove_prefixed(&mut self, prefixes: &[&str]) {
        self.vars.retain(|k, _| {
            !k.to_str()
                .is_some_and(|k| prefixes.iter().any(|p| k.starts_with(p)))
        });
    }

    /// `npm_lifecycle_event` / `npm_lifecycle_script`, which differ per script.
    pub fn set_lifecycle(&mut self, event: &str, script: &str) {
        self.set("npm_lifecycle_event", until_nul(event));
        self.set("npm_lifecycle_script", until_nul(script));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.vars
            .iter()
            .map(|(k, v)| (k.as_os_str(), v.as_os_str()))
    }

    pub fn into_pairs(self) -> Vec<(OsString, OsString)> {
        self.vars.into_iter().collect()
    }
}

/// What `npm_package_*` describes.
#[derive(Debug, Clone, Copy)]
pub struct PackageVars<'a> {
    pub dir: &'a Path,
    pub package_json: &'a Path,
    /// The folder/alias name; replaced by package.json `name` when the caller read it.
    pub name: &'a str,
    pub version: &'a str,
    pub config: &'a [(String, String)],
}

impl PackageVars<'_> {
    /// Writes `npm_package_json`, `npm_package_name`, `npm_package_version` (when non-empty)
    /// and `npm_package_config_<key>` (non-empty keys and values).
    ///
    /// package.json strings may contain `\u0000`, which an environment cannot hold; they are
    /// cut at the first NUL, as bun's NUL-terminated environment block does.
    pub fn apply(&self, env: &mut EnvMap) {
        env.set("npm_package_json", self.package_json.as_os_str());
        let (name, version) = (until_nul(self.name), until_nul(self.version));
        if !name.is_empty() {
            env.set("npm_package_name", name);
        }
        if !version.is_empty() {
            env.set("npm_package_version", version);
        }
        for (k, v) in self.config {
            let (k, v) = (until_nul(k), until_nul(v));
            if !k.is_empty() && !v.is_empty() {
                env.set(format!("npm_package_config_{k}"), v);
            }
        }
    }
}

/// `s` up to its first NUL byte (what a C string of it holds).
pub(crate) fn until_nul(s: &str) -> &str {
    s.split('\0').next().unwrap_or_default()
}

/// `name`, `version` and the string values of `config` from a parsed package.json.
pub fn package_json_vars(
    json: &serde_json::Value,
) -> (Option<String>, Option<String>, Vec<(String, String)>) {
    let s = |k: &str| json.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let config = json
        .get("config")
        .and_then(|c| c.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    (s("name"), s("version"), config)
}

/// The environment shared by every script of one install (bun `configure_env_for_scripts`).
#[derive(Debug, Clone)]
pub struct BaseEnv {
    env: EnvMap,
    /// The inherited PATH (bin dirs resolved, the node-gyp shim dir appended).
    original_path: OsString,
}

impl BaseEnv {
    /// For lifecycle scripts: `inherited` minus `npm_package_*` / `npm_lifecycle_*`,
    /// `npm_config_local_prefix` (always), `npm_config_user_agent`, `npm_execpath`,
    /// `npm_node_execpath`, `NODE`, `INIT_CWD`, `JOBS` (when unset), then `extra` on top.
    /// `node_gyp_dir` is appended to PATH.
    pub fn lifecycle(
        project_dir: &Path,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
        extra: &[(String, String)],
        node_gyp_dir: Option<&Path>,
    ) -> Self {
        let mut env = EnvMap::from_pairs(inherited);
        env.remove_prefixed(&["npm_package_", "npm_lifecycle_"]);
        seed_script_env(&mut env, project_dir);
        env.set_default("INIT_CWD", || Some(project_dir.as_os_str().to_owned()));
        // Run node-gyp jobs in parallel (node-gyp lib/build.js).
        let threads = fnpm_types::available_parallelism();
        if threads > 2 {
            env.set_default("JOBS", || Some(threads.to_string().into()));
        }
        for (k, v) in extra {
            env.set(k, v);
        }
        let mut original_path = resolve_bin_dirs_in_path(env.get("PATH").unwrap_or_default());
        if let Some(dir) = node_gyp_dir {
            append_path(&mut original_path, dir.as_os_str());
        }
        env.set("PATH", original_path.clone());
        Self { env, original_path }
    }

    /// For `fnpm run` / `exec`: `inherited`, `npm_config_local_prefix`, user agent,
    /// `npm_execpath`, `npm_node_execpath`, `npm_command`, then `extra` on top. Inherited
    /// `npm_package_*` stay (an unnamed package keeps an outer run's name, like bun).
    pub fn run(
        project_dir: &Path,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
        extra: &[(String, String)],
        npm_command: &str,
    ) -> Self {
        let mut env = EnvMap::from_pairs(inherited);
        seed_script_env(&mut env, project_dir);
        env.set("npm_command", npm_command);
        for (k, v) in extra {
            env.set(k, v);
        }
        let original_path = env.get("PATH").unwrap_or_default().to_owned();
        Self { env, original_path }
    }

    /// The PATH the caller started with.
    pub fn original_path(&self) -> &OsStr {
        &self.original_path
    }

    pub fn env(&self) -> &EnvMap {
        &self.env
    }

    pub fn env_mut(&mut self) -> &mut EnvMap {
        &mut self.env
    }

    /// The environment of one package's lifecycle scripts: PATH from [`lifecycle_path`] and
    /// `npm_package_*`.
    pub fn for_package(&self, package: &PackageVars<'_>, is_dependency: bool) -> EnvMap {
        let mut env = self.env.clone();
        env.set(
            "PATH",
            lifecycle_path(package.dir, &self.original_path, is_dependency),
        );
        package.apply(&mut env);
        env
    }
}

/// bun `seed_script_env` + `npm_config_local_prefix`.
fn seed_script_env(env: &mut EnvMap, project_dir: &Path) {
    // Overwritten like npm does: an outer run in another project may have left a stale value.
    env.set("npm_config_local_prefix", project_dir.as_os_str());
    let node_version = env
        .get("FNPM_NODE_VERSION")
        .and_then(OsStr::to_str)
        .map(str::to_owned);
    env.set_default("npm_config_user_agent", || {
        Some(user_agent(node_version.as_deref()).into())
    });
    let execpath = env.get("FNPM_EXECPATH").map(OsStr::to_owned);
    env.set_default("npm_execpath", || {
        execpath.or_else(|| std::env::current_exe().ok().map(PathBuf::into_os_string))
    });
    let path = env.get("PATH").unwrap_or_default().to_owned();
    let node = node_executable(&path);
    if let Some(node) = node {
        let node = node.into_os_string();
        env.set_default("npm_node_execpath", || Some(node.clone()));
        env.set_default("NODE", || Some(node));
    }
    if env
        .get("FNPM_FEATURE_FLAG_NO_ORPHANS")
        .is_some_and(|v| v == "1")
    {
        env.set_default("BUN_FEATURE_FLAG_NO_ORPHANS", || Some("1".into()));
    }
}

/// `fnpm/<version> npm/? node/<node version> <os> <arch>` (the `npm/?` copies yarn).
pub fn user_agent(node_version: Option<&str>) -> String {
    let node = node_version
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_NODE_VERSION);
    let node = if node.starts_with('v') {
        node.to_owned()
    } else {
        format!("v{node}")
    };
    format!(
        "fnpm/{FNPM_VERSION} npm/? node/{node} {} {}",
        os_name(),
        arch_name()
    )
}

/// Node's `process.platform` names.
pub fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Node's `process.arch` names.
pub fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "arm64",
        "arm" => "arm",
        _ => "unknown",
    }
}

/// The Node.js binary scripts see: this process when it is node (the napi addon), else
/// `node` on `path`.
fn node_executable(path: &OsStr) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && exe.file_stem().is_some_and(|s| s == "node")
    {
        return Some(exe);
    }
    crate::shell::which("node", path, Path::new("/"))
}

fn append_path(path: &mut OsString, entry: &OsStr) {
    if !path.is_empty() && !path.to_string_lossy().ends_with(PATH_DELIMITER) {
        path.push(PATH_DELIMITER_STR);
    }
    path.push(entry);
}

#[cfg(windows)]
const PATH_DELIMITER: char = ';';
#[cfg(not(windows))]
const PATH_DELIMITER: char = ':';
#[cfg(windows)]
const PATH_DELIMITER_STR: &str = ";";
#[cfg(not(windows))]
const PATH_DELIMITER_STR: &str = ":";

fn bin_dir(dir: &Path) -> PathBuf {
    dir.join("node_modules").join(".bin")
}

/// Directories inside an entry of the isolated layout (`node_modules/.fnpm/<key>/...`) or the
/// global virtual store (`<store>/v1/links/<key>/...`) do not end a dependency's own `.bin`
/// chain (bun: the path contains `/node_modules/.bun/`).
fn in_store_entry(dir: &Path) -> bool {
    let parts = normal_components(dir);
    parts
        .windows(3)
        .any(|w| w[0] == "node_modules" && w[1] == ".fnpm")
        || in_global_store(dir)
}

fn in_global_store(dir: &Path) -> bool {
    normal_components(dir)
        .windows(4)
        .any(|w| w[0] == "store" && is_layout_dir(w[1]) && w[2] == "links")
}

/// `v1`, `v2`, ...: a store layout directory (`fnpm_config::STORE_LAYOUT` and older ones).
fn is_layout_dir(c: &OsStr) -> bool {
    c.to_str().is_some_and(|s| {
        s.len() > 1 && s.starts_with('v') && s[1..].bytes().all(|b| b.is_ascii_digit())
    })
}

fn normal_components(dir: &Path) -> Vec<&OsStr> {
    dir.components()
        .filter_map(|c| {
            if let Component::Normal(n) = c {
                Some(n)
            } else {
                None
            }
        })
        .collect()
}

/// PATH for a package's lifecycle scripts (bun `spawn_package_lifecycle_scripts`): every
/// `node_modules/.bin` from `cwd` up to `/`. For a dependency the inherited PATH (without the
/// `.bin` dirs of `cwd` and its ancestors that an outer `fnpm run` put in front) goes right
/// after the dependency's own chain, before the first ancestor outside the store, because an
/// untrusted package can link a `node` or `sh` into a shared `.bin`. For the project and its
/// workspaces the inherited PATH comes last.
pub fn lifecycle_path(cwd: &Path, inherited: &OsStr, is_dependency: bool) -> OsString {
    let mut entries: Vec<OsString> = Vec::new();
    let mut pending_inherited = is_dependency.then_some(inherited);
    let mut parent = Some(cwd);
    while let Some(dir) = parent {
        if dir != cwd
            && !in_store_entry(dir)
            && let Some(inh) = pending_inherited.take()
        {
            push_without_ancestor_bins(&mut entries, inh, cwd);
        }
        entries.push(bin_dir(dir).into_os_string());
        parent = dir.parent();
    }
    match pending_inherited {
        Some(inh) => push_without_ancestor_bins(&mut entries, inh, cwd),
        None if is_dependency => {}
        None => {
            if !inherited.is_empty() {
                entries.push(inherited.to_owned());
            }
        }
    }
    join_paths(entries)
}

fn join_paths(entries: Vec<OsString>) -> OsString {
    let mut out = OsString::new();
    for (i, e) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(PATH_DELIMITER_STR);
        }
        out.push(e);
    }
    out
}

/// bun `append_path_without_ancestor_bins`.
fn push_without_ancestor_bins(entries: &mut Vec<OsString>, inherited: &OsStr, cwd: &Path) {
    for entry in split_path_keep_empty(inherited) {
        let p = Path::new(&entry);
        let is_ancestor_bin = p.ends_with(Path::new("node_modules").join(".bin"))
            && p.parent()
                .and_then(Path::parent)
                .is_some_and(|dir| cwd.starts_with(dir));
        if !is_ancestor_bin {
            entries.push(entry);
        }
    }
}

fn split_path_keep_empty(path: &OsStr) -> Vec<OsString> {
    if path.is_empty() {
        return Vec::new();
    }
    path.to_string_lossy()
        .split(PATH_DELIMITER)
        .map(OsString::from)
        .collect()
}

/// bun `resolve_bin_dirs_in_path`: rewrites absolute `.../node_modules/.bin` entries of PATH
/// to their real paths, so they compare equal to a script's (real) cwd.
pub fn resolve_bin_dirs_in_path(path: &OsStr) -> OsString {
    let entries = split_path_keep_empty(path)
        .into_iter()
        .map(|entry| {
            let p = Path::new(&entry);
            let trimmed = p.components().as_path();
            if trimmed.is_absolute()
                && trimmed.ends_with(Path::new("node_modules").join(".bin"))
                && let Some(dir) = trimmed.parent().and_then(Path::parent)
                && let Ok(real) = std::fs::canonicalize(dir)
            {
                return bin_dir(&real).into_os_string();
            }
            entry
        })
        .collect();
    join_paths(entries)
}

/// PATH for `fnpm run` (bun `path_for_run`): `<dir>/node_modules/.bin` for `cwd` and each
/// ancestor, then the original PATH.
pub fn path_for_run(cwd: &Path, original: &OsStr) -> OsString {
    let mut entries: Vec<OsString> = cwd
        .ancestors()
        .map(|d| bin_dir(d).into_os_string())
        .collect();
    entries.push(original.to_owned());
    join_paths(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &OsStr) -> String {
        p.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn root_path_puts_inherited_last() {
        let p = lifecycle_path(Path::new("/proj"), OsStr::new("/usr/bin:/bin"), false);
        assert_eq!(
            s(&p),
            "/proj/node_modules/.bin:/node_modules/.bin:/usr/bin:/bin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dependency_path_puts_inherited_before_ancestors() {
        let p = lifecycle_path(
            Path::new("/proj/node_modules/dep"),
            OsStr::new("/usr/bin"),
            true,
        );
        assert_eq!(
            s(&p),
            "/proj/node_modules/dep/node_modules/.bin:/usr/bin:/proj/node_modules/node_modules/.bin:\
             /proj/node_modules/.bin:/node_modules/.bin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn isolated_store_entries_keep_their_chain() {
        let cwd = Path::new("/p/node_modules/.fnpm/dep@1.0.0/node_modules/dep");
        let p = lifecycle_path(cwd, OsStr::new("/usr/bin"), true);
        let parts: Vec<String> = s(&p).split(':').map(str::to_owned).collect();
        assert_eq!(
            parts[0],
            "/p/node_modules/.fnpm/dep@1.0.0/node_modules/dep/node_modules/.bin"
        );
        assert_eq!(
            parts[1],
            "/p/node_modules/.fnpm/dep@1.0.0/node_modules/node_modules/.bin"
        );
        assert_eq!(
            parts[2],
            "/p/node_modules/.fnpm/dep@1.0.0/node_modules/.bin"
        );
        let usr = parts.iter().position(|p| p == "/usr/bin").unwrap();
        let root_bin = parts
            .iter()
            .position(|p| p == "/p/node_modules/.bin")
            .unwrap();
        assert!(usr < root_bin);
    }

    #[cfg(unix)]
    #[test]
    fn inherited_ancestor_bins_are_dropped_for_dependencies() {
        let inherited = OsStr::new("/proj/node_modules/.bin:/usr/bin:/other/node_modules/.bin");
        let p = lifecycle_path(Path::new("/proj/node_modules/dep"), inherited, true);
        let parts: Vec<String> = s(&p).split(':').map(str::to_owned).collect();
        assert_eq!(parts[1], "/usr/bin");
        assert_eq!(parts[2], "/other/node_modules/.bin");
        assert_eq!(
            parts
                .iter()
                .filter(|p| *p == "/proj/node_modules/.bin")
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_path() {
        assert_eq!(
            s(&path_for_run(Path::new("/a/b"), OsStr::new("/bin"))),
            "/a/b/node_modules/.bin:/a/node_modules/.bin:/node_modules/.bin:/bin"
        );
    }

    #[test]
    fn dependency_dirs() {
        assert!(is_dependency_dir(Path::new("/p/node_modules/x")));
        assert!(!is_dependency_dir(Path::new("/p/packages/x")));
        assert!(is_dependency_dir(Path::new(
            "/h/.fnpm/store/v1/links/x-1/node_modules/x"
        )));
    }

    #[test]
    fn user_agent_format() {
        let ua = user_agent(Some("20.1.0"));
        assert!(
            ua.starts_with(&format!("fnpm/{FNPM_VERSION} npm/? node/v20.1.0 ")),
            "{ua}"
        );
        assert!(user_agent(None).contains(DEFAULT_NODE_VERSION));
    }

    #[test]
    fn build_sets_npm_vars() {
        let inherited = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("npm_package_name"), OsString::from("outer")),
            (
                OsString::from("npm_lifecycle_event"),
                OsString::from("outer"),
            ),
            (OsString::from("npm_package_extra"), OsString::from("x")),
            (OsString::from("INIT_CWD"), OsString::from("/kept")),
            (
                OsString::from("npm_config_local_prefix"),
                OsString::from("/stale"),
            ),
            (
                OsString::from("npm_config_user_agent"),
                OsString::from("yarn/1"),
            ),
        ];
        let config = vec![
            ("port".to_owned(), "8080".to_owned()),
            ("".to_owned(), "x".to_owned()),
        ];
        let ctx = ScriptContext {
            project_dir: Path::new("/proj"),
            package_dir: Path::new("/proj/node_modules/dep"),
            package_json: Path::new("/proj/node_modules/dep/package.json"),
            package_name: "dep",
            package_version: "1.2.3",
            config: &config,
            event: "postinstall",
            script: "node x.js",
        };
        let env: BTreeMap<String, String> = build(&ctx, &inherited)
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(env["npm_lifecycle_event"], "postinstall");
        assert_eq!(env["npm_lifecycle_script"], "node x.js");
        assert_eq!(env["npm_package_name"], "dep");
        assert_eq!(env["npm_package_version"], "1.2.3");
        assert_eq!(env["npm_package_config_port"], "8080");
        assert!(!env.contains_key("npm_package_config_"));
        assert!(!env.contains_key("npm_package_extra"));
        assert_eq!(
            env["npm_package_json"],
            "/proj/node_modules/dep/package.json"
        );
        assert_eq!(env["INIT_CWD"], "/kept");
        assert_eq!(env["npm_config_local_prefix"], "/proj");
        assert_eq!(env["npm_config_user_agent"], "yarn/1");
        assert!(env.contains_key("npm_execpath"));
        assert!(env["PATH"].starts_with("/proj/node_modules/dep/node_modules/.bin"));
    }

    #[test]
    fn package_json_vars_reads_strings() {
        let json =
            serde_json::json!({"name": "a", "version": "1.0.0", "config": {"x": "1", "y": 2}});
        let (name, version, config) = package_json_vars(&json);
        assert_eq!(name.as_deref(), Some("a"));
        assert_eq!(version.as_deref(), Some("1.0.0"));
        assert_eq!(config, vec![("x".to_owned(), "1".to_owned())]);
    }
}
