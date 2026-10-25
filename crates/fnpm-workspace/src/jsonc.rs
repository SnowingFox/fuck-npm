//! JSONC support: bun's package.json parser accepts `//` and `/* */` comments and trailing
//! commas. Parsing tries strict JSON first (the common case), then strips the extensions
//! while keeping byte offsets, so error positions still point into the original text.
//!
//! [`parse_spans`] builds a tree of byte spans for the format-preserving editor.

use serde_json::Value;

/// Parses JSONC into a [`Value`] (key order preserved). A UTF-8 BOM is ignored.
pub(crate) fn parse_value(text: &str) -> Result<Value, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match serde_json::from_str::<Value>(text) {
        Ok(v) => Ok(v),
        Err(first) => {
            let stripped = strip(text);
            if stripped.len() == text.len() && stripped == text {
                return Err(first.to_string());
            }
            serde_json::from_str::<Value>(&stripped).map_err(|e| e.to_string())
        }
    }
}

/// Replaces comments and trailing commas with spaces (newlines inside block comments are
/// kept). Strings are copied verbatim.
pub(crate) fn strip(text: &str) -> String {
    let b = text.as_bytes();
    let mut out: Vec<u8> = b.to_vec();
    let mut i = 0;
    let mut in_string = false;
    // Index in `out` of the last `,` outside strings with only whitespace/comments after it.
    let mut pending_comma: Option<usize> = None;
    while i < b.len() {
        let c = b[i];
        if in_string {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_string = true;
                pending_comma = None;
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                let end = (i + 2).min(b.len());
                for (k, byte) in out.iter_mut().enumerate().take(end).skip(start) {
                    if b[k] != b'\n' && b[k] != b'\r' {
                        *byte = b' ';
                    }
                }
                i = end;
            }
            b',' => {
                pending_comma = Some(i);
                i += 1;
            }
            b'}' | b']' => {
                if let Some(p) = pending_comma.take() {
                    out[p] = b' ';
                }
                i += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            _ => {
                pending_comma = None;
                i += 1;
            }
        }
    }
    // Only ASCII bytes outside strings were replaced by ASCII spaces, so this stays UTF-8.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// A JSON value with the byte spans the editor needs.
#[derive(Debug, Clone)]
pub(crate) enum Node {
    Object(Object),
    Array,
    Scalar,
}

#[derive(Debug, Clone)]
pub(crate) struct Object {
    /// Offset of `{`.
    pub open: usize,
    /// Offset of `}`.
    pub close: usize,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub key: String,
    /// Offset of the key's opening quote.
    pub key_start: usize,
    pub value_start: usize,
    /// One past the value's last byte.
    pub value_end: usize,
    pub value: Node,
}

impl Object {
    pub(crate) fn get(&self, key: &str) -> Option<(usize, &Member)> {
        // The last duplicate wins, as when parsing.
        self.members
            .iter()
            .enumerate()
            .rev()
            .find(|(_, m)| m.key == key)
    }
}

/// Parses JSONC into spans. Returns `None` for invalid input or a non-object root.
pub(crate) fn parse_spans(text: &str) -> Option<Object> {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
        depth: 0,
    };
    if text.starts_with('\u{feff}') {
        p.i = 3;
    }
    p.ws();
    let root = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return None;
    }
    match root {
        Node::Object(o) => Some(o),
        _ => None,
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        loop {
            match self.b.get(self.i) {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.i += 1,
                Some(b'/') if self.b.get(self.i + 1) == Some(&b'/') => {
                    while self.i < self.b.len() && self.b[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                Some(b'/') if self.b.get(self.i + 1) == Some(&b'*') => {
                    self.i += 2;
                    while self.i < self.b.len()
                        && !(self.b[self.i] == b'*' && self.b.get(self.i + 1) == Some(&b'/'))
                    {
                        self.i += 1;
                    }
                    self.i = (self.i + 2).min(self.b.len());
                }
                _ => return,
            }
        }
    }

    fn value(&mut self) -> Option<Node> {
        match *self.b.get(self.i)? {
            b'{' => self.object().map(Node::Object),
            b'[' => self.array(),
            b'"' => {
                self.string()?;
                Some(Node::Scalar)
            }
            _ => {
                let start = self.i;
                while let Some(&c) = self.b.get(self.i) {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' | b'/') {
                        break;
                    }
                    self.i += 1;
                }
                if self.i == start {
                    None
                } else {
                    Some(Node::Scalar)
                }
            }
        }
    }

    fn string(&mut self) -> Option<(usize, usize)> {
        let start = self.i;
        self.i += 1;
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'\\' => self.i += 2,
                b'"' => {
                    self.i += 1;
                    return Some((start, self.i));
                }
                _ => self.i += 1,
            }
        }
        None
    }

    fn object(&mut self) -> Option<Object> {
        self.depth += 1;
        if self.depth > 512 {
            return None;
        }
        let open = self.i;
        self.i += 1;
        let mut members = Vec::new();
        loop {
            self.ws();
            match *self.b.get(self.i)? {
                b'}' => {
                    self.depth -= 1;
                    let close = self.i;
                    self.i += 1;
                    return Some(Object {
                        open,
                        close,
                        members,
                    });
                }
                b'"' => {
                    let (ks, ke) = self.string()?;
                    let key: String = serde_json::from_slice(&self.b[ks..ke]).ok()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return None;
                    }
                    self.i += 1;
                    self.ws();
                    let value_start = self.i;
                    let value = self.value()?;
                    let value_end = self.i;
                    members.push(Member {
                        key,
                        key_start: ks,
                        value_start,
                        value_end,
                        value,
                    });
                    self.ws();
                    match *self.b.get(self.i)? {
                        b',' => self.i += 1,
                        b'}' => {}
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Node> {
        self.depth += 1;
        if self.depth > 512 {
            return None;
        }
        self.i += 1;
        loop {
            self.ws();
            match *self.b.get(self.i)? {
                b']' => {
                    self.depth -= 1;
                    self.i += 1;
                    return Some(Node::Array);
                }
                _ => {
                    self.value()?;
                    self.ws();
                    match *self.b.get(self.i)? {
                        b',' => self.i += 1,
                        b']' => {}
                        _ => return None,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_and_jsonc() {
        assert_eq!(parse_value(r#"{"a": 1}"#).unwrap()["a"], 1);
        let v =
            parse_value("{\n  // c\n  \"a\": \"x//y\", /* b */\n  \"b\": [1, 2,],\n}\n").unwrap();
        assert_eq!(v["a"], "x//y");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
        assert_eq!(parse_value("\u{feff}{}").unwrap(), serde_json::json!({}));
        assert!(parse_value("{\"a\": }").is_err());
        assert!(parse_value("asdfsadfas").is_err());
        // a comma inside a string before `}` is kept
        assert_eq!(parse_value(r#"{"a": ",}"}"#).unwrap()["a"], ",}");
        // error positions survive stripping
        let err = parse_value("{\n// x\n\"a\": tru }").unwrap_err();
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn spans() {
        let text = "{\n  \"name\": \"x\",\n  \"deps\": { \"a\": \"1\" }, // c\n}";
        let o = parse_spans(text).unwrap();
        assert_eq!(o.members.len(), 2);
        let (_, deps) = o.get("deps").unwrap();
        assert_eq!(&text[deps.value_start..deps.value_end], "{ \"a\": \"1\" }");
        let Node::Object(inner) = &deps.value else {
            panic!()
        };
        assert_eq!(
            &text[inner.members[0].key_start..inner.members[0].value_end],
            "\"a\": \"1\""
        );
        assert!(parse_spans("[1]").is_none());
        assert!(parse_spans("{").is_none());
        assert!(parse_spans("{} x").is_none());
    }
}
