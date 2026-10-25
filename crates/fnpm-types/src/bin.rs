//! package.json `bin` and `directories.bin` (bun's `bin.rs` `Tag`).

use serde::{Deserialize, Serialize};

use crate::name::unscoped_package_name;

/// The executables a package provides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Bin {
    /// No `bin` field.
    #[default]
    None,
    /// `"bin": "./cli.js"`. The executable is named after the unscoped package name.
    File { path: String },
    /// `"bin": { "babel": "./cli.js" }` with exactly one entry.
    NamedFile { name: String, path: String },
    /// `"bin": { ... }` with more than one entry, in package.json order.
    Map { entries: Vec<(String, String)> },
    /// `"directories": { "bin": "./bin" }`: every file in the directory.
    Dir { path: String },
}

impl Bin {
    /// Builds from a `bin` object, normalising one entry to [`Bin::NamedFile`]. Entries whose
    /// name or target is unsafe are dropped.
    pub fn from_map(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut v: Vec<(String, String)> = entries
            .into_iter()
            .filter_map(|(n, p)| {
                Some((
                    normalize_bin_name(&n)?.to_owned(),
                    normalize_bin_target(&p)?,
                ))
            })
            .collect();
        match v.len() {
            0 => Self::None,
            1 => {
                let (name, path) = v.pop().expect("one entry");
                Self::NamedFile { name, path }
            }
            _ => Self::Map { entries: v },
        }
    }

    pub fn from_file(path: &str) -> Self {
        normalize_bin_target(path).map_or(Self::None, |path| Self::File { path })
    }

    pub fn from_dir(path: &str) -> Self {
        normalize_bin_target(path).map_or(Self::None, |path| Self::Dir { path })
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// `(name, target)` pairs. [`Bin::Dir`] needs a directory listing and yields nothing here.
    pub fn entries<'a>(&'a self, package_name: &'a str) -> Vec<(&'a str, &'a str)> {
        match self {
            Self::None | Self::Dir { .. } => Vec::new(),
            Self::File { path } => normalize_bin_name(unscoped_package_name(package_name))
                .map(|n| vec![(n, path.as_str())])
                .unwrap_or_default(),
            Self::NamedFile { name, path } => vec![(name.as_str(), path.as_str())],
            Self::Map { entries } => entries
                .iter()
                .map(|(n, p)| (n.as_str(), p.as_str()))
                .collect(),
        }
    }
}

/// npm's bin name normalisation: the last path segment, and never `.`/`..`/empty or anything
/// with a separator or control character.
pub fn normalize_bin_name(name: &str) -> Option<&str> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    if base.is_empty()
        || base == "."
        || base == ".."
        || base.chars().any(|c| c.is_control() || c == ':')
    {
        return None;
    }
    Some(base)
}

/// A bin target relative to the package root, with `./` removed and `\` turned into `/`.
/// Targets that escape the package (`../x`, absolute paths) are rejected.
pub fn normalize_bin_target(target: &str) -> Option<String> {
    let t = target.replace('\\', "/");
    if t.starts_with('/') || t.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in t.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(
            Bin::from_file("./cli.js"),
            Bin::File {
                path: "cli.js".into()
            }
        );
        let one = Bin::from_map([("babel".to_owned(), "./bin/babel.js".to_owned())]);
        assert_eq!(
            one,
            Bin::NamedFile {
                name: "babel".into(),
                path: "bin/babel.js".into()
            }
        );
        let many = Bin::from_map([
            ("a".to_owned(), "a.js".to_owned()),
            ("b".to_owned(), "b.js".to_owned()),
        ]);
        assert!(matches!(many, Bin::Map { ref entries } if entries.len() == 2));
        assert_eq!(
            Bin::from_map([("x".to_owned(), "../../etc/passwd".to_owned())]),
            Bin::None
        );
        assert_eq!(Bin::from_dir("./bin"), Bin::Dir { path: "bin".into() });
    }

    #[test]
    fn entries_and_names() {
        let b = Bin::from_file("cli.js");
        assert_eq!(b.entries("@scope/tool"), vec![("tool", "cli.js")]);
        assert_eq!(normalize_bin_name("@scope/x"), Some("x"));
        assert_eq!(normalize_bin_name(".."), None);
        assert_eq!(normalize_bin_target("a/../b"), Some("b".into()));
        assert_eq!(normalize_bin_target("/abs"), None);
        assert_eq!(normalize_bin_target("C:/x"), None);
        assert_eq!(normalize_bin_target(".."), None);
    }

    #[test]
    fn serde_shape() {
        let b = Bin::NamedFile {
            name: "x".into(),
            path: "y".into(),
        };
        let j = serde_json::to_string(&b).unwrap();
        assert_eq!(j, r#"{"kind":"namedFile","name":"x","path":"y"}"#);
        assert_eq!(serde_json::from_str::<Bin>(&j).unwrap(), b);
    }
}
