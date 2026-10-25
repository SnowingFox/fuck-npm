//! Flag values to validated `fnpm_core::InstallOptions`, with bun's messages for invalid
//! values and conflicting flags. A port of `pmOptions` / `installOptions` in
//! `packages/fuck-npm/src/core/options.ts`; the order of the checks is the order the TS code
//! throws in, so the first error reported is the same.

use std::path::{Component, Path, PathBuf};

use fnpm_config::{Backend, LinkerSetting, LogLevel};
use fnpm_core::options::{InstallCommand, UpdateGroups};
use fnpm_core::{InstallOptions, PmOptions};
use fnpm_types::DependencyGroup;

use crate::error::CliError;
use crate::parse::{Parsed, Value};

const VALID_CPU: [&str; 11] = [
    "arm", "arm64", "ia32", "mips", "mipsel", "ppc", "ppc64", "s390", "s390x", "x32", "x64",
];
const VALID_OS: [&str; 8] = [
    "aix", "darwin", "freebsd", "linux", "openbsd", "sunos", "win32", "android",
];
const VALID_LIBC: [&str; 2] = ["glibc", "musl"];

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Node's `path.resolve(base, path)` for an absolute `base`: lexical, no filesystem access.
/// Works on path components, so a base that is not UTF-8 survives; a relative `base` (the
/// start directory could not be read) is only joined.
pub fn resolve_path(base: &Path, path: &str) -> PathBuf {
    let joined = base.join(path);
    if cfg!(windows) || !joined.is_absolute() {
        return joined;
    }
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
        }
    }
    out
}

/// `--cwd` is resolved against the starting directory, like bun's chdir. An absolute path
/// is kept exactly as typed (`isAbsolute(cwd) ? cwd : resolve(start, cwd)`): normalising
/// `/link/../x` lexically would change where a symlinked path leads.
pub fn resolve_cwd(values: &Parsed, start: &Path) -> PathBuf {
    match values.string("cwd") {
        None | Some("") => start.to_path_buf(),
        Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
        Some(cwd) => resolve_path(start, cwd),
    }
}

/// `logLevelOf`: silent > verbose > quiet > default.
pub fn log_level_of(values: &Parsed) -> LogLevel {
    if values.bool("silent") {
        LogLevel::Silent
    } else if values.bool("verbose") {
        LogLevel::Verbose
    } else if values.bool("quiet") {
        LogLevel::Quiet
    } else {
        LogLevel::Default
    }
}

fn parse_linker(value: Option<&str>) -> Result<Option<LinkerSetting>, CliError> {
    match value {
        None => Ok(None),
        Some("isolated") => Ok(Some(LinkerSetting::Isolated)),
        Some("hoisted") => Ok(Some(LinkerSetting::Hoisted)),
        Some(other) => Err(CliError::invalid(format!(
            "Invalid value for --linker: \"{other}\". Must be 'isolated' or 'hoisted'."
        ))),
    }
}

fn parse_omit(values: &[String]) -> Result<Vec<String>, CliError> {
    let mut out: Vec<String> = Vec::new();
    for value in values.iter().flat_map(|entry| entry.split(',')) {
        let trimmed = value.trim();
        if matches!(trimmed, "dev" | "optional" | "peer") {
            if !out.iter().any(|existing| existing == trimmed) {
                out.push(trimmed.to_owned());
            }
            continue;
        }
        return Err(CliError::invalid(format!(
            "invalid `omit` value: '{trimmed}'"
        )));
    }
    Ok(out)
}

fn parse_u16(name: &str, value: Option<&str>) -> Result<Option<u16>, CliError> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value.parse::<u64>() {
        Ok(parsed) if is_digits(value) && parsed <= 65535 => Ok(Some(parsed as u16)),
        _ => Err(CliError::invalid(format!(
            "Expected --{name} to be a number between 0 and 65535: {value}"
        ))),
    }
}

/// `--concurrent-scripts`: 0 or anything unparsable means the default.
fn parse_concurrent_scripts(value: Option<&str>) -> Option<usize> {
    let value = value?;
    if !is_digits(value) {
        return None;
    }
    // Number("99999999999999999999") is finite and > 0 in JS; saturate the same way.
    let parsed = value.parse::<usize>().unwrap_or(usize::MAX);
    (parsed > 0).then_some(parsed)
}

fn unit_seconds(unit: &str) -> Option<f64> {
    Some(match unit {
        "" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        "w" | "week" | "weeks" => 604_800.0,
        _ => return None,
    })
}

/// bun's `parse_seconds`: a number of seconds, or `90m`, `12 hours`, `3d`, `2 weeks`.
/// Returns milliseconds; `None` for 0 (no gate).
pub fn parse_release_age(value: Option<&str>) -> Result<Option<u64>, CliError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let fail = || {
        CliError::invalid(format!(
            "Expected --minimum-release-age to be a positive number: {value}"
        ))
    };
    // /^\s*(\d+(?:\.\d+)?)\s*([a-z]*)\s*$/i
    let text = value.trim();
    let number_end = text
        .find(|char: char| !(char.is_ascii_digit() || char == '.'))
        .unwrap_or(text.len());
    let number = &text[..number_end];
    let unit = text[number_end..].trim_start();
    let valid_number = match number.split_once('.') {
        None => is_digits(number),
        Some((whole, fraction)) => is_digits(whole) && is_digits(fraction),
    };
    if !valid_number || !unit.chars().all(|char| char.is_ascii_alphabetic()) {
        return Err(fail());
    }
    let factor = unit_seconds(&unit.to_ascii_lowercase()).ok_or_else(fail)?;
    let seconds: f64 = number.parse().map_err(|_| fail())?;
    // Math.round; the value is never negative, where it agrees with `f64::round`.
    let ms = (seconds * factor * 1000.0).round();
    Ok(if ms == 0.0 { None } else { Some(ms as u64) })
}

/// `--cpu` / `--os` / `--libc`: repeatable, comma-separated, `*` / `any` / `none`, `!name`.
fn parse_platform_list(
    what: &str,
    values: &[String],
    valid: &[&str],
) -> Result<Vec<String>, CliError> {
    let mut out = Vec::new();
    for raw in values.iter().flat_map(|entry| entry.split(',')) {
        let value = raw.trim();
        if value.is_empty() {
            continue;
        }
        let name = value.trim_start_matches('!');
        if matches!(value, "*" | "any" | "none") || valid.contains(&name) {
            out.push(value.to_owned());
            continue;
        }
        return Err(CliError::invalid(format!(
            "invalid {what}: '{value}'. valid values: *, any, {}. use !name to negate",
            valid.join(", ")
        )));
    }
    Ok(out)
}

fn parse_registry(value: Option<&str>) -> Result<Option<String>, CliError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.starts_with("http://") || value.starts_with("https://") {
        return Ok(Some(value.to_owned()));
    }
    Err(CliError::invalid(format!(
        "Registry URL must start with 'https://' or 'http://': \"{}\"",
        redact_url(value)
    )))
}

/// Never echo credentials back: the first `//userinfo@` and every `token=` / `_authToken=`
/// value (`redactUrl`).
pub fn redact_url(value: &str) -> String {
    // .replace(/\/\/[^/@]*@/, "//<redacted>@")
    let mut text = value.to_owned();
    let bytes = text.as_bytes();
    let mut start = 0;
    while let Some(found) = text[start..].find("//") {
        let at = start + found;
        let after = &text[at + 2..];
        let end = after.find(['/', '@']);
        if let Some(end) = end
            && after.as_bytes()[end] == b'@'
        {
            text = format!("{}//<redacted>@{}", &text[..at], &after[end + 1..]);
            break;
        }
        start = at + 1;
        if start >= bytes.len() {
            break;
        }
    }
    // .replace(/(_authToken|token)=[^&]*/gi, "$1=<redacted>")
    let mut out = String::with_capacity(text.len());
    let lower = text.to_ascii_lowercase();
    let mut index = 0;
    while index < text.len() {
        let matched = ["_authtoken=", "token="]
            .into_iter()
            .find(|needle| lower[index..].starts_with(needle));
        if let Some(needle) = matched {
            let name_len = needle.len() - 1;
            out.push_str(&text[index..index + name_len]);
            out.push_str("=<redacted>");
            let value_start = index + needle.len();
            index = text[value_start..]
                .find('&')
                .map_or(text.len(), |end| value_start + end);
            continue;
        }
        let char = text[index..].chars().next().unwrap();
        out.push(char);
        index += char.len_utf8();
    }
    out
}

fn ca_store_of(values: &Parsed) -> Option<String> {
    if values.bool("useSystemCa") {
        Some("system".into())
    } else if values.bool("useOpensslCa") {
        Some("openssl".into())
    } else if values.bool("useBundledCa") {
        Some("bundled".into())
    } else {
        None
    }
}

/// bun's SHARED_PARAMS to `PmOptions` (`pmOptions`).
pub fn pm_options(values: &Parsed, start: &Path) -> Result<PmOptions, CliError> {
    let production = values.bool("production") || values.bool("prod");
    let frozen_lockfile = values.bool("frozenLockfile");
    let trust = values.bool("trust");
    if production && trust {
        return Err(CliError::conflict(
            "The '--production' and '--trust' flags together are not supported because the --trust flag potentially modifies the lockfile after installing packages",
        ));
    }
    if frozen_lockfile && trust {
        return Err(CliError::conflict(
            "The '--frozen-lockfile' and '--trust' flags together are not supported because the --trust flag potentially modifies the lockfile after installing packages",
        ));
    }

    // bun silently ignores an unknown or unsupported backend.
    let backend = match values.string("backend") {
        Some("clonefile") => Some(Backend::Clonefile),
        Some("hardlink") => Some(Backend::Hardlink),
        Some("symlink") => Some(Backend::Symlink),
        Some("copyfile") => Some(Backend::Copyfile),
        _ => None,
    };
    let release_age = parse_release_age(values.string("minimumReleaseAge"))?;
    let excludes = values.strings("minimumReleaseAgeExcludes");
    // `--save` is already the default, so only `--no-save` changes anything.
    let save = !values.bool("noSave");

    let omit = parse_omit(&values.strings("omit"))?;
    let cpu = parse_platform_list("cpu", &values.strings("cpu"), &VALID_CPU)?;
    let os = parse_platform_list("os", &values.strings("os"), &VALID_OS)?;
    let libc = parse_platform_list("libc", &values.strings("libc"), &VALID_LIBC)?;
    let registry = parse_registry(values.string("registry"))?;
    let concurrent_scripts = parse_concurrent_scripts(values.string("concurrentScripts"));
    let network_concurrency =
        parse_u16("network-concurrency", values.string("networkConcurrency"))?;
    let linker = parse_linker(values.string("linker"))?;

    let config = match values.get("config") {
        Some(Value::Str(path)) => Some(serde_json::Value::String(path.clone())),
        Some(Value::List(paths)) => paths
            .last()
            .map(|path| serde_json::Value::String(path.clone())),
        Some(Value::Bool(true)) => Some(serde_json::Value::Bool(true)),
        _ => None,
    };

    Ok(PmOptions {
        cwd: resolve_cwd(values, start),
        config,
        yarn: values.bool("yarn"),
        production,
        save,
        ca: values.strings("ca"),
        cafile: values.string("cafile").map(PathBuf::from),
        dry_run: values.bool("dryRun"),
        frozen_lockfile,
        force: values.bool("force"),
        cache_dir: values.string("cacheDir").map(PathBuf::from),
        no_cache: values.bool("noCache"),
        log_level: log_level_of(values),
        progress: !values.bool("noProgress"),
        summary: !values.bool("noSummary"),
        verify: !values.bool("noVerify"),
        offline: values.bool("offline"),
        prefer_offline: values.bool("preferOffline"),
        ignore_scripts: values.bool("ignoreScripts"),
        trust,
        global: values.bool("global"),
        env_file: values
            .strings("envFile")
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        no_env_file: values.bool("noEnvFile"),
        backend,
        registry,
        concurrent_scripts,
        network_concurrency,
        save_text_lockfile: values.bool("saveTextLockfile"),
        omit,
        lockfile_only: values.bool("lockfileOnly"),
        linker,
        minimum_release_age_ms: release_age,
        minimum_release_age_excludes: if excludes.is_empty() {
            None
        } else {
            Some(
                excludes
                    .iter()
                    .flat_map(|entry| entry.split(','))
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )
        },
        cpu,
        os,
        libc,
        ca_store: ca_store_of(values),
    })
}

/// Group precedence for `add`: dev > optional > peer > dependencies.
fn add_group(values: &Parsed) -> Option<DependencyGroup> {
    if values.bool("dev") || values.bool("development") {
        Some(DependencyGroup::Dev)
    } else if values.bool("optional") {
        Some(DependencyGroup::Optional)
    } else if values.bool("peer") {
        Some(DependencyGroup::Peer)
    } else if values.bool("saveProd") {
        Some(DependencyGroup::Prod)
    } else {
        None
    }
}

/// install / ci / add / remove / update to `InstallOptions`, with bun's conflict checks
/// (`installOptions`). `frozen` is set for `ci`.
pub fn install_options(
    command: InstallCommand,
    values: &Parsed,
    positionals: &[String],
    start: &Path,
    frozen: bool,
) -> Result<InstallOptions, CliError> {
    let base = pm_options(values, start)?;
    let filter = values.strings("filter");
    let catalog = match values.get("catalog") {
        Some(Value::Str(name)) => Some(name.trim().to_owned()),
        Some(Value::Bool(true)) => Some(String::new()),
        _ => None,
    };
    let analyze = values.bool("analyze");

    if base.global && !filter.is_empty() && command != InstallCommand::Update {
        return Err(CliError::conflict("--filter cannot be used with --global"));
    }
    if catalog.is_some() && base.global {
        return Err(CliError::conflict("--catalog cannot be used with --global"));
    }
    if analyze && positionals.is_empty() {
        return Err(CliError::missing(
            "Missing script(s) to analyze. Pass paths to scripts to install.",
        ));
    }
    if matches!(command, InstallCommand::Add | InstallCommand::Remove) && positionals.is_empty() {
        return Err(CliError::missing(if command == InstallCommand::Add {
            "no package specified to add"
        } else {
            "no package specified to remove"
        }));
    }
    if command == InstallCommand::Install && catalog.is_some() && positionals.is_empty() {
        return Err(CliError::missing("no package specified to add"));
    }

    let mut depth = None;
    if command == InstallCommand::Update
        && let Some(value) = values.string("depth")
    {
        if value != "0" {
            return Err(CliError::invalid(format!(
                "fnpm update --depth only accepts 0 (update direct dependencies, keep transitive dependencies locked), got '{value}'"
            )));
        }
        depth = Some(0);
    }

    let is_update = command == InstallCommand::Update;
    // On update, --production/--prod pick which entries to update; they do not omit
    // devDependencies or freeze the lockfile.
    let update_groups = UpdateGroups {
        dev: is_update && (values.bool("dev") || values.bool("development")),
        prod: is_update && base.production,
        no_optional: is_update && values.bool("noOptional"),
    };
    let global = base.global;
    let pm = PmOptions {
        production: if is_update { false } else { base.production },
        frozen_lockfile: base.frozen_lockfile || frozen,
        ..base
    };
    Ok(InstallOptions {
        pm,
        command,
        packages: positionals.to_vec(),
        group: if is_update { None } else { add_group(values) },
        save_prod: values.bool("saveProd"),
        exact: values.bool("exact"),
        filter,
        analyze,
        only_missing: values.bool("onlyMissing") || analyze,
        catalog,
        latest: values.bool("latest"),
        // --global silently disables --recursive.
        recursive: values.bool("recursive") && !global,
        update_groups,
        depth,
        dlx: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolves_like_node_path_resolve() {
        let base = Path::new("/work/app");
        for (path, expected) in [
            ("sub", "/work/app/sub"),
            ("../x/./y/", "/work/x/y"),
            ("../../../..", "/"),
            ("a//b/.", "/work/app/a/b"),
            ("/abs/../y/", "/y"),
        ] {
            assert_eq!(resolve_path(base, path), PathBuf::from(expected), "{path}");
        }
        // The start directory could not be read: never invent an absolute path.
        assert_eq!(resolve_path(Path::new("."), "sub"), PathBuf::from("./sub"));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_start_directory_is_kept() {
        use std::os::unix::ffi::OsStrExt;
        let base = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff/app"));
        let resolved = resolve_path(base, "../sub");
        assert_eq!(resolved.as_os_str().as_bytes(), b"/tmp/\xff/sub");
    }
}
