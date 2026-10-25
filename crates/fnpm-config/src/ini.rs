//! npm `ini` compatible parser, a port of bun's `src/ini/lib.rs` `Parser`.
//!
//! The result is the object npm's `ini.decode` builds, with npm's quirks: sections nest on `.`,
//! `key[]` appends, quoted values are `JSON.parse`d, `true`/`false`/`null` become their JSON
//! values, unquoted values lose an inline `;`/`#` comment, and `${VAR}` / `${VAR?}` expand from
//! the environment.

use std::fmt;

use crate::Env;

/// Past this many dot-separated segments the rest of a section header (dots included) is one
/// final segment, like bun.
const MAX_SECTION_ROPE_SEGMENTS: usize = 512;

/// Nesting limit of `${...${...}}` substitutions.
const MAX_ENV_SUBSTITUTION_DEPTH: usize = 32;

/// One value of a parsed INI document.
#[derive(Debug, Clone, PartialEq)]
pub enum IniValue {
    /// `null` (or a quoted JSON `null`).
    Null,
    /// `true`, `false`, or a key without `=` (`true`).
    Bool(bool),
    /// A quoted JSON number, e.g. `'1'`.
    Number(f64),
    /// Any other value.
    String(String),
    /// `key[] = …` lines, or a quoted JSON array.
    Array(Vec<IniValue>),
    /// A `[section]`, or a quoted JSON object.
    Object(IniObject),
}

impl IniValue {
    /// The value if it is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value if it is a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The value if it is an object.
    pub fn as_object(&self) -> Option<&IniObject> {
        match self {
            Self::Object(o) => Some(o),
            _ => None,
        }
    }

    fn from_json(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(b),
            serde_json::Value::Number(n) => Self::Number(n.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(s) => Self::String(s),
            serde_json::Value::Array(a) => {
                Self::Array(a.into_iter().map(Self::from_json).collect())
            }
            serde_json::Value::Object(o) => {
                let mut obj = IniObject::default();
                for (k, v) in o {
                    obj.put(k, Self::from_json(v));
                }
                Self::Object(obj)
            }
        }
    }
}

/// JavaScript's `String(value)` as bun's `ToStringFormatter` renders it (used where npm's ini
/// turns a JSON value into a property key).
impl fmt::Display for IniValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => f.write_str(s),
            Self::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                Ok(())
            }
            Self::Object(_) => f.write_str("[Object object]"),
        }
    }
}

/// An insertion-ordered object, like a JavaScript object: assigning an existing key replaces
/// its value in place.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IniObject {
    entries: Vec<(String, IniValue)>,
}

impl IniObject {
    /// The value of `key`.
    pub fn get(&self, key: &str) -> Option<&IniValue> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.entries.iter().position(|(k, _)| k == key)
    }

    /// Assigns `key`, keeping its position if it exists.
    pub fn put(&mut self, key: String, value: IniValue) {
        match self.position(&key) {
            Some(i) => self.entries[i].1 = value,
            None => self.entries.push((key, value)),
        }
    }

    /// Entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &IniValue)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of keys.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No keys.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Walks `path` from `self`, creating missing objects. `None` when a segment exists but is
    /// not an object (npm's ini would assign onto a string, which does nothing).
    fn get_or_put_object(&mut self, path: &[String]) -> Option<&mut IniObject> {
        let mut cur = self;
        for segment in path {
            let i = match cur.position(segment) {
                Some(i) => i,
                None => {
                    cur.entries
                        .push((segment.clone(), IniValue::Object(IniObject::default())));
                    cur.entries.len() - 1
                }
            };
            match &mut cur.entries[i].1 {
                IniValue::Object(o) => cur = o,
                _ => return None,
            }
        }
        Some(cur)
    }
}

/// `String.prototype.trim`'s whitespace set, which npm's ini uses (wider than ASCII).
pub(crate) fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub(crate) fn trim_js(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

fn trim_js_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

fn should_skip_line(line: &str) -> bool {
    let line = trim_js(line);
    line.is_empty() || line.starts_with(';') || line.starts_with('#')
}

fn is_quoted(val: &str) -> bool {
    (val.starts_with('"') && val.ends_with('"')) || (val.starts_with('\'') && val.ends_with('\''))
}

/// Index of the first `;` or `#` that a backslash does not escape.
fn inline_comment_start(val: &str) -> Option<usize> {
    let bytes = val.as_bytes();
    let mut from = 0;
    while from < bytes.len() {
        let at = from
            + bytes[from..]
                .iter()
                .position(|b| matches!(b, b'\\' | b';' | b'#'))?;
        if bytes[at] != b'\\' {
            return Some(at);
        }
        from = at + 2;
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Usage {
    Section,
    Key,
    Value,
}

enum Prepared {
    Value(IniValue),
    Section(Vec<String>),
    Key(String),
}

/// Parses an INI document. Never fails: like npm's ini, anything it cannot read is skipped.
pub fn parse(text: &str, env: &Env) -> IniObject {
    let mut root = IniObject::default();
    // `None` is the root; otherwise the path of the current `[section]`.
    let mut head: Vec<String> = Vec::new();
    let mut skip_until_next_section = false;

    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if should_skip_line(line) {
            continue;
        }

        if line.starts_with('[') {
            skip_until_next_section = false;
            let mut treat_as_key = false;
            match line.find(']') {
                None => {}
                Some(close) if !trim_js(&line[close + 1..]).is_empty() => treat_as_key = true,
                Some(close) => {
                    let Prepared::Section(path) = prepare_str(env, Usage::Section, &line[1..close])
                    else {
                        unreachable!("section usage yields a section")
                    };
                    if root.get_or_put_object(&path).is_some() {
                        head = path;
                    } else {
                        skip_until_next_section = true;
                    }
                }
            }
            if !treat_as_key {
                continue;
            }
        }
        if skip_until_next_section {
            continue;
        }

        let eq = line.find('=');
        let Prepared::Key(key_raw) =
            prepare_str(env, Usage::Key, &line[..eq.unwrap_or(line.len())])
        else {
            unreachable!("key usage yields a key")
        };
        let is_array = key_raw.len() > 2 && key_raw.ends_with("[]");
        let key = if is_array {
            key_raw[..key_raw.len() - 2].to_owned()
        } else {
            key_raw
        };
        if key == "__proto__" {
            continue;
        }

        let value_raw = match eq {
            Some(eq) if eq + 1 < line.len() => {
                match prepare_str(env, Usage::Value, &line[eq + 1..]) {
                    Prepared::Value(v) => v,
                    _ => unreachable!("value usage yields a value"),
                }
            }
            Some(_) => IniValue::String(String::new()),
            None => IniValue::Bool(true),
        };
        let value = match value_raw {
            IniValue::String(s) if s == "true" => IniValue::Bool(true),
            IniValue::String(s) if s == "false" => IniValue::Bool(false),
            IniValue::String(s) if s == "null" => IniValue::Null,
            other => other,
        };

        let Some(obj) = root.get_or_put_object(&head) else {
            continue;
        };
        if is_array {
            match obj.get(&key) {
                Some(IniValue::Array(_)) => {}
                Some(existing) => {
                    let existing = existing.clone();
                    obj.put(key.clone(), IniValue::Array(vec![existing]));
                }
                None => obj.put(key.clone(), IniValue::Array(Vec::new())),
            }
        }
        // A key that is already an array keeps growing even without `[]`.
        match obj.position(&key) {
            Some(i) => match &mut obj.entries[i].1 {
                IniValue::Array(items) => items.push(value),
                slot => *slot = value,
            },
            None => obj.entries.push((key, value)),
        }
    }
    root
}

/// Splits a section name on `.` (no escapes; used for quoted names).
fn str_to_rope(key: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = key;
    loop {
        if parts.len() + 1 >= MAX_SECTION_ROPE_SEGMENTS {
            parts.push(rest.to_owned());
            break;
        }
        match rest.find('.') {
            Some(dot) => {
                parts.push(rest[..dot].to_owned());
                rest = &rest[dot + 1..];
            }
            None => {
                parts.push(rest.to_owned());
                break;
            }
        }
    }
    parts
}

fn prepare_str(env: &Env, usage: Usage, raw: &str) -> Prepared {
    let mut val = trim_js(raw);

    if is_quoted(val) {
        if val.starts_with('\'') {
            val = if val.len() > 1 {
                &val[1..val.len() - 1]
            } else {
                &val[1..]
            };
        }
        // `JSON.parse("")` throws; fall through to the plain-string path.
        if !val.is_empty() {
            match serde_json::from_str::<serde_json::Value>(val) {
                Err(_) => {
                    if usage == Usage::Value {
                        return Prepared::Value(IniValue::String(env.expand(val)));
                    }
                }
                Ok(serde_json::Value::String(s)) => {
                    return match usage {
                        Usage::Value => Prepared::Value(IniValue::String(env.expand(&s))),
                        Usage::Section => Prepared::Section(str_to_rope(&s)),
                        Usage::Key => Prepared::Key(s),
                    };
                }
                Ok(json) => {
                    let json = IniValue::from_json(json);
                    return match usage {
                        Usage::Value => Prepared::Value(json),
                        Usage::Section => Prepared::Section(vec![json.to_string()]),
                        Usage::Key => Prepared::Key(json.to_string()),
                    };
                }
            }
        }
        return match usage {
            Usage::Value => Prepared::Value(IniValue::String(val.to_owned())),
            Usage::Key => Prepared::Key(val.to_owned()),
            Usage::Section => Prepared::Section(str_to_rope(val)),
        };
    }

    // Walk to the first unescaped comment character, unescaping as npm's `unsafe()` does.
    let mut did_any_escape = false;
    let mut esc = false;
    let mut comment_cut = false;
    let mut unesc = String::with_capacity(val.len());
    let mut rope: Vec<String> = Vec::new();

    let mut i = 0;
    while i < val.len() {
        let c = val[i..]
            .chars()
            .next()
            .expect("i is a char boundary below len");
        let clen = c.len_utf8();
        if esc {
            match c {
                '\\' => unesc.push('\\'),
                ';' | '#' | '$' => unesc.push(c),
                '.' if usage == Usage::Section => unesc.push('.'),
                _ => {
                    unesc.push('\\');
                    unesc.push(c);
                }
            }
            esc = false;
        } else {
            match c {
                '$' => {
                    if usage == Usage::Value {
                        // npm cuts the comment before it expands `${VAR}`, so the trim never
                        // touches the variable's own whitespace.
                        if !comment_cut {
                            comment_cut = true;
                            if let Some(at) = inline_comment_start(&val[i..]) {
                                val = trim_js_end(&val[..i + at]);
                            }
                        }
                        if let Some(close) = parse_env_substitution(env, val, i, i, 0, &mut unesc) {
                            did_any_escape = true;
                            i = close + 1;
                            continue;
                        }
                    }
                    unesc.push('$');
                }
                ';' | '#' => {
                    let kept = trim_js_end(&unesc).len();
                    unesc.truncate(kept);
                    did_any_escape = true;
                    break;
                }
                '\\' => {
                    esc = true;
                    did_any_escape = true;
                }
                '.' if usage == Usage::Section && rope.len() < MAX_SECTION_ROPE_SEGMENTS => {
                    rope.push(std::mem::take(&mut unesc));
                }
                _ => unesc.push(c),
            }
        }
        i += clen;
    }
    if esc {
        unesc.push('\\');
    }

    match usage {
        Usage::Section => {
            rope.push(unesc);
            Prepared::Section(rope)
        }
        Usage::Value if !did_any_escape => Prepared::Value(IniValue::String(val.to_owned())),
        Usage::Value => Prepared::Value(IniValue::String(unesc)),
        Usage::Key if !did_any_escape => Prepared::Key(val.to_owned()),
        Usage::Key => Prepared::Key(unesc),
    }
}

/// npm's env-replace regex selects the innermost `${...}`. `i` points at a `$`. Returns the
/// index of the closing `}` when a substitution happened (its text is appended to `unesc`,
/// preceded by whatever of `val[start..i]` an outer, abandoned `${` skipped).
fn parse_env_substitution(
    env: &Env,
    val: &str,
    start: usize,
    i: usize,
    depth: usize,
    unesc: &mut String,
) -> Option<usize> {
    if depth >= MAX_ENV_SUBSTITUTION_DEPTH {
        return None;
    }
    let bytes = val.as_bytes();
    if !(i + 2 < bytes.len() && bytes[i + 1] == b'{') {
        return None;
    }
    let mut esc = false;
    let mut j = i + 2;
    let mut found_closing = false;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => esc = !esc,
            b'$' if !esc => return parse_env_substitution(env, val, start, j, depth + 1, unesc),
            b'{' if !esc => return None,
            b'}' if !esc => {
                found_closing = true;
                break;
            }
            _ => {}
        }
        j += 1;
    }
    if !found_closing {
        return None;
    }
    let raw = &val[i + 2..j];
    let (name, optional) = match raw.strip_suffix('?') {
        Some(name) => (name, true),
        None => (raw, false),
    };
    // https://github.com/npm/cli/blob/534ad77/workspaces/config/lib/env-replace.js#L6
    match env.get(name) {
        Some(value) => {
            unesc.push_str(&val[start..i]);
            unesc.push_str(value);
        }
        None if optional => unesc.push_str(&val[start..i]),
        None => return None,
    }
    Some(j)
}

#[cfg(test)]
#[path = "tests/ini.rs"]
mod tests;
