//! Filesystem materialisation primitives shared by the store and the linker:
//! clonefile (macOS `clonefileat`, Linux `FICLONE`), hardlink (`linkat`), copy
//! (`copy_file_range` / `fcopyfile`), symlinks and junctions, with a sticky fallback.

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

pub use fnpm_config::Backend;

#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    #[error("{op} {from} -> {to}: {source}")]
    Io {
        op: &'static str,
        from: String,
        to: String,
        #[source]
        source: std::io::Error,
    },
}

/// The best backend that worked so far in this process. Once a clone fails with
/// `EXDEV`/`ENOTSUP`, every later file uses the fallback without retrying (bun's sticky
/// fallback).
#[derive(Debug)]
pub struct StickyBackend(AtomicU8);

impl StickyBackend {
    pub fn new(preferred: Backend) -> Self {
        Self(AtomicU8::new(preferred as u8))
    }

    pub fn current(&self) -> Backend {
        match self.0.load(Ordering::Relaxed) {
            0 => Backend::Clonefile,
            1 => Backend::Hardlink,
            2 => Backend::Copyfile,
            _ => Backend::Symlink,
        }
    }

    pub fn degrade(&self, to: Backend) {
        self.0.fetch_max(to as u8, Ordering::Relaxed);
    }
}

/// The default for this OS: clonefile on macOS, hardlink elsewhere.
pub fn default_backend() -> Backend {
    if cfg!(target_os = "macos") { Backend::Clonefile } else { Backend::Hardlink }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeStats {
    pub files: u64,
    pub cloned: u64,
    pub hardlinked: u64,
    pub copied: u64,
}

/// Places one file. `dst`'s parent must exist. Returns the backend that was used.
pub fn place_file(_src: &Path, _dst: &Path, _sticky: &StickyBackend) -> Result<Backend, MaterializeError> {
    todo!()
}

/// Clones a whole directory in one syscall where the OS allows it (macOS `clonefile` on a
/// directory); `Ok(false)` when not supported so the caller falls back to per-file.
pub fn clone_dir(_src: &Path, _dst: &Path) -> Result<bool, MaterializeError> {
    todo!()
}

/// Relative symlink on Unix; junction for directories on Windows (no admin rights needed).
pub fn symlink_dir(_target: &Path, _link: &Path) -> Result<(), MaterializeError> {
    todo!()
}

/// Write to `<path>.tmp-<rand>` then rename; the standard atomic write.
pub fn write_atomic(_path: &Path, _bytes: &[u8]) -> std::io::Result<()> {
    todo!()
}
