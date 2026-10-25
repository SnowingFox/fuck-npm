//! gzip -> tar -> staging package, buffered and streaming (bun `install/extract_tarball.rs`,
//! `install/TarballStream.rs`).
//!
//! The integrity hash covers the compressed bytes exactly as received, including anything
//! after the end of the archive. Extraction always runs to the end of the gzip stream so a
//! damaged trailer is noticed.
//!
//! A streamed (large) tarball is a pipeline: the thread that inflates and parses the tar hands
//! finished files to the rayon pool in batches, which write them while it inflates on, and the
//! SHA-512 of each compressed batch is computed beside the inflate. Duplicate entries of one
//! path are written after the batch that holds the earlier entry, so the last one still wins.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;

use fnpm_types::{HashAlgorithm, Integrity};

use crate::hash::IntegrityHasher;
use rayon::prelude::*;

use crate::gzip::{self, GzipStream};
use crate::index::IndexedFile;
use crate::staging::{ContentKey, LargeFile, Staging};
use crate::tar::{EntryKind, TarEvent, TarParser};
use crate::{Error, PackageKey, Result};

/// In the buffered path, a package with a file at least this large, or with at least
/// [`PARALLEL_TOTAL_MIN`] bytes in all, has its files hashed and stored on the rayon pool.
const PARALLEL_FILE_MIN: usize = 64 * 1024;
/// See [`PARALLEL_FILE_MIN`].
const PARALLEL_TOTAL_MIN: usize = 256 * 1024;
/// Fewer files than this are always stored on the calling thread.
const PARALLEL_MIN_FILES: usize = 2;
/// Files per rayon task, so tiny files are not scheduled one by one.
const PARALLEL_CHUNK: usize = 4;
/// In the streaming path, finished files are handed to the pool in batches of this many bytes
/// (or [`STREAM_BATCH_FILES`] files), whichever comes first.
const STREAM_BATCH_BYTES: usize = 256 * 1024;
/// See [`STREAM_BATCH_BYTES`].
const STREAM_BATCH_FILES: usize = 32;
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

    /// Errors with [`Error::Integrity`] when the integrity is verified and `computed` differs.
    pub(crate) fn check(&self, computed: Option<&Integrity>) -> Result<()> {
        if let Self::Pinned { key, verify: true } = self
            && let Some(actual) = computed
            && *actual != key.integrity
        {
            return Err(Error::Integrity {
                package: key.name.clone(),
                expected: key.integrity.to_sri(),
                actual: actual.to_sri(),
            });
        }
        Ok(())
    }

    /// The key the package is stored under, checking the integrity when asked to.
    pub(crate) fn resolve(self, computed: Option<Integrity>) -> Result<PackageKey> {
        self.check(computed.as_ref())?;
        match self {
            Self::Pinned { key, .. } => Ok(key),
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

    // Identical files (same mode, size, xxh3-128 and bytes) share an inode: the first one is
    // written, the others are hard linked to it once it exists.
    let mut first_of: HashMap<ContentKey, usize> = HashMap::with_capacity(files.len());
    let mut copy_of: Vec<Option<usize>> = vec![None; files.len()];
    for (i, f) in files.iter().enumerate() {
        match first_of.entry(ContentKey::of(&f.data, f.mode)) {
            Entry::Occupied(o) => {
                if files[*o.get()].data == f.data {
                    copy_of[i] = Some(*o.get());
                }
            }
            Entry::Vacant(v) => {
                v.insert(i);
            }
        }
    }
    let write = |f: &RawFile<'_>| {
        staging
            .write_file(&f.path, f.mode, &f.data)
            .map_err(io_at(staging, &f.path))
    };
    let firsts: Vec<&RawFile<'_>> = files
        .iter()
        .zip(&copy_of)
        .filter(|(_, c)| c.is_none())
        .map(|(f, _)| f)
        .collect();
    // Writes are independent per file. Small packages stay on this thread (a rayon hand-off
    // costs more than it saves); larger ones fan out over the pool.
    let total: usize = files.iter().map(|f| f.data.len()).sum();
    if firsts.len() >= PARALLEL_MIN_FILES
        && (total >= PARALLEL_TOTAL_MIN || files.iter().any(|f| f.data.len() >= PARALLEL_FILE_MIN))
    {
        firsts
            .par_iter()
            .with_min_len(PARALLEL_CHUNK)
            .try_for_each(|f| write(f))?;
    } else {
        firsts.iter().try_for_each(|f| write(f))?;
    }
    for (f, first) in files.iter().zip(&copy_of) {
        if let Some(first) = *first
            && staging.link_to(&files[first].path, &f.path).is_err()
        {
            write(f)?;
        }
    }
    let records = files
        .into_iter()
        .map(|f| IndexedFile {
            size: f.data.len() as u64,
            mode: f.mode,
            path: f.path,
        })
        .collect();
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
    /// The path had an earlier entry: written after the earlier one is (see [`FileSink`]).
    duplicate: bool,
    /// For a large duplicate, the temporary name it streams to.
    temp: Option<String>,
}

/// A finished file waiting to be written.
struct Ready {
    idx: usize,
    path: String,
    mode: u32,
    data: Vec<u8>,
}

/// Work that must wait until the batches in flight are written.
enum Deferred {
    Write(Ready),
    /// A large duplicate streamed to `temp`: renamed over `path`.
    Rename {
        idx: usize,
        temp: String,
        path: String,
        size: u64,
        mode: u32,
    },
}

type Done = Arc<Mutex<Vec<(usize, Result<IndexedFile>)>>>;

/// Receives tar events and stores files as they complete: in batches on the rayon pool, while
/// the tar keeps being inflated.
struct FileSink {
    staging: Arc<Staging>,
    current: Option<Current>,
    files: Vec<Option<IndexedFile>>,
    /// Entries replaced by a later entry of the same path (their records are dropped).
    replaced: HashSet<usize>,
    tracker: PathTracker,
    package_json: Option<String>,
    batch: Vec<Ready>,
    batch_bytes: usize,
    deferred: Vec<Deferred>,
    done: Done,
}

fn store_ready(staging: &Staging, r: &Ready) -> Result<IndexedFile> {
    staging
        .add_bytes(&r.path, r.mode, &r.data)
        .map_err(io_at(staging, &r.path))
}

impl FileSink {
    fn on_event<'s>(&mut self, event: TarEvent<'_>, scope: &rayon::Scope<'s>) -> Result<()> {
        match event {
            TarEvent::Entry(h) if h.kind == EntryKind::File => {
                let idx = self.files.len();
                let replaced = self.tracker.add(&h.path, idx)?;
                if let Some(old) = replaced {
                    self.files[old] = None;
                    self.replaced.insert(old);
                }
                self.files.push(None);
                let staging = &self.staging;
                staging
                    .ensure_dir(parent_of(&h.path))
                    .map_err(io_at(staging, &h.path))?;
                let duplicate = replaced.is_some();
                let mut temp = None;
                let data = if h.size > LARGE_FILE {
                    let name = if duplicate {
                        let t = format!("{}.{}", h.path, crate::sys::random_name("dup-"));
                        temp = Some(t.clone());
                        t
                    } else {
                        h.path.clone()
                    };
                    Pending::Disk(Box::new(
                        staging
                            .begin_large(&name, h.mode)
                            .map_err(io_at(staging, &name))?,
                    ))
                } else {
                    Pending::Mem(Vec::with_capacity(h.size as usize))
                };
                self.current = Some(Current {
                    idx,
                    path: h.path,
                    mode: h.mode,
                    data,
                    duplicate,
                    temp,
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
                    match cur.data {
                        Pending::Mem(v) => {
                            if cur.path == "package.json" {
                                self.package_json = Some(String::from_utf8_lossy(&v).into_owned());
                            }
                            let ready = Ready {
                                idx: cur.idx,
                                path: cur.path,
                                mode: cur.mode,
                                data: v,
                            };
                            if cur.duplicate {
                                self.deferred.push(Deferred::Write(ready));
                            } else {
                                self.batch_bytes += ready.data.len();
                                self.batch.push(ready);
                                if self.batch_bytes >= STREAM_BATCH_BYTES
                                    || self.batch.len() >= STREAM_BATCH_FILES
                                {
                                    self.dispatch(scope);
                                }
                            }
                        }
                        Pending::Disk(f) => {
                            let name = cur.temp.as_deref().unwrap_or(&cur.path);
                            let record = self.staging.finish_large(name, cur.mode, *f);
                            match cur.temp {
                                Some(temp) => self.deferred.push(Deferred::Rename {
                                    idx: cur.idx,
                                    temp,
                                    path: cur.path,
                                    size: record.size,
                                    mode: cur.mode,
                                }),
                                None => self.files[cur.idx] = Some(record),
                            }
                        }
                    }
                }
            }
            TarEvent::Entry(_) | TarEvent::Symlink { .. } => {}
        }
        Ok(())
    }

    /// Hands the current batch to the pool.
    fn dispatch<'s>(&mut self, scope: &rayon::Scope<'s>) {
        if self.batch.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.batch);
        self.batch_bytes = 0;
        let (staging, done) = (self.staging.clone(), self.done.clone());
        scope.spawn(move |_| {
            let results: Vec<(usize, Result<IndexedFile>)> = batch
                .iter()
                .map(|r| (r.idx, store_ready(&staging, r)))
                .collect();
            done.lock().extend(results);
        });
    }

    /// After the batches of a feed are written: records their results and runs the deferred
    /// work in order.
    fn settle(&mut self) -> Result<()> {
        let results = std::mem::take(&mut *self.done.lock());
        let mut first_err = None;
        for (idx, r) in results {
            match r {
                Ok(record) if !self.replaced.contains(&idx) => self.files[idx] = Some(record),
                Ok(_) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        for d in std::mem::take(&mut self.deferred) {
            match d {
                Deferred::Write(r) => {
                    let record = store_ready(&self.staging, &r)?;
                    if !self.replaced.contains(&r.idx) {
                        self.files[r.idx] = Some(record);
                    }
                }
                Deferred::Rename {
                    idx,
                    temp,
                    path,
                    size,
                    mode,
                } => {
                    let root = self.staging.package_root();
                    std::fs::rename(root.join(&temp), root.join(&path)).map_err(|source| {
                        Error::Io {
                            path: root.join(&path),
                            source,
                        }
                    })?;
                    if !self.replaced.contains(&idx) {
                        self.files[idx] = Some(IndexedFile { path, size, mode });
                    }
                }
            }
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
                replaced: HashSet::new(),
                tracker: PathTracker::default(),
                package_json: None,
                batch: Vec::new(),
                batch_bytes: 0,
                deferred: Vec::new(),
                done: Arc::new(Mutex::new(Vec::new())),
            },
        }
    }

    pub(crate) fn staging(&self) -> &Arc<Staging> {
        &self.sink.staging
    }

    /// Hashes and extracts `chunk`; returns once every file it completed is written.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<()> {
        let Self {
            hasher,
            decoder,
            tar,
            sink,
        } = self;
        match hasher.as_mut() {
            Some(h) if chunk.len() >= PARALLEL_HASH_MIN => {
                rayon::join(|| h.update(chunk), || decode(decoder, tar, sink, chunk)).1
            }
            Some(h) => {
                h.update(chunk);
                decode(decoder, tar, sink, chunk)
            }
            None => decode(decoder, tar, sink, chunk),
        }
    }

    /// Ends the input; returns the integrity of everything fed (when hashing) and the files.
    pub(crate) fn finish(mut self) -> Result<(Option<Integrity>, Built)> {
        if let Decoder::Detect(head) = &mut self.decoder {
            let head = std::mem::take(head);
            self.decoder = Decoder::Plain;
            decode(&mut self.decoder, &mut self.tar, &mut self.sink, &head)?;
        }
        if let Decoder::Gzip(g) = &mut self.decoder {
            let tar = &mut self.tar;
            let sink = &mut self.sink;
            let r = rayon::scope(|s| {
                let r = g.finish(&mut |out| tar.feed_with(out, |ev| sink.on_event(ev, s)));
                sink.dispatch(s);
                r
            });
            let settled = sink.settle();
            r?;
            settled?;
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

/// Inflates (when gzip) and untars `data` into `sink`, writing the files it completes on the
/// pool; returns when they are written.
fn decode(
    decoder: &mut Decoder,
    tar: &mut TarParser,
    sink: &mut FileSink,
    data: &[u8],
) -> Result<()> {
    if let Decoder::Detect(head) = decoder {
        head.extend_from_slice(data);
        if head.len() < 2 {
            return Ok(());
        }
        let head = std::mem::take(head);
        *decoder = if gzip::is_gzip(&head) {
            Decoder::Gzip(Box::new(GzipStream::new()))
        } else {
            Decoder::Plain
        };
        return decode(decoder, tar, sink, &head);
    }
    let r = rayon::scope(|s| {
        let r = match decoder {
            Decoder::Gzip(g) => g.feed(data, &mut |out| {
                tar.feed_with(out, |ev| sink.on_event(ev, s))
            }),
            Decoder::Plain => tar.feed_with(data, |ev| sink.on_event(ev, s)),
            Decoder::Detect(_) => unreachable!("decided before decoding"),
        };
        sink.dispatch(s);
        r
    });
    let settled = sink.settle();
    r?;
    settled
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
