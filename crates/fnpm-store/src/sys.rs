//! Thin filesystem helpers: directory handles that use `*at` syscalls on Unix (fewer path
//! walks in the hot loops) and plain paths elsewhere, plus temp names.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A directory opened once and addressed by relative paths afterwards.
#[derive(Debug)]
pub(crate) struct Dir {
    path: PathBuf,
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
}

#[cfg(unix)]
fn mode(m: u32) -> rustix::fs::Mode {
    rustix::fs::Mode::from_raw_mode(m as rustix::fs::RawMode)
}

impl Dir {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use rustix::fs::OFlags;
            let fd = rustix::fs::open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                mode(0),
            )?;
            Ok(Self {
                path: path.to_path_buf(),
                fd,
            })
        }
        #[cfg(not(unix))]
        {
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "not a directory",
                ));
            }
            Ok(Self {
                path: path.to_path_buf(),
            })
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }

    /// `mkdir`; `AlreadyExists` is returned as an error for the caller to decide.
    pub(crate) fn mkdir(&self, rel: &str, m: u32) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::mkdirat(&self.fd, rel, mode(m)).map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            let _ = m;
            std::fs::create_dir(self.join(rel))
        }
    }

    /// Creates `rel` and its missing parents.
    pub(crate) fn mkdir_all(&self, rel: &str) -> io::Result<()> {
        match self.mkdir(rel, 0o755) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(i) = rel.rfind('/') {
                    self.mkdir_all(&rel[..i])?;
                }
                match self.mkdir(rel, 0o755) {
                    Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
                    _ => Ok(()),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Creates a new file (`O_EXCL`), write-only.
    pub(crate) fn create_new(&self, rel: &str, m: u32) -> io::Result<File> {
        #[cfg(unix)]
        {
            use rustix::fs::OFlags;
            let fd = rustix::fs::openat(
                &self.fd,
                rel,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                mode(m),
            )?;
            Ok(File::from(fd))
        }
        #[cfg(not(unix))]
        {
            let _ = m;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.join(rel))
        }
    }

    /// Opens an existing file for reading (never follows a final symlink).
    pub(crate) fn open_read(&self, rel: &str) -> io::Result<File> {
        #[cfg(unix)]
        {
            use rustix::fs::OFlags;
            let fd = rustix::fs::openat(
                &self.fd,
                rel,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                mode(0),
            )?;
            Ok(File::from(fd))
        }
        #[cfg(not(unix))]
        {
            File::open(self.join(rel))
        }
    }

    pub(crate) fn unlink(&self, rel: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::unlinkat(&self.fd, rel, rustix::fs::AtFlags::empty())
                .map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            std::fs::remove_file(self.join(rel))
        }
    }

    /// `linkat(src/src_rel, dst/dst_rel)`.
    pub(crate) fn hard_link(src: &Dir, src_rel: &str, dst: &Dir, dst_rel: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::linkat(
                &src.fd,
                src_rel,
                &dst.fd,
                dst_rel,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            std::fs::hard_link(src.join(src_rel), dst.join(dst_rel))
        }
    }
}

/// `raw_os_error() == Some(code)`.
pub(crate) fn errno_is(e: &io::Error, code: i32) -> bool {
    e.raw_os_error() == Some(code)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name no other thread or process is likely to pick: pid, time and a counter hashed.
pub(crate) fn random_name(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = [0u8; 32];
    seed[..16].copy_from_slice(&nanos.to_le_bytes());
    seed[16..24].copy_from_slice(&n.to_le_bytes());
    seed[24..28].copy_from_slice(&std::process::id().to_le_bytes());
    let addr = &seed as *const _ as usize as u32;
    seed[28..32].copy_from_slice(&addr.to_le_bytes());
    let h = xxhash_rust::xxh3::xxh3_64(&seed);
    format!("{prefix}{h:016x}")
}

/// Whether `name` is one [`random_name`] made with an empty prefix (a staging directory).
pub(crate) fn is_staging_name(name: &str) -> bool {
    name.len() == 16
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Removes a tree, ignoring "already gone".
pub(crate) fn remove_tree(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        Ok(_) => match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
