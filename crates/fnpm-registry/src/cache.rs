//! On-disk manifest cache: `<cache_dir>/metadata/v1/<xxh3-64 hex of registry url + name>.rkyv`.
//!
//! The file is an rkyv archive of a flat record (strings + offsets), so loading is a `mmap`/read
//! plus validation, with no JSON parsing. Writes go to a temp file and are renamed into place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{PackageManifest, Result};

pub struct ManifestCache {
    #[allow(dead_code)]
    dir: PathBuf,
}

impl ManifestCache {
    pub fn open(dir: &Path) -> Result<Self> {
        Ok(Self { dir: dir.to_path_buf() })
    }

    pub fn path_for(&self, _registry_url: &str, _name: &str) -> PathBuf {
        todo!()
    }

    pub fn load(&self, _registry_url: &str, _name: &str) -> Option<Arc<PackageManifest>> {
        todo!()
    }

    /// Off the hot path: called from a blocking task.
    pub fn store(&self, _registry_url: &str, _manifest: &PackageManifest) -> Result<()> {
        todo!()
    }

    /// `fnpm cache clean`: removes every entry; returns bytes freed.
    pub fn clear(&self) -> Result<u64> {
        todo!()
    }
}
