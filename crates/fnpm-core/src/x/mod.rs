//! `fnpm x` / `fnpm dlx` / `fnpmx`, and `fnpm create` (bun `runtime/cli/bunx_command.rs` and
//! the `bun create` dispatch in `runtime/cli/mod.rs`).
//!
//! The bin is looked for, in order: on `PATH` (which starts with every `node_modules/.bin`
//! from the working directory up), in the project's installed copy of the package, in a tree
//! that an earlier `fnpm x` installed into the cache, and finally in a tree installed now.
//! A tree is `<cache>/.fnpmx-<uid>/<name>@<version>[+options]` ([`fnpm_store::dlx`]); it is
//! refreshed after a day, and an explicit dist-tag asks the registry on every run. The install
//! runs in process under an exclusive lock beside the tree, so concurrent runs install once.
//!
//! [`plan`] does everything up to running the bin and returns what to run: the native
//! launcher `exec`s it, the napi path ([`run`]) spawns it and waits.

mod args;
mod bins;

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fnpm_config::{CliOverrides, Config, Env, LoadOptions};
use fnpm_scripts::env::EnvMap;
use fnpm_scripts::shell::which;
use fnpm_types::spec::DependencySpec;
use fnpm_types::{Event, EventSink};

pub use args::{Parsed, XArgs, parse, skips_flag};
pub use bins::{BinError, InProject, PackageBin, Tree, bin_of, find_in_cache, find_in_project};

use crate::options::{DlxMode, InstallCommand, InstallOptions, PmOptions};
use crate::spec::Positional;

/// What to run.
#[derive(Debug, Clone)]
pub struct Exec {
    /// The bin.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// The complete environment.
    pub env: Vec<(OsString, OsString)>,
    /// The working directory.
    pub cwd: PathBuf,
}

/// How [`plan`] ended.
#[derive(Debug)]
pub enum Outcome {
    /// Run this.
    Run(Exec),
    /// Exit with this status; everything to say was written to stderr / stdout.
    Exit(i32),
}

/// One `fnpm x` / `fnpm create`.
#[derive(Debug, Clone)]
pub struct Request {
    /// The arguments after `x` / `create` (flags typed before the command included).
    pub args: Vec<String>,
    /// The working directory.
    pub cwd: PathBuf,
    /// `fnpm create`.
    pub create: bool,
    /// The CLI version (`--version`).
    pub version: String,
}

/// stderr diagnostics in bun's format (`error: `, `warn: `, `note: `).
struct Out {
    color: bool,
}

impl Out {
    fn new() -> Self {
        Self {
            color: std::io::stderr().is_terminal()
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        }
    }

    fn label(&self, label: &str, ansi: &str) -> String {
        if self.color {
            format!("\x1b[{ansi}m{label}\x1b[0m:")
        } else {
            format!("{label}:")
        }
    }

    fn error(&self, message: &str) {
        eprintln!("{} {message}", self.label("error", "31"));
    }

    fn warn(&self, message: &str) {
        eprintln!("{} {message}", self.label("warn", "33"));
    }

    fn note(&self, message: &str) {
        eprintln!("{} {message}", self.label("note", "34"));
    }

    fn dim(&self, line: &str) {
        if self.color {
            eprintln!("\x1b[2m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }
}

/// `fnpmx --help`.
pub const USAGE: &str = "\
Usage: fnpmx [flags] <package><@version> [flags and arguments for the package]
Execute an npm package executable (CLI), automatically installing into a global shared cache if not installed in node_modules.

Flags:
  -p, --package <package>    Specify package to install when binary name differs from package name
  --no-install           Skip installation if package is not already installed
  --cwd <path>            Set the working directory
  --env-file <file>       Load environment variables from the specified file(s)
  --minimum-release-age <seconds>  Only install packages published at least N seconds ago (security feature)
  --registry <url>        Install from a specific registry
  --ca <cert>, --cafile <path>  Trust a Certificate Authority signing certificate during installation
  --use-system-ca, --use-openssl-ca, --use-bundled-ca  Choose the CA store to trust during installation
  --trust                Run the lifecycle scripts of the package and of ALL of its dependencies during installation
  --ignore-scripts       Run no lifecycle scripts during installation
  --verbose              Enable verbose output during installation
  --silent               Suppress output during installation

Examples:
  fnpmx prisma migrate
  fnpmx prettier foo.js
  fnpmx -p @angular/cli ng new my-app
  fnpm x create-vite my-app
";

/// `fnpm create --help`.
pub const CREATE_USAGE: &str = "\
Usage:
  fnpm create <template> [...flags] dest

  <template> runs the create-<template> package with fnpm x:
    fnpm create vite         ->  fnpm x create-vite
    fnpm create @org/app     ->  fnpm x @org/create-app
    fnpm create @org         ->  fnpm x @org/create

Examples:
  fnpm create vite my-app
  fnpm create next-app
";

/// The legacy `bun create` templates that bun keeps instead of running `create-<name>`.
const NON_X_TEMPLATES: [&str; 3] = ["elysia", "elysia-buchta", "stric"];

/// `foo` -> `create-foo`, `@org/x@v` -> `@org/create-x@v`, `@org@v` -> `@org/create@v`
/// (bun `add_create_prefix`).
pub fn add_create_prefix(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    if !input.starts_with('@') {
        return format!("create-{input}");
    }
    if let Some(slash) = input.find('/') {
        return format!("{}create-{}", &input[..=slash], &input[slash + 1..]);
    }
    match input[1..].find('@') {
        Some(at) => format!("{}/create{}", &input[..at + 1], &input[at + 1..]),
        None => format!("{input}/create"),
    }
}

/// `fnpm create <template> [args]`: the arguments of the `fnpm x` it becomes, or the exit code.
fn create_args(args: &[String], out: &Out) -> Result<Vec<String>, i32> {
    if args.is_empty() {
        eprint!("{CREATE_USAGE}");
        return Err(1);
    }
    let mut template: Option<(String, usize)> = None;
    let mut dash_dash_bun = false;
    let mut help = false;
    for (i, arg) in args.iter().enumerate() {
        let slice = arg.trim_matches([' ', '\t', '\n']);
        if slice.is_empty() {
            continue;
        }
        if !slice.starts_with("--") && template.is_none() {
            template = Some((slice.to_owned(), i + 1));
        }
        if slice == "--bun" {
            dash_dash_bun = true;
        } else if slice == "--help" || slice == "-h" {
            help = true;
        }
    }
    let Some((name, rest)) = template.filter(|_| !help) else {
        eprint!("{CREATE_USAGE}");
        return Err(0);
    };
    if name == "react" {
        eprintln!(
            "The \"react\" template has been deprecated.\nIt is recommended to use \"react-app\" or \"vite\" instead.\n\nTo create a project using Create React App, run\n\n  fnpm create react-app\n\nTo create a React project using Vite, run\n\n  fnpm create vite\n\nThen select \"React\" from the list of frameworks.\n"
        );
        return Err(1);
    }
    if name == "next" {
        out.warn("No template create-next found.\nTo create a project with the official Next.js scaffolding tool, run\n  fnpm create next-app [destination]");
        return Err(1);
    }
    let uses_x =
        !NON_X_TEMPLATES.contains(&name.as_str()) && (!name.contains('/') || name.starts_with('@'));
    if !uses_x {
        out.error(&format!(
            "fnpm create does not support GitHub or local folder templates yet: {name}"
        ));
        out.note("run a create-<template> package instead, e.g. fnpm create vite");
        return Err(1);
    }
    let mut x = Vec::with_capacity(args.len() + 1);
    if dash_dash_bun {
        x.push("--bun".to_owned());
    }
    x.push(add_create_prefix(&name));
    x.extend(args[rest..].iter().cloned());
    Ok(x)
}

/// bun's `VersionTag` names, used in tree names.
fn version_tag(spec: &DependencySpec) -> &'static str {
    match spec {
        DependencySpec::Npm { .. } => "npm",
        DependencySpec::DistTag { .. } => "dist_tag",
        DependencySpec::Tarball { .. } => "tarball",
        DependencySpec::Folder { .. } => "folder",
        DependencySpec::Symlink { .. } => "symlink",
        DependencySpec::Workspace { .. } => "workspace",
        DependencySpec::Git(_) => "git",
        DependencySpec::Github(_) => "github",
        DependencySpec::Catalog { .. } => "catalog",
    }
}

/// The system temp dir (`$TMPDIR` and friends, made absolute against `cwd`).
fn temp_dir(env: &Env, cwd: &Path) -> PathBuf {
    let dir = env
        .first(&["FNPM_TMPDIR", "BUN_TMPDIR", "TMPDIR", "TMP", "TEMP"])
        .filter(|d| !d.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = if dir.is_absolute() {
        dir
    } else {
        cwd.join(dir)
    };
    normalize(&dir)
}

/// `a/./b/../c` -> `a/c`, without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The configuration of the user (not of the project): what the install into a tree reads.
fn user_config(env: &Env) -> Option<Config> {
    let root = PathBuf::from("/");
    Config::load(&LoadOptions {
        cwd: root.clone(),
        cli: CliOverrides::default(),
        env: env.clone(),
        project_dir: Some(root),
        node_version: None,
    })
    .ok()
}

/// The install cache of `config`, or `None` when it is only the working-directory fallback.
fn install_cache_dir(config: Option<&Config>) -> Option<PathBuf> {
    let config = config?;
    let fallback = Path::new("/").join("node_modules").join(".fnpm-cache");
    (config.cache_dir != fallback).then(|| config.cache_dir.clone())
}

/// The environment of the bin (bun `configure_env_for_run` + `configure_path_for_run`):
/// the process environment, `.env` files and `--env-file`s, `node_modules/.bin` of the
/// working directory and every parent on `PATH`, and npm's script variables.
fn run_env(opts: &XArgs, env: &Env, project: &Path) -> EnvMap {
    let cli = CliOverrides {
        env_file: opts.env_files.iter().map(PathBuf::from).collect(),
        ..CliOverrides::default()
    };
    let load = LoadOptions {
        cwd: opts.cwd.clone(),
        cli,
        env: env.clone(),
        project_dir: Some(project.to_path_buf()),
        node_version: None,
    };
    let extra: Vec<(String, String)> = load
        .effective_env()
        .map(|effective| {
            effective
                .iter()
                .filter(|(k, v)| env.get(k) != Some(*v))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let base = fnpm_scripts::env::BaseEnv::run(&opts.cwd, std::env::vars_os(), &extra, "exec");
    let mut map = base.env().clone();
    let path = map.get("PATH").map(OsStr::to_owned).unwrap_or_default();
    map.set("PATH", fnpm_scripts::env::path_for_run(&opts.cwd, &path));
    map.set("npm_lifecycle_event", "fnpmx");
    map.set("npm_lifecycle_script", opts.package_name.as_str());
    map
}

/// `PATH` without the directory `ignore` (bun `BUN_WHICH_IGNORE_CWD`: a temporary node-gyp
/// script must not find itself).
fn path_without(path: &OsStr, ignore: &str) -> OsString {
    let ignore = ignore.trim_end_matches('/');
    let kept: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|p| p.to_string_lossy().trim_end_matches('/') != ignore)
        .collect();
    std::env::join_paths(kept).unwrap_or_default()
}

fn prepend_path(map: &mut EnvMap, tree: &Path) {
    let bin = tree.join("node_modules").join(".bin");
    let path = map.get("PATH").map(OsStr::to_owned).unwrap_or_default();
    let mut new = bin.into_os_string();
    if !path.is_empty() {
        new.push(if cfg!(windows) { ";" } else { ":" });
        new.push(path);
    }
    map.set("PATH", new);
}

/// Prints warnings and errors of the install (not progress) to stderr.
struct DiagnosticSink {
    out: Out,
    silent: bool,
}

impl EventSink for DiagnosticSink {
    fn emit(&self, event: Event) {
        if self.silent {
            return;
        }
        match event {
            Event::Warn { message, .. } => self.out.warn(&message),
            Event::Error { message, .. } => self.out.error(&message),
            _ => {}
        }
    }

    fn enabled(&self) -> bool {
        !self.silent
    }
}

/// The registry's answer said nothing about the package (no connection, a 5xx / 429 / 408, or
/// a tarball that could not be downloaded), so a copy that is already installed can run.
fn unanswered(error: &crate::Error) -> bool {
    use fnpm_registry::Error as R;
    let registry = |e: &R| match e {
        R::Network { .. } => true,
        R::Status { status, .. } | R::Registry { status, .. } => {
            matches!(status, 500..=599 | 429 | 408)
        }
        _ => false,
    };
    match error {
        crate::Error::Fetch { .. } => true,
        crate::Error::Registry(e) | crate::Error::Resolve(fnpm_resolver::Error::Registry(e)) => {
            registry(e)
        }
        _ => false,
    }
}

/// bun's wording for an install failure (`GET <url> - <status>`).
fn install_error(error: &crate::Error, registry: &str) -> String {
    use fnpm_registry::Error as R;
    match error {
        crate::Error::Resolve(fnpm_resolver::Error::NotFound(name))
        | crate::Error::Registry(R::NotFound(name))
        | crate::Error::Resolve(fnpm_resolver::Error::Registry(R::NotFound(name))) => {
            let base = if registry.ends_with('/') {
                registry.to_owned()
            } else {
                format!("{registry}/")
            };
            format!("GET {base}{} - 404", name.replace('/', "%2f"))
        }
        crate::Error::Registry(R::Status { url, status })
        | crate::Error::Resolve(fnpm_resolver::Error::Registry(R::Status { url, status })) => {
            format!("GET {url} - {status}")
        }
        crate::Error::Registry(R::Network { url, message })
        | crate::Error::Resolve(fnpm_resolver::Error::Registry(R::Network { url, message }))
            if url.is_empty() =>
        {
            message.clone()
        }
        other => other.to_string(),
    }
}

/// An exclusive `flock` on `<tree>.lock`, released when dropped (and closed on exec).
struct InstallLock {
    _file: std::fs::File,
}

impl InstallLock {
    /// Blocks while another `fnpm x` holds it; `None` if it cannot be taken.
    fn acquire(tree: &Path) -> Option<Self> {
        let mut path = tree.as_os_str().to_owned();
        path.push(".lock");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(PathBuf::from(path)).ok()?;
        file.lock().ok()?;
        Some(Self { _file: file })
    }
}

/// Everything about the requested package that the lookups need.
struct Target {
    /// The package name (`""` for a URL or a path).
    name: String,
    /// What `node_modules/<name>/package.json` is looked up under.
    package: String,
    initial_bin_name: String,
    initial_bin_name_is_a_guess: bool,
    is_versioned: bool,
    is_dist_tag: bool,
    display_version: String,
    version_tag: &'static str,
    install_param: String,
}

fn target(opts: &XArgs) -> Result<Target, String> {
    let positional = Positional::parse(&opts.package_name).map_err(|e| e.to_string())?;
    let mut name = positional.name.clone().unwrap_or_default();
    // `fnpm x tsc` installs TypeScript, then runs tsc.
    if opts.specified_package.is_none() {
        if name == "tsc" {
            name = "typescript".into();
        } else if name == "claude" {
            // The npm package "claude" is an unrelated squatter with no bin.
            name = "@anthropic-ai/claude-code".into();
        }
    }
    let mut guess = false;
    let initial_bin_name = if let Some(bin) = &opts.binary_name {
        bin.clone()
    } else if name == "typescript" {
        "tsc".into()
    } else if name == "@anthropic-ai/claude-code" {
        "claude".into()
    } else if let DependencySpec::Github(g) = &positional.spec {
        g.repo.clone()
    } else if let Some(at) = name.rfind('/') {
        // The "bar" of `@foo/bar` is only a guess: the package's `bin` decides.
        guess = true;
        name[at + 1..].to_owned()
    } else if name.is_empty() {
        positional.request_name()
    } else {
        name.clone()
    };
    let display_version = if positional.literal.is_empty() {
        "latest".to_owned()
    } else {
        positional.literal.clone()
    };
    let package = if name.is_empty() {
        initial_bin_name.clone()
    } else {
        name.clone()
    };
    // `@latest` is left out when the user did not type it: without an explicit tag, `fnpm add`
    // does not pick a deprecated `latest`.
    let install_param = if positional.literal.is_empty() {
        package.clone()
    } else {
        format!("{package}@{display_version}")
    };
    Ok(Target {
        is_versioned: !positional.literal.is_empty(),
        is_dist_tag: matches!(positional.spec, DependencySpec::DistTag { .. })
            && !positional.literal.is_empty(),
        version_tag: version_tag(&positional.spec),
        name,
        package,
        initial_bin_name,
        initial_bin_name_is_a_guess: guess,
        display_version,
        install_param,
    })
}

fn exec(program: PathBuf, opts: &XArgs, env: &EnvMap) -> Outcome {
    Outcome::Run(Exec {
        program,
        args: opts.passthrough.clone(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        cwd: opts.cwd.clone(),
    })
}

/// Resolves what `req` runs, installing the package into the cache when needed.
pub fn plan(req: &Request) -> Outcome {
    let out = Out::new();
    let args = if req.create {
        match create_args(&req.args, &out) {
            Ok(args) => args,
            Err(code) => return Outcome::Exit(code),
        }
    } else {
        req.args.clone()
    };
    let opts = match args::parse(&args, &req.cwd) {
        Parsed::Run(opts) => opts,
        Parsed::Help => {
            eprint!("{USAGE}");
            return Outcome::Exit(0);
        }
        Parsed::Usage => {
            eprint!("{USAGE}");
            return Outcome::Exit(1);
        }
        Parsed::Version | Parsed::Revision => {
            println!("{}", req.version);
            return Outcome::Exit(0);
        }
        Parsed::Error { message, hint } => {
            out.error(&message);
            if let Some(hint) = hint {
                out.dim(&hint);
            }
            return Outcome::Exit(1);
        }
    };
    match plan_with(&opts, &out) {
        Ok(outcome) => outcome,
        Err(code) => Outcome::Exit(code),
    }
}

fn plan_with(opts: &XArgs, out: &Out) -> Result<Outcome, i32> {
    let target = match target(opts) {
        Ok(t) => t,
        Err(message) => {
            out.error(&message);
            return Err(1);
        }
    };
    let env = Env::from_process();
    let top = opts.cwd.clone();
    let project = fnpm_workspace::Workspace::find_root(&top).unwrap_or_else(|_| top.clone());
    let mut run_env = run_env(opts, &env, &project);
    let ignore_cwd = ["FNPM_WHICH_IGNORE_CWD", "BUN_WHICH_IGNORE_CWD"]
        .iter()
        .find_map(|k| run_env.get(k).map(|v| v.to_string_lossy().into_owned()))
        .filter(|v| !v.is_empty());
    run_env.remove("FNPM_WHICH_IGNORE_CWD");
    run_env.remove("BUN_WHICH_IGNORE_CWD");
    let path = run_env.get("PATH").map(OsStr::to_owned).unwrap_or_default();
    let (bin_path, which_cwd) = match &ignore_cwd {
        Some(dir) => (path_without(&path, dir), PathBuf::new()),
        None => (path.clone(), top.clone()),
    };
    let package_with_bin = opts
        .binary_name
        .is_none()
        .then_some(target.package.as_str());
    let look_for_existing_bin = opts.no_install || !target.is_versioned || !target.is_dist_tag;
    let no_bin = |found: Result<Option<PathBuf>, BinError>| -> Result<Option<PathBuf>, i32> {
        match found {
            Err(BinError::NoBin) => {
                out.error(&format!(
                    "could not determine executable to run for package {}",
                    target.package
                ));
                Err(1)
            }
            Err(BinError::Unreadable) => Ok(None),
            Ok(found) => Ok(found),
        }
    };

    // 1. Like npx: when one word names both the package and its bin, look on PATH (which
    //    starts with the project's node_modules/.bin).
    if !target.is_versioned && !target.initial_bin_name_is_a_guess && opts.binary_name.is_none() {
        if let Some(bin) = which(&target.initial_bin_name, &bin_path, &which_cwd) {
            return Ok(exec(bin, opts, &run_env));
        }
        #[cfg(unix)]
        for dir in std::env::split_paths(&bin_path) {
            if !dir.ends_with("node_modules/.bin") {
                continue;
            }
            let bin = dir.join(&target.initial_bin_name);
            if bin.exists() {
                out.error(&format!("{} is not executable", bin.display()));
                return Err(126);
            }
        }
    }

    // 2. The project's installed copy, unless a version, tag or URL says where it comes from.
    if !target.is_versioned {
        let found = match find_in_project(
            &top,
            &project,
            &target.package,
            &target.initial_bin_name,
            opts.binary_name.is_some(),
        ) {
            // Like npx, `--package` runs any command on PATH once the package is installed.
            Err(BinError::NoBin) if opts.binary_name.is_some() => {
                which(&target.initial_bin_name, &bin_path, &which_cwd)
                    .map_or(InProject::Absent, InProject::Run)
            }
            Err(BinError::NoBin) => {
                no_bin(Err(BinError::NoBin))?;
                InProject::Absent
            }
            Err(BinError::Unreadable) => InProject::Absent,
            Ok(found) => found,
        };
        match found {
            InProject::Absent => {}
            InProject::Run(bin) => return Ok(exec(bin, opts, &run_env)),
            InProject::NotExecutable(file) => {
                out.error(&format!("{} is not executable", file.display()));
                return Err(126);
            }
            InProject::Unresolved {
                file,
                error,
                in_node_modules,
            } => {
                eprintln!(
                    "{}: could not find the bin of {}: \"{}\" (realpath)",
                    bins::errno_label(&error),
                    target.package,
                    file.display()
                );
                if in_node_modules {
                    out.note(&format!(
                        "run 'fnpm install --force', or 'fnpm pm trust {0}' and 'fnpm install' if an install script writes the bin",
                        target.package
                    ));
                } else {
                    out.note(&format!(
                        "{} is a package of this project, and its bin is not built",
                        target.package
                    ));
                }
                return Err(1);
            }
        }
    }

    let tree_name = fnpm_store::dlx::tree_name(&fnpm_store::dlx::TreeOptions {
        name: &target.name,
        version: &target.display_version,
        version_tag: target.version_tag,
        bin_name: &target.initial_bin_name,
        trust: opts.trust,
        ignore_scripts: opts.ignore_scripts,
        minimum_release_age: opts.minimum_release_age.as_deref(),
        registry: opts.registry.as_deref(),
    });
    let user = user_config(&env);
    let install_cache = if cfg!(unix) {
        install_cache_dir(user.as_ref())
    } else {
        None
    };
    let temp = temp_dir(&env, &top);

    // 3. A tree that an earlier `fnpm x` installed. It goes stale after a day.
    if look_for_existing_bin
        && let Some(dir) =
            fnpm_store::dlx::private_cache_dir(install_cache.as_deref(), &temp, &tree_name, false)
    {
        let tree = bins::tree_state(&dir.path);
        let fresh = tree == Tree::Fresh;
        if (fresh || (opts.no_install && tree != Tree::Missing))
            && let Some(bin) = no_bin(find_in_cache(
                &dir.path,
                &which_cwd,
                &target.initial_bin_name,
                package_with_bin,
                &bin_path,
            ))?
        {
            if !fresh {
                out.warn(&format!(
                    "Using a stale installation of {} because --no-install was passed. Run `fnpmx` without --no-install to use a fresh binary.",
                    target.package
                ));
            }
            prepend_path(&mut run_env, &dir.path);
            return Ok(exec(bin, opts, &run_env));
        }
    }

    if opts.no_install {
        out.error(&format!(
            "Could not find an existing '{}' binary to run. Stopping because --no-install was passed.",
            target.initial_bin_name
        ));
        return Err(1);
    }

    // 4. Install into the tree.
    let Some(dir) =
        fnpm_store::dlx::private_cache_dir(install_cache.as_deref(), &temp, &tree_name, true)
    else {
        out.error(
            "failed to create private cache directory. Set FNPM_CACHE_DIR to a directory you own.",
        );
        return Err(1);
    };
    if dir.shared {
        out.warn(fnpm_store::dlx::DlxDir::SHARED_WARNING);
    }
    let tree_dir = dir.path;
    prepend_path(&mut run_env, &tree_dir);
    if let Err(e) = std::fs::create_dir_all(&tree_dir) {
        out.error(&format!("{}: {e}", tree_dir.display()));
        return Err(1);
    }
    // Held during the install and the lookups, until the bin runs. Without it, install
    // anyway.
    let mut lock = InstallLock::acquire(&tree_dir);

    // Checked under the lock: the run that held it may have just installed the tree.
    let tree = bins::tree_state(&tree_dir);
    let installed = if tree == Tree::Fresh {
        match find_in_cache(
            &tree_dir,
            &which_cwd,
            &target.initial_bin_name,
            package_with_bin,
            &bin_path,
        ) {
            // The version the dist-tag points to now may have a bin.
            Err(BinError::NoBin) if !look_for_existing_bin => None,
            found => no_bin(found)?,
        }
    } else {
        None
    };
    if let Some(bin) = installed.clone().filter(|_| look_for_existing_bin) {
        drop(lock.take());
        return Ok(exec(bin, opts, &run_env));
    }

    let package_json = tree_dir.join("package.json");
    let finished = std::fs::read(&package_json)
        .ok()
        .filter(|c| c.len() > DlxMode::UNFINISHED.len());
    if finished.is_none() {
        let _ = std::fs::write(&package_json, DlxMode::UNFINISHED);
    }
    if tree == Tree::Stale {
        // The lockfile would pin what the refresh is meant to update.
        for lockfile in ["fnpm.lock", "bun.lock", "bun.lockb"] {
            let _ = std::fs::remove_file(tree_dir.join(lockfile));
        }
    }
    // A refresh of a fresh tree only updates the requested package, so package.json gets its
    // old times back afterward: the dependencies' refresh is not postponed.
    let last_refresh = (tree == Tree::Fresh)
        .then(|| std::fs::metadata(&package_json).ok())
        .flatten();

    let has_copy = finished.is_some() && look_for_existing_bin;
    let mut install_env = env.clone();
    if has_copy {
        // With a copy to run, the registry is asked once whether it is the latest.
        install_env.set("FNPM_CONFIG_HTTP_RETRY_COUNT", "0");
    }
    let minimum_release_age_ms = opts
        .minimum_release_age
        .as_deref()
        .and_then(fnpm_config::files::parse_seconds)
        .map(|s| (s * 1000.0).round() as u64);
    let log_level = if opts.silent {
        fnpm_config::LogLevel::Silent
    } else if opts.verbose {
        fnpm_config::LogLevel::Verbose
    } else {
        fnpm_config::LogLevel::Default
    };
    let install = InstallOptions {
        pm: PmOptions {
            cwd: tree_dir.clone(),
            summary: false,
            progress: false,
            // Like `--force` / `--no-cache` on the `bun add` that bunx spawns: the requested
            // package's manifest comes from the registry; the lockfile still pins the rest.
            no_cache: target.is_dist_tag || tree == Tree::Stale || installed.is_none(),
            log_level,
            trust: opts.trust && !opts.ignore_scripts,
            ignore_scripts: opts.ignore_scripts,
            registry: opts.registry.clone(),
            ca: opts.ca.clone(),
            cafile: opts.cafile.as_ref().map(|f| top.join(f)),
            ca_store: opts.ca_store.map(str::to_owned),
            minimum_release_age_ms,
            ..PmOptions::default()
        },
        command: InstallCommand::Add,
        packages: vec![target.install_param.clone()],
        dlx: Some(DlxMode {
            relink: installed.is_none(),
        }),
        ..InstallOptions::default()
    };
    let sink: Arc<dyn EventSink> = Arc::new(DiagnosticSink {
        out: Out::new(),
        silent: opts.silent,
    });
    let result = crate::runtime().block_on(crate::install_with_env(install, sink, install_env));
    match result {
        Ok(_) => {
            if let Some(meta) = last_refresh
                && let Ok(file) = std::fs::File::options().write(true).open(&package_json)
            {
                let mut times = std::fs::FileTimes::new();
                if let Ok(t) = meta.accessed() {
                    times = times.set_accessed(t);
                }
                if let Ok(t) = meta.modified() {
                    times = times.set_modified(t);
                }
                let _ = file.set_times(times);
            }
        }
        Err(error) => {
            if unanswered(&error)
                && has_copy
                && let Some(contents) = &finished
            {
                // Rewriting package.json marks the tree as fresh: a run without a network
                // connection does not first wait for another failed attempt.
                let _ = std::fs::write(&package_json, contents);
                out.warn(&format!(
                    "registry unreachable, using cached {}",
                    target.package
                ));
            } else {
                if !opts.silent {
                    let registry = opts.registry.clone().unwrap_or_else(|| {
                        user.as_ref().map_or_else(
                            || "https://registry.npmjs.org/".to_owned(),
                            |c| c.registry_for(&target.package).url.clone(),
                        )
                    });
                    out.error(&install_error(&error, &registry));
                }
                return Err(1);
            }
        }
    }

    if let Some(bin) = no_bin(find_in_cache(
        &tree_dir,
        &which_cwd,
        &target.initial_bin_name,
        package_with_bin,
        &bin_path,
    ))? {
        drop(lock.take());
        return Ok(exec(bin, opts, &run_env));
    }
    if let (Some(_), Some(bin)) = (&opts.specified_package, &opts.binary_name) {
        out.error(&format!(
            "Package {} does not provide a binary named {bin}",
            target.package
        ));
        out.dim(&format!(
            "  hint: try running without --package to install and run {bin} directly"
        ));
    } else {
        out.error(&format!(
            "could not determine executable to run for package {}",
            target.package
        ));
    }
    let _ = std::io::stderr().flush();
    Err(1)
}

/// Runs `exec` as a child with inherited stdio and returns its exit status (128 + signal when
/// it was killed). For callers that cannot replace the process (napi).
pub fn spawn(exec: &Exec) -> i32 {
    let mut cmd = std::process::Command::new(&exec.program);
    cmd.args(&exec.args)
        .current_dir(&exec.cwd)
        .env_clear()
        .envs(exec.env.iter().map(|(k, v)| (k, v)));
    match cmd.status() {
        Ok(status) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(signal) = status.signal() {
                    return 128 + signal;
                }
            }
            status.code().unwrap_or(1)
        }
        Err(e) => {
            Out::new().error(&format!("{}: {e}", exec.program.display()));
            1
        }
    }
}

/// [`plan`], then [`spawn`].
pub fn run(req: &Request) -> i32 {
    match plan(req) {
        Outcome::Run(exec) => spawn(&exec),
        Outcome::Exit(code) => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_prefix() {
        assert_eq!(add_create_prefix("vite"), "create-vite");
        assert_eq!(add_create_prefix("@org/app@1"), "@org/create-app@1");
        assert_eq!(add_create_prefix("@org"), "@org/create");
        assert_eq!(add_create_prefix("@org@1.0.0"), "@org/create@1.0.0");
        assert_eq!(add_create_prefix(""), "");
    }

    #[test]
    fn create_arguments() {
        let out = Out { color: false };
        let s = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(create_args(&s(&[]), &out), Err(1));
        assert_eq!(create_args(&s(&[""]), &out), Err(0));
        assert_eq!(create_args(&s(&["--"]), &out), Err(0));
        assert_eq!(create_args(&s(&["--help"]), &out), Err(0));
        assert_eq!(
            create_args(&s(&["vite", "my-app", "--template", "react"]), &out),
            Ok(s(&["create-vite", "my-app", "--template", "react"]))
        );
        assert_eq!(
            create_args(&s(&["--bun", "@org/x"]), &out),
            Ok(s(&["--bun", "@org/create-x"]))
        );
        assert_eq!(create_args(&s(&["react"]), &out), Err(1));
    }

    #[test]
    fn targets() {
        let x = |pkg: &str| {
            target(&XArgs {
                package_name: pkg.into(),
                ..XArgs::default()
            })
            .unwrap()
        };
        let t = x("tsc");
        assert_eq!(
            (t.package.as_str(), t.initial_bin_name.as_str()),
            ("typescript", "tsc")
        );
        let t = x("@scope/tool@1.2.3");
        assert!(t.initial_bin_name_is_a_guess);
        assert_eq!(t.install_param, "@scope/tool@1.2.3");
        assert!(t.is_versioned && !t.is_dist_tag);
        let t = x("tool@latest");
        assert!(t.is_dist_tag);
        let t = x("tool");
        assert_eq!(
            (t.install_param.as_str(), t.display_version.as_str()),
            ("tool", "latest")
        );
        let t = x("github:owner/cowsay");
        assert_eq!(t.initial_bin_name, "cowsay");
        assert_eq!(t.version_tag, "github");
    }

    #[test]
    fn classifies_failures() {
        use fnpm_registry::Error as R;
        let status = |s| {
            crate::Error::Registry(R::Status {
                url: "u".into(),
                status: s,
            })
        };
        assert!(unanswered(&status(503)));
        assert!(unanswered(&status(429)));
        assert!(!unanswered(&status(404)));
        assert!(!unanswered(&status(403)));
        assert_eq!(
            install_error(
                &crate::Error::Resolve(fnpm_resolver::Error::NotFound("@a/b".into())),
                "http://r/"
            ),
            "GET http://r/@a%2fb - 404"
        );
    }
}
