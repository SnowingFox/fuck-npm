//! The few semver questions the lockfile code asks, answered by `fnpm-semver` with a
//! per-thread cache of parsed versions and ranges (lockfiles repeat the same literals).

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
    real::is_version(s)
}

/// Semver precedence of two version strings; unparsable versions sort first, then by text.
pub(crate) fn cmp(a: &str, b: &str) -> Ordering {
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
    real::satisfies(range, version)
}

/// `*`, `x`, `X` or empty: a range that matches every version (bun `Query::Group::is_star`).
pub(crate) fn is_star(range: &str) -> bool {
    matches!(range.trim(), "" | "*" | "x" | "X")
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

    /// node-semver answers for the range forms lockfiles contain.
    #[test]
    fn node_semver_vectors() {
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
        assert_eq!(cmp("1.0.0-alpha", "1.0.0"), Ordering::Less);
        assert!(!is_version("1.2"));
    }
}
