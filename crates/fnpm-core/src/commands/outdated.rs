//! `fnpm outdated` (bun `outdated_command.rs`): current / update / latest per direct registry
//! dependency of the selected workspaces. `data.packages` is also what `fnpm update -i` lists.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use fnpm_registry::{
    ManifestRequest, PackageManifest, PickOptions, PickResult, ReleaseAgeExcludes,
};
use fnpm_semver::{Range, Version};
use fnpm_types::{Behavior, DependencyGroup, LockGraph, PackageId, Resolution};
use fnpm_workspace::{RootSelection, SelectOptions, Selector};
use serde::Serialize;
use serde_json::json;

use super::{Ctx, ok, table};
use crate::Result;

/// One row of `fnpm outdated` (`OutdatedEntry` in `types.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutdatedEntry {
    /// The dependency name as declared (the alias for `npm:` aliases).
    pub name: String,
    /// The registry package when it differs from `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// The installed version.
    pub current: String,
    /// The newest version the declared range allows.
    pub update: String,
    /// The `latest` dist-tag (after `minimumReleaseAge`).
    pub latest: String,
    /// Where it is declared.
    pub dependency_type: DependencyGroup,
    /// The declaring workspace (or `catalog (a, b)` for a shared catalog entry), when the
    /// table shows the column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// `update` or `latest` was held back by `minimumReleaseAge`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub filtered: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// bun's package patterns: `*` / `**` match everything, `!*` nothing, `!pat` excludes, other
/// patterns are `*` globs over the package name or its alias.
fn selects(patterns: &[String], name: &str, alias: Option<&str>) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let m = |p: &str| {
        crate::install::name_matches(p, name)
            || alias.is_some_and(|a| crate::install::name_matches(p, a))
    };
    let mut positive = false;
    let mut any_positive = false;
    for p in patterns {
        match p.strip_prefix('!') {
            Some(neg) if m(neg) || neg == "*" || neg == "**" => return false,
            Some(_) => {}
            None => {
                any_positive = true;
                positive |= p == "*" || p == "**" || m(p);
            }
        }
    }
    positive || !any_positive
}

fn group_of(b: Behavior) -> DependencyGroup {
    if b.is_dev() {
        DependencyGroup::Dev
    } else if b.is_peer() {
        DependencyGroup::Peer
    } else if b.is_optional() {
        DependencyGroup::Optional
    } else {
        DependencyGroup::Prod
    }
}

fn group_rank(g: DependencyGroup) -> u8 {
    match g {
        DependencyGroup::Prod => 0,
        DependencyGroup::Dev => 1,
        DependencyGroup::Peer => 2,
        DependencyGroup::Optional => 3,
    }
}

fn label(g: DependencyGroup) -> &'static str {
    match g {
        DependencyGroup::Prod => "",
        DependencyGroup::Dev => " (dev)",
        DependencyGroup::Peer => " (peer)",
        DependencyGroup::Optional => " (optional)",
    }
}

/// A declared dependency to check.
struct Row {
    workspace: PackageId,
    alias: String,
    package: String,
    literal: String,
    current: Version,
    group: DependencyGroup,
    catalog: Option<String>,
}

/// The effective range of a declared literal: `npm:` aliases stripped, catalogs looked up.
fn effective_literal(graph: &LockGraph, alias: &str, literal: &str) -> (String, Option<String>) {
    if let Some(cat) = literal.strip_prefix("catalog:") {
        let spec = graph
            .catalogs
            .get(cat.trim(), alias)
            .unwrap_or("")
            .to_owned();
        return (
            effective_literal(graph, alias, &spec).0,
            Some(cat.trim().to_owned()),
        );
    }
    match crate::spec::split_npm_alias(literal) {
        Some((_, range)) => (
            if range.is_empty() {
                "latest".into()
            } else {
                range.to_owned()
            },
            None,
        ),
        None => (literal.to_owned(), None),
    }
}

/// The newest version `literal` allows (a dist-tag follows the tag).
fn find_update<'m>(
    m: &'m PackageManifest,
    literal: &str,
    opts: PickOptions,
    excludes: &ReleaseAgeExcludes,
) -> PickResult<'m> {
    let lit = literal.trim();
    if matches!(
        fnpm_types::spec::infer_tag(lit),
        fnpm_types::SpecTag::DistTag
    ) && !lit.is_empty()
    {
        return m.pick_tag(lit, opts, Some(excludes));
    }
    match Range::parse(if lit.is_empty() { "*" } else { lit }) {
        Ok(r) => m.pick_detailed(&r, opts, Some(excludes)),
        Err(_) => PickResult::NotFound,
    }
}

/// `fnpm outdated [patterns] [--filter ...] [-r]`.
pub(crate) async fn run(cx: &Ctx) -> Result<crate::CoreResult> {
    let (project, ws, graph) = cx.load_locked("nothing outdated")?;
    let config = project.config.clone();
    let filters = cx.strings("filter");
    let recursive = cx.flag("recursive");
    let was_filtered = !filters.is_empty() || recursive;
    let selected: Vec<PackageId> = if was_filtered {
        let selectors = Selector::parse_all(&filters)?;
        let sel = fnpm_workspace::select_with(
            &ws,
            &selectors,
            &cx.req.cwd,
            &SelectOptions {
                root: RootSelection::Implicit,
                ..SelectOptions::default()
            },
        );
        sel.selected
            .iter()
            .filter_map(|&i| {
                let path = if i == usize::MAX {
                    String::new()
                } else {
                    ws.members[i].path.clone()
                };
                graph.workspaces.get(&path).copied()
            })
            .collect()
    } else {
        let here = ws
            .member_for_dir(&cx.req.cwd)
            .map(|m| m.path.clone())
            .unwrap_or_default();
        graph.workspaces.get(&here).copied().into_iter().collect()
    };

    let patterns: Vec<String> = cx
        .req
        .positionals
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect();
    let mut rows = Vec::new();
    for &wid in &selected {
        let Some(wpkg) = graph.get(wid) else { continue };
        for e in &wpkg.dependencies {
            let Some(t) = e.target.and_then(|t| graph.get(t)) else {
                continue;
            };
            let Resolution::Npm { version, .. } = &t.resolution else {
                continue;
            };
            let Ok(current) = Version::parse(version) else {
                continue;
            };
            let alias = (e.name != t.name).then_some(e.name.as_str());
            if !selects(&patterns, &t.name, alias) {
                continue;
            }
            let (literal, catalog) = effective_literal(&graph, &e.name, &e.literal);
            rows.push(Row {
                workspace: wid,
                alias: e.name.clone(),
                package: t.name.clone(),
                literal,
                current,
                group: group_of(e.behavior),
                catalog,
            });
        }
    }

    // Fetch every manifest concurrently.
    let client = project.client()?;
    let full = config.minimum_release_age_ms.is_some();
    let names: BTreeSet<String> = rows.iter().map(|r| r.package.clone()).collect();
    let fetches = names.into_iter().map(|name| {
        let client = client.clone();
        async move {
            let r = client
                .manifest(
                    &name,
                    ManifestRequest {
                        full,
                        revalidate: true,
                    },
                )
                .await;
            (name, r)
        }
    });
    let mut manifests: BTreeMap<String, Arc<PackageManifest>> = BTreeMap::new();
    let mut failures = Vec::new();
    for (name, r) in futures_util::future::join_all(fetches).await {
        match r {
            Ok(m) => {
                manifests.insert(name, m);
            }
            Err(e) => failures.push(format!("error: {name}: {e}")),
        }
    }

    let opts = PickOptions {
        include_prerelease: false,
        minimum_release_age_ms: config.minimum_release_age_ms,
        now_ms: now_ms(),
    };
    let (excludes, _) = ReleaseAgeExcludes::parse(&config.minimum_release_age_excludes);
    let none = ReleaseAgeExcludes::default();
    let ungated = PickOptions {
        minimum_release_age_ms: None,
        ..opts
    };
    let ws_name = |id: PackageId| graph.get(id).map(|p| p.name.clone()).unwrap_or_default();

    struct Found {
        row: usize,
        update: String,
        latest: String,
        filtered: bool,
    }
    let mut found: Vec<Found> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let Some(m) = manifests.get(&r.package) else {
            continue;
        };
        let Some(actual_latest) = m.pick_latest(ungated, Some(&none)).version() else {
            continue;
        };
        let latest = m.pick_latest(opts, Some(&excludes));
        let update = find_update(m, &r.literal, opts, &excludes);
        // bun `is_outdated`: an update in range, or a newer release at all. A newer release that
        // the age gate holds back still lists the package, its columns marked with `*`.
        let is_outdated = update
            .version()
            .is_some_and(|u| u.version.cmp_precedence(&r.current).is_ne())
            || actual_latest.version.cmp_precedence(&r.current).is_gt();
        if !is_outdated {
            continue;
        }
        let current = r.current.to_string();
        found.push(Found {
            row: i,
            update: update
                .version()
                .map_or(current.clone(), |v| v.version.to_string()),
            latest: latest
                .version()
                .map_or(current.clone(), |v| v.version.to_string()),
            filtered: update.latest_is_filtered() || latest.latest_is_filtered(),
        });
    }

    // One row per catalog entry, listing the workspaces that use it.
    let mut catalog_users: BTreeMap<(String, String, u8), Vec<PackageId>> = BTreeMap::new();
    for f in &found {
        let r = &rows[f.row];
        if let Some(cat) = &r.catalog {
            catalog_users
                .entry((r.alias.clone(), cat.clone(), group_rank(r.group)))
                .or_default()
                .push(r.workspace);
        }
    }
    let show_workspace = was_filtered || !catalog_users.is_empty();
    let mut entries: Vec<(u8, OutdatedEntry)> = Vec::new();
    for f in &found {
        let r = &rows[f.row];
        let workspace = match &r.catalog {
            Some(cat) => {
                let users = &catalog_users[&(r.alias.clone(), cat.clone(), group_rank(r.group))];
                if users[0] != r.workspace
                    || entries.iter().any(|(_, e)| {
                        e.name == r.alias
                            && e.workspace
                                .as_deref()
                                .is_some_and(|w| w.starts_with("catalog"))
                    })
                {
                    continue;
                }
                let names: Vec<String> = users.iter().map(|u| ws_name(*u)).collect();
                let head = if cat.is_empty() {
                    "catalog (".to_owned()
                } else {
                    format!("catalog:{cat} (")
                };
                Some(format!("{head}{})", names.join(", ")))
            }
            None => show_workspace.then(|| ws_name(r.workspace)),
        };
        entries.push((
            group_rank(r.group),
            OutdatedEntry {
                name: r.alias.clone(),
                package: (r.alias != r.package).then(|| r.package.clone()),
                current: r.current.to_string(),
                update: f.update.clone(),
                latest: f.latest.clone(),
                dependency_type: r.group,
                workspace,
                filtered: f.filtered,
            },
        ));
    }
    entries.sort_by_key(|(rank, _)| *rank);
    let entries: Vec<OutdatedEntry> = entries.into_iter().map(|(_, e)| e).collect();

    let mut lines = Vec::new();
    if !entries.is_empty() {
        let mut header = vec!["Package", "Current", "Update", "Latest"];
        if show_workspace {
            header.push("Workspace");
        }
        let any_filtered = entries.iter().any(|e| e.filtered);
        let table_rows: Vec<Vec<String>> = entries
            .iter()
            .map(|e| {
                let name = match &e.package {
                    Some(pkg) => format!("{}@npm:{pkg}{}", e.name, label(e.dependency_type)),
                    None => format!("{}{}", e.name, label(e.dependency_type)),
                };
                let star = |s: &str| {
                    if e.filtered {
                        format!("{s} *")
                    } else {
                        s.to_owned()
                    }
                };
                let mut row = vec![name, e.current.clone(), star(&e.update), star(&e.latest)];
                if show_workspace {
                    row.push(e.workspace.clone().unwrap_or_default());
                }
                row
            })
            .collect();
        lines = table::render(&header, &table_rows, cx.unicode());
        if any_filtered {
            lines.push(
                "Note: The * indicates that version isn't true latest due to minimum release age"
                    .into(),
            );
        }
    }
    let code = i32::from(!failures.is_empty());
    lines.extend(failures.iter().cloned());
    let data = json!({ "packages": entries, "errors": failures });
    if code != 0 {
        return Ok(super::exit(code, data, lines));
    }
    Ok(ok(data, lines))
}

#[cfg(test)]
mod tests {
    use super::selects;

    #[test]
    fn patterns() {
        let p = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(selects(&p(&[]), "a", None));
        assert!(selects(&p(&["*"]), "@s/a", None));
        assert!(!selects(&p(&["!*"]), "a", None));
        assert!(selects(&p(&["re*"]), "react", None));
        assert!(!selects(&p(&["re*"]), "vue", None));
        assert!(selects(&p(&["my"]), "react", Some("my")));
        assert!(!selects(&p(&["!react"]), "react", None));
        assert!(selects(&p(&["!react"]), "vue", None));
    }
}
