//! Environment snapshot with the lookup rules of npm and bun.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::PathBuf;

/// FxHash (rustc's hasher): env lookups are on the config hot path and keys are trusted.
#[derive(Default, Clone, Copy)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        let mut h = self.0;
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            h = (h.rotate_left(5) ^ u64::from_le_bytes(*c)).wrapping_mul(K);
        }
        for &b in rest {
            h = (h.rotate_left(5) ^ u64::from(b)).wrapping_mul(K);
        }
        self.0 = h;
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type Vars = HashMap<String, String, BuildHasherDefault<FxHasher>>;

/// fnpm's own environment variable names (docs/ARCHITECTURE.md, "Environment variables").
/// The CLI, bench and conformance harness use these exact spellings.
pub mod names {
    /// Root of fnpm's data (`~/.fnpm`).
    pub const HOME: &str = "FNPM_HOME";
    /// Parent of the store (`$FNPM_STORE_DIR/v1`).
    pub const STORE_DIR: &str = "FNPM_STORE_DIR";
    /// Cache directory.
    pub const CACHE_DIR: &str = "FNPM_CACHE_DIR";
    /// Default registry, highest-priority spelling.
    pub const REGISTRY: &str = "FNPM_REGISTRY";
    /// `0` disables the progress bar.
    pub const PROGRESS: &str = "FNPM_PROGRESS";
    /// Any value turns on verbose logging.
    pub const VERBOSE: &str = "FNPM_VERBOSE";
    /// `-g` install directory.
    pub const GLOBAL_DIR: &str = "FNPM_GLOBAL_DIR";
    /// `-g` bin directory.
    pub const GLOBAL_BIN_DIR: &str = "FNPM_GLOBAL_BIN_DIR";
    /// Global virtual store toggle.
    pub const GLOBAL_STORE: &str = "FNPM_GLOBAL_STORE";
    /// Manifest cache mode (`0`, `1`, `2`).
    pub const MANIFEST_CACHE: &str = "FNPM_MANIFEST_CACHE";
    /// Temporary directory.
    pub const TMPDIR: &str = "FNPM_TMPDIR";
}

/// An immutable copy of the environment, so config loading is deterministic and testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    vars: Vars,
}

impl Env {
    /// Snapshot of the current process environment. Variables that are not valid UTF-8 are
    /// converted lossily.
    pub fn from_process() -> Self {
        Self {
            vars: std::env::vars_os()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        }
    }

    /// An environment made of exactly these pairs (tests, embedding).
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// Sets (or replaces) one variable. Used by `--env-file` loading in the core.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    /// Removes one variable.
    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.vars.remove(key)
    }

    /// The raw value of `key`, empty values included.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// The raw value of `key` if it is set and not empty.
    pub fn get_not_empty(&self, key: &str) -> Option<&str> {
        self.get(key).filter(|v| !v.is_empty())
    }

    /// Every variable, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The first non-empty (after trimming) value among `keys`; later keys are not consulted
    /// once one is set, matching bun's registry env lookup.
    pub fn first(&self, keys: &[&str]) -> Option<&str> {
        keys.iter()
            .filter_map(|k| self.get(k))
            .map(str::trim)
            .find(|v| !v.is_empty())
    }

    /// `FNPM_CONFIG_<KEY>`, then `BUN_CONFIG_<KEY>`, then `NPM_CONFIG_<key>` and
    /// `npm_config_<key>` (npm style, `-` as `_`). `key` is given in npm form, e.g.
    /// `"registry"`, `"ignore-scripts"`.
    pub fn config_var(&self, key: &str) -> Option<&str> {
        let mut name = String::with_capacity(key.len() + 12);
        let mut lookup = |prefix: &str, upper: bool| -> Option<&str> {
            name.clear();
            name.push_str(prefix);
            name.extend(key.chars().map(|c| match c {
                '-' => '_',
                c if upper => c.to_ascii_uppercase(),
                c => c,
            }));
            self.get(&name).map(str::trim).filter(|v| !v.is_empty())
        };
        lookup("FNPM_CONFIG_", true)
            .or_else(|| lookup("BUN_CONFIG_", true))
            .or_else(|| lookup("NPM_CONFIG_", true))
            .or_else(|| lookup("npm_config_", false))
    }

    /// The default registry from the environment: `FNPM_REGISTRY`, then
    /// [`config_var`](Self::config_var)`("registry")`.
    pub fn registry(&self) -> Option<&str> {
        self.first(&[names::REGISTRY])
            .or_else(|| self.config_var("registry"))
    }

    /// bun's `string_is_truthy`: `1`, `true`, `yes`, `on` (case-insensitive).
    pub fn truthy(&self, key: &str) -> Option<bool> {
        self.get(key).map(string_is_truthy)
    }

    /// `CI`, `TDDIUM`, `GITHUB_ACTIONS`, `JENKINS_URL`, `bamboo.buildKey`.
    pub fn is_ci(&self) -> bool {
        [
            "CI",
            "TDDIUM",
            "GITHUB_ACTIONS",
            "JENKINS_URL",
            "bamboo.buildKey",
        ]
        .iter()
        .any(|k| {
            self.get(k)
                .is_some_and(|v| !v.is_empty() && v != "false" && v != "0")
        })
    }

    /// The name of the CI vendor, a port of ci-info@4.0.0's vendor table (bun
    /// `bun_core/ci_info.rs`). `None` when `CI=false` or no vendor is recognised.
    pub fn ci_name(&self) -> Option<&'static str> {
        if self
            .get("CI")
            .is_some_and(|v| v.eq_ignore_ascii_case("false") || v == "0")
        {
            return None;
        }
        let set = |k: &str| self.get(k).is_some();
        let eq = |k: &str, v: &str| self.get(k) == Some(v);
        let table: &[(&'static str, bool)] = &[
            ("agola-ci", set("AGOLA_GIT_REF")),
            ("appcircle", set("AC_APPCIRCLE")),
            ("appveyor", set("APPVEYOR")),
            ("aws-codebuild", set("CODEBUILD_BUILD_ARN")),
            ("azure-pipelines", set("TF_BUILD")),
            ("bamboo", set("bamboo_planKey")),
            ("bitbucket-pipelines", set("BITBUCKET_COMMIT")),
            ("bitrise", set("BITRISE_IO")),
            ("buddy", set("BUDDY_WORKSPACE_ID")),
            ("buildkite", set("BUILDKITE")),
            ("circleci", set("CIRCLECI")),
            ("cirrus-ci", set("CIRRUS_CI")),
            ("cloudflare-pages", set("CF_PAGES")),
            ("cloudflare-workers", set("WORKERS_CI")),
            ("codefresh", set("CF_BUILD_ID")),
            ("codemagic", set("CM_BUILD_ID")),
            ("codeship", eq("CI_NAME", "codeship")),
            ("drone", set("DRONE")),
            ("dsari", set("DSARI")),
            ("earthly", set("EARTHLY_CI")),
            ("expo-application-services", set("EAS_BUILD")),
            ("gerrit", set("GERRIT_PROJECT")),
            ("gitea-actions", set("GITEA_ACTIONS")),
            ("github-actions", set("GITHUB_ACTIONS")),
            ("gitlab-ci", set("GITLAB_CI")),
            ("gocd", set("GO_PIPELINE_LABEL")),
            ("google-cloud-build", set("BUILDER_OUTPUT")),
            ("harness-ci", set("HARNESS_BUILD_ID")),
            (
                "heroku",
                self.get("NODE")
                    .is_some_and(|v| v.contains("/app/.heroku/node/bin/node")),
            ),
            ("hudson", set("HUDSON_URL")),
            ("jenkins", set("JENKINS_URL") && set("BUILD_ID")),
            ("layerci", set("LAYERCI")),
            ("magnum-ci", set("MAGNUM")),
            ("netlify-ci", set("NETLIFY")),
            ("nevercode", set("NEVERCODE")),
            ("prow", set("PROW_JOB_ID")),
            ("releasehub", set("RELEASE_BUILD_ID")),
            ("render", set("RENDER")),
            ("sail-ci", set("SAILCI")),
            ("screwdriver", set("SCREWDRIVER")),
            ("semaphore", set("SEMAPHORE")),
            ("sourcehut", eq("CI_NAME", "sourcehut")),
            ("strider-cd", set("STRIDER")),
            ("taskcluster", set("TASK_ID") && set("RUN_ID")),
            ("teamcity", set("TEAMCITY_VERSION")),
            ("travis-ci", set("TRAVIS")),
            ("vela", set("VELA")),
            ("vercel", set("NOW_BUILDER") || set("VERCEL")),
            ("visual-studio-app-center", set("APPCENTER_BUILD_ID")),
            ("woodpecker", eq("CI", "woodpecker")),
            ("xcode-cloud", set("CI_XCODE_PROJECT")),
            ("xcode-server", set("XCS")),
        ];
        table.iter().find(|(_, hit)| *hit).map(|(name, _)| *name)
    }

    /// The user's home directory: `$HOME`, then `$USERPROFILE` (Windows). Relative or empty
    /// values are ignored, like bun.
    pub fn home(&self) -> Option<PathBuf> {
        ["HOME", "USERPROFILE"]
            .iter()
            .filter_map(|k| self.get_not_empty(k))
            .map(PathBuf::from)
            .find(|p| p.is_absolute())
    }

    /// `$XDG_CONFIG_HOME` if it is an absolute path (the XDG spec ignores relative values).
    pub fn xdg_config_home(&self) -> Option<PathBuf> {
        self.get_not_empty("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    }

    /// `${VAR}` / `${VAR?}` expansion used by quoted `.npmrc` values (bun
    /// `ini::Parser::expand_env_vars`): an undefined `${VAR}` is left as written, an undefined
    /// `${VAR?}` becomes empty. Braces nest; the outermost balanced pair names the variable.
    pub fn expand(&self, value: &str) -> String {
        if !value.contains("${") {
            return value.to_owned();
        }
        let bytes = value.as_bytes();
        let mut out = String::with_capacity(value.len());
        let mut i = 0;
        let mut literal_start = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' && i + 2 < bytes.len() && bytes[i + 1] == b'{' {
                let mut j = i + 2;
                let mut depth = 1usize;
                while j < bytes.len() && depth > 0 {
                    match bytes[j] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    if depth > 0 {
                        j += 1;
                    }
                }
                if depth == 0 {
                    out.push_str(&value[literal_start..i]);
                    let raw = &value[i + 2..j];
                    let (name, optional) = match raw.strip_suffix('?') {
                        Some(name) => (name, true),
                        None => (raw, false),
                    };
                    match self.get(name) {
                        Some(v) => out.push_str(v),
                        None if !optional => out.push_str(&value[i..=j]),
                        None => {}
                    }
                    i = j + 1;
                    literal_start = i;
                    continue;
                }
            }
            i += 1;
        }
        out.push_str(&value[literal_start..]);
        out
    }

    /// bun's `DotEnv::get_auto`: `$NAME` becomes the value of `NAME` when it is set, anything
    /// else (including `$` alone) is returned unchanged.
    pub fn get_auto<'a>(&'a self, value: &'a str) -> &'a str {
        match value.strip_prefix('$') {
            Some(name) if !name.is_empty() => self.get(name).unwrap_or(value),
            _ => value,
        }
    }
}

/// bun's `env_var::string_is_truthy`: `1`, `true`, `yes`, `on` (case-insensitive, trimmed).
pub fn string_is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_order() {
        let env = Env::from_pairs([
            ("NPM_CONFIG_REGISTRY", "https://a/"),
            ("BUN_CONFIG_REGISTRY", " "),
        ]);
        assert_eq!(env.config_var("registry"), Some("https://a/"));
        assert_eq!(env.registry(), Some("https://a/"));
        let env = Env::from_pairs([
            ("FNPM_REGISTRY", "https://f/"),
            ("FNPM_CONFIG_REGISTRY", "https://c/"),
        ]);
        assert_eq!(env.registry(), Some("https://f/"));
        let env = Env::from_pairs([
            ("FNPM_CONFIG_IGNORE_SCRIPTS", "true"),
            ("npm_config_ignore_scripts", "false"),
        ]);
        assert_eq!(env.config_var("ignore-scripts"), Some("true"));
        assert_eq!(Env::from_pairs([("X", "Yes")]).truthy("X"), Some(true));
        assert!(Env::from_pairs([("CI", "1")]).is_ci());
        assert!(!Env::default().is_ci());
    }

    #[test]
    fn expand_quoted_rules() {
        let env = Env::from_pairs([("A", "x"), ("EMPTY", "")]);
        assert_eq!(env.expand("${A}"), "x");
        assert_eq!(env.expand("pre ${A} post"), "pre x post");
        assert_eq!(env.expand("${NOPE}"), "${NOPE}");
        assert_eq!(env.expand("${NOPE?}"), "");
        assert_eq!(env.expand("${A?}"), "x");
        assert_eq!(env.expand("${EMPTY}!"), "!");
        assert_eq!(env.expand("${A"), "${A");
        assert_eq!(env.expand("$A"), "$A");
        assert_eq!(env.expand("\\${A}"), "\\x");
        assert_eq!(env.expand("Bearer ${NOPE?}"), "Bearer ");
        assert_eq!(env.expand("${A}${A}"), "xx");
        assert_eq!(env.expand("é${A}ü"), "éxü");
    }

    #[test]
    fn get_auto() {
        let env = Env::from_pairs([("TOKEN", "t")]);
        assert_eq!(env.get_auto("$TOKEN"), "t");
        assert_eq!(env.get_auto("$NOPE"), "$NOPE");
        assert_eq!(env.get_auto("$"), "$");
        assert_eq!(env.get_auto("plain"), "plain");
    }

    #[test]
    fn ci_vendor() {
        assert_eq!(
            Env::from_pairs([("GITHUB_ACTIONS", "true")]).ci_name(),
            Some("github-actions")
        );
        assert_eq!(
            Env::from_pairs([("GITHUB_ACTIONS", "true"), ("CI", "false")]).ci_name(),
            None
        );
        assert_eq!(Env::from_pairs([("JENKINS_URL", "x")]).ci_name(), None);
        assert_eq!(
            Env::from_pairs([("JENKINS_URL", "x"), ("BUILD_ID", "1")]).ci_name(),
            Some("jenkins")
        );
        assert_eq!(
            Env::from_pairs([("CI", "woodpecker")]).ci_name(),
            Some("woodpecker")
        );
        assert_eq!(Env::default().ci_name(), None);
    }

    #[test]
    fn home_dirs() {
        assert_eq!(
            Env::from_pairs([("HOME", "/h")]).home(),
            Some(PathBuf::from("/h"))
        );
        assert_eq!(Env::from_pairs([("HOME", "rel")]).home(), None);
        assert_eq!(
            Env::from_pairs([("HOME", ""), ("USERPROFILE", "/u")]).home(),
            Some(PathBuf::from("/u"))
        );
        assert_eq!(
            Env::from_pairs([("XDG_CONFIG_HOME", "x")]).xdg_config_home(),
            None
        );
    }
}
