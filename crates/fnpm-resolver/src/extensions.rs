//! `packageExtensions` (bun `install/lockfile/package_extensions.rs`): add dependencies,
//! optional dependencies and peers to registry packages, or make their peers optional.
//!
//! Keys are `name` or `name@range` (the range matches prereleases too, like yarn). Values may use
//! versions, tags, git repositories, GitHub shorthands and remote tarball URLs; a path would be read
//! against the extended package, so it is refused. For one name in one package the first matching
//! extension wins, but any of them can make a peer optional. A package's own dependency always
//! wins over an extension's.

use std::collections::BTreeMap;

use fnpm_semver::{Range, SatisfyOptions, Version};
use fnpm_types::overrides::parse_package_segment;
use fnpm_types::spec::{infer_tag, is_remote_tarball, trim_literal};
use fnpm_types::{Behavior, DepEdge, DependencySpec, PackageExtension, PeerMeta, SpecTag};
use serde_json::Value;

/// One parsed extension.
#[derive(Debug, Clone)]
pub struct ExtensionRule {
    /// The key as written (`name` or `name@range`).
    pub key: String,
    /// The package the rule extends.
    pub name: String,
    range: Option<Range>,
    edges: Vec<ExtensionEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtensionEdge {
    name: String,
    literal: String,
    /// `PROD`, `OPTIONAL`, `PEER` or `PEER | OPTIONAL`.
    behavior: Behavior,
}

const GROUPS: [(&str, Behavior); 3] = [
    ("dependencies", Behavior::PROD),
    ("optionalDependencies", Behavior::OPTIONAL),
    ("peerDependencies", Behavior::PEER),
];

fn is_supported_specifier(name: &str, literal: &str) -> bool {
    let trimmed = trim_literal(literal);
    match infer_tag(trimmed) {
        SpecTag::Npm | SpecTag::DistTag | SpecTag::Git | SpecTag::Github => {}
        SpecTag::Tarball if is_remote_tarball(trimmed) => {}
        _ => return false,
    }
    DependencySpec::parse(name, literal).is_ok()
}

impl ExtensionRule {
    fn parse(key: &str, value: &Value) -> Result<(Self, PackageExtension), &'static str> {
        let Ok(selector) = parse_package_segment(key) else {
            return Err("expected \"name\" or \"name@range\"");
        };
        let range = if selector.range.is_empty() {
            None
        } else {
            // node-semver `includePrerelease` desugaring, as yarn matches the keys.
            match Range::parse_including_prerelease(&selector.range) {
                Ok(r) => Some(r),
                Err(_) => return Err("expected a semver range after the \"@\""),
            }
        };
        let Some(object) = value.as_object() else {
            return Err("expected an object");
        };
        let mut edges: Vec<ExtensionEdge> = Vec::new();
        let mut extension = PackageExtension::default();
        for (group, behavior) in GROUPS {
            let Some(rows) = object.get(group) else {
                continue;
            };
            let Some(rows) = rows.as_object() else {
                return Err("expected a map of specifiers");
            };
            for (name, literal) in rows {
                let Some(literal) = literal.as_str() else {
                    return Err("expected a map of specifiers");
                };
                if name.is_empty() || !is_supported_specifier(name, literal) {
                    return Err(
                        "expected a version, a tag, a git repository or a tarball URL for every dependency",
                    );
                }
                let edge = ExtensionEdge {
                    name: name.clone(),
                    literal: literal.to_owned(),
                    behavior,
                };
                match edges
                    .iter_mut()
                    .find(|o| o.name == edge.name && o.behavior.is_peer() == behavior.is_peer())
                {
                    Some(other) => *other = edge,
                    None => edges.push(edge),
                }
                let target = match group {
                    "dependencies" => &mut extension.dependencies,
                    "optionalDependencies" => &mut extension.optional_dependencies,
                    _ => &mut extension.peer_dependencies,
                };
                target.insert(name.clone(), literal.to_owned());
            }
        }
        if let Some(meta) = object
            .get("peerDependenciesMeta")
            .and_then(Value::as_object)
        {
            for (name, peer_meta) in meta {
                if peer_meta.get("optional").and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                extension
                    .peer_dependencies_meta
                    .insert(name.clone(), PeerMeta { optional: true });
                match edges
                    .iter_mut()
                    .find(|e| &e.name == name && e.behavior.is_peer())
                {
                    Some(peer) => peer.behavior |= Behavior::OPTIONAL,
                    None => edges.push(ExtensionEdge {
                        name: name.clone(),
                        literal: "*".into(),
                        behavior: Behavior::PEER | Behavior::OPTIONAL,
                    }),
                }
            }
        }
        Ok((
            Self {
                key: key.to_owned(),
                name: selector.name,
                range,
                edges,
            },
            extension,
        ))
    }

    fn matches(&self, name: &str, version: &Version) -> bool {
        self.name == name
            && self.range.as_ref().is_none_or(|r| {
                r.satisfies_with(
                    version,
                    SatisfyOptions {
                        include_prerelease: true,
                    },
                )
            })
    }
}

/// Whether `rules` extend the packages of `prev` the way `prev` records it. fnpm.lock (bun.lock)
/// keeps only what the rules added to each installed package, keyed by its exact version, so the
/// rules of package.json are compared by what they do: every registry package a rule matches has
/// the rule's dependencies, and the record of it names the same ones.
pub fn applied_alike(rules: &[ExtensionRule], prev: &fnpm_types::LockGraph) -> bool {
    use std::collections::BTreeSet;
    let Ok(recorded) = serde_json::to_value(&prev.package_extensions) else {
        return false;
    };
    let (recorded, _, _) =
        parse_package_extensions(&serde_json::json!({ "packageExtensions": recorded }));
    // `(name, peer, optional peer)` of what the rules matching `name@version` add.
    let added = |rules: &[ExtensionRule], name: &str, version: &Version| {
        rules
            .iter()
            .filter(|r| r.matches(name, version))
            .flat_map(|r| &r.edges)
            .map(|e| {
                (
                    e.name.clone(),
                    e.behavior.is_peer(),
                    e.behavior.is_optional_peer(),
                )
            })
            .collect::<BTreeSet<_>>()
    };
    prev.packages.iter().all(|p| {
        let fnpm_types::Resolution::Npm { version, .. } = &p.resolution else {
            return true;
        };
        let Ok(version) = Version::parse(version) else {
            return true;
        };
        let wanted = added(rules, &p.name, &version);
        // The record names only edges a rule added, so each has the rule's specifier.
        let specifier = |name: &str, peer: bool| {
            rules
                .iter()
                .filter(|r| r.matches(&p.name, &version))
                .flat_map(|r| &r.edges)
                .find(|e| e.name == name && e.behavior.is_peer() == peer)
                .map(|e| e.literal.as_str())
        };
        wanted == added(&recorded, &p.name, &version)
            && wanted.iter().all(|(name, peer, _)| {
                p.dependencies.iter().any(|e| {
                    &e.name == name
                        && e.behavior.is_peer() == *peer
                        && (*peer || specifier(name, *peer) == Some(e.literal.as_str()))
                })
            })
    })
}

/// Parses root `packageExtensions`. Returns the rules, the lockfile form (valid rules only) and
/// warnings for ignored entries.
pub fn parse_package_extensions(
    root: &Value,
) -> (
    Vec<ExtensionRule>,
    BTreeMap<String, PackageExtension>,
    Vec<String>,
) {
    let mut rules = Vec::new();
    let mut recorded = BTreeMap::new();
    let mut warnings = Vec::new();
    let Some(field) = root.get("packageExtensions") else {
        return (rules, recorded, warnings);
    };
    let Some(object) = field.as_object() else {
        warnings.push("\"packageExtensions\" expects an object".to_owned());
        return (rules, recorded, warnings);
    };
    for (key, value) in object {
        match ExtensionRule::parse(key, value) {
            Ok((rule, extension)) => {
                recorded.insert(key.clone(), extension);
                rules.push(rule);
            }
            Err(problem) => warnings.push(format!(
                "Ignored \"{key}\" in \"packageExtensions\": {problem}"
            )),
        }
    }
    (rules, recorded, warnings)
}

struct Wanted<'a> {
    edge: &'a ExtensionEdge,
    behavior: Behavior,
    found: bool,
}

fn wanted_for<'a>(rules: &'a [ExtensionRule], name: &str, version: &Version) -> Vec<Wanted<'a>> {
    let mut wanted: Vec<Wanted<'a>> = Vec::new();
    for rule in rules.iter().filter(|r| r.matches(name, version)) {
        for edge in &rule.edges {
            match wanted.iter_mut().find(|w| {
                w.edge.name == edge.name && w.behavior.is_peer() == edge.behavior.is_peer()
            }) {
                Some(first) => {
                    if edge.behavior.is_optional_peer() {
                        first.behavior |= Behavior::OPTIONAL;
                    }
                }
                None => wanted.push(Wanted {
                    edge,
                    behavior: edge.behavior,
                    found: false,
                }),
            }
        }
    }
    wanted
}

/// Whether any rule applies to `name@version` (used to decide if a locked package must be
/// rebuilt from its manifest after the rules changed).
pub fn rules_matching<'a>(
    rules: &'a [ExtensionRule],
    name: &str,
    version: &Version,
) -> Vec<&'a str> {
    rules
        .iter()
        .filter(|r| r.matches(name, version))
        .map(|r| r.key.as_str())
        .collect()
}

/// Applies the rules to the edges of a newly created registry package `name@version`. Returns
/// true if anything changed.
pub fn apply(
    rules: &[ExtensionRule],
    name: &str,
    version: &Version,
    edges: &mut Vec<DepEdge>,
) -> bool {
    let mut wanted = wanted_for(rules, name, version);
    if wanted.is_empty() {
        return false;
    }
    let mut changed = false;
    for edge in edges.iter_mut() {
        let Some(rule) = wanted
            .iter_mut()
            .find(|w| w.edge.name == edge.name && w.behavior.is_peer() == edge.behavior.is_peer())
        else {
            continue;
        };
        rule.found = true;
        // The package's own row wins. All a rule can do to it is make a peer optional.
        if edge.behavior.is_peer()
            && rule.behavior.contains(Behavior::OPTIONAL)
            && !edge.behavior.contains(Behavior::OPTIONAL)
        {
            edge.behavior |= Behavior::OPTIONAL;
            changed = true;
        }
    }
    for rule in wanted.iter().filter(|w| !w.found) {
        if edges.iter().any(|e| e.name == rule.edge.name) {
            continue;
        }
        edges.push(DepEdge::new(
            rule.edge.name.clone(),
            rule.edge.literal.clone(),
            rule.behavior,
        ));
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edges(list: &[(&str, &str, Behavior)]) -> Vec<DepEdge> {
        list.iter()
            .map(|(n, l, b)| DepEdge::new(*n, *l, *b))
            .collect()
    }

    #[test]
    fn first_extension_wins_any_can_make_a_peer_optional() {
        let (rules, recorded, warnings) = parse_package_extensions(&json!({"packageExtensions": {
            "x": {"dependencies": {"a": "^1"}, "peerDependencies": {"p": "^1"}},
            "x@^1": {"dependencies": {"a": "^2"}, "peerDependenciesMeta": {"p": {"optional": true}}},
            "x@^2": {"dependencies": {"z": "1"}}
        }}));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(recorded.len(), 3);
        let mut e = edges(&[]);
        assert!(apply(
            &rules,
            "x",
            &Version::parse("1.0.0").unwrap(),
            &mut e
        ));
        let a = e.iter().find(|e| e.name == "a").unwrap();
        assert_eq!(a.literal, "^1");
        let p = e.iter().find(|e| e.name == "p").unwrap();
        assert!(p.behavior.is_optional_peer());
        assert!(!e.iter().any(|e| e.name == "z"));
        assert_eq!(
            rules_matching(&rules, "x", &Version::parse("1.1.0-beta").unwrap()),
            ["x", "x@^1"]
        );
    }

    #[test]
    fn own_rows_win() {
        let (rules, _, _) = parse_package_extensions(
            &json!({"packageExtensions": {"x": {"dependencies": {"a": "^9"}, "optionalDependencies": {"b": "1"}}}}),
        );
        let mut e = edges(&[("a", "^1", Behavior::PROD), ("b", "^1", Behavior::PEER)]);
        apply(&rules, "x", &Version::parse("1.0.0").unwrap(), &mut e);
        assert_eq!(e.iter().find(|e| e.name == "a").unwrap().literal, "^1");
        // A non-peer extension of a name the package has as a peer adds nothing (one row per name).
        assert_eq!(e.len(), 2);
        assert!(!apply(
            &rules,
            "y",
            &Version::parse("1.0.0").unwrap(),
            &mut e
        ));
    }

    #[test]
    fn allowed_specifiers() {
        assert!(is_supported_specifier("a", "^1"));
        assert!(is_supported_specifier("a", "latest"));
        assert!(is_supported_specifier("a", "github:o/r"));
        assert!(is_supported_specifier("a", "https://x.test/a.tgz"));
        assert!(!is_supported_specifier("a", "file:./a"));
        assert!(!is_supported_specifier("a", "./a.tgz"));
        assert!(!is_supported_specifier("a", "workspace:*"));
        assert!(!is_supported_specifier("a", "link:../a"));
    }
}
