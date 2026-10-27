//! Filesystem helpers of the linker: materialising a store package with the best backend
//! (bun `PackageInstall.rs` install_with_*, `isolated_install/{FileCloner,Hardlinker,
//! FileCopier}.rs`), symlinks that are idempotent (`Symlinker.rs`), moving trees aside and
//! deleting them in the background, and lexical path arithmetic.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use fnpm_config::Backend;
use fnpm_store::fsops::StickyBackend;
use fnpm_store::{FileIndex, IndexedFile, MaterializeError};
use rayon::prelude::*;

// CLONE_NOFOLLOW from <sys/clonefile.h>, absent from libc 0.2.191. Same verified
// flag as fnpm-store (Bun 179664ea and reflink-copy 0.1.30).
#[cfg(target_os = "macos")]
const CLONE_NOFOLLOW: u32 = 0x0001;

/// A package with at least this many files has them linked by several threads (the calling
/// thread's rayon pool), [`LINK_CHUNK`] files per task: one big package (next: ~8,700 files)
/// would otherwise bound the whole link step.
const PARALLEL_LINK_MIN: usize = 768;
/// See [`PARALLEL_LINK_MIN`].
const LINK_CHUNK: usize = 192;

/// Prefix of a tree moved out of the way (bun `OldModules::PREFIX`).
pub const OLD_MODULES_PREFIX: &str = ".old_modules-";
/// Prefix of the linker's staging directories.
pub const STAGING_PREFIX: &str = ".fnpm-staging-";
/// Prefix of a package directory moved aside before it is replaced.
pub const MOVED_ASIDE_PREFIX: &str = ".fnpm-old-";

/// True for names an interrupted install can leave behind (bun `prune::is_leftover`).
pub fn is_leftover(name: &str) -> bool {
    name.starts_with(OLD_MODULES_PREFIX)
        || name.starts_with(STAGING_PREFIX)
        || name.starts_with(MOVED_ASIDE_PREFIX)
}

/// A process-unique random-looking suffix (no RNG dependency: time, pid and a counter hashed).
pub fn random_suffix() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut bytes = [0u8; 24];
    bytes[..8].copy_from_slice(&t.to_le_bytes());
    bytes[8..16].copy_from_slice(&u64::from(std::process::id()).to_le_bytes());
    bytes[16..].copy_from_slice(&n.to_le_bytes());
    xxhash_rust::xxh3::xxh3_64(&bytes)
}

fn mat_err(op: &'static str, from: &Path, to: &Path, source: io::Error) -> MaterializeError {
    MaterializeError::Io {
        op,
        from: from.display().to_string(),
        to: to.display().to_string(),
        source,
    }
}

fn is_errno(e: &io::Error, codes: &[i32]) -> bool {
    e.raw_os_error().is_some_and(|c| codes.contains(&c))
}

/// Errors that mean "this backend cannot work here", so the sticky backend degrades.
fn is_unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
        || is_errno(
            e,
            &[
                libc::EXDEV,
                libc::ENOTSUP,
                libc::EOPNOTSUPP,
                libc::EPERM,
                libc::EINVAL,
                libc::ENOSYS,
                libc::EMLINK,
            ],
        )
}

/// A clone or hard link that failed this way moves to the next backend.
fn falls_back(backend: Backend, e: &io::Error) -> bool {
    matches!(backend, Backend::Clonefile | Backend::Hardlink) && is_unsupported(e)
}

/// A hard link refused with `EACCES` (bun: FUSE filesystems such as Android's sdcard, or one
/// file the filesystem protects) is copied instead, that file only: the other packages keep
/// their hard links.
fn copies_instead(backend: Backend, e: &io::Error) -> bool {
    backend == Backend::Hardlink && is_errno(e, &[libc::EACCES])
}

/// Copies a store package into a fresh directory. Holds the process-wide sticky backend.
#[derive(Debug)]
pub struct Materializer {
    sticky: StickyBackend,
    /// Used at least once: (clonefile, hardlink, copyfile, symlink).
    used: [std::sync::atomic::AtomicBool; 4],
}

impl Materializer {
    /// Starts with `preferred`; failures move to the next backend for the rest of the process.
    pub fn new(preferred: Backend) -> Self {
        Self {
            sticky: StickyBackend::new(preferred),
            used: Default::default(),
        }
    }

    /// The backend the next file will use.
    pub fn current(&self) -> Backend {
        self.sticky.current()
    }

    /// The weakest backend that was used, i.e. what the report shows.
    pub fn used(&self) -> Option<Backend> {
        [
            Backend::Symlink,
            Backend::Copyfile,
            Backend::Hardlink,
            Backend::Clonefile,
        ]
        .into_iter()
        .find(|b| self.used[*b as usize].load(Ordering::Relaxed))
    }

    fn mark(&self, b: Backend) {
        self.used[b as usize].store(true, Ordering::Relaxed);
    }

    /// Materialises the package at `src` (a store `package/` dir described by `index`) into
    /// `dest`, which must not exist. Creates `dest`'s parents.
    pub fn materialize(
        &self,
        src: &Path,
        index: &FileIndex,
        dest: &Path,
    ) -> Result<Backend, MaterializeError> {
        if let Some(parent) = dest.parent() {
            create_dir_all(parent).map_err(|e| mat_err("mkdir", src, parent, e))?;
        }
        if self.current() == Backend::Clonefile && cfg!(target_os = "macos") {
            match clone_dir(src, dest) {
                Ok(()) => {
                    self.mark(Backend::Clonefile);
                    return Ok(Backend::Clonefile);
                }
                Err(e) if is_errno(&e, &[libc::EXDEV]) => self.sticky.degrade(Backend::Copyfile),
                Err(e) if is_unsupported(&e) => self.sticky.degrade(Backend::Hardlink),
                Err(e) if is_errno(&e, &[libc::EEXIST]) => {}
                Err(e) => return Err(mat_err("clonefile", src, dest, e)),
            }
        }
        match std::fs::create_dir(dest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(mat_err("mkdir", src, dest, e)),
        }
        for d in &index.dirs {
            if d.is_empty() {
                continue;
            }
            if !is_safe_index_path(d) {
                return Err(unsafe_index_path(src, dest, d));
            }
            let dir_buf = dest.join(d);
            match std::fs::create_dir(&dir_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    create_dir_all(&dir_buf).map_err(|e| mat_err("mkdir", src, &dir_buf, e))?;
                }
                Err(e) => return Err(mat_err("mkdir", src, &dir_buf, e)),
            }
        }
        // The root `package.json` goes last: an install that is interrupted (or fails on a
        // file) leaves no `package.json`, so the next run's cheap check rebuilds the folder
        // instead of trusting a half-copied package.
        let manifest = index.files.iter().find(|f| f.path == "package.json");
        let others = |chunk: &'_ [IndexedFile]| -> Result<Backend, MaterializeError> {
            self.place_files(src, dest, chunk.iter().filter(|f| f.path != "package.json"))
        };
        let mut weakest = if index.files.len() >= PARALLEL_LINK_MIN {
            index
                .files
                .par_chunks(LINK_CHUNK)
                .map(others)
                .try_reduce(|| Backend::Clonefile, |a, b| Ok(weaker(a, b)))?
        } else {
            others(&index.files)?
        };
        if let Some(f) = manifest {
            weakest = weaker(weakest, self.place_files(src, dest, std::iter::once(f))?);
        }
        if index.files.is_empty() {
            weakest = self.current();
        }
        Ok(weakest)
    }

    /// [`Materializer::materialize`] with a fallback of its own: what this package's files
    /// fail with moves only this package to the next backend. bun links a `file:` folder from
    /// the folder, which may be on another file system than every other package.
    pub fn materialize_alone(
        &self,
        src: &Path,
        index: &FileIndex,
        dest: &Path,
    ) -> Result<Backend, MaterializeError> {
        let own = Materializer::new(self.current());
        let result = own.materialize(src, index, dest);
        for (mine, theirs) in self.used.iter().zip(&own.used) {
            if theirs.load(Ordering::Relaxed) {
                mine.store(true, Ordering::Relaxed);
            }
        }
        result
    }

    /// Places `files` of the package `src` into `dest` (whose directories exist); returns the
    /// weakest backend used.
    fn place_files<'f>(
        &self,
        src: &Path,
        dest: &Path,
        files: impl Iterator<Item = &'f IndexedFile>,
    ) -> Result<Backend, MaterializeError> {
        let mut weakest = Backend::Clonefile;
        let mut linker = DirLinker::open(self.current(), src, dest);
        for f in files {
            if !is_safe_index_path(&f.path) {
                return Err(unsafe_index_path(src, dest, &f.path));
            }
            let used = match linker
                .as_mut()
                .filter(|_| self.current() == Backend::Hardlink)
                .and_then(|l| l.link(&f.path))
            {
                Some(()) => Backend::Hardlink,
                None => self.place_file(&src.join(&f.path), &dest.join(&f.path))?,
            };
            weakest = weaker(weakest, used);
        }
        if linker.is_some_and(|l| l.linked) {
            self.mark(Backend::Hardlink);
        }
        Ok(weakest)
    }

    /// Copies `from` to a new file `to`. An existing `to` is an error, as for the other backends:
    /// `std::fs::copy` would truncate it, and through a hardlink into the store that empties the
    /// store's file (two trees writing one folder, as a self-contained workspace and a sibling's
    /// own tree do).
    fn copy_new(from: &Path, to: &Path) -> io::Result<()> {
        let mut src = std::fs::File::open(from)?;
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?;
        io::copy(&mut src, &mut dst)?;
        let perms = src.metadata()?.permissions();
        dst.set_permissions(perms)
    }

    /// Places one file, degrading the sticky backend on "unsupported" errors.
    pub fn place_file(&self, from: &Path, to: &Path) -> Result<Backend, MaterializeError> {
        loop {
            let backend = self.current();
            let result = match backend {
                Backend::Clonefile => reflink_copy::reflink(from, to),
                Backend::Hardlink => std::fs::hard_link(from, to),
                Backend::Copyfile => Self::copy_new(from, to),
                Backend::Symlink => symlink_file(from, to),
            };
            match result {
                Ok(()) => {
                    self.mark(backend);
                    return Ok(backend);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    std::fs::remove_file(to).map_err(|e| mat_err("unlink", from, to, e))?;
                    let retry = match backend {
                        Backend::Clonefile => reflink_copy::reflink(from, to),
                        Backend::Hardlink => std::fs::hard_link(from, to),
                        Backend::Copyfile => Self::copy_new(from, to),
                        Backend::Symlink => symlink_file(from, to),
                    };
                    match retry {
                        Ok(()) => {
                            self.mark(backend);
                            return Ok(backend);
                        }
                        Err(e) if copies_instead(backend, &e) => {
                            let _ = std::fs::remove_file(to);
                            std::fs::copy(from, to)
                                .map_err(|e| mat_err("copyfile", from, to, e))?;
                            return Ok(Backend::Copyfile);
                        }
                        Err(e) if falls_back(backend, &e) => {
                            let _ = std::fs::remove_file(to);
                            self.degrade_after(backend, &e);
                        }
                        Err(e) => return Err(mat_err(op_name(backend), from, to, e)),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && !from.exists() => {
                    return Err(mat_err(op_name(backend), from, to, e));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if let Some(parent) = to.parent() {
                        create_dir_all(parent).map_err(|e| mat_err("mkdir", from, parent, e))?;
                    }
                }
                Err(e) if copies_instead(backend, &e) => {
                    let _ = std::fs::remove_file(to);
                    std::fs::copy(from, to).map_err(|e| mat_err("copyfile", from, to, e))?;
                    return Ok(Backend::Copyfile);
                }
                Err(e) if falls_back(backend, &e) => {
                    let _ = std::fs::remove_file(to);
                    self.degrade_after(backend, &e);
                }
                Err(e) => return Err(mat_err(op_name(backend), from, to, e)),
            }
        }
    }

    fn degrade_after(&self, backend: Backend, e: &io::Error) {
        let next = match backend {
            // A clone across devices cannot hardlink either.
            Backend::Clonefile if is_errno(e, &[libc::EXDEV]) => Backend::Copyfile,
            Backend::Clonefile => Backend::Hardlink,
            _ => Backend::Copyfile,
        };
        self.sticky.degrade(next);
    }
}

/// The weaker of two backends (the report shows the weakest one used).
fn weaker(a: Backend, b: Backend) -> Backend {
    if (b as u8) > (a as u8) { b } else { a }
}

/// Hard links relative to open directory descriptors of the package source and destination
/// (`linkat(src_fd, path, dest_fd, path)`), like bun: the kernel walks two short relative
/// paths instead of two absolute ones per file, and no `PathBuf` is built. Any failure is
/// left to [`Materializer::place_file`], which handles every error case.
struct DirLinker {
    #[cfg(unix)]
    src: std::fs::File,
    #[cfg(unix)]
    dest: std::fs::File,
    #[cfg(unix)]
    buf: Vec<u8>,
    linked: bool,
}

impl DirLinker {
    #[cfg(unix)]
    fn open(backend: Backend, src: &Path, dest: &Path) -> Option<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if backend != Backend::Hardlink {
            return None;
        }
        let open = |p: &Path| {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(p)
                .ok()
        };
        Some(Self {
            src: open(src)?,
            dest: open(dest)?,
            buf: Vec::with_capacity(256),
            linked: false,
        })
    }

    #[cfg(not(unix))]
    fn open(_: Backend, _: &Path, _: &Path) -> Option<Self> {
        None
    }

    /// `Some(())` when the link was made.
    #[cfg(unix)]
    fn link(&mut self, rel: &str) -> Option<()> {
        use std::os::fd::AsRawFd;
        self.buf.clear();
        self.buf.extend_from_slice(rel.as_bytes());
        self.buf.push(0);
        let path = std::ffi::CStr::from_bytes_with_nul(&self.buf).ok()?;
        // SAFETY: both descriptors are open directories owned by `self`, and `path` is a valid
        // NUL-terminated string that outlives the call.
        let rc = unsafe {
            libc::linkat(
                self.src.as_raw_fd(),
                path.as_ptr(),
                self.dest.as_raw_fd(),
                path.as_ptr(),
                0,
            )
        };
        if rc == 0 {
            self.linked = true;
            Some(())
        } else {
            None
        }
    }

    #[cfg(not(unix))]
    fn link(&mut self, _: &str) -> Option<()> {
        None
    }
}

/// A store index path must stay inside the package: relative, no `..`, no NUL. The store
/// writes sane indexes; this guards against a corrupted or planted index file.
fn is_safe_index_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with(['/', '\\'])
        && !p.contains('\0')
        && !Path::new(p).is_absolute()
        && !p.split(['/', '\\']).any(|c| c == "..")
}

fn unsafe_index_path(src: &Path, dest: &Path, p: &str) -> MaterializeError {
    mat_err(
        "index",
        src,
        dest,
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("store index entry {p:?} leaves the package"),
        ),
    )
}

fn op_name(b: Backend) -> &'static str {
    match b {
        Backend::Clonefile => "clonefile",
        Backend::Hardlink => "hardlink",
        Backend::Copyfile => "copyfile",
        Backend::Symlink => "symlink",
    }
}

#[cfg(unix)]
fn symlink_file(from: &Path, to: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(from, to)
}

#[cfg(windows)]
fn symlink_file(from: &Path, to: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(from, to)
}

/// macOS `clonefile` of a whole directory: one syscall for the whole package.
#[cfg(target_os = "macos")]
pub fn clone_dir(src: &Path, dest: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let s = std::ffi::CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let d = std::ffi::CString::new(dest.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
    let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Not available outside macOS.
#[cfg(not(target_os = "macos"))]
pub fn clone_dir(_src: &Path, _dest: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// `create_dir_all` that tolerates a concurrent creator.
pub fn create_dir_all(p: &Path) -> io::Result<()> {
    match std::fs::create_dir_all(p) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && p.is_dir() => Ok(()),
        r => r,
    }
}

/// What `lstat` says is at `p`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Nothing there.
    Missing,
    /// A real directory.
    Dir,
    /// A file (or something else that is not a directory or link).
    File,
    /// A symlink (or junction), not followed.
    Symlink,
}

/// `lstat` classification; errors other than "not found" count as a file (the caller will
/// try to remove it and report the real error).
pub fn kind(p: &Path) -> Kind {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => Kind::Symlink,
        Ok(m) if m.is_dir() => Kind::Dir,
        Ok(_) => Kind::File,
        Err(e) if e.kind() == io::ErrorKind::NotFound || is_errno(&e, &[libc::ENOTDIR]) => {
            Kind::Missing
        }
        Err(_) => Kind::File,
    }
}

/// Removes whatever is at `p` (a link is removed, never followed). Missing is fine.
pub fn remove_any(p: &Path) -> io::Result<()> {
    let r = match kind(p) {
        Kind::Missing => return Ok(()),
        Kind::Dir => std::fs::remove_dir_all(p),
        Kind::File => std::fs::remove_file(p),
        Kind::Symlink => remove_link(p),
    };
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Removes a symlink (or junction on Windows).
pub fn remove_link(p: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        std::fs::remove_dir(p).or_else(|_| std::fs::remove_file(p))
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_file(p)
    }
}

/// Paths queued for deletion on a background thread.
#[derive(Default)]
struct TrashState {
    queue: Vec<PathBuf>,
    /// A worker is draining the queue. Changes under the same lock as `queue`, so an item is
    /// never pushed after the worker decided to stop.
    active: bool,
    /// Deletions popped but not finished.
    in_flight: usize,
}

struct Trash {
    state: Mutex<TrashState>,
    idle: Condvar,
}

fn trash() -> &'static Trash {
    static TRASH: OnceLock<Trash> = OnceLock::new();
    TRASH.get_or_init(|| Trash {
        state: Mutex::new(TrashState::default()),
        idle: Condvar::new(),
    })
}

fn lock_trash() -> std::sync::MutexGuard<'static, TrashState> {
    trash().state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Deletes one path that was popped (and counted in `in_flight`), then reports it done.
fn finish_one(p: &Path) {
    let _ = remove_any(p);
    lock_trash().in_flight -= 1;
    trash().idle.notify_all();
}

fn worker() {
    loop {
        let next = {
            let mut st = lock_trash();
            match st.queue.pop() {
                Some(p) => {
                    st.in_flight += 1;
                    p
                }
                None => {
                    st.active = false;
                    drop(st);
                    trash().idle.notify_all();
                    return;
                }
            }
        };
        finish_one(&next);
    }
}

/// Deletes `p` on a background thread. The caller has already renamed it out of the way.
pub fn delete_in_background(p: PathBuf) {
    {
        let mut st = lock_trash();
        st.queue.push(p);
        if st.active {
            return;
        }
        st.active = true;
    }
    let spawned = std::thread::Builder::new()
        .name("fnpm-trash".into())
        .spawn(worker);
    if spawned.is_err() {
        // No thread: delete inline.
        worker();
    }
}

/// Blocks until every background deletion queued so far (by any thread) has finished.
pub fn flush_background_deletes() {
    let mut st = lock_trash();
    loop {
        if st.queue.is_empty() && st.in_flight == 0 {
            return;
        }
        if !st.active
            && let Some(p) = st.queue.pop()
        {
            st.in_flight += 1;
            drop(st);
            finish_one(&p);
            st = lock_trash();
            continue;
        }
        st = trash().idle.wait(st).unwrap_or_else(|e| e.into_inner());
    }
}

/// Renames `p` to `<dir of p>/.fnpm-old-<rand>` and deletes it in the background; deletes in
/// place when the rename fails (e.g. a mount point or another filesystem).
pub fn move_aside_and_delete(p: &Path) -> io::Result<()> {
    move_aside_into(p, p.parent().unwrap_or(Path::new(".")))
}

/// Like [`move_aside_and_delete`], with the temporary name in `parent` (same filesystem), so
/// that `p`'s own parent can be removed right away (a scope folder).
pub fn move_aside_into(p: &Path, parent: &Path) -> io::Result<()> {
    if kind(p) != Kind::Dir {
        return remove_any(p);
    }
    let aside = parent.join(format!("{MOVED_ASIDE_PREFIX}{:016x}", random_suffix()));
    match std::fs::rename(p, &aside) {
        Ok(()) => {
            delete_in_background(aside);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => remove_any(p),
    }
}

/// Lexically normalises an absolute or relative path (`.` dropped, `..` pops).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// The relative path from directory `from` to `to` (both absolute or both relative to the
/// same base; compared lexically).
pub fn relative(from: &Path, to: &Path) -> PathBuf {
    let from = normalize(from);
    let to = normalize(to);
    let fc: Vec<_> = from.components().collect();
    let tc: Vec<_> = to.components().collect();
    let common = fc.iter().zip(&tc).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..fc.len() {
        out.push("..");
    }
    for c in &tc[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `dir/name` where `name` may be scoped (`@s/n`), with `/` as the separator inside.
pub fn join_name(dir: &Path, name: &str) -> PathBuf {
    let mut p = dir.to_path_buf();
    for part in name.split('/') {
        p.push(part);
    }
    p
}

/// How [`ensure_symlink`] treats what is already at the destination (bun `Symlinker::Strategy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The destination's parent is new: just create, fix up on failure.
    ExpectMissing,
    /// Read the current link first; keep it when it already points at the target.
    ExpectExisting,
}

#[cfg(unix)]
fn symlink_dir_raw(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir_raw(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// Makes `dest` a symlink with the text `target`. Returns `Ok(true)` when something was
/// written. With [`Strategy::ExpectExisting`], a real directory that has a `package.json` is
/// kept (a `patch` checkout or a mount point), like bun. `real_base` is the deepest directory
/// of `dest` known to be a real directory; links planted in components below it are removed
/// so nothing is written through them.
pub fn ensure_symlink(
    target: &Path,
    dest: &Path,
    strategy: Strategy,
    real_base: Option<&Path>,
) -> io::Result<bool> {
    ensure_inner(target, dest, strategy, real_base, true)
}

fn create_link(target: &Path, dest: &Path) -> io::Result<()> {
    match symlink_dir_raw(target, dest) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dest.parent() {
                create_dir_all(parent)?;
            }
            symlink_dir_raw(target, dest)
        }
        r => r,
    }
}

fn ensure_inner(
    target: &Path,
    dest: &Path,
    strategy: Strategy,
    real_base: Option<&Path>,
    may_retry: bool,
) -> io::Result<bool> {
    if let (Some(base), Some(parent)) = (real_base, dest.parent()) {
        remove_planted_links(parent, base)?;
    }
    match strategy {
        Strategy::ExpectMissing => match symlink_dir_raw(target, dest) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = dest.parent() {
                    create_dir_all(parent)?;
                }
                symlink_dir_raw(target, dest).map(|()| true)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                remove_any(dest)?;
                symlink_dir_raw(target, dest).map(|()| true)
            }
            Err(e) => Err(e),
        },
        Strategy::ExpectExisting => match std::fs::read_link(dest) {
            Ok(current) => {
                if strip_trailing_sep(&current) == target {
                    return Ok(false);
                }
                remove_link(dest)?;
                create_link(target, dest).map(|()| true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound || is_errno(&e, &[libc::ENOTDIR]) => {
                match create_link(target, dest) {
                    Err(e) if may_retry && e.kind() == io::ErrorKind::AlreadyExists => {
                        ensure_inner(target, dest, strategy, None, false)
                    }
                    r => r.map(|()| true),
                }
            }
            Err(_) => {
                // A file or a directory.
                if dest.join("package.json").exists() && kind(dest) == Kind::Dir {
                    return Ok(false);
                }
                remove_any(dest)?;
                create_link(target, dest).map(|()| true)
            }
        },
    }
}

fn strip_trailing_sep(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    let t = s.trim_end_matches(['/', '\\']);
    if t.len() == s.len() || t.is_empty() {
        p.to_path_buf()
    } else {
        PathBuf::from(OsString::from(t))
    }
}

/// For each component of `path` below `real_base`: a symlink there came with the project (it
/// was not created by the installer), so it is removed rather than written through
/// (bun `prune::remove_planted_links`). `path` must start with `real_base`.
pub fn remove_planted_links(path: &Path, real_base: &Path) -> io::Result<()> {
    let Ok(rest) = path.strip_prefix(real_base) else {
        return Ok(());
    };
    let mut cur = real_base.to_path_buf();
    for c in rest.components() {
        cur.push(c.as_os_str());
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                remove_link(&cur)?;
                return Ok(());
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound || is_errno(&e, &[libc::ENOTDIR]) => {
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Atomically replaces the symlink at `dest` with one to `target` (write `<dest>.tmp`, rename).
pub fn replace_symlink_atomic(target: &Path, dest: &Path) -> io::Result<()> {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dest.with_file_name(format!("{STAGING_PREFIX}{name}-{:016x}", random_suffix()));
    symlink_dir_raw(target, &tmp)?;
    match std::fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use fnpm_store::IndexedFile;

    fn index(dirs: &[&str], files: &[&str]) -> FileIndex {
        FileIndex {
            dirs: dirs.iter().map(|s| (*s).to_owned()).collect(),
            files: files
                .iter()
                .map(|p| IndexedFile {
                    path: (*p).to_owned(),
                    size: 0,
                    mode: 0o644,
                })
                .collect(),
            package_json: None,
        }
    }

    fn src_pkg(root: &Path) -> PathBuf {
        let src = root.join("store/pkg");
        std::fs::create_dir_all(src.join("lib/deep")).unwrap();
        std::fs::write(
            src.join("package.json"),
            r#"{"name":"a","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(src.join("lib/index.js"), "x").unwrap();
        std::fs::write(src.join("lib/deep/d.js"), "d").unwrap();
        src
    }

    #[test]
    fn materialize_each_backend() {
        for backend in [
            Backend::Clonefile,
            Backend::Hardlink,
            Backend::Copyfile,
            Backend::Symlink,
        ] {
            let t = tempfile::tempdir().unwrap();
            let src = src_pkg(t.path());
            let idx = index(
                &["lib", "lib/deep"],
                &["package.json", "lib/index.js", "lib/deep/d.js"],
            );
            let m = Materializer::new(backend);
            let dest = t.path().join("nm/a");
            let used = m.materialize(&src, &idx, &dest).unwrap();
            assert_eq!(
                std::fs::read_to_string(dest.join("lib/deep/d.js")).unwrap(),
                "d"
            );
            assert!(m.used().is_some());
            if backend == Backend::Hardlink {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(used, Backend::Hardlink);
                let a = std::fs::metadata(src.join("lib/index.js")).unwrap().ino();
                let b = std::fs::metadata(dest.join("lib/index.js")).unwrap().ino();
                assert_eq!(a, b);
            }
            if backend == Backend::Symlink {
                assert_eq!(kind(&dest.join("package.json")), Kind::Symlink);
            }
        }
    }

    #[test]
    fn materialize_creates_missing_parent_dirs_not_in_index() {
        let t = tempfile::tempdir().unwrap();
        let src = src_pkg(t.path());
        let idx = index(&[], &["lib/deep/d.js"]);
        let dest = t.path().join("nm/b");
        Materializer::new(Backend::Hardlink)
            .materialize(&src, &idx, &dest)
            .unwrap();
        assert!(dest.join("lib/deep/d.js").exists());
    }

    #[test]
    fn materialize_missing_source_errors() {
        let t = tempfile::tempdir().unwrap();
        let src = src_pkg(t.path());
        let idx = index(&[], &["nope.js"]);
        let err =
            Materializer::new(Backend::Hardlink).materialize(&src, &idx, &t.path().join("nm/c"));
        assert!(err.is_err());
    }

    #[test]
    fn place_file_replaces_existing() {
        let t = tempfile::tempdir().unwrap();
        let src = src_pkg(t.path());
        let dst = t.path().join("x.js");
        std::fs::write(&dst, "old").unwrap();
        Materializer::new(Backend::Copyfile)
            .place_file(&src.join("lib/index.js"), &dst)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "x");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative(Path::new("/a/b/c"), Path::new("/a/d")),
            PathBuf::from("../../d")
        );
        assert_eq!(
            relative(
                Path::new("/a/node_modules"),
                Path::new("/a/node_modules/.fnpm/x/node_modules/x")
            ),
            PathBuf::from(".fnpm/x/node_modules/x")
        );
        assert_eq!(
            relative(Path::new("/a"), Path::new("/a")),
            PathBuf::from(".")
        );
        assert_eq!(
            relative(Path::new("/a/./b/../c"), Path::new("/a/c/d")),
            PathBuf::from("d")
        );
    }

    #[test]
    fn join_scoped_names() {
        assert_eq!(
            join_name(Path::new("/nm"), "@s/n"),
            PathBuf::from("/nm/@s/n")
        );
        assert_eq!(join_name(Path::new("/nm"), "n"), PathBuf::from("/nm/n"));
    }

    #[test]
    fn symlink_strategies() {
        let t = tempfile::tempdir().unwrap();
        let target_dir = t.path().join("target");
        std::fs::create_dir(&target_dir).unwrap();
        let dest = t.path().join("nm/@s/link");
        // Missing parent is created.
        assert!(
            ensure_symlink(
                Path::new("../../target"),
                &dest,
                Strategy::ExpectMissing,
                None
            )
            .unwrap()
        );
        assert!(dest.join(".").exists());
        // Same link: nothing written.
        assert!(
            !ensure_symlink(
                Path::new("../../target"),
                &dest,
                Strategy::ExpectExisting,
                None
            )
            .unwrap()
        );
        // Different link: replaced.
        assert!(
            ensure_symlink(
                Path::new("../../other"),
                &dest,
                Strategy::ExpectExisting,
                None
            )
            .unwrap()
        );
        assert_eq!(
            std::fs::read_link(&dest).unwrap(),
            PathBuf::from("../../other")
        );
        // ExpectMissing over an existing entry replaces it.
        assert!(
            ensure_symlink(
                Path::new("../../target"),
                &dest,
                Strategy::ExpectMissing,
                None
            )
            .unwrap()
        );
        assert_eq!(
            std::fs::read_link(&dest).unwrap(),
            PathBuf::from("../../target")
        );
        // An empty directory is replaced by the link.
        let d2 = t.path().join("nm/empty");
        std::fs::create_dir_all(&d2).unwrap();
        assert!(
            ensure_symlink(Path::new("../target"), &d2, Strategy::ExpectExisting, None).unwrap()
        );
        assert_eq!(kind(&d2), Kind::Symlink);
        // A directory with package.json is kept.
        let d3 = t.path().join("nm/patched");
        std::fs::create_dir_all(&d3).unwrap();
        std::fs::write(d3.join("package.json"), "{}").unwrap();
        assert!(
            !ensure_symlink(Path::new("../target"), &d3, Strategy::ExpectExisting, None).unwrap()
        );
        assert_eq!(kind(&d3), Kind::Dir);
        // A file is replaced.
        let f = t.path().join("nm/file");
        std::fs::write(&f, "x").unwrap();
        assert!(
            ensure_symlink(Path::new("../target"), &f, Strategy::ExpectExisting, None).unwrap()
        );
        assert_eq!(kind(&f), Kind::Symlink);
    }

    #[test]
    fn planted_links_are_removed_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let outside = t.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let nm = t.path().join("nm");
        std::fs::create_dir(&nm).unwrap();
        std::os::unix::fs::symlink(&outside, nm.join("@scope")).unwrap();
        let dest = nm.join("@scope/pkg");
        ensure_symlink(
            Path::new("../x"),
            &dest,
            Strategy::ExpectExisting,
            Some(&nm),
        )
        .unwrap();
        assert_eq!(kind(&nm.join("@scope")), Kind::Dir);
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn move_aside_deletes_in_background() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("pkg");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/c"), "x").unwrap();
        move_aside_and_delete(&d).unwrap();
        assert_eq!(kind(&d), Kind::Missing);
        flush_background_deletes();
        let left: Vec<_> = std::fs::read_dir(t.path()).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn remove_any_kinds() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        std::fs::write(&f, "x").unwrap();
        remove_any(&f).unwrap();
        remove_any(&f).unwrap();
        let l = t.path().join("l");
        std::os::unix::fs::symlink(t.path(), &l).unwrap();
        remove_any(&l).unwrap();
        assert!(t.path().exists());
    }

    #[test]
    fn leftovers() {
        assert!(is_leftover(".old_modules-abc"));
        assert!(is_leftover(".fnpm-staging-x"));
        assert!(is_leftover(".fnpm-old-1"));
        assert!(!is_leftover(".bin"));
    }

    #[test]
    fn atomic_replace() {
        let t = tempfile::tempdir().unwrap();
        let l = t.path().join("l");
        std::os::unix::fs::symlink("a", &l).unwrap();
        replace_symlink_atomic(Path::new("b"), &l).unwrap();
        assert_eq!(std::fs::read_link(&l).unwrap(), PathBuf::from("b"));
    }
}
