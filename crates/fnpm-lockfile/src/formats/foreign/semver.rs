//! The part of node-semver the converters need: is a version valid, and does it satisfy a range
//! (yarn/pnpm keys, workspace linking, peer binding, package extension keys).
//!
//! Kept local so the converters do not depend on the resolver's semver crate for these few
//! questions; semantics follow node-semver (and bun's `semver` crate where they differ, e.g.
//! partial versions such as `1.2` parse with a wildcard).

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ident {
    Num(u64),
    Alpha(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<Ident>,
}

impl Version {
    pub(crate) fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: Vec::new(),
        }
    }

    fn triple(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                Ident::Num(n) => write!(f, "{n}")?,
                Ident::Alpha(a) => f.write_str(a)?,
            }
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.triple().cmp(&other.triple()).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let o = match (a, b) {
                            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
                            (Ident::Num(_), Ident::Alpha(_)) => Ordering::Less,
                            (Ident::Alpha(_), Ident::Num(_)) => Ordering::Greater,
                            (Ident::Alpha(a), Ident::Alpha(b)) => a.cmp(b),
                        };
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A version as written, possibly partial (`1`, `1.2`, `1.x`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Vec<Ident>,
}

impl Partial {
    fn min(&self) -> Version {
        Version {
            major: self.major.unwrap_or(0),
            minor: self.minor.unwrap_or(0),
            patch: self.patch.unwrap_or(0),
            pre: self.pre.clone(),
        }
    }

    fn is_wildcard(&self) -> bool {
        self.major.is_none() || self.minor.is_none() || self.patch.is_none()
    }
}

fn parse_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn parse_pre(s: &str) -> Option<Vec<Ident>> {
    s.split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
                return None;
            }
            Some(match parse_number(part) {
                Some(n) => Ident::Num(n),
                None => Ident::Alpha(part.to_owned()),
            })
        })
        .collect()
}

/// `1.2.3-pre+build`, `v1`, `1.x`, `*`. Leading `v`/`=` and whitespace are accepted.
fn parse_partial(s: &str) -> Option<Partial> {
    let s = s.trim();
    let s = s.strip_prefix('=').unwrap_or(s).trim_start();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let s = s.split_once('+').map_or(s, |(v, _)| v);
    let (core, pre) = match s.find('-') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mut parts = core.split('.');
    let mut nums = [None; 3];
    let mut seen_wild = false;
    for slot in &mut nums {
        let Some(part) = parts.next() else { break };
        if matches!(part, "x" | "X" | "*") {
            seen_wild = true;
            continue;
        }
        if seen_wild {
            return None;
        }
        *slot = Some(parse_number(part)?);
    }
    if parts.next().is_some() || (nums[0].is_none() && !seen_wild) {
        return None;
    }
    let pre = match pre {
        Some(p) => parse_pre(p)?,
        None => Vec::new(),
    };
    Some(Partial {
        major: nums[0],
        minor: nums[1],
        patch: nums[2],
        pre,
    })
}

/// bun `Version::parse`: `Some((version, wildcard))` when `s` is a valid version. A partial
/// version is valid with `wildcard = true`, and the version is its minimum.
pub(crate) fn parse_version(s: &str) -> Option<(Version, bool)> {
    let p = parse_partial(s)?;
    Some((p.min(), p.is_wildcard()))
}

/// A complete version (`1.2.3`, `1.2.3-beta.1`).
pub(crate) fn exact_version(s: &str) -> Option<Version> {
    parse_version(s).and_then(|(v, wild)| (!wild).then_some(v))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Comparator {
    op: Op,
    version: Version,
}

impl Comparator {
    fn test(&self, v: &Version) -> bool {
        let o = v.cmp(&self.version);
        match self.op {
            Op::Lt => o == Ordering::Less,
            Op::Lte => o != Ordering::Greater,
            Op::Gt => o == Ordering::Greater,
            Op::Gte => o != Ordering::Less,
            Op::Eq => o == Ordering::Equal,
        }
    }
}

/// An OR of ANDs of comparators. An empty AND set matches everything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Range {
    sets: Vec<Vec<Comparator>>,
}

fn pre0(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        major,
        minor,
        patch,
        pre: vec![Ident::Num(0)],
    }
}

fn cmp(op: Op, version: Version) -> Comparator {
    Comparator { op, version }
}

fn desugar(op: &str, p: &Partial, out: &mut Vec<Comparator>) {
    let (ma, mi, pa) = (p.major, p.minor, p.patch);
    match op {
        "^" => {
            let Some(ma) = ma else { return };
            let min = p.min();
            let upper = if ma > 0 || mi.is_none() {
                pre0(ma + 1, 0, 0)
            } else {
                let mi = mi.unwrap_or(0);
                if mi > 0 || pa.is_none() {
                    pre0(0, mi + 1, 0)
                } else {
                    pre0(0, 0, pa.unwrap_or(0) + 1)
                }
            };
            out.push(cmp(Op::Gte, min));
            out.push(cmp(Op::Lt, upper));
        }
        "~" | "~>" => {
            let Some(ma) = ma else { return };
            out.push(cmp(Op::Gte, p.min()));
            out.push(cmp(
                Op::Lt,
                match mi {
                    None => pre0(ma + 1, 0, 0),
                    Some(mi) => pre0(ma, mi + 1, 0),
                },
            ));
        }
        "" | "=" => match (ma, mi, pa) {
            (None, _, _) => {}
            (Some(ma), None, _) => {
                out.push(cmp(Op::Gte, Version::new(ma, 0, 0)));
                out.push(cmp(Op::Lt, pre0(ma + 1, 0, 0)));
            }
            (Some(ma), Some(mi), None) => {
                out.push(cmp(Op::Gte, Version::new(ma, mi, 0)));
                out.push(cmp(Op::Lt, pre0(ma, mi + 1, 0)));
            }
            _ => out.push(cmp(Op::Eq, p.min())),
        },
        ">" => match (ma, mi, pa) {
            // `>*` matches nothing
            (None, _, _) => out.push(cmp(Op::Lt, pre0(0, 0, 0))),
            (Some(ma), None, _) => out.push(cmp(Op::Gte, Version::new(ma + 1, 0, 0))),
            (Some(ma), Some(mi), None) => out.push(cmp(Op::Gte, Version::new(ma, mi + 1, 0))),
            _ => out.push(cmp(Op::Gt, p.min())),
        },
        ">=" => {
            if ma.is_some() {
                out.push(cmp(Op::Gte, p.min()));
            }
        }
        "<" => match (ma, mi, pa) {
            (None, _, _) => out.push(cmp(Op::Lt, pre0(0, 0, 0))),
            (Some(_), _, Some(_)) => out.push(cmp(Op::Lt, p.min())),
            _ => out.push(cmp(Op::Lt, {
                let mut v = p.min();
                v.pre = vec![Ident::Num(0)];
                v
            })),
        },
        "<=" => match (ma, mi, pa) {
            (None, _, _) => {}
            (Some(ma), None, _) => out.push(cmp(Op::Lt, pre0(ma + 1, 0, 0))),
            (Some(ma), Some(mi), None) => out.push(cmp(Op::Lt, pre0(ma, mi + 1, 0))),
            _ => out.push(cmp(Op::Lte, p.min())),
        },
        _ => {}
    }
}

fn split_op(token: &str) -> (&str, &str) {
    for op in [">=", "<=", "~>", ">", "<", "=", "^", "~"] {
        if let Some(rest) = token.strip_prefix(op) {
            return (op, rest);
        }
    }
    ("", token)
}

fn parse_set(set: &str) -> Option<Vec<Comparator>> {
    let set = set.trim();
    let mut out = Vec::new();
    // hyphen range `a - b`
    if let Some((lo, hi)) = set.split_once(" - ") {
        let (lo, hi) = (parse_partial(lo)?, parse_partial(hi)?);
        if lo.major.is_some() {
            out.push(cmp(Op::Gte, lo.min()));
        }
        desugar("<=", &hi, &mut out);
        return Some(out);
    }
    let mut tokens: Vec<String> = Vec::new();
    let mut pending_op: Option<String> = None;
    for token in set.split_whitespace() {
        let (op, rest) = split_op(token);
        if rest.is_empty() && !op.is_empty() {
            pending_op = Some(op.to_owned());
            continue;
        }
        match pending_op.take() {
            Some(op) => tokens.push(format!("{op}{token}")),
            None => tokens.push(token.to_owned()),
        }
    }
    if pending_op.is_some() {
        return None;
    }
    for token in &tokens {
        let (op, rest) = split_op(token);
        let p = parse_partial(rest)?;
        desugar(op, &p, &mut out);
    }
    Some(out)
}

impl Range {
    /// `None` for anything that is not a range (a dist-tag, a URL, ...).
    pub(crate) fn parse(s: &str) -> Option<Range> {
        let sets = s.split("||").map(parse_set).collect::<Option<Vec<_>>>()?;
        Some(Range { sets })
    }

    /// `*`, `x`, `` or `>=0.0.0`.
    pub(crate) fn is_star(&self) -> bool {
        self.sets.iter().any(|set| {
            set.is_empty()
                || (set.len() == 1
                    && set[0].op == Op::Gte
                    && set[0].version == Version::new(0, 0, 0))
        })
    }

    pub(crate) fn satisfies(&self, v: &Version, include_prerelease: bool) -> bool {
        self.sets.iter().any(|set| {
            if !set.iter().all(|c| c.test(v)) {
                return false;
            }
            if v.pre.is_empty() || include_prerelease {
                return true;
            }
            // A prerelease matches only through a comparator with a prerelease on the same tuple.
            set.iter()
                .any(|c| !c.version.pre.is_empty() && c.version.triple() == v.triple())
        })
    }
}

/// `version` satisfies `range` (both as written). `false` when either does not parse.
#[cfg(test)]
pub(crate) fn satisfies(version: &str, range: &str) -> bool {
    match (exact_version(version), Range::parse(range)) {
        (Some(v), Some(r)) => r.satisfies(&v, false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.2.3"), Some((Version::new(1, 2, 3), false)));
        assert_eq!(
            parse_version("v1.2.3+build"),
            Some((Version::new(1, 2, 3), false))
        );
        assert_eq!(parse_version("1.2"), Some((Version::new(1, 2, 0), true)));
        assert!(parse_version("x.y").is_none());
        assert!(parse_version("latest").is_none());
        assert!(parse_version("").is_none());
        assert!(exact_version("1.2.3-beta.1").is_some());
        assert!(exact_version("1.2").is_none());
        assert_eq!(
            exact_version("1.2.3-rc.1").unwrap().to_string(),
            "1.2.3-rc.1"
        );
        assert!(Version::new(1, 0, 0) > exact_version("1.0.0-rc.1").unwrap());
        assert!(exact_version("1.0.0-alpha.2").unwrap() > exact_version("1.0.0-alpha.1").unwrap());
        assert!(
            exact_version("1.0.0-alpha.beta").unwrap() > exact_version("1.0.0-alpha.1").unwrap()
        );
    }

    #[test]
    fn ranges() {
        let yes = [
            ("1.2.3", "^1.0.0"),
            ("1.9.9", "~1.9.0"),
            ("0.0.3", "^0.0.3"),
            ("0.1.5", "^0.1.0"),
            ("2.0.0", ">=1 <3"),
            ("2.0.0", "> 1"),
            ("1.2.3", "1.2.x"),
            ("1.2.3", "1.x || 2.x"),
            ("1.5.0", "1.0.0 - 2.0.0"),
            ("2.0.5", "1 - 2"),
            ("3.0.0", "*"),
            ("3.0.0", ""),
            ("1.2.3", "=1.2.3"),
            ("1.2.3", "v1.2.3"),
            ("1.2.4-beta.1", ">=1.2.4-beta.0"),
            ("1.3.0", "~> 1.3"),
            ("1.2.9", "<=1.2"),
        ];
        for (v, r) in yes {
            assert!(satisfies(v, r), "{v} should satisfy {r}");
        }
        let no = [
            ("2.0.0", "^1.0.0"),
            ("0.0.4", "^0.0.3"),
            ("0.2.0", "^0.1.0"),
            ("1.10.0", "~1.9.0"),
            ("2.0.0-beta.1", "^1.0.0"),
            ("1.2.4-beta.1", "^1.0.0"),
            ("1.0.0", ">1"),
            ("1.3.0", "<=1.2"),
            ("1.0.0", "latest"),
            ("3.0.0", "1 - 2"),
        ];
        for (v, r) in no {
            assert!(!satisfies(v, r), "{v} should not satisfy {r}");
        }
        let r = Range::parse("^1.0.0").unwrap();
        assert!(r.satisfies(&exact_version("1.5.0-rc.1").unwrap(), true));
        assert!(Range::parse("*").unwrap().is_star());
        assert!(Range::parse(">=0.0.0").unwrap().is_star());
        assert!(!Range::parse("^1").unwrap().is_star());
        assert!(Range::parse("git+https://x").is_none());
    }
}
