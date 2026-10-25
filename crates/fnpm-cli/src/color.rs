//! picocolors 1.1.1, byte for byte: the TS renderer formats with it, so the native one must
//! produce the same escape sequences, including its nested-close handling.

/// `createColors(enabled)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub enabled: bool,
}

impl Colors {
    /// Colours on or off.
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// picocolors' `formatter(open, close, replace)`. Note the quirk kept from the original:
    /// the search for a nested `close` starts at `open.len()` of the *input*.
    fn format(&self, open: &str, close: &str, replace: &str, input: &str) -> String {
        if !self.enabled {
            return input.to_owned();
        }
        let from = open.len().min(input.len());
        let Some(first) = input
            .get(from..)
            .and_then(|tail| tail.find(close))
            .map(|at| at + from)
        else {
            return format!("{open}{input}{close}");
        };
        let mut result = String::with_capacity(input.len() + open.len() + close.len() + 8);
        result.push_str(open);
        let mut cursor = 0;
        let mut index = Some(first);
        while let Some(at) = index {
            result.push_str(&input[cursor..at]);
            result.push_str(replace);
            cursor = at + close.len();
            index = input[cursor..].find(close).map(|found| found + cursor);
        }
        result.push_str(&input[cursor..]);
        result.push_str(close);
        result
    }

    pub fn bold(&self, input: &str) -> String {
        self.format("\x1b[1m", "\x1b[22m", "\x1b[22m\x1b[1m", input)
    }

    pub fn dim(&self, input: &str) -> String {
        self.format("\x1b[2m", "\x1b[22m", "\x1b[22m\x1b[2m", input)
    }

    pub fn red(&self, input: &str) -> String {
        self.format("\x1b[31m", "\x1b[39m", "\x1b[31m", input)
    }

    pub fn green(&self, input: &str) -> String {
        self.format("\x1b[32m", "\x1b[39m", "\x1b[32m", input)
    }

    pub fn yellow(&self, input: &str) -> String {
        self.format("\x1b[33m", "\x1b[39m", "\x1b[33m", input)
    }

    pub fn cyan(&self, input: &str) -> String {
        self.format("\x1b[36m", "\x1b[39m", "\x1b[36m", input)
    }

    pub fn blue(&self, input: &str) -> String {
        self.format("\x1b[34m", "\x1b[39m", "\x1b[34m", input)
    }
}

/// Remove SGR sequences (`/\u001B\[[0-9;]*m/g`).
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    let mut copied = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            let mut end = index + 2;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                end += 1;
            }
            if bytes.get(end) == Some(&b'm') {
                out.push_str(&input[copied..index]);
                index = end + 1;
                copied = index;
                continue;
            }
        }
        index += 1;
    }
    out.push_str(&input[copied..]);
    out
}

/// JavaScript's `Number.prototype.toFixed(digits)` for finite, non-negative values below
/// 1e21: the nearest decimal, with exact ties rounded up (Rust's formatter rounds ties to
/// even, which differs for values such as 1.25).
pub fn to_fixed(value: f64, digits: usize) -> String {
    // Enough extra digits that a non-tie never shows up as `5000…`.
    let wide = format!("{value:.prec$}", prec = digits + 30);
    let dot = wide.find('.').unwrap_or(wide.len());
    let cut = dot + 1 + digits;
    let tail = &wide[cut.min(wide.len())..];
    let is_tie = tail.starts_with('5') && tail[1..].bytes().all(|byte| byte == b'0');
    if !is_tie {
        return format!("{value:.digits$}");
    }
    // Round the truncated digits up by one unit in the last place.
    let kept: String = if digits == 0 {
        wide[..dot].to_owned()
    } else {
        wide[..cut].to_owned()
    };
    let mut chars: Vec<u8> = kept.into_bytes();
    let mut position = chars.len();
    loop {
        if position == 0 {
            chars.insert(0, b'1');
            break;
        }
        position -= 1;
        match chars[position] {
            b'.' => continue,
            b'9' => chars[position] = b'0',
            digit => {
                chars[position] = digit + 1;
                break;
            }
        }
    }
    String::from_utf8(chars).expect("ascii")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_picocolors() {
        let on = Colors::new(true);
        assert_eq!(on.green("+"), "\x1b[32m+\x1b[39m");
        assert_eq!(
            on.bold(&format!("a{}b", on.dim("x"))),
            "\x1b[1ma\x1b[2mx\x1b[22m\x1b[1mb\x1b[22m"
        );
        // The search starts at open.len() of the input: a close inside the first 4 bytes stays.
        assert_eq!(on.bold("\x1b[22m"), "\x1b[1m\x1b[22m\x1b[22m");
        assert_eq!(Colors::new(false).red("x"), "x");
    }

    #[test]
    fn strips_sgr() {
        assert_eq!(
            strip_ansi("\x1b[1;32mok\x1b[0m done\x1b[K"),
            "ok done\x1b[K"
        );
    }

    #[test]
    fn to_fixed_rounds_ties_up() {
        assert_eq!(to_fixed(1.25, 1), "1.3");
        assert_eq!(to_fixed(0.05, 1), "0.1");
        assert_eq!(to_fixed(2.0, 1), "2.0");
        assert_eq!(to_fixed(12.0, 2), "12.00");
        assert_eq!(to_fixed(9.995, 2), "9.99"); // 9.995 is below the tie in binary
        assert_eq!(to_fixed(9.5, 0), "10");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(999.95, 1), "1000.0");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(0.125, 2), "0.13");
    }
}
