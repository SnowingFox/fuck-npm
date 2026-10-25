//! Filesystem materialisation primitives shared by the store and the linker:
//! clonefile (macOS `clonefile`, Linux `FICLONE`), hardlink (`linkat`), copy
//! (`copy_file_range` / `sendfile` / `fcopyfile` / read-write), symlinks and junctions, with a
//! sticky fallback.
//!
//! Ported from bun `install/PackageInstall.rs` (backend order and fallback to copyfile on
//! `EXDEV`/`EACCES`/`EPERM`), `sys/copy_file.rs` (the copy chain and its per-tree memo) and
//! `install/isolated_install/{FileCloner,Hardlinker,FileCopier}.rs`.
//!
//! - [`StickyBackend`] degrades process-wide: once a backend fails because the filesystem
//!   cannot do it, every later file uses the next one without retrying.
//! - [`CopyState`] remembers, for one directory tree, which copy syscalls failed (`EXDEV`
//!   seen once is very likely for every file of the tree), like bun's `LinuxCopyFileState`.
//! - A copy never truncates an existing destination (it may be a hard link of the source):
//!   it is unlinked and created again with `O_EXCL`.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicI8, AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

pub use fnpm_config::Backend;

use crate::index::FileIndex;
use crate::sys::{Dir, errno_is};

/// A failed filesystem operation while placing files.
#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    #[error("{op} {from} -> {to}: {source}")]
    /// `op` (`link`, `clonefile`, `copyfile`, `symlink`, `mkdir`, ...) failed.
    Io {
        /// The operation.
        op: &'static str,
        /// Source path.
        from: String,
        /// Destination path.
        to: String,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
}

impl MaterializeError {
    fn io(op: &'static str, from: &Path, to: &Path, source: io::Error) -> Self {
        Self::Io {
            op,
            from: from.display().to_string(),
            to: to.display().to_string(),
            source,
        }
    }

    /// The underlying I/O error.
    pub fn io_error(&self) -> &io::Error {
        match self {
            Self::Io { source, .. } => source,
        }
    }
}

const fn backend_code(b: Backend) -> u8 {
    match b {
        Backend::Clonefile => 0,
        Backend::Hardlink => 1,
        Backend::Copyfile => 2,
        Backend::Symlink => 3,
    }
}

/// The best backend that worked so far in this process. Once a clone fails with
/// `EXDEV`/`ENOTSUP`, every later file uses the fallback without retrying (bun's sticky
/// fallback).
#[derive(Debug)]
pub struct StickyBackend(AtomicU8);

impl StickyBackend {
    /// Starts at `preferred`.
    pub fn new(preferred: Backend) -> Self {
        Self(AtomicU8::new(backend_code(preferred)))
    }

    /// The backend to use now.
    pub fn current(&self) -> Backend {
        match self.0.load(Ordering::Relaxed) {
            0 => Backend::Clonefile,
            1 => Backend::Hardlink,
            2 => Backend::Copyfile,
            _ => Backend::Symlink,
        }
    }

    /// Moves to `to` unless an even more conservative backend is already in use.
    pub fn degrade(&self, to: Backend) {
        self.0.fetch_max(backend_code(to), Ordering::Relaxed);
    }
}

impl Default for StickyBackend {
    fn default() -> Self {
        Self::new(default_backend())
    }
}

/// The default for this OS: clonefile on macOS, hardlink elsewhere.
pub fn default_backend() -> Backend {
    if cfg!(target_os = "macos") {
        Backend::Clonefile
    } else {
        Backend::Hardlink
    }
}

/// Files placed, by backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeStats {
    /// Files placed.
    pub files: u64,
    /// By clonefile / FICLONE.
    pub cloned: u64,
    /// By hard link.
    pub hardlinked: u64,
    /// By copy.
    pub copied: u64,
}

impl MaterializeStats {
    fn count(&mut self, backend: Backend) {
        self.files += 1;
        match backend {
            Backend::Clonefile => self.cloned += 1,
            Backend::Hardlink => self.hardlinked += 1,
            Backend::Copyfile => self.copied += 1,
            Backend::Symlink => {}
        }
    }
}

/// Which copy syscalls failed for the current directory tree (bun `LinuxCopyFileState`).
/// Start a fresh one per package.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyState {
    /// `EXDEV` was seen: the tree crosses filesystems, so in-kernel copies are skipped.
    pub seen_exdev: bool,
    /// `FICLONE` failed for this tree.
    pub ficlone_failed: bool,
    /// `copy_file_range` failed for this tree.
    pub copy_file_range_failed: bool,
    /// `sendfile` failed for this tree.
    pub sendfile_failed: bool,
}

const UNKNOWN: i8 = 0;
const YES: i8 = 1;
const NO: i8 = -1;
static CAN_FICLONE: AtomicI8 = AtomicI8::new(UNKNOWN);
static CAN_COPY_FILE_RANGE: AtomicI8 = AtomicI8::new(UNKNOWN);

fn env_flag(names: &[&str]) -> bool {
    names.iter().any(|name| {
        std::env::var(name).is_ok_and(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        })
    })
}

fn allowed(flag: &AtomicI8, env: &[&str]) -> bool {
    match flag.load(Ordering::Relaxed) {
        YES => true,
        NO => false,
        _ => {
            let ok = !env_flag(env);
            flag.store(if ok { YES } else { NO }, Ordering::Relaxed);
            ok
        }
    }
}

/// `FICLONE` is used by copies unless disabled here or by
/// `FNPM_CONFIG_DISABLE_IOCTL_FICLONERANGE` / `BUN_CONFIG_DISABLE_ioctl_ficlonerange`.
pub fn can_use_ficlone() -> bool {
    allowed(
        &CAN_FICLONE,
        &[
            "FNPM_CONFIG_DISABLE_IOCTL_FICLONERANGE",
            "BUN_CONFIG_DISABLE_ioctl_ficlonerange",
        ],
    )
}

/// `copy_file_range` is used unless disabled here or by
/// `FNPM_CONFIG_DISABLE_COPY_FILE_RANGE` / `BUN_CONFIG_DISABLE_COPY_FILE_RANGE`.
pub fn can_use_copy_file_range() -> bool {
    allowed(
        &CAN_COPY_FILE_RANGE,
        &[
            "FNPM_CONFIG_DISABLE_COPY_FILE_RANGE",
            "BUN_CONFIG_DISABLE_COPY_FILE_RANGE",
        ],
    )
}

/// Turns `FICLONE` off for the rest of the process.
pub fn disable_ficlone() {
    CAN_FICLONE.store(NO, Ordering::Relaxed);
}

/// Turns `copy_file_range` off for the rest of the process.
pub fn disable_copy_file_range() {
    CAN_COPY_FILE_RANGE.store(NO, Ordering::Relaxed);
}

enum CloneFailure {
    /// The filesystem cannot clone: try hardlinks.
    Unsupported,
    /// Source and destination are on different filesystems: copy.
    CrossDevice,
    Other(io::Error),
}

fn classify_clone(e: io::Error) -> CloneFailure {
    #[cfg(unix)]
    {
        let code = e.raw_os_error().unwrap_or(0);
        if code == libc::EXDEV {
            return CloneFailure::CrossDevice;
        }
        if [
            libc::ENOTSUP,
            libc::EOPNOTSUPP,
            libc::EINVAL,
            libc::ENOTTY,
            libc::ENOSYS,
            libc::EPERM,
            libc::EBADF,
            libc::EACCES,
        ]
        .contains(&code)
        {
            return CloneFailure::Unsupported;
        }
    }
    if e.kind() == io::ErrorKind::Unsupported {
        return CloneFailure::Unsupported;
    }
    CloneFailure::Other(e)
}

/// Whether a failed hard link means "this filesystem / pair cannot hard link" (bun:
/// `EXDEV`, and `EACCES`/`EPERM` from FUSE filesystems such as Android's sdcard).
fn link_unsupported(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        let code = e.raw_os_error().unwrap_or(0);
        [
            libc::EXDEV,
            libc::EACCES,
            libc::EPERM,
            libc::ENOTSUP,
            libc::EOPNOTSUPP,
            libc::ENOSYS,
        ]
        .contains(&code)
    }
    #[cfg(not(unix))]
    {
        e.kind() == io::ErrorKind::Unsupported
            || e.kind() == io::ErrorKind::PermissionDenied
            || e.kind() == io::ErrorKind::CrossesDevices
    }
}

fn too_many_links(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        errno_is(e, libc::EMLINK)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

#[cfg(unix)]
fn file_perm(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

/// Creates `dst` with `O_EXCL`; an existing file is unlinked first rather than truncated,
/// because it may be a hard link of the source.
fn create_replacing(dst: &Path, perm: u32) -> io::Result<File> {
    let open = || {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(perm).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(not(unix))]
        let _ = perm;
        o.open(dst)
    };
    match open() {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(dst);
            open()
        }
        r => r,
    }
}

#[cfg(target_os = "macos")]
fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let s = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let d = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let clone = || {
        // SAFETY: both arguments are NUL-terminated paths that live for the call.
        let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), libc::CLONE_NOFOLLOW) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    match clone() {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // The existing file may belong to a different version.
            let _ = std::fs::remove_file(dst);
            clone()
        }
        r => r,
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    let input = File::open(src)?;
    let perm = file_perm(&input.metadata()?);
    let out = create_replacing(dst, perm)?;
    match rustix::fs::ioctl_ficlone(&out, &input) {
        Ok(()) => Ok(()),
        Err(e) => {
            drop(out);
            let _ = std::fs::remove_file(dst);
            Err(e.into())
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn clone_file(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

fn hard_link_replacing(src: &Path, dst: &Path) -> io::Result<()> {
    match std::fs::hard_link(src, dst) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(dst)?;
            std::fs::hard_link(src, dst)
        }
        r => r,
    }
}

#[cfg(unix)]
fn symlink_replacing(target: &Path, link: &Path) -> io::Result<()> {
    match std::os::unix::fs::symlink(target, link) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            crate::sys::remove_tree(link)?;
            std::os::unix::fs::symlink(target, link)
        }
        r => r,
    }
}

#[cfg(windows)]
fn symlink_replacing(target: &Path, link: &Path) -> io::Result<()> {
    match std::os::windows::fs::symlink_file(target, link) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            crate::sys::remove_tree(link)?;
            std::os::windows::fs::symlink_file(target, link)
        }
        r => r,
    }
}

/// Copies the contents and permission bits of `src` to `dst` (created anew).
pub fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    copy_file_with_state(src, dst, &mut CopyState::default())
}

/// [`copy_file`] that remembers failed syscalls in `state` for the rest of the tree.
#[cfg(unix)]
pub fn copy_file_with_state(src: &Path, dst: &Path, state: &mut CopyState) -> io::Result<()> {
    let input = File::open(src)?;
    let meta = input.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let perm = file_perm(&meta);
    let out = create_replacing(dst, perm)?;
    copy_data(&input, &out, state)?;
    // Best effort: filesystems without permission bits reject fchmod.
    let _ = rustix::fs::fchmod(
        &out,
        rustix::fs::Mode::from_raw_mode(perm as rustix::fs::RawMode),
    );
    Ok(())
}

/// [`copy_file`] that remembers failed syscalls in `state` for the rest of the tree.
#[cfg(not(unix))]
pub fn copy_file_with_state(src: &Path, dst: &Path, _state: &mut CopyState) -> io::Result<()> {
    match std::fs::remove_file(dst) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::copy(src, dst).map(|_| ())
}

#[cfg(target_os = "macos")]
fn copy_data(input: &File, out: &File, _state: &mut CopyState) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: both fds are open for the duration of the call; a null state is allowed.
    let rc = unsafe {
        libc::fcopyfile(
            input.as_raw_fd(),
            out.as_raw_fd(),
            std::ptr::null_mut(),
            libc::COPYFILE_DATA,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if !errno_is(&e, libc::EOPNOTSUPP) {
        return Err(e);
    }
    while read_write_chunk(input, out)? > 0 {}
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_data(input: &File, out: &File, state: &mut CopyState) -> io::Result<()> {
    use rustix::io::Errno;
    if can_use_ficlone() && !state.seen_exdev && !state.ficlone_failed {
        match rustix::fs::ioctl_ficlone(out, input) {
            Ok(()) => return Ok(()),
            Err(Errno::XDEV) => state.seen_exdev = true,
            Err(Errno::INTR) => {}
            Err(
                Errno::ACCESS
                | Errno::BADF
                | Errno::INVAL
                | Errno::OPNOTSUPP
                | Errno::NOSYS
                | Errno::PERM
                | Errno::NOTTY,
            ) => {
                disable_ficlone();
                state.ficlone_failed = true;
            }
            Err(_) => state.ficlone_failed = true,
        }
    }
    let mut hinted = false;
    loop {
        if copy_chunk(input, out, state, &mut hinted)? == 0 {
            return Ok(());
        }
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    windows
)))]
fn copy_data(input: &File, out: &File, _state: &mut CopyState) -> io::Result<()> {
    while read_write_chunk(input, out)? > 0 {}
    Ok(())
}

/// One step of bun's chain: `copy_file_range`, then `sendfile`, then read/write. Uses and
/// advances the file offsets, so a fallback continues where the previous call stopped.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_chunk(
    input: &File,
    out: &File,
    state: &mut CopyState,
    hinted: &mut bool,
) -> io::Result<usize> {
    use rustix::io::Errno;
    const LEN: usize = (i32::MAX - 1) as usize;
    if can_use_copy_file_range() && !state.seen_exdev && !state.copy_file_range_failed {
        loop {
            match rustix::fs::copy_file_range(input, None, out, None, LEN) {
                Ok(n) => return Ok(n),
                Err(Errno::INTR) => continue,
                Err(Errno::XDEV) => {
                    state.seen_exdev = true;
                    state.copy_file_range_failed = true;
                }
                Err(Errno::OPNOTSUPP | Errno::NOSYS) => {
                    state.copy_file_range_failed = true;
                    disable_copy_file_range();
                }
                Err(_) => state.copy_file_range_failed = true,
            }
            break;
        }
    }
    while !state.sendfile_failed {
        match rustix::fs::sendfile(out, input, None, LEN) {
            Ok(n) => return Ok(n),
            Err(Errno::INTR) => continue,
            Err(Errno::XDEV) => {
                state.seen_exdev = true;
                state.sendfile_failed = true;
            }
            Err(_) => state.sendfile_failed = true,
        }
    }
    if !*hinted {
        *hinted = true;
        let _ = rustix::fs::fadvise(input, 0, None, rustix::fs::Advice::Sequential);
    }
    read_write_chunk(input, out)
}

#[cfg(unix)]
fn read_write_chunk(mut input: &File, mut out: &File) -> io::Result<usize> {
    use std::io::{Read, Write};
    let mut buf = [0u8; 32 * 1024];
    let n = loop {
        match input.read(&mut buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => break r?,
        }
    };
    out.write_all(&buf[..n])?;
    Ok(n)
}

/// Places one file. `dst`'s parent must exist. Returns the backend that was used.
pub fn place_file(
    src: &Path,
    dst: &Path,
    sticky: &StickyBackend,
) -> Result<Backend, MaterializeError> {
    place_file_with_state(src, dst, sticky, &mut CopyState::default())
}

/// [`place_file`] with the copy memo of the tree being placed.
pub fn place_file_with_state(
    src: &Path,
    dst: &Path,
    sticky: &StickyBackend,
    state: &mut CopyState,
) -> Result<Backend, MaterializeError> {
    loop {
        match sticky.current() {
            Backend::Clonefile => match clone_file(src, dst) {
                Ok(()) => return Ok(Backend::Clonefile),
                Err(e) => match classify_clone(e) {
                    CloneFailure::Unsupported => sticky.degrade(Backend::Hardlink),
                    // A hard link would fail with EXDEV too.
                    CloneFailure::CrossDevice => sticky.degrade(Backend::Copyfile),
                    CloneFailure::Other(e) => {
                        return Err(MaterializeError::io("clonefile", src, dst, e));
                    }
                },
            },
            Backend::Hardlink => match hard_link_replacing(src, dst) {
                Ok(()) => return Ok(Backend::Hardlink),
                Err(e) if too_many_links(&e) => {
                    // This file has too many links; copy it alone.
                    copy_file_with_state(src, dst, state)
                        .map_err(|e| MaterializeError::io("copyfile", src, dst, e))?;
                    return Ok(Backend::Copyfile);
                }
                Err(e) if link_unsupported(&e) => sticky.degrade(Backend::Copyfile),
                Err(e) => return Err(MaterializeError::io("link", src, dst, e)),
            },
            Backend::Copyfile => {
                copy_file_with_state(src, dst, state)
                    .map_err(|e| MaterializeError::io("copyfile", src, dst, e))?;
                return Ok(Backend::Copyfile);
            }
            Backend::Symlink => {
                #[cfg(any(unix, windows))]
                {
                    symlink_replacing(src, dst)
                        .map_err(|e| MaterializeError::io("symlink", src, dst, e))?;
                    return Ok(Backend::Symlink);
                }
                #[cfg(not(any(unix, windows)))]
                sticky.degrade(Backend::Copyfile);
            }
        }
    }
}

/// Places `rel` from `src` into `dst` (both open directories). Hard links go through
/// `linkat` on the directory handles.
fn place_rel(
    src: &Dir,
    dst: &Dir,
    rel: &str,
    sticky: &StickyBackend,
    state: &mut CopyState,
) -> Result<Backend, MaterializeError> {
    if sticky.current() == Backend::Hardlink {
        let attempt = match Dir::hard_link(src, rel, dst, rel) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let _ = dst.unlink(rel);
                Dir::hard_link(src, rel, dst, rel)
            }
            r => r,
        };
        match attempt {
            Ok(()) => return Ok(Backend::Hardlink),
            Err(e) if link_unsupported(&e) => sticky.degrade(Backend::Copyfile),
            Err(e) if too_many_links(&e) => {}
            Err(e) => {
                return Err(MaterializeError::io(
                    "link",
                    &src.join(rel),
                    &dst.join(rel),
                    e,
                ));
            }
        }
    }
    place_file_with_state(&src.join(rel), &dst.join(rel), sticky, state)
}

/// Materialises a store package at `dst` from its `index` (bun `PackageInstall` backends,
/// without walking the source): one `clonefile` of the whole directory on macOS when the
/// backend allows it, else every directory of the index, then every file.
pub fn materialize_package(
    src_root: &Path,
    index: &FileIndex,
    dst: &Path,
    sticky: &StickyBackend,
) -> Result<MaterializeStats, MaterializeError> {
    let mut stats = MaterializeStats::default();
    if sticky.current() == Backend::Clonefile && clone_dir(src_root, dst)? {
        stats.files = index.files.len() as u64;
        stats.cloned = stats.files;
        return Ok(stats);
    }
    std::fs::create_dir_all(dst).map_err(|e| MaterializeError::io("mkdir", src_root, dst, e))?;
    let src = Dir::open(src_root).map_err(|e| MaterializeError::io("open", src_root, dst, e))?;
    let out = Dir::open(dst).map_err(|e| MaterializeError::io("open", src_root, dst, e))?;
    for dir in &index.dirs {
        match out.mkdir(dir, 0o755) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(MaterializeError::io(
                    "mkdir",
                    &src.join(dir),
                    &out.join(dir),
                    e,
                ));
            }
        }
    }
    let mut state = CopyState::default();
    for file in &index.files {
        let used = place_rel(&src, &out, &file.path, sticky, &mut state)?;
        stats.count(used);
    }
    Ok(stats)
}

/// Clones a whole directory in one syscall where the OS allows it (macOS `clonefile` on a
/// directory); `Ok(false)` when not supported so the caller falls back to per-file.
pub fn clone_dir(src: &Path, dst: &Path) -> Result<bool, MaterializeError> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let s = CString::new(src.as_os_str().as_bytes()).map_err(|_| {
            MaterializeError::io(
                "clonefile",
                src,
                dst,
                io::Error::from(io::ErrorKind::InvalidInput),
            )
        })?;
        let d = CString::new(dst.as_os_str().as_bytes()).map_err(|_| {
            MaterializeError::io(
                "clonefile",
                src,
                dst,
                io::Error::from(io::ErrorKind::InvalidInput),
            )
        })?;
        let clone = || {
            // SAFETY: NUL-terminated paths valid for the call.
            let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), libc::CLONE_NOFOLLOW) };
            if rc == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        let mut result = clone();
        if let Err(e) = &result
            && e.kind() == io::ErrorKind::NotFound
            && src.exists()
            && let Some(parent) = dst.parent()
        {
            let _ = std::fs::create_dir_all(parent);
            result = clone();
        }
        match result {
            Ok(()) => Ok(true),
            // EEXIST: the destination (or a node_modules inside it) exists; the per-file path
            // merges into it.
            Err(e)
                if [libc::EXDEV, libc::ENOTSUP, libc::EEXIST]
                    .contains(&e.raw_os_error().unwrap_or(0)) =>
            {
                Ok(false)
            }
            Err(e) => Err(MaterializeError::io("clonefile", src, dst, e)),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (src, dst);
        Ok(false)
    }
}

/// `to` relative to the directory `from`, lexically (both absolute).
pub fn relative_path(from: &Path, to: &Path) -> PathBuf {
    fn normalize(p: &Path) -> Vec<Component<'_>> {
        let mut out: Vec<Component<'_>> = Vec::new();
        for c in p.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    if matches!(out.last(), Some(Component::Normal(_))) {
                        out.pop();
                    }
                }
                c => out.push(c),
            }
        }
        out
    }
    let from = normalize(from);
    let to = normalize(to);
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut rel = PathBuf::new();
    for _ in common..from.len() {
        rel.push("..");
    }
    for c in &to[common..] {
        rel.push(c.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    rel
}

/// Relative symlink on Unix; junction for directories on Windows (no admin rights needed).
///
/// An existing link to the same target is kept; anything else at `link` is replaced. Missing
/// parents of `link` are created.
pub fn symlink_dir(target: &Path, link: &Path) -> Result<(), MaterializeError> {
    let text = match link.parent() {
        Some(parent) if target.is_absolute() && link.is_absolute() => relative_path(parent, target),
        _ => target.to_path_buf(),
    };
    let make = || -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&text, link)
        }
        #[cfg(windows)]
        {
            // Junctions need an absolute target.
            let absolute = if target.is_absolute() {
                target.to_path_buf()
            } else {
                link.parent().unwrap_or(Path::new(".")).join(target)
            };
            junction(&absolute, link)
        }
    };
    let mut result = make();
    if let Err(e) = &result
        && e.kind() == io::ErrorKind::NotFound
        && let Some(parent) = link.parent()
    {
        let _ = std::fs::create_dir_all(parent);
        result = make();
    }
    if let Err(e) = &result
        && e.kind() == io::ErrorKind::AlreadyExists
    {
        if std::fs::read_link(link).is_ok_and(|current| current == text) {
            return Ok(());
        }
        crate::sys::remove_tree(link)
            .map_err(|e| MaterializeError::io("remove", target, link, e))?;
        result = make();
    }
    result.map_err(|e| MaterializeError::io("symlink", target, link, e))
}

#[cfg(windows)]
fn junction(target: &Path, link: &Path) -> io::Result<()> {
    // std has no junction API; a directory symlink works with developer mode or elevation.
    std::os::windows::fs::symlink_dir(target, link)
}

/// Write to `<path>.tmp-<rand>` then rename; the standard atomic write.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let tmp = path.with_file_name(format!(
        "{}.{}",
        name.to_string_lossy(),
        crate::sys::random_name("tmp-")
    ));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
