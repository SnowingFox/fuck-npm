//! The content-addressed blobs (`files/<2 hex>/<128 hex BLAKE3>[-x]`) and the staging directory a
//! package is assembled in.
//!
//! A file is placed in a staging package with the fewest syscalls the store allows:
//!
//! 1. `linkat(files/<blob>, staging/<path>)`: when the blob exists the file is deduplicated
//!    and no byte is written.
//! 2. Otherwise the file is written once into the staging package (`O_EXCL`) and then linked
//!    into `files/` (`linkat`; `EEXIST` means another writer stored the same content first).
//!
//! The staging file and the blob are one inode, so a blob is never visible half-written:
//! it only gets its name after the content is complete.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::index::IndexedFile;
use crate::sys::Dir;

/// The `files/` directory.
#[derive(Debug)]
pub(crate) struct Cas {
    pub(crate) dir: Dir,
}

impl Cas {
    pub(crate) fn open(files_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            dir: Dir::open(files_dir)?,
        })
    }

    /// `<2 hex>/<128 hex>[-x]`, relative to `files/`.
    pub(crate) fn blob_rel(digest: &[u8; 64], executable: bool) -> String {
        let hex = hex::encode(digest);
        let mut s = String::with_capacity(3 + 128 + 2);
        s.push_str(&hex[..2]);
        s.push('/');
        s.push_str(&hex);
        if executable {
            s.push_str("-x");
        }
        s
    }

    /// Gives `pkg/rel` the name `blob` in the store. Best effort: a store that cannot hard
    /// link simply does not deduplicate.
    fn adopt(&self, pkg: &Dir, rel: &str, blob: &str) {
        match Dir::hard_link(pkg, rel, &self.dir, blob) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let _ = self.dir.mkdir(&blob[..2], 0o755);
                let _ = Dir::hard_link(pkg, rel, &self.dir, blob);
            }
            Err(_) => {}
        }
    }
}

/// `tmp/<random>/` with the package being built in `package/`. Removed on drop unless it was
/// published.
#[derive(Debug)]
pub(crate) struct Staging {
    cas: Arc<Cas>,
    root: PathBuf,
    pkg: Dir,
    dirs: Mutex<HashSet<String>>,
    published: Mutex<bool>,
}

/// A file too large to hold in memory, written to the staging package while it is hashed.
pub(crate) struct LargeFile {
    file: File,
    hasher: crate::hash::ContentHasher,
    size: u64,
}

impl LargeFile {
    pub(crate) fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.hasher.update(data);
        self.size += data.len() as u64;
        self.file.write_all(data)
    }
}

impl Staging {
    pub(crate) fn create(tmp_dir: &Path, cas: Arc<Cas>) -> io::Result<Self> {
        let root = tmp_dir.join(crate::sys::random_name(""));
        std::fs::create_dir(&root).or_else(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                std::fs::create_dir_all(tmp_dir)?;
                std::fs::create_dir(&root)
            } else {
                Err(e)
            }
        })?;
        let pkg_path = root.join("package");
        let pkg = std::fs::create_dir(&pkg_path).and_then(|()| Dir::open(&pkg_path));
        let pkg = match pkg {
            Ok(pkg) => pkg,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&root);
                return Err(e);
            }
        };
        Ok(Self {
            cas,
            root,
            pkg,
            dirs: Mutex::new(HashSet::new()),
            published: Mutex::new(false),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn package_root(&self) -> &Path {
        self.pkg.path()
    }

    pub(crate) fn mark_published(&self) {
        *self.published.lock() = true;
    }

    /// Creates `dir` (relative) and its parents, once.
    pub(crate) fn ensure_dir(&self, dir: &str) -> io::Result<()> {
        if dir.is_empty() || self.dirs.lock().contains(dir) {
            return Ok(());
        }
        self.pkg.mkdir_all(dir)?;
        let mut dirs = self.dirs.lock();
        let mut end = 0;
        while let Some(i) = dir[end..].find('/') {
            end += i;
            dirs.insert(dir[..end].to_owned());
            end += 1;
        }
        dirs.insert(dir.to_owned());
        Ok(())
    }

    /// Creates the directories of a whole package up front (sorted, so parents come first).
    pub(crate) fn create_dirs(&self, dirs: &[String]) -> io::Result<()> {
        for d in dirs {
            match self.pkg.mkdir(d, 0o755) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => self.pkg.mkdir_all(d)?,
                Err(e) => return Err(e),
            }
        }
        self.dirs.lock().extend(dirs.iter().cloned());
        Ok(())
    }

    /// Adds a file whose bytes are in memory. Its parent directory must exist.
    pub(crate) fn add_bytes(&self, rel: &str, mode: u32, data: &[u8]) -> io::Result<IndexedFile> {
        let digest = crate::hash::content_digest(data);
        let blob = Cas::blob_rel(&digest, mode & 0o111 != 0);
        let record = IndexedFile {
            path: rel.to_owned(),
            size: data.len() as u64,
            mode,
            digest,
        };
        for _ in 0..3 {
            match Dir::hard_link(&self.cas.dir, &blob, &self.pkg, rel) {
                Ok(()) => return Ok(record),
                // A duplicate tar entry: the later one wins.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self.pkg.unlink(rel)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                // Too many links to the blob, or a filesystem without hard links: keep a copy.
                Err(_) => break,
            }
        }
        let mut file = match self.pkg.create_new(rel, mode) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.pkg.unlink(rel)?;
                self.pkg.create_new(rel, mode)?
            }
            r => r?,
        };
        file.write_all(data)?;
        drop(file);
        self.cas.adopt(&self.pkg, rel, &blob);
        Ok(record)
    }

    /// Starts a file that is streamed to disk. Its parent directory must exist.
    pub(crate) fn begin_large(&self, rel: &str, mode: u32) -> io::Result<LargeFile> {
        let file = match self.pkg.create_new(rel, mode) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.pkg.unlink(rel)?;
                self.pkg.create_new(rel, mode)?
            }
            r => r?,
        };
        Ok(LargeFile {
            file,
            hasher: crate::hash::ContentHasher::new(),
            size: 0,
        })
    }

    /// Completes a streamed file: it becomes the blob, or is replaced by the blob that holds
    /// the same content already.
    pub(crate) fn finish_large(
        &self,
        rel: &str,
        mode: u32,
        large: LargeFile,
    ) -> io::Result<IndexedFile> {
        let LargeFile { file, hasher, size } = large;
        drop(file);
        let digest = hasher.finish();
        let blob = Cas::blob_rel(&digest, mode & 0o111 != 0);
        match Dir::hard_link(&self.pkg, rel, &self.cas.dir, &blob) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Same content stored before: share its inode, through a rename so the file
                // is never missing.
                let tmp = format!("{rel}.{}", crate::sys::random_name("dedupe-"));
                if Dir::hard_link(&self.cas.dir, &blob, &self.pkg, &tmp).is_ok()
                    && std::fs::rename(self.pkg.join(&tmp), self.pkg.join(rel)).is_err()
                {
                    let _ = self.pkg.unlink(&tmp);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.cas.adopt(&self.pkg, rel, &blob),
            // Too many links to the blob, or no hard links here: the file stays a plain copy.
            Err(_) => {}
        }
        Ok(IndexedFile {
            path: rel.to_owned(),
            size,
            mode,
            digest,
        })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !*self.published.lock() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
