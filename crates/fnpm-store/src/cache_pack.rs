//! `pm cache pack <file>` / `pm cache unpack <file>`: a portable archive of store packages
//! (bun `install/cache_pack.rs`, in fnpm's own format).
//!
//! A pack is untrusted input: nothing is published until the whole pack has been read and
//! every checksum verified, an existing package is never replaced, and every path is checked
//! to stay inside its package.
//!
//! Format: the 16-byte magic, then records (little endian) `u8 kind | u32 path_len | path |
//! u32 mode | u64 size | size bytes`. A package record (kind 1, path = `pkgs/` directory
//! name, in ascending order, size = the length of its body) is followed in its body by one
//! integrity record (kind 6, data = SRI), file records (kind 2) and a checksum record (kind 5:
//! xxh3-64 of the name and the body before it). The pack ends with a kind 0 record.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fnpm_types::Integrity;
use xxhash_rust::xxh3::Xxh3;

use crate::extract::Built;
use crate::publish::Published;
use crate::staging::Staging;
use crate::{Error, IndexMeta, PackageKey, Result, Store};

const MAGIC: &[u8; 16] = b"FNPMCACHEPACKv1\n";
const KIND_END: u8 = 0;
const KIND_PACKAGE: u8 = 1;
const KIND_FILE: u8 = 2;
const KIND_CHECKSUM: u8 = 5;
const KIND_INTEGRITY: u8 = 6;
const MAX_PATH_LEN: usize = 4095;
const HEADER_LEN: u64 = 1 + 4 + 4 + 8;

/// What [`pack`] wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackSummary {
    /// Packages written.
    pub packages: u32,
    /// Files written.
    pub files: u64,
    /// File bytes written.
    pub bytes: u64,
    /// Requested packages that are not (completely) in the store.
    pub missing: u32,
}

/// What [`unpack`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnpackSummary {
    /// Packages in the pack.
    pub packages: u32,
    /// Packages added to the store.
    pub created: u32,
    /// Packages the store had already.
    pub already_cached: u32,
    /// Names where something the store would not accept exists; left alone.
    pub incomplete: u32,
    /// File bytes read.
    pub bytes: u64,
}

fn refuse(reason: &str, record: u64, path: &str) -> Error {
    let mut message = format!("{reason} (record #{record}");
    if !path.is_empty() {
        message.push_str(&format!(": \"{}\"", crate::tar::escape_control(path)));
    }
    message.push(')');
    Error::Refused { message }
}

/// Every path in a pack: relative, no empty, `.` or `..` component, no backslash or NUL.
fn is_safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0'])
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !(cfg!(windows) && c.contains(':')))
}

/// The key a `pkgs/` name stands for, if it is one fnpm writes for an unpatched package.
fn key_of(dir_name: &str, integrity: Integrity) -> Option<PackageKey> {
    if !dir_name.bytes().all(|b| b.is_ascii_graphic())
        || dir_name.contains('/')
        || dir_name.len() > 255
    {
        return None;
    }
    let at = dir_name[1..].find('@')? + 1;
    let (name, rest) = (&dir_name[..at], &dir_name[at + 1..]);
    let dash = rest.rfind('-')?;
    let version = &rest[..dash];
    let name = if name.starts_with('@') {
        name.replacen('+', "/", 1)
    } else {
        name.to_owned()
    };
    let key = PackageKey {
        name,
        version: version.to_owned(),
        integrity,
    };
    (key.dir_name() == dir_name).then_some(key)
}

struct Writer<W: Write> {
    out: W,
    hasher: Xxh3,
}

impl<W: Write> Writer<W> {
    fn bytes(&mut self, b: &[u8]) -> io::Result<()> {
        self.hasher.update(b);
        self.out.write_all(b)
    }

    fn header(&mut self, kind: u8, path: &str, mode: u32, size: u64) -> io::Result<()> {
        self.bytes(&[kind])?;
        self.bytes(&(path.len() as u32).to_le_bytes())?;
        self.bytes(path.as_bytes())?;
        self.bytes(&mode.to_le_bytes())?;
        self.bytes(&size.to_le_bytes())
    }
}

/// Writes the store packages of `keys` (missing ones are counted and skipped) to
/// `destination` through a temporary file and a rename. With `dry_run` every package is read
/// and checked but nothing is written.
pub fn pack(
    store: &Store,
    keys: &[PackageKey],
    destination: &Path,
    dry_run: bool,
) -> Result<PackSummary> {
    let mut keys: Vec<&PackageKey> = keys.iter().collect();
    keys.sort_by_key(|k| k.dir_name());
    keys.dedup_by_key(|k| k.dir_name());
    let io_dest = |source| Error::Io {
        path: destination.to_path_buf(),
        source,
    };
    let tmp = destination.with_file_name(format!(
        "{}.{}",
        destination
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        crate::sys::random_name("tmp-")
    ));
    let sink: Box<dyn Write> = if dry_run {
        Box::new(io::sink())
    } else {
        if let Some(parent) = destination.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(io_dest)?;
        }
        Box::new(BufWriter::with_capacity(
            1 << 20,
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(io_dest)?,
        ))
    };
    let mut w = Writer {
        out: sink,
        hasher: Xxh3::new(),
    };
    let result = (|| -> Result<PackSummary> {
        let mut summary = PackSummary::default();
        w.bytes(MAGIC).map_err(io_dest)?;
        for key in keys {
            let Some(entry) = store.get(key)? else {
                summary.missing += 1;
                continue;
            };
            let name = key.dir_name();
            let sri = key.integrity.to_sri();
            let mut body = HEADER_LEN + sri.len() as u64 + HEADER_LEN + 8;
            for f in &entry.index.files {
                if f.path.len() > MAX_PATH_LEN || !is_safe_relative_path(&f.path) {
                    return Err(Error::Refused {
                        message: format!(
                            "cache entry path cannot be packed: \"{name}/{}\"",
                            f.path
                        ),
                    });
                }
                body += HEADER_LEN + f.path.len() as u64 + f.size;
            }
            w.header(KIND_PACKAGE, &name, 0, body).map_err(io_dest)?;
            w.hasher = Xxh3::new();
            w.hasher.update(name.as_bytes());
            w.header(KIND_INTEGRITY, "", 0, sri.len() as u64)
                .map_err(io_dest)?;
            w.bytes(sri.as_bytes()).map_err(io_dest)?;
            for f in &entry.index.files {
                let path = entry.root.join(&f.path);
                let mut file = File::open(&path).map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })?;
                w.header(KIND_FILE, &f.path, f.mode, f.size)
                    .map_err(io_dest)?;
                let mut left = f.size;
                let mut buf = vec![0u8; 256 * 1024];
                while left > 0 {
                    let want = buf.len().min(left as usize);
                    let n = file.read(&mut buf[..want]).map_err(|source| Error::Io {
                        path: path.clone(),
                        source,
                    })?;
                    if n == 0 {
                        return Err(Error::Refused {
                            message: format!("file changed while packing: \"{name}/{}\"", f.path),
                        });
                    }
                    w.bytes(&buf[..n]).map_err(io_dest)?;
                    left -= n as u64;
                }
                summary.files += 1;
                summary.bytes += f.size;
            }
            w.header(KIND_CHECKSUM, "", 0, 8).map_err(io_dest)?;
            let sum = w.hasher.digest();
            w.bytes(&sum.to_le_bytes()).map_err(io_dest)?;
            summary.packages += 1;
        }
        w.header(KIND_END, "", 0, 0).map_err(io_dest)?;
        w.out.flush().map_err(io_dest)?;
        Ok(summary)
    })();
    drop(w);
    match result {
        Ok(summary) => {
            if !dry_run {
                std::fs::rename(&tmp, destination).map_err(io_dest)?;
            }
            Ok(summary)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

struct Reader {
    inner: BufReader<File>,
    len: u64,
    pos: u64,
    hasher: Xxh3,
    record: u64,
}

struct Header {
    kind: u8,
    path: String,
    mode: u32,
    size: u64,
}

impl Reader {
    fn exact(&mut self, out: &mut [u8]) -> Result<()> {
        self.inner
            .read_exact(out)
            .map_err(|_| refuse("truncated pack file", self.record, ""))?;
        self.hasher.update(out);
        self.pos += out.len() as u64;
        Ok(())
    }

    fn header(&mut self) -> Result<Header> {
        self.record += 1;
        let mut head = [0u8; 5];
        self.exact(&mut head)?;
        let path_len = u32::from_le_bytes(head[1..5].try_into().expect("4 bytes")) as usize;
        if path_len > MAX_PATH_LEN {
            return Err(refuse("path is too long", self.record, ""));
        }
        let mut path = vec![0u8; path_len];
        self.exact(&mut path)?;
        let path =
            String::from_utf8(path).map_err(|_| refuse("path is not UTF-8", self.record, ""))?;
        let mut mode = [0u8; 4];
        self.exact(&mut mode)?;
        let mut size = [0u8; 8];
        self.exact(&mut size)?;
        let size = u64::from_le_bytes(size);
        if size > self.len.saturating_sub(self.pos) {
            return Err(refuse("truncated pack file", self.record, &path));
        }
        Ok(Header {
            kind: head[0],
            path,
            mode: u32::from_le_bytes(mode),
            size,
        })
    }

    fn skip(&mut self, size: u64) -> Result<()> {
        let copied = io::copy(&mut (&mut self.inner).take(size), &mut io::sink())
            .map_err(|_| refuse("truncated pack file", self.record, ""))?;
        if copied != size {
            return Err(refuse("truncated pack file", self.record, ""));
        }
        self.pos += size;
        Ok(())
    }
}

struct Staged {
    key: PackageKey,
    staging: Arc<Staging>,
    built: Built,
}

/// Adds the packages of the pack at `pack_path` that the store does not have. With
/// `dry_run` the pack is fully verified but nothing is published.
pub fn unpack(store: &Store, pack_path: &Path, dry_run: bool) -> Result<UnpackSummary> {
    let file = File::open(pack_path).map_err(|source| Error::Io {
        path: pack_path.to_path_buf(),
        source,
    })?;
    let len = file
        .metadata()
        .map_err(|source| Error::Io {
            path: pack_path.to_path_buf(),
            source,
        })?
        .len();
    let mut r = Reader {
        inner: BufReader::with_capacity(1 << 20, file),
        len,
        pos: 0,
        hasher: Xxh3::new(),
        record: 0,
    };
    let mut magic = [0u8; 16];
    if r.exact(&mut magic).is_err() || &magic != MAGIC {
        return Err(refuse("not an fnpm cache pack", 0, ""));
    }
    let mut summary = UnpackSummary::default();
    let mut previous = String::new();
    let mut staged: Vec<Staged> = Vec::new();
    loop {
        let h = r.header()?;
        if h.mode != 0 {
            return Err(refuse("malformed record", r.record, &h.path));
        }
        match h.kind {
            KIND_PACKAGE => {}
            KIND_END if h.path.is_empty() && h.size == 0 && r.pos == r.len => break,
            KIND_END => return Err(refuse("trailing bytes after pack", r.record, "")),
            _ => return Err(refuse("record outside of a package", r.record, &h.path)),
        }
        if h.path <= previous {
            return Err(refuse("packages out of order", r.record, &h.path));
        }
        previous.clone_from(&h.path);
        summary.packages += 1;
        let end = r.pos + h.size;
        let name = h.path;
        r.hasher = Xxh3::new();
        r.hasher.update(name.as_bytes());

        let ih = r.header()?;
        if ih.kind != KIND_INTEGRITY || !ih.path.is_empty() || ih.size > 256 {
            return Err(refuse("missing integrity record", r.record, &name));
        }
        let mut sri = vec![0u8; ih.size as usize];
        r.exact(&mut sri)?;
        let integrity = std::str::from_utf8(&sri)
            .ok()
            .and_then(|s| Integrity::parse(s).ok())
            .ok_or_else(|| refuse("invalid integrity", r.record, &name))?;
        let key = key_of(&name, integrity)
            .ok_or_else(|| refuse("invalid cache entry name", r.record, &name))?;
        if r.pos > end {
            return Err(refuse("record runs past end of package", r.record, &name));
        }

        if store.get(&key)?.is_some() {
            summary.already_cached += 1;
            r.skip(end - r.pos)?;
            continue;
        }
        // A dry run reads and verifies everything but writes nothing, not even blobs.
        let staging = if dry_run {
            None
        } else {
            Some(store.new_staging()?)
        };
        let mut tracker = std::collections::HashSet::new();
        let mut dirs = std::collections::HashSet::new();
        let mut files = Vec::new();
        let mut package_json = None;
        loop {
            let fh = r.header()?;
            if r.pos > end || fh.size > end - r.pos {
                return Err(refuse(
                    "record runs past end of package",
                    r.record,
                    &fh.path,
                ));
            }
            if fh.kind == KIND_CHECKSUM {
                let computed = r.hasher.digest();
                if !fh.path.is_empty() || fh.size != 8 || end - r.pos != 8 {
                    return Err(refuse("malformed checksum record", r.record, &name));
                }
                let mut stored = [0u8; 8];
                r.exact(&mut stored)?;
                if u64::from_le_bytes(stored) != computed {
                    return Err(refuse("checksum mismatch", r.record, &name));
                }
                break;
            }
            if fh.kind != KIND_FILE {
                return Err(refuse("unknown kind of record", r.record, &fh.path));
            }
            if !is_safe_relative_path(&fh.path)
                || dirs.contains(fh.path.as_str())
                || !tracker.insert(fh.path.clone())
            {
                return Err(refuse("unsafe path", r.record, &fh.path));
            }
            for (i, _) in fh.path.match_indices('/') {
                let dir = &fh.path[..i];
                if tracker.contains(dir) {
                    return Err(refuse("unsafe path", r.record, &fh.path));
                }
                dirs.insert(dir.to_owned());
            }
            if fh.size > 1 << 32 {
                return Err(refuse("file is too large", r.record, &fh.path));
            }
            let mut data = vec![0u8; fh.size as usize];
            r.exact(&mut data)?;
            summary.bytes += fh.size;
            let Some(staging) = staging.as_ref() else {
                continue;
            };
            if let Some(i) = fh.path.rfind('/') {
                staging
                    .ensure_dir(&fh.path[..i])
                    .map_err(|source| Error::Io {
                        path: staging.package_root().to_path_buf(),
                        source,
                    })?;
            }
            if fh.path == "package.json" {
                package_json = Some(String::from_utf8_lossy(&data).into_owned());
            }
            let mode = if fh.mode & 0o111 != 0 { 0o755 } else { 0o644 };
            files.push(
                staging
                    .add_bytes(&fh.path, mode, &data)
                    .map_err(|source| Error::Io {
                        path: PathBuf::from(&fh.path),
                        source,
                    })?,
            );
        }
        let Some(staging) = staging else {
            summary.created += 1;
            continue;
        };
        staged.push(Staged {
            key,
            staging,
            built: Built {
                files,
                package_json,
            },
        });
    }
    for s in staged {
        let dest = store.package_dir(&s.key);
        let index = crate::FileIndex::from_files(s.built.files, s.built.package_json);
        let meta = IndexMeta {
            integrity: Some(s.key.integrity),
            patch_hash: None,
        };
        let index_path = s.staging.root().join("index.bin");
        std::fs::write(&index_path, index.to_bytes(&meta)).map_err(|source| Error::Io {
            path: index_path.clone(),
            source,
        })?;
        let published = crate::publish::publish(
            s.staging.root(),
            &dest,
            store.tuning().windows_rename_retry_ms,
            |d| {
                Store::read_entry(d, Some(s.key.integrity), None)
                    .ok()
                    .flatten()
            },
        );
        match published {
            Ok(Published::Ours) => {
                s.staging.mark_published();
                summary.created += 1;
            }
            Ok(Published::Theirs(_)) => summary.already_cached += 1,
            Err(_) => summary.incomplete += 1,
        }
    }
    Ok(summary)
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl PackSummary {
    /// bun's output (without colours): the summary line and the missing-packages warning.
    pub fn lines(&self, file: &str, dry_run: bool) -> Vec<String> {
        let mut out = vec![format!(
            "{} {} package{} ({} file{}, {}) into {}",
            if dry_run { "Would pack" } else { "Packed" },
            self.packages,
            plural(self.packages.into()),
            self.files,
            plural(self.files),
            crate::prune::format_size(self.bytes),
            crate::tar::escape_control(file)
        )];
        if self.missing > 0 {
            out.push(format!(
                "warn: skipping {} package{} not in cache (run fnpm install first)",
                self.missing,
                plural(self.missing.into())
            ));
        }
        out
    }
}

impl UnpackSummary {
    /// bun's output (without colours).
    pub fn lines(&self, dry_run: bool) -> Vec<String> {
        let mut out = vec![format!(
            "{} {} package{} ({} new, {} already cached, {})",
            if dry_run { "Would unpack" } else { "Unpacked" },
            self.packages,
            plural(self.packages.into()),
            self.created,
            self.already_cached,
            crate::prune::format_size(self.bytes)
        )];
        if self.incomplete > 0 {
            let one = self.incomplete == 1;
            out.push(format!(
                "warn: ignoring {} incomplete cache entr{}, fnpm install will re-download {}",
                self.incomplete,
                if one { "y" } else { "ies" },
                if one { "it" } else { "them" }
            ));
        }
        out
    }
}
