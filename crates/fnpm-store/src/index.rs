//! `index.bin`: the precomputed file list of a package, so linking needs no `readdir`.
//!
//! Format: 8 bytes magic (`FNPMIDX` + format version), the xxh3-64 of the payload (little
//! endian), then the rkyv payload. A file that fails the checksum or validation is treated
//! as absent by the store, so a damaged entry is rebuilt instead of trusted.

use std::io;
use std::path::Path;

use fnpm_types::{HashAlgorithm, Integrity};
use rkyv::{Archive, Deserialize, Serialize};

use crate::{Error, Result};

const MAGIC: &[u8; 8] = b"FNPMIDX\x01";
const HEADER_LEN: usize = 16;

/// One file of a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    /// `/`-separated, relative to the package root.
    pub path: String,
    /// Bytes.
    pub size: u64,
    /// 0o755 or 0o644 (tar modes are normalised like npm).
    pub mode: u32,
    /// BLAKE3 (64-byte output) of the contents; names the CAS blob.
    pub digest: [u8; 64],
}

impl IndexedFile {
    /// Whether the file is executable (its CAS blob carries the `-x` suffix).
    pub fn is_executable(&self) -> bool {
        self.mode & 0o111 != 0
    }
}

/// Directories first (parents before children), then files, both sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileIndex {
    /// Every directory, sorted (parents first).
    pub dirs: Vec<String>,
    /// Every file, sorted by path.
    pub files: Vec<IndexedFile>,
    /// The package.json contents, so the linker and scripts need not read it from disk.
    pub package_json: Option<String>,
}

/// What `index.bin` records about the package besides its files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexMeta {
    /// The integrity the package was imported under (the full value; the directory name only
    /// carries 16 hex digits of it).
    pub integrity: Option<Integrity>,
    /// For a patched copy, the hash of the patch file ([`crate::patch::hash_patch`]).
    pub patch_hash: Option<u64>,
}

#[derive(Archive, Serialize, Deserialize)]
struct FileRecord {
    path: String,
    size: u64,
    mode: u32,
    digest: [u8; 64],
}

#[derive(Archive, Serialize, Deserialize)]
struct IndexRecord {
    integrity_algorithm: u8,
    integrity: Vec<u8>,
    patch_hash: Option<u64>,
    dirs: Vec<String>,
    files: Vec<FileRecord>,
    package_json: Option<String>,
}

fn algorithm_code(a: HashAlgorithm) -> u8 {
    match a {
        HashAlgorithm::Sha1 => 1,
        HashAlgorithm::Sha256 => 2,
        HashAlgorithm::Sha384 => 3,
        HashAlgorithm::Sha512 => 4,
    }
}

fn algorithm_from_code(c: u8) -> Option<HashAlgorithm> {
    Some(match c {
        1 => HashAlgorithm::Sha1,
        2 => HashAlgorithm::Sha256,
        3 => HashAlgorithm::Sha384,
        4 => HashAlgorithm::Sha512,
        _ => return None,
    })
}

fn corrupt(path: &Path, why: &str) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, why.to_owned()),
    }
}

impl FileIndex {
    /// Reads `index.bin`.
    pub fn read(path: &Path) -> Result<Self> {
        Ok(Self::read_with_meta(path)?.0)
    }

    /// Writes `index.bin` (temporary file + rename).
    pub fn write(&self, path: &Path) -> Result<()> {
        self.write_with_meta(path, &IndexMeta::default())
    }

    /// Reads `index.bin` and what it records about the package.
    pub fn read_with_meta(path: &Path) -> Result<(Self, IndexMeta)> {
        let bytes = std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_bytes(&bytes).ok_or_else(|| corrupt(path, "damaged index.bin"))
    }

    /// Writes `index.bin` through a temporary file and a rename.
    pub fn write_with_meta(&self, path: &Path, meta: &IndexMeta) -> Result<()> {
        let bytes = self.to_bytes(meta);
        crate::fsops::write_atomic(path, &bytes).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The serialised form.
    pub fn to_bytes(&self, meta: &IndexMeta) -> Vec<u8> {
        let record = IndexRecord {
            integrity_algorithm: meta.integrity.map_or(0, |i| algorithm_code(i.algorithm())),
            integrity: meta
                .integrity
                .map(|i| i.digest().to_vec())
                .unwrap_or_default(),
            patch_hash: meta.patch_hash,
            dirs: self.dirs.clone(),
            files: self
                .files
                .iter()
                .map(|f| FileRecord {
                    path: f.path.clone(),
                    size: f.size,
                    mode: f.mode,
                    digest: f.digest,
                })
                .collect(),
            package_json: self.package_json.clone(),
        };
        let payload = rkyv::to_bytes::<rkyv::rancor::Error>(&record)
            .expect("serialising plain data cannot fail");
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(&payload).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Parses the serialised form; `None` if it is damaged or from another format version.
    pub fn from_bytes(bytes: &[u8]) -> Option<(Self, IndexMeta)> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return None;
        }
        let sum = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let payload = &bytes[HEADER_LEN..];
        if xxhash_rust::xxh3::xxh3_64(payload) != sum {
            return None;
        }
        let mut aligned = rkyv::util::AlignedVec::<16>::with_capacity(payload.len());
        aligned.extend_from_slice(payload);
        let record = rkyv::from_bytes::<IndexRecord, rkyv::rancor::Error>(&aligned).ok()?;
        let integrity = match algorithm_from_code(record.integrity_algorithm) {
            Some(a) => Some(Integrity::from_digest(a, &record.integrity).ok()?),
            None if record.integrity_algorithm == 0 => None,
            None => return None,
        };
        let index = FileIndex {
            dirs: record.dirs,
            files: record
                .files
                .into_iter()
                .map(|f| IndexedFile {
                    path: f.path,
                    size: f.size,
                    mode: f.mode,
                    digest: f.digest,
                })
                .collect(),
            package_json: record.package_json,
        };
        Some((
            index,
            IndexMeta {
                integrity,
                patch_hash: record.patch_hash,
            },
        ))
    }

    /// Sum of the file sizes.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// The file at `path` (binary search; `files` is sorted).
    pub fn file(&self, path: &str) -> Option<&IndexedFile> {
        self.files
            .binary_search_by(|f| f.path.as_str().cmp(path))
            .ok()
            .map(|i| &self.files[i])
    }

    /// Builds an index from files in any order: sorted files, and every parent directory of
    /// them sorted so that parents come before children.
    pub fn from_files(mut files: Vec<IndexedFile>, package_json: Option<String>) -> Self {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut dirs: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for f in &files {
            let mut end = 0;
            while let Some(i) = f.path[end..].find('/') {
                end += i;
                let dir = &f.path[..end];
                if seen.insert(dir.to_owned()) {
                    dirs.push(dir.to_owned());
                }
                end += 1;
            }
        }
        dirs.sort();
        FileIndex {
            dirs,
            files,
            package_json,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FileIndex {
        FileIndex::from_files(
            vec![
                IndexedFile {
                    path: "lib/b/c.js".into(),
                    size: 3,
                    mode: 0o644,
                    digest: [3; 64],
                },
                IndexedFile {
                    path: "package.json".into(),
                    size: 2,
                    mode: 0o644,
                    digest: [1; 64],
                },
                IndexedFile {
                    path: "bin/x".into(),
                    size: 5,
                    mode: 0o755,
                    digest: [2; 64],
                },
            ],
            Some("{}".into()),
        )
    }

    #[test]
    fn from_files_orders_dirs_and_files() {
        let idx = sample();
        assert_eq!(idx.dirs, vec!["bin", "lib", "lib/b"]);
        let paths: Vec<_> = idx.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["bin/x", "lib/b/c.js", "package.json"]);
        assert_eq!(idx.total_size(), 10);
        assert!(idx.file("bin/x").unwrap().is_executable());
        assert!(idx.file("nope").is_none());
    }

    #[test]
    fn roundtrip_with_meta() {
        let idx = sample();
        let integrity = Integrity::compute(HashAlgorithm::Sha512, b"tgz");
        let meta = IndexMeta {
            integrity: Some(integrity),
            patch_hash: Some(42),
        };
        let bytes = idx.to_bytes(&meta);
        assert_eq!(FileIndex::from_bytes(&bytes), Some((idx.clone(), meta)));
        let plain = idx.to_bytes(&IndexMeta::default());
        assert_eq!(
            FileIndex::from_bytes(&plain),
            Some((idx, IndexMeta::default()))
        );
    }

    #[test]
    fn damage_is_detected() {
        let bytes = sample().to_bytes(&IndexMeta::default());
        for i in [0, 9, HEADER_LEN + 3, bytes.len() - 1] {
            let mut bad = bytes.clone();
            bad[i] ^= 0x40;
            assert!(FileIndex::from_bytes(&bad).is_none(), "byte {i}");
        }
        assert!(FileIndex::from_bytes(&bytes[..bytes.len() - 1]).is_none());
        assert!(FileIndex::from_bytes(b"").is_none());
    }

    #[test]
    fn file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.bin");
        sample().write(&p).unwrap();
        assert_eq!(FileIndex::read(&p).unwrap(), sample());
        std::fs::write(&p, b"garbage").unwrap();
        assert!(FileIndex::read(&p).is_err());
    }
}
