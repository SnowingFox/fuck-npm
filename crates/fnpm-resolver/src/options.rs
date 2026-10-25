//! Resolver-owned command-line values: `--minimum-release-age`, `--minimum-release-age-excludes`
//! and the `--cpu` / `--os` / `--libc` install target, plus the parsed form of
//! `minimumReleaseAgeExcludes` that version picking consults.

use fnpm_semver::Version;
use fnpm_types::{CpuSet, LibcSet, OsSet, Platform};

use crate::{Error, Result};

/// bun `fmt::parse_seconds`: a number of seconds, or a number followed by a unit (`90m`, `3d`,
/// `2 weeks`). Negative, infinite and malformed values are `None`.
pub fn parse_seconds(s: &str) -> Option<f64> {
    let seconds = match s.parse::<f64>() {
        Ok(v) if !s.is_empty() => v,
        _ => {
            let digits = s
                .bytes()
                .take_while(|c| c.is_ascii_digit() || *c == b'.')
                .count();
            let per_unit = match s[digits..].trim_start() {
                "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
                "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
                "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
                "d" | "day" | "days" => 86_400.0,
                "w" | "week" | "weeks" => 604_800.0,
                _ => return None,
            };
            if digits == 0 {
                return None;
            }
            s[..digits].parse::<f64>().ok()? * per_unit
        }
    };
    (seconds >= 0.0 && seconds.is_finite()).then_some(seconds)
}

/// `--minimum-release-age <N|duration>` in milliseconds. `0` clears the gate (`None`); anything
/// unparsable is the fatal error bun prints.
pub fn parse_minimum_release_age(value: &str) -> Result<Option<u64>> {
    let Some(seconds) = parse_seconds(value) else {
        return Err(Error::InvalidOption {
            flag: "--minimum-release-age".into(),
            message: format!("Expected --minimum-release-age to be a positive number: {value}"),
        });
    };
    let ms = (seconds * 1000.0).round();
    Ok((ms > 0.0).then_some(if ms >= u64::MAX as f64 {
        u64::MAX
    } else {
        ms as u64
    }))
}

/// `--minimum-release-age-excludes`: repeatable, each value comma-separated, entries trimmed and
/// empty ones dropped. The result replaces the configured list.
pub fn split_minimum_release_age_excludes<S: AsRef<str>>(values: &[S]) -> Vec<String> {
    values
        .iter()
        .flat_map(|v| v.as_ref().split(','))
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The install target after `--os` / `--cpu` / `--libc`. An empty list keeps that part of
/// `base`; unknown names are fatal with the list of valid values.
pub fn platform_from_cli<S: AsRef<str>>(
    base: Platform,
    os: &[S],
    cpu: &[S],
    libc: &[S],
) -> Result<Platform> {
    let mut platform = base;
    let invalid = |e: fnpm_types::PlatformError, flag: &str| Error::InvalidOption {
        flag: flag.into(),
        message: e.to_string(),
    };
    if !os.is_empty() {
        platform.os = OsSet::parse_cli(os).map_err(|e| invalid(e, "--os"))?;
    }
    if !cpu.is_empty() {
        platform.cpu = CpuSet::parse_cli(cpu).map_err(|e| invalid(e, "--cpu"))?;
    }
    if !libc.is_empty() {
        platform.libc = LibcSet::parse_cli(libc).map_err(|e| invalid(e, "--libc"))?;
    }
    Ok(platform)
}

/// `minimumReleaseAgeExcludes` in pnpm's grammar: `name` exempts every version of a package,
/// `name@1.2.3 || 1.2.4` only those versions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseAgeExcludes {
    entries: Vec<(String, Option<Version>)>,
}

impl ReleaseAgeExcludes {
    /// Parses the entries. A malformed entry (`foo@`, `foo@^1`) is dropped with a warning, never
    /// widened to the whole package.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> (Self, Vec<String>) {
        let mut list = Vec::new();
        let mut warnings = Vec::new();
        for entry in entries {
            let entry = entry.as_ref();
            let name_start = usize::from(entry.starts_with('@'));
            let Some(at) = entry[name_start..].find('@') else {
                list.push((entry.to_owned(), None));
                continue;
            };
            let (name, specs) = (&entry[..name_start + at], &entry[name_start + at + 1..]);
            let first = list.len();
            for spec in specs.split("||") {
                match Version::parse(spec.trim()) {
                    Ok(v) if !spec.trim().is_empty() => list.push((name.to_owned(), Some(v))),
                    _ => {
                        list.truncate(first);
                        warnings.push(format!(
                            "ignoring minimumReleaseAgeExcludes entry \"{entry}\": expected a package name or exact versions, e.g. \"{name}@1.2.3\" or \"{name}@1.2.3 || 1.2.4\""
                        ));
                        break;
                    }
                }
            }
        }
        (Self { entries: list }, warnings)
    }

    /// Every version of `name` bypasses the gate.
    pub fn excludes_package(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, v)| n == name && v.is_none())
    }

    /// This exact version of `name` is listed.
    pub fn excludes_version(&self, name: &str, version: &Version) -> bool {
        self.entries
            .iter()
            .any(|(n, v)| n == name && v.as_ref() == Some(version))
    }

    /// No entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
