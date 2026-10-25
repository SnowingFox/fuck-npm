//! A YAML tree that keeps every scalar as written.
//!
//! yarn and pnpm read every scalar as a string, but a typed YAML loader turns the plain scalar
//! `2.10` into the number `2.1` (bun works around this with `scalar_text`). This tree keeps the
//! text and the type of each scalar, so [`Node::text`] gives what the tools themselves read and
//! [`Node::as_str`] gives only what YAML types as a string.

use std::collections::HashMap;

use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::TScalarStyle;

/// Deeper documents are rejected instead of overflowing the stack.
const MAX_DEPTH: usize = 256;

/// How YAML 1.2 (core schema) types a scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Str,
    Number,
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Scalar { text: String, kind: Kind },
    Map(Vec<(Node, Node)>),
    Seq(Vec<Node>),
}

pub(crate) static EMPTY_MAP: Node = Node::Map(Vec::new());

impl Node {
    /// The value of the first entry whose key reads as `key` (the first, like bun `Expr::get`).
    pub(crate) fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.text() == Some(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// The entries of a mapping, or nothing for any other node.
    pub(crate) fn entries(&self) -> &[(Node, Node)] {
        match self {
            Node::Map(entries) => entries,
            _ => &[],
        }
    }

    /// `get(key)` or an empty mapping.
    pub(crate) fn get_or_empty(&self, key: &str) -> &Node {
        self.get(key).unwrap_or(&EMPTY_MAP)
    }

    /// A scalar that YAML types as a string (quoted, block, or a plain scalar that is not a
    /// number, boolean or null).
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar {
                text,
                kind: Kind::Str,
            } => Some(text),
            _ => None,
        }
    }

    /// `get(key)` as a string.
    pub(crate) fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Node::as_str)
    }

    /// The text yarn and pnpm read: any string, or the text of a plain number, boolean or null
    /// (`2.10`, `true`). An empty null has no text.
    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Node::Scalar {
                text,
                kind: Kind::Str,
            } => Some(text),
            Node::Scalar { text, .. } => {
                let plain = !text.is_empty()
                    && text.bytes().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.' | b'~')
                    });
                plain.then_some(text.as_str())
            }
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Node::Scalar {
                kind: Kind::Bool(b),
                ..
            } => Some(*b),
            _ => None,
        }
    }

    pub(crate) fn as_seq(&self) -> Option<&[Node]> {
        match self {
            Node::Seq(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn is_map(&self) -> bool {
        matches!(self, Node::Map(_))
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(
            self,
            Node::Scalar {
                kind: Kind::Null,
                ..
            }
        )
    }

    /// A short name of the node type for error messages.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Node::Scalar {
                kind: Kind::Str, ..
            } => "string",
            Node::Scalar {
                kind: Kind::Number, ..
            } => "number",
            Node::Scalar {
                kind: Kind::Bool(_),
                ..
            } => "boolean",
            Node::Scalar {
                kind: Kind::Null, ..
            } => "null",
            Node::Map(_) => "object",
            Node::Seq(_) => "array",
        }
    }
}

fn plain_kind(text: &str) -> Kind {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Kind::Null,
        "true" | "True" | "TRUE" => return Kind::Bool(true),
        "false" | "False" | "FALSE" => return Kind::Bool(false),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF"
        | ".nan" | ".NaN" | ".NAN" => return Kind::Number,
        _ => {}
    }
    let b = text.as_bytes();
    if let Some(hex) = text.strip_prefix("0x")
        && !hex.is_empty()
        && hex.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Kind::Number;
    }
    if let Some(oct) = text.strip_prefix("0o")
        && !oct.is_empty()
        && oct.bytes().all(|c| (b'0'..=b'7').contains(&c))
    {
        return Kind::Number;
    }
    // [-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?
    let mut i = usize::from(matches!(b.first(), Some(b'-' | b'+')));
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - s;
    }
    if int_digits == 0 && frac_digits == 0 {
        return Kind::Str;
    }
    if i < b.len() && matches!(b[i], b'e' | b'E') {
        i += 1;
        if i < b.len() && matches!(b[i], b'-' | b'+') {
            i += 1;
        }
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s {
            return Kind::Str;
        }
    }
    if i == b.len() {
        Kind::Number
    } else {
        Kind::Str
    }
}

/// Parses every document of a YAML stream. An empty stream gives no documents.
pub(crate) fn parse_documents(src: &str) -> Result<Vec<Node>, String> {
    let mut parser = Parser::new_from_str(src);
    let mut anchors: HashMap<usize, Node> = HashMap::new();
    let mut documents = Vec::new();
    loop {
        let (event, _) = parser.next_token().map_err(|e| e.to_string())?;
        match event {
            Event::StreamEnd => break,
            Event::DocumentStart => {
                let (event, _) = parser.next_token().map_err(|e| e.to_string())?;
                if matches!(event, Event::DocumentEnd) {
                    documents.push(Node::Scalar {
                        text: String::new(),
                        kind: Kind::Null,
                    });
                    continue;
                }
                documents.push(build(event, &mut parser, &mut anchors, 0)?);
            }
            _ => {}
        }
    }
    Ok(documents)
}

/// The last document (pnpm 11 writes `---<env lockfile>---<lockfile>`), or an empty null.
pub(crate) fn parse_last(src: &str) -> Result<Node, String> {
    Ok(parse_documents(src)?.pop().unwrap_or(Node::Scalar {
        text: String::new(),
        kind: Kind::Null,
    }))
}

type Events<'a> = Parser<std::str::Chars<'a>>;

fn next_event(parser: &mut Events<'_>) -> Result<Event, String> {
    parser
        .next_token()
        .map(|(e, _)| e)
        .map_err(|e| e.to_string())
}

fn build(
    event: Event,
    parser: &mut Events<'_>,
    anchors: &mut HashMap<usize, Node>,
    depth: usize,
) -> Result<Node, String> {
    if depth > MAX_DEPTH {
        return Err("YAML is nested too deeply".into());
    }
    let (node, anchor) = match event {
        Event::Scalar(text, style, anchor, tag) => {
            let kind = match style {
                TScalarStyle::Plain if tag.as_ref().is_none_or(|t| t.suffix != "str") => {
                    plain_kind(&text)
                }
                _ => Kind::Str,
            };
            (Node::Scalar { text, kind }, anchor)
        }
        Event::SequenceStart(anchor, _) => {
            let mut items = Vec::new();
            loop {
                match next_event(parser)? {
                    Event::SequenceEnd => break,
                    event => items.push(build(event, parser, anchors, depth + 1)?),
                }
            }
            (Node::Seq(items), anchor)
        }
        Event::MappingStart(anchor, _) => {
            let mut entries = Vec::new();
            loop {
                match next_event(parser)? {
                    Event::MappingEnd => break,
                    event => {
                        let key = build(event, parser, anchors, depth + 1)?;
                        let value_event = next_event(parser)?;
                        let value = build(value_event, parser, anchors, depth + 1)?;
                        entries.push((key, value));
                    }
                }
            }
            (Node::Map(entries), anchor)
        }
        Event::Alias(id) => {
            return anchors
                .get(&id)
                .cloned()
                .ok_or_else(|| "YAML alias refers to an unknown or enclosing anchor".into());
        }
        other => return Err(format!("unexpected YAML event {other:?}")),
    };
    if anchor > 0 {
        anchors.insert(anchor, node.clone());
    }
    Ok(node)
}

/// A key that appears twice in one mapping, looking `depth` levels down. yarn keeps the last
/// value while [`Node::get`] returns the first, so berry rejects these.
pub(crate) fn duplicate_key(node: &Node, depth: u8) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    for (key, value) in node.entries() {
        if let Some(key) = key.text()
            && !seen.insert(key)
        {
            return Some(key.to_owned());
        }
        if depth > 0
            && let Some(key) = duplicate_key(value, depth - 1)
        {
            return Some(key);
        }
    }
    None
}

/// Writes `s` as a YAML scalar the way pnpm (js-yaml) does: plain when that reads back as the
/// same string, else single-quoted.
pub(crate) fn scalar(s: &str) -> String {
    if needs_quotes(s) {
        quote(s)
    } else {
        s.to_owned()
    }
}

/// Like [`scalar`], for the inside of a flow collection (`{integrity: ...}`, `[x64]`), where
/// `,{}[]` end a plain scalar.
pub(crate) fn flow_scalar(s: &str) -> String {
    if needs_quotes(s) || s.contains([',', '{', '}', '[', ']']) {
        quote(s)
    } else {
        s.to_owned()
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn needs_quotes(s: &str) -> bool {
    let Some(first) = s.chars().next() else {
        return true;
    };
    if plain_kind(s) != Kind::Str {
        return true;
    }
    if matches!(
        s,
        "y" | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "n"
            | "N"
            | "no"
            | "No"
            | "NO"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
    ) {
        return true;
    }
    let indicator = "-?:,[]{}#&*!|>'\"%@`".contains(first);
    // `-x` and `?x`, `:x` are plain; `- x` starts a sequence entry.
    let indicator_ok = matches!(first, '-' | '?' | ':') && s.len() > 1 && !s[1..].starts_with(' ');
    (indicator && !indicator_ok)
        || first.is_whitespace()
        || s.ends_with(char::is_whitespace)
        || s.ends_with(':')
        || s.contains(": ")
        || s.contains(" #")
        || s.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_scalar_text() {
        let doc = parse_last("a: 2.10\nb: 'x'\nc: true\nd:\ne: 1.2.3\nf: \"1\"\n").unwrap();
        assert_eq!(doc.get("a").unwrap().text(), Some("2.10"));
        assert_eq!(doc.get("a").unwrap().as_str(), None);
        assert_eq!(doc.get_str("b"), Some("x"));
        assert_eq!(doc.get("c").unwrap().as_bool(), Some(true));
        assert_eq!(doc.get("c").unwrap().text(), Some("true"));
        assert!(doc.get("d").unwrap().is_null());
        assert_eq!(doc.get("d").unwrap().text(), None);
        assert_eq!(doc.get_str("e"), Some("1.2.3"));
        assert_eq!(doc.get_str("f"), Some("1"));
    }

    #[test]
    fn flow_collections_and_aliases() {
        let src = "r: {integrity: sha512-x/y+z==, tarball: 'https://a/b.tgz'}\nos: [darwin, linux]\na: &x {k: v}\nb: *x\n";
        let doc = parse_last(src).unwrap();
        assert_eq!(
            doc.get("r").unwrap().get_str("integrity"),
            Some("sha512-x/y+z==")
        );
        assert_eq!(doc.get("os").unwrap().as_seq().unwrap().len(), 2);
        assert_eq!(doc.get("b").unwrap().get_str("k"), Some("v"));
    }

    #[test]
    fn multi_document_and_duplicates() {
        let docs = parse_documents("---\na: 1\n---\nb: 2\n").unwrap();
        assert_eq!(docs.len(), 2);
        assert!(
            parse_last("---\na: 1\n---\nb: 2\n")
                .unwrap()
                .get("b")
                .is_some()
        );
        let doc = parse_last("x:\n  a: 1\n  a: 2\n").unwrap();
        assert_eq!(duplicate_key(&doc, 0), None);
        assert_eq!(duplicate_key(&doc, 1).as_deref(), Some("a"));
        assert!(parse_last("a: [").is_err());
        assert!(parse_documents("").unwrap().is_empty());
    }

    #[test]
    fn quoting() {
        assert_eq!(scalar("1.2.3"), "1.2.3");
        assert_eq!(scalar("^1.0.0"), "^1.0.0");
        assert_eq!(scalar("9.0"), "'9.0'");
        assert_eq!(scalar("@types/node@1.0.0"), "'@types/node@1.0.0'");
        assert_eq!(scalar(">=1"), "'>=1'");
        assert_eq!(scalar(""), "''");
        assert_eq!(scalar("it's: x"), "'it''s: x'");
        assert_eq!(scalar("true"), "'true'");
        assert_eq!(scalar("link:../a"), "link:../a");
        assert_eq!(flow_scalar("a,b"), "'a,b'");
        for s in [
            "9.0", "@a/b@1", ">=1 <2", "x: y", "-", "- a", "true", "~", "a #b", "1e5", "0x1f",
            "-1", "yes", "*",
        ] {
            let doc = parse_last(&format!("k: {}\n", scalar(s))).unwrap();
            assert_eq!(doc.get_str("k"), Some(s), "{s}");
        }
    }
}
