//! Deterministic package tarballs (bun `runtime/cli/pack_command.rs`): `package/` prefix,
//! `package.json` first, every entry a regular file with mtime 1985-10-26T08:15:00Z (npm's
//! pacote), pax-restricted ustar headers (a pax `path` record only when a name does not fit
//! the ustar name/prefix fields or is not ASCII), uid/gid 0, empty user and group names,
//! gzip without a timestamp.
//!
//! The bytes are stable for fnpm itself; they are not byte-identical to bun's (libarchive +
//! zlib-ng), so integrities of packed directories differ from bun's.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use fnpm_types::IntegrityHasher;

use crate::{Error, Result};

/// `1985-10-26T08:15:00.000Z` (pacote's `tar-create-options.js`).
pub const NPM_MTIME: u64 = 499_162_500;

/// A file below a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    /// `/`-separated, relative.
    pub path: String,
    /// Permission bits.
    pub mode: u32,
    /// Bytes.
    pub size: u64,
}

/// Every regular file below `dir`, sorted by path. Symlinks and special files are skipped.
pub fn walk_files(dir: &Path) -> Result<Vec<WalkedFile>> {
    let mut out = Vec::new();
    walk(dir, "", &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<WalkedFile>) -> Result<()> {
    let io_err = |source| Error::Io {
        path: dir.to_path_buf(),
        source,
    };
    for entry in std::fs::read_dir(dir).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(Error::Io {
                path: entry.path(),
                source: io::Error::new(io::ErrorKind::InvalidData, "file name is not UTF-8"),
            });
        };
        let rel = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let meta = std::fs::symlink_metadata(entry.path()).map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        if meta.is_dir() {
            walk(&entry.path(), &rel, out)?;
        } else if meta.is_file() {
            out.push(WalkedFile {
                path: rel,
                mode: mode_of(&meta),
                size: meta.len(),
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

/// Where the bytes of a packed file come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackSource {
    /// In memory (an edited package.json).
    Bytes(Vec<u8>),
    /// Read from disk while packing.
    File(PathBuf),
}

/// One file of a package tarball.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    /// Relative path without the `package/` prefix.
    pub path: String,
    /// Permission bits (bins get `| 0o111` from the caller, as bun does).
    pub mode: u32,
    /// The contents.
    pub source: PackSource,
}

/// A `Write` that feeds an integrity hasher.
pub struct HashWriter<'a>(pub &'a mut IntegrityHasher);

impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn octal_field(dst: &mut [u8], value: u64) {
    let width = dst.len() - 1;
    let s = format!("{value:0width$o}");
    let bytes = s.as_bytes();
    let start = bytes.len().saturating_sub(width);
    dst[..width].copy_from_slice(&bytes[start..]);
    dst[width] = 0;
}

/// ustar can hold `name` (100) + `prefix` (155) split at a `/`.
fn split_ustar(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() <= 100 {
        return Some((&[], path));
    }
    if path.len() > 256 {
        return None;
    }
    // The last `/` such that the name part fits and the prefix part fits.
    for (i, &b) in path.iter().enumerate().rev() {
        if b == b'/' {
            let (prefix, name) = (&path[..i], &path[i + 1..]);
            if name.len() > 100 {
                return None;
            }
            if prefix.len() <= 155 && !name.is_empty() {
                return Some((prefix, name));
            }
        }
    }
    None
}

fn header(name: &[u8], prefix: &[u8], mode: u32, size: u64, typeflag: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len().min(100)].copy_from_slice(&name[..name.len().min(100)]);
    octal_field(&mut h[100..108], u64::from(mode & 0o7777));
    octal_field(&mut h[108..116], 0);
    octal_field(&mut h[116..124], 0);
    octal_field(&mut h[124..136], size.min(0o777_7777_7777));
    octal_field(&mut h[136..148], NPM_MTIME);
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    octal_field(&mut h[329..337], 0);
    octal_field(&mut h[337..345], 0);
    h[345..345 + prefix.len().min(155)].copy_from_slice(&prefix[..prefix.len().min(155)]);
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    let s = format!("{sum:06o}");
    h[148..154].copy_from_slice(&s.as_bytes()[s.len() - 6..]);
    h[154] = 0;
    h[155] = b' ';
    h
}

fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let mut len = 0usize;
    loop {
        let mut rec = format!("{len} {key}=").into_bytes();
        rec.extend_from_slice(value);
        rec.push(b'\n');
        if rec.len() == len {
            return rec;
        }
        len = rec.len();
    }
}

fn pad(out: &mut dyn Write, len: u64) -> io::Result<()> {
    let n = ((512 - len % 512) % 512) as usize;
    out.write_all(&[0u8; 512][..n])
}

fn write_entry(out: &mut dyn Write, entry: &PackEntry) -> Result<()> {
    let io_err = |source| Error::Io {
        path: PathBuf::from(&entry.path),
        source,
    };
    let full = format!("package/{}", entry.path);
    let (data, size): (Option<&[u8]>, u64) = match &entry.source {
        PackSource::Bytes(b) => (Some(b), b.len() as u64),
        PackSource::File(p) => (
            None,
            std::fs::metadata(p)
                .map_err(|source| Error::Io {
                    path: p.clone(),
                    source,
                })?
                .len(),
        ),
    };
    let needs_pax_size = size > 0o777_7777_7777;
    let split = if full.is_ascii() {
        split_ustar(full.as_bytes())
    } else {
        None
    };
    if split.is_none() || needs_pax_size {
        let mut records = Vec::new();
        if split.is_none() {
            records.extend(pax_record("path", full.as_bytes()));
        }
        if needs_pax_size {
            records.extend(pax_record("size", size.to_string().as_bytes()));
        }
        let base = full.rsplit('/').next().unwrap_or(&full);
        let mut pax_name = format!("package/PaxHeader/{base}").into_bytes();
        pax_name.truncate(99);
        out.write_all(&header(&pax_name, b"", 0o644, records.len() as u64, b'x'))
            .map_err(io_err)?;
        out.write_all(&records).map_err(io_err)?;
        pad(out, records.len() as u64).map_err(io_err)?;
    }
    let (prefix, name): (&[u8], &[u8]) =
        split.unwrap_or((&[], &full.as_bytes()[..full.len().min(100)]));
    out.write_all(&header(name, prefix, entry.mode, size, b'0'))
        .map_err(io_err)?;
    match data {
        Some(bytes) => out.write_all(bytes).map_err(io_err)?,
        None => {
            let PackSource::File(p) = &entry.source else {
                unreachable!("bytes handled above")
            };
            let mut f = std::fs::File::open(p).map_err(|source| Error::Io {
                path: p.clone(),
                source,
            })?;
            let mut copied: u64 = 0;
            let mut buf = vec![0u8; 64 * 1024];
            while copied < size {
                let want = buf.len().min((size - copied) as usize);
                let n = f.read(&mut buf[..want]).map_err(|source| Error::Io {
                    path: p.clone(),
                    source,
                })?;
                if n == 0 {
                    return Err(Error::Io {
                        path: p.clone(),
                        source: io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "file changed while packing",
                        ),
                    });
                }
                out.write_all(&buf[..n]).map_err(io_err)?;
                copied += n as u64;
            }
        }
    }
    pad(out, size).map_err(io_err)?;
    Ok(())
}

/// Writes the uncompressed tarball: `package.json` first, then the other entries sorted by
/// path, then the end-of-archive blocks.
pub fn write_tar(entries: &[PackEntry], out: &mut dyn Write) -> Result<()> {
    let mut ordered: Vec<&PackEntry> = entries.iter().collect();
    ordered.sort_by(|a, b| {
        (a.path != "package.json")
            .cmp(&(b.path != "package.json"))
            .then_with(|| a.path.cmp(&b.path))
    });
    for entry in ordered {
        write_entry(out, entry)?;
    }
    out.write_all(&[0u8; 1024]).map_err(|source| Error::Io {
        path: PathBuf::from("package"),
        source,
    })
}

/// The gzipped tarball (`level` 0-9, bun's `--gzip-level`, default 9).
pub fn pack_tarball(entries: &[PackEntry], level: u32) -> Result<Vec<u8>> {
    let mut enc = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(3)
        .write(Vec::new(), flate2::Compression::new(level.min(9)));
    write_tar(entries, &mut enc)?;
    enc.finish().map_err(|source| Error::Io {
        path: PathBuf::from("package"),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar::{TarEvent, TarParser};

    fn read_back(tar: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let mut p = TarParser::new();
        let mut out: Vec<(String, u32, Vec<u8>)> = Vec::new();
        p.feed(tar, &mut |ev| {
            match ev {
                TarEvent::Entry(h) => out.push((h.path, h.mode, Vec::new())),
                TarEvent::Data(d) => out.last_mut().unwrap().2.extend_from_slice(d),
                _ => {}
            }
            Ok(())
        })
        .unwrap();
        p.finish().unwrap();
        out
    }

    fn entry(path: &str, data: &[u8], mode: u32) -> PackEntry {
        PackEntry {
            path: path.into(),
            mode,
            source: PackSource::Bytes(data.to_vec()),
        }
    }

    #[test]
    fn package_json_first_then_sorted() {
        let entries = vec![
            entry("z.js", b"z", 0o644),
            entry("package.json", b"{}", 0o644),
            entry("a/b.js", b"b", 0o755),
        ];
        let mut tar = Vec::new();
        write_tar(&entries, &mut tar).unwrap();
        let got = read_back(&tar);
        let names: Vec<&str> = got.iter().map(|g| g.0.as_str()).collect();
        assert_eq!(names, vec!["package.json", "a/b.js", "z.js"]);
        assert_eq!(got[1].1, 0o755);
        // mtime field is npm's.
        assert_eq!(&tar[136..147], format!("{NPM_MTIME:011o}").as_bytes());
    }

    #[test]
    fn deterministic_and_long_names() {
        let long = format!("{}/{}", "d".repeat(150), "f".repeat(90));
        let longer = "x".repeat(300);
        let unicode = "caf\u{e9}.js".to_owned();
        let entries = vec![
            entry(&long, b"1", 0o644),
            entry(&longer, b"2", 0o644),
            entry(&unicode, b"3", 0o644),
        ];
        let a = pack_tarball(&entries, 9).unwrap();
        let b = pack_tarball(&entries, 9).unwrap();
        assert_eq!(a, b);
        let mut tar = Vec::new();
        write_tar(&entries, &mut tar).unwrap();
        let names: Vec<String> = read_back(&tar).into_iter().map(|g| g.0).collect();
        let mut want = vec![long, longer, unicode];
        want.sort();
        assert_eq!(names, want);
    }

    #[test]
    fn walk_and_pack_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("lib/x.js"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("x.js", dir.path().join("lib/link.js")).unwrap();
        let files = walk_files(dir.path()).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["lib/x.js", "package.json"]);
        let entries: Vec<PackEntry> = files
            .iter()
            .map(|f| PackEntry {
                path: f.path.clone(),
                mode: f.mode,
                source: PackSource::File(dir.path().join(&f.path)),
            })
            .collect();
        let mut tar = Vec::new();
        write_tar(&entries, &mut tar).unwrap();
        assert_eq!(read_back(&tar).len(), 2);
    }
}
