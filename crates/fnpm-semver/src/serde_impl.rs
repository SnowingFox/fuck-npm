//! Versions and ranges serialize as their string forms.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Range, Version};

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    /// Accepts what [`Version::parse_loose`] accepts.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Version;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a semver version string")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Version, E> {
                Version::parse_loose(s).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(V)
    }
}

impl Serialize for Range {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Range {
    /// Accepts what [`Range::parse`] accepts.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Range;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a semver range string")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Range, E> {
                Range::parse(s).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(V)
    }
}
