//! `fnpm x` arguments (bun `bunx_command.rs` `Options::parse` and
//! `CommandLineArguments::bunx_skips_flag`).
//!
//! Flags must come before the package; everything after it belongs to the package. Flags of
//! npx, pnpm dlx and npm that fnpm has no use for are skipped (with their values), because
//! `npx` in a script runs `fnpm x`. Any other flag is an error: skipping it would make the
//! value of `--flag value` the package that is downloaded and run.

use std::path::{Path, PathBuf};

/// What the arguments ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Run a package's bin.
    Run(Box<XArgs>),
    /// `--help` / `-h`: print the usage, exit 0.
    Help,
    /// No package: print the usage, exit 1.
    Usage,
    /// `--version` / `-v` without a package.
    Version,
    /// `--revision` without a package.
    Revision,
    /// An error; the message is printed after `error: `, then `hint` (if any), exit 1.
    Error {
        /// The message.
        message: String,
        /// A dim line after the message.
        hint: Option<String>,
    },
}

/// The parsed flags of one `fnpm x`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct XArgs {
    /// Arguments for the bin.
    pub passthrough: Vec<String>,
    /// The package (`<package>[@version]`), or with `--package` the package to install.
    pub package_name: String,
    /// With `--package`: the bin to run.
    pub binary_name: Option<String>,
    /// `--package`.
    pub specified_package: Option<String>,
    /// `--verbose`.
    pub verbose: bool,
    /// `--silent`.
    pub silent: bool,
    /// `--no-install` and npm's spellings of it.
    pub no_install: bool,
    /// `--trust`: run the lifecycle scripts of the package and its dependencies.
    pub trust: bool,
    /// `--ignore-scripts` (wins over `--trust`).
    pub ignore_scripts: bool,
    /// `--minimum-release-age`, as typed (validated).
    pub minimum_release_age: Option<String>,
    /// `--registry` / `--reg` / `--config.registry`.
    pub registry: Option<String>,
    /// `--ca`.
    pub ca: Vec<String>,
    /// `--cafile`.
    pub cafile: Option<String>,
    /// `--env-file`, in order.
    pub env_files: Vec<String>,
    /// The directory after every `--cwd`.
    pub cwd: PathBuf,
    /// `--use-system-ca` / `--use-openssl-ca` / `--use-bundled-ca`.
    pub ca_store: Option<&'static str>,
    /// `--bun` / `-b`.
    pub run_in_bun: bool,
}

/// The CA store flags (bun `Global::CA_STORE_FLAGS`).
const CA_STORE_FLAGS: [(&str, &str); 3] = [
    ("--use-system-ca", "system"),
    ("--use-openssl-ca", "openssl"),
    ("--use-bundled-ca", "bundled"),
];

/// Flags of npm, yarn and pnpm without an fnpm equivalent (bun `IGNORED_FOREIGN_FLAGS`).
const IGNORED_FOREIGN_FLAGS: &[&str] = &[
    "aggregate-output",
    "all",
    "allow-same-version",
    "audit",
    "auto-dedupe",
    "auto-install-peers",
    "bin-links",
    "cached",
    "caret",
    "changeset",
    "check-cache",
    "check-files",
    "check-resolutions",
    "ci",
    "color",
    "dangerously-allow-all-builds",
    "dangerously-allow-all-scripts",
    "dedupe-peer-dependents",
    "default-rc",
    "disable-pnp",
    "emoji",
    "enable-pnp",
    "engine-strict",
    "fail-if-no-match",
    "fix-lockfile",
    "fixed",
    "flat",
    "focus",
    "foreground-scripts",
    "format-package-lock",
    "frozen-store",
    "fund",
    "global-style",
    "har",
    "hoist",
    "ignore-manifest-check",
    "ignore-platform",
    "ignore-pnpmfile",
    "ignore-workspace",
    "ignore-workspace-root-check",
    "immutable-cache",
    "include-github-actions",
    "include-workspace-root",
    "inline-builds",
    "install-links",
    "iwr",
    "json",
    "legacy-bundling",
    "link-duplicates",
    "link-workspace-packages",
    "lockfile",
    "long",
    "merge-git-branch-lockfiles",
    "no-frozen-lockfile",
    "no-immutable",
    "no-prefer-frozen-lockfile",
    "no-production",
    "no-proxy",
    "node-version-check",
    "omit-lockfile-registry-resolved",
    "package-lock",
    "parseable",
    "patches",
    "pnp",
    "prefer-dedupe",
    "prefer-online",
    "prefer-workspace-packages",
    "progress",
    "pure-lockfile",
    "rebuild-bundle",
    "recursive",
    "refresh-lockfile",
    "resolution-only",
    "resolve-peers-from-workspace-root",
    "reverse",
    "runtime",
    "save-build",
    "save-bundle",
    "save-types",
    "scripts-prepend-node-path",
    "shared-workspace-lockfile",
    "shrinkwrap",
    "side-effects-cache",
    "skip-integrity-check",
    "sort",
    "stream",
    "strict-allow-scripts",
    "strict-peer-dependencies",
    "strict-peer-deps",
    "strict-semver",
    "strict-ssl",
    "symlink",
    "tilde",
    "timing",
    "trust-lockfile",
    "unicode",
    "update-checksums",
    "update-notifier",
    "use-running-store-server",
    "use-stderr",
    "use-store-server",
    "verify-store-integrity",
    "workspace-root",
    "workspaces-update",
    "ws",
    "yes",
    "ignore-engines",
    "ignore-optional",
    "immutable",
    "legacy-peer-deps",
    "non-interactive",
    "package-lock-only",
    "prefer-frozen-lockfile",
    "save-catalog",
    "save-dev",
    "save-exact",
    "save-optional",
    "save-peer",
    "save-prod",
    "shamefully-hoist",
    "unsafe-perm",
];

/// The ones of [`IGNORED_FOREIGN_FLAGS`]'s table that take a value.
const IGNORED_FOREIGN_OPTIONS: &[&str] = &[
    "allow-build",
    "allow-directory",
    "allow-file",
    "allow-git",
    "allow-remote",
    "allow-scripts",
    "also",
    "before",
    "cache",
    "cache-folder",
    "cache-max",
    "cache-min",
    "cert",
    "changed-files-ignore-pattern",
    "child-concurrency",
    "dir",
    "enjoy-by",
    "fetch-min-speed-ki-bps",
    "fetch-retries",
    "fetch-retry-factor",
    "fetch-retry-maxtimeout",
    "fetch-retry-mintimeout",
    "fetch-timeout",
    "fetch-warn-timeout-ms",
    "filter-prod",
    "global-folder",
    "global-pnpmfile",
    "globalconfig",
    "hoist-pattern",
    "http-proxy",
    "https-proxy",
    "include",
    "install-strategy",
    "key",
    "link-folder",
    "local-address",
    "location",
    "lockfile-dir",
    "lockfile-version",
    "loglevel",
    "logs-dir",
    "logs-max",
    "maxsockets",
    "merge-git-branch-lockfiles-branch-pattern",
    "min-release-age",
    "mode",
    "mutex",
    "network-timeout",
    "node-gyp",
    "node-linker",
    "node-options",
    "noproxy",
    "npmrc-auth-file",
    "only",
    "otp",
    "package-import-method",
    "pnpmfile",
    "pnpr-server",
    "preferred-cache-folder",
    "prefix",
    "proxy",
    "public-hoist-pattern",
    "replace-registry-host",
    "reporter",
    "save-catalog-name",
    "save-prefix",
    "scope",
    "script-shell",
    "state-dir",
    "store-dir",
    "tag",
    "test-pattern",
    "umask",
    "use-yarnrc",
    "user-agent",
    "userconfig",
    "virtual-store-dir",
    "workspace",
    "workspace-concurrency",
    "workspace-packages",
];

/// Only `fnpm x` skips these: the flags of npx and pnpm dlx, and the npm flags that `fnpm add`
/// has too (bun `IGNORED_NPX_FLAGS`).
const IGNORED_NPX_FLAGS: &[&str] = &[
    "always-spawn",
    "call",
    "ignore-existing",
    "local",
    "shell-auto-fallback",
    "shell-mode",
    "dry-run",
    "force",
    "global",
    "if-present",
    "ignore-scripts",
    "prefer-offline",
    "production",
    "save",
    "workspaces",
];
const IGNORED_NPX_OPTIONS: &[&str] = &["node-arg", "npm", "shell", "cpu", "libc", "omit", "os"];

/// npm's one-letter flags that `fnpm x` doesn't have.
const FOREIGN_SHORT_FLAGS: &str = "acdfglqsyBDEHOPS?";
/// The ones that take a value; skipped only as `-w=value`.
const FOREIGN_SHORT_OPTIONS: &str = "mwCL";

/// The boolean long flags of `fnpm add` (bun `ADD_PARAMS`).
const ADD_BOOLEAN_FLAGS: &[&str] = &[
    "yarn",
    "production",
    "prod",
    "no-save",
    "save",
    "dry-run",
    "frozen-lockfile",
    "force",
    "no-cache",
    "silent",
    "quiet",
    "verbose",
    "no-progress",
    "no-summary",
    "no-verify",
    "offline",
    "prefer-offline",
    "ignore-scripts",
    "trust",
    "global",
    "no-env-file",
    "save-text-lockfile",
    "lockfile-only",
    "use-system-ca",
    "use-openssl-ca",
    "use-bundled-ca",
    "help",
    "dev",
    "development",
    "optional",
    "peer",
    "save-prod",
    "exact",
    "analyze",
    "only-missing",
];

/// clap's `IgnoredLongFlags::value_is_next_arg`.
fn ignored_value_is_next(
    flags: &[&str],
    options: &[&str],
    prefixes: &[&str],
    name: &str,
    next: &str,
    is_own_flag: impl Fn(&str) -> bool,
) -> Option<bool> {
    if options.contains(&name) {
        return Some(!next.starts_with('-'));
    }
    if flags.contains(&name)
        || name.strip_prefix("no-").is_some_and(|positive| {
            flags.contains(&positive)
                || options.contains(&positive)
                || (!flags.is_empty() && is_own_flag(positive))
        })
    {
        return Some(next == "true" || next == "false");
    }
    prefixes
        .iter()
        .any(|prefix| name.starts_with(prefix))
        .then_some(false)
}

/// bun `CommandLineArguments::bunx_skips_flag`: `None` when `arg` is not skipped, else
/// whether `next` is its value.
pub fn skips_flag(arg: &str, next: &str) -> Option<bool> {
    let name_end = arg.find('=').unwrap_or(arg.len());
    let has_value = name_end < arg.len();
    let value_is_next = if let Some(name) = arg[..name_end].strip_prefix("--") {
        let setting = name.strip_prefix("config.").unwrap_or(name);
        if matches!(
            setting,
            "userconfig"
                | "globalconfig"
                | "npmrc-auth-file"
                | "use-yarnrc"
                | "replace-registry-host"
                | "pnpr-server"
                | "ca"
                | "cafile"
        ) || setting.starts_with("//")
            || setting.ends_with(":registry")
        {
            return None;
        }
        let is_own_flag = |n: &str| ADD_BOOLEAN_FLAGS.contains(&n);
        ignored_value_is_next(
            IGNORED_NPX_FLAGS,
            IGNORED_NPX_OPTIONS,
            &[],
            name,
            next,
            is_own_flag,
        )
        .or_else(|| {
            ignored_value_is_next(
                IGNORED_FOREIGN_FLAGS,
                IGNORED_FOREIGN_OPTIONS,
                &["config."],
                name,
                next,
                is_own_flag,
            )
        })
        .or_else(|| (is_own_flag(name) && !has_value).then_some(false))?
    } else {
        let letters = arg[..name_end].strip_prefix('-')?;
        let mut chars = letters.chars();
        match (chars.next(), chars.next()) {
            (Some(letter), None) if has_value && FOREIGN_SHORT_OPTIONS.contains(letter) => false,
            (Some(_), _) if letters.chars().all(|c| FOREIGN_SHORT_FLAGS.contains(c)) => false,
            _ => return None,
        }
    };
    Some((value_is_next || arg == "--node-arg") && !has_value)
}

/// `<name>=<value>` or `<name> <value>`: `Some(Ok(value))`, `Some(Err)` when the value is
/// empty or missing, `None` when `argv[*i]` is not `name`.
fn flag_value(argv: &[String], i: &mut usize, name: &str) -> Option<Result<String, String>> {
    let rest = argv[*i].strip_prefix(name)?;
    let value = if rest.is_empty() {
        *i += 1;
        argv.get(*i).cloned().unwrap_or_default()
    } else {
        rest.strip_prefix('=')?.to_owned()
    };
    if value.is_empty() {
        return Some(Err(format!("{name} requires a value")));
    }
    Some(Ok(value))
}

fn error(message: impl Into<String>) -> Parsed {
    Parsed::Error {
        message: message.into(),
        hint: None,
    }
}

/// Parses the arguments after `x` (global flags typed before `x` included). `cwd` is where
/// `--cwd` starts from.
pub fn parse(argv: &[String], cwd: &Path) -> Parsed {
    parse_mode(argv, cwd, false)
}

pub(super) fn parse_create(argv: &[String], cwd: &Path) -> Parsed {
    parse_mode(argv, cwd, true)
}

fn parse_mode(argv: &[String], cwd: &Path, create: bool) -> Parsed {
    let mut opts = XArgs {
        cwd: cwd.to_path_buf(),
        ..XArgs::default()
    };
    let mut package: Option<String> = None;
    let mut has_version = false;
    let mut has_revision = false;
    let mut ca_stores: Vec<&'static str> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if package.is_some() {
            opts.passthrough.push(arg.to_owned());
            i += 1;
            continue;
        }
        if !arg.starts_with('-') {
            package = Some(arg.to_owned());
            i += 1;
            continue;
        }
        macro_rules! value {
            ($name:expr) => {
                match flag_value(argv, &mut i, $name) {
                    Some(Ok(v)) => Some(v),
                    Some(Err(message)) => return error(message),
                    None => None,
                }
            };
        }
        match arg {
            "--force" | "--no-git" | "--no-package-json" if create => {}
            "--version" | "-v" => has_version = true,
            "--revision" => has_revision = true,
            "--verbose" => opts.verbose = true,
            "--silent" => opts.silent = true,
            "--bun" | "-b" => opts.run_in_bun = true,
            "--no-install" | "--no" | "--no-yes" | "--yes=false" | "-n" | "--offline" => {
                opts.no_install = true;
            }
            "--package" | "-p" => {
                i += 1;
                let Some(name) = argv.get(i) else {
                    return error("--package requires a package name");
                };
                if name.is_empty() {
                    return error("--package requires a non-empty package name");
                }
                opts.specified_package = Some(name.clone());
            }
            "--yes" if argv.get(i + 1).is_some_and(|next| next == "false") => {
                opts.no_install = true;
                i += 1;
            }
            "--trust" => opts.trust = true,
            "--ignore-scripts" | "--ignore-scripts=true" => opts.ignore_scripts = true,
            "--help" | "-h" => return Parsed::Help,
            "--" => {}
            _ => {
                if let Some(name) = arg
                    .strip_prefix("--package=")
                    .or_else(|| arg.strip_prefix("-p="))
                {
                    if name.is_empty() {
                        return error("--package requires a non-empty package name");
                    }
                    opts.specified_package = Some(name.to_owned());
                } else if let Some(file) = value!("--env-file") {
                    opts.env_files.push(file);
                } else if let Some(dir) = value!("--cwd") {
                    let next = opts.cwd.join(&dir);
                    if !next.is_dir() {
                        return error(format!("Could not change directory to \"{dir}\""));
                    }
                    opts.cwd = next;
                } else if let Some(age) = value!("--minimum-release-age") {
                    if fnpm_config::files::parse_seconds(&age).is_none() || !valid_age(&age) {
                        return error(format!(
                            "Expected --minimum-release-age to be a positive number: {age}"
                        ));
                    }
                    opts.minimum_release_age = Some(age);
                } else if let Some(url) = value!("--registry") {
                    opts.registry = Some(url);
                } else if let Some(url) = value!("--reg") {
                    // `--reg` and `--config.registry` are npm's and pnpm's names for it.
                    opts.registry = Some(url);
                } else if let Some(url) = value!("--config.registry") {
                    opts.registry = Some(url);
                } else if let Some(ca) = value!("--ca") {
                    opts.ca.push(ca);
                } else if let Some(path) = value!("--cafile") {
                    opts.cafile = Some(path);
                } else if let Some((_, store)) = CA_STORE_FLAGS.iter().find(|(f, _)| *f == arg) {
                    ca_stores.push(store);
                } else if let Some(value_is_next) =
                    skips_flag(arg, argv.get(i + 1).map_or("", String::as_str))
                {
                    i += usize::from(value_is_next);
                } else {
                    return error(format!(
                        "fnpmx has no {arg} flag. Flags for the package go after its name."
                    ));
                }
            }
        }
        i += 1;
    }

    if ca_stores.len() > 1 {
        return error(
            "choose exactly one of --use-system-ca, --use-openssl-ca, or --use-bundled-ca",
        );
    }
    opts.ca_store = ca_stores.first().copied();

    let package = package.filter(|p| !p.is_empty() || (create && !opts.passthrough.is_empty()));
    if let Some(specified) = opts.specified_package.clone() {
        let Some(bin) = package else {
            return Parsed::Error {
                message: "When using --package, you must specify the binary to run".into(),
                hint: Some(
                    "  usage: fnpmx --package=<package-name> <binary-name> [args...]".into(),
                ),
            };
        };
        opts.binary_name = Some(bin);
        opts.package_name = specified;
    } else {
        let Some(package) = package else {
            return if has_revision {
                Parsed::Revision
            } else if has_version {
                Parsed::Version
            } else {
                Parsed::Usage
            };
        };
        opts.package_name = package;
    }
    Parsed::Run(Box::new(opts))
}

/// The value becomes part of a directory name, and `fnpm add` stops at the first byte that is
/// not part of a number: only digits, a dot and a unit word (`10d`, `2 weeks`).
fn valid_age(age: &str) -> bool {
    !age.contains(['/', '\\', ':']) && !age.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Parsed {
        let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        parse(&argv, Path::new("/"))
    }

    fn run(args: &[&str]) -> XArgs {
        match p(args) {
            Parsed::Run(x) => *x,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn package_and_passthrough() {
        let x = run(&["--trust", "tool@1", "--version", "-p", "x"]);
        assert_eq!(x.package_name, "tool@1");
        assert_eq!(x.passthrough, ["--version", "-p", "x"]);
        assert!(x.trust);
    }

    #[test]
    fn package_flag() {
        let x = run(&["-p", "@angular/cli", "ng", "new"]);
        assert_eq!(x.package_name, "@angular/cli");
        assert_eq!(x.binary_name.as_deref(), Some("ng"));
        let x = run(&["--package=a", "b"]);
        assert_eq!(x.package_name, "a");
        assert!(matches!(p(&["-p", "a"]), Parsed::Error { .. }));
        assert!(matches!(p(&["--package"]), Parsed::Error { .. }));
    }

    #[test]
    fn version_usage_help() {
        assert_eq!(p(&[]), Parsed::Usage);
        assert_eq!(p(&["--version"]), Parsed::Version);
        assert_eq!(p(&["--revision"]), Parsed::Revision);
        assert_eq!(p(&["-h"]), Parsed::Help);
        assert_eq!(
            p(&["--version", "--env-file"]),
            Parsed::Error {
                message: "--env-file requires a value".into(),
                hint: None
            }
        );
    }

    #[test]
    fn unknown_flags_are_errors() {
        for bad in [
            "--nope",
            "--cwdx=sub",
            "-Z",
            "-w",
            "--userconfig",
            "--userconfig=.npmrc",
            "--npmrc-auth-file",
            "--config.@hello:registry=http://localhost:1",
            "--config.//localhost:1/:_authToken=hello",
            "--trust=true",
            "-",
        ] {
            match p(&[bad, "tool"]) {
                Parsed::Error { message, .. } => assert!(message.contains(bad), "{bad}: {message}"),
                other => panic!("{bad}: {other:?}"),
            }
        }
    }

    #[test]
    fn npx_flags_are_skipped() {
        let flags = "--no --quiet -q -n -c -dd --offline --prefer-offline --loglevel=error --registry=http://localhost:1 --yes=true --";
        let mut args: Vec<&str> = flags.split(' ').collect();
        args.push("local-tool");
        let x = run(&args);
        assert_eq!(x.package_name, "local-tool");
        assert!(x.no_install);
        assert_eq!(run(&["--loglevel", "hello", "tool"]).package_name, "tool");
        assert_eq!(run(&["--force", "true", "tool"]).package_name, "tool");
        assert_eq!(
            run(&["--node-arg", "--inspect", "tool"]).package_name,
            "tool"
        );
        assert_eq!(run(&["-w=hello", "tool"]).package_name, "tool");
        assert!(run(&["--yes", "false", "tool"]).no_install);
    }

    #[test]
    fn values() {
        assert!(matches!(p(&["--cwd"]), Parsed::Error { .. }));
        assert!(matches!(p(&["--cwd=", "x"]), Parsed::Error { .. }));
        assert!(matches!(
            p(&["--minimum-release-age=abc", "x"]),
            Parsed::Error { .. }
        ));
        assert!(matches!(
            p(&["--minimum-release-age=5:6", "x"]),
            Parsed::Error { .. }
        ));
        assert_eq!(
            run(&["--minimum-release-age", "2 weeks", "x"])
                .minimum_release_age
                .as_deref(),
            Some("2 weeks")
        );
        assert_eq!(
            run(&["--reg=http://r", "x"]).registry.as_deref(),
            Some("http://r")
        );
        assert_eq!(
            run(&["--config.registry", "http://r", "x"])
                .registry
                .as_deref(),
            Some("http://r")
        );
        assert!(matches!(
            p(&["--use-openssl-ca", "--use-bundled-ca", "x"]),
            Parsed::Error { .. }
        ));
    }
}
