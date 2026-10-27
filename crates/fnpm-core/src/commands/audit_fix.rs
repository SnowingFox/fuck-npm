//! `fnpm audit fix [--latest] [--dry-run] [--json]` (bun `install/audit_fix.rs`).
//!
//! For each installed registry package that an advisory matches, the plan picks the lowest
//! published release above it that no advisory matches (else the highest safe release below it
//! on the same line, a downgrade) that every dependent's range accepts. A range is read the way
//! the resolver reads it: after `overrides` and catalogs. A version the project pins exactly
//! (in a dependency group, a catalog or an `overrides` rule) is treated as `^<pin>`, and the pin
//! is rewritten in package.json; with `--latest` any range of the project's own is rewritten.
//! Fixes are applied by an install over the lockfile with the vulnerable packages replaced, then
//! the saved lockfile is audited again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use fnpm_registry::{AuditPackage, ManifestRequest, PackageManifest};
use fnpm_semver::{Range, SatisfyOptions, Version};
use fnpm_types::{
    Behavior, DepEdge, LockGraph, OverrideRule, OverrideSelector, Package, PackageId,
    PackageSelector, Resolution,
};
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
        self.range.as_ref().is_some_and(|r| {
            r.satisfies_with(
                v,
                SatisfyOptions {
                    include_prerelease: true,
                },
            )
        })
    }
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "critical" => 4,
        "high" => 3,
        "moderate" => 2,
        "low" => 1,
        // bun counts and filters an unknown severity as moderate.
        _ => 2,
    }
}

/// bun's query parser finds no version in a range like `not a range`: it matches nothing.
fn advisory_range(text: &str) -> Option<Range> {
    let t = text.trim();
    let wildcard = !t.is_empty() && t.bytes().all(|b| matches!(b, b'*' | b'x' | b'X' | b' '));
    if !t.is_empty() && !wildcard && !t.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    Range::parse_including_prerelease(if t.is_empty() { "*" } else { t }).ok()
}

/// What `--audit-level` and `--ignore` left out.
#[derive(Default)]
struct Filtered {
    below_level: usize,
    ignored: usize,
}

fn advisories_of(
    resp: &Value,
    ignore: &[String],
    level: Option<&str>,
    filtered: &mut Filtered,
) -> Vec<Advisory> {
    let mut out = Vec::new();
    let Some(map) = resp.as_object() else {
        return out;
    };
    let level_rank = level.map_or(0, severity_rank);
    for (name, list) in map {
        for a in list.as_array().into_iter().flatten() {
            let id = a
                .get("id")
                .map(|v| v.to_string().trim_matches('"').to_owned())
                .unwrap_or_default();
            let url = a.get("url").and_then(Value::as_str).unwrap_or_default();
            let severity = a
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("moderate");
            if severity_rank(severity) < level_rank {
                filtered.below_level += 1;
                continue;
            }
            if ignore
                .iter()
                .any(|i| !i.is_empty() && (*i == id || url.contains(i.as_str())))
            {
                filtered.ignored += 1;
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
                .filter(|s| !s.is_empty())
                .unwrap_or("*")
                .to_owned();
            out.push(Advisory {
                name: name.clone(),
                range: advisory_range(&range_text),
                range_text,
                token: ghsa.unwrap_or(id),
            });
        }
    }
    out
}

/// Where a pin is written in a package.json.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Site {
    /// `key` in whichever dependency groups of the importer's package.json declare `from`.
    Dependencies,
    /// `key` in this catalog of the root package.json (`""` is the default catalog).
    Catalog(String),
    /// The rule for `key` in the root package.json's `overrides` (or `resolutions`): its parent
    /// (name and range) and its target range.
    Override {
        parent: Option<(String, String)>,
        target_range: String,
    },
}

impl Site {
    /// `parent@range>name@range`, the form `overrides` also accepts.
    fn override_key(parent: &Option<(String, String)>, target_range: &str, name: &str) -> String {
        let seg = |n: &str, r: &str| {
            if r.is_empty() {
                n.to_owned()
            } else {
                format!("{n}@{r}")
            }
        };
        match parent {
            Some((p, r)) => format!("{}>{}", seg(p, r), seg(name, target_range)),
            None => seg(name, target_range),
        }
    }
}

#[derive(Debug, Clone)]
struct PackageJsonEdit {
    /// The importer whose package.json is edited (the root for catalogs and overrides).
    owner: PackageId,
    /// Its workspace path (`""` is the root).
    workspace: String,
    file: String,
    site: Site,
    key: String,
    from: String,
    to: String,
}

impl PackageJsonEdit {
    fn same_site(&self, other: &PackageJsonEdit) -> bool {
        self.owner == other.owner
            && self.site == other.site
            && self.key == other.key
            && self.from == other.from
    }
}

/// An edge of the graph that reaches a vulnerable package.
#[derive(Debug, Clone)]
struct Edge {
    parent: PackageId,
    index: usize,
    /// The range after overrides and catalogs, when that is an npm range.
    range: Option<Range>,
    /// The text of `range`.
    range_text: Option<String>,
    /// The override's value, else the dependent's specifier.
    literal: String,
    dependent: String,
    /// The key of the `overrides` rule that holds the version.
    override_rule: Option<String>,
    bundled: bool,
    peer: bool,
    latest_fixes: bool,
    pin: Option<PackageJsonEdit>,
}

struct Instance {
    id: PackageId,
    name: String,
    from: String,
    current: Version,
    advisories: Vec<usize>,
    edges: Vec<Edge>,
}

#[derive(Debug, Clone)]
struct Fix {
    name: String,
    from: String,
    /// The vulnerable package this replaces.
    old: PackageId,
    to: String,
    downgrade: bool,
    too_recent: bool,
    edges: Vec<(PackageId, usize)>,
    edits: Vec<PackageJsonEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Blocker {
    dependent: String,
    range: String,
    bundled: bool,
    peer: bool,
    latest_fixes: bool,
    override_rule: Option<String>,
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

#[derive(Debug, Clone)]
struct Unavailable {
    name: String,
    from: String,
    /// `404`, the status code, or the error's first word.
    error: String,
    detail: String,
}

#[derive(Default)]
struct Plan {
    fixes: Vec<Fix>,
    blocked: Vec<Blocked>,
    unfixable: Vec<Unfixable>,
    unavailable: Vec<Unavailable>,
    unmatched: Vec<(String, String)>,
    unaudited: Vec<fnpm_registry::UnauditedRegistry>,
    fixed: usize,
    remaining: usize,
    checked: usize,
    /// Advisories that matched an installed version before the fix.
    matched: Vec<bool>,
    /// The `name@version`s the plan removes without a blocker.
    expected_gone: Vec<(String, String)>,
    /// The new versions, for the install.
    manifests: HashMap<String, Arc<PackageManifest>>,
}

/// The path of an importer (`""` for the root), `None` for any other package.
fn importer_path(graph: &LockGraph, id: PackageId) -> Option<String> {
    match &graph.packages.get(id.index())?.resolution {
        Resolution::Root => Some(String::new()),
        Resolution::Workspace { path } => Some(path.trim_end_matches('/').to_owned()),
        _ => None,
    }
}

fn importer_file(graph: &LockGraph, id: PackageId) -> Option<String> {
    importer_path(graph, id).map(|path| {
        if path.is_empty() {
            "package.json".to_owned()
        } else {
            format!("{path}/package.json")
        }
    })
}

/// The dependent as bun names it: the package.json of an importer, `name@version` otherwise.
fn label(graph: &LockGraph, id: PackageId) -> String {
    if let Some(file) = importer_file(graph, id) {
        return file;
    }
    let p = &graph.packages[id.index()];
    match &p.resolution {
        Resolution::Npm { version, .. } => format!("{}@{version}", p.name),
        _ => p.name.clone(),
    }
}

/// The version range of a specifier (an `npm:` alias unwrapped), as text, when it is one.
fn range_text_of(literal: &str) -> Option<&str> {
    let range = crate::spec::split_npm_alias(literal)
        .map_or(literal, |(_, r)| r)
        .trim();
    (fnpm_types::spec::infer_tag(range) == fnpm_types::SpecTag::Npm
        && !range.contains(':')
        && !range.contains('/'))
    .then_some(range)
}

fn is_exact(literal: &str) -> bool {
    range_text_of(literal)
        .and_then(|r| Range::parse(r).ok())
        .is_some_and(|r| r.as_exact().is_some())
}

fn catalog_name(literal: &str) -> Option<&str> {
    literal.trim().strip_prefix("catalog:").map(str::trim)
}

/// The rule's selector as the root package.json spells it, minus the target name.
fn site_of(rule: &OverrideRule) -> Site {
    Site::Override {
        parent: rule
            .selector
            .parent
            .as_ref()
            .map(|p| (p.name.clone(), p.range.clone())),
        target_range: rule.selector.target.range.clone(),
    }
}

fn rule_key(rule: &OverrideRule) -> String {
    match site_of(rule) {
        Site::Override {
            parent,
            target_range,
        } => Site::override_key(&parent, &target_range, &rule.selector.target.name),
        _ => unreachable!("site_of returns an override site"),
    }
}

fn selects(
    site_parent: &Option<(String, String)>,
    target_range: &str,
    name: &str,
    parent: Option<&PackageSelector>,
    target: &PackageSelector,
) -> bool {
    target.name == name
        && target.range == target_range
        && parent.map(|p| (p.name.clone(), p.range.clone())) == *site_parent
}

/// In file order, every value of the root package.json's `overrides` (or `resolutions`) that
/// reads as the rule `site` for `name`.
fn for_each_override_value(
    root: &mut Value,
    name: &str,
    site: &Site,
    mut f: impl FnMut(&mut Value),
) {
    let Site::Override {
        parent: site_parent,
        target_range,
    } = site
    else {
        return;
    };
    let field = if root.get("overrides").is_some() {
        "overrides"
    } else {
        "resolutions"
    };
    let nested = field == "overrides";
    let Some(rules) = root.get_mut(field).and_then(Value::as_object_mut) else {
        return;
    };
    for (key, value) in rules.iter_mut() {
        if !(nested && value.is_object()) {
            if let Ok(Some(sel)) = OverrideSelector::parse(key)
                && selects(
                    site_parent,
                    target_range,
                    name,
                    sel.parent.as_ref(),
                    &sel.target,
                )
            {
                f(value);
            }
            continue;
        }
        let Ok(parent) = fnpm_types::overrides::parse_package_segment(key) else {
            continue;
        };
        let Some(children) = value.as_object_mut() else {
            continue;
        };
        for (child_key, child) in children.iter_mut() {
            let (rule_parent, target) = if child_key == "." {
                (None, parent.clone())
            } else {
                match OverrideSelector::parse(child_key) {
                    Ok(Some(OverrideSelector {
                        parent: None,
                        target,
                    })) => (Some(parent.clone()), target),
                    _ => continue,
                }
            };
            if selects(
                site_parent,
                target_range,
                name,
                rule_parent.as_ref(),
                &target,
            ) {
                f(child);
            }
        }
    }
}

/// When several entries spell the same rule, the last one, which the resolver keeps.
fn override_literal(root: &Value, name: &str, site: &Site) -> Option<String> {
    let mut root = root.clone();
    let mut literal = None;
    for_each_override_value(&mut root, name, site, |value| {
        if let Some(text) = value.as_str() {
            literal = Some(text.trim().to_owned());
        }
    });
    literal
}

/// The entry whose rewrite would move this dependency: an exact pin, or with `latest`, any npm
/// range.
fn pin_for(
    graph: &LockGraph,
    root_json: Option<&Value>,
    e: &DepEdge,
    parent: PackageId,
    rule: Option<&OverrideRule>,
    latest: bool,
) -> Option<PackageJsonEdit> {
    if e.behavior.is_bundled() || e.behavior.is_workspace() {
        return None;
    }
    // The name to edit under comes from what declares the version: the rule, else the edge.
    let (name, value) = match rule {
        Some(rule) => (rule.selector.target.name.as_str(), rule.value.as_str()),
        None => {
            importer_path(graph, parent)?;
            (e.name.as_str(), e.literal.as_str())
        }
    };
    if let Some(catalog) = catalog_name(value) {
        let entry = graph.catalogs.get(catalog, name)?;
        range_text_of(entry)?;
        if !latest && !is_exact(entry) {
            return None;
        }
        return Some(PackageJsonEdit {
            owner: PackageId::ROOT,
            workspace: String::new(),
            file: "package.json".into(),
            site: Site::Catalog(catalog.to_owned()),
            key: name.to_owned(),
            from: entry.trim().to_owned(),
            to: String::new(),
        });
    }
    range_text_of(value)?;
    if !latest && !is_exact(value) {
        return None;
    }
    match rule {
        Some(rule) => {
            let site = site_of(rule);
            let written = override_literal(root_json?, name, &site)?;
            // They differ for a `$reference`, because the lockfile stores what it resolved to.
            if written != rule.value.trim() {
                return None;
            }
            Some(PackageJsonEdit {
                owner: PackageId::ROOT,
                workspace: String::new(),
                file: "package.json".into(),
                site,
                key: name.to_owned(),
                from: written,
                to: String::new(),
            })
        }
        None => Some(PackageJsonEdit {
            owner: parent,
            workspace: importer_path(graph, parent)?,
            file: importer_file(graph, parent)?,
            site: Site::Dependencies,
            key: name.to_owned(),
            from: value.trim().to_owned(),
            to: String::new(),
        }),
    }
}

/// The literal a pin is rewritten to: the same style (`=`, `~`, `^`, exact, `npm:` alias) with
/// the new version.
fn new_literal(old: &str, to: &str, exact: bool) -> String {
    crate::spec::updated_literal(old.trim(), to, exact, true).unwrap_or_else(|| to.to_owned())
}

/// bun names a manifest it could not fetch by the request's status, or the error.
fn unavailable_reason(
    client: &fnpm_registry::RegistryClient,
    name: &str,
    error: &fnpm_registry::Error,
) -> (String, String) {
    use fnpm_registry::Error as E;
    match error {
        E::NotFound(_) => (
            "404".into(),
            format!(
                "GET {}{} - 404",
                client.registry_url_for(name),
                name.replace('/', "%2f")
            ),
        ),
        E::Status { url, status } | E::Registry { url, status, .. } => {
            (status.to_string(), format!("GET {url} - {status}"))
        }
        other => {
            let detail = other.to_string();
            let first = detail
                .split_whitespace()
                .next()
                .unwrap_or("unknown")
                .trim_end_matches(':')
                .to_owned();
            (first, detail)
        }
    }
}

struct PlanInput<'a> {
    graph: &'a LockGraph,
    root_json: Option<&'a Value>,
    advisories: &'a [Advisory],
    client: &'a fnpm_registry::RegistryClient,
    latest: bool,
    exact: bool,
    /// `--minimum-release-age` and its excludes.
    min_age_ms: Option<u64>,
    age_excludes: &'a fnpm_registry::ReleaseAgeExcludes,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

async fn plan(cx: &Ctx, input: PlanInput<'_>) -> Plan {
    let PlanInput {
        graph,
        root_json,
        advisories,
        client,
        latest,
        exact,
        min_age_ms,
        age_excludes,
    } = input;
    let mut plan = Plan::default();
    let mut checked = HashSet::new();
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
            id,
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
        for (index, e) in owner.dependencies.iter().enumerate() {
            let Some(&inst) = e.target.and_then(|t| instance_of.get(&t)) else {
                continue;
            };
            if e.behavior.is_optional_peer() {
                continue;
            }
            let bundled = e.behavior.is_bundled();
            // A bundled copy ships inside its dependent whatever an override says.
            let rule = if bundled || e.behavior.is_workspace() {
                None
            } else {
                fnpm_resolver::overrides::find(&graph.overrides, Some(owner), &e.name, &e.literal)
            };
            let (spec_name, mut effective) = match rule {
                Some(r) => (r.selector.target.name.as_str(), r.value.as_str()),
                None => (e.name.as_str(), e.literal.as_str()),
            };
            if let Some(catalog) = catalog_name(effective)
                && let Some(entry) = graph.catalogs.get(catalog, spec_name)
            {
                effective = entry;
            }
            let range_text = range_text_of(effective).map(str::to_owned);
            let range = range_text.as_deref().and_then(|r| Range::parse(r).ok());
            let mut pin = pin_for(graph, root_json, e, parent, rule, true);
            let latest_fixes = !latest && pin.is_some();
            if latest_fixes {
                pin = pin_for(graph, root_json, e, parent, rule, false);
            }
            instances[inst].edges.push(Edge {
                parent,
                index,
                range,
                range_text,
                literal: rule.map_or(&e.literal, |r| &r.value).clone(),
                dependent: label(graph, parent),
                override_rule: rule.map(rule_key),
                bundled,
                peer: e.behavior.is_peer() && !bundled,
                latest_fixes,
                pin,
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
    let mut still: Vec<bool> = matched.iter().map(|m| !m).collect();

    for inst in instances {
        let manifest = match client
            .manifest(
                &inst.name,
                ManifestRequest {
                    // Publish times are only in the full document.
                    full: min_age_ms.is_some(),
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
                let (error, detail) = unavailable_reason(client, &inst.name, &e);
                plan.unavailable.push(Unavailable {
                    name: inst.name.clone(),
                    from: inst.from.clone(),
                    error,
                    detail,
                });
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
            // The bulk response only has advisories for the release lines that are installed.
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
            |e: &Edge, v: &Version| !e.bundled && e.range.as_ref().is_some_and(|r| r.satisfies(v));
        let pin_accepts = |e: &Edge, v: &Version| {
            e.pin.is_some() && (latest || caret.as_ref().is_some_and(|c| c.satisfies(v)))
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
        // If the root depends on this package, every peer dependency on it uses the root's
        // copy, whatever range the peer declares.
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
                // If the root's copy cannot be upgraded, upgrading a peer dependency has no effect.
                .filter(|(e, _)| !(e.peer && matches!(root_target, Some(None))))
                .filter_map(|(_, t)| t.map(|t| t.0))
                .collect()
        };
        chosen.sort_unstable();
        chosen.dedup();
        let age_limit = min_age_ms.filter(|_| !age_excludes.excludes_package(&inst.name));
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
                if *edit
                    && let Some(pin) = &e.pin
                    && !edits.iter().any(|x| x.same_site(pin))
                {
                    let mut pin = pin.clone();
                    pin.to = new_literal(&pin.from, &to, exact);
                    edits.push(pin);
                }
            }
            let too_recent = age_limit.is_some_and(|limit| {
                !age_excludes.excludes_version(&inst.name, version)
                    && manifest.time.get(&to).is_some_and(|&published| {
                        published == u64::MAX
                            || (published != 0 && now_ms().saturating_sub(published) < limit)
                    })
            });
            plan.fixes.push(Fix {
                name: inst.name.clone(),
                from: inst.from.clone(),
                old: inst.id,
                to,
                downgrade: *downgrade,
                too_recent,
                edges,
                edits,
            });
        }
        let mut blockers: Vec<Blocker> = Vec::new();
        for (e, _) in inst.edges.iter().zip(&target).filter(|(_, t)| t.is_none()) {
            let b = Blocker {
                dependent: if e.override_rule.is_some() {
                    "package.json".into()
                } else {
                    e.dependent.clone()
                },
                range: if e.bundled {
                    inst.from.clone()
                } else {
                    e.range_text.clone().unwrap_or_else(|| e.literal.clone())
                },
                bundled: e.bundled,
                peer: e.peer && e.override_rule.is_none() && !e.latest_fixes,
                latest_fixes: e.latest_fixes,
                override_rule: e.override_rule.clone(),
            };
            // Every edge a rule holds reports the same blocker.
            if !blockers.contains(&b) {
                blockers.push(b);
            }
        }
        plan.manifests.insert(inst.name.clone(), manifest.clone());
        if blockers.is_empty() {
            plan.expected_gone
                .push((inst.name.clone(), inst.from.clone()));
            continue;
        }
        // If the root's copy is upgraded, the vulnerable version is removed even though these
        // peer dependencies' ranges reject the new version.
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
    plan.fixes.sort_by(|a, b| {
        (&a.name, &a.from).cmp(&(&b.name, &b.from)).then_with(|| {
            let v = |s: &str| Version::parse(s).ok();
            v(&a.to).cmp(&v(&b.to))
        })
    });
    plan.blocked
        .sort_by(|a, b| (&a.name, &a.from).cmp(&(&b.name, &b.from)));
    plan.unfixable
        .sort_by(|a, b| (&a.name, &a.from).cmp(&(&b.name, &b.from)));
    plan.unavailable
        .sort_by(|a, b| (&a.name, &a.from).cmp(&(&b.name, &b.from)));
    for u in &plan.unavailable {
        cx.events.emit(fnpm_types::Event::warn(
            format!(
                "{}@{} was not checked for updates: {}",
                u.name, u.from, u.detail
            ),
            None,
        ));
    }
    plan.remaining = still.iter().filter(|s| **s).count();
    plan.fixed = advisories.len() - plan.remaining;
    plan.matched = matched;
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

/// Text on stdout as it is decided, so the plan is on screen before the install runs.
fn say(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn esc(s: &str) -> String {
    crate::security::escape_control(s, false)
}

fn sections(plan: &Plan) -> String {
    let mut lines: Vec<String> = Vec::new();
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
            if f.too_recent {
                line.push_str(" (newer than --minimum-release-age)");
            }
            lines.push(line);
            for e in &f.edits {
                let mut line = format!("    {}", esc(&e.file));
                match &e.site {
                    Site::Dependencies => {}
                    Site::Catalog(c) if c.is_empty() || c == "default" => {
                        line.push_str(" (catalog)");
                    }
                    Site::Catalog(c) => line.push_str(&format!(" (catalog {})", esc(c))),
                    Site::Override {
                        parent: None,
                        target_range,
                    } if target_range.is_empty() => line.push_str(" (overrides)"),
                    Site::Override {
                        parent,
                        target_range,
                    } => line.push_str(&format!(
                        " (overrides {})",
                        esc(&Site::override_key(parent, target_range, &e.key))
                    )),
                }
                line.push_str(&format!(": {} {ARROW} {}", esc(&e.from), esc(&e.to)));
                lines.push(line);
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
                let verb = if x.bundled {
                    "bundles"
                } else if x.override_rule.is_some() {
                    "overrides"
                } else {
                    "depends on"
                };
                let mut line = format!(
                    "    {} {verb} {}@{}",
                    esc(&x.dependent),
                    esc(&b.name),
                    esc(&x.range)
                );
                match &x.override_rule {
                    Some(rule) if *rule != b.name => line.push_str(&format!(" ({})", esc(rule))),
                    None if x.peer => line.push_str(" (peer)"),
                    _ => {}
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
            let tokens: Vec<String> = u.tokens.iter().map(|t| esc(t)).collect();
            lines.push(format!(
                "  {}@{}  {}",
                esc(&u.name),
                esc(&u.from),
                tokens.join(", ")
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
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn skipped_count(unaudited: &[fnpm_registry::UnauditedRegistry]) -> usize {
    unaudited.iter().map(|u| u.packages.len()).sum()
}

fn summary(plan: &Plan, dry_run: bool, fixed: usize, remaining: usize, start: Instant) -> String {
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
    let skipped = skipped_count(&plan.unaudited);
    let checked = plan.checked.saturating_sub(skipped);
    if skipped > 0 {
        line.push_str(&format!(" (checked {checked}, {skipped} skipped)"));
    } else {
        line.push_str(&format!(" (checked {checked})"));
    }
    line.push_str(&format!(
        " [{:.2}ms]\n",
        start.elapsed().as_secs_f64() * 1000.0
    ));
    if remaining > 0 {
        line.push_str(&format!("{remaining} {} remaining\n", vuln_word(remaining)));
    }
    line
}

fn edit_json(e: &PackageJsonEdit) -> Value {
    let catalog = match &e.site {
        Site::Catalog(c) if c.is_empty() => json!("default"),
        Site::Catalog(c) => json!(c),
        _ => Value::Null,
    };
    let rule = match &e.site {
        Site::Override {
            parent,
            target_range,
        } => json!(Site::override_key(parent, target_range, &e.key)),
        _ => Value::Null,
    };
    json!({ "file": e.file, "catalog": catalog, "override": rule, "key": e.key, "from": e.from, "to": e.to })
}

fn plan_json(plan: &Plan, dry_run: bool, fixed: usize, remaining: usize, after: &[Value]) -> Value {
    json!({
        "dryRun": dry_run,
        "fixed": fixed,
        "remaining": remaining,
        "fixes": plan.fixes.iter().map(|f| json!({
            "name": f.name, "from": f.from, "to": f.to, "downgrade": f.downgrade,
            "newerThanMinimumReleaseAge": f.too_recent,
            "packageJson": f.edits.iter().map(edit_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "blocked": plan.blocked.iter().map(|b| json!({
            "name": b.name, "from": b.from, "to": b.needs, "downgrade": b.downgrade,
            "latestFixes": b.blockers.iter().any(|x| x.latest_fixes),
            "blockers": b.blockers.iter().map(|x| json!({
                "dependent": x.dependent, "range": x.range, "bundled": x.bundled,
                "override": x.override_rule,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "unfixable": plan.unfixable.iter().map(|u| json!({
            "name": u.name, "from": u.from, "advisories": u.tokens,
        })).collect::<Vec<_>>(),
        "manifestUnavailable": plan.unavailable.iter().map(|u| json!({
            "name": u.name, "from": u.from, "error": u.error,
        })).collect::<Vec<_>>(),
        "unmatched": plan.unmatched.iter().map(|(n, r)| json!({ "name": n, "range": r })).collect::<Vec<_>>(),
        "unaudited": plan.unaudited.iter().map(|u| json!({
            "registry": fnpm_config::redact_url(&u.registry),
            "packages": u.packages,
            "reason": if u.reason.is_empty() { Value::Null } else { json!(u.reason) },
        })).collect::<Vec<_>>(),
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

const GROUPS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

fn rewrite(value: &mut Value, e: &PackageJsonEdit) {
    if value.as_str().is_some_and(|v| v.trim() == e.from) {
        *value = Value::String(e.to.clone());
    }
}

/// `edits` applied to the package.json value of their importer.
fn apply_to_json(root: &mut Value, edits: &[&PackageJsonEdit]) {
    for e in edits {
        match &e.site {
            Site::Dependencies => {
                for group in GROUPS {
                    if let Some(v) = root.get_mut(group).and_then(|g| g.get_mut(&e.key)) {
                        rewrite(v, e);
                    }
                }
            }
            Site::Catalog(catalog) => {
                let default = catalog.is_empty() || catalog == "default";
                for container in [None, Some("workspaces")] {
                    let base = match container {
                        None => Some(&mut *root),
                        Some(key) => root.get_mut(key),
                    };
                    let Some(base) = base.filter(|b| b.is_object()) else {
                        continue;
                    };
                    let object = if default {
                        base.get_mut("catalog")
                    } else {
                        base.get_mut("catalogs").and_then(|c| c.get_mut(catalog))
                    };
                    if let Some(v) = object.and_then(|o| o.get_mut(&e.key)) {
                        rewrite(v, e);
                    }
                }
            }
            Site::Override { .. } => {
                for_each_override_value(root, &e.key, &e.site, |v| rewrite(v, e));
            }
        }
    }
}

/// Writes the edits; returns each changed file with its old text, to put back if the install
/// fails.
fn write_edits(
    root_dir: &std::path::Path,
    ws: &fnpm_workspace::Workspace,
    edits: &[&PackageJsonEdit],
) -> Result<Vec<(PathBuf, String)>> {
    let mut by_workspace: BTreeMap<&str, Vec<&PackageJsonEdit>> = BTreeMap::new();
    for e in edits {
        by_workspace
            .entry(e.workspace.as_str())
            .or_default()
            .push(e);
    }
    let mut written = Vec::new();
    for (workspace, edits) in by_workspace {
        let dir = if workspace.is_empty() {
            root_dir.to_path_buf()
        } else {
            match ws.member_by_path(workspace) {
                Some(m) => m.dir.clone(),
                None => continue,
            }
        };
        let file = dir.join("package.json");
        let text = std::fs::read_to_string(&file).map_err(Error::io(&file))?;
        let mut value = super::json_text::parse_package_json(&text)
            .map_err(|e| Error::usage(format!("{}: {}", file.display(), e.0)))?;
        let before = value.clone();
        apply_to_json(&mut value, &edits);
        if value == before {
            continue;
        }
        let mut out = super::json_text::print(&value, &super::json_text::guess_indent(&text));
        if text.ends_with('\n') {
            out.push('\n');
        }
        crate::project::write_atomic(&file, out.as_bytes())?;
        written.push((file, text));
    }
    Ok(written)
}

fn restore(written: &[(PathBuf, String)]) {
    for (file, text) in written {
        let _ = crate::project::write_atomic(file, text.as_bytes());
    }
}

/// The graph edit that applies the plan: the vulnerable packages replaced, the rewritten pins
/// mirrored so the lockfile matches package.json.
fn graph_patch(plan: &Plan, config: &fnpm_config::Config) -> GraphPatch {
    let replacements: Vec<(Fix, Package)> = plan
        .fixes
        .iter()
        .filter_map(|f| {
            let m = plan.manifests.get(&f.name)?;
            let reg = config.registry_for(&f.name).url.clone();
            Some((f.clone(), package_for(&f.name, m, &f.to, &reg)?))
        })
        .collect();
    let edits: Vec<PackageJsonEdit> = plan.fixes.iter().flat_map(|f| f.edits.clone()).collect();
    GraphPatch(Arc::new(move |g: &mut LockGraph| {
        let mut replaced: HashMap<PackageId, PackageId> = HashMap::new();
        for (f, p) in &replacements {
            let new = PackageId(g.packages.len() as u32);
            g.packages.push(p.clone());
            replaced.entry(f.old).or_insert(new);
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
        // An optional peer follows the copy it was bound to.
        for p in &mut g.packages {
            for e in &mut p.dependencies {
                if e.behavior.is_optional_peer()
                    && let Some(new) = e.target.and_then(|t| replaced.get(&t))
                {
                    e.target = Some(*new);
                }
            }
        }
        for e in &edits {
            match &e.site {
                Site::Dependencies => {
                    let Some(owner) = g.packages.get_mut(e.owner.index()) else {
                        continue;
                    };
                    for d in &mut owner.dependencies {
                        if d.name == e.key && d.literal.trim() == e.from {
                            d.literal = e.to.clone();
                        }
                    }
                }
                Site::Catalog(catalog) => {
                    let map = if catalog.is_empty() || catalog == "default" {
                        Some(&mut g.catalogs.default)
                    } else {
                        g.catalogs.named.get_mut(catalog)
                    };
                    if let Some(v) = map.and_then(|m| m.get_mut(&e.key))
                        && v.trim() == e.from
                    {
                        *v = e.to.clone();
                    }
                }
                Site::Override { .. } => {
                    for rule in &mut g.overrides {
                        if rule.selector.target.name == e.key
                            && site_of(rule) == e.site
                            && rule.value.trim() == e.from
                        {
                            rule.value = e.to.clone();
                        }
                    }
                }
            }
        }
    }))
}

/// bun's `No vulnerabilities found (checked N packages, …)` line.
fn no_vulnerabilities(
    resp: &fnpm_registry::AuditResponse,
    filtered: &Filtered,
    level: Option<&str>,
    start: Instant,
) -> String {
    let checked = resp.checked;
    let mut line = format!(
        "No vulnerabilities found (checked {checked} package{}",
        if checked == 1 { "" } else { "s" }
    );
    if filtered.below_level > 0
        && let Some(l) = level
    {
        line.push_str(&format!(
            ", {} below --audit-level={l}",
            filtered.below_level
        ));
    }
    if filtered.ignored > 0 {
        line.push_str(&format!(", {} ignored", filtered.ignored));
    }
    if resp.skipped > 0 {
        line.push_str(&format!(", {} skipped", resp.skipped));
    }
    line.push_str(&format!(
        ") [{:.2}ms]\n",
        start.elapsed().as_secs_f64() * 1000.0
    ));
    line
}

/// `fnpm audit fix`.
pub(crate) async fn fix(cx: &Ctx) -> Result<CoreResult> {
    let start = Instant::now();
    let loud = cx.pm.log_level != fnpm_config::LogLevel::Silent;
    let fail = |message: &str, note: &str| {
        if loud {
            eprintln!("error: {message}");
            eprintln!("note: {note}");
        }
        Error::Reported
    };
    if cx.req.positionals.len() > 1 {
        return Err(fail(
            "fnpm audit fix does not take arguments, it always fixes the whole lockfile",
            "run 'fnpm audit --help' for more information",
        ));
    }
    let json_out = cx.req.json;
    // Like bun, the header comes first, whatever happens next.
    let printing = loud && !json_out;
    if printing {
        say(&format!("fnpm audit fix v{}\n\n", cli_version(cx)));
    }
    // bun `exit_unless_lockfile_writable`.
    let frozen = |frozen: bool| {
        if frozen {
            fail(
                "fnpm audit fix needs to write fnpm.lock, but the lockfile is frozen",
                "run 'fnpm audit fix' without --frozen-lockfile / --production",
            )
        } else {
            fail(
                "fnpm audit fix needs to write fnpm.lock, but saving the lockfile is disabled",
                "run 'fnpm audit fix' without --no-save",
            )
        }
    };
    let dry_run = cx.flag("dryRun");
    let latest = cx.flag("latest");
    if !dry_run {
        if cx.flag("frozenLockfile") || cx.flag("production") {
            return Err(frozen(true));
        }
        if cx.req.options.get("save") == Some(&Value::Bool(false)) {
            return Err(frozen(false));
        }
    }
    let (project, ws, graph) = match cx.load()? {
        (p, ws, Some(g)) => (p, ws, g),
        _ => {
            return Err(fail(
                "missing lockfile, nothing to audit",
                "run 'fnpm install' first",
            ));
        }
    };
    let config = project.config.clone();
    if !dry_run {
        if config.frozen_lockfile || config.production {
            return Err(frozen(true));
        }
        if !config.save_package_json || !config.lockfile.save {
            return Err(frozen(false));
        }
    }

    let packages = audit_packages(&graph);
    let client = project.client()?;
    // Reuse the resolver's complete declared-state check, including workspaces,
    // catalogs, overrides and missing resolutions, before asking the audit endpoint.
    if !fnpm_resolver::lockfile_matches(&ws, &graph)? {
        return Err(fail(
            "fnpm.lock does not match package.json, nothing to fix",
            "run 'fnpm install' first",
        ));
    }
    let resp = client.audit(&packages).await?;
    super::info::print_unaudited(&resp.unaudited, loud);
    if let Some(failure) = &resp.default_failure {
        super::info::report_audit_failure(failure, loud);
        return Err(Error::Reported);
    }
    let ignore = cx.strings("ignore");
    let level = cx.string("auditLevel").map(|l| l.to_ascii_lowercase());
    let mut filtered = Filtered::default();
    let advisories = advisories_of(&resp.advisories, &ignore, level.as_deref(), &mut filtered);
    if advisories.is_empty() && !json_out {
        if printing {
            say(&no_vulnerabilities(
                &resp,
                &filtered,
                level.as_deref(),
                start,
            ));
        }
        return Ok(super::exit(0, Value::Null, Vec::new()));
    }

    let root_json = std::fs::read_to_string(project.root_dir.join("package.json"))
        .ok()
        .and_then(|t| super::json_text::parse_package_json(&t).ok());
    let (age_excludes, _) =
        fnpm_registry::ReleaseAgeExcludes::parse(&config.minimum_release_age_excludes);
    let exact = config.exact || cx.flag("exact");
    let mut plan = plan(
        cx,
        PlanInput {
            graph: &graph,
            root_json: root_json.as_ref(),
            advisories: &advisories,
            client: &client,
            latest,
            exact,
            min_age_ms: config.minimum_release_age_ms.filter(|&ms| ms > 0),
            age_excludes: &age_excludes,
        },
    )
    .await;
    plan.unaudited = resp.unaudited.clone();
    if printing {
        say(&sections(&plan));
    }
    if dry_run || plan.fixes.is_empty() {
        let (fixed, remaining) = (plan.fixed, plan.remaining);
        let code = i32::from(remaining > 0);
        if json_out {
            let data = plan_json(&plan, dry_run, fixed, remaining, &[]);
            return Ok(super::exit(code, data, Vec::new()));
        }
        if printing {
            say(&summary(&plan, dry_run, fixed, remaining, start));
        }
        return Ok(super::exit(code, Value::Null, Vec::new()));
    }

    // Apply: the pins are rewritten in package.json, the vulnerable packages replaced in the
    // lockfile graph, and an install resolves what the new versions depend on.
    let edits: Vec<&PackageJsonEdit> = plan.fixes.iter().flat_map(|f| f.edits.iter()).collect();
    let written = write_edits(&project.root_dir, &ws, &edits)?;
    let mut excludes = config.minimum_release_age_excludes.clone();
    excludes.extend(
        plan.fixes
            .iter()
            .filter(|f| f.too_recent)
            .map(|f| f.name.clone()),
    );
    // The security scanner, if any, sees the new versions before anything is linked.
    let opts = InstallOptions {
        pm: crate::options::PmOptions {
            cwd: project.root_dir.clone(),
            summary: false,
            progress: false,
            json: json_out,
            minimum_release_age_excludes: Some(excludes),
            ..cx.pm.clone()
        },
        command: InstallCommand::Install,
        patch: Some(graph_patch(&plan, &config)),
        ..InstallOptions::default()
    };
    if let Err(e) = crate::install_with_env(opts, cx.events.clone(), cx.env.clone()).await {
        restore(&written);
        return Err(e);
    }
    if loud {
        eprintln!("Saved lockfile");
    }

    // Audit the saved lockfile, which is what the next `fnpm audit` reads.
    let (_, _, after_graph) = cx.load_locked("nothing to audit")?;
    let after_resp = client.audit(&audit_packages(&after_graph)).await?;
    if let Some(failure) = &after_resp.default_failure {
        super::info::report_audit_failure(failure, loud);
        return Err(Error::Reported);
    }
    let after = advisories_of(
        &after_resp.advisories,
        &ignore,
        level.as_deref(),
        &mut Filtered::default(),
    );
    if json_out {
        plan.unaudited = after_resp.unaudited.clone();
    }
    let staying: Vec<(&str, &str)> = plan
        .blocked
        .iter()
        .map(|b| (b.name.as_str(), b.from.as_str()))
        .chain(
            plan.unfixable
                .iter()
                .map(|u| (u.name.as_str(), u.from.as_str())),
        )
        .chain(
            plan.unavailable
                .iter()
                .map(|u| (u.name.as_str(), u.from.as_str())),
        )
        .collect();
    // An advisory of the plan is fixed when no installed version matches it any more.
    let mut plan_remaining: Vec<bool> = plan.matched.iter().map(|m| !m).collect();
    let mut still: Vec<(String, String, Vec<String>)> = Vec::new();
    for p in &after_graph.packages {
        let Resolution::Npm { version, .. } = &p.resolution else {
            continue;
        };
        let Ok(v) = Version::parse(version) else {
            continue;
        };
        for (i, a) in advisories.iter().enumerate() {
            if a.name == p.name && a.matches(&v) {
                plan_remaining[i] = true;
            }
        }
        let mut tokens: Vec<String> = after
            .iter()
            .filter(|a| a.name == p.name && a.matches(&v))
            .map(|a| a.token.clone())
            .collect();
        if tokens.is_empty() {
            continue;
        }
        tokens.sort();
        tokens.dedup();
        let planned_gone = plan
            .expected_gone
            .iter()
            .any(|(n, f)| *n == p.name && f == version);
        let reported_staying = staying.contains(&(p.name.as_str(), version.as_str()));
        if planned_gone || !reported_staying {
            still.push((p.name.clone(), version.clone(), tokens));
        }
    }
    still.sort();
    still.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let fixed = plan_remaining.iter().filter(|r| !**r).count();
    let remaining = after.len();
    let code = i32::from(remaining > 0);
    if json_out {
        let after_json: Vec<Value> = still
            .iter()
            .map(|(n, v, t)| json!({ "name": n, "version": v, "advisories": t }))
            .collect();
        let data = plan_json(&plan, false, fixed, remaining, &after_json);
        return Ok(super::exit(code, data, Vec::new()));
    }
    if printing {
        let mut text = String::new();
        if !still.is_empty() {
            text.push_str("vulnerable after install:\n");
            for (n, v, t) in &still {
                let tokens: Vec<String> = t.iter().map(|t| esc(t)).collect();
                text.push_str(&format!("  {}@{}  {}\n", esc(n), esc(v), tokens.join(", ")));
            }
            text.push('\n');
        }
        text.push_str(&summary(&plan, false, fixed, remaining, start));
        say(&text);
    }
    Ok(super::exit(code, Value::Null, Vec::new()))
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

    fn advisories(resp: &Value, ignore: &[String]) -> Vec<Advisory> {
        advisories_of(resp, ignore, None, &mut Filtered::default())
    }

    #[test]
    fn literals_keep_their_style() {
        assert_eq!(new_literal("1.0.2", "1.0.4", false), "1.0.4");
        assert_eq!(new_literal("^1.0.2", "1.1.0", false), "^1.1.0");
        assert_eq!(new_literal("~1.0.2", "1.0.4", false), "~1.0.4");
        assert_eq!(
            new_literal("npm:no-deps@1.0.0", "1.0.1", false),
            "npm:no-deps@1.0.1"
        );
    }

    #[test]
    fn advisory_ranges_include_prereleases_and_partial_lower_bounds() {
        let version = Version::parse("1.0.0-rc.1").unwrap();
        for range in [
            "<1.1.0",
            ">0 <1.1.0",
            "1.0.x",
            "0.x || 1.0",
            ">=1.0 <1.1.0",
            "^1.0 <1.1.0",
            "1.0.0 - 1.0.9",
        ] {
            let items = advisories(&json!({"a": [{"vulnerable_versions": range}]}), &[]);
            assert!(items[0].matches(&version), "{range}");
            assert!(
                !items[0].matches(&Version::parse("1.1.0").unwrap()),
                "{range}"
            );
        }
        let none = advisories(&json!({"a": [{"vulnerable_versions": "not a range"}]}), &[]);
        assert!(!none[0].matches(&version));
        let all = advisories(&json!({"a": [{"vulnerable_versions": "*"}]}), &[]);
        assert!(all[0].matches(&Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn advisory_tokens_and_filters() {
        let resp = json!({ "a": [
            { "id": 1, "url": "https://github.com/advisories/GHSA-aaaa-bbbb-cccc", "vulnerable_versions": "<1.0.4", "severity": "low" },
            { "id": 2, "url": "https://example.com/x", "vulnerable_versions": ">=2", "severity": "high" },
        ]});
        let all = advisories(&resp, &[]);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].token, "GHSA-aaaa-bbbb-cccc");
        assert_eq!(all[1].token, "2");
        assert!(all[0].matches(&Version::parse("1.0.2").unwrap()));
        assert!(!all[0].matches(&Version::parse("1.0.4").unwrap()));
        assert_eq!(advisories(&resp, &["GHSA-aaaa-bbbb-cccc".into()]).len(), 1);
        let mut filtered = Filtered::default();
        assert_eq!(
            advisories_of(&resp, &["2".into()], Some("high"), &mut filtered).len(),
            0
        );
        assert_eq!((filtered.below_level, filtered.ignored), (1, 1));
    }

    #[test]
    fn override_entries_are_found_in_every_spelling() {
        let site = |parent: Option<(&str, &str)>, range: &str| Site::Override {
            parent: parent.map(|(a, b)| (a.to_owned(), b.to_owned())),
            target_range: range.to_owned(),
        };
        for (text, s, key) in [
            (
                r#"{"overrides":{"no-deps":"1.0.0"}}"#,
                site(None, ""),
                "no-deps",
            ),
            (
                r#"{"overrides":{"no-deps":{".":"1.0.0"}}}"#,
                site(None, ""),
                "no-deps",
            ),
            (
                r#"{"resolutions":{"**/no-deps":"1.0.0"}}"#,
                site(None, ""),
                "no-deps",
            ),
            (
                r#"{"overrides":{"one-range-dep>no-deps":"1.0.0"}}"#,
                site(Some(("one-range-dep", "")), ""),
                "one-range-dep>no-deps",
            ),
            (
                r#"{"overrides":{"one-range-dep":{"no-deps":"1.0.0"}}}"#,
                site(Some(("one-range-dep", "")), ""),
                "one-range-dep>no-deps",
            ),
            (
                r#"{"overrides":{"no-deps@<1.0.1":"1.0.0"}}"#,
                site(None, "<1.0.1"),
                "no-deps@<1.0.1",
            ),
        ] {
            let mut root: Value = serde_json::from_str(text).unwrap();
            assert_eq!(
                override_literal(&root, "no-deps", &s).as_deref(),
                Some("1.0.0"),
                "{text}"
            );
            let Site::Override {
                parent,
                target_range,
            } = &s
            else {
                unreachable!()
            };
            assert_eq!(Site::override_key(parent, target_range, "no-deps"), key);
            let edit = PackageJsonEdit {
                owner: PackageId::ROOT,
                workspace: String::new(),
                file: "package.json".into(),
                site: s.clone(),
                key: "no-deps".into(),
                from: "1.0.0".into(),
                to: "1.0.1".into(),
            };
            apply_to_json(&mut root, &[&edit]);
            assert_eq!(
                override_literal(&root, "no-deps", &s).as_deref(),
                Some("1.0.1"),
                "{text}"
            );
        }
    }

    #[test]
    fn pins_in_every_group_and_catalog_are_rewritten() {
        let mut root = json!({
            "dependencies": { "a": "1.0.2" },
            "peerDependencies": { "a": "1.0.2" },
            "workspaces": { "catalog": { "b": "1.0.0" }, "catalogs": { "build": { "b": "1.0.0" } } },
        });
        let edit = |site: Site, key: &str, from: &str, to: &str| PackageJsonEdit {
            owner: PackageId::ROOT,
            workspace: String::new(),
            file: "package.json".into(),
            site,
            key: key.into(),
            from: from.into(),
            to: to.into(),
        };
        let edits = [
            edit(Site::Dependencies, "a", "1.0.2", "1.0.4"),
            edit(Site::Catalog("build".into()), "b", "1.0.0", "1.0.1"),
        ];
        apply_to_json(&mut root, &edits.iter().collect::<Vec<_>>());
        assert_eq!(root["dependencies"]["a"], "1.0.4");
        assert_eq!(root["peerDependencies"]["a"], "1.0.4");
        assert_eq!(root["workspaces"]["catalog"]["b"], "1.0.0");
        assert_eq!(root["workspaces"]["catalogs"]["build"]["b"], "1.0.1");
    }
}
