//! Per-package metadata kept in the lock graph so that installs need no manifest fetches.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Which lifecycle scripts a package declares (bun's `Scripts::NAMES` order).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScriptFlags(pub u8);

impl ScriptFlags {
    pub const PREINSTALL: Self = Self(1 << 0);
    pub const INSTALL: Self = Self(1 << 1);
    pub const POSTINSTALL: Self = Self(1 << 2);
    pub const PREPREPARE: Self = Self(1 << 3);
    pub const PREPARE: Self = Self(1 << 4);
    pub const POSTPREPARE: Self = Self(1 << 5);
    /// No `install`/`preinstall` script but a `binding.gyp`: runs `node-gyp rebuild`.
    pub const BINDING_GYP: Self = Self(1 << 6);

    /// In run order.
    pub const NAMES: [(&'static str, ScriptFlags); 6] = [
        ("preinstall", Self::PREINSTALL),
        ("install", Self::INSTALL),
        ("postinstall", Self::POSTINSTALL),
        ("preprepare", Self::PREPREPARE),
        ("prepare", Self::PREPARE),
        ("postprepare", Self::POSTPREPARE),
    ];

    pub const INSTALL_SCRIPTS: Self =
        Self(Self::PREINSTALL.0 | Self::INSTALL.0 | Self::POSTINSTALL.0 | Self::BINDING_GYP.0);

    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut f = Self::default();
        for n in names {
            if let Some((_, flag)) = Self::NAMES.iter().find(|(k, _)| *k == n) {
                f.0 |= flag.0;
            }
        }
        f
    }

    #[inline]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// npm's `hasInstallScript`: any of preinstall / install / postinstall / binding.gyp.
    pub fn has_install_script(self) -> bool {
        self.0 & Self::INSTALL_SCRIPTS.0 != 0
    }

    pub fn names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMES
            .into_iter()
            .filter(move |(_, f)| self.contains(*f))
            .map(|(n, _)| n)
    }
}

impl std::fmt::Debug for ScriptFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut l = f.debug_list();
        l.entries(self.names());
        if self.contains(Self::BINDING_GYP) {
            l.entry(&"binding.gyp");
        }
        l.finish()
    }
}

/// `bundleDependencies` / `bundledDependencies`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BundledDependencies {
    #[default]
    None,
    /// `true`: every dependency is bundled.
    All(bool),
    List(Vec<String>),
}

impl BundledDependencies {
    pub fn contains(&self, name: &str) -> bool {
        match self {
            Self::None => false,
            Self::All(b) => *b,
            Self::List(l) => l.iter().any(|n| n == name),
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Self::None | Self::All(false))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageMeta {
    #[serde(default, skip_serializing_if = "ScriptFlags::is_empty")]
    pub scripts: ScriptFlags,
    /// From the manifest's `hasInstallScript` or derived from `scripts`. Registry manifests
    /// only carry this boolean, so it is kept separately from [`PackageMeta::scripts`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_install_script: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
    /// `engines` as written (`node`, `npm`, ...). Not enforced, like bun.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub engines: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BundledDependencies::is_none")]
    pub bundled_dependencies: BundledDependencies,
    /// This package is itself bundled inside its parent's tarball and needs no fetch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bundled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// `directories.man` or `man` entries, for `fnpm -g` man page links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub man: Vec<String>,
}

impl PackageMeta {
    pub fn needs_scripts(&self) -> bool {
        self.has_install_script || self.scripts.has_install_script()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_flags() {
        let f = ScriptFlags::from_names(["postinstall", "prepare", "test"]);
        assert!(f.has_install_script());
        assert_eq!(f.names().collect::<Vec<_>>(), ["postinstall", "prepare"]);
        assert!(!ScriptFlags::from_names(["prepare"]).has_install_script());
        assert!(ScriptFlags::BINDING_GYP.has_install_script());
    }

    #[test]
    fn bundled() {
        let b: BundledDependencies = serde_json::from_str(r#"["a"]"#).unwrap();
        assert!(b.contains("a") && !b.contains("b"));
        let b: BundledDependencies = serde_json::from_str("true").unwrap();
        assert!(b.contains("x"));
        let m = PackageMeta::default();
        assert_eq!(serde_json::to_string(&m).unwrap(), "{}");
        assert!(!m.needs_scripts());
    }
}
