//! package.json text the way bun reads and prints it, for `pm pkg` and `pm version`: a JSON
//! parser that accepts what bun's package.json parser accepts (comments, trailing commas, numbers
//! that overflow), the strict one `pm pkg set --json` checks values with (bun's messages), and
//! the printer (`JSON.stringify` layout with the file's indentation, control characters as
//! upper-case `\u00XX`).

use serde_json::{Map, Number, Value};

/// A parse failure: bun's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError(pub String);

/// Parses package.json text (a UTF-8 BOM, comments and trailing commas are accepted).
pub(crate) fn parse_package_json(text: &str) -> Result<Value, ParseError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
        strict: false,
    };
    p.document()
}

/// Parses one `--json` value: JSON only, bun's messages for what is not.
pub(crate) fn parse_strict(text: &str) -> Result<Value, ParseError> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
        strict: true,
    };
    p.document()
}

/// The indentation of the first indented line (`guess_indentation`): a tab, or that many
/// spaces; two spaces when nothing is indented.
pub(crate) fn guess_indent(text: &str) -> String {
    for line in text.split('\n').skip(1) {
        let ws: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if ws.is_empty() {
            continue;
        }
        return if ws.starts_with('\t') {
            "\t".into()
        } else {
            ws
        };
    }
    "  ".into()
}

/// `value` as bun's JSON printer writes it.
pub(crate) fn print(value: &Value, indent: &str) -> String {
    let mut out = String::new();
    write_value(&mut out, value, indent, 0);
    out
}

fn newline(out: &mut String, indent: &str, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str(indent);
    }
}

fn write_value(out: &mut String, value: &Value, indent: &str, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_string(out, key);
                out.push_str(": ");
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

/// Integers as plain digits (never `1e4`), other numbers like `Number.prototype.toString`.
fn write_number(out: &mut String, n: &Number) {
    if n.is_i64() || n.is_u64() {
        out.push_str(&n.to_string());
        return;
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.fract() == 0.0 && f.abs() < 1e21 {
        out.push_str(&format!("{f:.0}"));
        return;
    }
    let text = n.to_string();
    match text.split_once('e') {
        Some((mantissa, exp)) if !exp.starts_with('-') => {
            out.push_str(mantissa);
            out.push_str("e+");
            out.push_str(exp);
        }
        _ => out.push_str(&text),
    }
}

/// Precedes a lone surrogate (`\ud800`) that a string was read with; the character after it is
/// `SURROGATE_BASE` plus its offset from U+D800 (plane 15, private use).
const SURROGATE_MARK: char = '\u{10ffff}';
const SURROGATE_BASE: u32 = 0xF0000;

/// A JSON string; control characters other than `\b \f \n \r \t` as `\u00XX`, and the lone
/// surrogates the text had as their escapes.
pub(crate) fn write_string(out: &mut String, s: &str) {
    out.push('"');
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == SURROGATE_MARK
            && let Some(&next) = chars.peek()
            && (SURROGATE_BASE..SURROGATE_BASE + 0x800).contains(&(next as u32))
        {
            chars.next();
            out.push_str(&format!("\\u{:04x}", next as u32 - SURROGATE_BASE + 0xD800));
            continue;
        }
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    strict: bool,
}

fn err<T>(message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError(message.into()))
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_ident(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn text(&self, from: usize, to: usize) -> String {
        String::from_utf8_lossy(&self.s[from..to]).into_owned()
    }

    /// Whitespace, and in package.json comments (`--json` values refuse them).
    fn skip(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.i += 1,
                Some(b'/') if matches!(self.s.get(self.i + 1), Some(b'/' | b'*')) => {
                    if self.strict {
                        return err("JSON does not support comments");
                    }
                    if self.s[self.i + 1] == b'/' {
                        while self.peek().is_some_and(|c| c != b'\n') {
                            self.i += 1;
                        }
                    } else {
                        self.i += 2;
                        while self.i < self.s.len()
                            && !(self.s[self.i] == b'*' && self.s.get(self.i + 1) == Some(&b'/'))
                        {
                            self.i += 1;
                        }
                        if self.i >= self.s.len() {
                            return err("Expected \"*/\" but found end of file");
                        }
                        self.i += 2;
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// The token at the cursor, for messages.
    fn token(&self) -> String {
        let start = self.i;
        let Some(c) = self.peek() else {
            return "end of file".into();
        };
        if is_ident(c) {
            let mut end = start;
            while end < self.s.len() && is_ident(self.s[end]) {
                end += 1;
            }
            return self.text(start, end);
        }
        let len = std::str::from_utf8(&self.s[start..])
            .ok()
            .and_then(|s| s.chars().next())
            .map_or(1, char::len_utf8);
        self.text(start, start + len)
    }

    fn document(&mut self) -> Result<Value, ParseError> {
        self.skip()?;
        if self.peek().is_none() {
            return err("Unexpected end of file");
        }
        let value = self.value()?;
        self.skip()?;
        match self.peek() {
            None => Ok(value),
            Some(
                b'+' | b'-' | b'*' | b'/' | b'%' | b'<' | b'>' | b'=' | b'&' | b'|' | b'^' | b'?',
            ) => err("Unsupported syntax: Operators are not allowed in JSON"),
            Some(_) => err(format!("Unexpected {}", self.token())),
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        let Some(c) = self.peek() else {
            return err("Unexpected end of file");
        };
        match c {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Value::String(self.string()?)),
            b'\'' if self.strict => err("Syntax Error"),
            b'\'' => Ok(Value::String(self.string()?)),
            b'-' | b'.' | b'0'..=b'9' => self.number(),
            b'+' => err("Unsupported syntax: Operators are not allowed in JSON"),
            c if is_ident_start(c) => {
                let word = self.token();
                self.i += word.len();
                match word.as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" => Ok(Value::Null),
                    _ => err(format!("Unexpected {word}")),
                }
            }
            _ => err(format!("Unexpected {}", self.token())),
        }
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.i += 1;
        let mut map = Map::new();
        self.skip()?;
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Object(map));
        }
        loop {
            let key = match self.peek() {
                Some(b'"') => self.string()?,
                Some(b'\'') if !self.strict => self.string()?,
                Some(b'\'') => return err("Syntax Error"),
                _ => return err(format!("Expected string but found \"{}\"", self.token())),
            };
            self.skip()?;
            if self.peek() != Some(b':') {
                return err(format!("Expected \":\" but found \"{}\"", self.token()));
            }
            self.i += 1;
            self.skip()?;
            let value = self.value()?;
            map.insert(key, value);
            self.skip()?;
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    self.skip()?;
                    if self.peek() == Some(b'}') {
                        if self.strict {
                            return err("JSON does not support trailing commas");
                        }
                        self.i += 1;
                        return Ok(Value::Object(map));
                    }
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(map));
                }
                _ => return err(format!("Expected \"}}\" but found \"{}\"", self.token())),
            }
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.i += 1;
        let mut items = Vec::new();
        self.skip()?;
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip()?;
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    self.skip()?;
                    if self.peek() == Some(b']') {
                        if self.strict {
                            return err("JSON does not support trailing commas");
                        }
                        self.i += 1;
                        return Ok(Value::Array(items));
                    }
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                _ => return err(format!("Expected \"]\" but found \"{}\"", self.token())),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .s
            .get(self.i..self.i + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        match digits {
            Some(v) => {
                self.i += 4;
                Ok(v)
            }
            None => err("Syntax Error"),
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let quote = self.s[self.i];
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return err("Unterminated string literal");
            };
            self.i += 1;
            match c {
                c if c == quote => break,
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return err("Unterminated string literal");
                    };
                    self.i += 1;
                    let ch = match e {
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'v' if !self.strict => '\u{b}',
                        b'0' if !self.strict => '\0',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi)
                                && self.s.get(self.i..self.i + 2) == Some(b"\\u")
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    self.i = save;
                                    hi
                                }
                            } else {
                                hi
                            };
                            match char::from_u32(code) {
                                Some(c) => c,
                                // A lone surrogate has no UTF-8 form: it is kept as a marker
                                // that `write_string` turns back into the escape.
                                None => {
                                    let mut buf = [0u8; 4];
                                    out.extend_from_slice(SURROGATE_MARK.encode_utf8(&mut buf).as_bytes());
                                    char::from_u32(SURROGATE_BASE + (code - 0xD800)).unwrap_or('\u{fffd}')
                                }
                            }
                        }
                        b'"' | b'\\' | b'/' => e as char,
                        b'\'' if !self.strict => '\'',
                        b'\n' if !self.strict => continue,
                        _ if self.strict => return err("Syntax Error"),
                        _ => e as char,
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                b'\n' => return err("Unterminated string literal"),
                c => out.push(c),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// A JSON number. Integers stay integers when they fit; a number too large for an `f64`
    /// reads as `null` (`JSON.stringify(Infinity)`). Other numeric literals are an error.
    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        let mut end = start;
        if self.s.get(end) == Some(&b'-') {
            end += 1;
            if !self
                .s
                .get(end)
                .is_some_and(|c| c.is_ascii_digit() || *c == b'.')
            {
                let mut word_end = end;
                while word_end < self.s.len() && is_ident(self.s[word_end]) {
                    word_end += 1;
                }
                return err(format!(
                    "Expected number but found \"{}\"",
                    self.text(start, word_end)
                ));
            }
        }
        while end < self.s.len() {
            let c = self.s[end];
            let sign_after_exponent =
                matches!(c, b'+' | b'-') && matches!(self.s[end - 1], b'e' | b'E');
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || sign_after_exponent {
                end += 1;
            } else {
                break;
            }
        }
        let text = self.text(start, end);
        self.i = end;
        if !is_json_number(&text) {
            return err("Syntax Error");
        }
        let integer = !text.contains(['.', 'e', 'E']);
        if integer {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Number(i.into()));
            }
            if let Ok(u) = text.parse::<u64>() {
                return Ok(Value::Number(u.into()));
            }
        }
        let f: f64 = text
            .parse()
            .map_err(|_| ParseError("Syntax Error".into()))?;
        Ok(Number::from_f64(if f == 0.0 { 0.0 } else { f }).map_or(Value::Null, Value::Number))
    }
}

/// `-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?`
fn is_json_number(text: &str) -> bool {
    let b = text.strip_prefix('-').unwrap_or(text).as_bytes();
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let int = digits(0);
    if int == 0 || (int > 1 && b[0] == b'0') {
        return false;
    }
    let mut i = int;
    if b.get(i) == Some(&b'.') {
        let frac = digits(i + 1);
        if frac == 0 {
            return false;
        }
        i += 1 + frac;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp = digits(i);
        if exp == 0 {
            return false;
        }
        i += exp;
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_values_are_json_only() {
        let message = |s: &str| parse_strict(s).unwrap_err().0;
        assert_eq!(message("undefined"), "Unexpected undefined");
        assert_eq!(
            message("-Infinity"),
            "Expected number but found \"-Infinity\""
        );
        assert_eq!(message("{x:1}"), "Expected string but found \"x\"");
        assert_eq!(
            message("1/0"),
            "Unsupported syntax: Operators are not allowed in JSON"
        );
        assert_eq!(
            message("+1"),
            "Unsupported syntax: Operators are not allowed in JSON"
        );
        assert_eq!(message("[1,]"), "JSON does not support trailing commas");
        assert_eq!(message("//c 1"), "JSON does not support comments");
        assert_eq!(message("1 2"), "Unexpected 2");
        assert_eq!(message("[1] {}"), "Unexpected {");
        for s in [
            "010", "-010", "0x10", "1_000", ".5", "5.", "'text'", "{'a':1}",
        ] {
            assert_eq!(message(s), "Syntax Error", "{s}");
        }
        assert_eq!(
            parse_strict(r#"{"a":[1,-0.5,1e3]}"#).unwrap(),
            json!({"a":[1,-0.5,1000.0]})
        );
    }

    #[test]
    fn package_json_is_lenient_and_prints_like_bun() {
        let v = parse_package_json("{\n\t// c\n\t\"a\": 1e400, \"b\": [10000,],\n}").unwrap();
        assert_eq!(v, json!({"a": null, "b": [10000]}));
        assert_eq!(guess_indent("{\n\t\"a\": 1\n}"), "\t");
        assert_eq!(
            print(
                &json!({"d": "a\u{1b}b\u{b}", "e": {}, "f": [1.5, 1e-7]}),
                "  "
            ),
            "{\n  \"d\": \"a\\u001Bb\\u000B\",\n  \"e\": {},\n  \"f\": [\n    1.5,\n    1e-7\n  ]\n}"
        );
    }

    #[test]
    fn lone_surrogates_survive_a_rewrite() {
        let value = parse_package_json(r#"{"\ud800k": "a\udfffb\ud83d\ude00"}"#).unwrap();
        let (key, v) = value.as_object().unwrap().iter().next().unwrap();
        let mut out = String::new();
        write_string(&mut out, key);
        write_string(&mut out, v.as_str().unwrap());
        assert_eq!(out, "\"\\ud800k\"\"a\\udfffb\u{1f600}\"");
    }
}
