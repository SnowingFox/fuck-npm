//! `fnpm pm untrusted`, `fnpm pm trust <names...> | --all [--dry-run]` and
//! `fnpm pm default-trusted` (bun `pm_trusted_command.rs`). fnpm-core finds the installed
//! packages (it knows the linker layout), these functions decide, render and build jobs.

use std::path::PathBuf;

use fnpm_types::{LockGraph, PackageId, Resolution};

use crate::scripts::{ScriptSource, job_scripts};
use crate::trust::{TrustPolicy, can_trust_by_name, is_trusted_folder_package, trusted_name};
use crate::{Error, Job, Result};

/// bun `CANNOT_TRUST`.
pub const CANNOT_TRUST: &str = "cannot trust by name: add it to your package.json dependencies";

/// An installed package, as the linker placed it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub package: PackageId,
    /// The dependency name it is installed under.
    pub alias: String,
    /// Its directory.
    pub dir: PathBuf,
    /// Depth of its `node_modules` (0 for the root's).
    pub depth: usize,
    /// Linked from the global virtual store: other projects share it, so its scripts only
    /// run on the next install, which moves it into the project.
    pub in_global_store: bool,
}

/// An installed dependency whose lifecycle scripts the installers do not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untrusted {
    pub package: PackageId,
    pub alias: String,
    /// The name `trustedDependencies` knows it by.
    pub trusted_name: String,
    pub dir: PathBuf,
    pub depth: usize,
    pub resolution: String,
    /// `(event, command)` that run once it is trusted.
    pub scripts: Vec<(String, String)>,
    pub can_trust: bool,
    pub in_global_store: bool,
}

/// bun `Blocked::scripts_at` over every candidate: the dependencies with scripts that are not
/// trusted. `file:` folders inside their declarer are skipped like in an install.
pub fn find_untrusted<'r>(
    graph: &LockGraph,
    policy: &TrustPolicy,
    candidates: impl IntoIterator<Item = Candidate>,
    registry_for: &dyn Fn(&str) -> &'r str,
) -> Result<Vec<Untrusted>> {
    let mut out = Vec::new();
    for c in candidates {
        let Some(pkg) = graph.get(c.package) else {
            continue;
        };
        if matches!(
            pkg.resolution,
            Resolution::Root | Resolution::Workspace { .. }
        ) || policy.has_trusted_dependency(graph, c.package, registry_for)
        {
            continue;
        }
        if matches!(pkg.resolution, Resolution::Folder { .. })
            && !is_trusted_folder_package(graph, c.package)
        {
            continue;
        }
        let source = ScriptSource::from_resolution(&pkg.resolution);
        let scripts = match job_scripts(&c.dir, source, true) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::Io {
                    path: c.dir.join("package.json"),
                    source,
                });
            }
        };
        if scripts.is_empty() {
            continue;
        }
        out.push(Untrusted {
            trusted_name: trusted_name(&c.alias, &pkg.name, &pkg.resolution).to_owned(),
            can_trust: can_trust_by_name(graph, &c.alias, c.package),
            resolution: pkg.resolution.to_string(),
            package: c.package,
            alias: c.alias,
            dir: c.dir,
            depth: c.depth,
            scripts,
            in_global_store: c.in_global_store,
        });
    }
    Ok(out)
}

/// How a listing marks scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintFormat {
    /// ` ✓ [event]: command`
    Completed,
    /// ` » [event]: command`
    Untrusted,
}

/// bun `List::print_scripts` + `Blocked::print`: the package's folder (relative from its
/// first `node_modules`), its resolution, its scripts, notes, and a blank line.
pub fn render_entry(u: &Untrusted, format: PrintFormat) -> Vec<String> {
    let mut lines = Vec::new();
    let dir = u.dir.to_string_lossy().replace('\\', "/");
    let dir = dir.trim_end_matches('/');
    let shown = match dir.find("/node_modules/") {
        Some(i) => format!("./{}", &dir[i + 1..]),
        None => dir.to_owned(),
    };
    lines.push(format!(
        "{} @{}",
        escape_control(&shown),
        escape_control(&u.resolution)
    ));
    for (event, command) in &u.scripts {
        let mark = match format {
            PrintFormat::Completed => "✓",
            PrintFormat::Untrusted => "»",
        };
        lines.push(format!(" {mark} [{event}]: {}", escape_control(command)));
    }
    if u.trusted_name != u.alias {
        lines.push(format!(" alias of \"{}\"", u.trusted_name));
    }
    if !u.can_trust {
        lines.push(format!(" {CANNOT_TRUST}"));
    }
    lines.push(String::new());
    lines
}

fn escape_control(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// `fnpm pm untrusted` output (after the `fnpm pm untrusted v<version>` header).
pub fn render_untrusted(list: &[Untrusted], global: bool) -> Vec<String> {
    if list.is_empty() {
        return vec![
            "Found 0 untrusted dependencies with scripts.".into(),
            String::new(),
            "This means all packages with scripts are in \"trustedDependencies\" or none of your dependencies have scripts.".into(),
            String::new(),
            "For more information, visit https://bun.com/docs/install/lifecycle#trusteddependencies".into(),
        ];
    }
    let mut lines: Vec<String> = list
        .iter()
        .flat_map(|u| render_entry(u, PrintFormat::Untrusted))
        .collect();
    lines.push("These dependencies had their lifecycle scripts blocked during install.".into());
    lines.push(String::new());
    lines.push(format!(
        "If you trust them and wish to run their scripts, use `fnpm pm {}trust`.",
        if global { "-g " } else { "" }
    ));
    lines
}

/// `fnpm pm default-trusted` output.
pub fn render_default_trusted() -> Vec<String> {
    let list = crate::trust::default_trusted();
    let mut lines = Vec::with_capacity(list.len() + 1);
    lines.push(format!("Default trusted dependencies ({}):", list.len()));
    lines.extend(list.iter().map(|n| format!(" - {n}")));
    lines
}

/// What `fnpm pm trust` does with each untrusted package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Skip,
    Run,
    /// Trusted, but its scripts run on the next install (it is in the global store).
    Defer,
}

/// The decisions of `fnpm pm trust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustPlan {
    /// Per entry of the untrusted list (sorted deepest first).
    pub actions: Vec<Action>,
    /// The list, sorted deepest first (scripts run deepest first, like during an install).
    pub order: Vec<usize>,
    /// Names to add to `trustedDependencies`, in the order found.
    pub names_to_add: Vec<String>,
    /// `(alias given, name it is trusted under)`.
    pub aliased: Vec<(String, String)>,
    /// Names that cannot be trusted by name.
    pub undeclared: Vec<String>,
    pub scripts_count: usize,
}

/// Checks the flags of `fnpm pm trust` with bun's messages.
pub fn check_trust_command(
    names: &[String],
    all: bool,
    ignore_scripts: bool,
    frozen: bool,
    dry_run: bool,
) -> Result<()> {
    if !all && names.is_empty() {
        return Err(Error::Message("expected package names(s) or --all".into()));
    }
    if ignore_scripts {
        return Err(Error::Message(
            "lifecycle scripts are disabled (ignore-scripts), nothing to run".into(),
        ));
    }
    if frozen && !dry_run {
        return Err(Error::Message(
            "lockfile is frozen. refusing to update package.json & lockfile".into(),
        ));
    }
    Ok(())
}

/// Decides what `fnpm pm trust <names> | --all` runs. Names compare exactly. Errors with bun's
/// message (and notes) when nothing can be trusted.
pub fn plan_trust(list: &[Untrusted], names: &[String], all: bool) -> Result<TrustPlan> {
    let mut order: Vec<usize> = (0..list.len()).collect();
    order.sort_by(|&a, &b| list[b].depth.cmp(&list[a].depth));
    let named = |n: &str| names.iter().any(|x| x == n);
    let mut plan = TrustPlan {
        actions: vec![Action::Skip; list.len()],
        order,
        names_to_add: Vec::new(),
        aliased: Vec::new(),
        undeclared: Vec::new(),
        scripts_count: 0,
    };
    for &i in &plan.order {
        let u = &list[i];
        if !all && !named(&u.trusted_name) {
            if u.trusted_name != u.alias
                && named(&u.alias)
                && !plan.aliased.iter().any(|(a, _)| *a == u.alias)
            {
                plan.aliased.push((u.alias.clone(), u.trusted_name.clone()));
            }
            continue;
        }
        if !u.can_trust {
            if !plan.undeclared.contains(&u.trusted_name) {
                plan.undeclared.push(u.trusted_name.clone());
            }
            continue;
        }
        if !plan.names_to_add.contains(&u.trusted_name) {
            plan.names_to_add.push(u.trusted_name.clone());
        }
        if u.in_global_store {
            plan.actions[i] = Action::Defer;
        } else {
            plan.actions[i] = Action::Run;
            plan.scripts_count += u.scripts.len();
        }
    }
    if plan.names_to_add.is_empty() {
        let mut message = if all {
            "0 scripts ran. This means all dependencies are already trusted or none have scripts."
                .to_owned()
        } else {
            let mut m =
                "0 scripts ran. The following packages are already trusted, don't have scripts to run, or don't exist:\n"
                    .to_owned();
            for n in names {
                m.push_str(&format!("\n - {n}"));
            }
            m
        };
        for line in alias_notes(&plan.aliased) {
            message.push('\n');
            message.push_str(&line);
        }
        for n in &plan.undeclared {
            message.push_str(&format!("\nnote: \"{n}\" {CANNOT_TRUST}"));
        }
        return Err(Error::Message(message));
    }
    Ok(plan)
}

/// bun `print_aliased_args`.
pub fn alias_notes(aliased: &[(String, String)]) -> Vec<String> {
    aliased
        .iter()
        .map(|(alias, name)| format!("note: \"{alias}\" is an alias of \"{name}\", run 'fnpm pm trust {name}' to trust it"))
        .collect()
}

/// The jobs to run, as levels of equal depth, deepest first (for [`crate::run_jobs`]).
pub fn trust_jobs(graph: &LockGraph, list: &[Untrusted], plan: &TrustPlan) -> Vec<Vec<Job>> {
    let mut levels: Vec<Vec<Job>> = Vec::new();
    let mut last_depth: Option<usize> = None;
    for &i in &plan.order {
        if plan.actions[i] != Action::Run {
            continue;
        }
        let u = &list[i];
        if last_depth != Some(u.depth) {
            levels.push(Vec::new());
            last_depth = Some(u.depth);
        }
        let version = graph
            .get(u.package)
            .map(|p| p.version.clone())
            .unwrap_or_default();
        if let Some(level) = levels.last_mut() {
            level.push(Job {
                package: u.package,
                name: u.alias.clone(),
                version,
                dir: u.dir.clone(),
                package_json: u.dir.join("package.json"),
                scripts: u.scripts.clone(),
                optional: false,
                foreground: false,
            });
        }
    }
    levels
}

/// `--dry-run` output.
pub fn render_dry_run(list: &[Untrusted], plan: &TrustPlan) -> Vec<String> {
    let mut lines = vec![String::new()];
    let (mut scripts, mut packages) = (0usize, 0usize);
    for &i in &plan.order {
        if plan.actions[i] != Action::Run {
            continue;
        }
        packages += 1;
        scripts += list[i].scripts.len();
        lines.extend(render_entry(&list[i], PrintFormat::Untrusted));
    }
    let s = |n: usize| if n != 1 { "s" } else { "" };
    lines.push(format!(
        "dry run: would run {scripts} script{} across {packages} package{}",
        s(scripts),
        s(packages)
    ));
    lines.push("dry run: would add to trustedDependencies:".into());
    lines.extend(
        plan.names_to_add
            .iter()
            .map(|n| format!(" - {}", escape_control(n))),
    );
    lines
}

/// The output after the scripts ran.
pub fn render_trust_result(
    list: &[Untrusted],
    plan: &TrustPlan,
    elapsed: std::time::Duration,
) -> Vec<String> {
    let mut lines = vec![String::new()];
    let (mut ran, mut packages, mut skipped, mut deferred) = (0usize, 0usize, 0usize, 0usize);
    for &i in &plan.order {
        match plan.actions[i] {
            Action::Skip => skipped += 1,
            Action::Defer => deferred += 1,
            Action::Run => {
                packages += 1;
                ran += list[i].scripts.len();
            }
        }
        let format = if plan.actions[i] == Action::Run {
            PrintFormat::Completed
        } else {
            PrintFormat::Untrusted
        };
        lines.extend(render_entry(&list[i], format));
    }
    let s = |n: usize| if n > 1 { "s" } else { "" };
    if ran > 0 {
        lines.push(format!(
            " {ran} script{} ran across {packages} package{} [{:.2}ms]",
            s(ran),
            s(packages),
            elapsed.as_secs_f64() * 1000.0
        ));
    }
    if deferred > 0 {
        if ran > 0 {
            lines.push(String::new());
        }
        lines.push(format!(
            " {deferred} trusted package{} linked from the global store: run `fnpm install` to run the scripts inside this project",
            s(deferred)
        ));
    }
    if skipped > 0 {
        lines.push(String::new());
        lines.push(format!(
            " {skipped} package{} with blocked scripts",
            s(skipped)
        ));
    }
    lines.extend(alias_notes(&plan.aliased));
    lines
}

/// bun `PackageJSONEditor::edit_trusted_dependencies`: adds the missing `names` to the root
/// package.json's `trustedDependencies` (created at the end when absent) and sorts the array
/// when something was added. Returns whether the document changed.
pub fn add_trusted_dependencies(package_json: &mut serde_json::Value, names: &[String]) -> bool {
    let Some(obj) = package_json.as_object_mut() else {
        return false;
    };
    let existing: Vec<String> = obj
        .get("trustedDependencies")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut to_add: Vec<&String> = names.iter().filter(|n| !existing.contains(n)).collect();
    to_add.dedup();
    let is_array = obj.get("trustedDependencies").is_some_and(|v| v.is_array());
    if to_add.is_empty() {
        if !is_array {
            obj.insert(
                "trustedDependencies".into(),
                serde_json::Value::Array(Vec::new()),
            );
            return true;
        }
        return false;
    }
    let mut items: Vec<serde_json::Value> = obj
        .get("trustedDependencies")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    items.extend(
        to_add
            .into_iter()
            .rev()
            .map(|n| serde_json::Value::String(n.clone())),
    );
    // bun `alphabetize_strings`: strings sorted, other items stay after them.
    items.sort_by(|a, b| match (a.as_str(), b.as_str()) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    if is_array {
        obj["trustedDependencies"] = serde_json::Value::Array(items);
    } else {
        obj.shift_remove("trustedDependencies");
        obj.insert(
            "trustedDependencies".into(),
            serde_json::Value::Array(items),
        );
    }
    true
}
