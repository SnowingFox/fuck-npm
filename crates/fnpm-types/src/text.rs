//! Text helpers shared by the crates that print what a package or a registry said.

/// bun's `escape_control_chars`: C0 and C1 control characters, DEL and the bidi controls
/// (U+2028-U+202E, U+2066-U+2069) are written as escapes (`\n`, `\r`, `\t`, `\x1b`,
/// `\u202e`) so a manifest cannot move the cursor or fake an output line.
pub fn escape_control_chars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0'..='\x1f' | '\x7f' => out.push_str(&format!("\\x{:02x}", c as u32)),
            '\u{80}'..='\u{9f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// [`escape_control_chars`] for a message that may span lines on purpose (a `warn:` or `error:`
/// with an `at <file>:<line>` pointer): every control character but `\n` is escaped.
pub fn escape_control_chars_in_lines(text: &str) -> String {
    if !text.chars().any(|c| c != '\n' && needs_escape(c)) {
        return text.to_owned();
    }
    text.split('\n')
        .map(escape_control_chars)
        .collect::<Vec<_>>()
        .join("\n")
}

fn needs_escape(c: char) -> bool {
    matches!(c, '\0'..='\x1f' | '\x7f' | '\u{80}'..='\u{9f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// bun `fmt::size`: `512B`, `1.50KB`, `2.00MB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2}{}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_bun() {
        assert_eq!(
            escape_control_chars("a\x1b[2Kb\nwarn: c"),
            "a\\x1b[2Kb\\nwarn: c"
        );
        assert_eq!(escape_control_chars("tab\there\r"), "tab\\there\\r");
        assert_eq!(
            escape_control_chars("\u{202e}rtl\u{85}"),
            "\\u202ertl\\u0085"
        );
        assert_eq!(
            escape_control_chars_in_lines("a\x1b]0;x\x07\n    at f:1:1"),
            "a\\x1b]0;x\\x07\n    at f:1:1"
        );
        assert_eq!(
            escape_control_chars("caf\u{e9} \"q\" \\"),
            "caf\u{e9} \"q\" \\"
        );
    }
}
