//! `os` / `cpu` / `libc` constraints with `!negation` (bun's `Negatable`, `resolver_hooks.rs`).

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    #[error("invalid {what}: '{value}'. valid values: *, any, none, {valid}; use !name to negate")]
    Unknown { what: &'static str, value: String, valid: String },
}

/// Accumulates `name` / `!name` / `any` / `none` entries, then [`Negatable::combine`]s them.
#[derive(Debug, Clone, Copy, Default)]
struct Negatable {
    added: u16,
    removed: u16,
    had_wildcard: bool,
    had_unrecognized: bool,
}

impl Negatable {
    fn apply(&mut self, s: &str, lookup: impl Fn(&str) -> Option<u16>) {
        if s.is_empty() {
            return;
        }
        if s == "any" {
            self.had_wildcard = true;
            return;
        }
        if s == "none" {
            self.had_unrecognized = true;
            return;
        }
        let (is_not, name) = match s.strip_prefix('!') {
            Some(n) => (true, n),
            None => (false, s),
        };
        let Some(bit) = lookup(name) else {
            if !is_not {
                self.had_unrecognized = true;
            }
            return;
        };
        // bun resets the wildcard / unrecognized flags on every recognised entry.
        *self = if is_not {
            Self { added: self.added, removed: self.removed | bit, ..Default::default() }
        } else {
            Self { added: self.added | bit, removed: self.removed, ..Default::default() }
        };
    }

    fn combine(self, none: u16, all: u16) -> u16 {
        let added = if self.had_wildcard { all } else { self.added };
        let removed = self.removed;
        if added == 0 && removed == 0 {
            return if self.had_unrecognized { none } else { all };
        }
        if added == 0 {
            return all & !removed;
        }
        if removed == 0 {
            return added;
        }
        added & !removed
    }
}

macro_rules! platform_set {
    (
        $(#[$doc:meta])* $set:ident, $what:literal, $int:ty,
        [ $( $name:literal => $bit:expr ),+ $(,)? ]
    ) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $set(pub $int);

        impl $set {
            /// Names in bun's name-map order, which is the order lockfiles print them in.
            pub const NAMES: &'static [(&'static str, $int)] = &[ $( ($name, $bit) ),+ ];
            pub const NONE: Self = Self(0);
            pub const ALL: Self = Self(0 $( | $bit )+);

            pub fn lookup(name: &str) -> Option<$int> {
                Self::NAMES.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
            }

            /// package.json semantics: absent or `[]` is everything; unknown positive names
            /// make it match nothing.
            pub fn from_list<S: AsRef<str>>(items: &[S]) -> Self {
                let mut n = Negatable::default();
                for i in items {
                    n.apply(i.as_ref(), |s| Self::lookup(s).map(u16::from));
                }
                Self(n.combine(0, u16::from(Self::ALL.0)) as $int)
            }

            /// CLI semantics (`--os`, `--cpu`, `--libc`): also accepts `*`; unknown names fail.
            pub fn parse_cli<S: AsRef<str>>(items: &[S]) -> Result<Self, PlatformError> {
                let mut normalized = Vec::with_capacity(items.len());
                for i in items {
                    let s = i.as_ref();
                    let s = if s == "*" { "any" } else { s };
                    let name = s.strip_prefix('!').unwrap_or(s);
                    if !matches!(s, "any" | "none") && Self::lookup(name).is_none() {
                        return Err(PlatformError::Unknown {
                            what: $what,
                            value: s.to_owned(),
                            valid: Self::NAMES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "),
                        });
                    }
                    normalized.push(s);
                }
                Ok(Self::from_list(&normalized))
            }

            #[inline]
            pub fn contains(self, bit: $int) -> bool {
                self.0 & bit != 0
            }

            pub fn names(self) -> impl Iterator<Item = &'static str> {
                Self::NAMES.iter().filter(move |(_, b)| self.0 & *b != 0).map(|(n, _)| *n)
            }

            /// bun's `Negatable::to_json` choice: `["none"]` for nothing, `[]` for everything,
            /// else whichever of the included or `!excluded` lists is shorter (included on a
            /// tie; bun prints the negated list on a tie).
            pub fn to_list(self) -> Vec<String> {
                if self == Self::NONE {
                    return vec!["none".to_owned()];
                }
                let removed = Self::NAMES.iter().filter(|(_, b)| self.0 & *b == 0).count();
                let included = Self::NAMES.len() - removed;
                if removed >= included {
                    self.names().map(str::to_owned).collect()
                } else {
                    Self::NAMES
                        .iter()
                        .filter(|(_, b)| self.0 & *b == 0)
                        .map(|(n, _)| format!("!{n}"))
                        .collect()
                }
            }
        }

        impl std::fmt::Debug for $set {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_list().entries(self.names()).finish()
            }
        }

        impl Serialize for $set {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                self.to_list().serialize(s)
            }
        }

        impl<'de> Deserialize<'de> for $set {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                #[serde(untagged)]
                enum OneOrMany {
                    One(String),
                    Many(Vec<String>),
                }
                Ok(match OneOrMany::deserialize(d)? {
                    OneOrMany::One(s) => Self::from_list(&[s]),
                    OneOrMany::Many(v) => Self::from_list(&v),
                })
            }
        }
    };
}

platform_set! {
    /// A set of operating systems (`process.platform` names).
    OsSet, "os", u16, [
        "aix" => 1 << 1, "darwin" => 1 << 2, "freebsd" => 1 << 3, "linux" => 1 << 4,
        "openbsd" => 1 << 5, "sunos" => 1 << 6, "win32" => 1 << 7, "android" => 1 << 8,
    ]
}

platform_set! {
    /// A set of CPU architectures (`process.arch` names).
    CpuSet, "cpu", u16, [
        "arm" => 1 << 1, "arm64" => 1 << 2, "ia32" => 1 << 3, "mips" => 1 << 4,
        "mipsel" => 1 << 5, "ppc" => 1 << 6, "ppc64" => 1 << 7, "s390" => 1 << 8,
        "s390x" => 1 << 9, "x32" => 1 << 10, "x64" => 1 << 11,
    ]
}

platform_set! {
    /// A set of C libraries. Unlike os/cpu, the empty set means "not declared" and matches
    /// every target.
    LibcSet, "libc", u8, [ "glibc" => 1 << 1, "musl" => 1 << 2 ]
}

impl OsSet {
    pub fn current() -> Self {
        let name = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            "solaris" | "illumos" => "sunos",
            other => other,
        };
        Self(Self::lookup(name).unwrap_or(0))
    }
}

impl CpuSet {
    pub fn current() -> Self {
        let name = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "x86" => "ia32",
            "powerpc" => "ppc",
            "powerpc64" => "ppc64",
            other => other,
        };
        Self(Self::lookup(name).unwrap_or(0))
    }
}

impl LibcSet {
    /// glibc or musl on Linux (by build target), every libc elsewhere.
    pub fn current() -> Self {
        if cfg!(all(target_os = "linux", target_env = "musl")) {
            Self(1 << 2)
        } else if cfg!(target_os = "linux") {
            Self(1 << 1)
        } else {
            Self::ALL
        }
    }

    /// A package's declared `libc` (`[]` / absent means not declared).
    pub fn from_package_list<S: AsRef<str>>(items: &[S]) -> Self {
        if items.iter().all(|s| s.as_ref().is_empty()) {
            return Self::NONE;
        }
        Self::from_list(items)
    }
}

/// The target being installed for. Defaults to the running machine; `--os`, `--cpu` and
/// `--libc` replace parts of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Platform {
    pub os: OsSet,
    pub cpu: CpuSet,
    pub libc: LibcSet,
}

impl Platform {
    pub fn current() -> Self {
        Self { os: OsSet::current(), cpu: CpuSet::current(), libc: LibcSet::current() }
    }
}

impl Default for Platform {
    fn default() -> Self {
        Self::current()
    }
}

/// A package's `os` / `cpu` / `libc` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlatformConstraints {
    #[serde(default = "all_os", skip_serializing_if = "is_all_os")]
    pub os: OsSet,
    #[serde(default = "all_cpu", skip_serializing_if = "is_all_cpu")]
    pub cpu: CpuSet,
    #[serde(default, skip_serializing_if = "is_undeclared_libc")]
    pub libc: LibcSet,
}

fn all_os() -> OsSet {
    OsSet::ALL
}
fn all_cpu() -> CpuSet {
    CpuSet::ALL
}
fn is_all_os(s: &OsSet) -> bool {
    *s == OsSet::ALL
}
fn is_all_cpu(s: &CpuSet) -> bool {
    *s == CpuSet::ALL
}
fn is_undeclared_libc(s: &LibcSet) -> bool {
    *s == LibcSet::NONE
}

impl Default for PlatformConstraints {
    fn default() -> Self {
        Self::ANY
    }
}

impl PlatformConstraints {
    pub const ANY: Self = Self { os: OsSet::ALL, cpu: CpuSet::ALL, libc: LibcSet::NONE };

    pub fn from_package_json<S: AsRef<str>>(os: &[S], cpu: &[S], libc: &[S]) -> Self {
        Self { os: OsSet::from_list(os), cpu: CpuSet::from_list(cpu), libc: LibcSet::from_package_list(libc) }
    }

    pub fn is_any(&self) -> bool {
        *self == Self::ANY
    }

    /// bun's `Meta::is_disabled` negated.
    pub fn matches(&self, target: &Platform) -> bool {
        self.os.0 & target.os.0 != 0
            && self.cpu.0 & target.cpu.0 != 0
            && (self.libc.0 == 0 || self.libc.0 & target.libc.0 != 0)
    }

    /// `--libc` only filters optional dependencies; required ones match every libc.
    pub fn matches_for_edge(&self, target: &Platform, optional: bool) -> bool {
        let mut t = *target;
        if !optional {
            t.libc = LibcSet::ALL;
        }
        self.matches(&t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> OsSet {
        OsSet::from_list(items)
    }

    #[test]
    fn negatable_semantics() {
        assert_eq!(os(&[]), OsSet::ALL);
        assert_eq!(os(&["darwin"]).names().collect::<Vec<_>>(), ["darwin"]);
        assert_eq!(os(&["!win32"]), OsSet(OsSet::ALL.0 & !(1 << 7)));
        assert_eq!(os(&["darwin", "linux", "!linux"]).names().collect::<Vec<_>>(), ["darwin"]);
        assert_eq!(os(&["any"]), OsSet::ALL);
        assert_eq!(os(&["none"]), OsSet::NONE);
        assert_eq!(os(&["beos"]), OsSet::NONE);
        assert_eq!(os(&["!beos"]), OsSet::ALL);
        assert_eq!(os(&["any", "!linux"]), OsSet(OsSet::ALL.0 & !(1 << 4)));
    }

    #[test]
    fn to_list_choice() {
        assert_eq!(os(&["darwin"]).to_list(), ["darwin"]);
        assert_eq!(os(&["!win32"]).to_list(), ["!win32"]);
        assert!(OsSet::ALL.to_list().is_empty());
        assert_eq!(OsSet::NONE.to_list(), ["none"]);
        for set in [os(&["darwin", "linux"]), os(&["!win32", "!aix"]), OsSet::ALL, OsSet::NONE] {
            assert_eq!(OsSet::from_list(&set.to_list()), set);
        }
    }

    #[test]
    fn cli_parse() {
        assert_eq!(CpuSet::parse_cli(&["*"]).unwrap(), CpuSet::ALL);
        assert_eq!(CpuSet::parse_cli(&["arm64", "!x64"]).unwrap().names().collect::<Vec<_>>(), ["arm64"]);
        let e = CpuSet::parse_cli(&["sparc"]).unwrap_err();
        assert!(e.to_string().starts_with("invalid cpu: 'sparc'"));
    }

    #[test]
    fn matching() {
        let linux_x64_glibc = Platform { os: os(&["linux"]), cpu: CpuSet::from_list(&["x64"]), libc: LibcSet::from_list(&["glibc"]) };
        let c = PlatformConstraints::from_package_json(&["linux"], &["x64", "arm64"], &[]);
        assert!(c.matches(&linux_x64_glibc));
        let musl = PlatformConstraints::from_package_json::<&str>(&[], &[], &["musl"]);
        assert!(!musl.matches(&linux_x64_glibc));
        assert!(musl.matches_for_edge(&linux_x64_glibc, false));
        assert!(!musl.matches_for_edge(&linux_x64_glibc, true));
        let darwin = PlatformConstraints::from_package_json::<&str>(&["darwin"], &[], &[]);
        assert!(!darwin.matches(&linux_x64_glibc));
        assert!(PlatformConstraints::ANY.matches(&Platform::current()));
        assert_ne!(OsSet::current(), OsSet::NONE);
    }

    #[test]
    fn serde() {
        let c = PlatformConstraints::from_package_json(&["!win32"], &[], &["glibc"]);
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, r#"{"os":["!win32"],"libc":["glibc"]}"#);
        assert_eq!(serde_json::from_str::<PlatformConstraints>(&j).unwrap(), c);
        assert_eq!(serde_json::from_str::<PlatformConstraints>("{}").unwrap(), PlatformConstraints::ANY);
        let one: OsSet = serde_json::from_str("\"darwin\"").unwrap();
        assert_eq!(one.names().collect::<Vec<_>>(), ["darwin"]);
    }
}
