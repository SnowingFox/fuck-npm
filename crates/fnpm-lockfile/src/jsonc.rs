//! A small, allocation-light JSONC reader for lockfiles.
//!
//! bun.lock / fnpm.lock are JSON with trailing commas (and bun's JSON parser also accepts
//! comments). Objects keep their rows in file order, duplicates included, because the
//! lockfile parsers report duplicates themselves. Strings without escapes borrow from the
//! input. Every value records the byte offset where it starts, for error messages.

use std::borrow::Cow;

/// A parsed value and the byte offset where it starts in the source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Node<'a> {
    pub value: Value<'a>,
    pub pos: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value<'a> {
    Null,
    Bool(bool),
    Number(f64),
    String(Cow<'a, str>),
    Array(Vec<Node<'a>>),
    Object(Vec<Row<'a>>),
}

/// An object member.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row<'a> {
    pub key: Cow<'a, str>,
    pub key_pos: u32,
    pub value: Node<'a>,
}

/// A syntax error at a byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyntaxError {
    pub pos: u32,
    pub message: &'static str,
}

impl<'a> Node<'a> {
    pub fn as_str(&self) -> Option<&str> {
        match &self.value {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[Row<'a>]> {
        match &self.value {
            Value::Object(rows) => Some(rows),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Node<'a>]> {
        match &self.value {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.value {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// The first row named `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Node<'a>> {
        self.as_object()?
            .iter()
            .find(|r| r.key == key)
            .map(|r| &r.value)
    }

    pub fn is_object(&self) -> bool {
        matches!(self.value, Value::Object(_))
    }
}

/// Parses `src` as one JSONC value (trailing commas and comments allowed).
pub(crate) fn parse(src: &str) -> Result<Node<'_>, SyntaxError> {
    let mut p = Parser {
        s: src.as_bytes(),
        src,
        i: 0,
        depth: 0,
    };
    // A UTF-8 byte order mark is ignored, as bun does.
    if p.s.starts_with(&[0xEF, 0xBB, 0xBF]) {
        p.i = 3;
    }
    p.skip_ws()?;
    let v = p.value()?;
    p.skip_ws()?;
    if p.i != p.s.len() {
        return Err(p.err("Unexpected content after the end of the document"));
    }
    Ok(v)
}

/// 1-based line and column (in characters) of a byte offset.
pub(crate) fn line_col(src: &str, pos: u32) -> (usize, usize) {
    let pos = (pos as usize).min(src.len());
    let before = &src.as_bytes()[..pos];
    let line = memchr::memchr_iter(b'\n', before).count() + 1;
    let line_start = memchr::memrchr(b'\n', before).map_or(0, |i| i + 1);
    let col = src[line_start..pos].chars().count() + 1;
    (line, col)
}

const MAX_DEPTH: u32 = 512;

struct Parser<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
    depth: u32,
}

impl<'a> Parser<'a> {
    fn err(&self, message: &'static str) -> SyntaxError {
        SyntaxError {
            pos: self.i as u32,
            message,
        }
    }

    fn skip_ws(&mut self) -> Result<(), SyntaxError> {
        loop {
            match self.s.get(self.i) {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.i += 1,
                Some(b'/') => match self.s.get(self.i + 1) {
                    Some(b'/') => {
                        self.i = memchr::memchr(b'\n', &self.s[self.i..])
                            .map_or(self.s.len(), |n| self.i + n);
                    }
                    Some(b'*') => {
                        let start = self.i;
                        let rest = &self.s[self.i + 2..];
                        match memchr::memmem::find(rest, b"*/") {
                            Some(n) => self.i += 2 + n + 2,
                            None => {
                                self.i = start;
                                return Err(self.err("Unterminated comment"));
                            }
                        }
                    }
                    _ => return Ok(()),
                },
                _ => return Ok(()),
            }
        }
    }

    fn value(&mut self) -> Result<Node<'a>, SyntaxError> {
        let pos = self.i as u32;
        let value = match self.s.get(self.i) {
            Some(b'{') => self.object()?,
            Some(b'[') => self.array()?,
            Some(b'"') => Value::String(self.string()?),
            Some(b't') => self.word(b"true", Value::Bool(true))?,
            Some(b'f') => self.word(b"false", Value::Bool(false))?,
            Some(b'n') => self.word(b"null", Value::Null)?,
            Some(b'-' | b'0'..=b'9') => self.number()?,
            Some(_) => return Err(self.err("Unexpected character")),
            None => return Err(self.err("Unexpected end of file")),
        };
        Ok(Node { value, pos })
    }

    fn word(&mut self, w: &'static [u8], v: Value<'a>) -> Result<Value<'a>, SyntaxError> {
        if self.s[self.i..].starts_with(w) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(self.err("Unexpected identifier"))
        }
    }

    fn number(&mut self) -> Result<Value<'a>, SyntaxError> {
        let start = self.i;
        if self.s[self.i] == b'-' {
            self.i += 1;
        }
        while let Some(c) = self.s.get(self.i) {
            if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.i += 1;
            } else {
                break;
            }
        }
        self.src[start..self.i]
            .parse::<f64>()
            .map(Value::Number)
            .map_err(|_| SyntaxError {
                pos: start as u32,
                message: "Invalid number",
            })
    }

    fn enter(&mut self) -> Result<(), SyntaxError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("Too deeply nested"));
        }
        Ok(())
    }

    fn object(&mut self) -> Result<Value<'a>, SyntaxError> {
        self.enter()?;
        self.i += 1;
        let mut rows = Vec::new();
        loop {
            self.skip_ws()?;
            match self.s.get(self.i) {
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                Some(b'"') => {}
                Some(_) => return Err(self.err("Expected a string key")),
                None => return Err(self.err("Unexpected end of file")),
            }
            let key_pos = self.i as u32;
            let key = self.string()?;
            self.skip_ws()?;
            if self.s.get(self.i) != Some(&b':') {
                return Err(self.err("Expected \":\""));
            }
            self.i += 1;
            self.skip_ws()?;
            let value = self.value()?;
            rows.push(Row {
                key,
                key_pos,
                value,
            });
            self.skip_ws()?;
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                Some(_) => return Err(self.err("Expected \",\" or \"}\"")),
                None => return Err(self.err("Unexpected end of file")),
            }
        }
        self.depth -= 1;
        Ok(Value::Object(rows))
    }

    fn array(&mut self) -> Result<Value<'a>, SyntaxError> {
        self.enter()?;
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.skip_ws()?;
            if self.s.get(self.i) == Some(&b']') {
                self.i += 1;
                break;
            }
            items.push(self.value()?);
            self.skip_ws()?;
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                Some(_) => return Err(self.err("Expected \",\" or \"]\"")),
                None => return Err(self.err("Unexpected end of file")),
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn string(&mut self) -> Result<Cow<'a, str>, SyntaxError> {
        let start = self.i + 1;
        let mut j = start;
        // Fast path: no escapes.
        match memchr::memchr3(b'"', b'\\', b'\n', &self.s[j..]) {
            Some(n) => {
                j += n;
                match self.s[j] {
                    b'"' => {
                        self.i = j + 1;
                        return Ok(Cow::Borrowed(&self.src[start..j]));
                    }
                    b'\\' => {}
                    _ => {
                        self.i = j;
                        return Err(self.err("Unterminated string"));
                    }
                }
            }
            None => {
                self.i = self.s.len();
                return Err(self.err("Unterminated string"));
            }
        }
        let mut out = String::with_capacity(j - start + 8);
        out.push_str(&self.src[start..j]);
        let mut i = j;
        loop {
            let Some(&c) = self.s.get(i) else {
                self.i = i;
                return Err(self.err("Unterminated string"));
            };
            match c {
                b'"' => {
                    self.i = i + 1;
                    return Ok(Cow::Owned(out));
                }
                b'\n' => {
                    self.i = i;
                    return Err(self.err("Unterminated string"));
                }
                b'\\' => {
                    let Some(&e) = self.s.get(i + 1) else {
                        self.i = i;
                        return Err(self.err("Unterminated string"));
                    };
                    i += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4(i)?;
                            i += 4;
                            let cp = if (0xD800..0xDC00).contains(&hi)
                                && self.s.get(i) == Some(&b'\\')
                                && self.s.get(i + 1) == Some(&b'u')
                            {
                                let lo = self.hex4(i + 2)?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    i += 6;
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    hi
                                }
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => {
                            self.i = i - 1;
                            return Err(self.err("Invalid escape sequence"));
                        }
                    }
                }
                _ => {
                    let run_end = memchr::memchr3(b'"', b'\\', b'\n', &self.s[i..])
                        .map_or(self.s.len(), |n| i + n);
                    out.push_str(&self.src[i..run_end]);
                    i = run_end;
                }
            }
        }
    }

    fn hex4(&mut self, at: usize) -> Result<u32, SyntaxError> {
        let h = self
            .s
            .get(at..at + 4)
            .and_then(|h| std::str::from_utf8(h).ok());
        match h.and_then(|h| u32::from_str_radix(h, 16).ok()) {
            Some(v) => Ok(v),
            None => {
                self.i = at;
                Err(self.err("Invalid unicode escape"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trailing_commas_and_comments() {
        let n =
            parse("{\n  // c\n  \"a\": [1, 2,],\n  /* x */ \"b\": { \"c\": true, },\n}").unwrap();
        assert_eq!(n.get("a").unwrap().as_array().unwrap().len(), 2);
        assert_eq!(n.get("b").unwrap().get("c").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn escapes_and_borrowing() {
        let n = parse(r#"{"k": "plain", "e": "a\"b\\c\u00e9\ud83d\ude00\n"}"#).unwrap();
        assert!(matches!(
            n.get("k").unwrap().value,
            Value::String(Cow::Borrowed("plain"))
        ));
        assert_eq!(
            n.get("e").unwrap().as_str(),
            Some("a\"b\\c\u{e9}\u{1F600}\n")
        );
    }

    #[test]
    fn duplicates_are_kept_in_order() {
        let n = parse(r#"{"a": 1, "a": 2}"#).unwrap();
        assert_eq!(n.as_object().unwrap().len(), 2);
        assert_eq!(n.get("a").unwrap().value, Value::Number(1.0));
    }

    #[test]
    fn errors_have_positions() {
        let src = "{\n  \"a\": ,\n}";
        let e = parse(src).unwrap_err();
        assert_eq!(line_col(src, e.pos), (2, 8));
        assert!(parse("{\"a\": 1} x").is_err());
        assert!(parse("\"abc").is_err());
        assert!(parse("[1 2]").is_err());
        assert!(parse("/* never closed").is_err());
        assert!(parse("{\"a\": \"\\q\"}").is_err());
        let deep = "[".repeat(600) + &"]".repeat(600);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn scalars() {
        assert_eq!(parse(" null ").unwrap().value, Value::Null);
        assert_eq!(parse("-1.5e2").unwrap().value, Value::Number(-150.0));
        assert_eq!(parse("\u{feff}false").unwrap().value, Value::Bool(false));
    }
}
