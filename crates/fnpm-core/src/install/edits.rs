//! package.json writes of `add`, `remove`, `update`, `--catalog` and `--trust`, through the
//! format-preserving [`PackageJsonEditor`] (indentation, key order, `\r\n` and the final
//! newline are kept, as bun's `PackageJSONEditor.rs` does).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fnpm_types::DependencyGroup;
use fnpm_workspace::{PackageJsonEditor, Workspace};
use serde_json::{Map, Value};

use crate::{Error, Result};

/// One change to one package.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Edit {
    /// Add or replace a dependency in `group` (bun's move-to-group rule).
    Set {
        group: DependencyGroup,
        name: String,
        spec: String,
    },
    /// Replace the spec of an existing dependency in whichever group declares it.
    Update { name: String, spec: String },
    /// Remove a dependency from every group.
    Remove { name: String },
    /// Root only: `catalog` / `catalogs.<name>` entry.
    Catalog {
        catalog: String,
        name: String,
        spec: String,
    },
    /// Root only: add names to `trustedDependencies`.
    Trust { names: Vec<String> },
}

/// The edits of one install, by workspace path (`""` is the root).
#[derive(Debug, Default)]
pub(crate) struct Edits {
    pub by_workspace: BTreeMap<String, Vec<Edit>>,
}

impl Edits {
    pub fn push(&mut self, workspace: &str, edit: Edit) {
        self.by_workspace
            .entry(workspace.to_owned())
            .or_default()
            .push(edit);
    }

    pub fn is_empty(&self) -> bool {
        self.by_workspace.values().all(Vec::is_empty)
    }

    /// Writes every edited package.json; returns the files that changed.
    pub fn apply(&self, root_dir: &Path, ws: &Workspace) -> Result<Vec<PathBuf>> {
        let mut changed = Vec::new();
        for (path, edits) in &self.by_workspace {
            let dir = if path.is_empty() {
                root_dir.to_path_buf()
            } else {
                match ws.member_by_path(path) {
                    Some(m) => m.dir.clone(),
                    None => continue,
                }
            };
            let file = dir.join("package.json");
            let text = std::fs::read_to_string(&file).map_err(Error::io(&file))?;
            let out = apply_text(&text, edits)?;
            if out != text {
                crate::project::write_atomic(&file, out.as_bytes())?;
                changed.push(file);
            }
        }
        Ok(changed)
    }
}

/// The project as it is once `edits` are written, when one of them sets a dependency named
/// like a workspace member (that range may now link the member). `None` otherwise.
pub(crate) fn edited_for_members(ws: &Workspace, edits: &Edits) -> Option<Workspace> {
    let names_member = edits.by_workspace.values().flatten().any(|e| match e {
        Edit::Set { name, .. } | Edit::Update { name, .. } => ws.member(name).is_some(),
        _ => false,
    });
    if !names_member {
        return None;
    }
    let mut edited = ws.clone();
    for (path, list) in &edits.by_workspace {
        let manifest = if path.is_empty() {
            &mut edited.root
        } else {
            &mut edited
                .members
                .iter_mut()
                .find(|m| &m.path == path)?
                .manifest
        };
        let text = serde_json::to_string(&manifest.raw).ok()?;
        let out = apply_text(&text, list).ok()?;
        *manifest = fnpm_workspace::PackageJson::parse(&out).ok()?;
    }
    Some(edited)
}

fn group_of(value: &Value, name: &str) -> Option<DependencyGroup> {
    DependencyGroup::ALL
        .into_iter()
        .find(|g| value.get(g.field()).and_then(|d| d.get(name)).is_some())
}

/// Applies `edits` to package.json text.
pub(crate) fn apply_text(text: &str, edits: &[Edit]) -> Result<String> {
    let mut editor = PackageJsonEditor::new(text.to_owned());
    for edit in edits {
        match edit {
            Edit::Set { group, name, spec } => editor.set_dependency(*group, name, spec),
            Edit::Update { name, spec } => {
                let current: Value = parse(&editor.text)?;
                // Prefer the group that is not peerDependencies (bun keeps the peer range).
                let group = DependencyGroup::ALL
                    .into_iter()
                    .filter(|g| *g != DependencyGroup::Peer)
                    .find(|g| current.get(g.field()).and_then(|d| d.get(name)).is_some())
                    .or_else(|| group_of(&current, name));
                if let Some(group) = group {
                    set_entry(&mut editor, &current, group.field(), name, spec);
                }
            }
            Edit::Remove { name } => {
                editor.remove_dependency(name);
            }
            Edit::Catalog {
                catalog,
                name,
                spec,
            } => {
                let current: Value = parse(&editor.text)?;
                let (container, _) = catalog_container(&current);
                let path = if catalog.is_empty() || catalog == "default" {
                    join(&container, "catalog")
                } else {
                    join(&join(&container, "catalogs"), catalog)
                };
                let mut entries = lookup(&current, &path)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                entries.insert(name.clone(), Value::String(spec.clone()));
                set_object(&mut editor, &current, &path, entries);
            }
            Edit::Trust { names } => {
                let current: Value = parse(&editor.text)?;
                let mut list: Vec<String> = current
                    .get("trustedDependencies")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let before = list.len();
                for n in names {
                    if !list.contains(n) {
                        list.push(n.clone());
                    }
                }
                if list.len() != before || current.get("trustedDependencies").is_none() {
                    list.sort();
                    editor.set_path(
                        "trustedDependencies",
                        Value::Array(list.into_iter().map(Value::String).collect()),
                    );
                }
            }
        }
    }
    Ok(editor.finish())
}

fn parse(text: &str) -> Result<Value> {
    fnpm_workspace::PackageJson::parse(text)
        .map(|p| p.raw)
        .map_err(Error::from)
}

/// Where `--catalog` writes: `workspaces.catalog(s)` when `workspaces` is an object that has
/// them (or when the root has none at the top level), else the root's top-level fields.
/// Returns the container path (`""` is the root) and whether `workspaces` exists.
fn catalog_container(root: &Value) -> (Vec<String>, bool) {
    let has = |v: &Value| v.get("catalog").is_some() || v.get("catalogs").is_some();
    match root.get("workspaces") {
        Some(ws @ Value::Object(_)) if has(ws) || !has(root) => (vec!["workspaces".into()], true),
        Some(_) => (Vec::new(), true),
        None => (Vec::new(), false),
    }
}

/// Whether `--catalog` can be used (bun: the root package.json must have `workspaces`).
pub(crate) fn catalog_supported(root: &Value) -> bool {
    catalog_container(root).1
}

/// The existing entry of `name` in catalog `catalog` of the root package.json.
pub(crate) fn catalog_entry(root: &Value, catalog: &str, name: &str) -> Option<String> {
    let (container, _) = catalog_container(root);
    let path = if catalog.is_empty() || catalog == "default" {
        join(&container, "catalog")
    } else {
        join(&join(&container, "catalogs"), catalog)
    };
    lookup(root, &path)?.get(name)?.as_str().map(str::to_owned)
}

fn join(base: &[String], key: &str) -> Vec<String> {
    let mut v = base.to_vec();
    v.push(key.to_owned());
    v
}

fn lookup<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(root, |v, k| v.get(k))
}

/// Sets the object at `path`. Keys may contain dots (`lodash.merge`), which the editor's
/// dotted paths cannot express, so the deepest dot-free ancestor is replaced as a whole.
fn set_object(
    editor: &mut PackageJsonEditor,
    current: &Value,
    path: &[String],
    value: Map<String, Value>,
) {
    let mut new_value = Value::Object(value);
    let mut depth = path.len();
    while depth > 0 && path[..depth].iter().any(|k| k.contains('.')) {
        depth -= 1;
        let parent = lookup(current, &path[..depth])
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut parent = parent;
        parent.insert(path[depth].clone(), new_value);
        new_value = Value::Object(parent);
    }
    if depth == 0 {
        if let Value::Object(map) = new_value {
            for (k, v) in map {
                editor.set_path(&k, v);
            }
        }
        return;
    }
    editor.set_path(&path[..depth].join("."), new_value);
}

/// Replaces `group.name` (keeping the key position).
fn set_entry(editor: &mut PackageJsonEditor, current: &Value, group: &str, name: &str, spec: &str) {
    if !name.contains('.') {
        editor.set_path(&format!("{group}.{name}"), Value::String(spec.to_owned()));
        return;
    }
    let mut map = current
        .get(group)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    map.insert(name.to_owned(), Value::String(spec.to_owned()));
    editor.set_path(group, Value::Object(map));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_keeps_formatting_and_moves_groups() {
        let text = "{\n    \"name\": \"app\",\n    \"devDependencies\": {\n        \"a\": \"^1.0.0\"\n    }\n}";
        let out = apply_text(
            text,
            &[Edit::Set {
                group: DependencyGroup::Prod,
                name: "a".into(),
                spec: "^2.0.0".into(),
            }],
        )
        .unwrap();
        assert!(
            out.contains("    \"dependencies\": {\n        \"a\": \"^2.0.0\""),
            "{out}"
        );
        assert!(!out.contains("devDependencies"), "{out}");
        assert!(!out.ends_with('\n'), "no trailing newline was there");
    }

    #[test]
    fn update_replaces_in_place() {
        let text =
            "{\n  \"devDependencies\": {\n    \"b\": \"^1.0.0\",\n    \"a\": \"~1.0.0\"\n  }\n}\n";
        let out = apply_text(
            text,
            &[Edit::Update {
                name: "a".into(),
                spec: "~1.0.4".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            out,
            "{\n  \"devDependencies\": {\n    \"b\": \"^1.0.0\",\n    \"a\": \"~1.0.4\"\n  }\n}\n"
        );
    }

    #[test]
    fn remove_and_trust() {
        let text = "{\r\n  \"dependencies\": {\r\n    \"a\": \"1\"\r\n  }\r\n}\r\n";
        let out = apply_text(
            text,
            &[
                Edit::Remove { name: "a".into() },
                Edit::Trust {
                    names: vec!["esbuild".into()],
                },
            ],
        )
        .unwrap();
        assert!(!out.contains("\"a\""), "{out}");
        assert!(out.contains("\"trustedDependencies\""), "{out}");
        assert!(out.ends_with("}\r\n"), "{out:?}");
    }

    #[test]
    fn catalog_goes_into_workspaces_object() {
        let text = "{\n  \"workspaces\": {\n    \"packages\": [\"pkgs/*\"]\n  }\n}\n";
        let root: Value = serde_json::from_str(text).unwrap();
        assert!(catalog_supported(&root));
        let out = apply_text(
            text,
            &[Edit::Catalog {
                catalog: String::new(),
                name: "lodash.merge".into(),
                spec: "^4.6.2".into(),
            }],
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["workspaces"]["catalog"]["lodash.merge"], "^4.6.2");
        assert_eq!(
            catalog_entry(&v, "", "lodash.merge").as_deref(),
            Some("^4.6.2")
        );
        let named = apply_text(
            &out,
            &[Edit::Catalog {
                catalog: "react18".into(),
                name: "react".into(),
                spec: "^18.3.1".into(),
            }],
        )
        .unwrap();
        let v: Value = serde_json::from_str(&named).unwrap();
        assert_eq!(v["workspaces"]["catalogs"]["react18"]["react"], "^18.3.1");
        assert!(!catalog_supported(&serde_json::json!({"name": "x"})));
    }
}
