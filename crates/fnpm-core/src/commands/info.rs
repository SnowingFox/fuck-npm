//! Registry reads: `fnpm info` / `pm view`, `whoami`, and `audit` / `fnpm-lock audit`.

use std::collections::BTreeMap;

use fnpm_registry::AuditPackage;
use fnpm_types::Resolution;
use serde_json::{Value, json};

use super::{Ctx, exit, ok};
use crate::{CoreResult, Error, Result};

/// The version of a packument a spec picks: a dist-tag, an exact version, or the highest
/// version satisfying a range (`latest` when no spec).
fn pick_version(doc: &Value, spec: Option<&str>) -> Option<String> {
    let tags = doc.get("dist-tags").and_then(Value::as_object);
    let versions = doc.get("versions").and_then(Value::as_object)?;
    let spec = spec.unwrap_or("latest");
    if let Some(v) = tags.and_then(|t| t.get(spec)).and_then(Value::as_str) {
        return Some(v.to_owned());
    }
    if versions.contains_key(spec) {
        return Some(spec.to_owned());
    }
    let range = fnpm_semver::Range::parse(spec).ok()?;
    versions
        .keys()
        .filter_map(|k| fnpm_semver::Version::parse(k).ok().map(|v| (v, k)))
        .filter(|(v, _)| range.satisfies(v))
        .max_by(|a, b| a.0.cmp_precedence(&b.0))
        .map(|(_, k)| k.clone())
}

/// `a.b.c` (or `a[0]`) into a JSON value.
fn lookup<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in path.split('.') {
        let (key, index) = match part.split_once('[') {
            Some((k, rest)) => (k, rest.trim_end_matches(']').parse::<usize>().ok()),
            None => (part, None),
        };
        if !key.is_empty() {
            cur = cur.get(key)?;
        }
        if let Some(i) = index {
            cur = cur.get(i)?;
        }
    }
    Some(cur)
}

fn render_value(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Null => Vec::new(),
        other => serde_json::to_string_pretty(other)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect(),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "kB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u + 1 < UNITS.len() {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// npm view's summary of one version.
fn summary(doc: &Value, version: &str, manifest: &Value) -> Vec<String> {
    let name = doc.get("name").and_then(Value::as_str).unwrap_or_default();
    let license = manifest
        .get("license")
        .and_then(Value::as_str)
        .unwrap_or("Proprietary");
    let deps = manifest
        .get("dependencies")
        .and_then(Value::as_object)
        .map_or(0, |d| d.len());
    let versions = doc
        .get("versions")
        .and_then(Value::as_object)
        .map_or(0, |v| v.len());
    let mut lines = vec![format!(
        "{name}@{version} | {license} | deps: {deps} | versions: {versions}"
    )];
    if let Some(d) = manifest.get("description").and_then(Value::as_str) {
        lines.push(d.to_owned());
    }
    if let Some(h) = manifest.get("homepage").and_then(Value::as_str) {
        lines.push(h.to_owned());
    }
    if let Some(dist) = manifest.get("dist") {
        lines.push(String::new());
        lines.push("dist".into());
        for key in ["tarball", "shasum", "integrity"] {
            if let Some(v) = dist.get(key).and_then(Value::as_str) {
                lines.push(format!(" .{key}: {v}"));
            }
        }
        if let Some(n) = dist.get("unpackedSize").and_then(Value::as_u64) {
            lines.push(format!(" .unpackedSize: {}", human_bytes(n)));
        }
    }
    if let Some(d) = manifest.get("dependencies").and_then(Value::as_object)
        && !d.is_empty()
    {
        lines.push(String::new());
        lines.push(format!("dependencies ({}):", d.len()));
        for (k, v) in d {
            lines.push(format!("- {k}: {}", v.as_str().unwrap_or_default()));
        }
    }
    if let Some(tags) = doc.get("dist-tags").and_then(Value::as_object) {
        lines.push(String::new());
        lines.push("dist-tags:".into());
        for (k, v) in tags {
            lines.push(format!("{k}: {}", v.as_str().unwrap_or_default()));
        }
    }
    if let Some(t) = doc
        .get("time")
        .and_then(|t| t.get(version))
        .and_then(Value::as_str)
    {
        lines.push(String::new());
        lines.push(format!("Published: {t}"));
    }
    lines
}

/// `fnpm info [<pkg>[@spec]] [property...]`.
pub(crate) async fn view(cx: &Ctx) -> Result<CoreResult> {
    let arg = cx.req.positionals.first().cloned().unwrap_or_default();
    let reads_package_json = arg.is_empty() || arg == "." || arg.ends_with("package.json");
    // A named package needs no project (`fnpm info react` anywhere, like bun).
    let project = if reads_package_json {
        cx.project()?
    } else {
        cx.project_anywhere()?
    };
    let (name, spec) = if reads_package_json {
        let path = if arg.ends_with("package.json") {
            cx.req.cwd.join(&arg)
        } else {
            project.root_dir.join("package.json")
        };
        let pj = fnpm_workspace::PackageJson::read(&path)?;
        let Some(name) = pj.name else {
            return Err(Error::usage(format!("{} has no \"name\"", path.display())));
        };
        (name, pj.version)
    } else {
        let (n, v) = fnpm_types::name::split_name_and_maybe_version(&arg);
        (n.to_owned(), v.map(str::to_owned))
    };
    let client = project.client()?;
    let doc = client.view(&name).await?;
    let Some(version) = pick_version(&doc, spec.as_deref()) else {
        return Err(Error::Resolve(fnpm_resolver::Error::NoMatchingVersion {
            name: name.clone(),
            spec: spec.unwrap_or_else(|| "latest".into()),
        }));
    };
    let manifest = doc
        .get("versions")
        .and_then(|v| v.get(&version))
        .cloned()
        .unwrap_or(Value::Null);
    let props: Vec<&String> = cx.req.positionals.iter().skip(1).collect();
    if props.is_empty() {
        let lines = summary(&doc, &version, &manifest);
        return Ok(ok(manifest, lines));
    }
    let mut data = serde_json::Map::new();
    let mut lines = Vec::new();
    for p in &props {
        let v = lookup(&manifest, p)
            .or_else(|| lookup(&doc, p))
            .cloned()
            .unwrap_or(Value::Null);
        if props.len() > 1 {
            let rendered = render_value(&v);
            match rendered.as_slice() {
                [one] => lines.push(format!("{p} = {one}")),
                many => {
                    lines.push(format!("{p} ="));
                    lines.extend(many.iter().cloned());
                }
            }
        } else {
            lines.extend(render_value(&v));
        }
        data.insert((*p).clone(), v);
    }
    let data = if props.len() == 1 {
        data.into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(Value::Null)
    } else {
        Value::Object(data)
    };
    Ok(ok(data, lines))
}

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
    let level = cx
        .string("auditLevel")
        .map_or(0, |l| severity_rank(&l.to_ascii_lowercase()));
    let ignore: Vec<String> = cx.strings("ignore");
    let mut found = Vec::new();
    if let Value::Object(map) = &resp.advisories {
        for (name, list) in map {
            for a in list.as_array().into_iter().flatten() {
                let id = a
                    .get("id")
                    .map(|v| v.to_string().trim_matches('"').to_owned())
                    .unwrap_or_default();
                let ghsa = a
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|u| u.rsplit('/').next())
                    .unwrap_or_default()
                    .to_owned();
                if ignore.iter().any(|i| *i == id || *i == ghsa) {
                    continue;
                }
                let severity = a
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("low")
                    .to_owned();
                if severity_rank(&severity) < level {
                    continue;
                }
                found.push((name.clone(), severity, a.clone()));
            }
        }
    }
    found.sort_by(|a, b| {
        severity_rank(&b.1)
            .cmp(&severity_rank(&a.1))
            .then(a.0.cmp(&b.0))
    });
    let mut lines = Vec::new();
    for (name, severity, a) in &found {
        let title = a.get("title").and_then(Value::as_str).unwrap_or_default();
        let range = a
            .get("vulnerable_versions")
            .and_then(Value::as_str)
            .unwrap_or_default();
        lines.push(format!("{name}  {range}"));
        lines.push(format!("  {severity}: {title}"));
        if let Some(url) = a.get("url").and_then(Value::as_str) {
            lines.push(format!("  {url}"));
        }
        lines.push(String::new());
    }
    for u in &resp.unaudited {
        lines.push(format!(
            "warn: {} packages from {} were not audited ({})",
            u.packages.len(),
            u.registry,
            u.reason
        ));
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, s, _) in &found {
        *counts.entry(s.as_str()).or_default() += 1;
    }
    if found.is_empty() {
        lines.push("No vulnerabilities found".into());
    } else {
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
        if !lock_tool {
            lines.push("To update all dependencies to the latest compatible versions:".into());
            lines.push("  fnpm update".into());
        }
    }
    let data = json!({
        "advisories": resp.advisories,
        "vulnerabilities": found.len(),
        "checked": resp.checked,
        "skipped": resp.skipped,
    });
    Ok(exit(i32::from(!found.is_empty()), data, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_picking_and_lookup() {
        let doc = json!({
            "name": "a",
            "dist-tags": { "latest": "1.1.0", "next": "2.0.0-rc.1" },
            "versions": { "1.0.0": {}, "1.1.0": { "dist": { "tarball": "t" } }, "2.0.0-rc.1": {} }
        });
        assert_eq!(pick_version(&doc, None).as_deref(), Some("1.1.0"));
        assert_eq!(
            pick_version(&doc, Some("next")).as_deref(),
            Some("2.0.0-rc.1")
        );
        assert_eq!(pick_version(&doc, Some("1.0.0")).as_deref(), Some("1.0.0"));
        assert_eq!(pick_version(&doc, Some("^1.0.0")).as_deref(), Some("1.1.0"));
        assert_eq!(pick_version(&doc, Some("^3")), None);
        let v = &doc["versions"]["1.1.0"];
        assert_eq!(lookup(v, "dist.tarball"), Some(&json!("t")));
        assert_eq!(human_bytes(1500), "1.5 kB");
        assert_eq!(severity_rank("high"), 3);
    }
}
