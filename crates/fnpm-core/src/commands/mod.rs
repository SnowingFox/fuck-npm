//! Non-install commands. `request.command` is the path the CLI registry
//! (`packages/fuck-npm/src/cli/trees/*.ts`) defines: `outdated`, `why`, `ls`, `info`, `link`,
//! `unlink`, `patch`, `patch-commit`, `prune`, `dedupe`, `audit`, `pack`, `publish`, `run`,
//! `exec`, `x`, `create`, `init`, `pm …` (and its `cache` / `pkg` groups), `store …`,
//! `doctor`, the `whoami` / `migrate` aliases, and `lock …` for `fnpm-lock`. The table in
//! ARCHITECTURE.md#fnpm-core maps each to its bun source.
//!
//! Every command returns [`CoreResult`]: `data` is the `--json` payload and `lines` the
//! pre-rendered human output (plain ASCII tables unless the request sets `options.color`).

mod files;
mod info;
mod inspect;
mod link;
mod maintenance;
mod outdated;
pub(crate) mod pack;
mod scripts;
mod table;

use std::path::PathBuf;
use std::sync::Arc;

use fnpm_config::Env;
use fnpm_types::{EventSink, LockGraph};
use fnpm_workspace::Workspace;
use serde_json::{Map, Value};

use crate::options::PmOptions;
use crate::project::Project;
use crate::{CoreRequest, CoreResult, Error, Result};

pub use outdated::OutdatedEntry;

/// One command invocation: the request plus what every command needs.
pub(crate) struct Ctx {
    pub req: CoreRequest,
    pub events: Arc<dyn EventSink>,
    pub env: Env,
    pub pm: PmOptions,
}

/// Parses the shared flags out of `options`, dropping any key whose value does not fit (the
/// CLI also sends command-specific flags under the same map).
fn pm_options(cwd: &std::path::Path, options: &Map<String, Value>) -> PmOptions {
    let mut map = options.clone();
    map.insert(
        "cwd".into(),
        Value::String(cwd.to_string_lossy().into_owned()),
    );
    if let Ok(pm) = serde_json::from_value::<PmOptions>(Value::Object(map.clone())) {
        return pm;
    }
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in keys {
        let mut one = Map::new();
        one.insert(k.clone(), map[&k].clone());
        if serde_json::from_value::<PmOptions>(Value::Object(one)).is_err() {
            map.remove(&k);
        }
    }
    serde_json::from_value(Value::Object(map)).unwrap_or_default()
}

impl Ctx {
    fn new(req: CoreRequest, events: Arc<dyn EventSink>, env: Env) -> Self {
        let cwd = if req.cwd.as_os_str().is_empty() {
            req.options
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_default()
        } else {
            req.cwd.clone()
        };
        let cwd = crate::project::absolute_cwd(&cwd);
        let pm = pm_options(&cwd, &req.options);
        let mut req = req;
        req.cwd = cwd;
        Self {
            req,
            events,
            env,
            pm,
        }
    }

    /// A flag value.
    pub fn opt(&self, key: &str) -> Option<&Value> {
        self.req.options.get(key).filter(|v| !v.is_null())
    }

    /// A boolean flag (`true`, or a non-empty string other than `false`).
    pub fn flag(&self, key: &str) -> bool {
        match self.opt(key) {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => !s.is_empty() && s != "false",
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            _ => false,
        }
    }

    /// A string flag.
    pub fn string(&self, key: &str) -> Option<String> {
        match self.opt(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Array(a) => a.last().and_then(Value::as_str).map(str::to_owned),
            _ => None,
        }
    }

    /// A repeatable flag.
    pub fn strings(&self, key: &str) -> Vec<String> {
        match self.opt(key) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        }
    }

    /// A numeric flag.
    pub fn number(&self, key: &str) -> Option<u64> {
        match self.opt(key)? {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// Box-drawing tables (a colour terminal) instead of ASCII.
    pub fn unicode(&self) -> bool {
        self.flag("color")
    }

    /// Loads the project (configuration) from the request's cwd.
    pub fn project(&self) -> Result<Project> {
        Project::load(
            &self.req.cwd,
            self.pm.to_cli_overrides(),
            self.env.clone(),
            false,
        )
    }

    /// [`Project::load_anywhere`]: the configuration even outside a project.
    pub fn project_anywhere(&self) -> Result<Project> {
        Project::load_anywhere(&self.req.cwd, self.pm.to_cli_overrides(), self.env.clone())
    }

    /// The project, its workspace and its lockfile graph (`None` without a lockfile).
    pub fn load(&self) -> Result<(Project, Workspace, Option<LockGraph>)> {
        let project = self.project()?;
        let ws = project.workspace(&*self.events)?;
        let lock = crate::install::load_lock(&project.config, &ws)?;
        for w in &lock.warnings {
            self.events.emit(fnpm_types::Event::warn(w.clone(), None));
        }
        Ok((project, ws, lock.graph))
    }

    /// [`Ctx::load`] that requires a lockfile (bun: `error: Lockfile not found`, with the
    /// command's `nothing <x>` suffix).
    pub fn load_locked(&self, nothing: &str) -> Result<(Project, Workspace, LockGraph)> {
        let (p, ws, g) = self.load()?;
        match g {
            Some(g) => Ok((p, ws, g)),
            None => Err(Error::usage(format!("missing lockfile, {nothing}"))),
        }
    }
}

/// A result with data and lines.
pub(crate) fn ok(data: Value, lines: Vec<String>) -> CoreResult {
    CoreResult {
        exit_code: 0,
        data: Some(data),
        lines: Some(lines),
    }
}

/// A result with a non-zero exit code.
pub(crate) fn exit(code: i32, data: Value, lines: Vec<String>) -> CoreResult {
    CoreResult {
        exit_code: code,
        data: Some(data),
        lines: Some(lines),
    }
}

fn not_implemented(path: &[&str]) -> Error {
    Error::NotImplemented {
        command: path.join(" "),
    }
}

/// Runs `request.command`.
pub async fn dispatch(
    request: CoreRequest,
    events: Arc<dyn EventSink>,
    env: Env,
) -> Result<CoreResult> {
    let command = request.command.clone();
    let path: Vec<&str> = command.iter().map(String::as_str).collect();
    let cx = Ctx::new(request, events, env);
    match path.as_slice() {
        ["outdated"] => outdated::run(&cx).await,
        ["why"] | ["pm", "why"] => inspect::why(&cx),
        ["ls"] | ["list"] | ["pm", "ls" | "list"] => inspect::ls(&cx),
        ["info" | "view" | "show"] | ["pm", "view" | "info"] => info::view(&cx).await,
        ["whoami"] | ["pm", "whoami"] => info::whoami(&cx).await,
        ["audit"] | ["pm", "audit"] => info::audit(&cx, false).await,
        ["link"] => link::link(&cx).await,
        ["unlink"] => link::unlink(&cx).await,
        ["prune"] => maintenance::prune(&cx),
        ["dedupe" | "ddp"] => maintenance::dedupe(&cx, false).await,
        ["run"] => scripts::run(&cx).await,
        ["exec"] => scripts::exec(&cx).await,
        ["init"] => files::init(&cx),
        ["migrate"] | ["pm", "migrate"] => maintenance::migrate(&cx, false),
        ["cache", rest @ ..] | ["pm", "cache", rest @ ..] => maintenance::cache(&cx, rest),
        ["pkg", rest @ ..] | ["pm", "pkg", rest @ ..] => files::pkg(&cx, rest),
        ["store", rest @ ..] => maintenance::store(&cx, rest),
        ["doctor"] => maintenance::doctor(&cx).await,
        ["pm", "bin"] => inspect::bin(&cx),
        ["pm", "hash"] => inspect::hash(&cx, inspect::Hash::Digest),
        ["pm", "hash-string"] => inspect::hash(&cx, inspect::Hash::String),
        ["pm", "hash-print"] => inspect::hash(&cx, inspect::Hash::Print),
        ["pm", "untrusted"] => scripts::untrusted(&cx),
        ["pm", "trust"] => scripts::trust(&cx).await,
        ["pm", "default-trusted"] => scripts::default_trusted(),
        ["pm", "version"] => files::version(&cx).await,
        ["pm", "licenses"] => inspect::licenses(&cx),
        ["pm", "fetch"] => maintenance::fetch(&cx).await,
        ["lock", "migrate"] => maintenance::migrate(&cx, true),
        ["lock", "export"] => maintenance::export(&cx),
        ["lock", "verify"] => inspect::verify(&cx),
        ["lock", "diff"] => inspect::diff(&cx),
        ["lock", "why"] => inspect::why(&cx),
        ["lock", "print"] => inspect::print(&cx),
        ["lock", "dedupe"] => maintenance::dedupe(&cx, true).await,
        ["lock", "audit"] => info::audit(&cx, true).await,
        // Later parity phases (docs/ROADMAP.md P4): a typed error, never a panic.
        ["pack"] | ["pm", "pack"] => pack::run(&cx).await,
        ["patch"]
        | ["patch-commit"]
        | ["publish"]
        | ["x" | "dlx"]
        | ["create"]
        | ["pm", "diff" | "scan"] => Err(not_implemented(&path)),
        _ => Err(Error::UnknownCommand(command)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pm_options_drop_mismatched_keys() {
        let mut m = Map::new();
        m.insert("registry".into(), Value::String("http://x/".into()));
        m.insert("omit".into(), Value::String("dev".into())); // wrong type
        m.insert("depth".into(), Value::from(2)); // not a PmOptions field
        let pm = pm_options(std::path::Path::new("/p"), &m);
        assert_eq!(pm.registry.as_deref(), Some("http://x/"));
        assert!(pm.omit.is_empty());
        assert!(pm.save, "defaults kept");
        assert_eq!(pm.cwd, PathBuf::from("/p"));
    }
}
