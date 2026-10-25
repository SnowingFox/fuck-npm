//! package.json as bun's pack reads and re-prints it (bun `parsers/json.rs`,
//! `js_printer::print_json`): comments and trailing commas are accepted, the last of two equal
//! keys wins in its place (`JSON.parse`), objects print one property per line, arrays stay on
//! one line when they were written on one, strings keep non-ASCII text, integers below 2^52
//! print as plain digits.

use std::fmt::Write as _;

/// A JSON value. Object keys are unique.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    /// The items, and whether the array was written on one line.
    Array(Vec<Json>, bool),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(props) => props.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Object(props) => props.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(props) => Some(props),
            _ => None,
        }
    }

    /// The value as `serde_json` (for the code that reads manifests that way).
    pub fn to_value(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => serde_json::Number::from_f64(*n).map_or(Value::Null, |n| {
                if n.as_f64()
                    .is_some_and(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                {
                    Value::from(n.as_f64().unwrap_or_default() as i64)
                } else {
                    Value::Number(n)
                }
            }),
            Json::String(s) => Value::String(s.clone()),
            Json::Array(items, _) => Value::Array(items.iter().map(Json::to_value).collect()),
            Json::Object(props) => Value::Object(
                props
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_value()))
                    .collect(),
            ),
        }
    }

    /// JS truthiness.
    pub fn is_truthy(&self) -> bool {
        match self {
            Json::Null => false,
            Json::Bool(b) => *b,
            Json::Number(n) => *n != 0.0 && !n.is_nan(),
            Json::String(s) => !s.is_empty(),
            _ => true,
        }
    }
}

/// Where a parse failed, 1-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

struct Parser<'a> {
    text: &'a [u8],
    pos: usize,
    depth: usize,
}

const MAX_DEPTH: usize = 1000;

/// Parses package.json text (a UTF-8 BOM is skipped).
pub(crate) fn parse(text: &str) -> Result<Json, ParseError> {
    let bytes = text.as_bytes();
    let start = if bytes.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    let mut p = Parser {
        text: bytes,
        pos: start,
        depth: 0,
    };
    p.skip_ws()?;
    if p.pos >= bytes.len() {
        return Err(p.error("Unexpected end of file"));
    }
    let value = p.value()?;
    p.skip_ws()?;
    if p.pos < bytes.len() {
        return Err(p.unexpected());
    }
    Ok(value)
}

impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> ParseError {
        let upto = &self.text[..self.pos.min(self.text.len())];
        let line = upto.iter().filter(|&&b| b == b'\n').count() + 1;
        let line_start = upto.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let column = String::from_utf8_lossy(&upto[line_start..]).chars().count() + 1;
        ParseError {
            message: message.into(),
            line,
            column,
        }
    }

    fn unexpected(&self) -> ParseError {
        match self.text.get(self.pos) {
            None => self.error("Unexpected end of file"),
            Some(_) => {
                let rest = String::from_utf8_lossy(&self.text[self.pos..]);
                let token: String = rest.chars().take(1).collect();
                self.error(format!("Unexpected {token}"))
            }
        }
    }

    /// Skips whitespace and comments. Returns whether a newline was skipped.
    fn skip_ws(&mut self) -> Result<bool, ParseError> {
        let mut newline = false;
        loop {
            match self.text.get(self.pos) {
                Some(b'\n') => {
                    newline = true;
                    self.pos += 1;
                }
                Some(b' ' | b'\t' | b'\r') => self.pos += 1,
                Some(b'/') if self.text.get(self.pos + 1) == Some(&b'/') => {
                    while self.pos < self.text.len() && self.text[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.text.get(self.pos + 1) == Some(&b'*') => {
                    let body = &self.text[self.pos + 2..];
                    match body.windows(2).position(|w| w == b"*/") {
                        Some(end) => {
                            newline |= body[..end].contains(&b'\n');
                            self.pos += 2 + end + 2;
                        }
                        None => {
                            self.pos = self.text.len();
                            return Err(
                                self.error("Expected \"*/\" to terminate multi-line comment")
                            );
                        }
                    }
                }
                Some(&b) if b >= 0x80 => {
                    // U+00A0, U+FEFF and the other Unicode spaces bun's lexer skips.
                    let rest = std::str::from_utf8(&self.text[self.pos..])
                        .ok()
                        .or_else(|| std::str::from_utf8(&self.text[self.pos..self.pos + 3]).ok());
                    let Some(c) = rest.and_then(|s| s.chars().next()) else {
                        return Ok(newline);
                    };
                    if matches!(c, '\u{2028}' | '\u{2029}') {
                        newline = true;
                    } else if !(c.is_whitespace() || c == '\u{feff}') {
                        return Ok(newline);
                    }
                    self.pos += c.len_utf8();
                }
                _ => return Ok(newline),
            }
        }
    }

    fn value(&mut self) -> Result<Json, ParseError> {
        match self.text.get(self.pos) {
            None => Err(self.error("Unexpected end of file")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => {
                for (word, value) in [
                    ("true", Json::Bool(true)),
                    ("false", Json::Bool(false)),
                    ("null", Json::Null),
                ] {
                    if self.text[self.pos..].starts_with(word.as_bytes()) {
                        self.pos += word.len();
                        return Ok(value);
                    }
                }
                Err(self.unexpected())
            }
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("Maximum call stack size exceeded"));
        }
        Ok(())
    }

    fn object(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.pos += 1;
        let mut props: Vec<(String, Json)> = Vec::new();
        self.skip_ws()?;
        if self.text.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Object(props));
        }
        loop {
            self.skip_ws()?;
            if self.text.get(self.pos) != Some(&b'"') {
                return Err(match self.text.get(self.pos) {
                    None => self.error("Unexpected end of file"),
                    Some(_) => self.error(format!("Expected string but found {}", self.found())),
                });
            }
            let key = self.string()?;
            self.skip_ws()?;
            if self.text.get(self.pos) != Some(&b':') {
                return Err(self.expected("\":\""));
            }
            self.pos += 1;
            self.skip_ws()?;
            let value = self.value()?;
            if let Some(i) = props.iter().position(|(k, _)| *k == key) {
                props.remove(i);
            }
            props.push((key, value));
            self.skip_ws()?;
            match self.text.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws()?;
                    if self.text.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        break;
                    }
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.expected("\"}\"")),
            }
        }
        self.depth -= 1;
        Ok(Json::Object(props))
    }

    fn array(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.pos += 1;
        let mut items = Vec::new();
        let mut single_line = !self.skip_ws()?;
        loop {
            match self.text.get(self.pos) {
                None => return Err(self.error("Unexpected end of file")),
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => {}
            }
            if !items.is_empty() {
                if self.text.get(self.pos) != Some(&b',') {
                    return Err(self.expected("\",\""));
                }
                self.pos += 1;
                if self.skip_ws()? {
                    single_line = false;
                }
                if self.text.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    break;
                }
            }
            items.push(self.value()?);
            if self.skip_ws()? {
                single_line = false;
            }
        }
        self.depth -= 1;
        Ok(Json::Array(items, single_line))
    }

    fn found(&self) -> String {
        match self.text.get(self.pos) {
            None => "end of file".to_owned(),
            Some(_) => {
                let rest = String::from_utf8_lossy(&self.text[self.pos..]);
                format!("\"{}\"", rest.chars().take(1).collect::<String>())
            }
        }
    }

    fn expected(&self, what: &str) -> ParseError {
        match self.text.get(self.pos) {
            None => self.error("Unexpected end of file"),
            Some(_) => self.error(format!("Expected {what} but found {}", self.found())),
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok());
        match digits {
            Some(v) => {
                self.pos += 4;
                Ok(v)
            }
            None => Err(self.error("Invalid escape sequence")),
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        self.pos += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&b) = self.text.get(self.pos) else {
                self.pos = start;
                return Err(self.error("Unterminated string literal"));
            };
            match b {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(&e) = self.text.get(self.pos) else {
                        return Err(self.error("Unterminated string literal"));
                    };
                    self.pos += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let first = self.hex4()?;
                            let mut cp = first;
                            if (0xd800..0xdc00).contains(&first)
                                && self.text.get(self.pos..self.pos + 2) == Some(b"\\u")
                            {
                                let save = self.pos;
                                self.pos += 2;
                                let second = self.hex4()?;
                                if (0xdc00..0xe000).contains(&second) {
                                    cp = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                                } else {
                                    self.pos = save;
                                }
                            }
                            char::from_u32(cp).unwrap_or('\u{fffd}')
                        }
                        _ => return Err(self.error("Invalid escape sequence")),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                b'\n' => {
                    return Err(self.error("Unterminated string literal"));
                }
                _ => {
                    out.push(b);
                    self.pos += 1;
                }
            }
        }
        Ok(String::from_utf8(out)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.pos;
        if self.text[self.pos] == b'-' {
            self.pos += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.pos;
            while p.text.get(p.pos).is_some_and(u8::is_ascii_digit) {
                p.pos += 1;
            }
            p.pos > s
        };
        if !digits(self) {
            return Err(self.unexpected());
        }
        if self.text.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if !digits(self) {
                return Err(self.unexpected());
            }
        }
        if matches!(self.text.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.text.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return Err(self.unexpected());
            }
        }
        let text = std::str::from_utf8(&self.text[start..self.pos]).unwrap_or("0");
        Ok(Json::Number(text.parse().unwrap_or(f64::NAN)))
    }
}

/// The indentation of a printed document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Indent {
    pub tab: bool,
    pub width: usize,
}

impl Default for Indent {
    fn default() -> Self {
        Self {
            tab: false,
            width: 2,
        }
    }
}

/// bun `JSON::guess_indentation`: the whitespace at the start of the first indented line
/// outside strings and comments; two spaces when there is none.
pub(crate) fn guess_indentation(s: &[u8]) -> Indent {
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < s.len() && s[i] != q {
                    i += if s[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
                continue;
            }
            b'/' if s.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < s.len() && s[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if s.get(i + 1) == Some(&b'*') => {
                let Some(close) = s[i + 2..].windows(2).position(|w| w == b"*/") else {
                    return Indent::default();
                };
                i += 2 + close + 2;
                continue;
            }
            b'\n' => {
                i += 1;
                while i < s.len() && (s[i] == b'\n' || s[i] == b'\r') {
                    i += 1;
                }
                if i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
                    let ch = s[i];
                    let mut count = 0;
                    while i < s.len() && s[i] == ch {
                        i += 1;
                        count += 1;
                    }
                    return Indent {
                        tab: ch == b'\t',
                        width: count,
                    };
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    Indent::default()
}

/// Prints `value` like bun's JSON printer. `newline` appends a final `\n`.
pub(crate) fn print(value: &Json, indent: Indent, newline: bool) -> String {
    let mut out = String::new();
    print_value(&mut out, value, indent, 0);
    if newline {
        out.push('\n');
    }
    out
}

fn push_indent(out: &mut String, indent: Indent, level: usize) {
    let c = if indent.tab { '\t' } else { ' ' };
    for _ in 0..indent.width * level {
        out.push(c);
    }
}

fn print_value(out: &mut String, value: &Json, indent: Indent, level: usize) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => print_number(out, *n),
        Json::String(s) => quote_into(out, s),
        Json::Array(items, single_line) => {
            out.push('[');
            if !items.is_empty() {
                let inner = if *single_line { level } else { level + 1 };
                for (i, item) in items.iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                        if *single_line {
                            out.push(' ');
                        }
                    }
                    if !single_line {
                        out.push('\n');
                        push_indent(out, indent, inner);
                    }
                    print_value(out, item, indent, inner);
                }
                if !single_line {
                    out.push('\n');
                    push_indent(out, indent, level);
                }
            }
            out.push(']');
        }
        Json::Object(props) => {
            out.push('{');
            if !props.is_empty() {
                for (i, (key, value)) in props.iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    push_indent(out, indent, level + 1);
                    quote_into(out, key);
                    out.push_str(": ");
                    print_value(out, value, indent, level + 1);
                }
                out.push('\n');
                push_indent(out, indent, level);
            }
            out.push('}');
        }
    }
}

fn print_number(out: &mut String, n: f64) {
    if !n.is_finite() {
        out.push_str("null");
        return;
    }
    if n.is_sign_negative() {
        out.push('-');
    }
    let abs = n.abs();
    if abs < (u64::MAX >> 12) as f64 && abs.fract() == 0.0 {
        let _ = write!(out, "{}", abs as u64);
    } else {
        let _ = write!(out, "{abs}");
    }
}

/// A JSON string literal with bun's escapes.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    quote_into(&mut out, s);
    out
}

fn quote_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' '..='~' => out.push(c),
            c if (c as u32) < 0x80 || matches!(c, '\u{feff}' | '\u{2028}' | '\u{2029}') => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reprint(text: &str) -> String {
        let value = parse(text).unwrap();
        print(
            &value,
            guess_indentation(text.as_bytes()),
            text.ends_with('\n'),
        )
    }

    #[test]
    fn compact_source_gets_two_spaces() {
        assert_eq!(
            reprint(r#"{"name":"a","version":"1.2.3","files":["x"],"n":10000}"#),
            "{\n  \"name\": \"a\",\n  \"version\": \"1.2.3\",\n  \"files\": [\"x\"],\n  \"n\": 10000\n}"
        );
    }

    #[test]
    fn last_key_wins_in_its_place() {
        let text =
            "{\n  \"name\": \"a\",\n  \"s\": { \"p\": 1, \"p\": 2 },\n  \"name\": \"b\"\n}\n";
        assert_eq!(
            reprint(text),
            "{\n  \"s\": {\n    \"p\": 2\n  },\n  \"name\": \"b\"\n}\n"
        );
    }

    #[test]
    fn multi_line_arrays_and_tabs() {
        let text = "{\n\t\"a\": [\n\t\t1,\n\t\t2\n\t],\n\t\"b\": []\n}";
        assert_eq!(reprint(text), text);
    }

    #[test]
    fn comments_trailing_commas_and_escapes() {
        let text = "{ // c\n \"a\": \"\\u00e9\\u0007\\u2028\", /* x */ \"b\": [1,],\n}";
        let value = parse(text).unwrap();
        assert_eq!(
            print(&value, Indent::default(), false),
            "{\n  \"a\": \"é\\u0007\\u2028\",\n  \"b\": [1]\n}"
        );
    }

    #[test]
    fn numbers() {
        let value = parse("[1, -0, 0.5, 1e21, 123456789012, -3]").unwrap();
        assert_eq!(
            print(&value, Indent::default(), false),
            "[1, -0, 0.5, 1000000000000000000000, 123456789012, -3]"
        );
    }

    #[test]
    fn errors_have_positions() {
        let err = parse("{ \"name\": \"x\",").unwrap_err();
        assert_eq!(
            (err.line, err.message.as_str()),
            (1, "Unexpected end of file")
        );
        let err = parse("{ broken\n").unwrap_err();
        assert_eq!(err.line, 1);
    }
}
