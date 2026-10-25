//! Environment snapshot with the lookup rules of npm and bun.

use std::collections::HashMap;

/// fnpm's own environment variable names (docs/ARCHITECTURE.md, "Environment variables").
/// The CLI, bench and conformance harness use these exact spellings.
pub mod names {
    pub const HOME: &str = "FNPM_HOME";
    pub const STORE_DIR: &str = "FNPM_STORE_DIR";
    pub const CACHE_DIR: &str = "FNPM_CACHE_DIR";
    pub const REGISTRY: &str = "FNPM_REGISTRY";
    pub const PROGRESS: &str = "FNPM_PROGRESS";
    pub const VERBOSE: &str = "FNPM_VERBOSE";
}

/// An immutable copy of the environment, so config loading is deterministic and testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    vars: HashMap<String, String>,
}

impl Env {
    pub fn from_process() -> Self {
        Self { vars: std::env::vars().collect() }
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self { vars: pairs.into_iter().map(|(k, v)| (k.into(), v.into())).collect() }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// The first non-empty (after trimming) value among `keys`; later keys are not consulted
    /// once one is set, matching bun's registry env lookup.
    pub fn first(&self, keys: &[&str]) -> Option<&str> {
        keys.iter().filter_map(|k| self.get(k)).map(str::trim).find(|v| !v.is_empty())
    }

    /// `FNPM_CONFIG_<KEY>`, then `BUN_CONFIG_<KEY>`, then `NPM_CONFIG_<key>` and
    /// `npm_config_<key>` (npm style, `-` as `_`). `key` is given in npm form, e.g.
    /// `"registry"`, `"ignore-scripts"`.
    pub fn config_var(&self, key: &str) -> Option<&str> {
        let snake = key.replace('-', "_");
        let upper = snake.to_ascii_uppercase();
        self.first(&[
            &format!("FNPM_CONFIG_{upper}"),
            &format!("BUN_CONFIG_{upper}"),
            &format!("NPM_CONFIG_{upper}"),
            &format!("npm_config_{snake}"),
        ])
    }

    /// The default registry from the environment: `FNPM_REGISTRY`, then
    /// [`config_var`](Self::config_var)`("registry")`.
    pub fn registry(&self) -> Option<&str> {
        self.first(&[names::REGISTRY]).or_else(|| self.config_var("registry"))
    }

    /// bun's `string_is_truthy`: `1`, `true`, `yes`, `on` (case-insensitive).
    pub fn truthy(&self, key: &str) -> Option<bool> {
        self.get(key).map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
    }

    /// `CI`, `TDDIUM`, `GITHUB_ACTIONS`, `JENKINS_URL`, `bamboo.buildKey`.
    pub fn is_ci(&self) -> bool {
        ["CI", "TDDIUM", "GITHUB_ACTIONS", "JENKINS_URL", "bamboo.buildKey"]
            .iter()
            .any(|k| self.get(k).is_some_and(|v| !v.is_empty() && v != "false" && v != "0"))
    }

    /// `${VAR}` / `${VAR?}` expansion used by `.npmrc`.
    pub fn expand(&self, _value: &str) -> String {
        todo!("port src/ini/lib.rs env expansion")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_order() {
        let env = Env::from_pairs([("NPM_CONFIG_REGISTRY", "https://a/"), ("BUN_CONFIG_REGISTRY", " ")]);
        assert_eq!(env.config_var("registry"), Some("https://a/"));
        assert_eq!(env.registry(), Some("https://a/"));
        let env = Env::from_pairs([("FNPM_REGISTRY", "https://f/"), ("FNPM_CONFIG_REGISTRY", "https://c/")]);
        assert_eq!(env.registry(), Some("https://f/"));
        let env = Env::from_pairs([("FNPM_CONFIG_IGNORE_SCRIPTS", "true"), ("npm_config_ignore_scripts", "false")]);
        assert_eq!(env.config_var("ignore-scripts"), Some("true"));
        assert_eq!(Env::from_pairs([("X", "Yes")]).truthy("X"), Some(true));
        assert!(Env::from_pairs([("CI", "1")]).is_ci());
        assert!(!Env::default().is_ci());
    }
}
