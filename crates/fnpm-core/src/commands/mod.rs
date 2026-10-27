//! Non-install commands. `request.command` is the path the CLI registry
//! (`packages/fuck-npm/src/cli/trees/*.ts`) defines: `outdated`, `why`, `ls`, `info`, `link`,
//! `unlink`, `patch`, `patch-commit`, `prune`, `dedupe`, `audit`, `pack`, `publish`, `run`,
//! `exec`, `x`, `create`, `init`, `pm …` (and its `cache` / `pkg` groups), `store …`,
//! `doctor`, the `whoami` / `migrate` aliases, and `lock …` for `fnpm-lock`. The table in
//! ARCHITECTURE.md#fnpm-core maps each to its bun source.
//!
//! Every command returns [`CoreResult`]: `data` is the `--json` payload and `lines` the
//! pre-rendered human output (plain ASCII tables unless the request sets `options.color`).

mod audit_fix;
mod diff;
mod files;
mod info;
mod inspect;
mod json_text;
mod licenses;
mod link;
mod ls;
mod maintenance;
mod outdated;
pub(crate) mod pack;
pub(crate) mod patch;
mod pkg;
mod prune;
mod publish;
mod scan;
mod scripts;
mod table;
mod update_interactive;
mod view;
mod why;
pub(crate) use update_interactive::edits as interactive_edits;

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
        self.report_lock(&lock);
        Ok((project, ws, lock.graph))
    }

    /// What loading the lockfile printed in bun: the failures, `migrated lockfile from <file>`
    /// for another manager's lockfile (read only; nothing is written), the warnings.
    fn report_lock(&self, lock: &crate::install::LockState) {
        for e in &lock.failures {
            self.events.emit(e.clone());
        }
        if lock.migrated
            && matches!(
                lock.format,
                Some(
                    fnpm_lockfile::Format::Npm
                        | fnpm_lockfile::Format::YarnV1
                        | fnpm_lockfile::Format::YarnBerry
                        | fnpm_lockfile::Format::Pnpm
                )
            )
            && let Some(name) = lock.path.as_deref().and_then(std::path::Path::file_name)
        {
            self.events.emit(fnpm_types::Event::notice(format!(
                "migrated lockfile from {}",
                name.to_string_lossy()
            )));
        }
        for w in &lock.warnings {
            self.events.emit(fnpm_types::Event::warn(w.clone(), None));
        }
    }

    /// [`Ctx::load`] that requires a lockfile (bun: `error: Lockfile not found`, with the
    /// command's `nothing <x>` suffix). One that does not parse stops the command, with its
    /// source frame.
    pub fn load_locked(&self, nothing: &str) -> Result<(Project, Workspace, LockGraph)> {
        self.load_locked_with(nothing, true)
    }

    /// [`Ctx::load_locked`]; `framed: false` names only the failure (bun `pm ls`).
    pub fn load_locked_with(
        &self,
        nothing: &str,
        framed: bool,
    ) -> Result<(Project, Workspace, LockGraph)> {
        let project = self.project()?;
        let ws = project.workspace(&*self.events)?;
        let lock = crate::install::load_lock(&project.config, &ws)?;
        if let Some(failure) = &lock.parse_failure {
            return Err(failure.read_error(framed));
        }
        self.report_lock(&lock);
        match lock.graph {
            Some(g) => Ok((project, ws, g)),
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

/// `fnpm x` / `fnpm create` for the TS CLI: the arguments come raw (in `passthrough`), and the
/// bin runs as a child of this process with inherited stdio.
async fn x_command(cx: &Ctx, create: bool) -> Result<CoreResult> {
    let mut args = cx.req.positionals.clone();
    args.extend(cx.req.passthrough.iter().cloned());
    let req = crate::x::Request {
        args,
        cwd: cx.req.cwd.clone(),
        create,
        // The bare version (`--version`), without the revision headers carry.
        version: cx
            .string("cliVersion")
            .map(|v| v.split(" (").next().unwrap_or_default().to_owned())
            .unwrap_or_else(|| crate::version().to_owned()),
        revision: cx.string("cliRevision").unwrap_or_default(),
        display: None,
    };
    let code = tokio::task::spawn_blocking(move || crate::x::run(&req))
        .await
        .map_err(|e| Error::usage(format!("fnpm x failed: {e}")))?;
    Ok(exit(
        code,
        serde_json::json!({ "exitCode": code }),
        Vec::new(),
    ))
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
        ["why"] | ["pm", "why"] => why::why(&cx),
        ["ls"] | ["list"] | ["pm", "ls" | "list"] => ls::ls(&cx),
        ["info" | "view" | "show"] | ["pm", "view" | "info"] => view::view(&cx).await,
        ["whoami"] | ["pm", "whoami"] => info::whoami(&cx).await,
        ["audit"] | ["pm", "audit"] => info::audit(&cx, false).await,
        ["link"] => link::link(&cx).await,
        ["unlink"] => link::unlink(&cx).await,
        ["prune"] => prune::prune(&cx),
        ["dedupe" | "ddp"] => maintenance::dedupe(&cx, false).await,
        ["run"] => scripts::run(&cx).await,
        ["exec"] => scripts::exec(&cx).await,
        ["init"] => files::init(&cx),
        ["migrate"] | ["pm", "migrate"] => maintenance::migrate(&cx, false).await,
        ["cache", rest @ ..] | ["pm", "cache", rest @ ..] => maintenance::cache(&cx, rest),
        ["pkg", rest @ ..] | ["pm", "pkg", rest @ ..] => pkg::run(&cx, rest),
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
        ["pm", "licenses"] => licenses::run(&cx),
        ["pm", "fetch"] => maintenance::fetch(&cx).await,
        ["lock", "migrate"] => maintenance::migrate(&cx, true).await,
        ["lock", "export"] => maintenance::export(&cx),
        ["lock", "verify"] => inspect::verify(&cx),
        ["lock", "diff"] => inspect::diff(&cx),
        ["lock", "why"] => why::why(&cx),
        ["lock", "print"] => inspect::print(&cx),
        ["lock", "dedupe"] => maintenance::dedupe(&cx, true).await,
        ["lock", "audit"] => info::audit(&cx, true).await,
        ["pack"] | ["pm", "pack"] => pack::run(&cx).await,
        ["publish"] => publish::run(&cx).await,
        ["patch"] => patch::run(&cx, false).await,
        ["patch-commit"] => patch::run(&cx, true).await,
        ["pm", "diff"] => diff::run(&cx).await,
        ["pm", "scan"] => tokio::task::spawn_blocking(move || scan::scan(&cx))
            .await
            .map_err(|e| Error::usage(format!("security scanner task failed: {e}")))?,
        // Internal: the package.json edits of `fnpm update -i` (the TS CLI draws the picker).
        ["x" | "dlx"] => x_command(&cx, false).await,
        ["create"] => x_command(&cx, true).await,
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
