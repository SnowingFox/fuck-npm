//! gzip -> tar -> CAS, buffered and streaming (bun `install/extract_tarball.rs`,
//! `install/TarballStream.rs`).
//!
//! The integrity hash covers the compressed bytes exactly as received, including anything
//! after the end of the archive. Extraction always runs to the end of the gzip stream so a
//! damaged trailer is noticed.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fnpm_types::{HashAlgorithm, Integrity};

use crate::hash::IntegrityHasher;
use rayon::prelude::*;

use crate::cas::{LargeFile, Staging};
use crate::gzip::{self, GzipStream};
use crate::index::IndexedFile;
use crate::tar::{EntryKind, TarEvent, TarParser};
use crate::{Error, PackageKey, Result};

/// Files at least this large are hashed and stored on the rayon pool in the buffered path.
const PARALLEL_FILE_MIN: usize = 64 * 1024;
/// In the streaming path, files larger than this are written to disk while they arrive
/// instead of being held in memory.
const LARGE_FILE: u64 = 8 * 1024 * 1024;
/// Hashing the compressed bytes runs beside extraction from this size on.
const PARALLEL_HASH_MIN: usize = 128 * 1024;

/// What an import is for.
#[derive(Debug, Clone)]
pub(crate) enum Target {
    /// The integrity is known; `verify` compares it with the bytes.
    Pinned { key: PackageKey, verify: bool },
    /// No integrity is known (a URL or `file:` tarball): SHA-512 is computed and becomes the key.
    Unpinned { name: String, version: String },
}

impl Target {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Pinned { key, .. } => &key.name,
            Self::Unpinned { name, .. } => name,
        }
    }

    pub(crate) fn hasher(&self) -> Option<IntegrityHasher> {
        match self {
            Self::Pinned { key, verify: true } => {
                Some(IntegrityHasher::new(key.integrity.algorithm()))
            }
            Self::Pinned { verify: false, .. } => None,
            Self::Unpinned { .. } => Some(IntegrityHasher::new(HashAlgorithm::Sha512)),
        }
    }

    /// The key the package is stored under, checking the integrity when asked to.
    pub(crate) fn resolve(self, computed: Option<Integrity>) -> Result<PackageKey> {
        match self {
            Self::Pinned { key, verify } => {
                if verify
                    && let Some(actual) = computed
                    && actual != key.integrity
                {
                    return Err(Error::Integrity {
                        package: key.name,
                        expected: key.integrity.to_sri(),
                        actual: actual.to_sri(),
                    });
                }
                Ok(key)
            }
            Self::Unpinned { name, version } => {
                let integrity = computed.expect("unpinned imports always hash");
                Ok(PackageKey {
                    name,
                    version,
                    integrity,
                })
            }
        }
    }
}

/// The files of an extracted package, before it is indexed.
#[derive(Debug, Default)]
pub(crate) struct Built {
    pub(crate) files: Vec<IndexedFile>,
    pub(crate) package_json: Option<String>,
}

/// Fills an error from the tar or gzip layer with the package name.
pub(crate) fn with_package(err: Error, package: &str) -> Error {
    match err {
        Error::InvalidTarball {
            package: p,
            message,
        } if p.is_empty() => Error::InvalidTarball {
            package: package.to_owned(),
            message,
        },
        Error::PathTraversal { package: p, path } if p.is_empty() => Error::PathTraversal {
            package: package.to_owned(),
            path,
        },
        e => e,
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidTarball {
        package: String::new(),
        message: message.into(),
    }
}

fn io_at(staging: &Staging, rel: &str) -> impl Fn(std::io::Error) -> Error {
    let path = staging.package_root().join(rel);
    move |source| Error::Io {
        path: path.clone(),
        source,
    }
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Tracks file and directory paths so a name used as both is refused, and so the last of
/// duplicate entries wins.
#[derive(Default)]
struct PathTracker {
    files: HashMap<String, usize>,
    dirs: HashSet<String>,
}

impl PathTracker {
    /// Records `path` as file number `idx`; returns the number of the entry it replaces.
    fn add(&mut self, path: &str, idx: usize) -> Result<Option<usize>> {
        if self.dirs.contains(path) {
            return Err(invalid(format!(
                "\"{path}\" is both a file and a directory"
            )));
        }
        let mut end = 0;
        while let Some(i) = path[end..].find('/') {
            end += i;
            let dir = &path[..end];
            if self.files.contains_key(dir) {
                return Err(invalid(format!("\"{dir}\" is both a file and a directory")));
            }
            if !self.dirs.contains(dir) {
                self.dirs.insert(dir.to_owned());
            }
            end += 1;
        }
        Ok(self.files.insert(path.to_owned(), idx))
    }
}

struct RawFile<'a> {
    path: String,
    mode: u32,
    data: Cow<'a, [u8]>,
}

/// Extracts a whole tar held in memory: entries are parsed first (file bytes are not
/// copied), then hashed and stored, the large ones in parallel.
pub(crate) fn build_from_tar(staging: &Staging, tar: &[u8]) -> Result<Built> {
    let mut parser = TarParser::new();
    let mut files: Vec<Option<RawFile<'_>>> = Vec::new();
    let mut tracker = PathTracker::default();
    let mut current: Option<RawFile<'_>> = None;
    parser.feed_with(tar, |event| {
        match event {
            TarEvent::Entry(h) if h.kind == EntryKind::File => {
                current = Some(RawFile {
                    path: h.path,
                    mode: h.mode,
                    data: Cow::Borrowed(&[]),
                });
            }
            TarEvent::Data(d) => {
                if let Some(cur) = current.as_mut() {
                    if cur.data.is_empty() {
                        cur.data = Cow::Borrowed(d);
                    } else {
                        cur.data.to_mut().extend_from_slice(d);
                    }
                }
            }
            TarEvent::EntryEnd => {
                if let Some(cur) = current.take() {
                    if let Some(old) = tracker.add(&cur.path, files.len())? {
                        files[old] = None;
                    }
                    files.push(Some(cur));
                }
            }
            TarEvent::Entry(_) | TarEvent::Symlink { .. } => {}
        }
        Ok(())
    })?;
    parser.finish()?;
    let files: Vec<RawFile<'_>> = files.into_iter().flatten().collect();

    let mut dirs: Vec<String> = tracker.dirs.into_iter().collect();
    dirs.sort_unstable();
    staging.create_dirs(&dirs).map_err(|source| Error::Io {
        path: staging.package_root().to_path_buf(),
        source,
    })?;

    let package_json = files
        .iter()
        .find(|f| f.path == "package.json")
        .map(|f| String::from_utf8_lossy(&f.data).into_owned());
    let store_one = |f: &RawFile<'_>| {
        staging
            .add_bytes(&f.path, f.mode, &f.data)
            .map_err(io_at(staging, &f.path))
    };
    let (big, small): (Vec<&RawFile<'_>>, Vec<&RawFile<'_>>) = files
        .iter()
        .partition(|f| f.data.len() >= PARALLEL_FILE_MIN);
    let mut records: Vec<IndexedFile> = Vec::with_capacity(files.len());
    if big.len() > 1 {
        let (big_records, small_records) = rayon::join(
            || {
                big.par_iter()
                    .map(|f| store_one(f))
                    .collect::<Result<Vec<_>>>()
            },
            || {
                small
                    .iter()
                    .map(|f| store_one(f))
                    .collect::<Result<Vec<_>>>()
            },
        );
        records.extend(big_records?);
        records.extend(small_records?);
    } else {
        for f in files.iter() {
            records.push(store_one(f)?);
        }
    }
    Ok(Built {
        files: records,
        package_json,
    })
}

enum Decoder {
    /// Not enough bytes yet to tell gzip from a plain tar.
    Detect(Vec<u8>),
    Gzip(Box<GzipStream>),
    Plain,
}

enum Pending {
    Mem(Vec<u8>),
    Disk(Box<LargeFile>),
}

struct Current {
    idx: usize,
    path: String,
    mode: u32,
    data: Pending,
}

/// Receives tar events and stores files as they complete.
struct FileSink {
    staging: Arc<Staging>,
    current: Option<Current>,
    files: Vec<Option<IndexedFile>>,
    tracker: PathTracker,
    package_json: Option<String>,
}

impl FileSink {
    fn on_event(&mut self, event: TarEvent<'_>) -> Result<()> {
        match event {
            TarEvent::Entry(h) if h.kind == EntryKind::File => {
                let idx = self.files.len();
                if let Some(old) = self.tracker.add(&h.path, idx)? {
                    self.files[old] = None;
                }
                self.files.push(None);
                let staging = &self.staging;
                staging
                    .ensure_dir(parent_of(&h.path))
                    .map_err(io_at(staging, &h.path))?;
                let data = if h.size > LARGE_FILE {
                    Pending::Disk(Box::new(
                        staging
                            .begin_large(&h.path, h.mode)
                            .map_err(io_at(staging, &h.path))?,
                    ))
                } else {
                    Pending::Mem(Vec::with_capacity(h.size as usize))
                };
                self.current = Some(Current {
                    idx,
                    path: h.path,
                    mode: h.mode,
                    data,
                });
            }
            TarEvent::Data(d) => {
                if let Some(cur) = self.current.as_mut() {
                    match &mut cur.data {
                        Pending::Mem(v) => v.extend_from_slice(d),
                        Pending::Disk(f) => f.write(d).map_err(io_at(&self.staging, &cur.path))?,
                    }
                }
            }
            TarEvent::EntryEnd => {
                if let Some(cur) = self.current.take() {
                    let staging = &self.staging;
                    let record = match cur.data {
                        Pending::Mem(v) => {
                            if cur.path == "package.json" {
                                self.package_json = Some(String::from_utf8_lossy(&v).into_owned());
                            }
                            staging
                                .add_bytes(&cur.path, cur.mode, &v)
                                .map_err(io_at(staging, &cur.path))?
                        }
                        Pending::Disk(f) => staging
                            .finish_large(&cur.path, cur.mode, *f)
                            .map_err(io_at(staging, &cur.path))?,
                    };
                    self.files[cur.idx] = Some(record);
                }
            }
            TarEvent::Entry(_) | TarEvent::Symlink { .. } => {}
        }
        Ok(())
    }
}

/// A resumable extractor: feed compressed chunks as they arrive, then [`finish`].
///
/// [`finish`]: StreamExtractor::finish
pub(crate) struct StreamExtractor {
    hasher: Option<IntegrityHasher>,
    decoder: Decoder,
    tar: TarParser,
    sink: FileSink,
    bytes_in: u64,
}

impl StreamExtractor {
    pub(crate) fn new(staging: Arc<Staging>, hasher: Option<IntegrityHasher>) -> Self {
        Self {
            hasher,
            decoder: Decoder::Detect(Vec::new()),
            tar: TarParser::new(),
            sink: FileSink {
                staging,
                current: None,
                files: Vec::new(),
                tracker: PathTracker::default(),
                package_json: None,
            },
            bytes_in: 0,
        }
    }

    pub(crate) fn staging(&self) -> &Arc<Staging> {
        &self.sink.staging
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<()> {
        if let Some(h) = self.hasher.as_mut() {
            h.update(chunk);
        }
        self.bytes_in += chunk.len() as u64;
        if let Decoder::Detect(head) = &mut self.decoder {
            head.extend_from_slice(chunk);
            if head.len() < 2 {
                return Ok(());
            }
            let head = std::mem::take(head);
            self.decoder = if gzip::is_gzip(&head) {
                Decoder::Gzip(Box::new(GzipStream::new()))
            } else {
                Decoder::Plain
            };
            return self.decode(&head);
        }
        self.decode(chunk)
    }

    fn decode(&mut self, data: &[u8]) -> Result<()> {
        let tar = &mut self.tar;
        let sink = &mut self.sink;
        match &mut self.decoder {
            Decoder::Gzip(g) => g.feed(data, &mut |out| tar.feed_with(out, |ev| sink.on_event(ev))),
            Decoder::Plain => tar.feed_with(data, |ev| sink.on_event(ev)),
            Decoder::Detect(_) => unreachable!("decided before decoding"),
        }
    }

    /// Ends the input; returns the integrity of everything fed (when hashing) and the files.
    pub(crate) fn finish(mut self) -> Result<(Option<Integrity>, Built)> {
        if let Decoder::Detect(head) = &mut self.decoder {
            let head = std::mem::take(head);
            self.decoder = Decoder::Plain;
            self.decode(&head)?;
        }
        if let Decoder::Gzip(g) = &mut self.decoder {
            let tar = &mut self.tar;
            let sink = &mut self.sink;
            g.finish(&mut |out| tar.feed_with(out, |ev| sink.on_event(ev)))?;
        }
        self.tar.finish()?;
        let computed = self.hasher.map(IntegrityHasher::finish);
        let sink = self.sink;
        Ok((
            computed,
            Built {
                files: sink.files.into_iter().flatten().collect(),
                package_json: sink.package_json,
            },
        ))
    }
}

/// Extracts `tgz` held in memory: libdeflate one-shot when it applies, else the streaming
/// decoder over the buffer (multi-member or large bodies).
pub(crate) fn extract_buffer(staging: &Arc<Staging>, tgz: &[u8]) -> Result<Built> {
    if gzip::is_gzip(tgz) {
        if let Some(tar) = gzip::inflate_oneshot(tgz) {
            let built = build_from_tar(staging, &tar);
            gzip::recycle(tar);
            return built;
        }
        let mut x = StreamExtractor::new(staging.clone(), None);
        for chunk in tgz.chunks(1 << 20) {
            x.feed(chunk)?;
        }
        return Ok(x.finish()?.1);
    }
    build_from_tar(staging, tgz)
}

/// Hashes and extracts a buffered body, side by side when it is large enough.
pub(crate) fn hash_and_extract(
    staging: &Arc<Staging>,
    hasher: Option<IntegrityHasher>,
    tgz: &[u8],
) -> (Option<Integrity>, Result<Built>) {
    let hash = |h: Option<IntegrityHasher>| {
        h.map(|mut h| {
            h.update(tgz);
            h.finish()
        })
    };
    if hasher.is_some() && tgz.len() >= PARALLEL_HASH_MIN {
        rayon::join(|| hash(hasher), || extract_buffer(staging, tgz))
    } else {
        let built = extract_buffer(staging, tgz);
        (hash(hasher), built)
    }
}
