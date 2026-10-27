//! package.json: a typed view plus a format-preserving editor.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use fnpm_types::{
    Behavior, Bin, Dependency, DependencyGroup, PeerMeta, PlatformConstraints, SpecError,
};
use serde_json::{Map, Value};

use crate::{Error, Result};

pub use crate::editor::PackageJsonEditor;

/// bun's message for a `workspaces` array with a non-string item.
pub const WORKSPACES_EXPECTS_STRINGS: &str = "Workspaces expects an array of strings, like:\n  \"workspaces\": [\n    \"path/to/package\"\n  ]";

/// The fields fnpm reads. Built from the raw JSON; `raw` keeps everything (key order
/// preserved, `serde_json` has `preserve_order`).
#[derive(Debug, Clone, Default)]
pub struct PackageJson {
    /// `None` when absent, not a string, or empty.
    pub name: Option<String>,
    /// `None` when absent or not a string.
    pub version: Option<String>,
    /// Per group, in file order.
    pub dependencies: Vec<(String, String)>,
    pub dev_dependencies: Vec<(String, String)>,
    pub optional_dependencies: Vec<(String, String)>,
    pub peer_dependencies: Vec<(String, String)>,
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    /// `bundleDependencies` / `bundledDependencies`; `true` means every name in
    /// `dependencies`.
    pub bundle_dependencies: Vec<String>,
    /// `workspaces` as an array or `{ packages, catalog, catalogs, nohoist }`. `None` when
    /// absent, of another shape, or holding a non-string item (bun reports that only for the
    /// root, see [`PackageJson::workspaces_error`]).
    pub workspaces: Option<Vec<String>>,
    pub bin: Bin,
    /// String values only.
    pub scripts: BTreeMap<String, String>,
    pub platform: PlatformConstraints,
    pub engines: BTreeMap<String, String>,
    pub raw: Value,
}

/// The message of an empty package.json (bun: `file is empty`).
pub(crate) const EMPTY_FILE: &str = "file is empty";

impl PackageJson {
    /// Parses package.json text. JSONC-tolerant like bun (comments, trailing commas, BOM).
    /// An empty file is an error (bun: `file is empty`). A dependency group that is not an
    /// object, or a non-string dependency value, is an error naming the key.
    pub fn parse(text: &str) -> Result<Self> {
        Self::parse_at(text, Path::new(""))
    }

    /// Reads and parses `path`.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let text = String::from_utf8(bytes).map_err(|_| invalid(path, "invalid UTF-8"))?;
        Self::parse_at(&text, path)
    }

    pub(crate) fn parse_at(text: &str, path: &Path) -> Result<Self> {
        if text
            .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
            .is_empty()
        {
            return Err(invalid(path, EMPTY_FILE));
        }
        let raw = crate::jsonc::parse_value(text).map_err(|e| invalid(path, &e))?;
        Self::from_value(raw, path)
    }

    /// Builds the typed view from an already parsed value.
    pub fn from_value(raw: Value, path: &Path) -> Result<Self> {
        Self::build(raw, path, true)
    }

    /// Reads `path` the way bun's runtime reads a package.json (`run --filter`): a
    /// dependency group that is not an object and non-string versions are ignored instead of
    /// failing the whole file.
    pub(crate) fn read_lenient(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let text = String::from_utf8(bytes).map_err(|_| invalid(path, "invalid UTF-8"))?;
        if text
            .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
            .is_empty()
        {
            return Err(invalid(path, EMPTY_FILE));
        }
        let raw = crate::jsonc::parse_value(&text).map_err(|e| invalid(path, &e))?;
        Self::build(raw, path, false)
    }

    fn build(raw: Value, path: &Path, strict: bool) -> Result<Self> {
        let Some(obj) = raw.as_object() else {
            return Err(invalid(path, "package.json must be an object"));
        };
        let mut pkg = PackageJson {
            name: obj
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            version: obj
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..Default::default()
        };
        for group in DependencyGroup::ALL {
            let list = string_map(obj, group.field(), path, strict)?;
            match group {
                DependencyGroup::Prod => pkg.dependencies = list,
                DependencyGroup::Dev => pkg.dev_dependencies = list,
                DependencyGroup::Optional => pkg.optional_dependencies = list,
                DependencyGroup::Peer => pkg.peer_dependencies = list,
            }
        }
        if let Some(Value::Object(meta)) = obj.get("peerDependenciesMeta") {
            for (name, m) in meta {
                let optional = m.get("optional").and_then(Value::as_bool).unwrap_or(false);
                pkg.peer_dependencies_meta
                    .insert(name.clone(), PeerMeta { optional });
            }
        }
        let bundled = obj
            .get("bundleDependencies")
            .or_else(|| obj.get("bundledDependencies"));
        pkg.bundle_dependencies = match bundled {
            Some(Value::Bool(true)) => pkg.dependencies.iter().map(|(n, _)| n.clone()).collect(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        pkg.workspaces = workspaces_strings(obj.get("workspaces")).ok().flatten();
        pkg.bin = match obj.get("bin") {
            Some(Value::String(s)) => Bin::from_file(s),
            Some(Value::Object(map)) => Bin::from_map(
                map.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect::<Vec<_>>(),
            ),
            _ => match obj
                .get("directories")
                .and_then(|d| d.get("bin"))
                .and_then(Value::as_str)
            {
                Some(dir) => Bin::from_dir(dir),
                None => Bin::None,
            },
        };
        if let Some(Value::Object(scripts)) = obj.get("scripts") {
            for (k, v) in scripts {
                if let Some(s) = v.as_str() {
                    pkg.scripts.insert(k.clone(), s.to_owned());
                }
            }
        }
        if let Some(Value::Object(engines)) = obj.get("engines") {
            for (k, v) in engines {
                if let Some(s) = v.as_str() {
                    pkg.engines.insert(k.clone(), s.to_owned());
                }
            }
        }
        let list = |key: &str| -> Vec<String> {
            match obj.get(key) {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            }
        };
        pkg.platform =
            PlatformConstraints::from_package_json(&list("os"), &list("cpu"), &list("libc"));
        pkg.raw = raw;
        Ok(pkg)
    }

    /// The entries of `group`, in file order.
    pub fn group(&self, group: DependencyGroup) -> &[(String, String)] {
        match group {
            DependencyGroup::Prod => &self.dependencies,
            DependencyGroup::Dev => &self.dev_dependencies,
            DependencyGroup::Optional => &self.optional_dependencies,
            DependencyGroup::Peer => &self.peer_dependencies,
        }
    }

    /// All declared edges with behaviors, deduplicated by name with npm's precedence, in the
    /// order groups are read (dependencies, devDependencies, optionalDependencies,
    /// peerDependencies):
    ///
    /// - `optionalDependencies` overrides `dependencies`: the edge takes the optional spec and
    ///   becomes optional (keeping `DEV` if it was also a dev dependency);
    /// - a dev entry that is also prod is one edge, prod plus dev, with the prod spec;
    /// - a peer that is also a dev dependency is one edge with the dev spec and `DEV | PEER`
    ///   (installed as dev); a peer that is also prod or optional is installed through that
    ///   edge and adds nothing;
    /// - other peers are `PEER`, plus `OPTIONAL` when `peerDependenciesMeta.<name>.optional`;
    ///   names only in `peerDependenciesMeta` (optional) become `*` optional peers, like
    ///   pnpm/yarn.
    ///
    /// `BUNDLED` is set for names in `bundleDependencies`, `WORKSPACE` for `workspace:` specs.
    pub fn dependency_list(&self, include_dev: bool) -> Result<Vec<Dependency>, SpecError> {
        let mut out: Vec<Dependency> = Vec::new();
        let mut index: HashMap<&str, usize> = HashMap::new();
        let mk = |name: &str, literal: &str, behavior: Behavior| -> Result<Dependency, SpecError> {
            let mut dep = Dependency::parse(name, literal, behavior)?;
            if matches!(dep.spec, fnpm_types::DependencySpec::Workspace { .. }) {
                dep.behavior |= Behavior::WORKSPACE;
            }
            if self.bundle_dependencies.iter().any(|b| b == name) {
                dep.behavior |= Behavior::BUNDLED;
            }
            Ok(dep)
        };
        for (name, literal) in &self.dependencies {
            match index.get(name.as_str()) {
                Some(&i) => out[i] = mk(name, literal, Behavior::PROD)?,
                None => {
                    index.insert(name, out.len());
                    out.push(mk(name, literal, Behavior::PROD)?);
                }
            }
        }
        if include_dev {
            for (name, literal) in &self.dev_dependencies {
                match index.get(name.as_str()) {
                    Some(&i) => out[i].behavior |= Behavior::DEV,
                    None => {
                        index.insert(name, out.len());
                        out.push(mk(name, literal, Behavior::DEV)?);
                    }
                }
            }
        }
        for (name, literal) in &self.optional_dependencies {
            match index.get(name.as_str()) {
                Some(&i) => {
                    let dev = out[i].behavior & Behavior::DEV;
                    out[i] = mk(name, literal, Behavior::OPTIONAL | dev)?;
                }
                None => {
                    index.insert(name, out.len());
                    out.push(mk(name, literal, Behavior::OPTIONAL)?);
                }
            }
        }
        for (name, literal) in &self.peer_dependencies {
            let optional = self
                .peer_dependencies_meta
                .get(name)
                .is_some_and(|m| m.optional);
            match index.get(name.as_str()) {
                Some(&i) => {
                    let b = out[i].behavior;
                    if b.is_dev() && !b.is_prod() && !b.contains(Behavior::OPTIONAL) {
                        out[i].behavior |= Behavior::PEER;
                    }
                }
                None => {
                    index.insert(name, out.len());
                    let behavior = Behavior::PEER.with(Behavior::OPTIONAL, optional);
                    out.push(mk(name, literal, behavior)?);
                }
            }
        }
        for (name, meta) in &self.peer_dependencies_meta {
            if meta.optional && !index.contains_key(name.as_str()) {
                index.insert(name, out.len());
                out.push(mk(name, "*", Behavior::PEER | Behavior::OPTIONAL)?);
            }
        }
        Ok(out)
    }

    /// `raw[key]` for root-only fields: overrides, resolutions, catalog(s), patchedDependencies,
    /// trustedDependencies, packageExtensions, nativeDependencies, ignoreScripts, fnpm.
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    /// bun's root-only check of `workspaces`: `Some(message)` when it is an array (or
    /// `{ packages: [...] }`) with a non-string item.
    pub fn workspaces_error(&self) -> Option<&'static str> {
        workspaces_strings(self.raw.get("workspaces")).err()
    }

    /// Whether any dependency group has an entry (bun warns about a nameless workspace that
    /// declares dependencies).
    pub fn declares_dependencies(&self) -> bool {
        DependencyGroup::ALL
            .iter()
            .any(|&g| !self.group(g).is_empty())
    }
}

fn invalid(path: &Path, message: &str) -> Error {
    Error::InvalidPackageJson {
        path: PathBuf::from(path),
        message: message.to_owned(),
    }
}

/// `workspaces` entries: `Ok(None)` when absent or not an array / `{ packages: array }`.
pub(crate) fn workspaces_strings(
    value: Option<&Value>,
) -> Result<Option<Vec<String>>, &'static str> {
    let arr = match value {
        Some(Value::Array(a)) => a,
        Some(Value::Object(o)) => match o.get("packages") {
            Some(Value::Array(a)) => a,
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or(WORKSPACES_EXPECTS_STRINGS)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn string_map(
    obj: &Map<String, Value>,
    key: &str,
    path: &Path,
    strict: bool,
) -> Result<Vec<(String, String)>> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) if !strict => Ok(map
            .iter()
            .filter_map(|(name, v)| Some((name.clone(), v.as_str()?.to_owned())))
            .collect()),
        Some(_) if !strict => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, v)| match v {
                Value::String(s) => Ok((name.clone(), s.clone())),
                _ => Err(invalid(
                    path,
                    &format!("\"{key}\" has a non-string version for \"{name}\""),
                )),
            })
            .collect(),
        Some(_) => Err(invalid(
            path,
            &format!(
                "{key} expects a map of specifiers, e.g.\n  \"{key}\": {{\n    \"bun\": \"latest\"\n  }}"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnpm_types::{DependencySpec, WorkspaceSpec};

    #[test]
    fn typed_fields() {
        let p = PackageJson::parse(
            r#"{
              // jsonc
              "name": "a", "version": "1.0.0",
              "dependencies": {"x": "^1", "y": "workspace:*"},
              "devDependencies": {"z": "2"},
              "peerDependencies": {"p": "*"},
              "peerDependenciesMeta": {"p": {"optional": true}, "q": {"optional": true}},
              "bundleDependencies": ["x"],
              "workspaces": {"packages": ["packages/*"], "catalog": {}},
              "bin": "./cli.js",
              "scripts": {"build": "tsc", "bad": 1},
              "engines": {"node": ">=18"},
              "os": ["linux"],
            }"#,
        )
        .unwrap();
        assert_eq!(p.name.as_deref(), Some("a"));
        assert_eq!(p.version.as_deref(), Some("1.0.0"));
        assert_eq!(
            p.dependencies,
            vec![
                ("x".into(), "^1".into()),
                ("y".into(), "workspace:*".into())
            ]
        );
        assert_eq!(
            p.group(DependencyGroup::Dev),
            &[("z".to_owned(), "2".to_owned())]
        );
        assert!(p.peer_dependencies_meta["p"].optional);
        assert_eq!(p.bundle_dependencies, vec!["x"]);
        assert_eq!(p.workspaces, Some(vec!["packages/*".to_owned()]));
        assert_eq!(p.bin, Bin::from_file("./cli.js"));
        assert_eq!(p.scripts.len(), 1);
        assert_eq!(p.engines["node"], ">=18");
        assert!(!p.platform.is_any());
        assert!(p.field("workspaces").is_some());
        assert!(p.declares_dependencies());
    }

    #[test]
    fn errors() {
        assert!(matches!(
            PackageJson::parse(""),
            Err(Error::InvalidPackageJson { .. })
        ));
        assert!(
            PackageJson::parse("  \n")
                .unwrap_err()
                .to_string()
                .contains("file is empty")
        );
        assert!(PackageJson::parse("asdfsadfas").is_err());
        assert!(PackageJson::parse("[]").is_err());
        let e = PackageJson::parse(r#"{"dependencies": {"a": 1}}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("\"a\""), "{e}");
        let e = PackageJson::parse(r#"{"dependencies": ["a"]}"#)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("dependencies expects a map of specifiers"),
            "{e}"
        );
        // null groups are treated as absent
        assert!(PackageJson::parse(r#"{"dependencies": null}"#).is_ok());
    }

    #[test]
    fn lenient_read_ignores_bad_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name": "a", "dependencies": {"x": 1, "y": "^1"}, "devDependencies": ["z"]}"#,
        )
        .unwrap();
        assert!(PackageJson::read(&path).is_err());
        let p = PackageJson::read_lenient(&path).unwrap();
        assert_eq!(p.dependencies, vec![("y".to_owned(), "^1".to_owned())]);
        assert!(p.dev_dependencies.is_empty());
        std::fs::write(&path, "asdf").unwrap();
        assert!(PackageJson::read_lenient(&path).is_err());
        std::fs::write(&path, " ").unwrap();
        assert!(PackageJson::read_lenient(&path).is_err());
    }

    #[test]
    fn names_and_workspaces_shapes() {
        assert_eq!(PackageJson::parse(r#"{"name": ""}"#).unwrap().name, None);
        assert_eq!(PackageJson::parse(r#"{"name": 1}"#).unwrap().name, None);
        let p = PackageJson::parse(r#"{"workspaces": ["a", 1]}"#).unwrap();
        assert_eq!(p.workspaces, None);
        assert_eq!(p.workspaces_error(), Some(WORKSPACES_EXPECTS_STRINGS));
        let p = PackageJson::parse(r#"{"workspaces": {"nohoist": []}}"#).unwrap();
        assert_eq!(p.workspaces, None);
        assert_eq!(p.workspaces_error(), None);
        let p = PackageJson::parse(r#"{"bundledDependencies": true, "dependencies": {"a": "1"}}"#)
            .unwrap();
        assert_eq!(p.bundle_dependencies, vec!["a"]);
        let p = PackageJson::parse(r#"{"directories": {"bin": "./bin"}}"#).unwrap();
        assert_eq!(p.bin, Bin::from_dir("./bin"));
        let p = PackageJson::parse(r#"{"bin": {"a": "./a.js", "b": "./b.js"}}"#).unwrap();
        assert_eq!(p.bin.entries("x").len(), 2);
    }

    fn deps(json: &str, dev: bool) -> Vec<(String, String, Behavior)> {
        PackageJson::parse(json)
            .unwrap()
            .dependency_list(dev)
            .unwrap()
            .into_iter()
            .map(|d| (d.name, d.literal, d.behavior))
            .collect()
    }

    #[test]
    fn dependency_precedence() {
        let json = r#"{
            "dependencies": {"a": "1", "b": "1", "c": "1"},
            "devDependencies": {"a": "2", "d": "2", "e": "2"},
            "optionalDependencies": {"b": "3", "d": "3"},
            "peerDependencies": {"e": "^2", "a": "*", "f": "*", "g": "*"},
            "peerDependenciesMeta": {"g": {"optional": true}, "h": {"optional": true}, "i": {"optional": false}}
        }"#;
        assert_eq!(
            deps(json, true),
            vec![
                ("a".into(), "1".into(), Behavior::PROD | Behavior::DEV),
                ("b".into(), "3".into(), Behavior::OPTIONAL),
                ("c".into(), "1".into(), Behavior::PROD),
                ("d".into(), "3".into(), Behavior::OPTIONAL | Behavior::DEV),
                ("e".into(), "2".into(), Behavior::DEV | Behavior::PEER),
                ("f".into(), "*".into(), Behavior::PEER),
                ("g".into(), "*".into(), Behavior::PEER | Behavior::OPTIONAL),
                ("h".into(), "*".into(), Behavior::PEER | Behavior::OPTIONAL),
            ]
        );
        let no_dev = deps(json, false);
        assert_eq!(no_dev[0], ("a".into(), "1".into(), Behavior::PROD));
        assert!(
            no_dev
                .iter()
                .any(|(n, l, b)| n == "e" && l == "^2" && *b == Behavior::PEER)
        );
        assert!(
            no_dev
                .iter()
                .any(|(n, _, b)| n == "d" && *b == Behavior::OPTIONAL)
        );
    }

    #[test]
    fn dependency_flags() {
        let list = PackageJson::parse(
            r#"{"dependencies": {"w": "workspace:^", "x": "1"}, "bundleDependencies": ["x"]}"#,
        )
        .unwrap()
        .dependency_list(true)
        .unwrap();
        assert_eq!(list[0].behavior, Behavior::PROD | Behavior::WORKSPACE);
        assert_eq!(
            list[0].spec,
            DependencySpec::Workspace {
                spec: WorkspaceSpec::Caret
            }
        );
        assert_eq!(list[1].behavior, Behavior::PROD | Behavior::BUNDLED);
    }
}
