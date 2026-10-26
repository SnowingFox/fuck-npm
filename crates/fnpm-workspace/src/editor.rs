//! Format-preserving package.json edits (bun `PackageJSONEditor.rs` rules, applied to text).
//!
//! Edits splice the original text, so indentation, key order, comments elsewhere, `\r\n` and
//! the final newline survive. Text that does not parse as a JSON(C) object falls back to a
//! full re-serialisation with the detected indentation.

use fnpm_types::DependencyGroup;
use serde_json::{Map, Value};

use crate::jsonc::{self, Member, Node, Object};

/// Edits package.json text while keeping indentation, key order, line endings and the
/// trailing newline (bun `PackageJSONEditor.rs`).
pub struct PackageJsonEditor {
    /// The current text; read it or call [`PackageJsonEditor::finish`].
    pub text: String,
}

impl PackageJsonEditor {
    /// Starts editing `text`.
    pub fn new(text: String) -> Self {
        Self { text }
    }

    /// Adds or replaces `name` in `group`, applying bun's move-to-group rule to the other
    /// groups: an entry in `peerDependencies` stays; when the target is `peerDependencies`, a
    /// `devDependencies` entry takes the same spec; every other entry is removed. An existing
    /// entry in `group` is replaced in place. A new key makes the group alphabetized, as bun
    /// rebuilds it sorted (a sorted group therefore gets the key in its sorted position).
    pub fn set_dependency(&mut self, group: DependencyGroup, name: &str, spec: &str) {
        for other in DependencyGroup::ALL {
            if other == group || !self.has_entry(other.field(), name) {
                continue;
            }
            if other == DependencyGroup::Peer {
                continue;
            }
            if group == DependencyGroup::Peer && other == DependencyGroup::Dev {
                self.put_entry(other.field(), name, spec);
            } else {
                self.remove_entry(other.field(), name);
            }
        }
        self.put_entry(group.field(), name, spec);
    }

    /// Removes `name` from every group (each of its keys, when a group repeats it), and a group
    /// once it is empty; returns whether anything changed.
    pub fn remove_dependency(&mut self, name: &str) -> bool {
        let mut changed = false;
        for group in DependencyGroup::ALL {
            while self.remove_entry(group.field(), name) {
                changed = true;
            }
        }
        changed
    }

    /// Sets a top-level or nested (`a.b.c`) key (`fnpm pkg set`, trustedDependencies). Missing
    /// intermediate objects are created and a non-object intermediate is replaced. A new key
    /// is appended to its object.
    pub fn set_path(&mut self, path: &str, value: Value) {
        let keys: Vec<&str> = path.split('.').collect();
        let Some(root) = jsonc::parse_spans(&self.text) else {
            self.fallback(|obj| set_in_map(obj, &keys, value));
            return;
        };
        let fmt = Format::detect(&self.text);
        let mut obj = &root;
        let mut depth = 0;
        for (i, key) in keys.iter().enumerate() {
            let last = i + 1 == keys.len();
            let rest_value = || nest(&keys[i + 1..], value.clone());
            match obj.get(key) {
                Some((_, m)) if last => {
                    let text = fmt.value(&value, depth + 1);
                    self.text.replace_range(m.value_start..m.value_end, &text);
                    return;
                }
                Some((_, m)) => match &m.value {
                    Node::Object(inner) => {
                        obj = inner;
                        depth += 1;
                    }
                    _ => {
                        let text = fmt.value(&rest_value(), depth + 1);
                        self.text.replace_range(m.value_start..m.value_end, &text);
                        return;
                    }
                },
                None => {
                    let v = if last { value.clone() } else { rest_value() };
                    let edit =
                        insert_member(&self.text, &fmt, obj, depth, obj.members.len(), key, &v);
                    edit.apply(&mut self.text);
                    return;
                }
            }
        }
    }

    /// Deletes a top-level or nested key; returns whether it existed.
    pub fn delete_path(&mut self, path: &str) -> bool {
        let keys: Vec<&str> = path.split('.').collect();
        let Some(root) = jsonc::parse_spans(&self.text) else {
            let mut found = false;
            self.fallback(|obj| found = delete_in_map(obj, &keys));
            return found;
        };
        let mut obj = &root;
        for (i, key) in keys.iter().enumerate() {
            let Some((idx, m)) = obj.get(key) else {
                return false;
            };
            if i + 1 == keys.len() {
                let edit = remove_member(&self.text, obj, idx);
                edit.apply(&mut self.text);
                return true;
            }
            match &m.value {
                Node::Object(inner) => obj = inner,
                _ => return false,
            }
        }
        false
    }

    /// The edited text.
    pub fn finish(self) -> String {
        self.text
    }

    fn has_entry(&self, list: &str, name: &str) -> bool {
        match jsonc::parse_spans(&self.text) {
            Some(root) => {
                matches!(root.get(list), Some((_, Member { value: Node::Object(o), .. })) if o.get(name).is_some())
            }
            None => crate::jsonc::parse_value(&self.text)
                .ok()
                .and_then(|v| v.get(list).and_then(|l| l.get(name)).map(|_| ()))
                .is_some(),
        }
    }

    fn put_entry(&mut self, list: &str, name: &str, spec: &str) {
        let Some(root) = jsonc::parse_spans(&self.text) else {
            self.fallback(|obj| {
                let group = obj
                    .entry(list.to_owned())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !group.is_object() {
                    *group = Value::Object(Map::new());
                }
                let group = group.as_object_mut().expect("object");
                let is_new = !group.contains_key(name);
                group.insert(name.to_owned(), Value::String(spec.to_owned()));
                if is_new {
                    let mut entries: Vec<(String, Value)> =
                        std::mem::take(group).into_iter().collect();
                    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                    group.extend(entries);
                }
            });
            return;
        };
        let fmt = Format::detect(&self.text);
        let spec_value = Value::String(spec.to_owned());
        let group = match root.get(list) {
            Some((
                _,
                Member {
                    value: Node::Object(o),
                    ..
                },
            )) => o,
            Some((_, m)) => {
                let mut map = Map::new();
                map.insert(name.to_owned(), spec_value);
                let text = fmt.value(&Value::Object(map), 1);
                self.text.replace_range(m.value_start..m.value_end, &text);
                return;
            }
            None => {
                let mut map = Map::new();
                map.insert(name.to_owned(), spec_value);
                let edit = insert_member(
                    &self.text,
                    &fmt,
                    &root,
                    0,
                    root.members.len(),
                    list,
                    &Value::Object(map),
                );
                edit.apply(&mut self.text);
                return;
            }
        };
        if let Some((_, m)) = group.get(name) {
            let text = serde_json::to_string(spec).expect("string serialises");
            self.text.replace_range(m.value_start..m.value_end, &text);
            return;
        }
        let sorted = group
            .members
            .windows(2)
            .all(|w| w[0].key.as_bytes() <= w[1].key.as_bytes());
        if sorted {
            let pos = group
                .members
                .iter()
                .position(|m| m.key.as_bytes() > name.as_bytes())
                .unwrap_or(group.members.len());
            let edit = insert_member(&self.text, &fmt, group, 1, pos, name, &spec_value);
            edit.apply(&mut self.text);
        } else {
            let edit = rebuild_sorted(&self.text, &fmt, group, 1, name, &spec_value);
            edit.apply(&mut self.text);
        }
    }

    fn remove_entry(&mut self, list: &str, name: &str) -> bool {
        let Some(root) = jsonc::parse_spans(&self.text) else {
            let mut found = false;
            self.fallback(|obj| {
                if let Some(Value::Object(group)) = obj.get_mut(list) {
                    found = group.shift_remove(name).is_some();
                    if found && group.is_empty() {
                        obj.shift_remove(list);
                    }
                }
            });
            return found;
        };
        let Some((
            gidx,
            Member {
                value: Node::Object(group),
                ..
            },
        )) = root.get(list)
        else {
            return false;
        };
        let Some((idx, _)) = group.get(name) else {
            return false;
        };
        let edit = if group.members.len() == 1 {
            remove_member(&self.text, &root, gidx)
        } else {
            remove_member(&self.text, group, idx)
        };
        edit.apply(&mut self.text);
        true
    }

    /// Edits through a full parse and re-serialisation (for text the span parser rejects).
    fn fallback(&mut self, f: impl FnOnce(&mut Map<String, Value>)) {
        let fmt = Format::detect(&self.text);
        let mut value = crate::jsonc::parse_value(&self.text).unwrap_or(Value::Object(Map::new()));
        if !value.is_object() {
            value = Value::Object(Map::new());
        }
        f(value.as_object_mut().expect("object"));
        let mut out = fmt.value(&value, 0);
        if fmt.final_newline {
            out.push_str(fmt.eol);
        }
        self.text = out;
    }
}

fn nest(keys: &[&str], value: Value) -> Value {
    keys.iter().rev().fold(value, |acc, k| {
        let mut m = Map::new();
        m.insert((*k).to_owned(), acc);
        Value::Object(m)
    })
}

fn set_in_map(obj: &mut Map<String, Value>, keys: &[&str], value: Value) {
    let (first, rest) = keys.split_first().expect("non-empty path");
    if rest.is_empty() {
        obj.insert((*first).to_owned(), value);
        return;
    }
    let child = obj
        .entry((*first).to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !child.is_object() {
        *child = Value::Object(Map::new());
    }
    set_in_map(child.as_object_mut().expect("object"), rest, value);
}

fn delete_in_map(obj: &mut Map<String, Value>, keys: &[&str]) -> bool {
    let (first, rest) = keys.split_first().expect("non-empty path");
    if rest.is_empty() {
        return obj.shift_remove(*first).is_some();
    }
    match obj.get_mut(*first) {
        Some(Value::Object(child)) => delete_in_map(child, rest),
        _ => false,
    }
}

/// Indentation, line ending and final newline of a file.
struct Format {
    indent: String,
    eol: &'static str,
    final_newline: bool,
}

impl Format {
    fn detect(text: &str) -> Self {
        let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let indent = text
            .lines()
            .skip(1)
            .find_map(|line| {
                let ws: String = line
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                (!ws.is_empty() && line.len() > ws.len()).then_some(ws)
            })
            .unwrap_or_else(|| "  ".to_owned());
        let final_newline = text.ends_with('\n') || text.is_empty();
        Format {
            indent,
            eol,
            final_newline,
        }
    }

    fn pad(&self, depth: usize) -> String {
        self.indent.repeat(depth)
    }

    /// `value` pretty-printed as the value of a member whose key sits at `depth` (members of the
    /// root are at depth 1; 0 prints a whole document).
    fn value(&self, value: &Value, depth: usize) -> String {
        use serde::Serialize;
        let mut buf = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(self.indent.as_bytes());
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
        value.serialize(&mut ser).expect("Value serialises");
        let s = String::from_utf8(buf).expect("serde_json writes UTF-8");
        let nl = format!("{}{}", self.eol, self.pad(depth));
        s.replace('\n', &nl)
    }
}

/// A text splice.
struct Edit {
    start: usize,
    end: usize,
    with: String,
}

impl Edit {
    fn apply(self, text: &mut String) {
        text.replace_range(self.start..self.end, &self.with);
    }
}

fn single_line(text: &str, obj: &Object) -> bool {
    !text[obj.open..obj.close].contains('\n') && !obj.members.is_empty()
}

/// Inserts `"key": value` as member `pos` of `obj`, whose own members sit at `depth + 1`.
fn insert_member(
    text: &str,
    fmt: &Format,
    obj: &Object,
    depth: usize,
    pos: usize,
    key: &str,
    value: &Value,
) -> Edit {
    let key_json = serde_json::to_string(key).expect("string serialises");
    let entry = format!("{key_json}: {}", fmt.value(value, depth + 1));
    if obj.members.is_empty() {
        let with = format!(
            "{{{eol}{}{entry}{eol}{}}}",
            fmt.pad(depth + 1),
            fmt.pad(depth),
            eol = fmt.eol
        );
        return Edit {
            start: obj.open,
            end: obj.close + 1,
            with,
        };
    }
    let compact = single_line(text, obj);
    if pos < obj.members.len() {
        let at = obj.members[pos].key_start;
        let sep = if compact {
            ", ".to_owned()
        } else {
            format!(",{}{}", fmt.eol, fmt.pad(depth + 1))
        };
        return Edit {
            start: at,
            end: at,
            with: format!("{entry}{sep}"),
        };
    }
    let last = obj.members.last().expect("non-empty");
    // Keep a trailing comma style: insert after it and add one.
    let after = &text[last.value_end..obj.close];
    let trailing = after
        .trim_start_matches([' ', '\t', '\r', '\n'])
        .starts_with(',');
    if compact {
        return Edit {
            start: last.value_end,
            end: last.value_end,
            with: format!(", {entry}"),
        };
    }
    if trailing {
        let comma = last.value_end + after.find(',').expect("comma");
        let with = format!("{}{}{entry},", fmt.eol, fmt.pad(depth + 1));
        return Edit {
            start: comma + 1,
            end: comma + 1,
            with,
        };
    }
    Edit {
        start: last.value_end,
        end: last.value_end,
        with: format!(",{}{}{entry}", fmt.eol, fmt.pad(depth + 1)),
    }
}

/// Removes member `idx` of `obj` with its separator.
fn remove_member(text: &str, obj: &Object, idx: usize) -> Edit {
    let n = obj.members.len();
    if n == 1 {
        return Edit {
            start: obj.open + 1,
            end: obj.close,
            with: String::new(),
        };
    }
    let m = &obj.members[idx];
    if idx + 1 < n {
        Edit {
            start: m.key_start,
            end: obj.members[idx + 1].key_start,
            with: String::new(),
        }
    } else {
        let prev = &obj.members[idx - 1];
        let _ = text;
        Edit {
            start: prev.value_end,
            end: m.value_end,
            with: String::new(),
        }
    }
}

/// Rewrites the members of `obj` sorted by key, with `key` added.
fn rebuild_sorted(
    text: &str,
    fmt: &Format,
    obj: &Object,
    depth: usize,
    key: &str,
    value: &Value,
) -> Edit {
    let key_json = serde_json::to_string(key).expect("string serialises");
    let mut entries: Vec<(&str, String)> = obj
        .members
        .iter()
        .map(|m| (m.key.as_str(), text[m.key_start..m.value_end].to_owned()))
        .collect();
    entries.push((key, format!("{key_json}: {}", fmt.value(value, depth + 1))));
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let sep = if obj.members.len() >= 2 {
        text[obj.members[0].value_end..obj.members[1].key_start].to_owned()
    } else if single_line(text, obj) {
        ", ".to_owned()
    } else {
        format!(",{}{}", fmt.eol, fmt.pad(depth + 1))
    };
    let first = obj.members.first().expect("non-empty");
    let last = obj.members.last().expect("non-empty");
    let joined = entries
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join(&sep);
    Edit {
        start: first.key_start,
        end: last.value_end,
        with: joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit(text: &str, f: impl FnOnce(&mut PackageJsonEditor)) -> String {
        let mut e = PackageJsonEditor::new(text.to_owned());
        f(&mut e);
        let out = e.finish();
        // Always valid JSONC afterwards.
        crate::jsonc::parse_value(&out).unwrap_or_else(|err| panic!("{err}\n{out}"));
        out
    }

    #[test]
    fn adds_group_when_missing() {
        let out = edit("{\n  \"name\": \"a\"\n}\n", |e| {
            e.set_dependency(DependencyGroup::Prod, "x", "^1.0.0")
        });
        assert_eq!(
            out,
            "{\n  \"name\": \"a\",\n  \"dependencies\": {\n    \"x\": \"^1.0.0\"\n  }\n}\n"
        );
    }

    #[test]
    fn keeps_four_space_indent_crlf_and_no_final_newline() {
        let out = edit("{\r\n    \"name\": \"a\"\r\n}", |e| {
            e.set_dependency(DependencyGroup::Dev, "x", "1")
        });
        assert_eq!(
            out,
            "{\r\n    \"name\": \"a\",\r\n    \"devDependencies\": {\r\n        \"x\": \"1\"\r\n    }\r\n}"
        );
        let out = edit("{\n\t\"name\": \"a\"\n}\n", |e| {
            e.set_dependency(DependencyGroup::Prod, "x", "1")
        });
        assert!(
            out.contains("\n\t\"dependencies\": {\n\t\t\"x\": \"1\"\n\t}\n}\n"),
            "{out}"
        );
    }

    #[test]
    fn sorted_insert_and_in_place_replace() {
        let text = "{\n  \"dependencies\": {\n    \"a\": \"1\",\n    \"c\": \"1\"\n  }\n}\n";
        let out = edit(text, |e| e.set_dependency(DependencyGroup::Prod, "b", "2"));
        assert_eq!(
            out,
            "{\n  \"dependencies\": {\n    \"a\": \"1\",\n    \"b\": \"2\",\n    \"c\": \"1\"\n  }\n}\n"
        );
        let out = edit(text, |e| e.set_dependency(DependencyGroup::Prod, "d", "2"));
        assert!(
            out.contains("\"c\": \"1\",\n    \"d\": \"2\"\n  }"),
            "{out}"
        );
        let out = edit(text, |e| e.set_dependency(DependencyGroup::Prod, "0", "2"));
        assert!(out.contains("{\n    \"0\": \"2\",\n    \"a\""), "{out}");
        let out = edit(text, |e| e.set_dependency(DependencyGroup::Prod, "c", "^3"));
        assert_eq!(out, text.replace("\"c\": \"1\"", "\"c\": \"^3\""));
    }

    #[test]
    fn unsorted_group_is_alphabetized_on_add_but_not_on_replace() {
        let text = "{\n  \"dependencies\": {\n    \"z\": \"1\",\n    \"a\": \"1\"\n  }\n}\n";
        let out = edit(text, |e| e.set_dependency(DependencyGroup::Prod, "m", "2"));
        assert_eq!(
            out,
            "{\n  \"dependencies\": {\n    \"a\": \"1\",\n    \"m\": \"2\",\n    \"z\": \"1\"\n  }\n}\n"
        );
        let out = edit(text, |e| e.set_dependency(DependencyGroup::Prod, "z", "2"));
        assert_eq!(out, text.replace("\"z\": \"1\"", "\"z\": \"2\""));
    }

    #[test]
    fn move_to_group() {
        let text = r#"{
  "dependencies": { "a": "1" },
  "devDependencies": {
    "a": "1",
    "b": "1"
  },
  "peerDependencies": {
    "a": "*"
  }
}
"#;
        let out = edit(text, |e| {
            e.set_dependency(DependencyGroup::Optional, "a", "2")
        });
        let v = crate::jsonc::parse_value(&out).unwrap();
        assert_eq!(v.get("dependencies"), None);
        assert_eq!(v["devDependencies"], json!({"b": "1"}));
        assert_eq!(v["peerDependencies"], json!({"a": "*"}));
        assert_eq!(v["optionalDependencies"], json!({"a": "2"}));
        // a peer target keeps dev in sync and leaves prod alone? bun removes it.
        let out = edit(text, |e| e.set_dependency(DependencyGroup::Peer, "a", "^2"));
        let v = crate::jsonc::parse_value(&out).unwrap();
        assert_eq!(v["devDependencies"]["a"], "^2");
        assert_eq!(v["peerDependencies"]["a"], "^2");
        assert_eq!(v.get("dependencies"), None);
    }

    #[test]
    fn compact_group() {
        let out = edit(r#"{"dependencies": {"a": "1"}}"#, |e| {
            e.set_dependency(DependencyGroup::Prod, "b", "2")
        });
        assert_eq!(out, r#"{"dependencies": {"a": "1", "b": "2"}}"#);
        let out = edit(r#"{"dependencies": {}}"#, |e| {
            e.set_dependency(DependencyGroup::Prod, "b", "2")
        });
        assert_eq!(out, "{\"dependencies\": {\n    \"b\": \"2\"\n  }}");
    }

    #[test]
    fn remove() {
        let text = "{\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"a\": \"1\",\n    \"b\": \"1\"\n  },\n  \"devDependencies\": {\n    \"a\": \"1\"\n  }\n}\n";
        let mut e = PackageJsonEditor::new(text.to_owned());
        assert!(e.remove_dependency("a"));
        assert_eq!(
            e.text,
            "{\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"b\": \"1\"\n  }\n}\n"
        );
        assert!(!e.remove_dependency("zzz"));
        assert!(e.remove_dependency("b"));
        assert_eq!(e.finish(), "{\n  \"name\": \"x\"\n}\n");
        let out = edit("{\"dependencies\": {\"a\": \"1\"}}", |e| {
            e.remove_dependency("a");
        });
        assert_eq!(out, "{}");
    }

    #[test]
    fn jsonc_trailing_comma_and_comments() {
        let text = "{\n  // comment\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"a\": \"1\", // keep\n  },\n}\n";
        let out = edit(text, |e| e.set_dependency(DependencyGroup::Prod, "b", "2"));
        assert!(out.contains("// comment"));
        let v = crate::jsonc::parse_value(&out).unwrap();
        assert_eq!(v["dependencies"], json!({"a": "1", "b": "2"}));
    }

    #[test]
    fn paths() {
        let text = "{\n  \"name\": \"x\",\n  \"scripts\": {\n    \"a\": \"b\"\n  }\n}\n";
        let out = edit(text, |e| e.set_path("scripts.test", json!("jest")));
        assert_eq!(
            out,
            "{\n  \"name\": \"x\",\n  \"scripts\": {\n    \"a\": \"b\",\n    \"test\": \"jest\"\n  }\n}\n"
        );
        let out = edit(text, |e| e.set_path("name", json!("y")));
        assert!(out.contains("\"name\": \"y\""));
        let out = edit(text, |e| e.set_path("a.b.c", json!(1)));
        assert!(
            out.ends_with("  \"a\": {\n    \"b\": {\n      \"c\": 1\n    }\n  }\n}\n"),
            "{out}"
        );
        let out = edit(text, |e| e.set_path("name.inner", json!(true)));
        assert_eq!(
            crate::jsonc::parse_value(&out).unwrap()["name"],
            json!({"inner": true})
        );
        let out = edit(text, |e| {
            e.set_path("trustedDependencies", json!(["a", "b"]))
        });
        assert!(
            out.contains("\"trustedDependencies\": [\n    \"a\",\n    \"b\"\n  ]\n}"),
            "{out}"
        );
        let mut e = PackageJsonEditor::new(text.to_owned());
        assert!(e.delete_path("scripts.a"));
        assert!(!e.delete_path("scripts.a"));
        assert!(!e.delete_path("name.x"));
        assert!(e.delete_path("name"));
        assert_eq!(e.finish(), "{\n  \"scripts\": {}\n}\n");
    }

    #[test]
    fn fallback_for_unparsable_spans() {
        // The span parser rejects a non-object root; the fallback rewrites the file.
        let out = edit("[]\n", |e| {
            e.set_dependency(DependencyGroup::Prod, "a", "1")
        });
        assert_eq!(out, "{\n  \"dependencies\": {\n    \"a\": \"1\"\n  }\n}\n");
        let out = edit("", |e| e.set_path("name", json!("x")));
        assert_eq!(out, "{\n  \"name\": \"x\"\n}\n");
    }
}
