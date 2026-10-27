//! The command tables compiled from `cli-spec.json` (see `build.rs`).
//!
//! Only what the launcher needs is materialised: enough of each bin's root to find the
//! command a token names (and step over flag values on the way), and full flag tables for
//! the natively handled leaves. Everything else is "not native", which is all the launcher
//! has to know about it.

/// One flag, as declared in `packages/fuck-npm/src/cli/trees/flags.ts`.
#[derive(Debug, PartialEq, Eq)]
pub struct FlagDef {
    /// camelCase of the long name: the key the parsed value is stored under.
    pub key: &'static str,
    /// Long name without dashes (`network-concurrency`, `no-save`).
    pub long: &'static str,
    /// Single-letter alias, if any.
    pub short: Option<char>,
    /// Takes a value (`[placeholder]` in the TS spelling).
    pub takes_value: bool,
    /// bun's `<STR>?`: the value only attaches with `=`.
    pub optional_value: bool,
    /// Occurrences collect into a list.
    pub repeatable: bool,
}

/// A positional argument of a native leaf.
#[derive(Debug, PartialEq, Eq)]
pub struct PositionalDef {
    pub name: &'static str,
    pub required: bool,
    pub description: &'static str,
}

/// Which foreign-flag table a leaf applies (`CommandLeaf.foreignFlags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignMode {
    Install,
    /// The install table minus the aliases that would select update groups.
    Update,
}

/// A natively handled command. The lookups are generated `match`es.
pub struct LeafDef {
    /// Canonical name (`install`, never `i`).
    pub name: &'static str,
    pub positionals: &'static [PositionalDef],
    pub foreign: Option<ForeignMode>,
    pub ignores_runtime_flags: bool,
    /// Long name to flag over the leaf's flags and the globals, first spelling wins.
    pub long: fn(&str) -> Option<&'static FlagDef>,
    /// Short letter to flag, first spelling wins.
    pub short: fn(char) -> Option<&'static FlagDef>,
    /// `allowedFlagNames`: every spelling accepted on the command line.
    pub allowed: fn(&str) -> bool,
    /// Long names of the leaf's flags plus the globals (`ownFlags` in foreign-flags.ts).
    pub own: fn(&str) -> bool,
    /// The boolean subset of `own`.
    pub own_boolean: fn(&str) -> bool,
    /// The leaf's foreign-flag rewrite table; `Some("")` drops the token.
    pub rewrite: fn(&str) -> Option<&'static str>,
}

impl std::fmt::Debug for LeafDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeafDef")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// What a root-level command token names.
#[derive(Debug, Clone, Copy)]
pub enum RootChild {
    /// A command the launcher runs itself.
    Native(&'static LeafDef),
    /// Any other command: handed to the TS CLI.
    Other,
}

/// One bin's root.
pub struct BinDef {
    pub name: &'static str,
    /// Long-name lookup over every flag of the tree plus the globals (`flagSpecsFor(root)`).
    pub root_long: fn(&str) -> Option<&'static FlagDef>,
    /// Short-letter lookup over the same list.
    pub root_short: fn(char) -> Option<&'static FlagDef>,
    /// The root child a segment names (by name or alias).
    pub root_child: fn(&str) -> Option<RootChild>,
    /// Whether a segment is a top-level shorthand (`fnpm whoami`).
    pub root_alias: fn(&str) -> bool,
    /// `flagOwners`: the commands (by preferred spelling) that declare a flag name.
    pub flag_owners: fn(&str) -> &'static [&'static str],
}

impl std::fmt::Debug for BinDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinDef")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

include!(concat!(env!("OUT_DIR"), "/spec.rs"));

/// The short git revision of the build, `""` when unknown.
pub const REVISION: &str = env!("FNPM_REVISION");

/// The version headers print, like bun's `version_with_sha`: `0.1.0 (abc1234)`.
pub fn version_with_sha() -> String {
    if REVISION.is_empty() {
        VERSION.to_owned()
    } else {
        format!("{VERSION} ({REVISION})")
    }
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn crates_match_the_npm_package() {
        // `npm_config_user_agent` and the core report the crate version; the CLI reports
        // packages/fuck-npm/package.json. Bump Cargo.toml's workspace version with it.
        assert_eq!(super::VERSION, env!("CARGO_PKG_VERSION"));
    }
}
