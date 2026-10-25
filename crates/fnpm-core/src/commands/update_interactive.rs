//! The package.json half of `fnpm update -i` (bun `update_interactive_command.rs`
//! `update_package_json_files_from_updates` / `update_catalog_definitions`): the TS CLI shows
//! the picker, sends the chosen versions here, and then runs `fnpm update <names>`.
//!
//! Each declared literal keeps its style: an `npm:` alias prefix, `^`, `~`, `=` and the
//! whitespace bun trims. A version in the declared range only rewrites `^`/`~`/exact forms
//! (like `fnpm update`); one outside it is written like `fnpm update --latest`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Ctx, ok};
use crate::install::Edit;
use crate::{CoreResult, Error, Result};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Choice {
    name: String,
    version: String,
    #[serde(default)]
    in_range: bool,
    /// The declaring workspace's name (absent for the root).
    #[serde(default)]
    workspace: Option<String>,
    /// `catalog (a, b)` rows rewrite the catalog entry.
    #[serde(default)]
    catalog: Option<String>,
}

fn literal_of(json: &Value, name: &str) -> Option<String> {
    [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .iter()
    .find_map(|g| json.get(g)?.get(name)?.as_str().map(str::to_owned))
}

/// `pm update-interactive-apply`: `options.choices` is `[{name, version, inRange, workspace?,
/// catalog?}]`. Returns the rewritten files.
pub(crate) fn apply(cx: &Ctx) -> Result<CoreResult> {
    let choices: Vec<Choice> = cx
        .opt("choices")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| Error::usage(format!("invalid choices: {e}")))?
        .unwrap_or_default();
    let project = cx.project()?;
    let ws = project.workspace(&*cx.events)?;
    let root = project.root_dir.clone();
    let mut by_file: BTreeMap<std::path::PathBuf, Vec<Edit>> = BTreeMap::new();
    for c in &choices {
        let new_literal =
            |old: &str| crate::spec::updated_literal(old.trim(), &c.version, false, !c.in_range);
        if let Some(catalog) = &c.catalog {
            let file = root.join("package.json");
            let text = std::fs::read_to_string(&file).map_err(Error::io(&file))?;
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let name = catalog.as_str();
            let table = |v: &Value| -> Option<String> {
                let cat = if name.is_empty() || name == "default" {
                    v.get("catalog")
                } else {
                    v.get("catalogs").and_then(|c| c.get(name))
                }?;
                cat.get(&c.name)?.as_str().map(str::to_owned)
            };
            let old = table(&json).or_else(|| json.get("workspaces").and_then(table));
            if let Some(spec) = old.as_deref().and_then(new_literal) {
                by_file.entry(file).or_default().push(Edit::Catalog {
                    catalog: if name == "default" {
                        String::new()
                    } else {
                        name.to_owned()
                    },
                    name: c.name.clone(),
                    spec,
                });
            }
            continue;
        }
        let dir = match &c.workspace {
            Some(w) if !w.is_empty() => match ws.members.iter().find(|m| &m.name == w) {
                Some(m) => m.dir.clone(),
                None => continue,
            },
            _ => root.clone(),
        };
        let file = dir.join("package.json");
        let text = std::fs::read_to_string(&file).map_err(Error::io(&file))?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let Some(old) = literal_of(&json, &c.name) else {
            continue;
        };
        if old.trim().starts_with("catalog:") {
            continue;
        }
        if let Some(spec) = new_literal(&old) {
            by_file.entry(file).or_default().push(Edit::Update {
                name: c.name.clone(),
                spec,
            });
        }
    }
    let mut written = Vec::new();
    for (file, edits) in by_file {
        let text = std::fs::read_to_string(&file).map_err(Error::io(&file))?;
        let out = crate::install::edit_package_json_text(&text, &edits)?;
        if out != text {
            crate::project::write_atomic(&file, out.as_bytes())?;
            written.push(file.display().to_string());
        }
    }
    Ok(ok(json!({ "written": written }), Vec::new()))
}
