//! Allocation-free scanners shared by version and range parsing.
//!
//! Port of bun `semver/Version.rs` (`Version::parse`, `Tag::parse_with_pre_count`). Everything
//! here borrows the input; turning a scan into an owned [`crate::Version`] happens in
//! `version.rs` and only allocates for prerelease identifiers and build metadata.

/// Which part of a partial version is a wildcard (`x`, `X`, `*`) or missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Wildcard {
    #[default]
    None,
    Major,
    Minor,
    Patch,
}

/// The prerelease and build parts of a scanned version, as written (without `-` / `+`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RawTag<'a> {
    pub pre: &'a [u8],
    pub build: &'a [u8],
}

/// A version whose missing parts are `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Partial<'a> {
    pub major: Option<u64>,
    pub minor: Option<u64>,
    pub patch: Option<u64>,
    pub tag: RawTag<'a>,
}

/// Result of [`scan_version`], mirroring bun's `ParseResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scan<'a> {
    pub wildcard: Wildcard,
    pub valid: bool,
    /// A number before any wildcard does not fit in `u64`.
    pub overflow: bool,
    /// The prerelease was not introduced by `-` (`1.0.0rc1`).
    pub implicit_pre: bool,
    pub version: Partial<'a>,
    /// Where scanning stopped.
    pub len: usize,
}

impl Default for Scan<'_> {
    fn default() -> Self {
        Self {
            wildcard: Wildcard::None,
            valid: true,
            overflow: false,
            implicit_pre: false,
            version: Partial::default(),
            len: 0,
        }
    }
}

/// The whitespace bun trims around versions (`strings::WHITESPACE_CHARS`).
#[inline]
pub(crate) fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// `strings::length_of_leading_whitespace_ascii`.
#[inline]
pub(crate) fn leading_ws(s: &[u8]) -> usize {
    s.iter().take_while(|c| is_ws(**c)).count()
}

/// Trims [`is_ws`] bytes on both ends.
#[inline]
pub(crate) fn trim_ws(s: &[u8]) -> &[u8] {
    let start = leading_ws(s);
    let s = &s[start..];
    let end = s.iter().rposition(|c| !is_ws(*c)).map_or(0, |i| i + 1);
    &s[..end]
}

/// Parses ASCII digits into a `u64`; `None` when empty, non-digit, or above `u64::MAX`.
/// Leading zeros are accepted (bun `parse_unsigned`).
#[inline]
pub(crate) fn parse_u64(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    let mut acc: u64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(acc)
}

/// Scans one version (possibly partial or with wildcards) at the start of `input`.
pub(crate) fn scan_version(input: &[u8]) -> Scan<'_> {
    let mut result = Scan::default();
    if input.is_empty() {
        result.valid = false;
        return result;
    }

    let len = input.len();
    let i = input
        .iter()
        .position(|&c| !(is_ws(c) || c == b'v' || c == b'='))
        .unwrap_or(len);
    if i == len {
        result.valid = false;
        result.wildcard = Wildcard::Major;
        result.len = i;
        return result;
    }

    if let Some(fast) = scan_plain(input, i) {
        return fast;
    }
    scan_general(input, i)
}

/// The general loop of [`scan_version`], starting after the `v` / `=` / whitespace prefix.
fn scan_general(input: &[u8], start: usize) -> Scan<'_> {
    let len = input.len();
    let mut i = start;
    let mut result = Scan::default();
    let mut part_i: u8 = 0;
    while i < len {
        match input[i] {
            b' ' => break,
            b'|' | b'^' | b'#' | b'&' | b'%' | b'!' => {
                i = i.saturating_sub(1);
                break;
            }
            b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < len && input[i].is_ascii_digit() {
                    i += 1;
                }
                if part_i < 3 {
                    let number = parse_u64(&input[start..i]);
                    if number.is_none() {
                        result.valid = false;
                        // A range ignores a number after a wildcard, so keep reading the token.
                        if result.wildcard == Wildcard::None {
                            result.overflow = true;
                            break;
                        }
                    }
                    match part_i {
                        0 => result.version.major = number,
                        1 => result.version.minor = number,
                        _ => result.version.patch = number,
                    }
                    part_i += 1;
                }
                if i < len && input[i] == b'.' && part_i != 3 {
                    i += 1;
                }
            }
            b'.' => {
                result.valid = false;
                break;
            }
            c @ (b'-' | b'+') => {
                // `1-2` is left to the range parser, which reads it as `1 - 2`.
                let fewest_parts = if c == b'+' { 1 } else { 2 };
                if part_i < fewest_parts && result.wildcard == Wildcard::None {
                    result.valid = false;
                    break;
                }
                let (tag, tag_len) = scan_tag(&input[i..], false);
                result.version.tag = tag;
                i += tag_len;
                break;
            }
            b'x' | b'*' | b'X' => {
                // `2x` and `1.2x` are not `2.x` and `1.2.x`.
                if i > 0 && input[i - 1].is_ascii_digit() {
                    result.valid = false;
                }
                i += 1;
                while i < len && matches!(input[i], b'x' | b'*' | b'X') {
                    i += 1;
                }
                if i < len && input[i] == b'.' {
                    i += 1;
                }
                if result.wildcard == Wildcard::None {
                    match part_i {
                        0 => {
                            result.wildcard = Wildcard::Major;
                            part_i = 1;
                        }
                        1 => {
                            result.wildcard = Wildcard::Minor;
                            part_i = 2;
                        }
                        2 => {
                            result.wildcard = Wildcard::Patch;
                            part_i = 3;
                        }
                        _ => {}
                    }
                }
            }
            c => {
                // npm accepts versions like `1.0.0rc.1` with no `-` before the prerelease.
                if result.wildcard == Wildcard::None && part_i >= 2 && c.is_ascii_alphabetic() {
                    let (tag, tag_len) = scan_tag(&input[i..], true);
                    result.version.tag = tag;
                    result.implicit_pre = true;
                    i += tag_len;
                    break;
                }
                result.valid = false;
                break;
            }
        }
    }

    if result.wildcard == Wildcard::None {
        match part_i {
            0 => {
                result.valid = false;
                result.wildcard = Wildcard::Major;
            }
            1 => result.wildcard = Wildcard::Minor,
            2 => result.wildcard = Wildcard::Patch,
            _ => {}
        }
    }

    result.len = i;
    result
}

/// Fast path of [`scan_version`] for `major.minor.patch` followed by the end, a space, or a
/// `-`/`+` tag: gives the same result as the general loop, or `None` to fall back to it.
#[inline]
fn scan_plain(input: &[u8], mut i: usize) -> Option<Scan<'_>> {
    let len = input.len();
    let mut parts = [0u64; 3];
    for (n, part) in parts.iter_mut().enumerate() {
        let start = i;
        while i < len && input[i].is_ascii_digit() {
            i += 1;
        }
        *part = parse_u64(&input[start..i])?;
        if n < 2 {
            if input.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
    }
    let mut result = Scan::default();
    result.version.major = Some(parts[0]);
    result.version.minor = Some(parts[1]);
    result.version.patch = Some(parts[2]);
    match input.get(i) {
        None | Some(b' ') => {}
        Some(b'-' | b'+') => {
            let (tag, tag_len) = scan_tag(&input[i..], false);
            result.version.tag = tag;
            i += tag_len;
        }
        Some(_) => return None,
    }
    result.len = i;
    Some(result)
}

/// Scans `-pre+build` (or, with `implicit_pre`, `pre+build` with no leading `-`).
/// Returns the tag and the number of bytes read.
pub(crate) fn scan_tag(input: &[u8], implicit_pre: bool) -> (RawTag<'_>, usize) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        None,
        Pre,
        Build,
    }

    // bun counts every `-` and `+` up to the next space only to test for zero. Stopping at the
    // first one keeps a range without spaces (`1.0.0-a||1.0.0-b||...`) linear instead of
    // rescanning the rest of the input for every tag.
    let has_tag = implicit_pre
        || input
            .iter()
            .take_while(|c| **c != b' ')
            .any(|c| matches!(c, b'+' | b'-'));
    if !has_tag {
        return (RawTag::default(), 0);
    }

    let mut tag = RawTag::default();
    let mut state = State::None;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < input.len() {
        match input[i] {
            b'+' => {
                if state == State::Pre || (state == State::None && implicit_pre) {
                    tag.pre = &input[start..i];
                }
                if state != State::Build {
                    state = State::Build;
                    start = i + 1;
                }
            }
            // Inside a prerelease or build metadata, `-` is an identifier character.
            b'-' => {
                if state == State::None {
                    state = State::Pre;
                    start = i + 1;
                }
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' => {}
            _ => {
                match state {
                    State::None => {}
                    State::Pre => tag.pre = &input[start..i],
                    State::Build => tag.build = &input[start..i],
                }
                state = State::None;
                break;
            }
        }
        i += 1;
    }

    if state == State::None && implicit_pre {
        state = State::Pre;
        start = 0;
    }
    match state {
        State::None => {}
        State::Pre => tag.pre = &input[start..i],
        State::Build => tag.build = &input[start..i],
    }
    (tag, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn general(input: &[u8]) -> Scan<'_> {
        let start = input
            .iter()
            .position(|&c| !(is_ws(c) || c == b'v' || c == b'='))
            .unwrap_or(input.len());
        if input.is_empty() || start == input.len() {
            return scan_version(input);
        }
        scan_general(input, start)
    }

    #[test]
    fn fast_path_matches_general_loop() {
        let heads = ["", "v", "=", " ", "v ", "=v"];
        let cores = [
            "1.2.3",
            "0.0.0",
            "01.2.3",
            "1.2",
            "1",
            "1.2.x",
            "18446744073709551615.0.0",
            "18446744073709551616.0.0",
            "1.2.18446744073709551616",
            "1.2.3.4",
            "1.2.3x",
            "1.2.3rc1",
            "10.20.30",
        ];
        let tails = [
            "",
            " ",
            " || 2",
            "-rc.1",
            "+b",
            "-rc.1+b.2",
            "-",
            "+",
            "-a b",
            "|",
            "^",
            ",",
            "x",
            " - 2",
            "-0",
        ];
        for head in heads {
            for core in cores {
                for tail in tails {
                    let text = format!("{head}{core}{tail}");
                    assert_eq!(
                        scan_version(text.as_bytes()),
                        general(text.as_bytes()),
                        "{text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn scans_like_bun() {
        let s = scan_version(b"1.2.3-rc.1+build.5 junk");
        assert!(s.valid);
        assert_eq!(
            (s.version.major, s.version.minor, s.version.patch),
            (Some(1), Some(2), Some(3))
        );
        assert_eq!(
            s.version.tag,
            RawTag {
                pre: b"rc.1",
                build: b"build.5"
            }
        );
        assert_eq!(s.len, 18);
        // `1-2` is left to the range parser as `1 - 2`.
        let s = scan_version(b"1-2");
        assert!(!s.valid && s.len == 1);
        let s = scan_version(b"1.x.99999999999999999999");
        assert!(!s.valid && !s.overflow && s.wildcard == Wildcard::Minor);
        let s = scan_version(b"99999999999999999999.0.0");
        assert!(s.overflow);
        let s = scan_version(b"1.0.0rc.1+b");
        assert!(s.valid && s.implicit_pre);
        assert_eq!(
            s.version.tag,
            RawTag {
                pre: b"rc.1",
                build: b"b"
            }
        );
        let s = scan_version(b"vvv");
        assert!(!s.valid && s.wildcard == Wildcard::Major && s.len == 3);
        assert_eq!(
            scan_tag(b"+build-1", false).0,
            RawTag {
                pre: b"",
                build: b"build-1"
            }
        );
        assert_eq!(
            scan_tag(b"--canary.1", false).0,
            RawTag {
                pre: b"-canary.1",
                build: b""
            }
        );
        assert_eq!(scan_tag(b"x", false), (RawTag::default(), 0));
        assert_eq!(trim_ws(b" \t1.2.3\n\x0b"), b"1.2.3");
        assert_eq!(trim_ws(b"   "), b"");
        assert_eq!(parse_u64(b"007"), Some(7));
        assert_eq!(parse_u64(b""), None);
        assert_eq!(parse_u64(b"18446744073709551616"), None);
    }
}
