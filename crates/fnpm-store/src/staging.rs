//! The staging directory a package is assembled in, and how a file gets into it.
//!
//! An import builds the package in `tmp/<random>/package/` and publishes it with one `rename`
//! into `pkgs/` (see [`crate::publish`]). Every file is written exactly once, straight into
//! the staging package (`O_EXCL`), and nothing else in the store is touched per file: an
//! import costs `open + write + close` per file, and the linker one hard link (or clone) per
//! file from `pkgs/<dir>/package/` into `node_modules`.
//!
//! Identical files inside one package share one inode: a file whose size, mode and xxh3-128
//! match a file already written is compared byte for byte with it and, when equal, hard
//! linked to it instead of being written again. The comparison makes a hash collision
//! harmless. Files of different packages are never shared (docs/DECISIONS.md, "Store
//! without a global blob directory").

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use xxhash_rust::xxh3::Xxh3;

use crate::index::IndexedFile;
use crate::sys::Dir;

/// What makes two files of a package candidates for sharing an inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ContentKey {
    hash: u128,
    size: u64,
    executable: bool,
}

impl ContentKey {
    pub(crate) fn of(data: &[u8], mode: u32) -> Self {
        Self {
            hash: xxhash_rust::xxh3::xxh3_128(data),
            size: data.len() as u64,
            executable: mode & 0o111 != 0,
        }
    }
}

/// `tmp/<random>/` with the package being built in `package/`. Removed on drop unless it was
/// published.
#[derive(Debug)]
pub(crate) struct Staging {
    root: PathBuf,
    pkg: Dir,
    dirs: Mutex<HashSet<String>>,
    /// The first file written with each content, for in-package deduplication.
    contents: Mutex<HashMap<ContentKey, String>>,
    published: AtomicBool,
}

/// A file too large to hold in memory, written to the staging package while it arrives.
pub(crate) struct LargeFile {
    file: File,
    hasher: Xxh3,
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
    pub(crate) fn create(tmp_dir: &Path) -> io::Result<Self> {
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
            root,
            pkg,
            dirs: Mutex::new(HashSet::new()),
            contents: Mutex::new(HashMap::new()),
            published: AtomicBool::new(false),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn package_root(&self) -> &Path {
        self.pkg.path()
    }

    pub(crate) fn mark_published(&self) {
        self.published.store(true, Ordering::Release);
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

    /// Adds a file whose bytes are in memory: a hard link to an identical file of this package
    /// when there is one, else a new file. Its parent directory must exist.
    pub(crate) fn add_bytes(&self, rel: &str, mode: u32, data: &[u8]) -> io::Result<IndexedFile> {
        let key = ContentKey::of(data, mode);
        let first = self.contents.lock().get(&key).cloned();
        if let Some(first) = first
            && first != rel
            && self.same_content(&first, data)
            && self.link_to(&first, rel).is_ok()
        {
            return Ok(record(rel, data.len() as u64, mode));
        }
        self.write_file(rel, mode, data)?;
        self.contents
            .lock()
            .entry(key)
            .or_insert_with(|| rel.to_owned());
        Ok(record(rel, data.len() as u64, mode))
    }

    /// Writes a new file (replacing an earlier entry of the same path: the later one wins).
    /// Its parent directory must exist.
    pub(crate) fn write_file(&self, rel: &str, mode: u32, data: &[u8]) -> io::Result<()> {
        let mut file = match self.pkg.create_new(rel, mode) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.pkg.unlink(rel)?;
                self.pkg.create_new(rel, mode)?
            }
            r => r?,
        };
        file.write_all(data)
    }

    /// Makes `rel` a hard link of `first` (both relative to the package root). A failure (no
    /// hard links on this filesystem, too many links) leaves `rel` absent.
    pub(crate) fn link_to(&self, first: &str, rel: &str) -> io::Result<()> {
        match Dir::hard_link(&self.pkg, first, &self.pkg, rel) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.pkg.unlink(rel)?;
                Dir::hard_link(&self.pkg, first, &self.pkg, rel)
            }
            r => r,
        }
    }

    /// Whether the file `first` holds exactly `data`.
    fn same_content(&self, first: &str, data: &[u8]) -> bool {
        let Ok(mut f) = self.pkg.open_read(first) else {
            return false;
        };
        let mut buf = vec![0u8; data.len()];
        let mut extra = [0u8; 1];
        f.read_exact(&mut buf).is_ok() && buf == data && matches!(f.read(&mut extra), Ok(0))
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
            hasher: Xxh3::new(),
            size: 0,
        })
    }

    /// Completes a streamed file. Later identical files of the package link to it.
    pub(crate) fn finish_large(&self, rel: &str, mode: u32, large: LargeFile) -> IndexedFile {
        let LargeFile { file, hasher, size } = large;
        drop(file);
        let key = ContentKey {
            hash: hasher.digest128(),
            size,
            executable: mode & 0o111 != 0,
        };
        self.contents
            .lock()
            .entry(key)
            .or_insert_with(|| rel.to_owned());
        record(rel, size, mode)
    }
}

fn record(rel: &str, size: u64, mode: u32) -> IndexedFile {
    IndexedFile {
        path: rel.to_owned(),
        size,
        mode,
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.published.load(Ordering::Acquire) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
