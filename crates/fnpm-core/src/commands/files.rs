//! package.json commands: `init`, `pm pkg get | set | delete | fix` and `pm version`. Edits go
//! through the format-preserving `PackageJsonEditor`.

use std::path::{Path, PathBuf};

use fnpm_semver::{ReleaseType, Version};
use fnpm_workspace::PackageJsonEditor;
use serde_json::{Map, Value, json};

use super::{Ctx, ok};
use crate::{CoreResult, Error, Result};

/// The nearest package.json from `cwd`.
fn nearest_package_json(cwd: &Path) -> Result<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let p = d.join("package.json");
        if p.is_file() {
            return Ok(p);
        }
        dir = d.parent();
    }
    Err(Error::usage("No package.json was found for directory"))
}

fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(Error::io(path))
}

fn parse(text: &str) -> Result<Value> {
    Ok(fnpm_workspace::PackageJson::parse(text)?.raw)
}

/// `npm init -y`'s name for a directory: lowercased, unsafe characters replaced.
fn package_name_for(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "package".into());
    let mut name: String = base
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_start_matches(['.', '_']).to_owned();
    if name.is_empty() {
        "package".into()
    } else {
        name
    }
}

/// `fnpm init [folder]`: writes a package.json (and nothing else; the TS CLI owns the
/// interactive template).
pub(crate) fn init(cx: &Ctx) -> Result<CoreResult> {
    let dir = match cx.req.positionals.first() {
        Some(f) => cx.req.cwd.join(f),
        None => cx.req.cwd.clone(),
    };
    std::fs::create_dir_all(&dir).map_err(Error::io(&dir))?;
    let path = dir.join("package.json");
    if path.exists() {
        let text = read_text(&path)?;
        let v = parse(&text)?;
        return Ok(ok(
            json!({ "created": false, "path": path, "packageJson": v }),
            vec![format!("package.json already exists at {}", path.display())],
        ));
    }
    let pj = json!({
        "name": package_name_for(&dir),
        "version": "1.0.0",
        "private": true,
        "type": "module",
        "main": "index.js",
        "scripts": {},
        "dependencies": {},
        "devDependencies": {},
    });
    let text = serde_json::to_string_pretty(&pj).unwrap_or_default() + "\n";
    crate::project::write_atomic(&path, text.as_bytes())?;
    Ok(ok(
        json!({ "created": true, "path": path, "packageJson": pj }),
        vec![format!("Wrote to {}", path.display())],
    ))
}

/// A property path: `a.b`, `a[0]`, `a.b[1].c`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Key(String),
    Index(usize),
}

fn segments(path: &str) -> Vec<Seg> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let mut rest = part;
        if let Some(i) = rest.find('[') {
            if i > 0 {
                out.push(Seg::Key(rest[..i].to_owned()));
            }
            rest = &rest[i..];
            while let Some(stripped) = rest.strip_prefix('[') {
                let Some(end) = stripped.find(']') else {
                    out.push(Seg::Key(rest.to_owned()));
                    break;
                };
                let inner = &stripped[..end];
                match inner.parse::<usize>() {
                    Ok(n) => out.push(Seg::Index(n)),
                    Err(_) => out.push(Seg::Key(inner.trim_matches(['"', '\'']).to_owned())),
                }
                rest = &stripped[end + 1..];
            }
        } else {
            out.push(Seg::Key(rest.to_owned()));
        }
    }
    out
}

fn get<'a>(v: &'a Value, segs: &[Seg]) -> Option<&'a Value> {
    segs.iter().try_fold(v, |cur, s| match s {
        Seg::Key(k) => cur.get(k),
        Seg::Index(i) => cur.get(*i),
    })
}

fn set(v: &mut Value, segs: &[Seg], value: Value) {
    let Some((first, rest)) = segs.split_first() else {
        *v = value;
        return;
    };
    match first {
        Seg::Key(k) => {
            if !v.is_object() {
                *v = Value::Object(Map::new());
            }
            let child = v
                .as_object_mut()
                .expect("object")
                .entry(k.clone())
                .or_insert(Value::Null);
            set(child, rest, value);
        }
        Seg::Index(i) => {
            if !v.is_array() {
                *v = Value::Array(Vec::new());
            }
            let arr = v.as_array_mut().expect("array");
            if arr.len() <= *i {
                arr.resize(i + 1, Value::Null);
            }
            set(&mut arr[*i], rest, value);
        }
    }
}

fn delete(v: &mut Value, segs: &[Seg]) -> bool {
    let Some((last, parents)) = segs.split_last() else {
        return false;
    };
    let mut cur = v;
    for s in parents {
        let next = match s {
            Seg::Key(k) => cur.get_mut(k),
            Seg::Index(i) => cur.get_mut(*i),
        };
        let Some(n) = next else { return false };
        cur = n;
    }
    match (last, cur) {
        (Seg::Key(k), Value::Object(m)) => m.shift_remove(k).is_some(),
        (Seg::Index(i), Value::Array(a)) if *i < a.len() => {
            a.remove(*i);
            true
        }
        _ => false,
    }
}

/// Dotted paths of plain keys go straight to the editor; anything with an index or a key
/// containing a dot rewrites the whole top-level value.
fn editor_path(segs: &[Seg]) -> Option<String> {
    let keys: Option<Vec<&str>> = segs
        .iter()
        .map(|s| match s {
            Seg::Key(k) if !k.contains('.') && !k.is_empty() => Some(k.as_str()),
            _ => None,
        })
        .collect();
    keys.map(|k| k.join("."))
}

fn set_with_editor(editor: &mut PackageJsonEditor, current: &Value, segs: &[Seg], value: Value) {
    if let Some(path) = editor_path(segs) {
        editor.set_path(&path, value);
        return;
    }
    let Some(Seg::Key(top)) = segs.first() else {
        return;
    };
    let mut top_value = current.get(top).cloned().unwrap_or(Value::Null);
    set(&mut top_value, &segs[1..], value);
    editor.set_path(top, top_value);
}

/// `fnpm pm pkg <get|set|delete|fix> ...`.
pub(crate) fn pkg(cx: &Ctx, rest: &[&str]) -> Result<CoreResult> {
    let path = nearest_package_json(&cx.req.cwd)?;
    let text = read_text(&path)?;
    let value = parse(&text)?;
    let args = &cx.req.positionals;
    match rest {
        ["get"] => {
            let data = match args.as_slice() {
                [] => value.clone(),
                [one] => get(&value, &segments(one)).cloned().unwrap_or(Value::Null),
                many => Value::Object(
                    many.iter()
                        .map(|k| {
                            (
                                k.clone(),
                                get(&value, &segments(k)).cloned().unwrap_or(Value::Null),
                            )
                        })
                        .collect(),
                ),
            };
            let lines = if data.is_null() {
                Vec::new()
            } else {
                serde_json::to_string_pretty(&data)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_owned)
                    .collect()
            };
            Ok(ok(data, lines))
        }
        ["set"] => {
            if args.is_empty() {
                return Err(Error::usage(
                    "fnpm pm pkg set expects a key=value pair of args",
                ));
            }
            let as_json = cx.req.json || cx.flag("json");
            let mut editor = PackageJsonEditor::new(text.clone());
            let mut current = value;
            for a in args {
                let Some((k, v)) = a.split_once('=') else {
                    return Err(Error::usage(format!(
                        "fnpm pm pkg set expects a key=value pair of args, got \"{a}\""
                    )));
                };
                if k.is_empty() {
                    return Err(Error::usage(format!("Invalid key: \"{a}\"")));
                }
                let v = if as_json {
                    serde_json::from_str(v)
                        .map_err(|e| Error::usage(format!("Invalid JSON value for \"{k}\": {e}")))?
                } else {
                    Value::String(v.to_owned())
                };
                let segs = segments(k);
                set_with_editor(&mut editor, &current, &segs, v.clone());
                set(&mut current, &segs, v);
            }
            let out = editor.finish();
            if out != text {
                crate::project::write_atomic(&path, out.as_bytes())?;
            }
            Ok(ok(current, Vec::new()))
        }
        ["delete"] => {
            if args.is_empty() {
                return Err(Error::usage("fnpm pm pkg delete expects key args"));
            }
            let mut editor = PackageJsonEditor::new(text.clone());
            let mut current = value;
            for k in args {
                let segs = segments(k);
                match editor_path(&segs) {
                    Some(p) => {
                        editor.delete_path(&p);
                    }
                    None => {
                        let Some(Seg::Key(top)) = segs.first() else {
                            continue;
                        };
                        let mut top_value = current.get(top).cloned().unwrap_or(Value::Null);
                        if delete(&mut top_value, &segs[1..]) {
                            editor.set_path(top, top_value);
                        }
                    }
                }
                delete(&mut current, &segs);
            }
            let out = editor.finish();
            if out != text {
                crate::project::write_atomic(&path, out.as_bytes())?;
            }
            Ok(ok(current, Vec::new()))
        }
        ["fix"] => fix(&path, &text, value),
        _ => Err(Error::UnknownCommand(
            std::iter::once("pkg".to_owned())
                .chain(rest.iter().map(|s| (*s).to_owned()))
                .collect(),
        )),
    }
}

/// `pm pkg fix`: npm's normalisations that are safe to apply to the file.
fn fix(path: &Path, text: &str, value: Value) -> Result<CoreResult> {
    let mut editor = PackageJsonEditor::new(text.to_owned());
    let mut fixed = Vec::new();
    if let Some(name) = value.get("name").and_then(Value::as_str) {
        let trimmed = name.trim();
        if trimmed != name {
            editor.set_path("name", Value::String(trimmed.to_owned()));
            fixed.push("name: trimmed whitespace".to_owned());
        }
    }
    if let Some(v) = value.get("version").and_then(Value::as_str)
        && let Ok(parsed) = Version::parse_loose(v)
    {
        let clean = parsed.to_string();
        if clean != v {
            editor.set_path("version", Value::String(clean.clone()));
            fixed.push(format!("version: \"{v}\" was cleaned to \"{clean}\""));
        }
    }
    if let Some(bin) = value.get("bin").and_then(Value::as_object) {
        let mut new = bin.clone();
        let mut changed = false;
        for (_, target) in new.iter_mut() {
            if let Some(t) = target.as_str()
                && let Some(stripped) = t.strip_prefix("./")
            {
                *target = Value::String(stripped.to_owned());
                changed = true;
            }
        }
        if changed {
            editor.set_path("bin", Value::Object(new));
            fixed.push("bin: removed leading ./ from targets".into());
        }
    }
    for group in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        let Some(map) = value.get(group).and_then(Value::as_object) else {
            continue;
        };
        let bad: Vec<&String> = map
            .iter()
            .filter(|(_, v)| !v.is_string())
            .map(|(k, _)| k)
            .collect();
        if bad.is_empty() {
            continue;
        }
        let mut keep = map.clone();
        for k in &bad {
            keep.shift_remove(*k);
            fixed.push(format!("{group}: removed invalid entry \"{k}\""));
        }
        editor.set_path(group, Value::Object(keep));
    }
    let out = editor.finish();
    if out != text {
        crate::project::write_atomic(path, out.as_bytes())?;
    }
    Ok(ok(json!({ "fixed": fixed }), fixed.clone()))
}

fn release_type(s: &str) -> Option<ReleaseType> {
    Some(match s {
        "major" => ReleaseType::Major,
        "minor" => ReleaseType::Minor,
        "patch" => ReleaseType::Patch,
        "premajor" => ReleaseType::Premajor,
        "preminor" => ReleaseType::Preminor,
        "prepatch" => ReleaseType::Prepatch,
        "prerelease" => ReleaseType::Prerelease,
        _ => return None,
    })
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `fnpm pm version <increment>`: bumps package.json, runs `preversion` / `version` /
/// `postversion`, and commits and tags (`v<version>`) in a clean git repository unless
/// `--no-git-tag-version`.
pub(crate) fn version(cx: &Ctx) -> Result<CoreResult> {
    let path = nearest_package_json(&cx.req.cwd)?;
    let dir = path.parent().unwrap_or(&cx.req.cwd).to_path_buf();
    let text = read_text(&path)?;
    let value = parse(&text)?;
    let current_text = value
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("0.0.0")
        .to_owned();
    let Some(increment) = cx.req.positionals.first().cloned() else {
        let lines = vec![
            format!("Current package version: v{current_text}"),
            String::new(),
            "Increment: patch, minor, major, prepatch, preminor, premajor, prerelease, from-git or a version".into(),
        ];
        return Ok(ok(json!({ "version": current_text }), lines));
    };
    let current = Version::parse(&current_text).map_err(|_| {
        Error::usage(format!(
            "Current version \"{current_text}\" is not a valid semver"
        ))
    })?;
    let preid = cx.string("preid");
    let in_git = git(&dir, &["rev-parse", "--is-inside-work-tree"]).is_some_and(|s| s == "true");
    let next = if let Some(rt) = release_type(&increment) {
        current
            .try_inc(rt, preid.as_deref())
            .map_err(|e| Error::usage(e.to_string()))?
    } else if increment == "from-git" {
        let tag = git(&dir, &["describe", "--tags", "--abbrev=0"])
            .ok_or_else(|| Error::usage("No git tags found"))?;
        Version::parse(tag.trim_start_matches('v'))
            .map_err(|_| Error::usage(format!("Git tag \"{tag}\" is not a valid version")))?
    } else {
        Version::parse(increment.trim_start_matches('v'))
            .map_err(|_| Error::usage(format!("Invalid version argument: \"{increment}\"")))?
    };
    let next_text = next.to_string();
    if next_text == current_text && !cx.flag("allowSameVersion") {
        return Err(Error::usage(format!("Version not changed: {next_text}")));
    }
    let tag_version = match cx.opt("gitTagVersion") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s != "false",
        _ => !cx.flag("noGitTagVersion"),
    };
    let do_git = tag_version && in_git;
    if do_git {
        let status = git(&dir, &["status", "--porcelain"]).unwrap_or_default();
        if !status.is_empty() {
            return Err(Error::usage("Git working directory not clean."));
        }
    }
    let scripts = fnpm_scripts::run::scripts_of(&value);
    let run = |event: &str| -> Result<()> {
        let Some((_, script)) = scripts.iter().find(|(k, _)| k == event) else {
            return Ok(());
        };
        let status = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                ["/d", "/s", "/c"].as_slice()
            } else {
                ["-c"].as_slice()
            })
            .arg(script)
            .current_dir(&dir)
            .env("npm_lifecycle_event", event)
            .env("npm_old_version", &current_text)
            .env("npm_new_version", &next_text)
            .status()
            .map_err(Error::io(&dir))?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Scripts(fnpm_scripts::Error::Failed {
                package: value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                script: event.to_owned(),
                code: status.code().unwrap_or(1),
            }))
        }
    };
    run("preversion")?;
    let mut editor = PackageJsonEditor::new(text.clone());
    editor.set_path("version", Value::String(next_text.clone()));
    crate::project::write_atomic(&path, editor.finish().as_bytes())?;
    run("version")?;
    if do_git {
        let message = cx
            .string("message")
            .map_or_else(|| format!("v{next_text}"), |m| m.replace("%s", &next_text));
        let rel = path
            .strip_prefix(&dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        git(&dir, &["add", &rel]).ok_or_else(|| Error::usage("git add failed"))?;
        git(&dir, &["commit", "-m", &message]).ok_or_else(|| Error::usage("git commit failed"))?;
        git(
            &dir,
            &["tag", "-a", &format!("v{next_text}"), "-m", &message],
        )
        .ok_or_else(|| Error::usage("git tag failed"))?;
    }
    run("postversion")?;
    Ok(ok(
        json!({ "from": current_text, "to": next_text }),
        vec![format!("v{next_text}")],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_parse() {
        assert_eq!(
            segments("a.b[1].c"),
            vec![
                Seg::Key("a".into()),
                Seg::Key("b".into()),
                Seg::Index(1),
                Seg::Key("c".into())
            ]
        );
        let mut v = json!({});
        set(&mut v, &segments("scripts.build"), json!("tsc"));
        set(&mut v, &segments("files[1]"), json!("dist"));
        assert_eq!(
            v,
            json!({ "scripts": { "build": "tsc" }, "files": [null, "dist"] })
        );
        assert!(delete(&mut v, &segments("scripts.build")));
        assert!(!delete(&mut v, &segments("nope.x")));
        assert_eq!(get(&v, &segments("files[1]")), Some(&json!("dist")));
        assert_eq!(editor_path(&segments("a.b")), Some("a.b".into()));
        assert_eq!(editor_path(&segments("a[0]")), None);
        assert_eq!(package_name_for(Path::new("/x/My App")), "my-app");
    }
}
