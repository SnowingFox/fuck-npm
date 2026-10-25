//! Catalogs (bun `install/lockfile/CatalogMap.rs`): `catalog:` / `catalog:<name>` specifiers.
//!
//! Catalogs are read from the root package.json only when it has `workspaces`: from the
//! `workspaces` object (`catalog`, `catalogs`), else from top-level `catalog` / `catalogs`.
//! `pnpm-workspace.yaml` catalogs fill entries package.json does not define. `catalog:` and
//! `catalog:default` name the same catalog, which may be written as `catalog` or
//! `catalogs.default` (defining a name in both is an error).

use std::collections::BTreeMap;

use fnpm_types::{Catalogs, DependencySpec};
use serde_json::Value;

use crate::{Error, Result};

fn is_default(name: &str) -> bool {
    name.is_empty() || name == "default"
}

/// `catalog:` and `catalog:default` name the same catalog.
pub fn same_name(a: &str, b: &str) -> bool {
    a == b || (is_default(a) && is_default(b))
}

/// The specifier `catalog:<catalog>` stands for, for dependency `name`. The default catalog is
/// looked up under both spellings, the one matching `catalog` first.
pub fn lookup<'a>(catalogs: &'a Catalogs, catalog: &str, name: &str) -> Option<&'a str> {
    let in_default = || catalogs.default.get(name).map(String::as_str);
    let in_group = |group: &str| {
        catalogs
            .named
            .get(group)
            .and_then(|m| m.get(name))
            .map(String::as_str)
    };
    if catalog.is_empty() {
        return in_default().or_else(|| in_group("default"));
    }
    in_group(catalog).or_else(|| {
        if catalog == "default" {
            in_default()
        } else {
            None
        }
    })
}

fn parse_group(group: &Value, out: &mut BTreeMap<String, String>) -> Result<()> {
    let Some(object) = group.as_object() else {
        return Ok(());
    };
    for (name, value) in object {
        let Some(literal) = value.as_str() else {
            continue;
        };
        if DependencySpec::parse(name, literal).is_err() {
            return Err(Error::InvalidPackageJson {
                path: "package.json".into(),
                message: format!(
                    "Invalid dependency version \"{literal}\" for \"{name}\" in catalog"
                ),
            });
        }
        out.insert(name.clone(), literal.to_owned());
    }
    Ok(())
}

fn parse_from(source: &Value, catalogs: &mut Catalogs) -> Result<bool> {
    let mut found = false;
    if let Some(default) = source.get("catalog") {
        found = true;
        parse_group(default, &mut catalogs.default)?;
    }
    if let Some(named) = source.get("catalogs") {
        found = true;
        if let Some(object) = named.as_object() {
            for (catalog, group) in object {
                let entry = catalogs.named.entry(catalog.clone()).or_default();
                parse_group(group, entry)?;
            }
        }
    }
    if !catalogs.default.is_empty()
        && let Some(default_group) = source
            .get("catalogs")
            .and_then(|c| c.get("default"))
            .and_then(Value::as_object)
        && let Some(name) = default_group
            .keys()
            .find(|n| catalogs.default.contains_key(*n))
    {
        return Err(Error::InvalidPackageJson {
            path: "package.json".into(),
            message: format!(
                "\"{name}\" is defined in both \"catalog\" and \"catalogs.default\"; keep one of them"
            ),
        });
    }
    Ok(found)
}

/// Reads the catalogs of the root package.json and merges `pnpm`'s (which only fill entries the
/// package.json does not have).
pub fn parse_catalogs(root: &Value, pnpm: &Catalogs) -> Result<Catalogs> {
    let mut catalogs = Catalogs::default();
    if let Some(workspaces) = root.get("workspaces") {
        let found = parse_from(workspaces, &mut catalogs)?;
        if !found {
            parse_from(root, &mut catalogs)?;
        }
    }
    for (name, literal) in &pnpm.default {
        catalogs
            .default
            .entry(name.clone())
            .or_insert_with(|| literal.clone());
    }
    for (group, entries) in &pnpm.named {
        let target = catalogs.named.entry(group.clone()).or_default();
        for (name, literal) in entries {
            target
                .entry(name.clone())
                .or_insert_with(|| literal.clone());
        }
    }
    catalogs.named.retain(|_, m| !m.is_empty());
    Ok(catalogs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_catalog_spellings() {
        let c = parse_catalogs(
            &json!({"workspaces": {"catalog": {"a": "1"}, "catalogs": {"tools": {"b": "2"}}}}),
            &Catalogs::default(),
        )
        .unwrap();
        assert_eq!(lookup(&c, "", "a"), Some("1"));
        assert_eq!(lookup(&c, "default", "a"), Some("1"));
        assert_eq!(lookup(&c, "tools", "b"), Some("2"));
        assert_eq!(lookup(&c, "tools", "a"), None);
        let c = parse_catalogs(
            &json!({"workspaces": {"catalogs": {"default": {"a": "3"}}}}),
            &Catalogs::default(),
        )
        .unwrap();
        assert_eq!(lookup(&c, "", "a"), Some("3"));
        assert!(same_name("", "default") && same_name("x", "x") && !same_name("x", ""));
    }

    #[test]
    fn top_level_only_when_workspaces_has_none_and_pnpm_fills_gaps() {
        let root = json!({"workspaces": ["packages/*"], "catalog": {"a": "1"}});
        let mut pnpm = Catalogs::default();
        pnpm.default.insert("a".into(), "9".into());
        pnpm.default.insert("p".into(), "2".into());
        let c = parse_catalogs(&root, &pnpm).unwrap();
        assert_eq!(c.default.get("a").map(String::as_str), Some("1"));
        assert_eq!(c.default.get("p").map(String::as_str), Some("2"));
        assert!(
            parse_catalogs(&json!({"catalog": {"a": "1"}}), &Catalogs::default())
                .unwrap()
                .is_empty()
        );
    }
}
