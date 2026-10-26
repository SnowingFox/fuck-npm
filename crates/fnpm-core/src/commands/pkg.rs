//! `fnpm pm pkg get | set | delete | fix` (bun `pm_pkg_command.rs`). package.json is read with
//! bun's parser and written back whole, in its own indentation; errors go to stderr.

use std::path::Path;

use serde_json::{Map, Value};

use super::json_text::{self, ParseError};
use super::{Ctx, exit};
use crate::{CoreResult, Error, Result};

pub(super) const EMPTY_BRACKETS_GET: &str =
    "Empty brackets are not valid syntax for retrieving values.";
const EMPTY_BRACKETS_DELETE: &str = "Empty brackets are not valid syntax for deleting values.";

/// Prints `error: <message>` (and `note: <note>`) and fails without another word from the CLI.
fn fail(message: &str, note: Option<&str>) -> Error {
    eprintln!("error: {message}");
    if let Some(note) = note {
        eprintln!("note: {note}");
    }
    Error::Reported
}

/// `JSON.stringify(s)`, for keys in messages.
fn quote(s: &str) -> String {
    let mut out = String::new();
    json_text::write_string(&mut out, s);
    out
}

/// One step of a key path such as `contributors[0].name` or `keywords[]`.
#[derive(Clone, Copy)]
pub(super) struct Segment<'a> {
    /// An index when applied to an array, else a property name. Empty only for `[]` (append).
    name: &'a str,
    /// `a[b]` rather than `a.b`: `files[0]=x` and `files[]=x` create an array, `config.0=x`
    /// an object.
    bracketed: bool,
    /// The offset just past this segment in the key; errors name a value as `&key[..end]`.
    end: usize,
}

impl Segment<'_> {
    pub(super) fn is_append(self) -> bool {
        self.name.is_empty()
    }

    fn index(self) -> Option<usize> {
        decimal(self.name)
    }

    fn creates_array(self) -> bool {
        self.bracketed && (self.is_append() || self.index().is_some())
    }
}

fn decimal(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `[...]` holds one literal name, dots included. `None`: an unclosed `[`.
pub(super) fn parse_key_path(key: &str) -> Option<Vec<Segment<'_>>> {
    let mut segments = Vec::new();
    let mut start = 0;
    while start < key.len() {
        let end = key[start..]
            .find(['.', '['])
            .map_or(key.len(), |i| start + i);
        if end > start {
            segments.push(Segment {
                name: &key[start..end],
                bracketed: false,
                end,
            });
        }
        start = end + 1;
        if key.as_bytes().get(end) == Some(&b'[') {
            let close = start + key.get(start..)?.find(']')?;
            segments.push(Segment {
                name: &key[start..close],
                bracketed: true,
                end: close + 1,
            });
            start = close + 1;
        }
    }
    Some(segments)
}

struct PackageJson {
    path: std::path::PathBuf,
    root: Value,
    indent: String,
    trailing_newline: bool,
}

fn find_package_json(cwd: &Path) -> Result<std::path::PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let p = d.join("package.json");
        if p.is_file() {
            return Ok(p);
        }
        dir = d.parent();
    }
    Err(fail("No package.json was found for directory", None))
}

fn load(cwd: &Path) -> Result<PackageJson> {
    let path = find_package_json(cwd)?;
    let bytes = std::fs::read(&path).map_err(|e| {
        fail(
            &format!(
                "Failed to read package.json: {}",
                super::pack::out::errno_name(&e)
            ),
            None,
        )
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let root = json_text::parse_package_json(&text)
        .map_err(|ParseError(e)| fail(&format!("Failed to parse package.json: {e}"), None))?;
    if !root.is_object() {
        return Err(fail("package.json root must be an object", None));
    }
    Ok(PackageJson {
        indent: json_text::guess_indent(&text),
        trailing_newline: text.ends_with('\n'),
        path,
        root,
    })
}

/// Writes `root` back (or prints it with `--dry-run`).
fn save(pkg: &PackageJson, root: &Value, dry_run: bool) -> Result<Vec<String>> {
    let content = json_text::print(root, &pkg.indent);
    if dry_run {
        return Ok(content.split('\n').map(str::to_owned).collect());
    }
    let mut bytes = content.into_bytes();
    if pkg.trailing_newline {
        bytes.push(b'\n');
    }
    crate::project::write_atomic(&pkg.path, &bytes)
        .map_err(|e| fail(&format!("Failed to write package.json: {e}"), None))?;
    Ok(Vec::new())
}

fn help(version: &str) -> Vec<String> {
    let text = format!(
        "fnpm pm pkg v{version}
  Manage data in package.json

Subcommands:
  get [key ...]          Get values from package.json
  set key=value ...      Set values in package.json
    └ --json             Parse values as JSON (e.g. {{\"a\":1}})
  delete key ...         Delete keys from package.json
  fix                    Auto-correct common package.json errors

Examples:
  $ fnpm pm pkg get name version
  $ fnpm pm pkg set description=\"My awesome package\"
  $ fnpm pm pkg set keywords='[\"test\",\"demo\",\"example\"]' --json
  $ fnpm pm pkg set config='{{\"port\":3000,\"debug\":true}}' --json
  $ fnpm pm pkg set scripts.test=\"fnpm test\"
  $ fnpm pm pkg set bin.mycli=cli.js
  $ fnpm pm pkg set 'keywords[]=cli' 'files[0]=dist'
  $ fnpm pm pkg delete scripts.test devDependencies.webpack
  $ fnpm pm pkg delete 'keywords[0]'
  $ fnpm pm pkg fix"
    );
    text.lines().map(str::to_owned).collect()
}

/// `fnpm pm pkg <subcommand> [args]`. `rest` is the subcommand when the CLI sent it in the
/// command path; otherwise it is the first positional.
pub(crate) fn run(cx: &Ctx, rest: &[&str]) -> Result<CoreResult> {
    let mut args: Vec<&str> = cx.req.positionals.iter().map(String::as_str).collect();
    let sub = match rest.first() {
        Some(sub) => *sub,
        None if args.is_empty() => "help",
        None => args.remove(0),
    };
    let version = cx
        .string("cliVersion")
        .unwrap_or_else(|| crate::version().to_owned());
    let dry_run = cx.pm.dry_run || cx.flag("dryRun");
    match sub {
        "get" => get(cx, &args),
        "set" => set(cx, &args, dry_run),
        "delete" => delete(cx, &args, dry_run),
        "fix" => fix(cx, dry_run),
        "help" => Ok(exit(0, Value::Null, help(&version))),
        other => {
            eprintln!("error: Unknown subcommand: {other}");
            Ok(exit(1, Value::Null, help(&version)))
        }
    }
}

fn get(cx: &Ctx, keys: &[&str]) -> Result<CoreResult> {
    let pkg = load(&cx.req.cwd)?;
    if keys.is_empty() {
        let lines = json_text::print(&pkg.root, "  ")
            .lines()
            .map(str::to_owned)
            .collect();
        return Ok(exit(0, pkg.root, lines));
    }
    let mut found = Map::new();
    for key in keys {
        let path = parse_key_path(key).filter(|p| !p.iter().any(|s| s.is_append()));
        let Some(path) = path else {
            if key.contains("[]") {
                return Err(fail(EMPTY_BRACKETS_GET, None));
            }
            return Err(fail(&format!("Invalid key: {}", quote(key)), None));
        };
        query_in(&pkg.root, &path, key, &mut String::new(), false, &mut found);
    }
    let value = match found.len() {
        0 => Value::Object(Map::new()),
        1 => found
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(Value::Null),
        _ => Value::Object(found),
    };
    let lines = json_text::print(&value, "  ")
        .lines()
        .map(str::to_owned)
        .collect();
    Ok(exit(0, value, lines))
}

/// Like in npm, a name applied to an array (`maintainers.name`) reads `maintainers[0].name`,
/// and so on; `label` is the path walked so far, used once an array was `expanded`.
pub(super) fn query_in(
    value: &Value,
    path: &[Segment<'_>],
    key: &str,
    label: &mut String,
    expanded: bool,
    out: &mut Map<String, Value>,
) {
    let [segment, rest @ ..] = path else {
        let name = if expanded {
            label.clone()
        } else {
            key.to_owned()
        };
        out.entry(name).or_insert_with(|| value.clone());
        return;
    };
    let label_len = label.len();
    match value {
        Value::Array(items) => match segment.index() {
            Some(index) => {
                if let Some(item) = items.get(index) {
                    label.push_str(&format!("[{index}]"));
                    query_in(item, rest, key, label, expanded, out);
                }
            }
            None => {
                for (index, item) in items.iter().enumerate() {
                    label.push_str(&format!("[{index}]"));
                    query_in(item, path, key, label, true, out);
                    label.truncate(label_len);
                }
            }
        },
        Value::Object(map) => {
            if let Some(next) = map.get(segment.name) {
                if segment.bracketed {
                    label.push('[');
                } else if label_len > 0 {
                    label.push('.');
                }
                label.push_str(segment.name);
                if segment.bracketed {
                    label.push(']');
                }
                query_in(next, rest, key, label, expanded, out);
            }
        }
        _ => {}
    }
    label.truncate(label_len);
}

fn set(cx: &Ctx, args: &[&str], dry_run: bool) -> Result<CoreResult> {
    if args.is_empty() {
        return Err(fail(
            "fnpm pm pkg set expects a key=value pair of args",
            None,
        ));
    }
    let parse_json = cx.req.json || cx.flag("json");
    let pkg = load(&cx.req.cwd)?;
    let mut root = pkg.root.clone();
    for arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            return Err(fail(
                &format!("Invalid argument: {arg} (expected key=value)"),
                None,
            ));
        };
        if key.is_empty() {
            return Err(fail(&format!("Empty key in argument: {arg}"), None));
        }
        if value.is_empty() {
            return Err(fail(&format!("Empty value in argument: {arg}"), None));
        }
        let Some(path) = parse_key_path(key).filter(|p| !p.is_empty()) else {
            return Err(fail(&format!("Invalid key: {}", quote(key)), None));
        };
        let value = if parse_json {
            json_text::parse_strict(value).map_err(|ParseError(reason)| {
                fail(
                    &format!("Invalid JSON value for \"{key}\": {reason}"),
                    Some("--json parses each value as JSON. To store the text as a string, drop --json."),
                )
            })?
        } else {
            Value::String(value.to_owned())
        };
        set_in_container(&mut root, "package.json", key, &path, value)?;
    }
    let lines = save(&pkg, &root, dry_run)?;
    Ok(exit(0, root, lines))
}

/// Like in npm, only the final segment may replace a value; any other value in the way is an
/// error.
fn set_in_container(
    container: &mut Value,
    container_name: &str,
    key: &str,
    path: &[Segment<'_>],
    value: Value,
) -> Result<()> {
    let [segment, rest @ ..] = path else {
        return Ok(());
    };
    let slot_name = &key[..segment.end];
    if let Value::Array(items) = container {
        let len = items.len();
        let index = match segment.index() {
            _ if segment.is_append() => len,
            Some(index) if index <= len => index,
            Some(index) => {
                return Err(fail(
                    &format!(
                        "{}: index {index} is out of range for {} (length {len})",
                        quote(key),
                        quote(container_name)
                    ),
                    Some(&format!(
                        "{container_name}[] appends to the end of the array"
                    )),
                ));
            }
            None => {
                return Err(fail(
                    &format!(
                        "{}: {} is an array, so {} must be an index or []",
                        quote(key),
                        quote(container_name),
                        quote(segment.name)
                    ),
                    None,
                ));
            }
        };
        if index == len {
            items.push(Value::Null);
        }
        let Some(next) = rest.first() else {
            items[index] = value;
            return Ok(());
        };
        let child = &mut items[index];
        child_container(child, *next, key, slot_name)?;
        return set_in_container(child, slot_name, key, rest, value);
    }
    if segment.is_append() {
        return Err(fail(
            &format!(
                "{}: cannot append to {} because it is not an array",
                quote(key),
                quote(container_name)
            ),
            None,
        ));
    }
    let Value::Object(map) = container else {
        unreachable!("the root is an object and child_container makes arrays or objects");
    };
    let Some(next) = rest.first() else {
        map.insert(segment.name.to_owned(), value);
        return Ok(());
    };
    let child = map.entry(segment.name.to_owned()).or_insert(Value::Null);
    child_container(child, *next, key, slot_name)?;
    set_in_container(child, slot_name, key, rest, value)
}

/// A missing or `null` slot gets a new container shaped for `next`.
fn child_container(slot: &mut Value, next: Segment<'_>, key: &str, slot_name: &str) -> Result<()> {
    match slot {
        Value::Array(_) | Value::Object(_) => Ok(()),
        Value::Null => {
            *slot = if next.creates_array() {
                Value::Array(Vec::new())
            } else {
                Value::Object(Map::new())
            };
            Ok(())
        }
        _ => Err(fail(
            &format!(
                "{}: {} already exists and is not an object or array",
                quote(key),
                quote(slot_name)
            ),
            None,
        )),
    }
}

fn delete(cx: &Ctx, keys: &[&str], dry_run: bool) -> Result<CoreResult> {
    if keys.is_empty() {
        return Err(fail("fnpm pm pkg delete expects key args", None));
    }
    let pkg = load(&cx.req.cwd)?;
    let mut root = pkg.root.clone();
    let mut modified = false;
    for key in keys {
        let Some(path) = parse_key_path(key) else {
            return Err(fail(&format!("Invalid key: {}", quote(key)), None));
        };
        modified |= delete_in_container(&mut root, &path)?;
    }
    let lines = if modified {
        save(&pkg, &root, dry_run)?
    } else {
        Vec::new()
    };
    Ok(exit(0, root, lines))
}

/// Splices out an array element or removes a property; whether anything was removed.
fn delete_in_container(container: &mut Value, path: &[Segment<'_>]) -> Result<bool> {
    let [segment, rest @ ..] = path else {
        return Ok(false);
    };
    if segment.is_append() {
        return Err(fail(EMPTY_BRACKETS_DELETE, None));
    }
    let child = match container {
        Value::Array(items) => {
            let Some(index) = segment.index().filter(|&i| i < items.len()) else {
                return Ok(false);
            };
            if rest.is_empty() {
                items.remove(index);
                return Ok(true);
            }
            &mut items[index]
        }
        Value::Object(map) => {
            if rest.is_empty() {
                return Ok(map.shift_remove(segment.name).is_some());
            }
            let Some(child) = map.get_mut(segment.name) else {
                return Ok(false);
            };
            child
        }
        _ => return Ok(false),
    };
    delete_in_container(child, rest)
}

/// Lower-cases the name and warns about bins that do not exist.
fn fix(cx: &Ctx, dry_run: bool) -> Result<CoreResult> {
    let pkg = load(&cx.req.cwd)?;
    let mut root = pkg.root.clone();
    let mut modified = false;
    if let Some(Value::String(name)) = root.get("name") {
        let lower = name.to_ascii_lowercase();
        if *name != lower {
            root["name"] = Value::String(lower);
            modified = true;
        }
    }
    let dir = pkg.path.parent().unwrap_or(&cx.req.cwd);
    if let Some(Value::Object(bins)) = root.get("bin") {
        for target in bins.values() {
            let Value::String(target) = target else {
                continue;
            };
            // A path too long to open does not exist either.
            if !dir.join(target).exists() {
                eprintln!(
                    "warn: No bin file found at {}",
                    fnpm_types::text::escape_control_chars(target)
                );
            }
        }
    }
    let lines = if modified {
        save(&pkg, &root, dry_run)?
    } else {
        Vec::new()
    };
    Ok(exit(0, root, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_paths() {
        let names = |key: &str| -> Vec<(String, bool)> {
            parse_key_path(key)
                .unwrap()
                .iter()
                .map(|s| (s.name.to_owned(), s.bracketed))
                .collect()
        };
        assert_eq!(
            names("a.b[1].c[x.y][]"),
            [
                ("a".into(), false),
                ("b".into(), false),
                ("1".into(), true),
                ("c".into(), false),
                ("x.y".into(), true),
                (String::new(), true)
            ]
        );
        assert!(parse_key_path("a[b").is_none());
    }

    #[test]
    fn query_expands_arrays() {
        let root = serde_json::json!({"arr": [{"q": "}"}, {"q": "{}"}]});
        let mut out = Map::new();
        let path = parse_key_path("arr.q").unwrap();
        query_in(&root, &path, "arr.q", &mut String::new(), false, &mut out);
        assert_eq!(
            serde_json::Value::Object(out),
            serde_json::json!({"arr[0].q": "}", "arr[1].q": "{}"})
        );
    }
}
