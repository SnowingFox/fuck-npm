//! Registry reads: `whoami`, and `audit` / `fnpm-lock audit` (`fnpm info` is in `view.rs`).

use std::collections::{BTreeMap, HashMap, HashSet};

use fnpm_registry::AuditPackage;
use fnpm_types::Resolution;
use serde_json::{Value, json};

use super::{Ctx, exit, ok};
use crate::{CoreResult, Result};

/// `fnpm whoami`.
pub(crate) async fn whoami(cx: &Ctx) -> Result<CoreResult> {
    let project = cx.project_anywhere()?;
    let client = project.client()?;
    let (user, notice) = client.whoami(cx.string("authType").as_deref()).await?;
    let mut lines = Vec::new();
    if let Some(n) = &notice {
        lines.push(format!("npm notice {n}"));
    }
    lines.push(user.clone());
    Ok(ok(json!({ "username": user }), lines))
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "critical" => 4,
        "high" => 3,
        "moderate" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// `fnpm audit [--audit-level x] [--ignore id]` / `fnpm-lock audit`.
pub(crate) async fn audit(cx: &Ctx, lock_tool: bool) -> Result<CoreResult> {
    let started = std::time::Instant::now();
    if cx.req.positionals.first().is_some_and(|a| a == "fix") && !lock_tool {
        return super::audit_fix::fix(cx).await;
    }
    let (project, _, graph) = cx.load_locked("nothing to audit")?;
    let mut versions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in graph.packages.iter().skip(1) {
        if let Resolution::Npm { version, .. } = &p.resolution {
            let v = versions.entry(p.name.clone()).or_default();
            if !v.contains(version) {
                v.push(version.clone());
            }
        }
    }
    let packages: Vec<AuditPackage> = versions
        .into_iter()
        .map(|(name, versions)| AuditPackage { name, versions })
        .collect();
    let client = project.client()?;
    let resp = client.audit(&packages).await?;
    let level = cx.string("auditLevel").map(|l| l.to_ascii_lowercase());
    let level_rank = level.as_deref().map_or(0, severity_rank);
    let ignore: Vec<String> = cx.strings("ignore");
    let (mut below_level, mut ignored) = (0usize, 0usize);
    let mut found = Vec::new();
    if let Value::Object(map) = &resp.advisories {
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
                    .unwrap_or("moderate")
                    .to_owned();
                if severity_rank(&severity) < level_rank {
                    below_level += 1;
                    continue;
                }
                if ignore
                    .iter()
                    .any(|i| !i.is_empty() && (*i == id || url.contains(i.as_str())))
                {
                    ignored += 1;
                    continue;
                }
                found.push((name.clone(), severity, a.clone()));
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    for u in &resp.unaudited {
        cx.events.emit(fnpm_types::Event::warn(
            format!(
                "{} did not answer the audit request ({}); skipped {}",
                fnpm_config::redact_url(&u.registry),
                u.reason,
                u.packages.join(", ")
            ),
            None,
        ));
    }
    let esc = |s: &str| crate::security::escape_control(s, false);
    let mut lines = Vec::new();
    if !lock_tool {
        lines.push(format!(
            "fnpm audit v{}",
            cx.string("cliVersion")
                .unwrap_or_else(|| crate::version().to_owned())
        ));
        lines.push(String::new());
    }
    if found.is_empty() {
        let checked = resp.checked;
        let mut line = format!(
            "No vulnerabilities found (checked {checked} package{}",
            if checked == 1 { "" } else { "s" }
        );
        if below_level > 0
            && let Some(l) = &level
        {
            line.push_str(&format!(", {below_level} below --audit-level={l}"));
        }
        if ignored > 0 {
            line.push_str(&format!(", {ignored} ignored"));
        }
        if resp.skipped > 0 {
            line.push_str(&format!(", {} skipped", resp.skipped));
        }
        line.push_str(&format!(
            ") [{:.2}ms]",
            started.elapsed().as_secs_f64() * 1000.0
        ));
        lines.push(line);
    } else {
        let separator = ">";
        // name -> the names of the packages that depend on it.
        let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for p in graph.packages.iter().skip(1) {
            if matches!(p.resolution, Resolution::Workspace { .. }) {
                continue;
            }
            for e in &p.dependencies {
                let list = dependents.entry(e.name.as_str()).or_default();
                if !list.contains(&p.name.as_str()) {
                    list.push(p.name.as_str());
                }
            }
        }
        let root = graph.root();
        let workspaces: Vec<&fnpm_types::Package> = graph
            .packages
            .iter()
            .filter(|p| matches!(p.resolution, Resolution::Workspace { .. }))
            .collect();
        let depends_on =
            |p: &fnpm_types::Package, name: &str| p.dependencies.iter().any(|e| e.name == name);
        let paths_to = |target: &str| -> Vec<Vec<String>> {
            let mut paths: Vec<Vec<String>> = Vec::new();
            if depends_on(root, target) {
                paths.push(vec![target.to_owned()]);
            }
            for w in &workspaces {
                if depends_on(w, target) {
                    paths.push(vec![format!("workspace:{}", w.name), target.to_owned()]);
                }
            }
            let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
            let mut parent: HashMap<&str, &str> = HashMap::new();
            let mut visited: HashSet<&str> = HashSet::new();
            for d in dependents.get(target).into_iter().flatten() {
                queue.push_back(d);
                parent.entry(d).or_insert(target);
            }
            while let Some(cur) = queue.pop_front() {
                if !visited.insert(cur) {
                    continue;
                }
                let ws = workspaces.iter().find(|w| depends_on(w, cur));
                if depends_on(root, cur) || ws.is_some() {
                    let mut path = Vec::new();
                    let mut seen = HashSet::new();
                    let mut t = cur;
                    loop {
                        if !seen.insert(t) {
                            break;
                        }
                        path.push(t.to_owned());
                        match parent.get(t) {
                            Some(p) => t = p,
                            None => break,
                        }
                    }
                    if let Some(w) = ws {
                        path.insert(0, format!("workspace:{}", w.name));
                    }
                    paths.push(path);
                } else {
                    for d in dependents.get(cur).into_iter().flatten() {
                        if !visited.contains(d) {
                            queue.push_back(d);
                            parent.entry(d).or_insert(cur);
                        }
                    }
                }
            }
            paths
        };
        let mut i = 0;
        while i < found.len() {
            let name = found[i].0.clone();
            let group: Vec<&(String, String, Value)> =
                found[i..].iter().take_while(|f| f.0 == name).collect();
            i += group.len();
            let mut heading = esc(&name);
            for (j, v) in versions_of(&graph, &name).iter().enumerate() {
                heading.push_str(if j == 0 { "@" } else { ", " });
                heading.push_str(&esc(v));
            }
            lines.push(heading);
            for path in paths_to(&name) {
                let Some((pkg, deps)) = path.split_last() else {
                    continue;
                };
                if deps.is_empty() {
                    lines.push("  (direct dependency)".into());
                } else {
                    lines.push(format!(
                        "  {} {separator} {}",
                        esc(&deps.join(&format!(" {separator} "))),
                        esc(pkg)
                    ));
                }
            }
            for (_, severity, a) in group {
                let title = a.get("title").and_then(Value::as_str).unwrap_or_default();
                if title.is_empty() {
                    continue;
                }
                let sev = match severity.as_str() {
                    "critical" | "high" | "low" => severity.as_str(),
                    _ => "moderate",
                };
                let mut line = format!("  {sev}: {}", esc(title));
                let range = a
                    .get("vulnerable_versions")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !range.is_empty() {
                    line.push_str(&format!(" ({})", esc(range)));
                }
                let url = a.get("url").and_then(Value::as_str).unwrap_or_default();
                line.push_str(&format!(" - {}", esc(url)));
                lines.push(line);
            }
            lines.push(String::new());
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, s, _) in &found {
            let s = match s.as_str() {
                "critical" | "high" | "low" => s.as_str(),
                _ => "moderate",
            };
            *counts.entry(s).or_default() += 1;
        }
        let parts: Vec<String> = ["critical", "high", "moderate", "low"]
            .iter()
            .filter_map(|s| counts.get(s).map(|n| format!("{n} {s}")))
            .collect();
        let n = found.len();
        lines.push(format!(
            "{n} {} ({})",
            if n == 1 {
                "vulnerability"
            } else {
                "vulnerabilities"
            },
            parts.join(", ")
        ));
        lines.push(String::new());
        if lock_tool {
            lines.push("  fnpm-lock audit --json   the advisories as JSON".into());
        } else {
            lines.push(
                "  fnpm audit fix           upgrade the vulnerable packages within their ranges"
                    .into(),
            );
            lines.push("  fnpm audit fix --latest  also cross major versions".into());
        }
    }
    if cx.req.json {
        lines.clear();
    }
    let data = json!({
        "advisories": resp.advisories,
        "vulnerabilities": found.len(),
        "checked": resp.checked,
        "skipped": resp.skipped,
    });
    Ok(exit(i32::from(!found.is_empty()), data, lines))
}

/// The installed versions of `name`, in graph order.
fn versions_of(graph: &fnpm_types::LockGraph, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in &graph.packages {
        if p.name == name
            && let Resolution::Npm { version, .. } = &p.resolution
            && !out.contains(version)
        {
            out.push(version.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severities() {
        assert_eq!(severity_rank("high"), 3);
    }
}
