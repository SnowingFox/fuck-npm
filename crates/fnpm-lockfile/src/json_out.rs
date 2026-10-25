//! JSON string output byte-for-byte like bun's `format_json_string_utf8`
//! (`bun_core::string::printer::write_pre_quoted_string` in JSON mode).

/// Appends `s` as a quoted JSON string.
#[inline]
pub(crate) fn quoted(out: &mut String, s: &str) {
    out.push('"');
    escaped(out, s);
    out.push('"');
}

/// Appends the escaped body of `s` without quotes.
pub(crate) fn escaped(out: &mut String, s: &str) {
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if (0x20..0x7F).contains(&b) && b != b'"' && b != b'\\' {
            i += 1;
            continue;
        }
        if b >= 0x80 {
            // Multi-byte character: only U+2028, U+2029 and U+FEFF are escaped.
            let ch = s[i..].chars().next().expect("char boundary");
            let len = ch.len_utf8();
            if matches!(ch, '\u{2028}' | '\u{2029}' | '\u{FEFF}') {
                out.push_str(&s[start..i]);
                push_u(out, ch as u32);
                start = i + len;
            }
            i += len;
            continue;
        }
        out.push_str(&s[start..i]);
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            _ => push_u(out, u32::from(b)),
        }
        i += 1;
        start = i;
    }
    out.push_str(&s[start..]);
}

fn push_u(out: &mut String, c: u32) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push_str("\\u");
    for shift in [12, 8, 4, 0] {
        out.push(HEX[((c >> shift) & 0xF) as usize] as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> String {
        let mut o = String::new();
        quoted(&mut o, s);
        o
    }

    #[test]
    fn matches_bun_escapes() {
        assert_eq!(q("plain"), "\"plain\"");
        assert_eq!(q("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(q("\n\r\t\u{8}\u{c}"), r#""\n\r\t\b\f""#);
        assert_eq!(q("\u{7}\u{b}\u{1}\u{7f}"), r#""\u0007\u000B\u0001\u007F""#);
        assert_eq!(q("é\u{2028}x\u{feff}"), "\"é\\u2028x\\uFEFF\"");
        assert_eq!(q("$`'"), "\"$`'\"");
        assert_eq!(q("😀"), "\"😀\"");
    }
}
