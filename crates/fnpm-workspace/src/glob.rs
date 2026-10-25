//! Path globs for `workspaces` entries and `--filter` selectors (bun `glob/matcher.rs`
//! semantics for the syntax fnpm supports).
//!
//! Patterns are matched segment by segment: `*`, `?`, `[...]` and `\` escapes inside a
//! segment (via `globset` with `literal_separator`), `**` for any number of segments
//! (including none), and `{a,b}` alternation, which may span `/`. Extglobs (`@(a|b)`) are not
//! supported and match literally.

use globset::{GlobBuilder, GlobMatcher};

/// How wildcards treat segments that start with `.` (bun `glob::Dots`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dots {
    /// Like any other segment.
    Match,
    /// Wildcards and `**` do not match a segment starting with `.` unless the pattern segment
    /// itself starts with `.` (`dot: false` in minimatch).
    Hide,
}

/// Returns true if `pattern` contains glob syntax not escaped with a backslash, or starts with
/// `!` (bun `detect_glob_syntax`).
pub fn detect_glob_syntax(pattern: &str) -> bool {
    let b = pattern.as_bytes();
    if b.first() == Some(&b'!') {
        return true;
    }
    if has_extglob_prefix(b) {
        return true;
    }
    for token in *b"*{[?" {
        for (idx, &c) in b.iter().enumerate() {
            if c != token {
                continue;
            }
            let mut i = idx;
            let mut escaped = false;
            while i > 0 && b[i - 1] == b'\\' {
                escaped = !escaped;
                i -= 1;
            }
            if !escaped {
                return true;
            }
        }
    }
    false
}

fn has_extglob_prefix(b: &[u8]) -> bool {
    b.windows(2)
        .any(|w| matches!(w[0], b'@' | b'+' | b'!' | b'?' | b'*') && w[1] == b'(')
}

/// Removes leading `!`s; an odd count negates (bun `strip_negations`).
pub fn strip_negations(raw: &str) -> (&str, bool) {
    let mut remain = raw;
    let mut negated = false;
    while let Some(rest) = remain.strip_prefix('!') {
        negated = !negated;
        remain = rest;
    }
    (remain, negated)
}

#[derive(Debug, Clone)]
pub(crate) enum Seg {
    Literal(String),
    /// Exactly `*`.
    Star,
    /// `**`.
    Globstar,
    /// Any other segment with glob syntax. The flag is whether the segment starts with `.`.
    Pattern(GlobMatcher, bool),
}

impl Seg {
    fn compile(seg: &str) -> Seg {
        match seg {
            "*" => return Seg::Star,
            "**" => return Seg::Globstar,
            _ => {}
        }
        if !detect_glob_syntax(seg.strip_prefix('!').unwrap_or(seg)) {
            return Seg::Literal(unescape(seg));
        }
        match GlobBuilder::new(seg)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
        {
            Ok(g) => Seg::Pattern(g.compile_matcher(), seg.starts_with('.')),
            Err(_) => Seg::Literal(unescape(seg)),
        }
    }

    /// Whether this (non-globstar) segment matches the path segment `name`.
    pub(crate) fn matches(&self, name: &str, dots: Dots) -> bool {
        let hidden = dots == Dots::Hide && name.starts_with('.');
        match self {
            Seg::Literal(l) => l == name,
            Seg::Star => !hidden && !name.is_empty(),
            Seg::Globstar => !hidden,
            Seg::Pattern(m, dot) => (!hidden || *dot) && m.is_match(name),
        }
    }

    pub(crate) fn literal(&self) -> Option<&str> {
        match self {
            Seg::Literal(l) => Some(l),
            _ => None,
        }
    }
}

fn unescape(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    let mut chars = seg.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(n) = chars.next()
        {
            out.push(n);
            continue;
        }
        out.push(c);
    }
    out
}

/// Expands `{a,b}` groups (nested, escapes respected). Gives up past 1024 alternatives and
/// keeps the rest literal.
pub(crate) fn expand_braces(pattern: &str) -> Vec<String> {
    let mut out = Vec::new();
    expand_into(pattern, &mut out);
    out
}

fn expand_into(pattern: &str, out: &mut Vec<String>) {
    let b = pattern.as_bytes();
    let mut i = 0;
    let mut open = None;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => {
                open = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let Some(open) = open else {
        out.push(pattern.to_owned());
        return;
    };
    // Find the matching close and the top-level commas.
    let mut depth = 0usize;
    let mut commas = Vec::new();
    let mut close = None;
    let mut j = open;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            b',' if depth == 1 => commas.push(j),
            _ => {}
        }
        j += 1;
    }
    let Some(close) = close else {
        out.push(pattern.to_owned());
        return;
    };
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let mut from = open + 1;
    let mut bounds = commas;
    bounds.push(close);
    for to in bounds {
        if out.len() >= 1024 {
            out.push(pattern.to_owned());
            return;
        }
        let alt = &pattern[from..to];
        from = to + 1;
        expand_into(&format!("{prefix}{alt}{suffix}"), out);
    }
}

/// Splits a `/`-separated pattern into compiled segments. Empty segments and `.` are dropped.
pub(crate) fn compile_segments(pattern: &str) -> Vec<Seg> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .map(Seg::compile)
        .collect()
}

/// A compiled glob over `/`-separated paths.
#[derive(Debug, Clone)]
pub struct Glob {
    absolute: bool,
    alternatives: Vec<Vec<Seg>>,
}

impl Glob {
    /// Compiles `pattern`. Never fails: invalid segment syntax matches literally.
    pub fn new(pattern: &str) -> Self {
        // Backslashes are escapes, except on Windows where they are separators.
        let pattern = if cfg!(windows) {
            pattern.replace('\\', "/")
        } else {
            pattern.to_owned()
        };
        let absolute = pattern.starts_with('/');
        let alternatives = expand_braces(&pattern)
            .iter()
            .map(|p| compile_segments(p))
            .collect();
        Glob {
            absolute,
            alternatives,
        }
    }

    /// Whether `path` matches. `path` is `/`-separated; an absolute pattern only matches an
    /// absolute path and vice versa.
    pub fn matches(&self, path: &str, dots: Dots) -> bool {
        if path.starts_with('/') != self.absolute {
            return false;
        }
        let parts: Vec<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        self.alternatives
            .iter()
            .any(|segs| match_segments(segs, &parts, dots))
    }
}

/// NFA over pattern positions. `states[p]` means "segments `..p` matched".
pub(crate) fn match_segments(segs: &[Seg], parts: &[&str], dots: Dots) -> bool {
    let mut states = vec![false; segs.len() + 1];
    states[0] = true;
    close(segs, &mut states);
    for part in parts {
        states = step(segs, &states, part, dots);
        if !states.iter().any(|&s| s) {
            return false;
        }
    }
    states[segs.len()]
}

/// Adds the positions reachable by letting a `**` match nothing.
pub(crate) fn close(segs: &[Seg], states: &mut [bool]) {
    for p in 0..segs.len() {
        if states[p] && matches!(segs[p], Seg::Globstar) {
            states[p + 1] = true;
        }
    }
}

/// The state set after consuming one path segment.
pub(crate) fn step(segs: &[Seg], states: &[bool], part: &str, dots: Dots) -> Vec<bool> {
    let mut next = vec![false; segs.len() + 1];
    for p in 0..segs.len() {
        if !states[p] {
            continue;
        }
        match &segs[p] {
            Seg::Globstar => {
                if segs[p].matches(part, dots) {
                    next[p] = true;
                }
            }
            seg => {
                if seg.matches(part, dots) {
                    next[p + 1] = true;
                }
            }
        }
    }
    close(segs, &mut next);
    next
}

/// The `!` entries of a `workspaces` array (bun `NegatedWorkspaces`). `bun install` and
/// `bun run --filter` apply the same rule.
#[derive(Debug, Default, Clone)]
pub struct NegatedWorkspaces(Vec<Glob>);

impl NegatedWorkspaces {
    /// Returns true if `entry` is negated; it is then stored as a matched-directory pattern
    /// (`./` prefixes and trailing slashes removed) and is not walked.
    pub fn take(&mut self, entry: &str) -> bool {
        let (glob, negated) = strip_negations(entry);
        if negated {
            let mut remain = if cfg!(windows) {
                glob.replace('\\', "/")
            } else {
                glob.to_owned()
            };
            while let Some(rest) = remain.strip_prefix("./") {
                remain = rest.to_owned();
            }
            let trimmed = remain.trim_end_matches('/');
            let mut glob = Glob::new(trimmed);
            // `dir/**` also removes `dir` itself, as in npm. `**` matching no segment already
            // covers it; keep the base for brace forms like `{a,b}/**`.
            if let Some(base) = trimmed.strip_suffix("/**") {
                glob.alternatives.extend(Glob::new(base).alternatives);
            }
            self.0.push(glob);
        }
        negated
    }

    /// Only call this for a `dir` (relative to the root, `/`-separated) that a glob entry
    /// matched. A workspace listed by its exact path is never excluded.
    pub fn excludes(&self, dir: &str) -> bool {
        self.0.iter().any(|g| g.matches(dir, Dots::Match))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        Glob::new(p).matches(s, Dots::Match)
    }

    #[test]
    fn detects_syntax() {
        assert!(detect_glob_syntax("packages/*"));
        assert!(detect_glob_syntax("!packages/a"));
        assert!(detect_glob_syntax("a/{b,c}"));
        assert!(detect_glob_syntax("a/[bc]"));
        assert!(detect_glob_syntax("a?"));
        assert!(detect_glob_syntax("@(a|b)"));
        assert!(!detect_glob_syntax("packages/a"));
        assert!(!detect_glob_syntax("packages/\\*"));
        assert!(detect_glob_syntax("packages/\\\\*"));
        assert!(!detect_glob_syntax("@scope/pkg"));
    }

    #[test]
    fn negations() {
        assert_eq!(strip_negations("!a"), ("a", true));
        assert_eq!(strip_negations("!!a"), ("a", false));
        assert_eq!(strip_negations("!!!a"), ("a", true));
        assert_eq!(strip_negations("a"), ("a", false));
    }

    #[test]
    fn segments() {
        assert!(m("packages/*", "packages/a"));
        assert!(!m("packages/*", "packages/a/b"));
        assert!(!m("packages/*", "packages"));
        assert!(m("packages/**", "packages"));
        assert!(m("packages/**", "packages/a/b/c"));
        assert!(m("**/legacy", "packages/legacy"));
        assert!(m("**/legacy", "legacy"));
        assert!(m("packages/{app,legacy}", "packages/app"));
        assert!(m("packages/{app,legacy}", "packages/legacy"));
        assert!(!m("packages/{app,legacy}", "packages/lib"));
        assert!(m("{src/**,lib}", "src/a/b"));
        assert!(m("{src/**,lib}", "lib"));
        assert!(m("pkg-[ab]", "pkg-a"));
        assert!(!m("pkg-[ab]", "pkg-c"));
        assert!(m("pkg?", "pkga"));
        assert!(m("@scope/*", "@scope/x"));
        assert!(!m("@scope/*", "@other/x"));
        assert!(m("/root/packages/*", "/root/packages/a"));
        assert!(!m("/root/packages/*", "root/packages/a"));
        assert!(m("/root/**", "/root"));
        assert!(m("a\\*b", "a*b"));
        assert!(!m("a\\*b", "axb"));
        assert!(m("pkg*", "pkga"));
        assert!(m("*a*", "banana"));
    }

    #[test]
    fn dots() {
        let g = Glob::new("packages/*");
        assert!(!g.matches("packages/.hidden", Dots::Hide));
        assert!(g.matches("packages/.hidden", Dots::Match));
        assert!(Glob::new("packages/.*").matches("packages/.hidden", Dots::Hide));
        assert!(!Glob::new("**/x").matches(".a/x", Dots::Hide));
    }

    #[test]
    fn braces() {
        assert_eq!(expand_braces("a/{b,c}/d"), vec!["a/b/d", "a/c/d"]);
        assert_eq!(expand_braces("{a,{b,c}}"), vec!["a", "b", "c"]);
        assert_eq!(expand_braces("a{b"), vec!["a{b"]);
        assert_eq!(expand_braces("x\\{a,b}"), vec!["x\\{a,b}"]);
    }

    #[test]
    fn negated_workspaces() {
        // bun filter-workspace.test.ts: every spelling of the directory
        for entry in [
            "!packages/legacy",
            "!./packages/legacy",
            "!packages/legacy/",
            "!././packages/legacy//",
            "!**/legacy",
            "!packages/legacy/**",
            "!packages/leg*/**",
        ] {
            let mut n = NegatedWorkspaces::default();
            assert!(n.take(entry), "{entry}");
            assert!(n.excludes("packages/legacy"), "{entry}");
            assert!(!n.excludes("packages/lib"), "{entry}");
        }
        let mut n = NegatedWorkspaces::default();
        n.take("!packages/legacy/**");
        assert!(n.excludes("packages/legacy/nested"));
        let mut n = NegatedWorkspaces::default();
        n.take("!packages/legacy");
        assert!(!n.excludes("packages/legacy/nested"));
        assert!(!n.excludes("other/legacy"));
        let mut n = NegatedWorkspaces::default();
        assert!(!n.take("packages/*"));
        assert!(n.is_empty());
        n.take("!packages/{app,legacy}");
        assert!(
            n.excludes("packages/app")
                && n.excludes("packages/legacy")
                && !n.excludes("packages/lib")
        );
    }
}
