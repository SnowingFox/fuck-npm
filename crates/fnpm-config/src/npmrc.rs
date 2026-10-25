//! `.npmrc` (npm `ini` semantics, bun `src/ini/lib.rs`).

use std::path::Path;

use crate::{Env, Result};

/// One parsed `.npmrc`: flat `key = value` pairs in file order (later wins), env-expanded.
/// Sections (`[foo]`) prefix keys with `foo.` like npm's ini.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Npmrc {
    pub entries: Vec<(String, String)>,
}

impl Npmrc {
    /// Parses text. Values: quoted strings unescaped, `key[] = v` appends, `true`/`false`
    /// stay strings (callers interpret), `${VAR}` expanded from `env`.
    pub fn parse(_text: &str, _env: &Env) -> Result<Self> {
        todo!("port src/ini/lib.rs")
    }

    pub fn load(_path: &Path, _env: &Env) -> Result<Option<Self>> {
        todo!()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// All values of `key[]` (or repeated `key`).
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries.iter().filter(move |(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}
