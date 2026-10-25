//! `fnpm audit fix [--latest] [--dry-run] [--json]` (bun `install/audit_fix.rs`).
//!
//! For each installed registry package that an advisory matches, the plan picks the lowest
//! published release above it that no advisory matches (else the highest safe release below it
//! on the same line, a downgrade) that every dependent's range accepts. A dependency the root
//! or a workspace pins exactly is treated as `^<pin>`, and the pin is rewritten in package.json;
//! with `--latest` any range of the project's own is rewritten. Fixes are applied by an install
//! over the lockfile with the vulnerable packages replaced, then the result is audited again.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use fnpm_registry::{AuditPackage, ManifestRequest, PackageManifest};
use fnpm_semver::{Range, Version};
use fnpm_types::{Behavior, DepEdge, LockGraph, Package, PackageId, Resolution};
use serde_json::{Value, json};

use super::Ctx;
use crate::options::{GraphPatch, InstallCommand, InstallOptions};
use crate::{CoreResult, Error, Result};

/// One advisory of the bulk response.
#[derive(Debug, Clone)]
struct Advisory {
    name: String,
    range: Option<Range>,
    range_text: String,
    /// The GHSA id from the URL, else the numeric id (what `--ignore` takes).
    token: String,
}

impl Advisory {
    fn matches(&self, v: &Version) -> bool {
        self.range.as_ref().is_some_and(|r| r.satisfies(v))
    }
}

fn advisories_of(resp: &Value, ignore: &[String]) -> Vec<Advisory> {
    let mut out = Vec::new();
    let Some(map) = resp.as_object() else {
        return out;
    };
    for (name, list) in map {
        for a in list.as_array().into_iter().flatten() {
            let id = a
                .get("id")
                .map(|v| v.to_string().trim_matches('"').to_owned())
                .unwrap_or_default();
            let url = a.get("url").and_then(Value::as_str).unwrap_or_default();
            if ignore
                .iter()
                .any(|i| !i.is_empty() && (*i == id || url.contains(i.as_str())))
            {
                continue;
            }
            let ghsa = url
                .rsplit('/')
                .next()
                .filter(|s| s.starts_with("GHSA-"))
                .map(str::to_owned);
            let range_text = a
                .get("vulnerable_versions")
                .and_then(Value::as_str)
                .unwrap_or("*")
                .to_owned();
            out.push(Advisory {
                name: name.clone(),
                range: Range::parse(&range_text).ok(),
                range_text,
                token: ghsa.unwrap_or(id),
            });
        }
    }
    out
}

/// An edge of the graph that reaches a vulnerable package.
#[derive(Debug, Clone)]
struct Edge {
    parent: PackageId,
    index: usize,
    range: Option<Range>,
    literal: String,
    dependent: String,
    peer: bool,
    /// The project's own exact pin (or, with `--latest`, any range of its own).
    pin: bool,
    /// The pin would move with `--latest` only.
    latest_fixes: bool,
}

struct Instance {
    name: String,
    from: String,
    current: Version,
    advisories: Vec<usize>,
    edges: Vec<Edge>,
}

#[derive(Debug, Clone)]
struct PackageJsonEdit {
    workspace: String,
    file: String,
    key: String,
    from: String,
    to: String,
}

#[derive(Debug, Clone)]
struct Fix {
    name: String,
    from: String,
    to: String,
    downgrade: bool,
    edges: Vec<(PackageId, usize)>,
    edits: Vec<PackageJsonEdit>,
}

#[derive(Debug, Clone)]
struct Blocker {
    dependent: String,
    range: String,
    peer: bool,
    latest_fixes: bool,
}

#[derive(Debug, Clone)]
struct Blocked {
    name: String,
    from: String,
    needs: String,
    downgrade: bool,
    blockers: Vec<Blocker>,
}

#[derive(Debug, Clone)]
struct Unfixable {
    name: String,
    from: String,
    tokens: Vec<String>,
}

#[derive(Default)]
struct Plan {
    fixes: Vec<Fix>,
    blocked: Vec<Blocked>,
    unfixable: Vec<Unfixable>,
    unavailable: Vec<(String, String, String)>,
    unmatched: Vec<(String, String)>,
    fixed: usize,
    remaining: usize,
    checked: usize,
    /// The new versions, for the install.
    manifests: HashMap<String, Arc<PackageManifest>>,
}

/// The dependent as bun names it: the package.json of an importer, `name@version` otherwise.
fn label(graph: &LockGraph, id: PackageId) -> String {
    let p = &graph.packages[id.index()];
    match &p.resolution {
        Resolution::Root => "package.json".into(),
        Resolution::Workspace { path } => format!("{path}/package.json"),
        _ => format!("{}@{}", p.name, p.version),
    }
}

fn importer_path(graph: &LockGraph, id: PackageId) -> Option<String> {
    if id == PackageId::ROOT {
        return Some(String::new());
    }
    match &graph.packages[id.index()].resolution {
        Resolution::Workspace { path } => Some(path.clone()),
        _ => None,
    }
}

/// The npm range of `literal` (`npm:` aliases unwrapped).
fn npm_range(literal: &str) -> Option<Range> {
    let range = crate::spec::split_npm_alias(literal).map_or(literal, |(_, r)| r);
    if range.contains(':') || range.contains('/') {
        return None;
    }
    Range::parse(range).ok()
}

/// The literal a pin is rewritten to: the same style (`=`, `~`, `^`, exact) with the new
/// version.
fn new_literal(old: &str, to: &str, exact: bool) -> String {
    crate::spec::updated_literal(old, to, exact, true).unwrap_or_else(|| to.to_owned())
}

async fn plan(
    cx: &Ctx,
    graph: &LockGraph,
    advisories: &[Advisory],
    client: &fnpm_registry::RegistryClient,
    latest: bool,
    exact: bool,
) -> Plan {
    let mut plan = Plan::default();
    let mut checked = std::collections::HashSet::new();
    let mut instances: Vec<Instance> = Vec::new();
    let mut instance_of: HashMap<PackageId, usize> = HashMap::new();
    let mut matched = vec![false; advisories.len()];
    for id in graph.ids() {
        let p = &graph.packages[id.index()];
        let Resolution::Npm { version, .. } = &p.resolution else {
            continue;
        };
        checked.insert(p.name.clone());
        let Ok(current) = Version::parse(version) else {
            continue;
        };
        let hits: Vec<usize> = advisories
            .iter()
            .enumerate()
            .filter(|(_, a)| a.name == p.name && a.matches(&current))
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() {
            continue;
        }
        for &i in &hits {
            matched[i] = true;
        }
        instance_of.insert(id, instances.len());
        instances.push(Instance {
            name: p.name.clone(),
            from: version.clone(),
            current,
            advisories: hits,
            edges: Vec::new(),
        });
    }
    plan.checked = checked.len();
    for parent in graph.ids() {
        let owner = &graph.packages[parent.index()];
        let own = importer_path(graph, parent).is_some();
        for (index, e) in owner.dependencies.iter().enumerate() {
            let Some(&inst) = e.target.and_then(|t| instance_of.get(&t)) else {
                continue;
            };
            if e.behavior.is_peer() && e.behavior.contains(Behavior::OPTIONAL) {
                continue;
            }
            let range = npm_range(&e.literal);
            let is_exact = range.as_ref().is_some_and(|r| r.as_exact().is_some());
            let pin_latest = own && range.is_some();
            let pin = own && (is_exact || (latest && pin_latest));
            instances[inst].edges.push(Edge {
                parent,
                index,
                range,
                literal: e.literal.clone(),
                dependent: label(graph, parent),
                peer: e.behavior.is_peer(),
                pin,
                latest_fixes: !latest && pin_latest && !is_exact,
            });
        }
    }
    let mut unmatched: Vec<(String, String)> = advisories
        .iter()
        .zip(&matched)
        .filter(|(_, m)| !**m)
        .map(|(a, _)| (a.name.clone(), a.range_text.clone()))
        .collect();
    unmatched.sort();
    unmatched.dedup();
    plan.unmatched = unmatched;
    let mut still = vec![false; advisories.len()];
    for (i, m) in matched.iter().enumerate() {
        still[i] = !*m;
    }

    for inst in instances {
        let manifest = match client
            .manifest(
                &inst.name,
                ManifestRequest {
                    full: false,
                    revalidate: true,
                },
            )
            .await
        {
            Ok(m) => m,
            Err(e) => {
                for &a in &inst.advisories {
                    still[a] = true;
                }
                plan.unavailable
                    .push((inst.name.clone(), inst.from.clone(), e.to_string()));
                continue;
            }
        };
        let safe = |v: &Version| {
            !advisories
                .iter()
                .any(|a| a.name == inst.name && a.matches(v))
        };
        // Upgrades ascending, then downgrades on the same release line, descending.
        let releases: Vec<&Version> = manifest
            .versions
            .iter()
            .map(|vm| &vm.version)
            .filter(|v| !v.is_prerelease())
            .collect();
        let mut candidates: Vec<(Version, bool)> = releases
            .iter()
            .filter(|v| ***v > inst.current && safe(v))
            .map(|v| ((*v).clone(), false))
            .collect();
        let upgrades = candidates.len();
        for v in releases.iter().rev() {
            if **v >= inst.current {
                continue;
            }
            let cur = &inst.current;
            if v.major != cur.major || (v.major == 0 && v.minor != cur.minor) {
                break;
            }
            if safe(v) {
                candidates.push(((*v).clone(), true));
            }
        }
        if candidates.is_empty() {
            for &a in &inst.advisories {
                still[a] = true;
            }
            let mut tokens: Vec<String> = inst
                .advisories
                .iter()
                .map(|&a| advisories[a].token.clone())
                .collect();
            tokens.sort();
            tokens.dedup();
            plan.unfixable.push(Unfixable {
                name: inst.name.clone(),
                from: inst.from.clone(),
                tokens,
            });
            continue;
        }
        let caret = Range::parse(&format!("^{}", inst.from)).ok();
        let range_accepts =
            |e: &Edge, v: &Version| e.range.as_ref().is_some_and(|r| r.satisfies(v));
        let pin_accepts = |e: &Edge, v: &Version| {
            e.pin && (latest || caret.as_ref().is_some_and(|c| c.satisfies(v)))
        };
        // (candidate, edit)
        let find = |e: &Edge, span: std::ops::Range<usize>| -> Option<(usize, bool)> {
            span.clone()
                .find(|&c| range_accepts(e, &candidates[c].0))
                .map(|c| (c, false))
                .or_else(|| {
                    span.clone()
                        .find(|&c| pin_accepts(e, &candidates[c].0))
                        .map(|c| (c, true))
                })
        };
        let mut target: Vec<Option<(usize, bool)>> = inst
            .edges
            .iter()
            .map(|e| find(e, 0..upgrades).or_else(|| find(e, upgrades..candidates.len())))
            .collect();
        // One version for every edge that upgrades, if one fits them all.
        let ups: Vec<usize> = target
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.filter(|t| t.0 < upgrades).map(|_| i))
            .collect();
        if !ups.is_empty()
            && let Some(common) = (0..upgrades).find(|&c| {
                ups.iter().all(|&i| {
                    let (_, edit) = target[i].unwrap_or((0, false));
                    if edit {
                        pin_accepts(&inst.edges[i], &candidates[c].0)
                    } else {
                        range_accepts(&inst.edges[i], &candidates[c].0)
                    }
                })
            })
        {
            for &i in &ups {
                if let Some(t) = &mut target[i] {
                    t.0 = common;
                }
            }
        }
        let root_target = inst
            .edges
            .iter()
            .zip(&target)
            .find_map(|(e, t)| (e.parent == PackageId::ROOT && !e.peer).then_some(*t));
        let mut chosen: Vec<usize> = if inst.edges.is_empty() {
            vec![0]
        } else {
            inst.edges
                .iter()
                .zip(&target)
                .filter(|(e, _)| !(e.peer && matches!(root_target, Some(None))))
                .filter_map(|(_, t)| t.map(|t| t.0))
                .collect()
        };
        chosen.sort_unstable();
        chosen.dedup();
        for c in chosen {
            let (version, downgrade) = &candidates[c];
            let to = version.to_string();
            let mut edges = Vec::new();
            let mut edits: Vec<PackageJsonEdit> = Vec::new();
            for (e, t) in inst.edges.iter().zip(&target) {
                let Some((tc, edit)) = t else { continue };
                if *tc != c {
                    continue;
                }
                edges.push((e.parent, e.index));
                if *edit && let Some(ws) = importer_path(graph, e.parent) {
                    let name = graph.packages[e.parent.index()].dependencies[e.index]
                        .name
                        .clone();
                    if !edits.iter().any(|x| x.workspace == ws && x.key == name) {
                        edits.push(PackageJsonEdit {
                            file: if ws.is_empty() {
                                "package.json".into()
                            } else {
                                format!("{ws}/package.json")
                            },
                            workspace: ws,
                            key: name,
                            from: e.literal.clone(),
                            to: new_literal(&e.literal, &to, exact),
                        });
                    }
                }
            }
            plan.fixes.push(Fix {
                name: inst.name.clone(),
                from: inst.from.clone(),
                to,
                downgrade: *downgrade,
                edges,
                edits,
            });
        }
        let mut blockers: Vec<Blocker> = Vec::new();
        for (e, _) in inst.edges.iter().zip(&target).filter(|(_, t)| t.is_none()) {
            let b = Blocker {
                dependent: e.dependent.clone(),
                range: e.literal.clone(),
                peer: e.peer && !e.latest_fixes,
                latest_fixes: e.latest_fixes,
            };
            if !blockers
                .iter()
                .any(|x| x.dependent == b.dependent && x.range == b.range)
            {
                blockers.push(b);
            }
        }
        plan.manifests.insert(inst.name.clone(), manifest.clone());
        if blockers.is_empty() {
            continue;
        }
        if !(matches!(root_target, Some(Some(_))) && blockers.iter().all(|b| b.peer)) {
            for &a in &inst.advisories {
                still[a] = true;
            }
        }
        plan.blocked.push(Blocked {
            name: inst.name.clone(),
            from: inst.from.clone(),
            needs: candidates[0].0.to_string(),
            downgrade: candidates[0].1,
            blockers,
        });
    }
    plan.fixes
        .sort_by(|a, b| (&a.name, &a.from, &a.to).cmp(&(&b.name, &b.from, &b.to)));
    plan.blocked
        .sort_by(|a, b| (&a.name, &a.from).cmp(&(&b.name, &b.from)));
    plan.unfixable
        .sort_by(|a, b| (&a.name, &a.from).cmp(&(&b.name, &b.from)));
    for (name, from, error) in &plan.unavailable {
        cx.events.emit(fnpm_types::Event::warn(
            format!("{name}@{from} was not checked for updates: {error}"),
            None,
        ));
    }
    plan.remaining = still.iter().filter(|s| **s).count();
    plan.fixed = advisories.len() - plan.remaining;
    plan
}

const ARROW: &str = "->";

fn vuln_word(n: usize) -> &'static str {
    if n == 1 {
        "vulnerability"
    } else {
        "vulnerabilities"
    }
}

fn sections(plan: &Plan) -> Vec<String> {
    let esc = |s: &str| crate::security::escape_control(s, false);
    let mut lines = Vec::new();
    if !plan.fixes.is_empty() {
        lines.push("fixing:".into());
        for f in &plan.fixes {
            let glyph = if f.downgrade { "v" } else { "^" };
            let mut line = format!(
                "  {glyph} {} {} {ARROW} {}",
                esc(&f.name),
                esc(&f.from),
                esc(&f.to)
            );
            if f.downgrade {
                line.push_str(" (downgrade)");
            }
            lines.push(line);
            for e in &f.edits {
                lines.push(format!(
                    "    {}: {} {ARROW} {}",
                    esc(&e.file),
                    esc(&e.from),
                    esc(&e.to)
                ));
            }
        }
        lines.push(String::new());
    }
    if !plan.blocked.is_empty() {
        lines.push("blocked by a dependent's range:".into());
        for b in &plan.blocked {
            let glyph = if b.downgrade { "v" } else { "^" };
            let mut line = format!(
                "  {glyph} {} {} {ARROW} {}",
                esc(&b.name),
                esc(&b.from),
                esc(&b.needs)
            );
            if b.downgrade {
                line.push_str(" (downgrade)");
            }
            lines.push(line);
            for x in &b.blockers {
                let mut line = format!(
                    "    {} depends on {}@{}",
                    esc(&x.dependent),
                    esc(&b.name),
                    esc(&x.range)
                );
                if x.peer {
                    line.push_str(" (peer)");
                }
                lines.push(line);
            }
            if b.blockers.iter().any(|x| x.latest_fixes) {
                lines.push("    fnpm audit fix --latest".into());
            }
        }
        lines.push(String::new());
    }
    if !plan.unfixable.is_empty() {
        lines.push("no published version fixes:".into());
        let mut all: Vec<String> = Vec::new();
        for u in &plan.unfixable {
            lines.push(format!(
                "  {}@{}  {}",
                esc(&u.name),
                esc(&u.from),
                u.tokens.join(", ")
            ));
            for t in &u.tokens {
                if !all.contains(t) {
                    all.push(t.clone());
                }
            }
        }
        let mut cmd = "    fnpm audit fix".to_owned();
        for t in &all {
            cmd.push_str(&format!(" --ignore {}", esc(t)));
        }
        lines.push(cmd);
        lines.push(String::new());
    }
    if !plan.unmatched.is_empty() {
        lines.push("not matched to an installed version:".into());
        for (name, range) in &plan.unmatched {
            lines.push(format!("  {}@{}", esc(name), esc(range)));
        }
        lines.push(String::new());
    }
    lines
}

fn summary(
    plan: &Plan,
    dry_run: bool,
    fixed: usize,
    remaining: usize,
    start: Instant,
) -> Vec<String> {
    let mut line = if plan.fixes.is_empty() {
        let total = fixed + remaining;
        format!("Fixed 0 of {total} {}", vuln_word(total))
    } else {
        let mut names: Vec<&str> = plan.fixes.iter().map(|f| f.name.as_str()).collect();
        names.dedup();
        let packages = names.len();
        let verb = if dry_run { "Would fix" } else { "Fixed" };
        format!(
            "{verb} {fixed} {} in {packages} {}",
            vuln_word(fixed),
            if packages == 1 { "package" } else { "packages" }
        )
    };
    line.push_str(&format!(" (checked {})", plan.checked));
    line.push_str(&format!(
        " [{:.2}ms]",
        start.elapsed().as_secs_f64() * 1000.0
    ));
    let mut lines = vec![line];
    if remaining > 0 {
        lines.push(format!("{remaining} {} remaining", vuln_word(remaining)));
    }
    lines
}

fn plan_json(plan: &Plan, dry_run: bool, fixed: usize, remaining: usize, after: &[Value]) -> Value {
    json!({
        "dryRun": dry_run,
        "fixed": fixed,
        "remaining": remaining,
        "fixes": plan.fixes.iter().map(|f| json!({
            "name": f.name, "from": f.from, "to": f.to, "downgrade": f.downgrade,
            "newerThanMinimumReleaseAge": false,
            "packageJson": f.edits.iter().map(|e| json!({
                "file": e.file, "catalog": null, "override": null, "key": e.key, "from": e.from, "to": e.to,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "blocked": plan.blocked.iter().map(|b| json!({
            "name": b.name, "from": b.from, "to": b.needs, "downgrade": b.downgrade,
            "latestFixes": b.blockers.iter().any(|x| x.latest_fixes),
            "blockers": b.blockers.iter().map(|x| json!({
                "dependent": x.dependent, "range": x.range, "bundled": false, "override": null,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "unfixable": plan.unfixable.iter().map(|u| json!({
            "name": u.name, "from": u.from, "advisories": u.tokens,
        })).collect::<Vec<_>>(),
        "manifestUnavailable": plan.unavailable.iter().map(|(n, f, e)| json!({
            "name": n, "from": f, "error": e,
        })).collect::<Vec<_>>(),
        "unmatched": plan.unmatched.iter().map(|(n, r)| json!({ "name": n, "range": r })).collect::<Vec<_>>(),
        "unaudited": [],
        "vulnerableAfterInstall": after,
    })
}

/// The registry package that replaces a vulnerable one, with its edges unresolved.
fn package_for(
    name: &str,
    manifest: &PackageManifest,
    version: &str,
    registry: &str,
) -> Option<Package> {
    let vm = manifest
        .versions
        .iter()
        .find(|vm| vm.version.to_string() == version)?;
    let unscoped = name.rsplit('/').next().unwrap_or(name);
    let default = format!(
        "{}/{name}/-/{unscoped}-{version}.tgz",
        registry.trim_end_matches('/')
    );
    let tarball_url = (!vm.dist.tarball.is_empty() && vm.dist.tarball != default)
        .then(|| vm.dist.tarball.clone());
    let mut p = Package::new(
        name,
        version,
        Resolution::Npm {
            version: version.to_owned(),
            tarball_url,
            integrity: vm.dist.integrity,
        },
    );
    let mut edges: Vec<DepEdge> = Vec::new();
    for (n, l) in &vm.dependencies {
        if !vm.optional_dependencies.iter().any(|(o, _)| o == n) {
            edges.push(DepEdge::new(n, l, Behavior::PROD));
        }
    }
    for (n, l) in &vm.optional_dependencies {
        edges.push(DepEdge::new(n, l, Behavior::OPTIONAL));
    }
    for (n, l) in &vm.peer_dependencies {
        if edges.iter().any(|e| &e.name == n) {
            continue;
        }
        let optional = vm.peer_dependencies_meta.get(n).is_some_and(|m| m.optional);
        let b = if optional {
            Behavior::PEER | Behavior::OPTIONAL
        } else {
            Behavior::PEER
        };
        edges.push(DepEdge::new(n, l, b));
    }
    edges.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.behavior.group().cmp(&b.behavior.group()))
    });
    p.dependencies = edges;
    p.bin = vm.bin.clone();
    p.platform = vm.platform;
    p.meta.has_install_script = vm.has_install_script;
    p.meta.engines = vm.engines.clone();
    p.meta.license = vm.license.clone();
    p.peer_meta = vm.peer_dependencies_meta.clone();
    Some(p)
}

/// `fnpm audit fix`.
pub(crate) async fn fix(cx: &Ctx) -> Result<CoreResult> {
    let start = Instant::now();
    if cx.req.positionals.len() > 1 {
        return Err(Error::usage(format!(
            "unexpected argument \"{}\" for `fnpm audit fix`",
            cx.req.positionals[1]
        )));
    }
    let dry_run = cx.flag("dryRun");
    let latest = cx.flag("latest");
    let exact = cx.flag("exact");
    let json_out = cx.req.json;
    if !dry_run {
        let refuse = if cx.flag("frozenLockfile") {
            Some("--frozen-lockfile")
        } else if cx.flag("production") {
            Some("--production")
        } else if cx.req.options.get("save") == Some(&Value::Bool(false)) {
            Some("--no-save")
        } else {
            None
        };
        if let Some(flag) = refuse {
            eprintln!("error: `fnpm audit fix` writes the lockfile, so it cannot run with {flag}");
            eprintln!("note: pass --dry-run to see the fixes without applying them");
            return Err(Error::Reported);
        }
    }
    let (project, _, graph) = cx.load_locked("nothing to fix")?;
    let packages = audit_packages(&graph);
    let client = project.client()?;
    let resp = client.audit(&packages).await?;
    let ignore = cx.strings("ignore");
    let advisories = advisories_of(&resp.advisories, &ignore);
    let mut plan = plan(cx, &graph, &advisories, &client, latest, exact).await;
    plan.checked = plan
        .checked
        .saturating_sub(resp.skipped)
        .max(resp.checked.min(plan.checked));

    let mut lines = vec![
        format!("fnpm audit fix v{}", cli_version(cx)),
        String::new(),
    ];
    if advisories.is_empty() && !json_out {
        let n = plan.checked;
        lines.push(format!(
            "No vulnerabilities found (checked {n} package{}) [{:.2}ms]",
            if n == 1 { "" } else { "s" },
            start.elapsed().as_secs_f64() * 1000.0
        ));
        return Ok(super::ok(json!({}), lines));
    }
    if !json_out {
        lines.extend(sections(&plan));
    }
    if dry_run || plan.fixes.is_empty() {
        let (fixed, remaining) = (plan.fixed, plan.remaining);
        if json_out {
            let data = plan_json(&plan, dry_run, fixed, remaining, &[]);
            return Ok(super::exit(i32::from(remaining > 0), data, Vec::new()));
        }
        lines.extend(summary(&plan, dry_run, fixed, remaining, start));
        return Ok(super::exit(i32::from(remaining > 0), json!({}), lines));
    }

    // Apply: the vulnerable packages are replaced in the lockfile graph, the pins rewritten in
    // package.json, and an install resolves what the new versions depend on.
    let registry = project.config.registry.url.clone();
    let replacements: Vec<(Fix, Package)> = plan
        .fixes
        .iter()
        .filter_map(|f| {
            let m = plan.manifests.get(&f.name)?;
            let reg = project.config.registry_for(&f.name).url.clone();
            Some((f.clone(), package_for(&f.name, m, &f.to, &reg)?))
        })
        .collect();
    let _ = registry;
    let edits: Vec<(String, String, String)> = plan
        .fixes
        .iter()
        .flat_map(|f| {
            f.edits
                .iter()
                .map(|e| (e.workspace.clone(), e.key.clone(), e.to.clone()))
        })
        .collect();
    let patch = GraphPatch(Arc::new(move |g: &mut LockGraph| {
        for (f, p) in &replacements {
            let new = PackageId(g.packages.len() as u32);
            g.packages.push(p.clone());
            for &(parent, index) in &f.edges {
                if let Some(e) = g
                    .packages
                    .get_mut(parent.index())
                    .and_then(|o| o.dependencies.get_mut(index))
                {
                    e.target = Some(new);
                }
            }
        }
        for (ws, key, to) in &edits {
            let Some(&owner) = g.workspaces.get(ws) else {
                continue;
            };
            for e in &mut g.packages[owner.index()].dependencies {
                if &e.name == key {
                    e.literal = to.clone();
                }
            }
        }
    }));
    let pj_edits: BTreeMap<String, Vec<(String, String)>> = plan
        .fixes
        .iter()
        .flat_map(|f| f.edits.iter())
        .fold(BTreeMap::new(), |mut m, e| {
            m.entry(e.workspace.clone())
                .or_default()
                .push((e.key.clone(), e.to.clone()));
            m
        });
    crate::install::apply_package_json_updates(&project.root_dir, &pj_edits)?;

    // The security scanner, if any, sees the new versions before anything is linked.
    let opts = InstallOptions {
        pm: crate::options::PmOptions {
            cwd: project.root_dir.clone(),
            summary: false,
            progress: false,
            ..cx.pm.clone()
        },
        command: InstallCommand::Install,
        patch: Some(patch),
        ..InstallOptions::default()
    };
    crate::install_with_env(opts, cx.events.clone(), cx.env.clone()).await?;
    eprintln!("Saved lockfile");

    // Audit what is installed now.
    let (_, _, after_graph) = cx.load_locked("nothing to fix")?;
    let after_resp = client.audit(&audit_packages(&after_graph)).await?;
    let after = advisories_of(&after_resp.advisories, &ignore);
    let mut still: Vec<(String, String, Vec<String>)> = Vec::new();
    let staying: Vec<(&str, &str)> = plan
        .blocked
        .iter()
        .map(|b| (b.name.as_str(), b.from.as_str()))
        .chain(
            plan.unfixable
                .iter()
                .map(|u| (u.name.as_str(), u.from.as_str())),
        )
        .collect();
    let mut remaining_set = std::collections::HashSet::new();
    for p in &after_graph.packages {
        let Resolution::Npm { version, .. } = &p.resolution else {
            continue;
        };
        let Ok(v) = Version::parse(version) else {
            continue;
        };
        let mut tokens: Vec<String> = after
            .iter()
            .enumerate()
            .filter(|(_, a)| a.name == p.name && a.matches(&v))
            .map(|(i, a)| {
                remaining_set.insert(i);
                a.token.clone()
            })
            .collect();
        if tokens.is_empty() {
            continue;
        }
        tokens.sort();
        tokens.dedup();
        if !staying.contains(&(p.name.as_str(), version.as_str())) {
            still.push((p.name.clone(), version.clone(), tokens));
        }
    }
    still.sort();
    still.dedup();
    let remaining = remaining_set.len();
    let fixed = advisories.len().saturating_sub(remaining);
    if json_out {
        let after_json: Vec<Value> = still
            .iter()
            .map(|(n, v, t)| json!({ "name": n, "version": v, "advisories": t }))
            .collect();
        let data = plan_json(&plan, false, fixed, remaining, &after_json);
        return Ok(super::exit(i32::from(remaining > 0), data, Vec::new()));
    }
    if !still.is_empty() {
        lines.push("vulnerable after install:".into());
        for (n, v, t) in &still {
            lines.push(format!("  {n}@{v}  {}", t.join(", ")));
        }
        lines.push(String::new());
    }
    lines.extend(summary(&plan, false, fixed, remaining, start));
    Ok(super::exit(i32::from(remaining > 0), json!({}), lines))
}

fn cli_version(cx: &Ctx) -> String {
    cx.string("cliVersion")
        .unwrap_or_else(|| crate::version().to_owned())
}

/// Every registry package of the graph, by name.
pub(crate) fn audit_packages(graph: &LockGraph) -> Vec<AuditPackage> {
    let mut versions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in graph.packages.iter().skip(1) {
        if let Resolution::Npm { version, .. } = &p.resolution {
            let v = versions.entry(p.name.clone()).or_default();
            if !v.contains(version) {
                v.push(version.clone());
            }
        }
    }
    versions
        .into_iter()
        .map(|(name, versions)| AuditPackage { name, versions })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_keep_their_style() {
        assert_eq!(new_literal("1.0.2", "1.0.4", false), "1.0.4");
        assert_eq!(new_literal("^1.0.2", "1.1.0", false), "^1.1.0");
        assert_eq!(new_literal("~1.0.2", "1.0.4", false), "~1.0.4");
    }

    #[test]
    fn advisory_tokens() {
        let resp = json!({ "a": [
            { "id": 1, "url": "https://github.com/advisories/GHSA-aaaa-bbbb-cccc", "vulnerable_versions": "<1.0.4" },
            { "id": 2, "url": "https://example.com/x", "vulnerable_versions": ">=2" },
        ]});
        let all = advisories_of(&resp, &[]);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].token, "GHSA-aaaa-bbbb-cccc");
        assert_eq!(all[1].token, "2");
        assert!(all[0].matches(&Version::parse("1.0.2").unwrap()));
        assert!(!all[0].matches(&Version::parse("1.0.4").unwrap()));
        assert_eq!(
            advisories_of(&resp, &["GHSA-aaaa-bbbb-cccc".into()]).len(),
            1
        );
        assert_eq!(advisories_of(&resp, &["2".into()]).len(), 1);
    }
}
