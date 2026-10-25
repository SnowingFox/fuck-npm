//! pnpm's package-name matcher for `hoistPattern` and `publicHoistPattern` (bun
//! `install_types/NodeLinker.rs` `PnpmMatcher`, pnpm `config/matcher`).
//!
//! A pattern is a package name where `*` matches any run of characters; every other character
//! is literal (bun escapes everything but `*` before compiling its regex). A leading `!`
//! excludes. With only include patterns, any match wins; with only exclude patterns, a name
//! matches unless an exclude matches it; with both, the last pattern that matches decides.

/// One compiled pattern: the literal pieces between `*`s, or "everything".
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    MatchAll,
    /// `a*b*c` is `["a", "b", "c"]`; anchored at both ends.
    Glob(Vec<String>),
}

impl Pattern {
    fn matches(&self, name: &str) -> bool {
        match self {
            Self::MatchAll => true,
            Self::Glob(parts) => glob_match(parts, name),
        }
    }
}

fn glob_match(parts: &[String], name: &str) -> bool {
    let Some((first, rest)) = parts.split_first() else {
        return name.is_empty();
    };
    let Some(mut remain) = name.strip_prefix(first.as_str()) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        // No `*` at all: the whole name must equal the literal.
        return remain.is_empty();
    };
    for part in middle {
        match remain.find(part.as_str()) {
            Some(i) => remain = &remain[i + part.len()..],
            None => return false,
        }
    }
    remain.len() >= last.len() && remain.ends_with(last.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Matcher {
    pattern: Pattern,
    exclude: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    AllInclude,
    AllExclude,
    Mixed,
}

/// A compiled list of patterns. An empty list matches nothing (bun: no matchers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PnpmMatcher {
    matchers: Vec<Matcher>,
    behavior: Behavior,
}

impl PnpmMatcher {
    /// Compiles `patterns` (bun `create_matcher` for each, then the behaviour rule).
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Self {
        let mut has_include = false;
        let mut has_exclude = false;
        let matchers: Vec<Matcher> = patterns
            .iter()
            .map(|p| {
                let m = create_matcher(p.as_ref());
                has_include |= !m.exclude;
                has_exclude |= m.exclude;
                m
            })
            .collect();
        let behavior = if !has_include {
            Behavior::AllExclude
        } else if !has_exclude {
            Behavior::AllInclude
        } else {
            Behavior::Mixed
        };
        Self { matchers, behavior }
    }

    /// True when there are no patterns at all.
    pub fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }

    /// Whether `name` is selected by the patterns.
    pub fn is_match(&self, name: &str) -> bool {
        if self.matchers.is_empty() {
            return false;
        }
        match self.behavior {
            Behavior::AllInclude => self.matchers.iter().any(|m| m.pattern.matches(name)),
            Behavior::AllExclude => !self.matchers.iter().any(|m| m.pattern.matches(name)),
            Behavior::Mixed => {
                let mut matched = false;
                for m in &self.matchers {
                    if m.pattern.matches(name) {
                        matched = !m.exclude;
                    }
                }
                matched
            }
        }
    }
}

fn create_matcher(raw: &str) -> Matcher {
    let mut trimmed = raw.trim();
    let mut exclude = false;
    if let Some(rest) = trimmed.strip_prefix('!') {
        exclude = true;
        trimmed = rest;
    }
    if trimmed == "*" {
        return Matcher {
            pattern: Pattern::MatchAll,
            exclude,
        };
    }
    Matcher {
        pattern: Pattern::Glob(trimmed.split('*').map(str::to_owned).collect()),
        exclude,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &[&str]) -> PnpmMatcher {
        PnpmMatcher::new(p)
    }

    #[test]
    fn empty_matches_nothing() {
        assert!(!m(&[]).is_match("react"));
        assert!(m(&[]).is_empty());
    }

    #[test]
    fn star_matches_everything() {
        assert!(m(&["*"]).is_match("react"));
        assert!(m(&["*"]).is_match("@types/node"));
        assert!(!m(&["!*"]).is_match("react"));
    }

    #[test]
    fn globs_are_anchored() {
        let p = m(&["*eslint*"]);
        assert!(p.is_match("eslint"));
        assert!(p.is_match("eslint-plugin-react"));
        assert!(p.is_match("@typescript-eslint/parser"));
        assert!(!p.is_match("prettier"));
        let p = m(&["@types/*"]);
        assert!(p.is_match("@types/node"));
        assert!(!p.is_match("types/node"));
        assert!(!p.is_match("@typesx/node"));
        let p = m(&["react"]);
        assert!(p.is_match("react"));
        assert!(!p.is_match("react-dom"));
        assert!(!p.is_match("preact"));
        let p = m(&["a*b*c"]);
        assert!(p.is_match("abc"));
        assert!(p.is_match("aXbYc"));
        assert!(!p.is_match("aXcYb"));
        assert!(!p.is_match("ab"));
        let p = m(&["*-dom"]);
        assert!(p.is_match("react-dom"));
        assert!(!p.is_match("react-domx"));
    }

    #[test]
    fn regex_specials_are_literal() {
        let p = m(&["a.b"]);
        assert!(p.is_match("a.b"));
        assert!(!p.is_match("axb"));
        assert!(m(&["a+b"]).is_match("a+b"));
        assert!(!m(&["a?"]).is_match("ab"));
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert!(m(&["  react\t"]).is_match("react"));
        assert!(!m(&[" !react "]).is_match("react"));
    }

    #[test]
    fn all_exclude() {
        let p = m(&["!react", "!vue"]);
        assert!(!p.is_match("react"));
        assert!(!p.is_match("vue"));
        assert!(p.is_match("svelte"));
    }

    #[test]
    fn mixed_last_match_wins() {
        let p = m(&["*", "!react"]);
        assert!(p.is_match("vue"));
        assert!(!p.is_match("react"));
        let p = m(&["!react", "*"]);
        assert!(p.is_match("react"));
        let p = m(&["*eslint*", "!eslint-config-*", "eslint-config-good"]);
        assert!(p.is_match("eslint"));
        assert!(!p.is_match("eslint-config-bad"));
        assert!(p.is_match("eslint-config-good"));
        assert!(!p.is_match("prettier"));
    }
}
