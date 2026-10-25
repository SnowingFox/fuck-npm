//! A small JSON / JSONC reader for lockfiles: values borrow from the input (strings without
//! escapes are not copied) and objects keep their keys in file order. package-lock.json and
//! bun.lock are read once, front to back, so a hash per object would only cost time.

use std::borrow::Cow;

/// A parsed value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json<'a> {
    Null,
    Bool(bool),
    /// The number as written.
    Number(&'a str),
    Str(Cow<'a, str>),
    Array(Vec<Json<'a>>),
    Object(Obj<'a>),
}

/// An object, in file order. A repeated key keeps every row; [`Obj::get`] returns the last.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Obj<'a>(pub(crate) Vec<(Cow<'a, str>, Json<'a>)>);

impl<'a> Obj<'a> {
    pub(crate) fn get(&self, key: &str) -> Option<&Json<'a>> {
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Json<'a>)> {
        self.0.iter().map(|(k, v)| (k.as_ref(), v))
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_ref())
    }
}

impl<'o, 'a> IntoIterator for &'o Obj<'a> {
    type Item = (&'o str, &'o Json<'a>);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'o, (Cow<'a, str>, Json<'a>)>,
        fn(&'o (Cow<'a, str>, Json<'a>)) -> (&'o str, &'o Json<'a>),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(k, v)| (k.as_ref(), v))
    }
}

impl<'a> Json<'a> {
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_object(&self) -> Option<&Obj<'a>> {
        match self {
            Json::Object(o) => Some(o),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Json<'a>]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => n.parse().ok(),
            _ => None,
        }
    }
}

/// Parses `input`. With `jsonc`, `//` and `/* */` comments and trailing commas are allowed
/// (bun.lock).
pub(crate) fn parse(input: &[u8], jsonc: bool) -> Result<Json<'_>, String> {
    let text = std::str::from_utf8(input).map_err(|e| format!("invalid UTF-8: {e}"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = Parser {
        s: text,
        b: text.as_bytes(),
        i: 0,
        jsonc,
        depth: 0,
    };
    p.ws()?;
    let value = p.value()?;
    p.ws()?;
    if p.i != p.b.len() {
        return Err(p.error("unexpected trailing characters"));
    }
    Ok(value)
}

const MAX_DEPTH: u32 = 512;

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
    jsonc: bool,
    depth: u32,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> String {
        let line = 1 + self.b[..self.i.min(self.b.len())]
            .iter()
            .filter(|&&c| c == b'\n')
            .count();
        format!("{message} at line {line}")
    }

    fn ws(&mut self) -> Result<(), String> {
        loop {
            match self.b.get(self.i) {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.i += 1,
                Some(b'/') if self.jsonc => match self.b.get(self.i + 1) {
                    Some(b'/') => {
                        self.i = memchr::memchr(b'\n', &self.b[self.i..])
                            .map_or(self.b.len(), |n| self.i + n);
                    }
                    Some(b'*') => {
                        let rest = &self.b[self.i + 2..];
                        let end = memchr::memmem::find(rest, b"*/")
                            .ok_or_else(|| self.error("unterminated comment"))?;
                        self.i += 2 + end + 2;
                    }
                    _ => return Err(self.error("unexpected '/'")),
                },
                _ => return Ok(()),
            }
        }
    }

    fn value(&mut self) -> Result<Json<'a>, String> {
        match self.b.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn literal(&mut self, word: &str, value: Json<'a>) -> Result<Json<'a>, String> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn number(&mut self) -> Result<Json<'a>, String> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while p.b.get(p.i).is_some_and(u8::is_ascii_digit) {
                p.i += 1;
            }
            p.i > s
        };
        if !digits(self) {
            return Err(self.error("invalid number"));
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !digits(self) {
                return Err(self.error("invalid number"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return Err(self.error("invalid number"));
            }
        }
        Ok(Json::Number(&self.s[start..self.i]))
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(self.error("nesting too deep"))
        } else {
            Ok(())
        }
    }

    fn array(&mut self) -> Result<Json<'a>, String> {
        self.enter()?;
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.ws()?;
            if self.b.get(self.i) == Some(&b']') {
                if !items.is_empty() && !self.jsonc {
                    return Err(self.error("trailing comma"));
                }
                self.i += 1;
                break;
            }
            items.push(self.value()?);
            self.ws()?;
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
        self.depth -= 1;
        Ok(Json::Array(items))
    }

    fn object(&mut self) -> Result<Json<'a>, String> {
        self.enter()?;
        self.i += 1;
        let mut rows = Vec::new();
        loop {
            self.ws()?;
            match self.b.get(self.i) {
                Some(b'}') => {
                    if !rows.is_empty() && !self.jsonc {
                        return Err(self.error("trailing comma"));
                    }
                    self.i += 1;
                    break;
                }
                Some(b'"') => {}
                _ => return Err(self.error("expected a string key")),
            }
            let key = self.string()?;
            self.ws()?;
            if self.b.get(self.i) != Some(&b':') {
                return Err(self.error("expected ':'"));
            }
            self.i += 1;
            self.ws()?;
            let value = self.value()?;
            rows.push((key, value));
            self.ws()?;
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
        self.depth -= 1;
        Ok(Json::Object(Obj(rows)))
    }

    fn string(&mut self) -> Result<Cow<'a, str>, String> {
        self.i += 1;
        let start = self.i;
        let Some(end) = memchr::memchr2(b'"', b'\\', &self.b[self.i..]).map(|n| self.i + n) else {
            return Err(self.error("unterminated string"));
        };
        if self.b[end] == b'"' {
            let s = &self.s[start..end];
            if s.bytes().any(|c| c < 0x20) {
                return Err(self.error("control character in string"));
            }
            self.i = end + 1;
            return Ok(Cow::Borrowed(s));
        }
        let mut out = String::with_capacity(end - start + 16);
        out.push_str(&self.s[start..end]);
        self.i = end;
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return Err(self.error("unterminated string"));
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(Cow::Owned(out));
                }
                b'\\' => {
                    let esc = *self
                        .b
                        .get(self.i + 1)
                        .ok_or_else(|| self.error("unterminated string"))?;
                    self.i += 2;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                if self.b.get(self.i) == Some(&b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(self.error("invalid surrogate pair"));
                                    }
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    return Err(self.error("lone surrogate"));
                                }
                            } else {
                                hi
                            };
                            out.push(
                                char::from_u32(code).ok_or_else(|| self.error("invalid escape"))?,
                            );
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                c if c < 0x20 => return Err(self.error("control character in string")),
                _ => {
                    let next = memchr::memchr2(b'"', b'\\', &self.b[self.i..])
                        .map_or(self.b.len(), |n| self.i + n);
                    let chunk = &self.s[self.i..next];
                    if chunk.bytes().any(|c| c < 0x20) {
                        return Err(self.error("control character in string"));
                    }
                    out.push_str(chunk);
                    self.i = next;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .s
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.error("invalid \\u escape"))?;
        let v = u32::from_str_radix(digits, 16).map_err(|_| self.error("invalid \\u escape"))?;
        self.i += 4;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_and_order() {
        let v = parse(br#"{ "b": 1, "a": [true, false, null, -1.5e3, "x\n\u00e9\ud83d\ude00"], "b": "last" }"#, false).unwrap();
        let o = v.as_object().unwrap();
        assert_eq!(
            o.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            ["b", "a", "b"]
        );
        assert_eq!(o.get("b").and_then(Json::as_str), Some("last"));
        let a = o.get("a").unwrap().as_array().unwrap();
        assert_eq!(a[0], Json::Bool(true));
        assert_eq!(a[2], Json::Null);
        assert_eq!(a[3].as_f64(), Some(-1500.0));
        assert_eq!(a[4].as_str(), Some("x\né😀"));
        assert!(matches!(&a[4], Json::Str(Cow::Owned(_))));
        let plain = parse(br#""abc""#, false).unwrap();
        assert!(matches!(plain, Json::Str(Cow::Borrowed("abc"))));
    }

    #[test]
    fn jsonc_only_when_asked() {
        let text = b"{ // c\n \"a\": [1, 2,], /* x */ \"b\": {},\n}";
        assert!(parse(text, false).is_err());
        let v = parse(text, true).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            &b"{"[..],
            b"[1 2]",
            b"{\"a\" 1}",
            b"\"\x01\"",
            b"01x",
            b"tru",
            b"\"\\x\"",
            b"\"\\ud800\"",
            b"{} {}",
            b"-",
            b"1.",
        ] {
            assert!(
                parse(bad, true).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let deep = "[".repeat(600) + &"]".repeat(600);
        assert!(parse(deep.as_bytes(), false).is_err());
    }
}
