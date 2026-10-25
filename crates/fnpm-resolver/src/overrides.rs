//! Override matching (bun `install/lockfile/OverrideMap.rs`): which [`OverrideRule`] applies to
//! an edge.
//!
//! Rules come from the root `package.json` `overrides` (npm; nested objects one level deep) or,
//! when that field is absent, `resolutions` (yarn), plus `pnpm-workspace.yaml` `overrides`.
//! A rule is *flat* (`name`) or *scoped* (`parent>child`, `parent@range>child`,
//! `child@range`). Precedence for one edge: ranged parent > parent > no parent; within a tier a
//! matching target range wins (+1); ties go to the parent range text, then the target range text,
//! that sorts first; then the flat rule. A flat rule never applies to an `npm:` alias; only a key
//! that spells the alias out (`name@npm:other@^1`) does.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use fnpm_semver::{Range, Version};
use fnpm_types::overrides::parse_package_segment;
use fnpm_types::{
    DependencySpec, OverrideRule, OverrideSelector, Package, PackageSelector, SelectorError,
};
use fnpm_workspace::PackageJson;
use serde_json::Value;

use crate::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Overrides,
    Resolutions,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Overrides => "override",
            Field::Resolutions => "resolution",
        }
    }
    fn json_name(self) -> &'static str {
        match self {
            Field::Overrides => "overrides",
            Field::Resolutions => "resolutions",
        }
    }
    fn missing_name_message(self) -> &'static str {
        match self {
            Field::Overrides => "Missing overridden package name",
            Field::Resolutions => "Missing resolution package name",
        }
    }
}

/// Parses the root package.json `overrides` / `resolutions`. `$name` references are resolved
/// against the root's declared specs only; use [`parse_overrides_with`] to also consult
/// workspace members. A field that is not an object yields no rules and a warning.
pub fn parse_overrides(root: &Value) -> (Vec<OverrideRule>, Vec<String>) {
    match parse_overrides_with(root, &[], &[]) {
        Ok(parsed) => parsed,
        Err(e) => (Vec::new(), vec![e.to_string()]),
    }
}

/// Like [`parse_overrides`], with workspace members for `$name` references and the
/// `pnpm-workspace.yaml` `overrides` (applied first, so package.json rules replace them). A field
/// that is not an object is an error, as in bun: installing with no rule applied would hide the
/// typo.
pub fn parse_overrides_with(
    root: &Value,
    members: &[&PackageJson],
    pnpm_overrides: &[(String, String)],
) -> Result<(Vec<OverrideRule>, Vec<String>)> {
    let mut parser = Parser {
        field: Field::Overrides,
        root,
        members,
        rules: Vec::new(),
        warnings: Vec::new(),
    };
    for (key, value) in pnpm_overrides {
        parser.string_rule(key, value);
    }
    let field = if root.get("overrides").is_some() {
        Some(Field::Overrides)
    } else if root.get("resolutions").is_some() {
        Some(Field::Resolutions)
    } else {
        None
    };
    if let Some(field) = field {
        parser.field = field;
        let rules = &root[field.json_name()];
        let Some(object) = rules.as_object() else {
            let name = field.json_name();
            return Err(Error::InvalidPackageJson {
                path: "package.json".into(),
                message: format!(
                    "\"{name}\" expects a map of package names to versions, e.g.\n  \"{name}\": {{\n    \"react\": \"18.2.0\"\n  }}"
                ),
            });
        };
        for (key, value) in object {
            parser.property(key, value);
        }
    }
    Ok((parser.rules, parser.warnings))
}

struct Parser<'a> {
    field: Field,
    root: &'a Value,
    members: &'a [&'a PackageJson],
    rules: Vec<OverrideRule>,
    warnings: Vec<String>,
}

fn is_comment_key(key: &str) -> bool {
    key.starts_with("//")
}

impl Parser<'_> {
    fn property(&mut self, key: &str, value: &Value) {
        if key.is_empty() {
            self.warnings.push(self.field.missing_name_message().into());
            return;
        }
        if is_comment_key(key) {
            return;
        }
        if let Some(text) = value.as_str() {
            self.string_rule(key, text);
            return;
        }
        let Some(children) = value.as_object().filter(|_| self.field == Field::Overrides) else {
            self.warnings.push(format!(
                "Invalid {} value for \"{key}\"",
                self.field.label()
            ));
            return;
        };
        let parent = match parse_package_segment(key) {
            Ok(parent) => parent,
            Err(e) => return self.selector_error(key, e),
        };
        if !self.valid_parent(&parent) {
            return;
        }
        for (child_key, child_value) in children {
            if is_comment_key(child_key) {
                continue;
            }
            if child_value.is_object() {
                self.selector_error(child_key, SelectorError::TooDeep);
                continue;
            }
            let Some(child_text) = child_value.as_str() else {
                self.warnings.push(format!(
                    "Invalid {} value for \"{child_key}\"",
                    self.field.label()
                ));
                continue;
            };
            if child_key == "." {
                self.put_rule(None, parent.clone(), child_text);
                continue;
            }
            match OverrideSelector::parse(child_key) {
                Ok(Some(OverrideSelector {
                    parent: None,
                    target,
                })) => self.put_rule(Some(parent.clone()), target, child_text),
                Ok(Some(_)) => self.selector_error(child_key, SelectorError::TooDeep),
                Ok(None) => {}
                Err(e) => self.selector_error(child_key, e),
            }
        }
    }

    fn string_rule(&mut self, key: &str, value: &str) {
        if key.is_empty() {
            self.warnings.push(self.field.missing_name_message().into());
            return;
        }
        match OverrideSelector::parse(key) {
            Ok(Some(selector)) => {
                if let Some(parent) = &selector.parent
                    && !self.valid_parent(parent)
                {
                    return;
                }
                self.put_rule(selector.parent, selector.target, value);
            }
            Ok(None) => {}
            Err(e) => self.selector_error(key, e),
        }
    }

    fn selector_error(&mut self, key: &str, e: SelectorError) {
        let message = match e {
            SelectorError::EmptyName => self.field.missing_name_message().to_owned(),
            SelectorError::InvalidName => format!("invalid package name \"{key}\""),
            SelectorError::TooDeep => {
                format!(
                    "fnpm currently only supports one level of nested \"{}\"",
                    self.field.json_name()
                )
            }
            SelectorError::EmptyRange => format!("Missing version range after \"{key}\""),
        };
        self.warnings.push(message);
    }

    /// A parent is selected by its installed version: only a plain npm range is allowed.
    fn valid_parent(&mut self, parent: &PackageSelector) -> bool {
        if parent.range.is_empty() || is_selector_range(&parent.name, &parent.range, false) {
            return true;
        }
        self.warnings.push(format!(
            "Invalid version range \"{}\" for \"{}\"",
            parent.range, parent.name
        ));
        false
    }

    fn put_rule(&mut self, parent: Option<PackageSelector>, target: PackageSelector, value: &str) {
        if value.starts_with("patch:") {
            self.warnings.push(format!(
                "fnpm currently does not support patched package \"{}\"",
                self.field.json_name()
            ));
            return;
        }
        if value.is_empty() {
            self.warnings
                .push(format!("Missing {} value", self.field.label()));
            return;
        }
        if value == "-" {
            self.warnings.push(format!(
                "Removing \"{}\" with \"-\" is not supported",
                target.name
            ));
            return;
        }
        let literal = match value.strip_prefix('$') {
            Some(reference) => match self.reference(reference) {
                Ok(Some(literal)) => literal,
                Ok(None) => {
                    self.warnings.push(format!(
                        "Could not resolve \"{value}\": \"{reference}\" is not in dependencies"
                    ));
                    return;
                }
                Err(()) => {
                    self.warnings.push(format!(
                        "Could not resolve \"{value}\": workspaces declare different versions of \"{reference}\""
                    ));
                    return;
                }
            },
            None => value.to_owned(),
        };
        if DependencySpec::parse(&target.name, &literal).is_err() {
            self.warnings
                .push(format!("Invalid {} value \"{value}\"", self.field.label()));
            return;
        }
        if !target.range.is_empty() && !is_selector_range(&target.name, &target.range, true) {
            self.warnings.push(format!(
                "Invalid version range \"{}\" for \"{}\"",
                target.range, target.name
            ));
            return;
        }
        let rule = OverrideRule {
            selector: OverrideSelector { parent, target },
            value: literal,
        };
        let same = |r: &OverrideRule| r.selector == rule.selector;
        match self.rules.iter_mut().find(|r| same(r)) {
            Some(existing) => *existing = rule,
            None => self.rules.push(rule),
        }
    }

    /// `$name`: the spec the root declares for `name` as written, else the one the workspaces
    /// agree on. `Err` when they disagree.
    fn reference(&self, name: &str) -> std::result::Result<Option<String>, ()> {
        for group in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ] {
            if let Some(v) = self
                .root
                .get(group)
                .and_then(|g| g.get(name))
                .and_then(Value::as_str)
            {
                return Ok(Some(v.to_owned()));
            }
        }
        let mut found: Option<String> = None;
        for member in self.members {
            let declared = fnpm_types::DependencyGroup::ALL.iter().find_map(|g| {
                member
                    .group(*g)
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            });
            let Some(literal) = declared else { continue };
            match &found {
                None => found = Some(literal),
                Some(first) if *first == literal => {}
                Some(_) => return Err(()),
            }
        }
        Ok(found)
    }
}

/// A target is selected by how it is declared (npm range or `catalog:`); a parent by its
/// installed version (a plain, non-alias npm range).
fn is_selector_range(name: &str, range: &str, is_target: bool) -> bool {
    match DependencySpec::parse(name, range) {
        Ok(DependencySpec::Npm {
            range: r, is_alias, ..
        }) => (is_target || !is_alias) && (r.is_empty() || Range::parse(&r).is_ok()),
        Ok(DependencySpec::Catalog { .. }) => is_target,
        _ => false,
    }
}

/// The package that declares an edge, as overrides see it.
#[derive(Debug, Clone, Copy)]
pub struct Owner<'a> {
    /// The declaring package's name.
    pub name: &'a str,
    /// The npm (or workspace) version; `None` for the root and non-registry packages.
    pub version: Option<&'a Version>,
}

struct Scoped {
    rule: usize,
    parent: Option<(String, Option<Range>, String)>,
    /// The parsed `name@<range>` part of the key and its text.
    target: Option<(DependencySpec, String)>,
}

/// The rules indexed for lookup during a resolve.
pub(crate) struct OverrideSet {
    rules: Vec<OverrideRule>,
    flat: HashMap<String, usize>,
    scoped: Vec<Scoped>,
    scoped_names: HashSet<String>,
}

fn same_catalog(a: &str, b: &str) -> bool {
    let is_default = |n: &str| n.is_empty() || n == "default";
    a == b || (is_default(a) && is_default(b))
}

fn is_npm_alias(spec: &DependencySpec) -> bool {
    matches!(spec, DependencySpec::Npm { is_alias: true, .. })
}

/// npm and pnpm select a `name@<range>` key when the ranges overlap.
fn range_selects(range: &DependencySpec, declared: &DependencySpec) -> bool {
    match (range, declared) {
        (
            DependencySpec::Npm {
                name: rn,
                range: rr,
                is_alias: ra,
            },
            DependencySpec::Npm {
                name: dn,
                range: dr,
                is_alias: da,
            },
        ) => {
            if ra != da || (*ra && rn != dn) {
                return false;
            }
            match (Range::parse(rr), Range::parse(dr)) {
                (Ok(a), Ok(b)) => a.intersects(&b),
                _ => false,
            }
        }
        (DependencySpec::Catalog { catalog: a }, DependencySpec::Catalog { catalog: b }) => {
            same_catalog(a, b)
        }
        _ => false,
    }
}

impl OverrideSet {
    pub(crate) fn new(rules: &[OverrideRule]) -> Self {
        let mut set = Self {
            rules: rules.to_vec(),
            flat: HashMap::new(),
            scoped: Vec::new(),
            scoped_names: HashSet::new(),
        };
        for (i, rule) in rules.iter().enumerate() {
            let OverrideSelector { parent, target } = &rule.selector;
            if parent.is_none() && target.range.is_empty() {
                set.flat.insert(target.name.clone(), i);
                continue;
            }
            let parent = parent.as_ref().map(|p| {
                let range = if p.range.is_empty() {
                    None
                } else {
                    Range::parse(&p.range).ok()
                };
                (p.name.clone(), range, p.range.clone())
            });
            let target_spec = if target.range.is_empty() {
                None
            } else {
                DependencySpec::parse(&target.name, &target.range)
                    .ok()
                    .map(|s| (s, target.range.clone()))
            };
            set.scoped_names.insert(target.name.clone());
            set.scoped.push(Scoped {
                rule: i,
                parent,
                target: target_spec,
            });
        }
        set
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// A plain rule for `name` exists (then every edge of that name is root-authored).
    pub(crate) fn has_flat(&self, name: &str) -> bool {
        self.flat.contains_key(name)
    }

    pub(crate) fn has_rule_for(&self, name: &str) -> bool {
        self.flat.contains_key(name) || self.scoped_names.contains(name)
    }

    /// The rule for edge `name` declared as `declared` by `owner` (`None` when the declarer is
    /// unknown).
    pub(crate) fn lookup(
        &self,
        owner: Option<Owner<'_>>,
        name: &str,
        declared: &DependencySpec,
    ) -> Option<&OverrideRule> {
        if !self.scoped.is_empty() && self.scoped_names.contains(name) {
            let mut best: Option<(&Scoped, u8)> = None;
            for rule in &self.scoped {
                if self.rules[rule.rule].selector.target.name != name {
                    continue;
                }
                if rule.target.is_none() && is_npm_alias(declared) {
                    continue;
                }
                let mut tier = 0u8;
                if let Some((parent_name, parent_range, parent_text)) = &rule.parent {
                    let Some(owner) = owner else { continue };
                    if owner.name != parent_name {
                        continue;
                    }
                    if !parent_text.is_empty() {
                        let (Some(range), Some(version)) = (parent_range, owner.version) else {
                            continue;
                        };
                        if !range.satisfies(version) {
                            continue;
                        }
                        tier = 4;
                    } else {
                        tier = 2;
                    }
                }
                if let Some((range, _)) = &rule.target {
                    if best.is_some_and(|(_, t)| t > tier + 1) {
                        continue;
                    }
                    if !range_selects(range, declared) {
                        continue;
                    }
                    tier += 1;
                }
                let replace = match best {
                    None => true,
                    Some((b, bt)) => {
                        tier > bt || (tier == bt && self.cmp_range_text(rule, b) == Ordering::Less)
                    }
                };
                if replace {
                    best = Some((rule, tier));
                }
            }
            if let Some((rule, _)) = best {
                return Some(&self.rules[rule.rule]);
            }
        }
        if is_npm_alias(declared) {
            return None;
        }
        self.flat.get(name).map(|&i| &self.rules[i])
    }

    fn cmp_range_text(&self, l: &Scoped, r: &Scoped) -> Ordering {
        let by_parent = match (&l.parent, &r.parent) {
            (Some((_, _, a)), Some((_, _, b))) if !a.is_empty() && !b.is_empty() => a.cmp(b),
            _ => Ordering::Equal,
        };
        let text = |s: &Scoped| s.target.as_ref().map_or("", |(_, t)| t.as_str()).to_owned();
        by_parent.then_with(|| text(l).cmp(&text(r)))
    }
}

/// The rule for dependency `name` declared as `declared` by `parent` (`None` for an unknown
/// declarer). See the module docs for the precedence.
pub fn find<'a>(
    rules: &'a [OverrideRule],
    parent: Option<&Package>,
    name: &str,
    declared: &str,
) -> Option<&'a OverrideRule> {
    let declared = DependencySpec::parse(name, declared).ok()?;
    let version = parent.and_then(|p| match &p.resolution {
        fnpm_types::Resolution::Npm { version, .. } => Version::parse(version).ok(),
        fnpm_types::Resolution::Workspace { .. } => Version::parse(&p.version).ok(),
        _ => None,
    });
    let owner = parent.map(|p| Owner {
        name: &p.name,
        version: version.as_ref(),
    });
    let set = OverrideSet::new(rules);
    let index = set
        .lookup(owner, name, &declared)
        .map(|r| rules.iter().position(|x| x == r))?;
    index.map(|i| &rules[i])
}
