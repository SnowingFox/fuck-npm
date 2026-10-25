//! A small streaming tar reader for npm tarballs (ustar, GNU long names, pax `path`).
//!
//! It reads archives the way bun reads them through its patched libarchive
//! (`patches/libarchive/tar-npm-compat.patch`, `install/TarballStream.rs`,
//! `libarchive/lib.rs`):
//!
//! - Entries are relativised: the first path component (`package/` by convention, any name
//!   accepted) is stripped. In npm mode a leading `/` counts as an empty first component, as
//!   in npm.
//! - In npm mode only regular files (types `0`, NUL and `7`) are kept; a name with a `..`
//!   component is skipped, a pax `path` with a NUL in it is skipped, and two zero blocks end
//!   the archive (nothing after them is read). Directories, links and devices are skipped.
//! - In generic mode ([`TarOptions::generic`], for non-npm archives) directories and symlinks
//!   are reported too, `..` is resolved lexically and an entry that would climb above the
//!   root is skipped; zero blocks are skipped so concatenated archives are read.
//! - A header block with a bad checksum is skipped (libarchive's damaged-block retry) and
//!   resets any pending pax / GNU long-name state.
//! - Pax keys starting with an upper-case letter (vendor keys such as `GNU.sparse.*`) are
//!   ignored in npm mode; old GNU sparse maps are not read, so a sparse member is a skipped
//!   entry (type `S`) or, in pax format 1.0, a file holding the stored bytes, as npm does.
//! - File modes are normalised to `0o755` (any execute bit set) or `0o644`.

use crate::{Error, Result};

/// The tar block size.
pub const BLOCK_SIZE: usize = 512;

/// The longest entry path (after stripping the first component) that is extracted; longer
/// ones are skipped with a warning, like bun's `PathBuffer` check.
#[cfg(target_os = "macos")]
pub const MAX_ENTRY_PATH_BYTES: usize = 1024;
/// The longest entry path (after stripping the first component) that is extracted; longer
/// ones are skipped with a warning, like bun's `PathBuffer` check.
#[cfg(windows)]
pub const MAX_ENTRY_PATH_BYTES: usize = 32767;
/// The longest entry path (after stripping the first component) that is extracted; longer
/// ones are skipped with a warning, like bun's `PathBuffer` check.
#[cfg(not(any(target_os = "macos", windows)))]
pub const MAX_ENTRY_PATH_BYTES: usize = 4096;

/// pax / GNU long-name payloads larger than this are refused.
const MAX_META_SIZE: u64 = 8 << 20;

/// What a reported entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file; its bytes follow as [`TarEvent::Data`].
    File,
    /// A directory (generic mode only).
    Directory,
}

/// A file or directory entry, with its sanitised path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryHeader {
    /// `/`-separated, relative to the package root, normalised, never empty.
    pub path: String,
    /// File or directory.
    pub kind: EntryKind,
    /// `0o755` or `0o644` for files; `0o755` for directories.
    pub mode: u32,
    /// Bytes of data that follow (0 for directories).
    pub size: u64,
}

/// How an archive is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TarOptions {
    /// Read like npm (the default): files only, `..` names skipped, two zero blocks end the
    /// archive. `false` is the generic mode used for archives that are not npm packages.
    pub npm: bool,
}

impl Default for TarOptions {
    fn default() -> Self {
        Self::npm()
    }
}

impl TarOptions {
    /// npm package tarballs.
    pub const fn npm() -> Self {
        Self { npm: true }
    }

    /// Any other archive: directories and symlinks are reported, concatenated archives read.
    pub const fn generic() -> Self {
        Self { npm: false }
    }
}

/// Events produced by [`TarParser::feed`].
#[derive(Debug, PartialEq, Eq)]
pub enum TarEvent<'a> {
    /// A file (or, in generic mode, directory) entry starts.
    Entry(EntryHeader),
    /// Bytes of the current file, in order. A file may be split over several events.
    Data(&'a [u8]),
    /// The current entry is complete.
    EntryEnd,
    /// A symbolic link (generic mode only). `target` is not validated; see
    /// [`is_symlink_target_safe`].
    Symlink {
        /// Sanitised path of the link.
        path: String,
        /// The link text as stored.
        target: String,
    },
}

#[derive(Debug, Clone, Copy)]
enum State {
    Header,
    Data {
        remaining: u64,
        pad: u64,
        emit: bool,
    },
    Pad {
        remaining: u64,
    },
    Meta {
        kind: u8,
        remaining: u64,
        pad: u64,
    },
    PastEnd,
}

const SEEN_X: u8 = 1;
const SEEN_G: u8 = 2;
const SEEN_L: u8 = 4;
const SEEN_K: u8 = 8;
const SEEN_A: u8 = 16;

#[derive(Debug, Default)]
struct Meta {
    pax_path: Option<Vec<u8>>,
    pax_linkpath: Option<Vec<u8>>,
    pax_size: Option<u64>,
    long_name: Option<Vec<u8>>,
    long_link: Option<Vec<u8>>,
    seen: u8,
    pax: bool,
}

/// Push parser: feed decompressed bytes as they arrive, receive entries and file bytes.
pub struct TarParser {
    opts: TarOptions,
    state: State,
    block: Box<[u8; BLOCK_SIZE]>,
    filled: usize,
    meta: Meta,
    meta_buf: Vec<u8>,
    null_blocks: u8,
    saw_archive: bool,
    offset: u64,
    warnings: Vec<String>,
}

impl std::fmt::Debug for TarParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TarParser")
            .field("state", &self.state)
            .field("offset", &self.offset)
            .finish()
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidTarball {
        package: String::new(),
        message: message.into(),
    }
}

#[inline]
fn pad_of(size: u64) -> u64 {
    (BLOCK_SIZE as u64 - size % BLOCK_SIZE as u64) % BLOCK_SIZE as u64
}

impl TarParser {
    /// A parser in npm mode.
    pub fn new() -> Self {
        Self::with_options(TarOptions::npm())
    }

    /// A parser with explicit options.
    pub fn with_options(opts: TarOptions) -> Self {
        Self {
            opts,
            state: State::Header,
            block: Box::new([0; BLOCK_SIZE]),
            filled: 0,
            meta: Meta::default(),
            meta_buf: Vec::new(),
            null_blocks: 0,
            saw_archive: false,
            offset: 0,
            warnings: Vec::new(),
        }
    }

    /// Consumes `input`, calling `on_event` for every header, data chunk and entry end.
    pub fn feed(
        &mut self,
        input: &[u8],
        on_event: &mut dyn FnMut(TarEvent<'_>) -> Result<()>,
    ) -> Result<()> {
        self.feed_with(input, on_event)
    }

    /// [`TarParser::feed`] with a statically dispatched callback whose `Data` slices borrow
    /// `input` for `'a`, so a caller holding the whole archive can keep them.
    pub fn feed_with<'a, F>(&mut self, mut input: &'a [u8], mut on_event: F) -> Result<()>
    where
        F: FnMut(TarEvent<'a>) -> Result<()>,
    {
        while !input.is_empty() {
            match self.state {
                State::PastEnd => {
                    self.offset += input.len() as u64;
                    return Ok(());
                }
                State::Header => {
                    if self.filled == 0 && input.len() >= BLOCK_SIZE {
                        let (block, rest) = input.split_at(BLOCK_SIZE);
                        input = rest;
                        self.offset += BLOCK_SIZE as u64;
                        let block: &[u8; BLOCK_SIZE] =
                            block.try_into().expect("split_at gives a full block");
                        self.process_header(block, &mut on_event)?;
                    } else {
                        let n = (BLOCK_SIZE - self.filled).min(input.len());
                        self.block[self.filled..self.filled + n].copy_from_slice(&input[..n]);
                        self.filled += n;
                        self.offset += n as u64;
                        input = &input[n..];
                        if self.filled == BLOCK_SIZE {
                            self.filled = 0;
                            let block = *self.block;
                            self.process_header(&block, &mut on_event)?;
                        }
                    }
                }
                State::Data {
                    remaining,
                    pad,
                    emit,
                } => {
                    let n = remaining.min(input.len() as u64) as usize;
                    if emit && n > 0 {
                        on_event(TarEvent::Data(&input[..n]))?;
                    }
                    input = &input[n..];
                    self.offset += n as u64;
                    let remaining = remaining - n as u64;
                    if remaining == 0 {
                        if emit {
                            on_event(TarEvent::EntryEnd)?;
                        }
                        self.state = if pad > 0 {
                            State::Pad { remaining: pad }
                        } else {
                            State::Header
                        };
                    } else {
                        self.state = State::Data {
                            remaining,
                            pad,
                            emit,
                        };
                    }
                }
                State::Pad { remaining } => {
                    let n = remaining.min(input.len() as u64);
                    input = &input[n as usize..];
                    self.offset += n;
                    let remaining = remaining - n;
                    self.state = if remaining == 0 {
                        State::Header
                    } else {
                        State::Pad { remaining }
                    };
                }
                State::Meta {
                    kind,
                    remaining,
                    pad,
                } => {
                    let n = remaining.min(input.len() as u64) as usize;
                    self.meta_buf.extend_from_slice(&input[..n]);
                    input = &input[n..];
                    self.offset += n as u64;
                    let remaining = remaining - n as u64;
                    if remaining == 0 {
                        self.apply_meta(kind);
                        self.state = if pad > 0 {
                            State::Pad { remaining: pad }
                        } else {
                            State::Header
                        };
                    } else {
                        self.state = State::Meta {
                            kind,
                            remaining,
                            pad,
                        };
                    }
                }
            }
        }
        Ok(())
    }

    /// Errors if the archive ended inside an entry, or if nothing in it was a tar block.
    pub fn finish(self) -> Result<()> {
        match self.state {
            State::PastEnd => Ok(()),
            State::Header if self.filled == 0 => {
                if self.saw_archive {
                    Ok(())
                } else {
                    Err(invalid("Unrecognized archive format"))
                }
            }
            _ => Err(invalid("Truncated tar archive")),
        }
    }

    /// True once the end-of-archive marker (two zero blocks, npm mode) was read.
    pub fn is_past_end(&self) -> bool {
        matches!(self.state, State::PastEnd)
    }

    /// Decompressed bytes consumed so far.
    pub fn bytes_consumed(&self) -> u64 {
        self.offset
    }

    /// Warnings about skipped entries (over-long paths, malformed pax records) since the last
    /// call.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    fn process_header<'a, F>(&mut self, block: &[u8; BLOCK_SIZE], on_event: &mut F) -> Result<()>
    where
        F: FnMut(TarEvent<'a>) -> Result<()>,
    {
        if block.iter().all(|&b| b == 0) {
            self.saw_archive = true;
            self.null_blocks = self.null_blocks.saturating_add(1);
            if self.opts.npm && self.null_blocks >= 2 {
                self.state = State::PastEnd;
            }
            return Ok(());
        }
        self.null_blocks = 0;
        if !checksum_ok(block) {
            // Damaged block: skipped, and whatever pax / long-name state was pending is dropped.
            self.meta = Meta::default();
            return Ok(());
        }
        self.saw_archive = true;
        let typeflag = block[156];
        let size = parse_number(&block[124..136]);
        if size < 0 {
            return Err(invalid("Tar entry has negative size"));
        }
        let size = size as u64;

        let meta_bit = match typeflag {
            b'x' | b'X' => Some(SEEN_X),
            b'g' => Some(SEEN_G),
            b'L' => Some(SEEN_L),
            b'K' => Some(SEEN_K),
            b'A' => Some(SEEN_A),
            b'V' => Some(0),
            _ => None,
        };
        if let Some(bit) = meta_bit {
            if bit != 0 {
                if self.meta.seen & bit != 0 {
                    return Err(invalid(format!("Redundant '{}' header", typeflag as char)));
                }
                self.meta.seen |= bit;
            }
            if size > MAX_META_SIZE {
                return Err(invalid("Special header too large"));
            }
            self.meta_buf.clear();
            if size == 0 {
                self.apply_meta(typeflag);
                self.state = State::Header;
            } else {
                self.state = State::Meta {
                    kind: typeflag,
                    remaining: size,
                    pad: pad_of(size),
                };
            }
            return Ok(());
        }
        self.regular_entry(block, typeflag, size, on_event)
    }

    fn regular_entry<'a, F>(
        &mut self,
        block: &[u8; BLOCK_SIZE],
        typeflag: u8,
        size: u64,
        on_event: &mut F,
    ) -> Result<()>
    where
        F: FnMut(TarEvent<'a>) -> Result<()>,
    {
        let meta = std::mem::take(&mut self.meta);
        let npm = self.opts.npm;
        let is_gnu = &block[257..265] == b"ustar  \0";
        let is_ustar =
            !is_gnu && &block[257..262] == b"ustar" && (!npm || &block[257..265] == b"ustar\x0000");
        let name = cstr(&block[0..100]);
        let mut pax_nul = false;
        let mut path: Vec<u8> = if let Some(p) = meta.pax_path {
            if let Some(nul) = p.iter().position(|&b| b == 0) {
                pax_nul = true;
                if npm { p } else { p[..nul].to_vec() }
            } else {
                p
            }
        } else if let Some(long) = meta.long_name {
            long
        } else if is_ustar {
            let prefix = cstr(&block[345..500]);
            if prefix.is_empty() {
                name.to_vec()
            } else {
                [prefix, b"/", name].concat()
            }
        } else {
            name.to_vec()
        };
        let perm = (parse_number(&block[100..108]) as u64 & 0o7777) as u32;
        let size = meta.pax_size.unwrap_or(size);
        let trailing_slash = path.last() == Some(&b'/');

        #[derive(PartialEq)]
        enum Kind {
            File,
            Dir,
            Symlink,
            Skip,
        }
        // (kind, bytes of data that follow, whether padding follows them)
        let (mut kind, data_len) = match typeflag {
            b'0' | 0 => (Kind::File, size),
            b'7' => (Kind::File, size),
            // A hard link only carries data in a pax archive.
            b'1' => (Kind::Skip, if meta.pax { size } else { 0 }),
            b'2' => (Kind::Symlink, 0),
            b'3' | b'4' | b'6' => (Kind::Skip, 0),
            b'5' => (Kind::Dir, 0),
            // GNU dumpdir: a directory with a listing as its data.
            b'D' => (Kind::Dir, size),
            // libarchive reads every other type as a regular file; npm keeps only 0, NUL, 7.
            _ => (if npm { Kind::Skip } else { Kind::File }, size),
        };
        let mut data_len = data_len;
        let mut pad = pad_of(data_len);
        // A "regular" entry whose name ends in `/` is a directory (old tar programs); its size
        // field is ignored and no data is consumed, as libarchive does.
        if kind == Kind::File && trailing_slash && (!npm || matches!(typeflag, b'0' | 0)) {
            kind = Kind::Dir;
            data_len = 0;
            pad = 0;
        }
        if npm && (kind != Kind::File || pax_nul) {
            kind = Kind::Skip;
        }
        let skip_state = |data_len: u64, pad: u64| {
            if data_len == 0 {
                if pad > 0 {
                    State::Pad { remaining: pad }
                } else {
                    State::Header
                }
            } else {
                State::Data {
                    remaining: data_len,
                    pad,
                    emit: false,
                }
            }
        };

        let checked = match kind {
            Kind::Skip => {
                self.state = skip_state(data_len, pad);
                return Ok(());
            }
            _ => check_path(&path, npm),
        };
        let clean = match checked {
            PathCheck::Ok(clean) => clean,
            PathCheck::Skip => {
                self.state = skip_state(data_len, pad);
                return Ok(());
            }
            PathCheck::TooLong => {
                path.truncate(200);
                self.warnings.push(format!(
                    "Skipping entry with a path longer than the maximum path length: {}",
                    escape_control(&String::from_utf8_lossy(&path))
                ));
                self.state = skip_state(data_len, pad);
                return Ok(());
            }
        };
        match kind {
            Kind::File => {
                let mode = if perm & 0o111 != 0 { 0o755 } else { 0o644 };
                on_event(TarEvent::Entry(EntryHeader {
                    path: clean,
                    kind: EntryKind::File,
                    mode,
                    size: data_len,
                }))?;
                if data_len == 0 {
                    on_event(TarEvent::EntryEnd)?;
                    self.state = if pad > 0 {
                        State::Pad { remaining: pad }
                    } else {
                        State::Header
                    };
                } else {
                    self.state = State::Data {
                        remaining: data_len,
                        pad,
                        emit: true,
                    };
                }
            }
            Kind::Dir => {
                on_event(TarEvent::Entry(EntryHeader {
                    path: clean,
                    kind: EntryKind::Directory,
                    mode: 0o755,
                    size: 0,
                }))?;
                on_event(TarEvent::EntryEnd)?;
                self.state = skip_state(data_len, pad);
            }
            Kind::Symlink => {
                let target = meta
                    .pax_linkpath
                    .or(meta.long_link)
                    .unwrap_or_else(|| cstr(&block[157..257]).to_vec());
                let target = String::from_utf8_lossy(cstr(&target)).into_owned();
                on_event(TarEvent::Symlink {
                    path: clean,
                    target,
                })?;
                self.state = skip_state(data_len, pad);
            }
            Kind::Skip => unreachable!("handled above"),
        }
        Ok(())
    }

    fn apply_meta(&mut self, kind: u8) {
        let data = std::mem::take(&mut self.meta_buf);
        match kind {
            b'x' | b'X' => self.parse_pax(&data),
            b'L' => self.meta.long_name = Some(cstr(&data).to_vec()),
            b'K' => self.meta.long_link = Some(cstr(&data).to_vec()),
            _ => {}
        }
        self.meta_buf = data;
        self.meta_buf.clear();
    }

    fn parse_pax(&mut self, data: &[u8]) {
        self.meta.pax = true;
        let mut rest = data;
        while !rest.is_empty() {
            let mut len: usize = 0;
            let mut i = 0;
            loop {
                let Some(&c) = rest.get(i) else {
                    return self.malformed_pax();
                };
                i += 1;
                if c == b' ' {
                    break;
                }
                if !c.is_ascii_digit() {
                    return self.malformed_pax();
                }
                len = len * 10 + usize::from(c - b'0');
                if len > 999_999 {
                    self.warnings
                        .push("Rejecting pax extended attribute > 1MB".to_owned());
                    return;
                }
            }
            if len > rest.len() || len < 1 || i > len || rest[len - 1] != b'\n' {
                return self.malformed_pax();
            }
            let record = &rest[i..len - 1];
            let Some(eq) = record.iter().position(|&b| b == b'=') else {
                return self.malformed_pax();
            };
            self.pax_attribute(&record[..eq], &record[eq + 1..]);
            rest = &rest[len..];
        }
    }

    fn malformed_pax(&mut self) {
        self.warnings
            .push("Ignoring malformed pax extended attributes".to_owned());
    }

    fn pax_attribute(&mut self, key: &[u8], value: &[u8]) {
        // npm reads none of the vendor keys; they all start with a capital letter.
        if self.opts.npm && key.first().is_some_and(u8::is_ascii_uppercase) {
            return;
        }
        match key {
            b"path" => self.meta.pax_path = Some(value.to_vec()),
            b"linkpath" => self.meta.pax_linkpath = Some(value.to_vec()),
            b"size" => {
                let digits = value.iter().take_while(|b| b.is_ascii_digit());
                let mut n: u64 = 0;
                for &d in digits {
                    n = n.saturating_mul(10).saturating_add(u64::from(d - b'0'));
                }
                self.meta.pax_size = Some(n);
            }
            _ => {}
        }
    }
}

impl Default for TarParser {
    fn default() -> Self {
        Self::new()
    }
}

/// The bytes before the first NUL.
fn cstr(field: &[u8]) -> &[u8] {
    match field.iter().position(|&b| b == 0) {
        Some(i) => &field[..i],
        None => field,
    }
}

/// libarchive `tar_atol`: octal (leading spaces / tabs skipped, stops at the first non-digit)
/// or GNU base-256 when the high bit of the first byte is set.
fn parse_number(field: &[u8]) -> i64 {
    let Some(&first) = field.first() else {
        return 0;
    };
    if first & 0x80 != 0 {
        // Base-256: the remaining bits of the first byte, then every other byte, big-endian.
        let negative = first & 0x40 != 0;
        let mut value: i128 = i128::from(first & 0x3f);
        if negative {
            value -= 0x40;
        }
        for &b in &field[1..] {
            value = value.saturating_mul(256).saturating_add(i128::from(b));
            if value > i128::from(i64::MAX) {
                return i64::MAX;
            }
            if value < i128::from(i64::MIN) {
                return i64::MIN;
            }
        }
        return value as i64;
    }
    let mut s = field;
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    let negative = if let [b'-', rest @ ..] = s {
        s = rest;
        true
    } else {
        false
    };
    let mut value: i64 = 0;
    for &b in s {
        if !(b'0'..=b'7').contains(&b) {
            break;
        }
        value = value.saturating_mul(8).saturating_add(i64::from(b - b'0'));
    }
    if negative { -value } else { value }
}

/// libarchive `checksum`: the stored value may be the unsigned or the signed byte sum.
fn checksum_ok(block: &[u8; BLOCK_SIZE]) -> bool {
    let mut unsigned: i64 = 0;
    let mut signed: i64 = 0;
    for (i, &b) in block.iter().enumerate() {
        let b = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += i64::from(b);
        signed += i64::from(b as i8);
    }
    let stored = parse_number(&block[148..156]);
    stored == unsigned || stored == signed
}

enum PathCheck {
    Ok(String),
    Skip,
    TooLong,
}

/// Strips the first component. `None` when the name has no component at all.
fn strip_first_component(raw: &[u8], npm: bool) -> Option<&[u8]> {
    let mut s = raw;
    if !npm {
        while let [b'/', rest @ ..] = s {
            s = rest;
        }
    }
    if s.is_empty() {
        return None;
    }
    s = match s.iter().position(|&b| b == b'/') {
        Some(i) => &s[i + 1..],
        None => &[],
    };
    while let [b'/', rest @ ..] = s {
        s = rest;
    }
    Some(s)
}

fn is_sep_any(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

#[cfg(windows)]
fn is_sep(b: u8) -> bool {
    is_sep_any(b)
}

#[cfg(not(windows))]
fn is_sep(b: u8) -> bool {
    b == b'/'
}

fn check_path(raw: &[u8], npm: bool) -> PathCheck {
    let Some(rest) = strip_first_component(raw, npm) else {
        return PathCheck::Skip;
    };
    if rest.len() >= MAX_ENTRY_PATH_BYTES {
        return PathCheck::TooLong;
    }
    let normalized: Vec<&[u8]> = if npm {
        // npm skips a name that has a `..` part behind `package/`, wherever it leads.
        if rest.split(|&b| is_sep_any(b)).any(|part| part == b"..") {
            return PathCheck::Skip;
        }
        rest.split(|&b| is_sep(b))
            .filter(|part| !part.is_empty() && *part != b".")
            .collect()
    } else {
        let mut stack: Vec<&[u8]> = Vec::new();
        for part in rest.split(|&b| is_sep(b)) {
            match part {
                b"" | b"." => {}
                b".." => {
                    if stack.last().is_some_and(|last| *last != b"..") {
                        stack.pop();
                    } else {
                        stack.push(part);
                    }
                }
                _ => stack.push(part),
            }
        }
        if stack.first() == Some(&&b".."[..]) {
            return PathCheck::Skip;
        }
        stack
    };
    if normalized.is_empty() {
        return PathCheck::Skip;
    }
    let joined = normalized.join(&b'/');
    // `C:...`: absolute on Windows, never written anywhere.
    if joined.len() >= 2 && joined[1] == b':' && joined[0].is_ascii_alphabetic() {
        return PathCheck::Skip;
    }
    let clean = String::from_utf8_lossy(&joined).into_owned();
    #[cfg(windows)]
    let clean = if npm {
        escape_windows_chars(&clean)
    } else {
        clean
    };
    PathCheck::Ok(clean)
}

/// node-tar writes characters Windows rejects in file names in the 0xf000 private-use range.
#[cfg(windows)]
fn escape_windows_chars(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            '|' | '<' | '>' | '?' | ':' => char::from_u32(0xf000 + c as u32).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// Control characters are shown escaped in messages.
pub(crate) fn escape_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// The npm-mode path an entry named `raw` (with its first component) is extracted to; `None`
/// for an entry that is skipped (a `..` part, empty, absolute or drive path, too long).
pub fn sanitize_entry_path(raw: &str) -> Option<String> {
    match check_path(raw.as_bytes(), true) {
        PathCheck::Ok(path) => Some(path),
        PathCheck::Skip | PathCheck::TooLong => None,
    }
}

/// Whether a symlink at `symlink_path` (relative to the extraction root) pointing at `target`
/// stays inside the root (bun `libarchive::is_symlink_target_safe`). Absolute targets are never
/// safe, and neither is a `..` after a named component (`a/../../x`).
pub fn is_symlink_target_safe(symlink_path: &str, target: &str) -> bool {
    let t = target.as_bytes();
    if t.is_empty() || t[0] == b'/' {
        return false;
    }
    #[cfg(windows)]
    if t[0] == b'\\' || (t.len() >= 2 && t[1] == b':') {
        return false;
    }
    let mut seen_named = false;
    for component in t.split(|&b| b == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                if seen_named {
                    return false;
                }
            }
            _ => seen_named = true,
        }
    }
    let dir = match symlink_path.rfind('/') {
        Some(i) => &symlink_path[..i],
        None => "",
    };
    let joined = if dir.is_empty() {
        target.to_owned()
    } else {
        format!("{dir}/{target}")
    };
    let mut stack: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if stack.last().is_some_and(|last| *last != "..") {
                    stack.pop();
                } else {
                    stack.push(part);
                }
            }
            _ => stack.push(part),
        }
    }
    stack.first() != Some(&"..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Got {
        File(String, u32, Vec<u8>),
        Dir(String),
        Link(String, String),
    }

    fn parse_chunks(tar: &[u8], opts: TarOptions, chunk: usize) -> Result<(Vec<Got>, Vec<String>)> {
        let mut parser = TarParser::with_options(opts);
        let mut out = Vec::new();
        let mut current: Option<(String, u32, Vec<u8>, EntryKind)> = None;
        for piece in tar.chunks(chunk.max(1)) {
            parser.feed(piece, &mut |ev| {
                match ev {
                    TarEvent::Entry(h) => current = Some((h.path, h.mode, Vec::new(), h.kind)),
                    TarEvent::Data(d) => current.as_mut().expect("entry").2.extend_from_slice(d),
                    TarEvent::EntryEnd => {
                        let (p, m, d, k) = current.take().expect("entry");
                        out.push(match k {
                            EntryKind::File => Got::File(p, m, d),
                            EntryKind::Directory => Got::Dir(p),
                        });
                    }
                    TarEvent::Symlink { path, target } => out.push(Got::Link(path, target)),
                }
                Ok(())
            })?;
        }
        let warnings = parser.take_warnings();
        parser.finish()?;
        Ok((out, warnings))
    }

    fn parse(tar: &[u8]) -> Result<Vec<Got>> {
        Ok(parse_chunks(tar, TarOptions::npm(), tar.len().max(1))?.0)
    }

    fn file(p: &str, body: &[u8]) -> Got {
        Got::File(p.to_owned(), 0o644, body.to_vec())
    }

    #[test]
    fn strips_first_component_of_any_name() {
        let tar = TarBuilder::new()
            .file("package/package.json", b"{}")
            .file("@scope/index.js", b"x")
            .file("node/lib/a.js", b"a")
            .end()
            .tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![
                file("package.json", b"{}"),
                file("index.js", b"x"),
                file("lib/a.js", b"a")
            ]
        );
    }

    #[test]
    fn leading_slash_counts_as_the_first_component_like_npm() {
        let tar = TarBuilder::new()
            .file("/package/index.js", b"x")
            .file("//a/b.js", b"y")
            .end()
            .tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![file("package/index.js", b"x"), file("a/b.js", b"y")]
        );
    }

    #[test]
    fn names_with_dotdot_parts_are_skipped_as_npm_skips_them() {
        let skipped = [
            "../climb-1.txt",
            "../../../climb-3.txt",
            "nested/../../climb-nested.txt",
            "a/b/../../../climb-ab.txt",
            "a/../climb-inside.txt",
            "a\\..\\climb-backslash.txt",
        ];
        let mut b = TarBuilder::new();
        b.file("package/ok.txt", b"ok");
        for s in skipped {
            b.file(&format!("package/{s}"), s.as_bytes());
        }
        b.file("package/after.txt", b"after").end();
        assert_eq!(
            parse(&b.tar()).unwrap(),
            vec![file("ok.txt", b"ok"), file("after.txt", b"after")]
        );
    }

    #[test]
    fn dot_and_empty_components_are_normalised() {
        let tar = TarBuilder::new()
            .file("package/./a//b/./c.js", b"c")
            .file("package/.", b"")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("a/b/c.js", b"c")]);
    }

    #[test]
    fn root_only_entries_are_skipped() {
        let tar = TarBuilder::new()
            .file("package", b"x")
            .file("package/", b"")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![]);
    }

    #[test]
    fn pax_path_longer_than_100_bytes() {
        let long = "package/very/deeply/nested/directory/structure/that/exceeds/the/one/hundred/byte/ustar/limit/long-name-file.txt";
        assert!(long.len() > 100);
        let tar = TarBuilder::new().file(long, b"long path ok\n").end().tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![file(&long["package/".len()..], b"long path ok\n")]
        );
    }

    #[test]
    fn gnu_long_name() {
        let name = format!("package/{}", "n".repeat(150));
        let mut long = name.clone().into_bytes();
        long.push(0);
        let tar = TarBuilder::new()
            .entry(
                header_with(b"././@LongLink", long.len() as u64, b'L', |h| {
                    h[257..265].copy_from_slice(b"ustar  \0")
                }),
                &long,
            )
            .entry(header("truncated", 3, b'0'), b"abc")
            .end()
            .tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![file(&name["package/".len()..], b"abc")]
        );
    }

    #[test]
    fn ustar_prefix_needs_the_version_in_npm_mode() {
        let with_prefix = |version: &[u8; 2]| {
            let version = *version;
            header_with(b"file.js", 1, b'0', move |h| {
                h[345..352].copy_from_slice(b"package");
                h[263..265].copy_from_slice(&version);
            })
        };
        let tar = TarBuilder::new()
            .entry(with_prefix(b"00"), b"a")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("file.js", b"a")]);
        // Without "00" npm reads an old-style header: no prefix, so `file.js` is the root.
        let tar = TarBuilder::new()
            .entry(with_prefix(b"  "), b"a")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![]);
        // The generic reader takes any `ustar` magic.
        let tar = TarBuilder::new()
            .entry(with_prefix(b"  "), b"a")
            .end()
            .tar();
        assert_eq!(
            parse_chunks(&tar, TarOptions::generic(), 512).unwrap().0,
            vec![Got::File("file.js".into(), 0o644, b"a".to_vec())]
        );
    }

    #[test]
    fn gnu_magic_has_no_prefix() {
        let h = header_with(b"package/x.js", 1, b'0', |h| {
            h[257..265].copy_from_slice(b"ustar  \0");
            h[345..350].copy_from_slice(b"zzzzz");
        });
        assert_eq!(
            parse(&TarBuilder::new().entry(h, b"1").end().tar()).unwrap(),
            vec![file("x.js", b"1")]
        );
    }

    #[test]
    fn only_regular_files_are_kept_in_npm_mode() {
        let tar = TarBuilder::new()
            .entry(header("package/", 0, b'5'), b"")
            .entry(header("package/dir/", 0, b'5'), b"")
            .entry(
                header_with(b"package/link", 0, b'2', |h| {
                    h[157..163].copy_from_slice(b"../etc")
                }),
                b"",
            )
            .entry(
                header_with(b"package/hard", 0, b'1', |h| {
                    h[157..170].copy_from_slice(b"package/a.txt")
                }),
                b"",
            )
            .entry(header("package/fifo", 0, b'6'), b"")
            .entry(header("package/contig", 2, b'7'), b"c7")
            .entry(header("package/weird", 5, b'Z'), b"zzzzz")
            .entry(header("package/nul", 1, 0), b"n")
            .end()
            .tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![file("contig", b"c7"), file("nul", b"n")]
        );
    }

    #[test]
    fn regular_entry_with_trailing_slash_is_a_directory_and_its_data_is_not_skipped() {
        // The 3 "data" bytes of the slash entry are not consumed; the next block is the header.
        let tar = TarBuilder::new()
            .raw(&header("package/dir/", 3, b'0'))
            .file("package/a.js", b"a")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("a.js", b"a")]);
    }

    #[test]
    fn hardlink_data_is_only_read_in_pax_archives() {
        let pax = pax_record("comment", b"x");
        let tar = TarBuilder::new()
            .entry(header("PaxHeader", pax.len() as u64, b'x'), &pax)
            .entry(header("package/hard", 4, b'1'), b"data")
            .file("package/a.js", b"a")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("a.js", b"a")]);
    }

    #[test]
    fn modes_are_normalised() {
        let tar = TarBuilder::new()
            .file_mode("package/bin.js", b"#!", 0o700)
            .file_mode("package/a.js", b"a", 0o600)
            .file_mode("package/s.js", b"s", 0o4755)
            .end()
            .tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![
                Got::File("bin.js".into(), 0o755, b"#!".to_vec()),
                Got::File("a.js".into(), 0o644, b"a".to_vec()),
                Got::File("s.js".into(), 0o755, b"s".to_vec()),
            ]
        );
    }

    #[test]
    fn base256_mode_and_size() {
        let wide_mode = [0x80, 0, 0, 0, 0x80, 0, 0x01, 0xed];
        let h = header_with(b"package/x", 0, b'0', |h| {
            h[100..108].copy_from_slice(&wide_mode);
            let mut size = [0u8; 12];
            size[0] = 0x80;
            size[11] = 3;
            h[124..136].copy_from_slice(&size);
        });
        let tar = TarBuilder::new().entry(h, b"abc").end().tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![Got::File("x".into(), 0o755, b"abc".to_vec())]
        );
    }

    #[test]
    fn negative_size_is_an_error() {
        let h = header_with(b"package/x", 0, b'0', |h| {
            h[124..136].copy_from_slice(&[0xff; 12])
        });
        let err = parse(&TarBuilder::new().raw(&h).end().tar()).unwrap_err();
        assert!(err.to_string().contains("negative size"), "{err}");
    }

    #[test]
    fn signed_checksum_is_accepted() {
        let mut h = header_with("package/\u{e9}.js".as_bytes(), 1, b'0', |_| {});
        h[148..156].fill(b' ');
        let signed: i64 = h.iter().map(|&b| i64::from(b as i8)).sum();
        h[148..156].copy_from_slice(&octal(signed as u64, 8));
        assert_eq!(
            parse(&TarBuilder::new().entry(h, b"e").end().tar()).unwrap(),
            vec![file("\u{e9}.js", b"e")]
        );
    }

    #[test]
    fn damaged_block_is_skipped_and_resets_pax_state() {
        let mut damaged = [0u8; 512];
        damaged[..4].copy_from_slice(b"junk");
        damaged[148..156].fill(b' ');
        let pax = pax_record("path", b"package/evil.js");
        let tar = TarBuilder::new()
            .entry(header("PaxHeader", pax.len() as u64, b'x'), &pax)
            .raw(&damaged)
            .file("package/good.js", b"good")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("good.js", b"good")]);
    }

    #[test]
    fn global_headers_around_a_damaged_block() {
        // bun "buffered extract: damaged-block retry resets header state"
        let pax = b"16 comment=test\n";
        let mut damaged = [0u8; 512];
        damaged[..4].copy_from_slice(b"junk");
        damaged[148..156].fill(b' ');
        let tar = TarBuilder::new()
            .entry(header("pax_global_header", 16, b'g'), pax)
            .raw(&damaged)
            .entry(header("pax_global_header", 16, b'g'), pax)
            .file("package/package.json", b"{}")
            .file("package/index.js", b"ok")
            .end()
            .tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![file("package.json", b"{}"), file("index.js", b"ok")]
        );
        let tar = TarBuilder::new()
            .entry(header("pax_global_header", 16, b'g'), pax)
            .entry(header("pax_global_header", 16, b'g'), pax)
            .file("package/index.js", b"ok")
            .end()
            .tar();
        let err = parse(&tar).unwrap_err();
        assert!(err.to_string().contains("Redundant 'g' header"), "{err}");
    }

    #[test]
    fn two_zero_blocks_end_the_archive() {
        let mut tar = TarBuilder::new().file("package/a", b"a").end().tar();
        tar.extend_from_slice(&header("package/after-end", 1, b'0'));
        tar.extend_from_slice(b"garbage that is not a block");
        assert_eq!(parse(&tar).unwrap(), vec![file("a", b"a")]);
    }

    #[test]
    fn a_single_zero_block_is_skipped() {
        let tar = TarBuilder::new()
            .file("package/a", b"a")
            .raw(&[0; 512])
            .file("package/b", b"b")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("a", b"a"), file("b", b"b")]);
    }

    #[test]
    fn archive_without_end_marker_is_fine() {
        let tar = TarBuilder::new().file("package/a", b"a").tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("a", b"a")]);
    }

    #[test]
    fn empty_archive_is_fine_and_garbage_is_not() {
        assert_eq!(parse(&TarBuilder::new().end().tar()).unwrap(), vec![]);
        assert!(parse(b"this is not a gzip stream").is_err());
        assert!(parse(&[0x55; 1024]).is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn header_that_declares_more_than_the_archive_holds() {
        let mut tar = TarBuilder::new().file("package/package.json", b"{}").tar();
        tar.extend_from_slice(&header("package/big.bin", 16 * 1024 * 1024, b'0'));
        tar.extend_from_slice(&[0x78; 100]);
        tar.extend_from_slice(&pad(100));
        tar.extend_from_slice(&[0; 1024]);
        let err = parse(&tar).unwrap_err();
        assert!(err.to_string().contains("Truncated"), "{err}");
    }

    #[test]
    fn truncated_header_and_padding() {
        let tar = TarBuilder::new().file("package/a", b"a").tar();
        assert!(parse(&tar[..300]).is_err());
        assert!(parse(&tar[..515]).is_err());
    }

    #[test]
    fn every_chunk_size_gives_the_same_result() {
        let long = format!("package/{}/x.txt", "d".repeat(120));
        let body = incompressible(5000, 7);
        let tar = TarBuilder::new()
            .file("package/package.json", b"{\"name\":\"x\"}")
            .file(&long, &body)
            .file("package/empty", b"")
            .end()
            .tar();
        let whole = parse(&tar).unwrap();
        for chunk in [1, 2, 7, 100, 511, 512, 513, 4096] {
            assert_eq!(
                parse_chunks(&tar, TarOptions::npm(), chunk).unwrap().0,
                whole,
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn overlong_path_is_skipped_with_a_warning() {
        let long = format!("package/{}", "a".repeat(40000));
        let tar = TarBuilder::new()
            .file("package/ok", b"ok")
            .file(&long, b"long name body\n")
            .end()
            .tar();
        let (got, warnings) = parse_chunks(&tar, TarOptions::npm(), 4096).unwrap();
        assert_eq!(got, vec![file("ok", b"ok")]);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0]
                .starts_with("Skipping entry with a path longer than the maximum path length")
        );
    }

    #[test]
    fn pax_path_with_nul_is_skipped() {
        let pax = pax_record("path", b"package/a\0b");
        let tar = TarBuilder::new()
            .entry(header("PaxHeader", pax.len() as u64, b'x'), &pax)
            .entry(header("package/x", 1, b'0'), b"x")
            .file("package/y", b"y")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("y", b"y")]);
    }

    #[test]
    fn pax_size_overrides_the_header() {
        let pax = pax_record("size", b"5");
        let tar = TarBuilder::new()
            .entry(header("PaxHeader", pax.len() as u64, b'x'), &pax)
            .entry(header("package/x", 0, b'0'), b"12345")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("x", b"12345")]);
    }

    #[test]
    fn malformed_pax_is_ignored_with_a_warning() {
        let pax = b"zz path=package/evil\n";
        let tar = TarBuilder::new()
            .entry(header("PaxHeader", pax.len() as u64, b'x'), pax)
            .entry(header("package/x", 1, b'0'), b"x")
            .end()
            .tar();
        let (got, warnings) = parse_chunks(&tar, TarOptions::npm(), 512).unwrap();
        assert_eq!(got, vec![file("x", b"x")]);
        assert_eq!(
            warnings,
            vec!["Ignoring malformed pax extended attributes".to_owned()]
        );
    }

    #[test]
    fn sparse_members_are_read_as_npm_reads_them() {
        // Old GNU sparse ('S'): skipped, stored bytes consumed.
        let stored = vec![0x41u8; 512];
        let s = header_with(b"package/gnu-sparse.bin", stored.len() as u64, b'S', |h| {
            h[257..265].copy_from_slice(b"ustar  \0");
            h[386..398].copy_from_slice(&octal(0, 12));
            h[398..410].copy_from_slice(&octal(512, 12));
            h[483..495].copy_from_slice(&octal(300_000, 12));
        });
        // PAX sparse 1.0: vendor keys ignored, the member is a file of the stored bytes.
        let mut pax = pax_record("GNU.sparse.major", b"1");
        pax.extend(pax_record("GNU.sparse.minor", b"0"));
        pax.extend(pax_record("GNU.sparse.name", b"package/pax-sparse.bin"));
        pax.extend(pax_record("GNU.sparse.realsize", b"300000"));
        let mut body = b"1\n0\n512\n".to_vec();
        body.extend(pad(body.len()));
        body.extend(vec![0x42u8; 512]);
        let tar = TarBuilder::new()
            .entry(s, &stored)
            .entry(header("PaxHeaders.0/sparse", pax.len() as u64, b'x'), &pax)
            .entry(
                header(
                    "package/GNUSparseFile.0/pax-sparse.bin",
                    body.len() as u64,
                    b'0',
                ),
                &body,
            )
            .end()
            .tar();
        assert_eq!(
            parse(&tar).unwrap(),
            vec![file("GNUSparseFile.0/pax-sparse.bin", &body)]
        );
    }

    #[test]
    fn duplicate_entries_are_all_reported() {
        let tar = TarBuilder::new()
            .file("package/a", b"1")
            .file("package/a", b"2")
            .end()
            .tar();
        assert_eq!(parse(&tar).unwrap(), vec![file("a", b"1"), file("a", b"2")]);
    }

    #[test]
    fn generic_mode_reports_directories_and_symlinks() {
        let tar = TarBuilder::new()
            .entry(header("owner-repo-abc/", 0, b'5'), b"")
            .entry(header("owner-repo-abc/lib/", 0, b'5'), b"")
            .file("owner-repo-abc/lib/../index.js", b"i")
            .file("owner-repo-abc/../../escape.js", b"e")
            .entry(
                header_with(b"owner-repo-abc/link", 0, b'2', |h| {
                    h[157..165].copy_from_slice(b"index.js")
                }),
                b"",
            )
            .entry(header("owner-repo-abc/weird", 2, b'Z'), b"zz")
            .end()
            .tar();
        let (got, _) = parse_chunks(&tar, TarOptions::generic(), 512).unwrap();
        assert_eq!(
            got,
            vec![
                Got::Dir("lib".into()),
                file("index.js", b"i"),
                Got::Link("link".into(), "index.js".into()),
                file("weird", b"zz"),
            ]
        );
    }

    #[test]
    fn generic_mode_reads_concatenated_archives() {
        let mut tar = TarBuilder::new().file("r/a", b"a").end().tar();
        tar.extend(TarBuilder::new().file("r/b", b"b").end().tar());
        let (got, _) = parse_chunks(&tar, TarOptions::generic(), 512).unwrap();
        assert_eq!(got, vec![file("a", b"a"), file("b", b"b")]);
        assert_eq!(parse(&tar).unwrap(), vec![file("a", b"a")]);
    }

    #[test]
    fn sanitize_entry_path_cases() {
        assert_eq!(
            sanitize_entry_path("package/index.js").as_deref(),
            Some("index.js")
        );
        assert_eq!(
            sanitize_entry_path("package/a/./b//c").as_deref(),
            Some("a/b/c")
        );
        assert_eq!(sanitize_entry_path("package/../x"), None);
        assert_eq!(sanitize_entry_path("package/a/../x"), None);
        assert_eq!(sanitize_entry_path("package/C:/x"), None);
        assert_eq!(sanitize_entry_path("package"), None);
        assert_eq!(sanitize_entry_path(""), None);
        assert_eq!(
            sanitize_entry_path("/etc/passwd").as_deref(),
            Some("etc/passwd")
        );
        assert_eq!(
            sanitize_entry_path("package//etc/passwd").as_deref(),
            Some("etc/passwd")
        );
    }

    #[test]
    fn symlink_target_safety() {
        assert!(is_symlink_target_safe("bin/tool", "../lib/tool.js"));
        assert!(is_symlink_target_safe("link", "index.js"));
        assert!(is_symlink_target_safe("a/b/c", "../../d"));
        assert!(!is_symlink_target_safe("link", "../outside"));
        assert!(!is_symlink_target_safe("a/link", "../../outside"));
        assert!(!is_symlink_target_safe("link", "/etc/passwd"));
        assert!(!is_symlink_target_safe("link", ""));
        assert!(!is_symlink_target_safe("a/link", "x/../../../packages/y"));
        assert!(!is_symlink_target_safe(
            "packages/a/link",
            "../../../packages/a"
        ));
    }
}
