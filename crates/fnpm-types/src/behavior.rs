//! Dependency kinds and edge flags (bun's `Behavior`, `resolver_hooks.rs`).

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign, Not};

use serde::{Deserialize, Serialize};

/// The flags of a dependency edge. Bit layout matches bun's so tooling ported from bun keeps
/// working: bit 0 and bit 7 are unused.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Behavior(pub u8);

impl Behavior {
    pub const NONE: Self = Self(0);
    pub const PROD: Self = Self(1 << 1);
    pub const OPTIONAL: Self = Self(1 << 2);
    pub const DEV: Self = Self(1 << 3);
    pub const PEER: Self = Self(1 << 4);
    /// The edge points at a workspace package (`workspace:` or a range a workspace satisfies).
    pub const WORKSPACE: Self = Self(1 << 5);
    /// Listed in `bundleDependencies`; not set for transitive bundled deps.
    pub const BUNDLED: Self = Self(1 << 6);

    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    #[inline]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
    #[inline]
    pub fn with(self, flag: Self, on: bool) -> Self {
        if on {
            Self(self.0 | flag.0)
        } else {
            Self(self.0 & !flag.0)
        }
    }
    #[inline]
    pub fn is_prod(self) -> bool {
        self.contains(Self::PROD)
    }
    #[inline]
    pub fn is_dev(self) -> bool {
        self.contains(Self::DEV)
    }
    #[inline]
    pub fn is_peer(self) -> bool {
        self.contains(Self::PEER)
    }
    /// An optional dependency. Optional peers are reported by [`Behavior::is_optional_peer`].
    #[inline]
    pub fn is_optional(self) -> bool {
        self.contains(Self::OPTIONAL) && !self.contains(Self::PEER)
    }
    #[inline]
    pub fn is_optional_peer(self) -> bool {
        self.contains(Self::OPTIONAL) && self.contains(Self::PEER)
    }
    #[inline]
    pub fn is_required(self) -> bool {
        !self.is_optional()
    }
    #[inline]
    pub fn is_workspace(self) -> bool {
        self.contains(Self::WORKSPACE)
    }
    #[inline]
    pub fn is_bundled(self) -> bool {
        self.contains(Self::BUNDLED)
    }

    /// The group an edge is written to in package.json and lockfiles.
    pub fn group(self) -> DependencyGroup {
        if self.is_peer() {
            DependencyGroup::Peer
        } else if self.is_dev() {
            DependencyGroup::Dev
        } else if self.contains(Self::OPTIONAL) {
            DependencyGroup::Optional
        } else {
            DependencyGroup::Prod
        }
    }
}

impl BitOr for Behavior {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for Behavior {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl BitAnd for Behavior {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
impl Not for Behavior {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0 & 0b0111_1110)
    }
}

impl fmt::Debug for Behavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_set();
        for (name, flag) in [
            ("prod", Self::PROD),
            ("optional", Self::OPTIONAL),
            ("dev", Self::DEV),
            ("peer", Self::PEER),
            ("workspace", Self::WORKSPACE),
            ("bundled", Self::BUNDLED),
        ] {
            if self.contains(flag) {
                list.entry(&name);
            }
        }
        list.finish()
    }
}

/// A package.json dependency group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DependencyGroup {
    #[serde(rename = "dependencies")]
    Prod,
    #[serde(rename = "devDependencies")]
    Dev,
    #[serde(rename = "optionalDependencies")]
    Optional,
    #[serde(rename = "peerDependencies")]
    Peer,
}

impl DependencyGroup {
    /// The order bun reads groups in; an earlier group wins for a duplicated name, except that
    /// `optionalDependencies` overrides `dependencies` like npm.
    pub const ALL: [DependencyGroup; 4] = [Self::Prod, Self::Dev, Self::Optional, Self::Peer];

    pub fn field(self) -> &'static str {
        match self {
            Self::Prod => "dependencies",
            Self::Dev => "devDependencies",
            Self::Optional => "optionalDependencies",
            Self::Peer => "peerDependencies",
        }
    }

    pub fn from_field(field: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.field() == field)
    }

    pub fn behavior(self) -> Behavior {
        match self {
            Self::Prod => Behavior::PROD,
            Self::Dev => Behavior::DEV,
            Self::Optional => Behavior::OPTIONAL,
            Self::Peer => Behavior::PEER,
        }
    }
}

/// `peerDependenciesMeta.<name>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerMeta {
    #[serde(default)]
    pub optional: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags() {
        let b = Behavior::PEER | Behavior::OPTIONAL;
        assert!(b.is_optional_peer());
        assert!(!b.is_optional());
        assert!(b.is_peer());
        assert_eq!(b.group(), DependencyGroup::Peer);
        let o = Behavior::OPTIONAL;
        assert!(o.is_optional() && !o.is_required());
        assert_eq!(o.group(), DependencyGroup::Optional);
        assert_eq!(Behavior::DEV.with(Behavior::DEV, false), Behavior::NONE);
        assert_eq!(Behavior::PROD.bits(), 2);
        assert_eq!(
            format!("{:?}", Behavior::PROD | Behavior::WORKSPACE),
            "{\"prod\", \"workspace\"}"
        );
        assert_eq!(
            DependencyGroup::from_field("devDependencies"),
            Some(DependencyGroup::Dev)
        );
        assert_eq!(
            serde_json::to_string(&DependencyGroup::Optional).unwrap(),
            "\"optionalDependencies\""
        );
    }
}
