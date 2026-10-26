//! Folder and GitHub templates, copied only after destination conflicts are checked.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use fnpm_config::Env;
use fnpm_scripts::env::ScriptContext;
use serde_json::Value;

use super::{Out, Request};
use crate::options::{InstallCommand, InstallOptions, PmOptions};
use crate::{Error, Result};

fn local_source(cwd: &Path, name: &str, env: &Env) -> Option<PathBuf> {
    let absolute = Path::new(name).is_absolute();
    if absolute || name.starts_with(['.', '~']) {
        let path = if let Some(s) = name.strip_prefix("~/") {
            PathBuf::from(env.get("HOME")?).join(s)
        } else {
            cwd.join(name)
        };
        return path.is_dir().then_some(path);
    }
    if Path::new(name)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    let mut roots = Vec::new();
    if let Some(root) = env
        .get_not_empty("FNPM_CREATE_DIR")
        .or_else(|| env.get_not_empty("BUN_CREATE_DIR"))
    {
        roots.push(cwd.join(root));
    }
    roots.push(cwd.join("bun-create"));
    if let Some(home) = env.get_not_empty("HOME") {
        roots.push(PathBuf::from(home).join(".bun-create"));
    }
    roots.into_iter().map(|p| p.join(name)).find(|p| p.is_dir())
}

fn github(name: &str) -> Option<(String, String, String)> {
    let name = name
        .strip_prefix("https://github.com/")
        .or_else(|| name.strip_prefix("http://github.com/"))
        .or_else(|| name.strip_prefix("github.com/"))
        .or_else(|| name.strip_prefix("github:"))
        .unwrap_or(name);
    let (name, branch) = name.split_once('#').unwrap_or((name, ""));
    let (owner, repo) = name.split_once('/')?;
    let repo = repo.trim_end_matches('/').trim_end_matches(".git");
    let safe = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            && !matches!(s, "." | "..")
    };
    if !safe(owner)
        || !safe(repo)
        || branch
            .chars()
            .any(|c| c.is_control() || matches!(c, '?' | '#' | '\\'))
    {
        return None;
    }
    Some((owner.into(), repo.into(), branch.into()))
}

/// A template request, or `None` when `create-<name>` should handle it.
pub(super) fn maybe_run(req: &Request, out: &Out) -> Option<i32> {
    if req
        .args
        .iter()
        .any(|s| matches!(s.as_str(), "--help" | "-h"))
    {
        return None;
    }
    let opts = match super::args::parse_create(&req.args, &req.cwd) {
        super::Parsed::Run(opts) => opts,
        super::Parsed::Error { message, .. } => {
            out.error(&message);
            return Some(1);
        }
        _ => return None,
    };
    let index = req.args.len() - opts.passthrough.len() - 1;
    let name = opts.package_name.as_str();
    if name.len() > 4096 {
        out.error("template name too long");
        return Some(1);
    }
    let folded = name.replace('\\', "/");
    if folded.is_empty() || matches!(folded.as_str(), "." | "..") || folded.ends_with("/..") {
        out.error(&format!(
            "{} is not a template name.",
            format_args!("\"{}\"", crate::security::escape_control(name, false))
        ));
        return Some(1);
    }
    let env = Env::from_process();
    let source = local_source(&opts.cwd, name, &env);
    let remote = source.is_none().then(|| github(name)).flatten();
    if source.is_none() && remote.is_none() {
        return None;
    }
    let req = Request {
        cwd: opts.cwd.clone(),
        ..req.clone()
    };
    let pm = PmOptions {
        cwd: opts.cwd.clone(),
        registry: opts.registry,
        ca: opts.ca,
        cafile: opts.cafile.map(|path| opts.cwd.join(path)),
        ca_store: opts.ca_store.map(str::to_owned),
        env_file: opts.env_files.iter().map(PathBuf::from).collect(),
        ..Default::default()
    };
    match run(&req, index, &pm, source, remote, &env) {
        Ok(code) => Some(code),
        Err(Error::Reported) => Some(1),
        Err(e) => {
            out.error(&e.to_string());
            Some(1)
        }
    }
}

fn deepest_existing(path: &Path) -> Result<PathBuf> {
    let mut path = path;
    loop {
        match std::fs::canonicalize(path) {
            Ok(p) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                path = path
                    .parent()
                    .ok_or_else(|| Error::usage("destination has no existing parent"))?;
            }
            Err(e) => {
                return Err(Error::Io {
                    path: path.into(),
                    source: e,
                });
            }
        }
    }
}

fn relative_path(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    if path.is_empty()
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains([':', '\0']))
        || Path::new(&path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(Error::usage(format!(
            "unsafe template path: {}",
            crate::security::escape_control(&path, false)
        )));
    }
    Ok(path)
}

fn files(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();
    for file in fnpm_store::pack::walk_files(root)? {
        let path = relative_path(&file.path)?;
        if path
            .split('/')
            .any(|p| matches!(p, ".git" | "node_modules"))
            || matches!(
                path.as_str(),
                ".npmignore" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | ".git"
            )
        {
            continue;
        }
        if files.insert(path.clone(), root.join(file.path)).is_some() {
            return Err(Error::usage(format!(
                "duplicate template path: {}",
                crate::security::escape_control(&path, false)
            )));
        }
    }
    if let Some(source) = files.remove("gitignore") {
        files.entry(".gitignore".into()).or_insert(source);
    }
    Ok(files)
}

fn destination_links(dest: &Path, rel: &str) -> Vec<PathBuf> {
    let mut path = dest.to_owned();
    let mut links = Vec::new();
    for c in Path::new(rel).components() {
        path.push(c);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
            links.push(path.clone());
            break;
        }
    }
    links
}

fn task(dest: &Path, json: &Value, env: &Env, event: &str, value: &Value) -> Result<i32> {
    let tasks: Vec<_> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for script in tasks {
        let actual = json
            .get("scripts")
            .and_then(|s| s.get(script))
            .and_then(Value::as_str)
            .unwrap_or(script);
        let pairs: Vec<_> = env.iter().map(|(k, v)| (k.into(), v.into())).collect();
        let package_json = dest.join("package.json");
        let context = ScriptContext {
            project_dir: dest,
            package_dir: dest,
            package_json: &package_json,
            package_name: json["name"].as_str().unwrap_or(""),
            package_version: json["version"].as_str().unwrap_or(""),
            config: &[],
            event,
            script: actual,
        };
        let pairs = fnpm_scripts::env::build(&context, &pairs);
        let path = pairs
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_os_str())
            .unwrap_or_default();
        let shell = fnpm_scripts::shell::select(None, path, dest)
            .ok_or_else(|| Error::usage("no shell found for create task"))?;
        if actual != script {
            eprintln!("$ fnpm run {script}");
        } else {
            eprintln!("$ {actual}");
        }
        let status = Command::new(shell.program)
            .args(shell.flags)
            .arg(actual)
            .current_dir(dest)
            .env_clear()
            .envs(pairs)
            .status()
            .map_err(Error::io(dest))?;
        if !status.success() {
            let code = status.code().unwrap_or(1);
            eprintln!("error: script \"{event}\" exited with code {code}");
            return Ok(code);
        }
    }
    Ok(0)
}

fn git(dest: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(dest)
        .stdin(Stdio::null())
        .output()
        .ok()
}

fn init_git(dest: &Path) -> bool {
    let started = std::time::Instant::now();
    if !git(dest, &["init", "--quiet"]).is_some_and(|o| o.status.success()) {
        return false;
    }
    if git(dest, &["add", "."]).is_some_and(|o| o.status.success()) {
        let commit = git(
            dest,
            &[
                "commit",
                "--quiet",
                "-m",
                "Initial commit (via fnpm create)",
            ],
        );
        if let Some(output) = commit.filter(|o| !o.status.success()) {
            let message = String::from_utf8_lossy(&output.stderr);
            if message.contains("Please tell me who you are")
                || message.contains("no email was given")
                || message.contains("unable to auto-detect email address")
            {
                eprintln!(
                    "note: git has no user.name and user.email, files staged but not committed"
                );
            }
        }
    }
    eprintln!("[{:.2}ms] git", started.elapsed().as_secs_f64() * 1000.0);
    true
}

fn run(
    req: &Request,
    index: usize,
    pm: &PmOptions,
    source: Option<PathBuf>,
    remote: Option<(String, String, String)>,
    env: &Env,
) -> Result<i32> {
    let name = &req.args[index];
    let force = req.args.iter().any(|s| s == "--force");
    let no_install = req.args.iter().any(|s| s == "--no-install");
    let no_git = req.args.iter().any(|s| s == "--no-git");
    let no_json = req.args.iter().any(|s| s == "--no-package-json");
    let ignore_scripts = req.args.iter().any(|s| s == "--ignore-scripts");
    let dest_arg = req.args[index + 1..].iter().find(|s| !s.starts_with('-'));
    let label = remote
        .as_ref()
        .map(|(o, r, _)| format!("{o}/{r}"))
        .unwrap_or_else(|| name.to_owned());
    let default_dest = remote
        .as_ref()
        .map(|(_, r, _)| r.as_str())
        .unwrap_or_else(|| {
            Path::new(name)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("app")
        });
    let dest = super::normalize(
        &req.cwd
            .join(dest_arg.map(String::as_str).unwrap_or(default_dest)),
    );
    let parent =
        crate::project::Project::load_anywhere(&req.cwd, pm.to_cli_overrides(), env.clone())?;
    let staging;
    let source = if let Some(source) = source {
        source
    } else {
        let (owner, repo, branch) = remote
            .as_ref()
            .ok_or_else(|| Error::usage("invalid GitHub template"))?;
        staging = tempfile::tempdir().map_err(Error::io(std::env::temp_dir()))?;
        let base = env
            .get_not_empty("GITHUB_API_URL")
            .map(str::to_owned)
            .or_else(|| {
                env.get_not_empty("GITHUB_API_DOMAIN")
                    .map(|d| format!("https://{d}"))
            })
            .unwrap_or_else(|| "https://api.github.com".into());
        let url = format!(
            "{}/repos/{owner}/{repo}/tarball{}",
            base.trim_end_matches('/'),
            if branch.is_empty() {
                String::new()
            } else {
                format!("/{branch}")
            }
        );
        let view = req
            .display
            .as_ref()
            .map(|display| display.start(&format!("create {label}")));
        if let Some(view) = &view {
            view.emit(fnpm_types::Event::Download {
                done: 0,
                total: 1,
                bytes: None,
                name: Some(format!("template {label}")),
            });
        }
        let fetched = crate::runtime().block_on(async {
            parent
                .client()?
                .tarball_bytes(&url)
                .await
                .map_err(Error::from)
        });
        if let Some(display) = &req.display {
            display.stop(None);
        }
        let data = fetched.map_err(|error| match error {
            Error::Registry(fnpm_registry::Error::Status { status, .. }) if status >= 500 => {
                Error::usage("GitHub returned a server error")
            }
            Error::Registry(fnpm_registry::Error::Status {
                status: status @ (403 | 429),
                ..
            }) => Error::usage(format!(
                "GitHub returned {status}. This usually means GitHub is rate limiting your requests"
            )),
            other => other,
        })?;
        fnpm_store::archive::extract_to_dir(
            &data,
            staging.path(),
            fnpm_store::archive::ExtractOptions {
                skip_symlinks: true,
            },
        )
        .map_err(|error| match error {
            fnpm_store::Error::InvalidTarball { message, .. } if message.contains("Truncated") => {
                Error::usage("Unexpected end of template archive")
            }
            other => other.into(),
        })?;
        staging.path().to_owned()
    };
    let source = std::fs::canonicalize(&source).map_err(Error::io(&source))?;
    if deepest_existing(&dest)?.starts_with(&source) {
        return Err(Error::usage("destination is inside template"));
    }
    if std::fs::symlink_metadata(&dest).is_ok_and(|m| m.is_symlink()) {
        return Err(Error::usage("destination is a symbolic link"));
    }
    let mut files = files(&source)?;
    let has_manifest = files.contains_key("package.json");
    let plain_gitignore = files
        .get(".gitignore")
        .is_some_and(|p| p.file_name().is_some_and(|n| n == "gitignore"));
    let rename_error = plain_gitignore && dest.join(".gitignore").is_dir();
    if rename_error {
        if let Some(source) = files.remove(".gitignore") {
            files.insert("gitignore".into(), source);
        }
    } else if plain_gitignore && dest.join(".gitignore").exists() && !force {
        files.remove(".gitignore");
    }
    let mut conflicts = Vec::new();
    for rel in files.keys() {
        let mut path = dest.clone();
        let parts: Vec<_> = Path::new(rel).components().collect();
        let mut conflict = false;
        for (i, part) in parts.iter().enumerate() {
            path.push(part);
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                let last = i + 1 == parts.len();
                if meta.is_symlink() {
                    if !force {
                        conflict = true;
                    }
                    break;
                }
                if (!last && !meta.is_dir()) || (last && meta.is_dir()) {
                    conflict = true;
                    break;
                }
                if last && !force && rel != "README.md" {
                    conflict = true;
                }
            }
        }
        if conflict {
            conflicts.push(if remote.is_some() {
                rel.split_once('/')
                    .map_or_else(|| rel.clone(), |(top, _)| format!("{top}/"))
            } else {
                rel.clone()
            });
        }
    }
    if !conflicts.is_empty() {
        if force {
            return Err(Error::usage(format!(
                "failed to copy file {}: destination has an incompatible file type",
                crate::security::escape_control(&conflicts[0], false)
            )));
        }
        conflicts.sort();
        conflicts.dedup();
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        eprintln!("error: The directory {name}/ contains files that could conflict:");
        for rel in conflicts {
            eprintln!("  {}", crate::security::escape_control(&rel, false));
        }
        eprintln!("To overwrite them, use --force");
        return Err(Error::Reported);
    }
    std::fs::create_dir_all(&dest).map_err(Error::io(&dest))?;
    for (rel, src) in files {
        let target = dest.join(&rel);
        for link in destination_links(&dest, &rel) {
            std::fs::remove_file(&link).map_err(Error::io(&link))?;
        }
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(Error::io(dir))?;
        }
        let bytes = std::fs::read(&src).map_err(Error::io(&src))?;
        crate::project::write_atomic(&target, &bytes)?;
        let permissions = std::fs::metadata(&src)
            .map_err(Error::io(&src))?
            .permissions();
        std::fs::set_permissions(&target, permissions).map_err(Error::io(&target))?;
    }
    if rename_error {
        return Err(Error::usage("renaming gitignore to .gitignore"));
    }
    let manifest_path = dest.join("package.json");
    let mut json: Value = if has_manifest {
        serde_json::from_slice(&std::fs::read(&manifest_path).map_err(Error::io(&manifest_path))?)
            .map_err(|e| Error::usage(format!("invalid template package.json: {e}")))?
    } else {
        Value::Null
    };
    let tasks = json.get("bun-create").cloned().unwrap_or(Value::Null);
    if has_manifest && !no_json {
        if let Some(obj) = json.as_object_mut() {
            if obj.contains_key("name") {
                obj.insert(
                    "name".into(),
                    Value::String(
                        dest.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into(),
                    ),
                );
            }
            obj.remove("bun-create");
        }
        let text = std::fs::read_to_string(&manifest_path).map_err(Error::io(&manifest_path))?;
        let multiline = text.contains('\n');
        let mut editor = fnpm_workspace::PackageJsonEditor::new(text);
        if let Some(name) = json.get("name") {
            editor.set_path("name", name.clone());
        }
        editor.delete_path("bun-create");
        let edited = if multiline {
            editor.finish()
        } else {
            format!(
                "{}\n",
                serde_json::to_string_pretty(&json).map_err(|e| Error::usage(e.to_string()))?
            )
        };
        crate::project::write_atomic(&manifest_path, edited.as_bytes())?;
    }
    let existing_repo = dest.ancestors().any(|dir| dir.join(".git").exists());
    let git_created = !no_git && !existing_repo && init_git(&dest);
    let skip_scripts = ignore_scripts || parent.config.ignore_scripts || no_json;
    if !no_install && has_manifest {
        if !skip_scripts {
            let code = task(&dest, &json, env, "preinstall", &tasks["preinstall"])?;
            if code != 0 {
                return Ok(code);
            }
        }
        let dependencies = ["dependencies", "devDependencies"].iter().any(|k| {
            json.get(k)
                .and_then(Value::as_object)
                .is_some_and(|o| !o.is_empty())
        });
        if dependencies {
            println!(
                "$ fnpm install{}",
                if skip_scripts {
                    " --ignore-scripts"
                } else {
                    ""
                }
            );
            let view = req
                .display
                .as_ref()
                .map(|display| display.start(&format!("create {label} · dependencies")));
            let opts = InstallOptions {
                pm: PmOptions {
                    cwd: dest.clone(),
                    ignore_scripts: skip_scripts,
                    progress: view.is_some(),
                    ..pm.clone()
                },
                command: InstallCommand::Install,
                ..Default::default()
            };
            let started = std::time::Instant::now();
            // Without a live view, bun's status lines, warnings and errors still print: an
            // error reported through events is not repeated below.
            let sink = view.unwrap_or_else(|| {
                Arc::new(super::DiagnosticSink {
                    out: Out::new(),
                    silent: false,
                })
            });
            let installed =
                crate::runtime().block_on(crate::install_with_env(opts, sink, env.clone()));
            if let Some(display) = &req.display {
                display.stop(None);
            }
            let summary = match installed {
                Ok(summary) => summary,
                Err(error) => {
                    let code = match &error {
                        Error::Scripts(fnpm_scripts::Error::Failed { code, .. }) => *code,
                        Error::Scripts(fnpm_scripts::Error::Signaled { signal, .. }) => {
                            128 + *signal
                        }
                        _ => 1,
                    };
                    if !matches!(error, Error::Reported) {
                        eprintln!("error: {error}");
                    }
                    eprintln!(
                        "error: fnpm install failed in {} (code: {code})",
                        serde_json::to_string(&crate::security::escape_control(
                            &dest.to_string_lossy(),
                            false
                        ))
                        .unwrap_or_default()
                    );
                    eprintln!(
                        "note: template files were written. run fnpm install there once the error is fixed"
                    );
                    return Ok(code);
                }
            };
            eprintln!(
                "[{:.2}ms] fnpm install",
                started.elapsed().as_secs_f64() * 1000.0
            );
            println!(
                "{} package{} installed",
                summary.installed_count,
                if summary.installed_count == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        if !skip_scripts {
            let code = task(&dest, &json, env, "postinstall", &tasks["postinstall"])?;
            if code != 0 {
                return Ok(code);
            }
        }
        if git_created {
            println!(
                "A local git repository was created for you{}.",
                if dependencies {
                    " and dependencies were installed automatically"
                } else {
                    ""
                }
            );
        }
    } else if git_created {
        println!("A local git repository was created for you.");
    }
    let dest_name = dest_arg.map(String::as_str).unwrap_or(default_dest);
    println!(
        "Success! {} loaded into {}",
        crate::security::escape_control(&label, false),
        crate::security::escape_control(dest_name, false)
    );
    if dest_name != "." {
        let guide = if Path::new(dest_name).is_absolute() {
            dest.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(dest_name)
        } else {
            dest_name
        };
        println!("\n  cd {}", crate::security::escape_control(guide, false));
    }
    if let Some(start) = tasks.get("start").and_then(Value::as_str) {
        println!("  {}", crate::security::escape_control(start, false));
    } else if dest_name == "." {
        println!("\n# To get started, run:\n\n  fnpm run dev\n");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_validated_after_backslash_normalization() {
        assert_eq!(relative_path("src\\index.js").unwrap(), "src/index.js");
        for path in [
            "../outside",
            "..\\outside",
            "a\\..\\outside",
            "\\root",
            "C:\\root",
            "a//b",
            "a/./b",
            "",
        ] {
            assert!(relative_path(path).is_err(), "{path:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn source_selection_rejects_escape_and_normalized_collisions() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("..\\outside");
        std::fs::write(&outside, "escape").unwrap();
        assert!(files(root.path()).is_err());
        std::fs::remove_file(outside).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/index.js"), "one").unwrap();
        std::fs::write(root.path().join("src\\index.js"), "two").unwrap();
        assert!(files(root.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn protected_paths_are_filtered_after_normalization() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".git\\config"), "private").unwrap();
        std::fs::write(root.path().join("node_modules\\pkg.js"), "private").unwrap();
        std::fs::write(root.path().join("index.js"), "template").unwrap();
        let selected = files(root.path()).unwrap();
        assert_eq!(
            selected.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["index.js"]
        );
    }
}
