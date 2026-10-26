//! Writing the tarball (bun `pack_command.rs`): pax-restricted ustar entries with mode
//! normalised by the caller, mtime 1985-10-26T08:15:00Z, uid/gid 0, `package/` prefix, gzip
//! without a timestamp (OS "unknown"), no padding past the two end blocks. A file destination
//! is written through a temporary file next to it and renamed over it, so a failed pack never
//! leaves a truncated tarball; a destination that cannot be replaced (a FIFO, a device, a
//! read-only file, a symlink `--filename` names) is written in place at the end.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha1::Digest as _;

use super::out::{FResult, Out, show};

/// `1985-10-26T08:15:00.000Z` (pacote's `tar-create-options.js`).
pub(crate) const NPM_MTIME: u64 = 499_162_500;

/// PATH_MAX of bun's path buffers.
#[cfg(target_os = "linux")]
pub(crate) const MAX_PATH_BYTES: usize = 4096;
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) const MAX_PATH_BYTES: usize = 1024;
#[cfg(not(unix))]
pub(crate) const MAX_PATH_BYTES: usize = 32767 * 3 + 1;

/// Counts and hashes the compressed bytes on their way to the sink, and keeps the sink's
/// error so a failed write can be told from a failed read.
pub(crate) struct HashingSink {
    sink: Sink,
    pub packed_size: u64,
    sha1: sha1::Sha1,
    sha512: sha2::Sha512,
    pub write_error: Option<io::Error>,
}

pub(crate) enum Sink {
    File(std::fs::File),
    Memory(Vec<u8>),
}

impl Write for HashingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = match &mut self.sink {
            Sink::File(f) => f.write_all(buf),
            Sink::Memory(v) => {
                v.extend_from_slice(buf);
                Ok(())
            }
        };
        if let Err(e) = result {
            let copy = io::Error::new(e.kind(), e.to_string());
            self.write_error = Some(e);
            return Err(copy);
        }
        self.packed_size += buf.len() as u64;
        self.sha1.update(buf);
        self.sha512.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A tar writer over gzip over a [`HashingSink`].
pub(crate) struct TarWriter {
    gz: flate2::write::GzEncoder<HashingSink>,
}

/// What a finished tarball was.
pub(crate) struct Finished {
    pub sink: Sink,
    pub packed_size: u64,
    pub shasum: [u8; 20],
    pub integrity: [u8; 64],
}

/// Why writing an entry failed.
pub(crate) enum EntryError {
    /// Writing the tarball (the sink's error).
    Write(io::Error),
    /// Reading the file, with the syscall.
    Read(io::Error, &'static str),
    /// The file is shorter than its size when it was opened.
    Shrank,
}

fn octal(dst: &mut [u8], value: u64) {
    let width = dst.len() - 1;
    let s = format!("{value:0width$o}");
    let bytes = s.as_bytes();
    let start = bytes.len().saturating_sub(width);
    dst[..width].copy_from_slice(&bytes[start..]);
    dst[width] = 0;
}

fn split_ustar(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() <= 100 {
        return Some((&[], path));
    }
    if path.len() > 256 {
        return None;
    }
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
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name[..n]);
    octal(&mut h[100..108], u64::from(mode & 0o7777));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size.min(0o777_7777_7777));
    octal(&mut h[136..148], NPM_MTIME);
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    octal(&mut h[329..337], 0);
    octal(&mut h[337..345], 0);
    let p = prefix.len().min(155);
    h[345..345 + p].copy_from_slice(&prefix[..p]);
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

const ZEROES: [u8; 512] = [0u8; 512];

impl TarWriter {
    pub fn new(sink: Sink, level: u32) -> Self {
        let hashing = HashingSink {
            sink,
            packed_size: 0,
            sha1: sha1::Sha1::new(),
            sha512: sha2::Sha512::new(),
            write_error: None,
        };
        let gz = flate2::GzBuilder::new()
            .mtime(0)
            .operating_system(255)
            .write(hashing, flate2::Compression::new(level.min(9)));
        Self { gz }
    }

    fn take_error(&mut self, fallback: io::Error) -> io::Error {
        self.gz.get_mut().write_error.take().unwrap_or(fallback)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), EntryError> {
        match self.gz.write_all(bytes) {
            Ok(()) => Ok(()),
            Err(e) => Err(EntryError::Write(self.take_error(e))),
        }
    }

    fn pad(&mut self, len: u64) -> Result<(), EntryError> {
        let n = ((512 - len % 512) % 512) as usize;
        self.put(&ZEROES[..n])
    }

    fn entry_header(&mut self, path: &[u8], mode: u32, size: u64) -> Result<(), EntryError> {
        let full = [b"package/".as_slice(), path].concat();
        let needs_pax_size = size > 0o777_7777_7777;
        let split = if full.is_ascii() {
            split_ustar(&full)
        } else {
            None
        };
        if split.is_none() || needs_pax_size {
            let mut records = Vec::new();
            if split.is_none() {
                records.extend(pax_record("path", &full));
            }
            if needs_pax_size {
                records.extend(pax_record("size", size.to_string().as_bytes()));
            }
            let base = full.rsplit(|&b| b == b'/').next().unwrap_or(&full);
            let mut pax_name = [b"package/PaxHeader/".as_slice(), base].concat();
            pax_name.truncate(99);
            self.put(&header(&pax_name, b"", 0o644, records.len() as u64, b'x'))?;
            self.put(&records)?;
            self.pad(records.len() as u64)?;
        }
        let (prefix, name): (&[u8], &[u8]) = split.unwrap_or((&[], &full[..full.len().min(100)]));
        self.put(&header(name, prefix, mode, size, b'0'))
    }

    /// An entry with `bytes` as its contents.
    pub fn add_bytes(&mut self, path: &[u8], mode: u32, bytes: &[u8]) -> Result<(), EntryError> {
        self.entry_header(path, mode, bytes.len() as u64)?;
        self.put(bytes)?;
        self.pad(bytes.len() as u64)
    }

    /// An entry of `size` bytes read from `file`. Bytes past `size` are dropped; a file that
    /// is shorter by then is an error. Returns the bytes written.
    pub fn add_file(
        &mut self,
        path: &[u8],
        mode: u32,
        size: u64,
        file: &mut std::fs::File,
        buf: &mut [u8],
    ) -> Result<u64, EntryError> {
        use std::io::Read;
        self.entry_header(path, mode, size)?;
        let mut written = 0u64;
        loop {
            let n = match file.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(EntryError::Read(e, "read")),
            };
            let take = (size - written).min(n as u64) as usize;
            if take > 0 {
                self.put(&buf[..take])?;
                written += take as u64;
            }
        }
        if written < size {
            return Err(EntryError::Shrank);
        }
        self.pad(size)?;
        Ok(written)
    }

    /// The end blocks, then the gzip trailer.
    pub fn finish(mut self) -> Result<Finished, io::Error> {
        if let Err(e) = self.gz.write_all(&[0u8; 1024]) {
            return Err(self.take_error(e));
        }
        let mut sink = match self.gz.try_finish() {
            Ok(()) => self.gz.finish()?,
            Err(e) => return Err(self.take_error(e)),
        };
        let shasum: [u8; 20] = std::mem::take(&mut sink.sha1).finalize().into();
        let integrity: [u8; 64] = std::mem::take(&mut sink.sha512).finalize().into();
        Ok(Finished {
            packed_size: sink.packed_size,
            shasum,
            integrity,
            sink: sink.sink,
        })
    }
}

/// bun `join_abs_string_buf`: `part` against `cwd` (unless absolute), `.` and `..` resolved,
/// no trailing slash. `None` if the result does not fit bun's path buffer.
pub(crate) fn join_abs(cwd: &[u8], part: &[u8]) -> Option<Vec<u8>> {
    let joined = if part.first() == Some(&b'/') {
        part.to_vec()
    } else {
        [cwd, b"/", part].concat()
    };
    let mut parts: Vec<&[u8]> = Vec::new();
    for seg in joined.split(|&b| b == b'/') {
        match seg {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut out = Vec::with_capacity(joined.len());
    for p in &parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    (out.len() <= MAX_PATH_BYTES).then_some(out)
}

fn without_trailing_slash(b: &[u8]) -> &[u8] {
    let mut b = b;
    while b.len() > 1 && b.last() == Some(&b'/') {
        b = &b[..b.len() - 1];
    }
    b
}

/// The tarball path, and the length of the directory to create (0 for `--filename`).
pub(crate) fn tarball_destination(
    out: &Out,
    destination: &[u8],
    filename: &[u8],
    workspace_dir: &[u8],
    tarball_name: &str,
) -> FResult<(Vec<u8>, usize)> {
    if !filename.is_empty() {
        if filename.len() + 1 > MAX_PATH_BYTES {
            return Err(out.error(&format!(
                "archive filename too long: \"{}\"",
                show(filename)
            )));
        }
        return Ok((filename.to_vec(), 0));
    }
    let too_long = |dir: &[u8]| {
        out.error(&format!(
            "archive destination name too long: \"{}/{tarball_name}\"",
            show(without_trailing_slash(dir))
        ))
    };
    let Some(dir) = join_abs(workspace_dir, destination) else {
        return Err(too_long(destination));
    };
    let trimmed = without_trailing_slash(&dir);
    let full = [trimmed, b"/", tarball_name.as_bytes()].concat();
    if full.len() + 1 > MAX_PATH_BYTES {
        return Err(too_long(&dir));
    }
    let dir_len = dir.len();
    Ok((full, dir_len))
}

/// `<scope>-<name>-<version>.tgz` (bun `TarballNameStyle::Normalize`).
pub(crate) fn tarball_name(name: &str, version: &str) -> String {
    let mut name = name;
    let mut scope = String::new();
    if let Some(rest) = name.strip_prefix('@') {
        name = rest;
        if let Some((s, unscoped)) = rest.split_once('/') {
            scope = format!("{s}-");
            name = unscoped;
        }
    }
    format!("{scope}{name}-{version}.tgz")
}

/// The temporary file next to the destination; removed when dropped unless kept.
pub(crate) struct TempTarball {
    pub path: PathBuf,
    keep: bool,
}

impl Drop for TempTarball {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn temp_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed =
        nanos ^ (u64::from(std::process::id()) << 32) ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!(".fnpm-pack.{mixed:016x}.tmp")
}

#[cfg(unix)]
fn is_writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(not(unix))]
fn is_writable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.permissions().readonly())
}

/// Creates the temporary file the tarball is streamed into, with the mode of the file it
/// replaces. `None` when the destination is something that must be written in place.
pub(crate) fn open_tarball(
    dest: &Path,
    typed_filename: bool,
) -> Option<(std::fs::File, TempTarball)> {
    let mode: u32 = match std::fs::symlink_metadata(dest) {
        Err(_) => 0o666,
        Ok(meta) => {
            let t = meta.file_type();
            if t.is_file() && is_writable(dest) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o777
                }
                #[cfg(not(unix))]
                {
                    0o666
                }
            } else if t.is_symlink() && !typed_filename {
                0o666
            } else {
                return None;
            }
        }
    };
    let dir = dest.parent().unwrap_or(Path::new("/"));
    let tmp = dir.join(temp_name());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let _ = mode;
    let file = options.open(&tmp).ok()?;
    Some((
        file,
        TempTarball {
            path: tmp,
            keep: false,
        },
    ))
}

/// Renames the temporary file over the destination, or copies it in place when the
/// destination cannot be replaced.
pub(crate) fn finish_tarball(
    out: &Out,
    file: std::fs::File,
    mut tmp: TempTarball,
    dest: &Path,
    typed_filename: bool,
) -> FResult<()> {
    drop(file);
    if std::fs::rename(&tmp.path, dest).is_ok() {
        tmp.keep = true;
        return Ok(());
    }
    let bytes = std::fs::read(&tmp.path);
    drop(tmp);
    match bytes {
        Ok(bytes) => write_tarball_in_place(out, dest, &bytes, typed_filename),
        Err(e) => Err(fail_write_tarball(out, &e, dest)),
    }
}

pub(crate) fn fail_write_tarball(out: &Out, e: &io::Error, dest: &Path) -> super::out::Fatal {
    out.sys(
        e,
        "write",
        &format!("failed to write tarball \"{}\"", dest.display()),
    )
}

pub(crate) fn write_tarball_in_place(
    out: &Out,
    dest: &Path,
    bytes: &[u8],
    typed_filename: bool,
) -> FResult<()> {
    #[cfg(not(unix))]
    let _ = typed_filename;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o666);
        if !typed_filename {
            options.custom_flags(libc::O_NOFOLLOW);
        }
    }
    let mut file = options.open(dest).map_err(|e| {
        out.sys(
            &e,
            "open",
            &format!(
                "failed to open tarball file destination: \"{}\"",
                dest.display()
            ),
        )
    })?;
    if let Err(e) = file.write_all(bytes) {
        drop(file);
        if std::fs::symlink_metadata(dest).is_ok_and(|m| m.file_type().is_file()) {
            let _ = std::fs::remove_file(dest);
        }
        return Err(fail_write_tarball(out, &e, dest));
    }
    Ok(())
}

/// A write past `ulimit -f` must fail with `EFBIG` instead of killing the process.
pub(crate) fn ignore_sigxfsz() {
    #[cfg(unix)]
    // SAFETY: setting a signal's disposition to ignore is async-signal-safe and has no
    // preconditions.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(tarball_name("@scoped/pkg", "1.1.1"), "scoped-pkg-1.1.1.tgz");
        assert_eq!(tarball_name("@", "1.1.1"), "-1.1.1.tgz");
        assert_eq!(tarball_name("@/", "1.1.1"), "--1.1.1.tgz");
        assert_eq!(tarball_name("//", "1.1.1"), "//-1.1.1.tgz");
        assert_eq!(tarball_name("@/s", "1.1.1"), "-s-1.1.1.tgz");
        assert_eq!(tarball_name("@s", "1.1.1"), "s-1.1.1.tgz");
    }

    #[test]
    fn joins() {
        assert_eq!(join_abs(b"/a/b", b"../c/").unwrap(), b"/a/c");
        assert_eq!(join_abs(b"/a", b"/x/./y").unwrap(), b"/x/y");
        assert_eq!(join_abs(b"/a", b"").unwrap(), b"/a");
        let long = vec![b'd'; MAX_PATH_BYTES + 1];
        assert!(join_abs(b"/a", &long).is_none());
        assert!(join_abs(b"/a", &[long.as_slice(), b"/.."].concat()).is_some());
    }

    #[test]
    fn unpadded_and_stable() {
        let write = || {
            let mut w = TarWriter::new(Sink::Memory(Vec::new()), 9);
            w.add_bytes(b"package.json", 0o644, b"{}").ok().unwrap();
            w.add_bytes(b"index.js", 0o644, b"x").ok().unwrap();
            let f = w.finish().unwrap();
            let Sink::Memory(bytes) = f.sink else {
                panic!()
            };
            (bytes, f.shasum)
        };
        let (a, sa) = write();
        let (b, sb) = write();
        assert_eq!((a.clone(), sa), (b, sb));
        let mut tar = Vec::new();
        use std::io::Read;
        flate2::read::GzDecoder::new(&a[..])
            .read_to_end(&mut tar)
            .unwrap();
        assert_eq!(tar.len(), 6 * 512);
        assert_eq!(&tar[..20], b"package/package.json");
    }
}
