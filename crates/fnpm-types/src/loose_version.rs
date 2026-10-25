//! A port of the scanning half of bun's `Semver::Version::parse` (`src/semver/Version.rs`).
//!
//! `fnpm-types` sits below `fnpm-semver`, but specifier classification needs two answers from
//! the version scanner: "is this a wildcard-ish non-version such as `2x` or `1beta`" and "is this
//! exactly one complete version". Only those flags are computed here.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wildcard {
    None,
    Major,
    Minor,
    Patch,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Scan {
    pub valid: bool,
    pub wildcard: Wildcard,
    pub len: usize,
}

fn is_tag_char(c: u8) -> bool {
    matches!(c, b'-' | b'+' | b'.' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9')
}

fn tag_len(input: &[u8]) -> usize {
    input.iter().take_while(|&&c| is_tag_char(c)).count()
}

pub(crate) fn scan(input: &[u8]) -> Scan {
    let mut result = Scan { valid: true, wildcard: Wildcard::None, len: 0 };
    if input.is_empty() {
        result.valid = false;
        return result;
    }
    let mut i = input
        .iter()
        .position(|&c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C | b'v' | b'='))
        .unwrap_or(input.len());
    if i == input.len() {
        result.valid = false;
        result.wildcard = Wildcard::Major;
        result.len = i;
        return result;
    }

    let mut part_i: u8 = 0;
    while i < input.len() {
        match input[i] {
            b' ' => break,
            b'|' | b'^' | b'#' | b'&' | b'%' | b'!' => {
                i = i.saturating_sub(1);
                break;
            }
            b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < input.len() && input[i].is_ascii_digit() {
                    i += 1;
                }
                if part_i < 3 {
                    let overflow = std::str::from_utf8(&input[start..i])
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                        .is_none();
                    if overflow {
                        result.valid = false;
                        if result.wildcard == Wildcard::None {
                            break;
                        }
                    }
                    part_i += 1;
                }
                if i < input.len() && input[i] == b'.' && part_i != 3 {
                    i += 1;
                }
            }
            b'.' => {
                result.valid = false;
                break;
            }
            c @ (b'-' | b'+') => {
                let fewest = if c == b'+' { 1 } else { 2 };
                if part_i < fewest && result.wildcard == Wildcard::None {
                    result.valid = false;
                    break;
                }
                i += tag_len(&input[i..]);
                break;
            }
            b'x' | b'*' | b'X' => {
                if i > 0 && input[i - 1].is_ascii_digit() {
                    result.valid = false;
                }
                i += 1;
                while i < input.len() && matches!(input[i], b'x' | b'*' | b'X') {
                    i += 1;
                }
                if i < input.len() && input[i] == b'.' {
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
                // `1.0.0rc.1`: a pre-release tag without the `-`.
                if result.wildcard == Wildcard::None && part_i >= 2 && c.is_ascii_alphabetic() {
                    i += tag_len(&input[i..]);
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

/// One complete version and nothing else, apart from surrounding whitespace.
pub(crate) fn is_exact_version(input: &str) -> bool {
    let trimmed = input.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0B' | '\x0C'));
    let s = scan(trimmed.as_bytes());
    s.valid && s.wildcard == Wildcard::None && s.len == trimmed.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact() {
        for ok in ["1.2.3", "v1.2.3", "1.2.3-beta.1", "1.2.3+build", "=1.2.3", "1.0.0rc.1", " 1.2.3 "] {
            assert!(is_exact_version(ok), "{ok}");
        }
        for bad in ["1", "1.2", "1.x", "^1.2.3", "1.2.3 - 2", "latest", "", "1.2.3.4", "x"] {
            assert!(!is_exact_version(bad), "{bad}");
        }
    }

    #[test]
    fn wildcard_words() {
        let s = scan(b"2x");
        assert!(!s.valid && s.wildcard != Wildcard::None);
        let s = scan(b"1beta");
        assert!(!s.valid && s.wildcard != Wildcard::None);
        let s = scan(b"1");
        assert!(s.valid && s.wildcard == Wildcard::Minor);
        let s = scan(b"1.x");
        assert!(s.valid && s.wildcard == Wildcard::Minor);
    }
}
