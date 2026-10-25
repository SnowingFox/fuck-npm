//! Is the package already installed at this directory? (bun `PackageInstall.rs` `verify`.)
//!
//! Registry packages: read `package.json` and compare the top-level `name` and `version`
//! (build metadata ignored, a leading `v`/`=`/whitespace tolerated). Git, GitHub and tarball
//! packages: compare the `.fnpm-tag` the linker wrote (resolved commit, or the tarball's SRI).
//! Links and workspaces: the link resolves to the expected directory (inode compare).

use std::io::Read;
use std::path::Path;

use fnpm_types::Resolution;

/// The marker file written into git, GitHub and tarball installs (bun `.bun-tag`).
pub const TAG_FILE: &str = ".fnpm-tag";

/// What [`verify`] compares against.
pub enum Expected<'a> {
    /// `package.json` must have this name and version.
    NameVersion {
        /// The package name.
        name: &'a str,
        /// The version.
        version: &'a str,
    },
    /// `.fnpm-tag` must have exactly these bytes. `allow_empty`: a `--no-verify` install.
    Tag {
        /// The expected content.
        tag: String,
        /// An empty tag also matches.
        allow_empty: bool,
    },
    /// The path must resolve to the same inode as this directory.
    SameDir(&'a Path),
    /// The directory must merely exist.
    Exists,
}

/// The tag text for a resolution, `None` when the resolution uses no tag.
pub fn tag_for(resolution: &Resolution) -> Option<String> {
    match resolution {
        Resolution::Git {
            resolved_commit, ..
        }
        | Resolution::Github {
            resolved_commit, ..
        } => Some(resolved_commit.clone()),
        Resolution::RemoteTarball { integrity, .. }
        | Resolution::LocalTarball { integrity, .. } => {
            Some(integrity.map(|i| i.to_sri()).unwrap_or_default())
        }
        _ => None,
    }
}

/// What a correct install of `name@resolution` (installed from the store) looks like.
pub fn expected_for<'a>(
    name: &'a str,
    version: &'a str,
    resolution: &'a Resolution,
) -> Expected<'a> {
    match resolution {
        Resolution::Git { .. } | Resolution::Github { .. } => Expected::Tag {
            tag: tag_for(resolution).unwrap_or_default(),
            allow_empty: false,
        },
        Resolution::RemoteTarball { integrity, .. }
        | Resolution::LocalTarball { integrity, .. } => Expected::Tag {
            tag: tag_for(resolution).unwrap_or_default(),
            allow_empty: integrity.is_none(),
        },
        Resolution::Root | Resolution::Folder { .. } => Expected::Exists,
        _ => Expected::NameVersion { name, version },
    }
}

/// Checks the directory `dir` against `expected`. Any I/O error is "not installed".
pub fn verify(dir: &Path, expected: &Expected<'_>) -> bool {
    match expected {
        Expected::NameVersion { name, version } => verify_package_json(dir, name, version),
        Expected::Tag { tag, allow_empty } => match std::fs::read(dir.join(TAG_FILE)) {
            Ok(bytes) => bytes == tag.as_bytes() || (*allow_empty && bytes.is_empty()),
            Err(_) => false,
        },
        Expected::SameDir(target) => same_file(dir, target),
        Expected::Exists => dir.is_dir(),
    }
}

/// Whether `a` and `b` (links followed) are the same inode.
pub fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

/// Reads at most the first 64 KiB of `dir/package.json` and checks its name and version.
pub fn verify_package_json(dir: &Path, name: &str, version: &str) -> bool {
    let Ok(mut f) = std::fs::File::open(dir.join("package.json")) else {
        return false;
    };
    let mut buf = Vec::with_capacity(2048);
    if f.by_ref().take(64 * 1024).read_to_end(&mut buf).is_err() {
        return false;
    }
    let minimum = r#"{"name":"","version":""}"#.len() + name.len() + version.len();
    if buf.len() < minimum {
        return false;
    }
    match name_and_version(&buf) {
        Some((found_name, found_version)) => {
            found_name == name && version_matches(&found_version, version)
        }
        None => false,
    }
}

/// bun's version comparison: build tags ignored; then leading whitespace, `v` and `=` skipped.
pub fn version_matches(found: &str, expected: &str) -> bool {
    let f = found.rsplit_once('+').map_or(found, |(v, _)| v);
    let e = expected.rsplit_once('+').map_or(expected, |(v, _)| v);
    if f == e {
        return true;
    }
    let offset = found
        .bytes()
        .position(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C | b'v' | b'='));
    match offset {
        Some(o) => &found[o..] == expected,
        None => false,
    }
}

/// Extracts the top-level `"name"` and `"version"` strings of a JSON object, stopping as soon
/// as both are found (bun `PackageJSONVersionChecker`). `None` when either is missing or the
/// text before them is not JSON.
pub fn name_and_version(bytes: &[u8]) -> Option<(String, String)> {
    let mut p = Parser { b: bytes, i: 0 };
    if p.b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        p.i = 3;
    }
    p.ws();
    p.expect(b'{')?;
    let mut name = None;
    let mut version = None;
    loop {
        p.ws();
        if p.peek()? == b'}' {
            break;
        }
        let key = p.string()?;
        p.ws();
        p.expect(b':')?;
        p.ws();
        let is_target =
            (key == "name" && name.is_none()) || (key == "version" && version.is_none());
        if is_target && p.peek()? == b'"' {
            let v = p.string()?;
            if key == "name" {
                name = Some(v);
            } else {
                version = Some(v);
            }
            if name.is_some() && version.is_some() {
                break;
            }
        } else {
            p.skip_value(0)?;
        }
        p.ws();
        match p.next()? {
            b',' => continue,
            b'}' => break,
            _ => return None,
        }
    }
    Some((name?, version?))
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn expect(&mut self, c: u8) -> Option<()> {
        (self.next()? == c).then_some(())
    }

    fn ws(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let s = self.b.get(self.i..self.i + 4)?;
        self.i += 4;
        u32::from_str_radix(std::str::from_utf8(s).ok()?, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.next()?;
            match c {
                b'"' => break,
                b'\\' => match self.next()? {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let mut cp = self.hex4()?;
                        if (0xD800..0xDC00).contains(&cp) {
                            if self.next()? != b'\\' || self.next()? != b'u' {
                                return None;
                            }
                            let lo = self.hex4()?;
                            cp =
                                0x10000 + ((cp - 0xD800) << 10) + (lo.checked_sub(0xDC00)? & 0x3FF);
                        }
                        let ch = char::from_u32(cp)?;
                        let mut tmp = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                    }
                    _ => return None,
                },
                c => out.push(c),
            }
        }
        String::from_utf8(out).ok()
    }

    fn skip_value(&mut self, depth: u32) -> Option<()> {
        if depth > 128 {
            return None;
        }
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' => {
                self.i += 1;
                self.ws();
                if self.peek()? == b'}' {
                    self.i += 1;
                    return Some(());
                }
                loop {
                    self.ws();
                    self.string()?;
                    self.ws();
                    self.expect(b':')?;
                    self.ws();
                    self.skip_value(depth + 1)?;
                    self.ws();
                    match self.next()? {
                        b',' => {}
                        b'}' => break,
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.i += 1;
                self.ws();
                if self.peek()? == b']' {
                    self.i += 1;
                    return Some(());
                }
                loop {
                    self.ws();
                    self.skip_value(depth + 1)?;
                    self.ws();
                    match self.next()? {
                        b',' => {}
                        b']' => break,
                        _ => return None,
                    }
                }
            }
            _ => {
                let start = self.i;
                while let Some(c) = self.peek() {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.i += 1;
                }
                if self.i == start {
                    return None;
                }
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_version() {
        assert_eq!(
            name_and_version(br#"{"name":"a","version":"1.0.0"}"#),
            Some(("a".into(), "1.0.0".into()))
        );
        let j = br#"{ "description": "x", "nested": {"name": "inner", "arr": [1, {"version":"9"}, "s"]},
            "version": "2.0.0", "name": "@s/b", "trailing": true }"#;
        assert_eq!(name_and_version(j), Some(("@s/b".into(), "2.0.0".into())));
        assert_eq!(name_and_version(br#"{"name":"a"}"#), None);
        assert_eq!(name_and_version(b"not json"), None);
        assert_eq!(
            name_and_version(br#"{"name":"ab","version":"1"}"#),
            Some(("ab".into(), "1".into()))
        );
        assert_eq!(
            name_and_version(br#"{"name":1,"version":"1","name":"x"}"#),
            Some(("x".into(), "1".into()))
        );
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(br#"{"name":"a","version":"1"}"#);
        assert_eq!(name_and_version(&bom), Some(("a".into(), "1".into())));
    }

    #[test]
    fn stops_once_both_are_found() {
        // Everything after the two fields may be garbage (bun's checker stops too).
        assert_eq!(
            name_and_version(br#"{"name":"a","version":"1", garbage"#),
            Some(("a".into(), "1".into()))
        );
    }

    #[test]
    fn version_rules() {
        assert!(version_matches("1.0.0", "1.0.0"));
        assert!(version_matches("1.0.0+build", "1.0.0"));
        assert!(version_matches("1.0.0", "1.0.0+other"));
        assert!(version_matches("v1.0.0", "1.0.0"));
        assert!(version_matches(" =1.0.0", "1.0.0"));
        assert!(!version_matches("1.0.1", "1.0.0"));
        assert!(!version_matches("vv", "1.0.0"));
    }

    #[test]
    fn verifies_directories() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("a");
        std::fs::create_dir(&d).unwrap();
        assert!(!verify_package_json(&d, "a", "1.0.0"));
        std::fs::write(d.join("package.json"), r#"{"name":"a","version":"1.0.0"}"#).unwrap();
        assert!(verify_package_json(&d, "a", "1.0.0"));
        assert!(!verify_package_json(&d, "b", "1.0.0"));
        assert!(!verify_package_json(&d, "a", "1.0.1"));
        let r = Resolution::npm("1.0.0", None);
        assert!(verify(&d, &expected_for("a", "1.0.0", &r)));

        let git = Resolution::Git {
            url: "u".into(),
            committish: String::new(),
            resolved_commit: "abc".into(),
            integrity: None,
        };
        assert!(!verify(&d, &expected_for("a", "1.0.0", &git)));
        std::fs::write(d.join(TAG_FILE), "abc").unwrap();
        assert!(verify(&d, &expected_for("a", "1.0.0", &git)));

        let tgz = Resolution::RemoteTarball {
            url: "u".into(),
            integrity: None,
        };
        std::fs::write(d.join(TAG_FILE), "").unwrap();
        assert!(verify(&d, &expected_for("a", "1.0.0", &tgz)));
        let i = fnpm_types::Integrity::compute(fnpm_types::HashAlgorithm::Sha512, b"x");
        let tgz2 = Resolution::RemoteTarball {
            url: "u".into(),
            integrity: Some(i),
        };
        assert!(!verify(&d, &expected_for("a", "1.0.0", &tgz2)));
        std::fs::write(d.join(TAG_FILE), i.to_sri()).unwrap();
        assert!(verify(&d, &expected_for("a", "1.0.0", &tgz2)));

        assert!(verify(&d, &Expected::SameDir(&d)));
        assert!(!verify(&d, &Expected::SameDir(t.path())));
        assert!(verify(&d, &Expected::Exists));
        assert!(!verify(&t.path().join("zz"), &Expected::Exists));
    }
}
