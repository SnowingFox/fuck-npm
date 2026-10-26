//! `fnpm info` / `fnpm pm view` (bun `pm_view_command.rs`): registry metadata of one version
//! of a package, as a summary, one or more fields, or JSON.

use serde_json::{Map, Value};

use super::json_text;
use super::pkg::{EMPTY_BRACKETS_GET, parse_key_path, query_in};
use super::{Ctx, exit};
use crate::{CoreResult, Error, Result};

fn escape(s: &str) -> String {
    fnpm_types::text::escape_control_chars(s)
}

/// bun `escape_control_chars_multiline`: like [`escape`], but tabs, newlines and CRLF stay.
fn escape_multiline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' | '\n' => out.push(c),
            '\r' if chars.peek() == Some(&'\n') => out.push(c),
            c => out.push_str(&escape(c.encode_utf8(&mut [0u8; 4]))),
        }
    }
    out
}

/// A value from the registry as bun prints it: on one line, as the registry sent it.
fn compact(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) if !items.is_empty() => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                compact(item, out);
            }
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            out.push_str("{ ");
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                json_text::write_string(out, key);
                out.push_str(": ");
                compact(item, out);
            }
            out.push_str(" }");
        }
        other => out.push_str(&json_text::print(other, "  ")),
    }
}

/// `versions` is built here, so it is printed over several lines; anything else the registry
/// sent stays on one.
fn to_json_text(value: &Value, built: bool) -> String {
    if built {
        return json_text::print(value, "  ");
    }
    let mut out = String::new();
    compact(value, &mut out);
    out
}

/// A `--json` failure is one flat object on stdout.
fn json_error(error: &str, spec: &str, property: Option<&str>) -> Vec<String> {
    let mut line = format!("{{ \"error\": \"{error}\", \"version\": ");
    json_text::write_string(&mut line, spec);
    if let Some(property) = property {
        line.push_str(", \"property\": ");
        json_text::write_string(&mut line, property);
    }
    line.push_str(" }");
    vec![line]
}

fn fail(message: &str) -> Error {
    eprintln!("error: {message}");
    Error::Reported
}

fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// `1.23 KB`, like bun's size formatter.
fn size(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1000.0;
    let mut unit = 0;
    while v >= 1000.0 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    format!("{v:.2} {}", UNITS[unit])
}

/// What to look up: the package name (`.` and nothing mean the project's package.json).
fn package_spec(cx: &Ctx, arg: &str) -> Result<String> {
    if arg.ends_with("package.json") {
        // fnpm extra: a path to a package.json views that package.
        let pj = fnpm_workspace::PackageJson::read(&cx.req.cwd.join(arg))?;
        return pj
            .name
            .ok_or_else(|| Error::usage(format!("{arg} has no \"name\"")));
    }
    if !arg.is_empty() && arg != "." {
        return Ok(arg.to_owned());
    }
    let root =
        fnpm_workspace::Workspace::find_root(&cx.req.cwd).unwrap_or_else(|_| cx.req.cwd.clone());
    let path = root.join("package.json");
    if !path.is_file() {
        return Err(Error::Reported);
    }
    let name = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| json_text::parse_package_json(&text).ok())
        .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_owned))
        .filter(|n| !n.is_empty());
    Ok(name.unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }))
}

/// `fnpm info [<pkg>[@spec]] [property...] [--json]`.
pub(crate) async fn view(cx: &Ctx) -> Result<CoreResult> {
    let json = cx.req.json;
    let arg = cx.req.positionals.first().cloned().unwrap_or_default();
    let fields: Vec<String> = cx.req.positionals.iter().skip(1).cloned().collect();
    let spec = match package_spec(cx, &arg) {
        Ok(spec) => spec,
        Err(Error::Reported) => {
            const NO_NAME: &str = "no package name given and no package.json found";
            if json {
                return Ok(exit(1, Value::Null, json_error(NO_NAME, &arg, None)));
            }
            return Err(fail(NO_NAME));
        }
        Err(e) => return Err(e),
    };
    let (name, version) = match fnpm_types::name::split_name_and_maybe_version(&spec) {
        (name, Some(v)) if !v.is_empty() => (name.to_owned(), v.to_owned()),
        (name, _) => (name.to_owned(), "latest".to_owned()),
    };
    // A named package needs no project (`fnpm info react` anywhere, like bun).
    let project = cx.project_anywhere()?;
    let client = project.client()?;
    let doc = match client.view(&name).await {
        Ok(doc) => doc,
        Err(fnpm_registry::Error::NotFound(what)) => {
            return Err(fail(&format!("GET {what} - 404 Not Found")));
        }
        Err(fnpm_registry::Error::Status { url, status }) => {
            return Err(fail(&format!("GET {url} - {status} {}", reason(status))));
        }
        Err(fnpm_registry::Error::Registry {
            url,
            status,
            message,
        }) => {
            return Err(fail(&format!(
                "GET {url} - {status} {}: {}",
                reason(status),
                escape(&message)
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let raw_versions = doc.get("versions").and_then(Value::as_object);
    // The registry controls the entries; one that is not an object is reported once picked.
    let mut parse_doc = doc.clone();
    if let Some(Value::Object(versions)) = parse_doc.get_mut("versions") {
        for (key, entry) in versions.iter_mut() {
            if !entry.is_object() {
                *entry = serde_json::json!({ "name": name, "version": key });
            }
        }
    }
    let manifest = fnpm_registry::PackageManifest::parse(
        &name,
        &serde_json::to_vec(&parse_doc).unwrap_or_default(),
    )
    .map_err(|e| fail(&format!("failed to parse package manifest: {e}")))?;

    // Something that is neither a tag nor a range (`github:a/b`, `npm:x@1`) matches nothing.
    let picked = (!version.starts_with("npm:")
        && matches!(
            fnpm_types::spec::infer_tag(&version),
            fnpm_types::SpecTag::Npm | fnpm_types::SpecTag::DistTag
        ))
    .then(|| manifest.find_by_spec(&version))
    .flatten()
    .map(|v| v.version.clone());
    let found = picked.as_ref().and_then(|wanted| {
        raw_versions?
            .iter()
            .find(|(key, _)| fnpm_semver::Version::parse(key).is_ok_and(|v| v == *wanted))
    });
    let Some((version_key, entry)) = found else {
        let is_dist_tag = matches!(
            fnpm_types::spec::infer_tag(&version),
            fnpm_types::SpecTag::DistTag
        ) || manifest.dist_tags.contains_key(&version);
        if json {
            let error = if is_dist_tag {
                "Dist-tag not found"
            } else {
                "No matching version found"
            };
            return Ok(exit(1, Value::Null, json_error(error, &spec, None)));
        }
        if is_dist_tag {
            eprintln!(
                "error: Package \"{}\" with tag \"{}\" not found, but package exists",
                escape(&name),
                escape(&version)
            );
            let tags: Vec<(&String, &Value)> = doc
                .get("dist-tags")
                .and_then(Value::as_object)
                .map(|t| t.iter().collect())
                .unwrap_or_default();
            if !tags.is_empty() {
                eprintln!("\nTags:");
                for (tag, v) in tags.iter().take(10) {
                    eprintln!(
                        "- {}: {}",
                        escape(tag),
                        escape(v.as_str().unwrap_or_default())
                    );
                }
                if tags.len() > 10 {
                    eprintln!("  ... and {} more", tags.len() - 10);
                }
            }
        } else {
            eprintln!(
                "error: No version of \"{}\" satisfying \"{}\" found",
                escape(&name),
                escape(&version)
            );
            // The newest published versions; prereleases only when nothing else was published.
            let mut all: Vec<&fnpm_semver::Version> = manifest
                .versions
                .iter()
                .map(|v| &v.version)
                .filter(|v| !v.is_prerelease())
                .collect();
            if all.is_empty() {
                all = manifest.versions.iter().map(|v| &v.version).collect();
            }
            let start = all.len().saturating_sub(5);
            if start < all.len() {
                eprintln!("\nRecent versions:");
                for v in &all[start..] {
                    eprintln!("- {v}");
                }
                if start > 0 {
                    eprintln!("  ... and {start} more");
                }
            }
        }
        return Err(Error::Reported);
    };
    let Value::Object(entry) = entry else {
        return Err(fail(&format!(
            "failed to parse package manifest: version \"{}\" is not an object",
            escape(version_key)
        )));
    };
    let pkg_version = version_key.clone();
    let mut selected: Map<String, Value> = entry.clone();
    // Versions as npm normalizes them: only the valid ones, in semver order.
    if let Some(raw) = raw_versions {
        let mut keys: Vec<(fnpm_semver::Version, &String)> = raw
            .keys()
            .filter_map(|k| fnpm_semver::Version::parse(k).ok().map(|v| (v, k)))
            .collect();
        keys.sort_by(|a, b| a.0.cmp_precedence(&b.0));
        selected.insert(
            "versions".into(),
            Value::Array(
                keys.into_iter()
                    .map(|(_, k)| Value::String(k.clone()))
                    .collect(),
            ),
        );
    }
    // Like `npm view`, the package-level fields too; the readme only on request.
    if let Value::Object(root) = &doc {
        for (key, value) in root {
            if key != "versions"
                && (key != "readme" || !fields.is_empty())
                && !selected.contains_key(key)
            {
                selected.insert(key.clone(), value.clone());
            }
        }
    }
    let selected = Value::Object(selected);

    if !fields.is_empty() {
        let mut found = Map::new();
        for field in &fields {
            match parse_key_path(field).filter(|p| !p.iter().any(|s| s.is_append())) {
                Some(path) => query_in(
                    &selected,
                    &path,
                    field,
                    &mut String::new(),
                    false,
                    &mut found,
                ),
                None if field.contains("[]") => {
                    if json {
                        return Ok(exit(
                            1,
                            Value::Null,
                            json_error(EMPTY_BRACKETS_GET, &spec, Some(field)),
                        ));
                    }
                    return Err(fail(EMPTY_BRACKETS_GET));
                }
                None => {}
            }
        }
        let built = |label: &str| label == "versions";
        let lines: Vec<String> = match found.len() {
            // A field that does not exist is skipped unless there is nothing else to print.
            0 => {
                if json {
                    return Ok(exit(
                        1,
                        Value::Null,
                        json_error("Property not found", &spec, Some(&fields[0])),
                    ));
                }
                for field in &fields {
                    eprintln!("error: Property {field} not found");
                }
                return Err(Error::Reported);
            }
            1 => {
                let (label, value) = found.iter().next().expect("one");
                let text = match value {
                    Value::String(s) if json => {
                        let mut out = String::new();
                        json_text::write_string(&mut out, s);
                        out
                    }
                    Value::String(s) => escape_multiline(s),
                    other if json => to_json_text(other, built(label)),
                    other => escape_multiline(&to_json_text(other, built(label))),
                };
                text.split('\n').map(str::to_owned).collect()
            }
            _ if json => json_text::print(&Value::Object(found.clone()), "  ")
                .lines()
                .map(str::to_owned)
                .collect(),
            _ => found
                .iter()
                .map(|(label, value)| {
                    format!(
                        "{} = {}",
                        escape(label),
                        escape_multiline(&to_json_text(value, built(label)))
                    )
                })
                .collect(),
        };
        return Ok(exit(0, Value::Object(found), lines));
    }
    if json {
        let lines = json_text::print(&selected, "  ")
            .lines()
            .map(str::to_owned)
            .collect();
        return Ok(exit(0, selected, lines));
    }
    let lines = summary(
        &doc,
        &selected,
        &name,
        &pkg_version,
        raw_versions.map_or(0, Map::len),
    );
    Ok(exit(0, selected, lines))
}

/// npm view's summary of the selected version. Every string comes from the registry, so all of
/// them are escaped.
fn summary(doc: &Value, m: &Value, name: &str, version: &str, versions: usize) -> Vec<String> {
    let string = |key: &str| m.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let pkg_name = string("name").unwrap_or(name);
    let pkg_version = string("version").unwrap_or(version);
    // `license` is a string, or the legacy `{ "type": "MIT", "url": ... }`.
    let license = m
        .get("license")
        .and_then(|l| l.as_str().or_else(|| l.get("type").and_then(Value::as_str)))
        .filter(|l| !l.is_empty())
        .unwrap_or("Proprietary");
    let deps: Vec<(&String, &Value)> = m
        .get("dependencies")
        .and_then(Value::as_object)
        .map(|d| d.iter().collect())
        .unwrap_or_default();
    let mut lines = vec![format!(
        "{}@{} | {} | deps: {} | versions: {versions}",
        escape(pkg_name),
        escape(pkg_version),
        escape(license),
        deps.len()
    )];
    if let Some(d) = string("description") {
        lines.push(escape(d));
    }
    if let Some(h) = string("homepage") {
        lines.push(escape(h));
    }
    if let Some(keywords) = m.get("keywords").and_then(Value::as_array) {
        let words: Vec<&str> = keywords.iter().filter_map(Value::as_str).collect();
        if !words.is_empty() {
            lines.push(format!("keywords: {}", escape(&words.join(", "))));
        }
    }
    if let Some(bin) = m.get("bin").and_then(Value::as_object)
        && !bin.is_empty()
    {
        let names: Vec<&str> = bin.keys().map(String::as_str).collect();
        lines.push(format!("bin: {}", escape(&names.join(", "))));
    }
    if let Some(d) = string("deprecated") {
        lines.push(String::new());
        lines.push(format!("DEPRECATED ⚠️  - {}", escape(d)));
    }
    if !deps.is_empty() {
        lines.push(String::new());
        lines.push(format!("dependencies ({}):", deps.len()));
        for (dep, range) in &deps {
            if let Some(range) = range.as_str() {
                lines.push(format!("- {}: {}", escape(dep), escape(range)));
            }
        }
    }
    if let Some(dist) = m.get("dist").and_then(Value::as_object) {
        lines.push(String::new());
        lines.push("dist".into());
        for key in ["tarball", "shasum", "integrity"] {
            if let Some(v) = dist.get(key).and_then(Value::as_str) {
                lines.push(format!(" .{key}: {}", escape(v)));
            }
        }
        if let Some(n) = dist.get("unpackedSize").and_then(Value::as_f64) {
            lines.push(format!(" .unpackedSize: {}", size(n as u64)));
        }
    }
    if let Some(tags) = doc.get("dist-tags").and_then(Value::as_object) {
        lines.push(String::new());
        lines.push("dist-tags:".into());
        for (tag, v) in tags {
            if let Some(v) = v.as_str() {
                lines.push(format!("{}: {}", escape(tag), escape(v)));
            }
        }
    }
    if let Some(maintainers) = doc.get("maintainers").and_then(Value::as_array) {
        lines.push(String::new());
        lines.push("maintainers:".into());
        for person in maintainers {
            let field = |k: &str| escape(person.get(k).and_then(Value::as_str).unwrap_or(""));
            let (nm, em) = (field("name"), field("email"));
            if !em.is_empty() {
                lines.push(format!("- {nm} <{em}>"));
            } else if !nm.is_empty() {
                lines.push(format!("- {nm}"));
            }
        }
    }
    if let Some(time) = doc.get("time") {
        let at = |k: &str| time.get(k).and_then(Value::as_str);
        if let Some(published) = at(pkg_version).or_else(|| at("modified")) {
            lines.push(String::new());
            lines.push(format!("Published: {}", escape(published)));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printing() {
        assert_eq!(escape_multiline("a\r\nb\tc\rd\u{1b}"), "a\r\nb\tc\\rd\\x1b");
        assert_eq!(
            to_json_text(&serde_json::json!(["a", "b"]), false),
            "[\"a\", \"b\"]"
        );
        assert_eq!(
            to_json_text(&serde_json::json!(["a"]), true),
            "[\n  \"a\"\n]"
        );
        assert_eq!(size(9615), "9.62 KB");
    }
}
