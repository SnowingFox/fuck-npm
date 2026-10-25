//! The few semver questions the lockfile code asks, answered by `fnpm-semver` with a
//! per-thread cache of parsed versions and ranges (lockfiles repeat the same literals).
//!
//! Unit tests of this crate must not depend on another crate's in-progress implementation,
//! so under `cfg(test)` a small node-semver compatible fallback answers when `fnpm-semver`
//! is not usable yet. Production code always uses `fnpm-semver`.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;

thread_local! {
    static VERSIONS: RefCell<HashMap<String, Option<fnpm_semver::Version>>> = RefCell::new(HashMap::new());
    static RANGES: RefCell<HashMap<String, Option<fnpm_semver::Range>>> = RefCell::new(HashMap::new());
}

const CACHE_LIMIT: usize = 1 << 16;

/// Looks `key` up in a per-thread cache, parsing and inserting it on a miss.
fn cached<'c, T>(
    cache: &'c mut HashMap<String, Option<T>>,
    key: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Option<&'c T> {
    if !cache.contains_key(key) {
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key.to_owned(), parse(key));
    }
    cache.get(key).and_then(Option::as_ref)
}

fn parse_version(s: &str) -> Option<fnpm_semver::Version> {
    fnpm_semver::Version::parse(s).ok()
}

fn parse_range(s: &str) -> Option<fnpm_semver::Range> {
    fnpm_semver::Range::parse(s).ok()
}

mod real {
    use super::*;

    pub fn is_version(s: &str) -> bool {
        VERSIONS.with(|c| cached(&mut c.borrow_mut(), s, parse_version).is_some())
    }

    pub fn cmp(a: &str, b: &str) -> Ordering {
        // One borrow for both lookups: the cache is a `RefCell`, so looking `b` up from inside
        // `a`'s lookup would be a second mutable borrow.
        VERSIONS
            .with(|c| {
                let mut c = c.borrow_mut();
                let a_ok = cached(&mut c, a, parse_version).is_some();
                let b_ok = cached(&mut c, b, parse_version).is_some();
                match (a_ok, b_ok) {
                    (true, true) => {
                        let va = c.get(a).and_then(Option::as_ref);
                        let vb = c.get(b).and_then(Option::as_ref);
                        va.cmp(&vb)
                    }
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => Ordering::Equal,
                }
            })
            .then_with(|| a.cmp(b))
    }

    pub fn satisfies(range: &str, version: &str) -> bool {
        VERSIONS.with(|vc| {
            let mut vc = vc.borrow_mut();
            let Some(v) = cached(&mut vc, version, parse_version) else {
                return false;
            };
            RANGES.with(|rc| {
                cached(&mut rc.borrow_mut(), range, parse_range).is_some_and(|r| r.satisfies(v))
            })
        })
    }
}

/// `true` if `s` parses as a full `major.minor.patch` version.
pub(crate) fn is_version(s: &str) -> bool {
    #[cfg(test)]
    if !fallback::real_available() {
        return fallback::parse_version(s).is_some();
    }
    real::is_version(s)
}

/// Semver precedence of two version strings; unparsable versions sort first, then by text.
pub(crate) fn cmp(a: &str, b: &str) -> Ordering {
    #[cfg(test)]
    if !fallback::real_available() {
        return match (fallback::parse_version(a), fallback::parse_version(b)) {
            (Some(x), Some(y)) => fallback::cmp(&x, &y),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        }
        .then_with(|| a.cmp(b));
    }
    real::cmp(a, b)
}

thread_local! {
    /// `(range, version)` answers, keyed by a 128-bit hash of both strings so a lookup
    /// allocates nothing.
    static ANSWERS: RefCell<HashMap<u128, bool>> = RefCell::new(HashMap::new());
}

fn pair_key(range: &str, version: &str) -> u128 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(range.as_bytes());
    h.update(&[0]);
    h.update(version.as_bytes());
    h.digest128()
}

/// Does `version` satisfy `range` (node-semver rules, prereleases excluded unless named)?
pub(crate) fn satisfies(range: &str, version: &str) -> bool {
    let key = pair_key(range, version);
    if let Some(hit) = ANSWERS.with(|a| a.borrow().get(&key).copied()) {
        return hit;
    }
    let answer = satisfies_uncached(range, version);
    ANSWERS.with(|a| {
        let mut a = a.borrow_mut();
        if a.len() >= CACHE_LIMIT {
            a.clear();
        }
        a.insert(key, answer);
    });
    answer
}

fn satisfies_uncached(range: &str, version: &str) -> bool {
    #[cfg(test)]
    if !fallback::real_available() {
        return fallback::satisfies(range, version);
    }
    real::satisfies(range, version)
}

/// `*`, `x`, `X` or empty: a range that matches every version (bun `Query::Group::is_star`).
pub(crate) fn is_star(range: &str) -> bool {
    matches!(range.trim(), "" | "*" | "x" | "X")
}

#[cfg(test)]
pub(crate) mod fallback {
    use std::cmp::Ordering;
    use std::sync::OnceLock;

    /// Probes `fnpm-semver` once; while it is a stub (it panics) the fallback is used.
    pub fn real_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let ok = std::panic::catch_unwind(|| {
                let v = fnpm_semver::Version::parse("1.2.3").ok()?;
                let r = fnpm_semver::Range::parse("^1.0.0").ok()?;
                Some(r.satisfies(&v))
            });
            std::panic::set_hook(hook);
            matches!(ok, Ok(Some(true)))
        })
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct V {
        pub n: [u64; 3],
        pub pre: Vec<String>,
    }

    pub fn parse_version(s: &str) -> Option<V> {
        let s = s.trim();
        let s = s.strip_prefix('=').unwrap_or(s).trim_start();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, p.split('.').map(str::to_owned).collect()),
            None => (s, Vec::new()),
        };
        let mut n = [0u64; 3];
        let mut parts = core.split('.');
        for slot in &mut n {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *slot = p.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(V { n, pre })
    }

    pub fn cmp(a: &V, b: &V) -> Ordering {
        a.n.cmp(&b.n)
            .then_with(|| match (a.pre.is_empty(), b.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (x, y) in a.pre.iter().zip(&b.pre) {
                        let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            _ => x.cmp(y),
                        };
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    a.pre.len().cmp(&b.pre.len())
                }
            })
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Op {
        Eq,
        Lt,
        Lte,
        Gt,
        Gte,
    }

    type Set = Vec<(Op, V)>;

    /// A partial version: missing parts are `None`.
    fn partial(s: &str) -> Option<([Option<u64>; 3], Vec<String>)> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, p.split('.').map(str::to_owned).collect()),
            None => (s, Vec::new()),
        };
        let mut out = [None; 3];
        if core.is_empty() {
            return Some((out, pre));
        }
        for (i, p) in core.split('.').enumerate() {
            if i >= 3 {
                return None;
            }
            out[i] = match p {
                "x" | "X" | "*" => None,
                p if p.bytes().all(|b| b.is_ascii_digit()) && !p.is_empty() => {
                    Some(p.parse().ok()?)
                }
                _ => return None,
            };
            if out[i].is_none() {
                break;
            }
        }
        Some((out, pre))
    }

    fn v(n: [u64; 3], pre: Vec<String>) -> V {
        V { n, pre }
    }

    fn zero_pre() -> Vec<String> {
        vec!["0".to_owned()]
    }

    fn comparator(tok: &str, set: &mut Set) -> Option<()> {
        let (op, rest) = if let Some(r) = tok.strip_prefix(">=") {
            (Some(Op::Gte), r)
        } else if let Some(r) = tok.strip_prefix("<=") {
            (Some(Op::Lte), r)
        } else if let Some(r) = tok.strip_prefix('>') {
            (Some(Op::Gt), r)
        } else if let Some(r) = tok.strip_prefix('<') {
            (Some(Op::Lt), r)
        } else if let Some(r) = tok.strip_prefix('=') {
            (Some(Op::Eq), r)
        } else {
            (None, tok)
        };
        if let Some(r) = rest.strip_prefix('^') {
            let ([ma, mi, pa], pre) = partial(r)?;
            let ma = ma.unwrap_or(0);
            if r.trim_start_matches('v').is_empty() || matches!(r, "*" | "x" | "X") {
                set.push((Op::Gte, v([0, 0, 0], vec![])));
                return Some(());
            }
            let lo = v([ma, mi.unwrap_or(0), pa.unwrap_or(0)], pre);
            let hi = if ma > 0 || mi.is_none() {
                [ma + 1, 0, 0]
            } else if mi.unwrap_or(0) > 0 || pa.is_none() {
                [0, mi.unwrap_or(0) + 1, 0]
            } else {
                [0, 0, pa.unwrap_or(0) + 1]
            };
            set.push((Op::Gte, lo));
            set.push((Op::Lt, v(hi, zero_pre())));
            return Some(());
        }
        if let Some(r) = rest.strip_prefix('~') {
            let r = r.strip_prefix('>').unwrap_or(r);
            let ([ma, mi, pa], pre) = partial(r)?;
            let Some(ma) = ma else {
                set.push((Op::Gte, v([0, 0, 0], vec![])));
                return Some(());
            };
            set.push((Op::Gte, v([ma, mi.unwrap_or(0), pa.unwrap_or(0)], pre)));
            let hi = if mi.is_none() {
                [ma + 1, 0, 0]
            } else {
                [ma, mi.unwrap_or(0) + 1, 0]
            };
            set.push((Op::Lt, v(hi, zero_pre())));
            return Some(());
        }
        let ([ma, mi, pa], pre) = partial(rest)?;
        let op = op.unwrap_or(Op::Eq);
        let Some(ma) = ma else {
            if matches!(op, Op::Lt | Op::Gt) {
                set.push((Op::Lt, v([0, 0, 0], zero_pre())));
            } else {
                set.push((Op::Gte, v([0, 0, 0], vec![])));
            }
            return Some(());
        };
        match (mi, pa) {
            (Some(mi), Some(pa)) => set.push((op, v([ma, mi, pa], pre))),
            _ => {
                let lo = [ma, mi.unwrap_or(0), 0];
                let hi = if mi.is_none() {
                    [ma + 1, 0, 0]
                } else {
                    [ma, mi.unwrap_or(0) + 1, 0]
                };
                match op {
                    Op::Eq => {
                        set.push((Op::Gte, v(lo, vec![])));
                        set.push((Op::Lt, v(hi, zero_pre())));
                    }
                    Op::Gte => set.push((Op::Gte, v(lo, vec![]))),
                    Op::Lt => set.push((Op::Lt, v(lo, zero_pre()))),
                    Op::Gt => set.push((Op::Gte, v(hi, vec![]))),
                    Op::Lte => set.push((Op::Lt, v(hi, zero_pre()))),
                }
            }
        }
        Some(())
    }

    pub fn parse_range(s: &str) -> Option<Vec<Set>> {
        let mut sets = Vec::new();
        for part in s.split("||") {
            let part = part.trim();
            let mut set = Set::new();
            if let Some((a, b)) = part.split_once(" - ") {
                let ([ama, ami, apa], apre) = partial(a.trim())?;
                set.push((
                    Op::Gte,
                    v([ama.unwrap_or(0), ami.unwrap_or(0), apa.unwrap_or(0)], apre),
                ));
                let ([bma, bmi, bpa], bpre) = partial(b.trim())?;
                match (bma, bmi, bpa) {
                    (Some(x), Some(y), Some(z)) => set.push((Op::Lte, v([x, y, z], bpre))),
                    (Some(x), Some(y), None) => set.push((Op::Lt, v([x, y + 1, 0], zero_pre()))),
                    (Some(x), None, _) => set.push((Op::Lt, v([x + 1, 0, 0], zero_pre()))),
                    _ => {}
                }
            } else {
                // Join an operator separated from its version by spaces (">= 1.2.3").
                let mut toks: Vec<String> = Vec::new();
                for t in part.split_whitespace() {
                    match toks.last_mut() {
                        Some(last)
                            if matches!(
                                last.as_str(),
                                ">=" | "<=" | ">" | "<" | "=" | "^" | "~"
                            ) =>
                        {
                            last.push_str(t)
                        }
                        _ => toks.push(t.to_owned()),
                    }
                }
                if toks.is_empty() {
                    set.push((Op::Gte, v([0, 0, 0], vec![])));
                }
                for t in &toks {
                    comparator(t, &mut set)?;
                }
            }
            sets.push(set);
        }
        Some(sets)
    }

    fn test(op: Op, c: &V, x: &V) -> bool {
        let o = cmp(x, c);
        match op {
            Op::Eq => o == Ordering::Equal,
            Op::Lt => o == Ordering::Less,
            Op::Lte => o != Ordering::Greater,
            Op::Gt => o == Ordering::Greater,
            Op::Gte => o != Ordering::Less,
        }
    }

    pub fn satisfies(range: &str, version: &str) -> bool {
        let (Some(sets), Some(x)) = (parse_range(range), parse_version(version)) else {
            return false;
        };
        sets.iter().any(|set| {
            set.iter().all(|(op, c)| test(*op, c, &x))
                && (x.pre.is_empty()
                    || set
                        .iter()
                        .any(|(_, c)| !c.pre.is_empty() && c.pre != zero_pre() && c.n == x.n))
        })
    }

    #[test]
    fn fallback_vectors() {
        for (r, v, ok) in [
            ("^1.0.0", "1.2.3", true),
            ("^1.0.0", "2.0.0", false),
            ("^0.2.3", "0.2.9", true),
            ("^0.2.3", "0.3.0", false),
            ("^0.0.3", "0.0.4", false),
            ("~1.2.3", "1.2.9", true),
            ("~1.2.3", "1.3.0", false),
            ("1.x", "1.9.9", true),
            ("1.2", "1.3.0", false),
            ("*", "3.0.0", true),
            ("", "3.0.0", true),
            (">=1.0.0 <2.0.0", "1.5.0", true),
            (">= 1.0.0", "1.5.0", true),
            ("1.0.0 - 2.0.0", "2.0.0", true),
            ("1.0.0 - 2", "2.9.0", true),
            ("^1.0.0 || ^2.0.0", "2.1.0", true),
            ("^1.0.0", "1.1.0-beta.1", false),
            ("^1.1.0-beta.0", "1.1.0-beta.1", true),
            ("1.0.0", "1.0.0", true),
            ("=1.0.0", "1.0.1", false),
            (">1.0.0", "1.0.0", false),
            ("<1.0.0", "0.9.0", true),
            ("~1", "1.9.0", true),
            ("^18.11.18", "18.18.11", true),
        ] {
            assert_eq!(satisfies(r, v), ok, "{r} {v}");
        }
        assert_eq!(
            cmp(
                &parse_version("1.0.0-alpha").unwrap(),
                &parse_version("1.0.0").unwrap()
            ),
            Ordering::Less
        );
        assert!(parse_version("1.2").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_api() {
        assert!(is_version("1.2.3"));
        assert!(!is_version("1.2"));
        assert!(satisfies("^1.0.0", "1.2.3"));
        assert_eq!(cmp("1.0.0", "2.0.0"), Ordering::Less);
        assert!(is_star("*") && is_star("") && !is_star("^1"));
    }

    /// The production path, once `fnpm-semver` is usable (it used to borrow its cache twice in
    /// `cmp`, which panics with "already borrowed" on every call).
    #[test]
    fn real_shim_answers_without_panicking() {
        if !fallback::real_available() {
            return;
        }
        assert!(real::is_version("1.2.3") && !real::is_version("1.2"));
        assert_eq!(real::cmp("1.0.0", "2.0.0"), Ordering::Less);
        assert_eq!(real::cmp("2.0.0", "1.0.0"), Ordering::Greater);
        assert_eq!(real::cmp("1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(real::cmp("1.0.0", "1.0.0-rc.1"), Ordering::Greater);
        assert_eq!(real::cmp("1.0.0", "nope"), Ordering::Greater);
        assert!(real::satisfies("^1.0.0", "1.2.3"));
        assert!(!real::satisfies("^2.0.0", "1.2.3"));
        assert!(!real::satisfies("^1.0.0", "nope"));
    }
}
