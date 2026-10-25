//! Script environment (bun `PackageManagerLifecycle.rs`, `lifecycle_script_runner.rs`).

use std::ffi::OsString;
use std::path::Path;

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
pub fn build(_ctx: &ScriptContext<'_>, _inherited: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    todo!()
}
