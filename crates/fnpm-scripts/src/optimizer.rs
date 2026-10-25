//! Postinstall optimizer (bun `postinstall_optimizer.rs`).
//!
//! `nativeDependencies` (default: esbuild, @anthropic-ai/claude-code): skip the trusted
//! package's scripts when one of its dependencies with explicit os+cpu matches the target,
//! and point its bins into that platform package. `ignoreScripts` (default: sharp >= 0.33.0
//! from the registry): never run its scripts. A present array in the root package.json
//! replaces the default (an empty array turns the default off).

use std::cmp::Ordering;

use fnpm_types::{CpuSet, LockGraph, OsSet, PackageId, Resolution};

/// What to do with a trusted package's lifecycle scripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run,
    Skip,
    /// Skip scripts and redirect bins to the platform package.
    NativeBinlink {
        platform_package: PackageId,
    },
}

/// The two lists, from the root package.json or the defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Optimizer {
    /// Names whose scripts are replaced by linking the matching platform package's bins.
    pub native_dependencies: Vec<String>,
    /// Names whose scripts never run. Entries from the default list carry a minimum version
    /// (`sharp@>=0.33.0`); names from package.json apply to every version.
    pub ignore_scripts: Vec<String>,
}

/// bun `DEFAULT_NATIVE_BINLINKS_NAME_HASHES`.
pub const DEFAULT_NATIVE_DEPENDENCIES: [&str; 2] = ["esbuild", "@anthropic-ai/claude-code"];
/// bun `DEFAULT_IGNORE`: `(name, minimum version)`.
pub const DEFAULT_IGNORE_SCRIPTS: [(&str, &str); 1] = [("sharp", "0.33.0")];

const NATIVE_FLAGS: [&str; 2] = [
    "FNPM_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER",
    "BUN_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER",
];
const IGNORE_FLAGS: [&str; 2] = [
    "FNPM_FEATURE_FLAG_DISABLE_IGNORE_SCRIPTS",
    "BUN_FEATURE_FLAG_DISABLE_IGNORE_SCRIPTS",
];

impl Default for Optimizer {
    fn default() -> Self {
        Self {
            native_dependencies: DEFAULT_NATIVE_DEPENDENCIES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ignore_scripts: DEFAULT_IGNORE_SCRIPTS
                .iter()
                .map(|(n, v)| format!("{n}@>={v}"))
                .collect(),
        }
    }
}

/// bun `from_string_array_group`: `Some(names)` when `value` is an array (non-string and empty
/// entries skipped), `None` otherwise, so a non-array keeps the default.
fn string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let array = value?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

impl Optimizer {
    /// From the root package.json `nativeDependencies` / `ignoreScripts` (defaults when
    /// absent). `BUN_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER` /
    /// `..._DISABLE_IGNORE_SCRIPTS` (and `FNPM_` equivalents) disable each half.
    pub fn from_root(root: &serde_json::Value, env: &fnpm_config::Env) -> Self {
        let defaults = Self::default();
        let flag = |keys: [&str; 2]| keys.iter().any(|k| env.truthy(k) == Some(true));
        // Disabling the ignore half means every trusted package runs its scripts, natives
        // included; their own postinstall then does what the bin redirect would.
        let ignore_off = flag(IGNORE_FLAGS);
        let native_dependencies = if ignore_off || flag(NATIVE_FLAGS) {
            Vec::new()
        } else {
            string_array(root.get("nativeDependencies")).unwrap_or(defaults.native_dependencies)
        };
        let ignore_scripts = if ignore_off {
            Vec::new()
        } else {
            string_array(root.get("ignoreScripts")).unwrap_or(defaults.ignore_scripts)
        };
        Self {
            native_dependencies,
            ignore_scripts,
        }
    }

    /// The decision for a trusted package `id`.
    pub fn decide(
        &self,
        graph: &LockGraph,
        id: PackageId,
        platform: &fnpm_types::Platform,
    ) -> Decision {
        let Some(pkg) = graph.get(id) else {
            return Decision::Run;
        };
        if self.is_ignored(&pkg.name, &pkg.resolution) {
            return Decision::Skip;
        }
        if self.native_dependencies.contains(&pkg.name)
            && let Some(platform_package) = native_replacement(graph, id, platform)
        {
            return Decision::NativeBinlink { platform_package };
        }
        Decision::Run
    }

    /// For the linker: the platform package whose bins replace `id`'s, when `id` is a native
    /// dependency (independent of trust, like bun's `maybe_replace_node_modules_path`).
    pub fn native_binlink_target(
        &self,
        graph: &LockGraph,
        id: PackageId,
        platform: &fnpm_types::Platform,
    ) -> Option<PackageId> {
        let pkg = graph.get(id)?;
        if !self.native_dependencies.contains(&pkg.name) {
            return None;
        }
        native_replacement(graph, id, platform)
    }

    fn is_ignored(&self, name: &str, resolution: &Resolution) -> bool {
        self.ignore_scripts
            .iter()
            .any(|entry| match entry.split_once("@>=") {
                Some((n, min)) if !n.is_empty() => {
                    // Only a registry package has a version here. A git fork or a workspace with the
                    // same name still has to run its own install scripts.
                    n == name
                        && resolution.npm_version().is_some_and(|v| {
                            compare_versions(v, min).is_some_and(|o| o != Ordering::Less)
                        })
                }
                _ => entry == name,
            })
    }
}

/// bun `get_native_binlink_replacement_package_id`: the first dependency with explicit `os`
/// and `cpu` lists that matches the target (libc checked for every kind of dependency).
fn native_replacement(
    graph: &LockGraph,
    id: PackageId,
    platform: &fnpm_types::Platform,
) -> Option<PackageId> {
    let pkg = graph.get(id)?;
    pkg.dependencies
        .iter()
        .filter_map(|e| e.target)
        .find(|&target| {
            graph.get(target).is_some_and(|dep| {
                let c = dep.platform;
                c.os != OsSet::ALL && c.cpu != CpuSet::ALL && c.matches(platform)
            })
        })
}

/// Semver precedence of two `major.minor.patch[-pre][+build]` strings (build ignored).
/// `None` when either does not parse.
pub(crate) fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    fn parse(v: &str) -> Option<([u64; 3], Option<&str>)> {
        let v = v.trim().trim_start_matches(['v', '=']);
        let v = v.split_once('+').map_or(v, |(core, _)| core);
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (v, None),
        };
        let mut parts = core.split('.');
        let mut nums = [0u64; 3];
        for n in &mut nums {
            *n = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some((nums, pre))
    }
    let (an, ap) = parse(a)?;
    let (bn, bp) = parse(b)?;
    Some(an.cmp(&bn).then_with(|| match (ap, bp) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => compare_prerelease(x, y),
    }))
}

fn compare_prerelease(a: &str, b: &str) -> Ordering {
    let mut xs = a.split('.');
    let mut ys = b.split('.');
    loop {
        match (xs.next(), ys.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}
