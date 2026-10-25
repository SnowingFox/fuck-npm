//! `index.bin`: the precomputed file list of a package, so linking needs no `readdir`.

use std::path::Path;

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    /// `/`-separated, relative to the package root.
    pub path: String,
    pub size: u64,
    /// 0o755 or 0o644 (tar modes are normalised like npm).
    pub mode: u32,
    /// SHA-512 of the contents; names the CAS blob.
    pub digest: [u8; 64],
}

/// Directories first (parents before children), then files, both sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileIndex {
    pub dirs: Vec<String>,
    pub files: Vec<IndexedFile>,
    /// The package.json contents, so the linker and scripts need not read it from disk.
    pub package_json: Option<String>,
}

impl FileIndex {
    pub fn read(_path: &Path) -> Result<Self> {
        todo!("rkyv")
    }

    pub fn write(&self, _path: &Path) -> Result<()> {
        todo!("rkyv, temp + rename")
    }

    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}
