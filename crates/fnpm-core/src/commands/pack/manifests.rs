//! `workspace:` and `catalog:` specs in a packed package.json, replaced with what the
//! registry understands (bun `pack_command.rs` `Substitution`, `workspace_manifests.rs`). The
//! versions come from the package.json files, never from the lockfile, which may be stale.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use fnpm_types::Catalogs;

use super::json::Json;
use super::out::{FResult, Out};

/// The dependency groups, in the order errors are reported.
pub(crate) const GROUPS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

/// The workspaces and catalogs of the project a package is packed in.
pub(crate) struct WorkspaceManifests {
    root_dir: PathBuf,
    root_package_json: PathBuf,
    /// Every name `workspace:` links: the members, and the root unless a member has its name.
    names: HashSet<String>,
    /// Members by directory relative to the root.
    paths: HashMap<String, String>,
    versions: HashMap<String, String>,
    catalogs: Catalogs,
}

fn exact_version(version: &str) -> Option<String> {
    let v = fnpm_semver::Version::parse(version.trim()).ok()?;
    Some(v.to_string())
}

/// 1-based line and column of byte `offset`.
pub(crate) fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

impl WorkspaceManifests {
    /// Reads the root package.json and every workspace. Fails with `fnpm install`'s errors.
    pub fn load(out: &Out, root_dir: &Path) -> FResult<Self> {
        let root_package_json = root_dir.join("package.json");
        let ws = match fnpm_workspace::Workspace::load_with(root_dir, &Default::default()) {
            Ok((ws, _warnings)) => ws,
            Err(e) => {
                let mut text = out.error(&e.to_string()).0;
                if let fnpm_workspace::Error::WorkspaceNotFound(entry) = &e
                    && let Ok(source) = std::fs::read_to_string(&root_package_json)
                    && let Some(at) = source.find(&super::json::quote(entry))
                {
                    let (line, col) = line_col(&source, at);
                    text.push_str(&format!(
                        "\n    at {}:{line}:{col}",
                        root_package_json.display()
                    ));
                }
                return Err(super::out::Fatal(text));
            }
        };
        let mut names = HashSet::new();
        let mut paths = HashMap::new();
        let mut versions = HashMap::new();
        for m in &ws.members {
            names.insert(m.name.clone());
            paths.insert(m.path.clone(), m.name.clone());
            if let Some(v) = exact_version(&m.version) {
                versions.insert(m.name.clone(), v);
            }
        }
        if let Some(root_name) = ws.root.name.as_deref().filter(|n| !n.is_empty())
            && !names.contains(root_name)
        {
            names.insert(root_name.to_owned());
            if let Some(v) = ws.root.version.as_deref().and_then(exact_version) {
                versions.insert(root_name.to_owned(), v);
            }
        }
        Ok(Self {
            root_dir: ws.root_dir.clone(),
            root_package_json,
            names,
            paths,
            versions,
            catalogs: ws.settings.catalogs.clone(),
        })
    }

    fn has_workspace(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    fn workspace_version(&self, name: &str) -> Option<&str> {
        self.versions.get(name).map(String::as_str)
    }

    /// The member that `workspace:<path>` in `package_dir` links.
    fn workspace_name_at_path(&self, package_dir: &Path, path: &str) -> Option<&str> {
        if path.is_empty() {
            return None;
        }
        let joined = super::walk::normalize_posix(
            format!("{}/{}", package_dir.to_string_lossy(), path).as_bytes(),
        );
        let root = super::walk::normalize_posix(self.root_dir.to_string_lossy().as_bytes());
        let rel = joined.strip_prefix(root.as_slice())?;
        let rel = rel.strip_prefix(b"/").unwrap_or(rel);
        let mut rel = String::from_utf8_lossy(rel).into_owned();
        while rel.ends_with('/') {
            rel.pop();
        }
        self.paths.get(&rel).map(String::as_str)
    }

    fn root_package_json_path(&self) -> String {
        self.root_package_json.to_string_lossy().into_owned()
    }

    fn catalog_version(&self, catalog: &str, name: &str) -> Option<&str> {
        self.catalogs.get(catalog, name)
    }

    /// Whether `dir` is the project root or one of its workspaces.
    pub(crate) fn contains_package_dir(&self, dir: &Path) -> bool {
        let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let dir = real(dir);
        dir == real(&self.root_dir)
            || self.paths.keys().any(|rel| real(&self.root_dir.join(rel)) == dir)
    }

    /// The version `fnpm pm pack` in `package_dir` publishes for the dependency `name` written
    /// as `spec`, when `spec` is a `workspace:` or `catalog:` version that resolves.
    pub(crate) fn published_spec(&self, package_dir: &Path, name: &str, spec: &str) -> Option<String> {
        published(for_spec(spec)?, self, package_dir, name).ok()
    }
}

/// What replaces a `workspace:` or `catalog:` spec.
enum Substitution<'a> {
    /// `workspace:^`, `workspace:~`, `workspace:*`: the version with that prefix.
    WorkspaceVersion(&'static str),
    /// `workspace:@acme/core@*` (alias), `workspace:../core`, or `workspace:1.x`.
    WorkspaceRangeOrDirectory(&'a str),
    Catalog(&'a str),
}

fn for_spec(spec: &str) -> Option<Substitution<'_>> {
    let spec = spec.trim();
    if let Some(range) = spec.strip_prefix("workspace:") {
        return Some(match range {
            "^" => Substitution::WorkspaceVersion("^"),
            "~" => Substitution::WorkspaceVersion("~"),
            "*" => Substitution::WorkspaceVersion(""),
            _ => Substitution::WorkspaceRangeOrDirectory(range),
        });
    }
    spec.strip_prefix("catalog:")
        .map(|name| Substitution::Catalog(name.trim()))
}

/// Packages without such specs pack whatever state the rest of the workspace is in.
pub(crate) fn needs_workspace_manifests(json: &Json) -> bool {
    GROUPS.iter().any(|group| {
        json.get(group)
            .and_then(Json::as_object)
            .is_some_and(|deps| {
                deps.iter()
                    .any(|(_, v)| v.as_str().and_then(for_spec).is_some())
            })
    })
}

/// `workspace:<name>@<range>`; `<name>` may be scoped, `../@scope/pkg` is a directory.
fn split_workspace_alias(spec: &str) -> Option<(&str, &str)> {
    let at = spec.rfind('@').filter(|&at| at > 0)?;
    let (name, range) = (&spec[..at], &spec[at + 1..]);
    let is_name = match fnpm_types::name::is_scoped_package_name(name) {
        Ok(true) => true,
        Ok(false) => !name.contains('/'),
        Err(_) => false,
    };
    is_name.then_some((name, range))
}

/// The version under the workspace's own name, otherwise the `npm:` alias pnpm publishes.
fn publish_workspace(dependency_name: &str, workspace_name: &str, version: &str) -> String {
    if workspace_name == dependency_name {
        version.to_owned()
    } else {
        format!("npm:{workspace_name}@{version}")
    }
}

fn publish_spec_for_workspace_range_or_directory(
    manifests: &WorkspaceManifests,
    package_dir: &Path,
    dependency_name: &str,
    spec: &str,
) -> Result<String, String> {
    let alias = split_workspace_alias(spec);
    if let Some((workspace_name, range)) = alias.filter(|(name, _)| manifests.has_workspace(name)) {
        let range = range.trim();
        let prefix = match range {
            "" | "*" => Some(""),
            "^" => Some("^"),
            "~" => Some("~"),
            _ => None,
        };
        let Some(prefix) = prefix else {
            return Ok(publish_workspace(dependency_name, workspace_name, range));
        };
        let Some(version) = manifests.workspace_version(workspace_name) else {
            return Err(format!(
                "workspace \"{workspace_name}\" has no version in package.json"
            ));
        };
        return Ok(publish_workspace(
            dependency_name,
            workspace_name,
            &format!("{prefix}{version}"),
        ));
    }
    if let Some(workspace_name) = manifests.workspace_name_at_path(package_dir, spec) {
        let Some(version) = manifests.workspace_version(workspace_name) else {
            return Err(format!(
                "workspace \"{workspace_name}\" in \"{spec}\" has no version in package.json"
            ));
        };
        return Ok(publish_workspace(dependency_name, workspace_name, version));
    }
    if alias.is_none() && manifests.has_workspace(dependency_name) {
        return Ok(spec.to_owned());
    }
    Err(format!(
        "\"{}\" has no workspace named \"{}\" and no workspace in the directory \"{spec}\"",
        manifests.root_package_json_path(),
        alias.map_or(dependency_name, |(name, _)| name),
    ))
}

fn published(
    sub: Substitution<'_>,
    manifests: &WorkspaceManifests,
    package_dir: &Path,
    dependency_name: &str,
) -> Result<String, (&'static str, String)> {
    match sub {
        Substitution::WorkspaceVersion(prefix) => {
            match manifests.workspace_version(dependency_name) {
                Some(version) => Ok(format!("{prefix}{version}")),
                None => Err((
                    "workspace",
                    format!(
                        "\"{}\" has no workspace named \"{dependency_name}\", or its package.json has no version",
                        manifests.root_package_json_path()
                    ),
                )),
            }
        }
        Substitution::WorkspaceRangeOrDirectory(spec) => {
            publish_spec_for_workspace_range_or_directory(
                manifests,
                package_dir,
                dependency_name,
                spec,
            )
            .map_err(|why| ("workspace", why))
        }
        Substitution::Catalog(catalog) => {
            let spec = manifests
                .catalog_version(catalog, dependency_name)
                .ok_or(("catalog", "no matching catalog dependency".to_owned()))?;
            match for_spec(spec) {
                None => Ok(spec.to_owned()),
                Some(Substitution::Catalog(_)) => Err((
                    "catalog",
                    "catalog entry refers to another catalog".to_owned(),
                )),
                Some(found) => published(found, manifests, package_dir, dependency_name),
            }
        }
    }
}

/// Replaces the `workspace:` and `catalog:` specs in `json` (a package in `package_dir`).
pub(crate) fn substitute(
    out: &Out,
    manifests: Option<&WorkspaceManifests>,
    package_dir: &Path,
    json: &mut Json,
) -> FResult<()> {
    for group in GROUPS {
        let Some(Json::Object(deps)) = json.get_mut(group) else {
            continue;
        };
        for (name, value) in deps.iter_mut() {
            let Some(sub) = value.as_str().and_then(for_spec) else {
                continue;
            };
            let Some(manifests) = manifests else {
                continue;
            };
            match published(sub, manifests, package_dir, name) {
                Ok(replacement) => *value = Json::String(replacement),
                Err((what, why)) => {
                    return Err(out.error(&format!(
                        "Failed to resolve {what} version for \"{name}\" in `{group}` ({why})."
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases() {
        assert_eq!(
            split_workspace_alias("@acme/core@*"),
            Some(("@acme/core", "*"))
        );
        assert_eq!(split_workspace_alias("pkg1@1.x"), Some(("pkg1", "1.x")));
        assert_eq!(split_workspace_alias("../@scoped/thing"), None);
        assert_eq!(split_workspace_alias("1.2.3"), None);
        assert_eq!(publish_workspace("a", "b", "1.0.0"), "npm:b@1.0.0");
        assert_eq!(publish_workspace("b", "b", "^1.0.0"), "^1.0.0");
    }
}
