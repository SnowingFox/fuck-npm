//! Override / resolution keys (`bun/src/install/lockfile/override_selector.rs`).
//!
//! Key forms: `name`, `name@range`, pnpm `parent>child` and `parent@range>child@range`, yarn
//! `parent/child`, `**/child`, `@scope/a/@scope/b`, and `name@npm:@scope/other@^1`. Matching
//! (which parent, which range) is the resolver's job; this module only parses.

use serde::{Deserialize, Serialize};

use crate::name::split_name_and_maybe_version;
use crate::spec::{SpecTag, infer_tag};

/// One side of a selector.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PackageSelector {
    pub name: String,
    /// Empty when the rule applies to every version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub range: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OverrideSelector {
    /// `Some` for scoped rules (`parent>child`): only applies under that parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<PackageSelector>,
    pub target: PackageSelector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelectorError {
    #[error("Missing overridden package name")]
    EmptyName,
    #[error("invalid package name")]
    InvalidName,
    #[error("only one level of nested overrides is supported")]
    TooDeep,
    #[error("Missing version range after '@'")]
    EmptyRange,
}

/// An override rule: every dependency matched by `selector` uses `value` instead of its own
/// specifier. `value` is any dependency specifier, already resolved from `$name` references.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OverrideRule {
    pub selector: OverrideSelector,
    pub value: String,
}

fn is_bare_scope(name: &str) -> bool {
    name.starts_with('@') && !name.contains('/')
}

pub fn parse_package_segment(segment: &str) -> Result<PackageSelector, SelectorError> {
    if segment.is_empty() {
        return Err(SelectorError::EmptyName);
    }
    let (name, range) = split_name_and_maybe_version(segment);
    if name.is_empty() {
        return Err(SelectorError::EmptyName);
    }
    if is_bare_scope(name) {
        return Err(SelectorError::InvalidName);
    }
    let range = range.unwrap_or("");
    // `npm:^1` is the range `^1`; `npm:other@^1` is an alias and keeps the prefix.
    let range = match range.strip_prefix("npm:") {
        Some(rest) if rest.is_empty() || infer_tag(rest) == SpecTag::Npm => rest,
        _ => range,
    };
    if range.is_empty() && segment.len() > name.len() {
        return Err(SelectorError::EmptyRange);
    }
    Ok(PackageSelector {
        name: name.to_owned(),
        range: range.to_owned(),
    })
}

/// pnpm's `parent>child` delimiter (`/[^ |@]>/`).
fn pnpm_delimiter(key: &str) -> Option<usize> {
    let b = key.as_bytes();
    let mut from = 1;
    while from < b.len() {
        let i = from + key[from..].find('>')?;
        match b[i - 1] {
            b' ' | b'|' | b'@' | b':' => from = i + 1,
            _ => return Some(i),
        }
    }
    None
}

fn next_token(key: &str, pos: usize) -> (&str, Option<usize>) {
    match key[pos..].find('/') {
        Some(i) => (&key[pos..pos + i], Some(pos + i + 1)),
        None => (&key[pos..], None),
    }
}

fn parse_yarn_path(key: &str) -> Result<OverrideSelector, SelectorError> {
    let mut segments: Vec<&str> = Vec::with_capacity(2);
    let mut cursor = Some(0usize);
    while let Some(start) = cursor {
        let (token, next) = next_token(key, start);
        cursor = next;
        if token.is_empty() {
            return Err(SelectorError::EmptyName);
        }
        if token == "**" {
            if next.is_none() {
                return Err(SelectorError::EmptyName);
            }
            continue;
        }
        let mut end = start + token.len();
        if token.starts_with('@') {
            let Some(name_start) = next else {
                return Err(SelectorError::InvalidName);
            };
            let (name, after) = next_token(key, name_start);
            cursor = after;
            if name.is_empty() {
                return Err(SelectorError::EmptyName);
            }
            end = name_start + name.len();
        }
        if key[start..end].contains("@npm:@")
            && let Some(rest_start) = cursor
        {
            let (rest, after) = next_token(key, rest_start);
            cursor = after;
            end = rest_start + rest.len();
        }
        if segments.len() == 2 {
            return Err(SelectorError::TooDeep);
        }
        segments.push(&key[start..end]);
    }
    match segments.as_slice() {
        [] => Err(SelectorError::EmptyName),
        [one] => Ok(OverrideSelector {
            parent: None,
            target: parse_package_segment(one)?,
        }),
        [parent, target, ..] => Ok(OverrideSelector {
            parent: Some(parse_package_segment(parent)?),
            target: parse_package_segment(target)?,
        }),
    }
}

impl OverrideSelector {
    /// Parses an `overrides` / `resolutions` key. Keys starting with `//` are comments and
    /// give `Ok(None)`.
    pub fn parse(key: &str) -> Result<Option<Self>, SelectorError> {
        if key.starts_with("//") {
            return Ok(None);
        }
        if let Some(d) = pnpm_delimiter(key) {
            let parent = key[..d].trim();
            let name = key[d + 1..].trim();
            if name.is_empty() {
                return Err(SelectorError::EmptyName);
            }
            if pnpm_delimiter(name).is_some() {
                return Err(SelectorError::TooDeep);
            }
            let parent = parse_package_segment(parent)?;
            return Ok(Some(Self {
                parent: Some(parent),
                target: parse_package_segment(name)?,
            }));
        }
        parse_yarn_path(key).map(Some)
    }

    /// The canonical key fnpm.lock writes: `name`, `name@range`, `parent>child`.
    pub fn to_key(&self) -> String {
        let seg = |s: &PackageSelector| {
            if s.range.is_empty() {
                s.name.clone()
            } else {
                format!("{}@{}", s.name, s.range)
            }
        };
        match &self.parent {
            Some(p) => format!("{}>{}", seg(p), seg(&self.target)),
            None => seg(&self.target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(key: &str) -> OverrideSelector {
        OverrideSelector::parse(key).unwrap().unwrap()
    }

    fn ps(name: &str, range: &str) -> PackageSelector {
        PackageSelector {
            name: name.into(),
            range: range.into(),
        }
    }

    #[test]
    fn flat() {
        assert_eq!(
            sel("lodash"),
            OverrideSelector {
                parent: None,
                target: ps("lodash", "")
            }
        );
        assert_eq!(
            sel("lodash@^4"),
            OverrideSelector {
                parent: None,
                target: ps("lodash", "^4")
            }
        );
        assert_eq!(
            sel("@s/a@1"),
            OverrideSelector {
                parent: None,
                target: ps("@s/a", "1")
            }
        );
        assert_eq!(sel("a@npm:^1").target, ps("a", "^1"));
        assert_eq!(sel("a@npm:1").target, ps("a", "1"));
        assert_eq!(sel("a@npm:other@1").target, ps("a", "npm:other@1"));
        assert_eq!(OverrideSelector::parse("//comment").unwrap(), None);
    }

    #[test]
    fn scoped() {
        assert_eq!(
            sel("foo>bar"),
            OverrideSelector {
                parent: Some(ps("foo", "")),
                target: ps("bar", "")
            }
        );
        assert_eq!(
            sel("foo@1>bar@2"),
            OverrideSelector {
                parent: Some(ps("foo", "1")),
                target: ps("bar", "2")
            }
        );
        assert_eq!(
            sel("a@>=1 > b"),
            OverrideSelector {
                parent: None,
                target: ps("a", ">=1 > b")
            }
        );
        assert_eq!(
            sel("foo/bar"),
            OverrideSelector {
                parent: Some(ps("foo", "")),
                target: ps("bar", "")
            }
        );
        assert_eq!(
            sel("**/bar"),
            OverrideSelector {
                parent: None,
                target: ps("bar", "")
            }
        );
        assert_eq!(
            sel("@a/b/@c/d"),
            OverrideSelector {
                parent: Some(ps("@a/b", "")),
                target: ps("@c/d", "")
            }
        );
        assert_eq!(sel("x@npm:@s/o@^1/y").parent, Some(ps("x", "npm:@s/o@^1")));
        assert_eq!(sel("foo@1>bar@2").to_key(), "foo@1>bar@2");
        assert_eq!(sel("foo/bar").to_key(), "foo>bar");
    }

    #[test]
    fn range_comparators_are_not_delimiters() {
        assert_eq!(
            sel("foo@>1"),
            OverrideSelector {
                parent: None,
                target: ps("foo", ">1")
            }
        );
        assert_eq!(
            sel("foo@1 || >2"),
            OverrideSelector {
                parent: None,
                target: ps("foo", "1 || >2")
            }
        );
    }

    #[test]
    fn errors() {
        assert_eq!(OverrideSelector::parse(""), Err(SelectorError::EmptyName));
        assert_eq!(
            OverrideSelector::parse("@scope"),
            Err(SelectorError::InvalidName)
        );
        assert_eq!(
            OverrideSelector::parse("a@"),
            Err(SelectorError::EmptyRange)
        );
        assert_eq!(
            OverrideSelector::parse("a>b>c"),
            Err(SelectorError::TooDeep)
        );
        assert_eq!(
            OverrideSelector::parse("a/b/c"),
            Err(SelectorError::TooDeep)
        );
        assert_eq!(OverrideSelector::parse("a>"), Err(SelectorError::EmptyName));
        assert_eq!(OverrideSelector::parse("**"), Err(SelectorError::EmptyName));
    }
}
