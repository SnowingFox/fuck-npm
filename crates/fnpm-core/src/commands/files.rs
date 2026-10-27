//! package.json commands: `init` and `pm version` (`pm pkg` is in `pkg.rs`). `pm version`
//! reads and writes package.json like bun, with `json_text`.

use std::path::{Path, PathBuf};

use fnpm_semver::{ReleaseType, Version};
use serde_json::{Value, json};

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

/// Prints `error: <message>` and fails without another word from the CLI.
fn fail(message: &str) -> Error {
    eprintln!("error: {message}");
    Error::Reported
}

/// The largest major, minor or patch number node-semver accepts (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// `<increment>` applied to `current`, as `semver.inc` does.
fn increment_version(current: &str, rt: ReleaseType, preid: Option<&str>) -> Result<String> {
    let parsed = Version::parse(current)
        .ok()
        .filter(|v| v.major.max(v.minor).max(v.patch) <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            fail(&format!(
                "Current version \"{}\" is not a valid semver",
                fnpm_types::text::escape_control_chars(current)
            ))
        })?;
    parsed
        .try_inc(rt, preid)
        .map(|v| v.to_string())
        .map_err(|_| {
            fail(&format!(
                "Invalid --preid: \"{}\"",
                preid.unwrap_or_default()
            ))
        })
}

/// A version given as the argument, as `semver.clean` leaves it: an optional `v` or `=`, three
/// numbers no larger than `Number.MAX_SAFE_INTEGER`, a prerelease of valid identifiers, and no
/// build metadata.
fn specific_version(arg: &str) -> Option<String> {
    let text = arg.trim().trim_start_matches(['=', 'v']);
    let text = text.split_once('+').map_or(text, |(v, _)| v);
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let numbers: Vec<u64> = core
        .split('.')
        .map(|n| {
            (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .then(|| n.parse::<u64>().ok())
                .flatten()
                .filter(|&n| n <= MAX_SAFE_INTEGER)
        })
        .collect::<Option<_>>()?;
    let [major, minor, patch] = numbers[..] else {
        return None;
    };
    let mut out = format!("{major}.{minor}.{patch}");
    if let Some(pre) = pre {
        let ids: Vec<String> = pre
            .split('.')
            .map(|id| {
                let valid =
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
                valid.then(|| match id.parse::<u64>() {
                    Ok(n) if id.bytes().all(|b| b.is_ascii_digit()) && n < MAX_SAFE_INTEGER => {
                        n.to_string()
                    }
                    _ => id.to_owned(),
                })
            })
            .collect::<Option<_>>()?;
        out.push('-');
        out.push_str(&ids.join("."));
    }
    Some(out)
}

/// `fnpm pm version` without an increment: what each one would write.
fn version_help(cli: &str, declared: Option<&str>, preid: Option<&str>) -> Result<Vec<String>> {
    let current = declared.unwrap_or("1.0.0");
    let shown = fnpm_types::text::escape_control_chars(current);
    let next = |rt, preid| increment_version(current, rt, preid);
    let mut lines = vec![format!("fnpm pm version v{cli}")];
    if declared.is_some() {
        lines.push(format!("Current package version: v{shown}"));
    }
    lines.push(String::new());
    lines.push("Increment:".into());
    for (label, rt) in [
        ("patch     ", ReleaseType::Patch),
        ("minor     ", ReleaseType::Minor),
        ("major     ", ReleaseType::Major),
        ("prerelease", ReleaseType::Prerelease),
    ] {
        lines.push(format!("  {label} {shown} → {}", next(rt, preid)?));
    }
    if current.contains('-') || preid.is_some_and(|p| !p.is_empty()) {
        for (label, rt) in [
            ("prepatch  ", ReleaseType::Prepatch),
            ("preminor  ", ReleaseType::Preminor),
            ("premajor  ", ReleaseType::Premajor),
        ] {
            lines.push(format!("  {label} {shown} → {}", next(rt, preid)?));
        }
    }
    let beta = next(ReleaseType::Prerelease, Some("beta"))?;
    let text = format!(
        "  from-git   Use version from latest git tag
  1.2.3      Set specific version

Options:
  --no-git-tag-version Skip git operations
  --allow-same-version Prevents throwing error if version is the same
  --message=<val>, -m  Custom commit message, use %s for version substitution
  --preid=<val>        Prerelease identifier (i.e beta → {beta})
  --force, -f          Bypass dirty git history check
  --dry-run            Print the new version without writing, running scripts, or tagging

Examples:
  $ fnpm pm version patch
  $ fnpm pm version 1.2.3 --no-git-tag-version
  $ fnpm pm version prerelease --preid beta --message \"Release beta: %s\""
    );
    lines.extend(text.lines().map(str::to_owned));
    Ok(lines)
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

/// Runs `git args` in `dir`; `Some(stdout)` (trimmed) when it exited 0.
async fn git(dir: &Path, args: &[&str]) -> Result<Option<String>> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| {
            Error::usage(format!(
                "git must be installed to use `fnpm pm version` ({e})"
            ))
        })?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// `from-git`: the newest tag that looks like a version (`--match=*.*.*`, like npm), with any
/// prefix before the first digit (`v1.2.3`, `name@1.2.3`) dropped.
async fn version_from_git(dir: &Path) -> Result<Version> {
    let tag = git(dir, &["describe", "--tags", "--abbrev=0", "--match=*.*.*"])
        .await?
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::usage("No git tags found"))?;
    let at = tag.find(|c: char| c.is_ascii_digit()).unwrap_or(tag.len());
    Version::parse(&tag[at..])
        .map_err(|_| Error::usage(format!("Tag is not a valid version: \"{tag}\"")))
}

/// `fnpm pm version <increment>` (bun `pm_version_command.rs`): bumps package.json, runs
/// `preversion` / `version` / `postversion` with the `fnpm run` environment (unless
/// `--ignore-scripts`), and commits and tags `v<version>` when the package directory holds a
/// `.git` (unless `--no-git-tag-version`; a dirty tree is an error without `--force`).
/// `--dry-run` only prints the new version.
pub(crate) async fn version(cx: &Ctx) -> Result<CoreResult> {
    let path = nearest_package_json(&cx.req.cwd)?;
    let dir = path.parent().unwrap_or(&cx.req.cwd).to_path_buf();
    let text = read_text(&path)?;
    let mut value = match super::json_text::parse_package_json(&text) {
        Ok(v) if v.is_object() => v,
        Ok(_) => return Err(fail("Failed to parse package.json: root must be an object")),
        Err(e) => return Err(fail(&format!("Failed to parse package.json: {}", e.0))),
    };
    let declared = value
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let current_text = declared.clone().unwrap_or_else(|| "0.0.0".to_owned());
    let preid = cx.string("preid");
    let Some(increment) = cx.req.positionals.first().cloned() else {
        let cli = cx
            .string("cliVersion")
            .unwrap_or_else(|| crate::version().to_owned());
        let lines = version_help(&cli, declared.as_deref(), preid.as_deref())?;
        return Ok(ok(json!({ "version": current_text }), lines));
    };
    let dry_run = cx.pm.dry_run;
    let mut git_tag_version = match cx.opt("gitTagVersion") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s != "false",
        _ => !cx.flag("noGitTagVersion"),
    };
    // bun `verify_git`: only a `.git` in the package directory itself (a file in worktrees and
    // submodules) turns the commit and tag on.
    if git_tag_version {
        if !dir.join(".git").exists() {
            git_tag_version = false;
        } else if !cx.pm.force && !cx.flag("force") {
            let status = git(&dir, &["status", "--porcelain"]).await?;
            if status.is_none_or(|s| !s.is_empty()) {
                return Err(fail("Git working directory not clean."));
            }
        }
    }
    let next_text = if let Some(rt) = release_type(&increment) {
        increment_version(&current_text, rt, preid.as_deref())?
    } else if increment == "from-git" {
        version_from_git(&dir).await?.to_string()
    } else {
        specific_version(&increment).ok_or_else(|| {
            eprintln!("error: Invalid version argument: \"{increment}\"");
            eprintln!(
                "note: Valid options: patch, minor, major, prepatch, preminor, premajor, prerelease, from-git, or a specific semver version"
            );
            Error::Reported
        })?
    };
    if declared.as_deref() == Some(next_text.as_str()) && !cx.flag("allowSameVersion") {
        return Err(fail("Version not changed"));
    }
    let done = ok(
        json!({ "from": current_text, "to": next_text }),
        vec![format!("v{next_text}")],
    );
    if dry_run {
        return Ok(done);
    }

    let ignore_scripts = cx.pm.ignore_scripts
        || cx
            .project()
            .map(|p| p.config.ignore_scripts)
            .unwrap_or(false);
    let scripts = if ignore_scripts {
        Vec::new()
    } else {
        fnpm_scripts::run::scripts_of(&value)
    };
    let mut opts = super::scripts::run_options(cx, &dir);
    opts.env.extend([
        ("npm_command".to_owned(), "version".to_owned()),
        ("npm_old_version".to_owned(), current_text.clone()),
        ("npm_new_version".to_owned(), next_text.clone()),
    ]);
    let package = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let run = |event: &'static str| {
        let (scripts, opts, dir, package) = (&scripts, &opts, &dir, &package);
        async move {
            let Some((_, script)) = scripts.iter().find(|(k, _)| k == event) else {
                return Ok(());
            };
            let code = fnpm_scripts::run::run_lifecycle_script_foreground(dir, event, script, opts)
                .await
                .map_err(Error::Scripts)?;
            let _ = package;
            if code == 0 {
                Ok(())
            } else {
                Err(fail(&format!("script \"{event}\" exited with code {code}")))
            }
        }
    };
    run("preversion").await?;
    // Written back whole, like bun: a number that overflowed reads as null, which stays JSON.
    value["version"] = Value::String(next_text.clone());
    let mut out = super::json_text::print(&value, &super::json_text::guess_indent(&text));
    if text.ends_with('\n') {
        out.push('\n');
    }
    crate::project::write_atomic(&path, out.as_bytes())?;
    run("version").await?;
    if git_tag_version {
        let tag = format!("v{next_text}");
        let message = cx
            .string("message")
            .map_or_else(|| tag.clone(), |m| m.replace("%s", &next_text));
        if git(&dir, &["add", "package.json"]).await?.is_none() {
            return Err(Error::usage("Git add failed"));
        }
        if git(&dir, &["commit", "-m", &message]).await?.is_none() {
            return Err(Error::usage("Git commit failed"));
        }
        if git(&dir, &["tag", "-a", &tag, "-m", &tag]).await?.is_none() {
            return Err(Error::usage("Git tag failed"));
        }
    }
    run("postversion").await?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names() {
        assert_eq!(package_name_for(Path::new("/x/My App")), "my-app");
    }
}
