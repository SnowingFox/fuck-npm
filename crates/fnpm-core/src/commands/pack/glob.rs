//! The glob matching of `files` entries and ignore files (bun `glob/matcher.rs`, with
//! `Dots::Match`): `*` and `?` stay inside a path segment, `**` as a whole segment spans any
//! number of segments (`a/**/b` matches `a/b`), `[...]` classes (`!` or `^` negates, ranges),
//! `{a,b}` alternatives (nested), `\` escapes, and leading `!`s that toggle negation.
//! Wildcards match names that start with a dot. Extglobs are matched literally.

use std::collections::HashMap;

/// Whether a path matched (after the leading `!`s) and whether the pattern was negated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MatchResult {
    pub matches: bool,
    pub negated: bool,
}

/// Matches `path` (`/`-separated) against `glob`.
pub(crate) fn glob_match(glob: &[u8], path: &[u8]) -> MatchResult {
    let mut negated = false;
    let mut start = 0;
    while glob.get(start) == Some(&b'!') {
        negated = !negated;
        start += 1;
    }
    let body = &glob[start..];
    let matched = if body.contains(&b'{') {
        expand_braces(body)
            .iter()
            .any(|alternative| match_plain(alternative, path))
    } else {
        match_plain(body, path)
    };
    MatchResult {
        matches: matched != negated,
        negated,
    }
}

const MAX_ALTERNATIVES: usize = 1024;

/// The index of the `}` closing the `{` at `open`, and the top-level `,` positions.
fn brace_group(glob: &[u8], open: usize) -> Option<(usize, Vec<usize>)> {
    let mut depth = 0usize;
    let mut commas = Vec::new();
    let mut i = open;
    while i < glob.len() {
        match glob[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((i, commas));
                }
            }
            b',' if depth == 1 => commas.push(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// `a{b,c}d` to `abd`, `acd`. A `{` without a matching `}` or without a `,` is literal.
fn expand_braces(glob: &[u8]) -> Vec<Vec<u8>> {
    let mut i = 0;
    while i < glob.len() {
        match glob[i] {
            b'\\' => i += 1,
            b'{' => {
                if let Some((close, commas)) = brace_group(glob, i) {
                    if commas.is_empty() {
                        i = close;
                    } else {
                        let mut bounds = vec![i];
                        bounds.extend(&commas);
                        bounds.push(close);
                        let (head, tail) = (&glob[..i], &glob[close + 1..]);
                        let mut out = Vec::new();
                        for pair in bounds.windows(2) {
                            let mut alternative = head.to_vec();
                            alternative.extend_from_slice(&glob[pair[0] + 1..pair[1]]);
                            alternative.extend_from_slice(tail);
                            for expanded in expand_braces(&alternative) {
                                if out.len() == MAX_ALTERNATIVES {
                                    return out;
                                }
                                out.push(expanded);
                            }
                        }
                        return out;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    vec![glob.to_vec()]
}

fn char_len(path: &[u8], at: usize) -> usize {
    let width = match path[at] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    };
    width.min(path.len() - at)
}

/// Decodes the code point at `at` (a byte for invalid UTF-8).
fn code_point(bytes: &[u8], at: usize) -> (u32, usize) {
    let len = char_len(bytes, at);
    match std::str::from_utf8(&bytes[at..at + len]) {
        Ok(s) => (s.chars().next().map_or(0, u32::from), len),
        Err(_) => (u32::from(bytes[at]), 1),
    }
}

/// `[...]` at `open`: the index after `]`, and whether the class matches `c`. `None` if the
/// class is not closed.
fn class(glob: &[u8], open: usize, c: u32) -> Option<(usize, bool)> {
    let mut i = open + 1;
    let negated = matches!(glob.get(i), Some(b'!' | b'^'));
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let &b = glob.get(i)?;
        if b == b']' && !first {
            return Some((i + 1, matched != negated));
        }
        first = false;
        let (low, low_len) = if b == b'\\' && i + 1 < glob.len() {
            let (cp, len) = code_point(glob, i + 1);
            (cp, len + 1)
        } else {
            code_point(glob, i)
        };
        i += low_len;
        let mut high = low;
        if glob.get(i) == Some(&b'-') && glob.get(i + 1).is_some_and(|&n| n != b']') {
            i += 1;
            let (cp, len) = if glob[i] == b'\\' && i + 1 < glob.len() {
                let (cp, len) = code_point(glob, i + 1);
                (cp, len + 1)
            } else {
                code_point(glob, i)
            };
            high = cp;
            i += len;
        }
        if low <= c && c <= high {
            matched = true;
        }
    }
}

fn is_globstar_at(glob: &[u8], gi: usize) -> bool {
    glob.get(gi + 1) == Some(&b'*')
        && (gi == 0 || glob[gi - 1] == b'/')
        && (gi + 2 == glob.len() || glob[gi + 2] == b'/')
}

fn match_plain(glob: &[u8], path: &[u8]) -> bool {
    let mut memo = HashMap::new();
    match_at(glob, 0, path, 0, &mut memo)
}

fn match_at(
    glob: &[u8],
    gi: usize,
    path: &[u8],
    pi: usize,
    memo: &mut HashMap<(usize, usize), bool>,
) -> bool {
    if let Some(&known) = memo.get(&(gi, pi)) {
        return known;
    }
    let result = match_uncached(glob, gi, path, pi, memo);
    memo.insert((gi, pi), result);
    result
}

fn match_uncached(
    glob: &[u8],
    gi: usize,
    path: &[u8],
    pi: usize,
    memo: &mut HashMap<(usize, usize), bool>,
) -> bool {
    let Some(&g) = glob.get(gi) else {
        return pi == path.len();
    };
    match g {
        b'*' if is_globstar_at(glob, gi) => {
            // Several `**/` in a row are one.
            let mut next = gi + 2;
            while glob.get(next) == Some(&b'/') && is_globstar_at(glob, next + 1) {
                next += 3;
            }
            if next == glob.len() {
                return true;
            }
            // `**/rest`: `rest` at the start of any later segment, or right here.
            let rest = next + 1;
            if match_at(glob, rest, path, pi, memo) {
                return true;
            }
            (pi..path.len())
                .filter(|&k| path[k] == b'/')
                .any(|k| match_at(glob, rest, path, k + 1, memo))
        }
        b'*' => {
            let mut next = gi + 1;
            while glob.get(next) == Some(&b'*') {
                next += 1;
            }
            let mut k = pi;
            loop {
                if match_at(glob, next, path, k, memo) {
                    return true;
                }
                if k == path.len() || path[k] == b'/' {
                    return false;
                }
                k += char_len(path, k);
            }
        }
        b'?' => {
            pi < path.len()
                && path[pi] != b'/'
                && match_at(glob, gi + 1, path, pi + char_len(path, pi), memo)
        }
        b'[' => {
            if pi < path.len() && path[pi] != b'/' {
                let (c, len) = code_point(path, pi);
                if let Some((after, matched)) = class(glob, gi, c) {
                    return matched && match_at(glob, after, path, pi + len, memo);
                }
            } else if class(glob, gi, 0).is_some() {
                return false;
            }
            path.get(pi) == Some(&b'[') && match_at(glob, gi + 1, path, pi + 1, memo)
        }
        b'\\' if gi + 1 < glob.len() => {
            path.get(pi) == Some(&glob[gi + 1]) && match_at(glob, gi + 2, path, pi + 1, memo)
        }
        literal => path.get(pi) == Some(&literal) && match_at(glob, gi + 1, path, pi + 1, memo),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(glob: &str, path: &str) -> bool {
        glob_match(glob.as_bytes(), path.as_bytes()).matches
    }

    #[test]
    fn stars() {
        assert!(m("*.js", "a.js"));
        assert!(m("*.js", ".a.js"));
        assert!(!m("*.js", "a/b.js"));
        assert!(m("**/*.js", "a/b/c.js"));
        assert!(m("**/*.js", "c.js"));
        assert!(m("a/**/b", "a/b"));
        assert!(m("a/**/b", "a/x/y/b"));
        assert!(m("a/**", "a/x/y"));
        assert!(!m("a/**", "a"));
        assert!(m("**/b/*.js", "x/b/y/b/z.js"));
        assert!(m("a**b", "axxb"));
        assert!(!m("a**b", "ax/xb"));
        assert!(m("**", "a/b"));
    }

    #[test]
    fn classes_braces_escapes() {
        assert!(m("[ab].js", "a.js"));
        assert!(!m("[!ab].js", "a.js"));
        assert!(m("[a-c]", "b"));
        assert!(m("{a,b}.js", "b.js"));
        assert!(m("x{a,{b,c}}y", "xcy"));
        assert!(m("\\*", "*"));
        assert!(!m("\\*", "a"));
        assert!(m("[", "["));
        assert!(m("{a", "{a"));
        assert!(m("?.js", "ä.js"));
    }

    #[test]
    fn negation() {
        let r = glob_match(b"!a", b"a");
        assert!(r.negated && !r.matches);
        let r = glob_match(b"!a", b"b");
        assert!(r.negated && r.matches);
        let r = glob_match(b"!!a", b"a");
        assert!(!r.negated && r.matches);
        assert!(m("!\\(x", "y"));
        assert!(!m("!\\(x", "(x"));
    }
}
