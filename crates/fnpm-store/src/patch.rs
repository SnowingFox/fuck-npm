//! Patch file parser and applier (bun `patch/lib.rs`, itself a port of patch-package), used to
//! build the `_patch_hash=` copies of patched dependencies (bun `install/patch_install.rs`).
//!
//! - Hunks are placed by matching their `-` side, nearest to the header line first, like
//!   `git apply`; trailing whitespace is ignored when matching so CRLF files match.
//! - Paths are refused if absolute or containing `..`; every directory component is opened
//!   without following symlinks, and a target file must be a regular file with one link.
//! - Sections that cannot apply (binary files, text that is not a diff) are skipped with a
//!   warning, as bun does.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

const WHITESPACE: &[u8] = b" \t\n\r";
/// A larger target is an error rather than an unbounded allocation.
const MAX_PATCH_TARGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

fn trim(s: &[u8]) -> &[u8] {
    trim_right(trim_left(s))
}

fn trim_left(mut s: &[u8]) -> &[u8] {
    while let [c, rest @ ..] = s {
        if WHITESPACE.contains(c) {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn trim_right(mut s: &[u8]) -> &[u8] {
    while let [rest @ .., c] = s {
        if WHITESPACE.contains(c) {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// The first 8 bytes of the patch file's SHA-1, little endian: the `_patch_hash=` value.
pub fn hash_patch(contents: &[u8]) -> u64 {
    let digest = crate::hash::sha1(contents);
    u64::from_le_bytes(digest[..8].try_into().expect("SHA-1 has 20 bytes"))
}

/// True for a relative path that cannot leave the folder it starts in, as long as no
/// component of the path is a symlink.
pub fn is_safe_patch_path(path: &[u8]) -> bool {
    !path.is_empty()
        && !path.contains(&0)
        && path[0] != b'/'
        && path[0] != b'\\'
        && !(path.len() >= 2 && path[1] == b':' && path[0].is_ascii_alphabetic())
        && !(cfg!(windows) && path.contains(&b':'))
        && !path
            .split(|&b| b == b'/' || b == b'\\')
            .any(|part| part == b"..")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileMode {
    NonExecutable,
    Executable,
}

impl FileMode {
    fn bits(self) -> u32 {
        match self {
            Self::NonExecutable => 0o644,
            Self::Executable => 0o755,
        }
    }
}

fn parse_file_mode(mode: &[u8]) -> Option<FileMode> {
    let s = std::str::from_utf8(mode).ok()?;
    match u32::from_str_radix(s, 8).ok()? & 0o777 {
        0o644 => Some(FileMode::NonExecutable),
        0o755 => Some(FileMode::Executable),
        _ => None,
    }
}

/// Whether a hunk line is kept, added or removed.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum PartType {
    /// ` ` lines.
    #[default]
    Context,
    /// `+` lines.
    Insertion,
    /// `-` lines.
    Deletion,
}

#[derive(Debug, Default)]
struct Part<'a> {
    ty: PartType,
    lines: Vec<&'a [u8]>,
    no_newline_at_end_of_file: bool,
}

#[derive(Debug, Copy, Clone)]
struct Range {
    start: u32,
    len: u32,
}

impl Default for Range {
    fn default() -> Self {
        Self { start: 1, len: 0 }
    }
}

#[derive(Debug, Default)]
struct Hunk<'a> {
    original: Range,
    patched: Range,
    parts: Vec<Part<'a>>,
    offset: usize,
}

impl<'a> Hunk<'a> {
    fn verify_integrity(&self, pending: Option<&Part<'a>>) -> bool {
        let (mut original, mut patched) = (0usize, 0usize);
        for part in self.parts.iter().chain(pending) {
            match part.ty {
                PartType::Context => {
                    original += part.lines.len();
                    patched += part.lines.len();
                }
                PartType::Insertion => patched += part.lines.len(),
                PartType::Deletion => original += part.lines.len(),
            }
        }
        original == self.original.len as usize && patched == self.patched.len as usize
    }
}

#[derive(Debug)]
enum Op<'a> {
    Patch {
        path: &'a [u8],
        hunks: Vec<Hunk<'a>>,
    },
    Delete {
        path: &'a [u8],
    },
    Create {
        path: &'a [u8],
        mode: FileMode,
        hunk: Option<Hunk<'a>>,
    },
    Rename {
        from: &'a [u8],
        to: &'a [u8],
    },
    Mode {
        path: &'a [u8],
        mode: FileMode,
    },
}

/// A section of the patch that [`PatchFile::apply`] ignores.
#[derive(Debug, Clone)]
pub struct SkippedSection<'a> {
    /// Byte offset of the section's first line in the patch file.
    pub offset: usize,
    /// The file the section is about, if it says.
    pub path: Option<&'a [u8]>,
    /// `Binary files a/x and b/x differ`: the patch does not have the new content.
    pub is_binary: bool,
}

/// A parsed patch; strings borrow the patch text.
#[derive(Debug, Default)]
pub struct PatchFile<'a> {
    ops: Vec<Op<'a>>,
    /// Sections that are skipped.
    pub skipped: Vec<SkippedSection<'a>>,
}

/// Why a patch did not apply.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A filesystem operation failed (`ELOOP` for a symlink in the way, `EMLINK` for a hard
    /// linked target, `EINVAL` for an unsafe path or a non-regular file).
    #[error("{path}: {source}")]
    Io {
        /// Relative path in the patch.
        path: String,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The `-` side of a hunk is not in the file.
    #[error("hunk #{hunk} does not apply to {path} (expected at line {line})")]
    HunkDoesNotApply {
        /// Relative path in the patch.
        path: String,
        /// 1-based hunk number.
        hunk: usize,
        /// The hunk header's start line.
        line: u32,
    },
}

fn apply_io(path: &[u8]) -> impl Fn(io::Error) -> ApplyError + '_ {
    move |source| ApplyError::Io {
        path: String::from_utf8_lossy(path).into_owned(),
        source,
    }
}

fn einval() -> io::Error {
    io::Error::from_raw_os_error(einval_code())
}

#[cfg(unix)]
fn einval_code() -> i32 {
    libc::EINVAL
}

#[cfg(not(unix))]
fn einval_code() -> i32 {
    22
}

impl<'a> PatchFile<'a> {
    /// Applies every part, in order, to the package directory `dir`.
    pub fn apply(&self, dir: &Path) -> Result<(), ApplyError> {
        for op in &self.ops {
            match op {
                Op::Delete { path } => {
                    let (parent, base) =
                        resolve_target(dir, path, false).map_err(apply_io(path))?;
                    std::fs::remove_file(parent.join(base)).map_err(apply_io(path))?;
                }
                Op::Rename { from, to } => {
                    let (fp, fb) = resolve_target(dir, from, false).map_err(apply_io(from))?;
                    let (tp, tb) = resolve_target(dir, to, true).map_err(apply_io(to))?;
                    std::fs::rename(fp.join(fb), tp.join(tb)).map_err(apply_io(from))?;
                }
                Op::Create { path, mode, hunk } => {
                    create_file(dir, path, *mode, hunk.as_ref()).map_err(apply_io(path))?
                }
                Op::Patch { path, hunks } => patch_file(dir, path, hunks)?,
                Op::Mode { path, mode } => {
                    let (parent, base) =
                        resolve_target(dir, path, false).map_err(apply_io(path))?;
                    let target = parent.join(base);
                    check_target_file(&target).map_err(apply_io(path))?;
                    set_mode(&target, mode.bits()).map_err(apply_io(path))?;
                }
            }
        }
        Ok(())
    }

    /// Every path the patch reads or writes.
    pub fn touched_paths(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut add = |p: &[u8]| {
            out.insert(String::from_utf8_lossy(p).replace('\\', "/"));
        };
        for op in &self.ops {
            match op {
                Op::Patch { path, .. }
                | Op::Delete { path }
                | Op::Create { path, .. }
                | Op::Mode { path, .. } => add(path),
                Op::Rename { from, to } => {
                    add(from);
                    add(to);
                }
            }
        }
        out
    }

    /// bun's warnings for the skipped sections: `skipping <path>: <reason>`.
    pub fn skipped_warnings(&self) -> Vec<String> {
        self.skipped
            .iter()
            .map(|s| {
                let subject = s.path.map_or_else(|| "non-patch text".to_owned(), |p| String::from_utf8_lossy(p).into_owned());
                let reason = if s.is_binary { "binary file, content not in patch" } else { "no applicable changes" };
                format!("skipping {subject}: {reason} (recreate the patch with fnpm patch, or delete these lines)")
            })
            .collect()
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, bits: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _bits: u32) -> io::Result<()> {
    Ok(())
}

/// The verified directory holding `path`'s final component, and that component. Each
/// directory component is checked not to be a symlink; with `create`, missing ones are made.
fn resolve_target<'p>(dir: &Path, path: &'p [u8], create: bool) -> io::Result<(PathBuf, &'p str)> {
    if !is_safe_patch_path(path) {
        return Err(einval());
    }
    let text = std::str::from_utf8(path).map_err(|_| einval())?;
    let mut parent = dir.to_path_buf();
    let mut parts: Vec<&str> = text
        .split(|c| c == '/' || (cfg!(windows) && c == '\\'))
        .collect();
    let base = parts.pop().filter(|b| !b.is_empty()).ok_or_else(einval)?;
    for component in parts {
        if component.is_empty() || component == "." {
            continue;
        }
        parent.push(component);
        match std::fs::symlink_metadata(&parent) {
            Ok(m) if m.file_type().is_symlink() => return Err(eloop()),
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(io::Error::from(io::ErrorKind::NotADirectory)),
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                match std::fs::create_dir(&parent) {
                    Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                    _ => {}
                }
                if std::fs::symlink_metadata(&parent)?.file_type().is_symlink() {
                    return Err(eloop());
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok((parent, base))
}

fn eloop() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::ELOOP)
    }
    #[cfg(not(unix))]
    {
        io::Error::other("symlink in the way")
    }
}

/// Only a regular file with one hard link is accepted, because a second link could reach the
/// inode from outside the package directory.
fn check_target_file(path: &Path) -> io::Result<std::fs::Metadata> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(eloop());
    }
    if !meta.is_file() {
        return Err(einval());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Err(io::Error::from_raw_os_error(libc::EMLINK));
        }
    }
    Ok(meta)
}

fn create_file(dir: &Path, path: &[u8], mode: FileMode, hunk: Option<&Hunk<'_>>) -> io::Result<()> {
    let (parent, base) = resolve_target(dir, path, true)?;
    let target = parent.join(base);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => {
            check_target_file(&target)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut contents = Vec::new();
    // A `@@ -0,0 +0,0 @@` header with no body parses to a hunk with zero parts.
    if let Some(first) = hunk.and_then(|h| h.parts.first()) {
        contents = first.lines.join(&b'\n');
        if !first.no_newline_at_end_of_file {
            contents.push(b'\n');
        }
    }
    std::fs::write(&target, contents)?;
    set_mode(&target, mode.bits())
}

fn patch_file(dir: &Path, path: &[u8], hunks: &[Hunk<'_>]) -> Result<(), ApplyError> {
    let (parent, base) = resolve_target(dir, path, false).map_err(apply_io(path))?;
    let target = parent.join(base);
    let meta = check_target_file(&target).map_err(apply_io(path))?;
    if meta.len() > MAX_PATCH_TARGET_BYTES {
        return Err(apply_io(path)(einval()));
    }
    let filebuf = std::fs::read(&target).map_err(apply_io(path))?;
    let mut lines: Vec<&[u8]> = filebuf.split(|&b| b == b'\n').collect();
    // The empty string after the final `\n` is not a line.
    let mut final_newline = lines.last().is_some_and(|l| l.is_empty());
    if final_newline {
        lines.pop();
    }
    let does_not_apply = |index: usize, hunk: &Hunk<'_>| ApplyError::HunkDoesNotApply {
        path: String::from_utf8_lossy(path).into_owned(),
        hunk: index + 1,
        line: hunk.original.start,
    };
    let mut ordered: Vec<(usize, &Hunk<'_>)> = hunks.iter().enumerate().collect();
    ordered.sort_by_key(|(_, h)| h.original.start);
    let mut patched: Vec<&[u8]> = Vec::with_capacity(lines.len());
    let mut cursor = 0usize;
    for (index, hunk) in ordered {
        // `\ No newline at end of file` ends that side of the file: only an insertion may
        // follow it. A marker on a context line is exempt (`git diff --ignore-cr-at-eol`).
        if let Some(i) = hunk
            .parts
            .iter()
            .position(|p| p.no_newline_at_end_of_file && p.ty != PartType::Context)
            && hunk.parts[i + 1..]
                .iter()
                .any(|p| p.ty != PartType::Insertion)
        {
            return Err(does_not_apply(index, hunk));
        }
        let start = hunk.original.start as usize;
        let hunk_start = if hunk.original.len == 0 {
            // `-N,0`: insert after line N, where 0 is the top.
            if start < cursor || start > lines.len() {
                return Err(does_not_apply(index, hunk));
            }
            start
        } else {
            let expected = start as isize - 1 - cursor as isize;
            match find_hunk_position(hunk, &lines[cursor..], expected) {
                Some(i) => cursor + i,
                None => return Err(does_not_apply(index, hunk)),
            }
        };
        patched.extend_from_slice(&lines[cursor..hunk_start]);
        cursor = hunk_start;
        for part in &hunk.parts {
            if part.ty == PartType::Insertion {
                patched.extend_from_slice(&part.lines);
                continue;
            }
            let Some(old) = lines.get(cursor..cursor + part.lines.len()) else {
                return Err(does_not_apply(index, hunk));
            };
            if part.ty == PartType::Context {
                patched.extend_from_slice(old);
            }
            cursor += old.len();
        }
        // A hunk that reaches the end of the file decides whether it ends in a newline.
        if cursor == lines.len() {
            let last_kept = hunk.parts.iter().rfind(|p| p.ty != PartType::Deletion);
            final_newline = !last_kept.is_some_and(|p| p.no_newline_at_end_of_file);
        }
    }
    patched.extend_from_slice(&lines[cursor..]);
    if final_newline {
        patched.push(b"");
    }
    std::fs::write(&target, patched.join(&b'\n')).map_err(apply_io(path))
}

/// The index nearest to `expected` where the hunk's `-` side matches, forward first.
fn find_hunk_position(hunk: &Hunk<'_>, lines: &[&[u8]], expected: isize) -> Option<usize> {
    let last = lines.len() as isize;
    let expected = expected.clamp(0, last);
    let mut offset: isize = 0;
    loop {
        let forward = expected + offset;
        let backward = expected - offset;
        if forward > last && backward < 0 {
            return None;
        }
        if (0..=last).contains(&forward) && hunk_matches_at(hunk, lines, forward as usize) {
            return Some(forward as usize);
        }
        if offset != 0
            && (0..=last).contains(&backward)
            && hunk_matches_at(hunk, lines, backward as usize)
        {
            return Some(backward as usize);
        }
        offset += 1;
    }
}

/// Trailing whitespace is ignored (patch-package's `linesAreEqual`) so CRLF files match.
fn hunk_matches_at(hunk: &Hunk<'_>, lines: &[&[u8]], start: usize) -> bool {
    let mut cursor = start;
    for part in &hunk.parts {
        if part.ty == PartType::Insertion {
            continue;
        }
        for patch_line in &part.lines {
            let Some(file_line) = lines.get(cursor) else {
                return false;
            };
            if trim_right(file_line) != trim_right(patch_line) {
                return false;
            }
            cursor += 1;
        }
    }
    true
}

/// Why a patch file did not parse (bun's names).
#[allow(non_camel_case_types, missing_docs)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ParseErr {
    unrecognized_pragma,
    no_newline_at_eof_pragma_encountered_without_context,
    hunk_lines_encountered_before_hunk_header,
    hunk_header_integrity_check_failed,
    bad_diff_line,
    bad_header_line,
    rename_from_and_to_not_give,
    no_path_given_for_file_deletion,
    no_path_given_for_file_creation,
    bad_file_mode,
    git_binary_patch_not_supported,
    no_path_given_for_hunk,
}

impl fmt::Display for ParseErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// A parse error and the byte offset of the line at fault.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ParseErrAt {
    /// What is wrong.
    pub err: ParseErr,
    /// Byte offset of the line that caused it.
    pub offset: usize,
}

#[derive(Debug, Default)]
struct FileDeets<'a> {
    diff_line_from_path: Option<&'a [u8]>,
    diff_line_to_path: Option<&'a [u8]>,
    old_mode: Option<&'a [u8]>,
    new_mode: Option<&'a [u8]>,
    deleted_file_mode: Option<&'a [u8]>,
    new_file_mode: Option<&'a [u8]>,
    rename_from: Option<&'a [u8]>,
    rename_to: Option<&'a [u8]>,
    from_path: Option<&'a [u8]>,
    to_path: Option<&'a [u8]>,
    hunks: Vec<Hunk<'a>>,
    is_binary: bool,
    offset: usize,
}

impl FileDeets<'_> {
    fn nullify_empty_strings(&mut self) {
        for f in [
            &mut self.diff_line_from_path,
            &mut self.diff_line_to_path,
            &mut self.old_mode,
            &mut self.new_mode,
            &mut self.deleted_file_mode,
            &mut self.new_file_mode,
            &mut self.rename_from,
            &mut self.rename_to,
            &mut self.from_path,
            &mut self.to_path,
        ] {
            if f.is_some_and(<[u8]>::is_empty) {
                *f = None;
            }
        }
    }
}

#[derive(Default, PartialEq, Eq)]
enum ParserState {
    #[default]
    Header,
    Hunks,
}

#[derive(Default)]
struct LinesParser<'a> {
    result: Vec<FileDeets<'a>>,
    current: FileDeets<'a>,
    state: ParserState,
    hunk: Option<Hunk<'a>>,
    part: Option<Part<'a>>,
    offset: usize,
}

impl<'a> LinesParser<'a> {
    fn parse(&mut self, file: &'a [u8]) -> Result<(), ParseErr> {
        let file = file.strip_suffix(b"\n").unwrap_or(file);
        if file.is_empty() {
            return Ok(());
        }
        // (offset, line) pairs so the cursor can step back one line.
        let mut lines: Vec<(usize, &'a [u8])> = Vec::new();
        let mut start = 0;
        for (i, &b) in file.iter().enumerate() {
            if b == b'\n' {
                lines.push((start, &file[start..i]));
                start = i + 1;
            }
        }
        lines.push((start, &file[start..]));
        let mut i = 0;
        while i < lines.len() {
            let (offset, line) = lines[i];
            self.offset = offset;
            i += 1;
            match self.state {
                ParserState::Header => {
                    if line.starts_with(b"@@") {
                        self.state = ParserState::Hunks;
                        self.current.hunks = Vec::new();
                        i -= 1;
                    } else if line.starts_with(b"diff --git ") {
                        if self.current.diff_line_from_path.is_some() {
                            self.commit_file();
                        }
                        let (a, b) = parse_diff_line_paths(line).ok_or(ParseErr::bad_diff_line)?;
                        self.current.diff_line_from_path = Some(a);
                        self.current.diff_line_to_path = Some(b);
                    } else if let Some(rest) = line.strip_prefix(b"old mode ") {
                        self.current.old_mode = Some(trim(rest));
                    } else if let Some(rest) = line.strip_prefix(b"new mode ") {
                        self.current.new_mode = Some(trim(rest));
                    } else if let Some(rest) = line.strip_prefix(b"deleted file mode ") {
                        self.current.deleted_file_mode = Some(trim(rest));
                    } else if let Some(rest) = line.strip_prefix(b"new file mode ") {
                        self.current.new_file_mode = Some(trim(rest));
                    } else if let Some(rest) = line.strip_prefix(b"rename from ") {
                        self.current.rename_from = Some(trim(rest));
                    } else if let Some(rest) = line.strip_prefix(b"rename to ") {
                        self.current.rename_to = Some(trim(rest));
                    } else if line.starts_with(b"index ") {
                        // `index <hash>..<hash> <mode>`: the hashes are not used.
                    } else if line.starts_with(b"--- ") {
                        self.current.from_path =
                            Some(trim(line.get(b"--- a/".len()..).unwrap_or_default()));
                    } else if line.starts_with(b"+++ ") {
                        self.current.to_path =
                            Some(trim(line.get(b"+++ b/".len()..).unwrap_or_default()));
                    } else if line.starts_with(b"Binary files ") {
                        self.current.is_binary = true;
                    } else if line.starts_with(b"GIT binary patch")
                        && self.current.deleted_file_mode.is_none()
                    {
                        return Err(ParseErr::git_binary_patch_not_supported);
                    }
                }
                ParserState::Hunks => {
                    // A legacy file header, or a deletion line unless the hunk is complete.
                    if line.starts_with(b"--- a/")
                        && self
                            .hunk
                            .as_ref()
                            .is_some_and(|h| h.verify_integrity(self.part.as_ref()))
                    {
                        self.state = ParserState::Header;
                        self.commit_file();
                        i -= 1;
                        continue;
                    }
                    let ty = match line.first().copied() {
                        None | Some(b' ' | b'\r') => PartType::Context,
                        Some(b'-') => PartType::Deletion,
                        Some(b'+') => PartType::Insertion,
                        Some(b'@') => {
                            self.commit_hunk();
                            let mut hunk = parse_hunk_header_line(line)?;
                            hunk.offset = self.offset;
                            self.hunk = Some(hunk);
                            continue;
                        }
                        Some(b'\\') => {
                            if !line.starts_with(b"\\ No newline at end of file") {
                                return Err(ParseErr::unrecognized_pragma);
                            }
                            self.part
                                .as_mut()
                                .ok_or(
                                    ParseErr::no_newline_at_eof_pragma_encountered_without_context,
                                )?
                                .no_newline_at_end_of_file = true;
                            continue;
                        }
                        Some(_) => {
                            self.state = ParserState::Header;
                            self.commit_file();
                            i -= 1;
                            continue;
                        }
                    };
                    let hunk = self
                        .hunk
                        .as_mut()
                        .ok_or(ParseErr::hunk_lines_encountered_before_hunk_header)?;
                    if self.part.as_ref().is_some_and(|p| p.ty != ty) {
                        hunk.parts.extend(self.part.take());
                    }
                    self.part
                        .get_or_insert_with(|| Part {
                            ty,
                            ..Default::default()
                        })
                        .lines
                        .push(&line[1.min(line.len())..]);
                }
            }
        }
        self.commit_file();
        for deets in &self.result {
            for hunk in &deets.hunks {
                if !hunk.verify_integrity(None) {
                    self.offset = hunk.offset;
                    return Err(ParseErr::hunk_header_integrity_check_failed);
                }
            }
        }
        Ok(())
    }

    fn commit_hunk(&mut self) {
        if let Some(mut hunk) = self.hunk.take() {
            if let Some(part) = self.part.take() {
                hunk.parts.push(part);
            }
            self.current.hunks.push(hunk);
        }
    }

    fn commit_file(&mut self) {
        self.commit_hunk();
        self.current.nullify_empty_strings();
        let deets = std::mem::take(&mut self.current);
        self.result.push(deets);
        self.current.offset = self.offset;
    }
}

fn split_ascii_digits(text: &[u8]) -> (&[u8], &[u8]) {
    text.split_at(text.iter().take_while(|c| c.is_ascii_digit()).count())
}

fn parse_decimal(digits: &[u8]) -> Option<u32> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// `100,32 ` or `100 `: the range and the rest, starting at the space.
fn parse_header_range(text: &[u8]) -> Result<(Range, &[u8]), ParseErr> {
    let (start, mut rest) = split_ascii_digits(text);
    let mut len: &[u8] = b"1";
    if rest.len() < 2 {
        return Err(ParseErr::bad_header_line);
    }
    if let Some(after_comma) = rest.strip_prefix(b",") {
        (len, rest) = split_ascii_digits(after_comma);
    }
    if !rest.starts_with(b" ") {
        return Err(ParseErr::bad_header_line);
    }
    let range = Range {
        start: parse_decimal(start).ok_or(ParseErr::bad_header_line)?,
        len: parse_decimal(len).ok_or(ParseErr::bad_header_line)?,
    };
    Ok((range, rest))
}

fn parse_hunk_header_line<'a>(line: &[u8]) -> Result<Hunk<'a>, ParseErr> {
    let line = trim(line)
        .strip_prefix(b"@@ -")
        .ok_or(ParseErr::bad_header_line)?;
    let (original, line) = parse_header_range(line)?;
    let line = line.strip_prefix(b" +").ok_or(ParseErr::bad_header_line)?;
    let (patched, line) = parse_header_range(line)?;
    if !line.starts_with(b" @@") {
        return Err(ParseErr::bad_header_line);
    }
    Ok(Hunk {
        original,
        patched,
        ..Default::default()
    })
}

/// `diff --git a/<from> b/<to>` (patch-package's `/^diff --git a\/(.*?) b\/(.*?)\s*$/`).
fn parse_diff_line_paths(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = trim_right(line.strip_prefix(b"diff --git a/")?);
    if rest.is_empty() {
        return None;
    }
    // Without a rename both paths are identical, so ` b/` is in the exact middle.
    let (a, b_part) = rest.split_at(rest.len().saturating_sub(b" b/".len()) / 2);
    if b_part.strip_prefix(b" b/") == Some(a) {
        return Some((a, a));
    }
    let mut i = 0usize;
    loop {
        let found = rest[i..].iter().position(|&c| c == b'b')?;
        i += found;
        if i > 0 && rest[i - 1] == b' ' && i + 1 < rest.len() && rest[i + 1] == b'/' {
            return Some((&rest[..i - 1], trim_right(&rest[i + 2..])));
        }
        i += 1;
    }
}

/// Parses a patch file.
pub fn parse_patch_file(file: &[u8]) -> Result<PatchFile<'_>, ParseErrAt> {
    let mut parser = LinesParser::default();
    let parsed = parser.parse(file).and_then(|()| {
        let mut files = std::mem::take(&mut parser.result);
        second_pass(&mut files, &mut parser.offset)
    });
    parsed.map_err(|err| ParseErrAt {
        err,
        offset: parser.offset,
    })
}

#[derive(PartialEq, Eq)]
enum Kind {
    Rename,
    Delete,
    Create,
    Patch,
    Mode,
}

fn second_pass<'a>(
    files: &mut [FileDeets<'a>],
    offset: &mut usize,
) -> Result<PatchFile<'a>, ParseErr> {
    let mut result = PatchFile::default();
    for file in files.iter_mut() {
        *offset = file.offset;
        let before = result.ops.len();
        let kind = if file.rename_from.is_some() {
            Kind::Rename
        } else if file.deleted_file_mode.is_some() {
            Kind::Delete
        } else if file.new_file_mode.is_some() {
            Kind::Create
        } else if !file.hunks.is_empty() {
            Kind::Patch
        } else {
            Kind::Mode
        };
        let mut destination: Option<&'a [u8]> = None;
        match kind {
            Kind::Rename => {
                let (Some(from), Some(to)) = (file.rename_from, file.rename_to) else {
                    return Err(ParseErr::rename_from_and_to_not_give);
                };
                result.ops.push(Op::Rename { from, to });
                destination = Some(to);
            }
            Kind::Delete => {
                let path = file
                    .from_path
                    .or(file.diff_line_from_path)
                    .ok_or(ParseErr::no_path_given_for_file_deletion)?;
                parse_file_mode(file.deleted_file_mode.unwrap_or_default())
                    .ok_or(ParseErr::bad_file_mode)?;
                file.hunks.clear();
                result.ops.push(Op::Delete { path });
            }
            // The patch does not have the content; an empty file is not what it asks for.
            Kind::Create if file.is_binary => {}
            Kind::Create => {
                let path = file
                    .to_path
                    .or(file.diff_line_to_path)
                    .ok_or(ParseErr::no_path_given_for_file_creation)?;
                let mode = parse_file_mode(file.new_file_mode.unwrap_or_default())
                    .ok_or(ParseErr::bad_file_mode)?;
                let hunk = if file.hunks.is_empty() {
                    None
                } else {
                    Some(file.hunks.remove(0))
                };
                file.hunks.clear();
                result.ops.push(Op::Create { path, mode, hunk });
            }
            Kind::Patch | Kind::Mode => destination = file.to_path.or(file.diff_line_to_path),
        }
        if let (Some(path), Some(old), Some(new)) = (destination, file.old_mode, file.new_mode)
            && old != new
        {
            parse_file_mode(old).ok_or(ParseErr::bad_file_mode)?;
            let mode = parse_file_mode(new).ok_or(ParseErr::bad_file_mode)?;
            result.ops.push(Op::Mode { path, mode });
        }
        if let Some(path) = destination
            && !file.hunks.is_empty()
        {
            result.ops.push(Op::Patch {
                path,
                hunks: std::mem::take(&mut file.hunks),
            });
        }
        // A section that produces nothing (a copy, text that is not a diff) is skipped.
        let has_no_op = result.ops.len() == before;
        if has_no_op && let Some(hunk) = file.hunks.first() {
            *offset = hunk.offset;
            return Err(ParseErr::no_path_given_for_hunk);
        }
        let is_binary = file.is_binary && kind != Kind::Delete;
        if has_no_op || is_binary {
            result.skipped.push(SkippedSection {
                offset: file.offset,
                path: file.to_path.or(file.diff_line_to_path),
                is_binary,
            });
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "diff --git a/index.js b/index.js\n--- a/index.js\n+++ b/index.js\n";

    fn apply_to(
        files: &[(&str, &str)],
        patch: &str,
    ) -> (tempfile::TempDir, Result<(), ApplyError>) {
        let dir = tempfile::tempdir().unwrap();
        for (p, d) in files {
            let path = dir.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, d).unwrap();
        }
        let parsed = parse_patch_file(patch.as_bytes()).unwrap();
        let r = parsed.apply(dir.path());
        (dir, r)
    }

    fn read(dir: &tempfile::TempDir, p: &str) -> String {
        std::fs::read_to_string(dir.path().join(p)).unwrap()
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn simple_insertion_and_deletion() {
        let (d, r) = apply_to(
            &[("hello.txt", "hello!\n")],
            "diff --git a/hello.txt b/hello.txt\n--- a/hello.txt\n+++ b/hello.txt\n@@ -1 +1,2 @@\n hello!\n+wassup?\n",
        );
        r.unwrap();
        assert_eq!(read(&d, "hello.txt"), "hello!\nwassup?\n");
        let (d, r) = apply_to(
            &[("hello.txt", "hello!\nwassup?\nlmao\n")],
            "diff --git a/hello.txt b/hello.txt\n--- a/hello.txt\n+++ b/hello.txt\n@@ -1,3 +1 @@\n-hello!\n wassup?\n-lmao\n",
        );
        r.unwrap();
        assert_eq!(read(&d, "hello.txt"), "wassup?\n");
    }

    #[test]
    fn stale_plus_start_line_is_placed_by_context() {
        let file: String = (1..=12).map(|i| format!("line{i:02}\n")).collect();
        let patch = [
            "diff --git a/hello.txt b/hello.txt",
            "--- a/hello.txt",
            "+++ b/hello.txt",
            "@@ -1,4 +1,6 @@",
            " line01",
            " line02",
            "+INSERT-ONE",
            "+INSERT-TWO",
            " line03",
            " line04",
            "@@ -7,6 +7,7 @@",
            " line07",
            " line08",
            "-line09",
            "+line09-changed",
            "+SECOND-MARKER",
            " line10",
            " line11",
            " line12",
            "",
        ]
        .join("\n");
        let (d, r) = apply_to(&[("hello.txt", &file)], &patch);
        r.unwrap();
        assert_eq!(
            read(&d, "hello.txt"),
            "line01\nline02\nINSERT-ONE\nINSERT-TWO\nline03\nline04\nline05\nline06\nline07\nline08\nline09-changed\nSECOND-MARKER\nline10\nline11\nline12\n"
        );
    }

    #[test]
    fn hunks_that_match_nowhere_fail_without_writing() {
        let file: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        let patch = "diff --git a/sub/hello.txt b/sub/hello.txt\n--- a/sub/hello.txt\n+++ b/sub/hello.txt\n@@ -1,2 +1,3 @@\n line1\n+FIRST\n line2\n@@ -8,2 +9,3 @@\n line8\n+SECOND\n not-line9\n";
        let (d, r) = apply_to(&[("sub/hello.txt", &file)], patch);
        assert_eq!(
            r.unwrap_err().to_string(),
            "hunk #2 does not apply to sub/hello.txt (expected at line 8)"
        );
        assert_eq!(read(&d, "sub/hello.txt"), file);
    }

    #[test]
    fn found_by_context_far_away() {
        let file: String = (0..60)
            .map(|i| {
                if i == 49 {
                    "target\n".to_owned()
                } else {
                    format!("filler{}\n", i + 1)
                }
            })
            .collect();
        let (d, r) = apply_to(
            &[("hello.txt", &file)],
            "diff --git a/hello.txt b/hello.txt\n--- a/hello.txt\n+++ b/hello.txt\n@@ -1,3 +1,3 @@\n filler49\n-target\n+replaced\n filler51\n",
        );
        r.unwrap();
        let lines: Vec<String> = read(&d, "hello.txt")
            .split('\n')
            .map(str::to_owned)
            .collect();
        assert_eq!(
            (
                lines[48].as_str(),
                lines[49].as_str(),
                lines[50].as_str(),
                lines.len()
            ),
            ("filler49", "replaced", "filler51", 61)
        );
    }

    #[test]
    fn duplicate_context_and_crlf() {
        let p = format!(
            "{HEADER}@@ -1,3 +1,4 @@\n s\n-dup\n+ONE\n+MORE\n e\n@@ -4,3 +5,3 @@\n s\n-dup\n+TWO\n e\n"
        );
        let (d, r) = apply_to(&[("index.js", "s\ndup\ne\ns\ndup\ne\n")], &p);
        r.unwrap();
        assert_eq!(read(&d, "index.js"), "s\nONE\nMORE\ne\ns\nTWO\ne\n");
        let (d, r) = apply_to(
            &[("index.js", "a\r\nb\r\nc\r\n")],
            &format!("{HEADER}@@ -1,3 +1,2 @@\n a\n-b\n c\n"),
        );
        r.unwrap();
        assert_eq!(read(&d, "index.js"), "a\r\nc\r\n");
    }

    #[test]
    fn zero_context_hunks() {
        let cases: &[(&str, String, &str, &str)] = &[
            (
                "delete one line",
                numbered(8),
                "@@ -6 +5,0 @@\n-line 6\n",
                "line 1\nline 2\nline 3\nline 4\nline 5\nline 7\nline 8\n",
            ),
            (
                "delete a run",
                numbered(8),
                "@@ -3,3 +2,0 @@\n-line 3\n-line 4\n-line 5\n",
                "line 1\nline 2\nline 6\nline 7\nline 8\n",
            ),
            (
                "delete the first line",
                numbered(3),
                "@@ -1 +0,0 @@\n-line 1\n",
                "line 2\nline 3\n",
            ),
            (
                "delete the last line",
                numbered(3),
                "@@ -3 +2,0 @@\n-line 3\n",
                "line 1\nline 2\n",
            ),
            (
                "two deletions",
                numbered(8),
                "@@ -2 +1,0 @@\n-line 2\n@@ -6 +4,0 @@\n-line 6\n",
                "line 1\nline 3\nline 4\nline 5\nline 7\nline 8\n",
            ),
            (
                "insert after a line",
                numbered(4),
                "@@ -3,0 +4,2 @@\n+X\n+Y\n",
                "line 1\nline 2\nline 3\nX\nY\nline 4\n",
            ),
            (
                "insert at the top",
                numbered(2),
                "@@ -0,0 +1 @@\n+X\n",
                "X\nline 1\nline 2\n",
            ),
            (
                "insert at the end",
                numbered(2),
                "@@ -2,0 +3 @@\n+X\n",
                "line 1\nline 2\nX\n",
            ),
            (
                "replace a line",
                numbered(3),
                "@@ -2 +2 @@\n-line 2\n+TWO\n",
                "line 1\nTWO\nline 3\n",
            ),
            (
                "delete then insert",
                numbered(6),
                "@@ -2 +1,0 @@\n-line 2\n@@ -5,0 +5 @@\n+X\n",
                "line 1\nline 3\nline 4\nline 5\nX\nline 6\n",
            ),
            (
                "insert then insert",
                numbered(4),
                "@@ -1,0 +2,2 @@\n+A\n+B\n@@ -3,0 +6 @@\n+C\n",
                "line 1\nA\nB\nline 2\nline 3\nC\nline 4\n",
            ),
        ];
        for (name, before, hunks, after) in cases {
            let (d, r) = apply_to(&[("index.js", before)], &format!("{HEADER}{hunks}"));
            r.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(read(&d, "index.js"), *after, "{name}");
        }
    }

    #[test]
    fn insertion_past_the_end_does_not_apply() {
        for (hunk, line) in [("@@ -9,0 +10 @@\n+X\n", 9), ("@@ -3,0 +4 @@\n+X\n", 3)] {
            let (d, r) = apply_to(
                &[("index.js", "line 1\nline 2\n")],
                &format!("{HEADER}{hunk}"),
            );
            assert_eq!(
                r.unwrap_err().to_string(),
                format!("hunk #1 does not apply to index.js (expected at line {line})")
            );
            assert_eq!(read(&d, "index.js"), "line 1\nline 2\n");
        }
    }

    #[test]
    fn misplaced_no_newline_pragmas_do_not_apply() {
        for (before, hunk) in [
            (
                "line 1\nline 2",
                "@@ -1,2 +1,1 @@\n+X\n\\ No newline at end of file\n-line 1\n-line 2\n",
            ),
            (
                "line 1\nline 2",
                "@@ -1,2 +1,3 @@\n+X\n\\ No newline at end of file\n line 1\n line 2\n",
            ),
            (
                "line 1\nline 2\n",
                "@@ -1,2 +1,3 @@\n+X\n\\ No newline at end of file\n line 1\n line 2\n",
            ),
            (
                "line 1\nline 2",
                "@@ -1,2 +1,1 @@\n-line 1\n\\ No newline at end of file\n line 2\n",
            ),
        ] {
            let text = format!("{HEADER}{hunk}");
            let parsed = parse_patch_file(text.as_bytes()).unwrap();
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("index.js"), before).unwrap();
            let err = parsed.apply(dir.path()).unwrap_err();
            assert_eq!(
                err.to_string(),
                "hunk #1 does not apply to index.js (expected at line 1)"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("index.js")).unwrap(),
                before
            );
        }
    }

    #[test]
    fn no_newline_pragmas_where_git_puts_them() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "a\nb",
                "@@ -1,2 +1,3 @@\n a\n-b\n\\ No newline at end of file\n+b\n+c\n",
                "a\nb\nc\n",
            ),
            (
                "a\nb\nc\n",
                "@@ -1,3 +1,2 @@\n a\n-b\n-c\n+b\n\\ No newline at end of file\n",
                "a\nb",
            ),
            (
                "a\nb",
                "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+B\n\\ No newline at end of file\n",
                "a\nB",
            ),
            (
                "a\nb",
                "@@ -1,2 +1,3 @@\n+X\n a\n b\n\\ No newline at end of file\n",
                "X\na\nb",
            ),
            (
                "one\ntwo\nthree",
                "@@ -1,3 +1 @@\n one\n\\ No newline at end of file\n-two\n-three\n\\ No newline at end of file\n",
                "one",
            ),
            (
                "a\nb\n",
                "@@ -1,2 +1 @@\n a\n\\ No newline at end of file\n-b\n",
                "a",
            ),
            (
                "one",
                "@@ -1 +1,3 @@\n one\n+two\n+three\n\\ No newline at end of file\n",
                "one\ntwo\nthree",
            ),
            ("one", "@@ -1 +1,2 @@\n one\n+two\n", "one\ntwo\n"),
            (
                "a\nb\n",
                "@@ -1,2 +1,2 @@\n-a\n+A\n b\n\\ No newline at end of file\n",
                "A\nb",
            ),
            ("a\nb", "@@ -1,2 +1,2 @@\n-a\n+A\n b\n", "A\nb\n"),
        ];
        for (before, hunk, after) in cases {
            let (d, r) = apply_to(&[("index.js", before)], &format!("{HEADER}{hunk}"));
            r.unwrap_or_else(|e| panic!("{hunk:?}: {e}"));
            assert_eq!(read(&d, "index.js"), *after, "{hunk:?}");
        }
    }

    #[test]
    fn far_header_start_and_out_of_order_hunks() {
        let (d, r) = apply_to(
            &[("index.js", "line 1\nline 2\nline 3\n")],
            &format!("{HEADER}@@ -4000000000 +4000000000 @@\n-line 2\n+TWO\n"),
        );
        r.unwrap();
        assert_eq!(read(&d, "index.js"), "line 1\nTWO\nline 3\n");
        let cases: &[(&str, &str)] = &[
            (
                "@@ -6 +6 @@\n-line 6\n+SIX\n@@ -2 +2 @@\n-line 2\n+TWO\n",
                "line 1\nTWO\nline 3\nline 4\nline 5\nSIX\nline 7\nline 8\n",
            ),
            (
                "@@ -5,4 +7,2 @@\n line 5\n-line 6\n-line 7\n line 8\n@@ -2,2 +2,4 @@\n line 2\n+A\n+B\n line 3\n",
                "line 1\nline 2\nA\nB\nline 3\nline 4\nline 5\nline 8\n",
            ),
        ];
        for (hunks, after) in cases {
            let (d, r) = apply_to(&[("index.js", &numbered(8))], &format!("{HEADER}{hunks}"));
            r.unwrap();
            assert_eq!(read(&d, "index.js"), *after);
        }
        let overlap = format!(
            "{HEADER}@@ -4,2 +4,2 @@\n-line 4\n+Y\n line 5\n@@ -2,3 +2,3 @@\n line 2\n-line 3\n+X\n line 4\n"
        );
        let (d, r) = apply_to(&[("index.js", &numbered(8))], &overlap);
        assert_eq!(
            r.unwrap_err().to_string(),
            "hunk #1 does not apply to index.js (expected at line 4)"
        );
        assert_eq!(read(&d, "index.js"), numbered(8));
    }

    #[test]
    fn large_file_patch_is_linear() {
        let (mut before, mut after, mut hunk) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..200_000 {
            let line = format!("const v{i} = {i};");
            if i % 10 == 0 {
                after.push(format!("{line} // x"));
                after.push("// added".to_owned());
                hunk.push(format!("-{line}"));
                hunk.push(format!("+{line} // x"));
                hunk.push("+// added".to_owned());
            } else if i % 10 == 5 {
                hunk.push(format!("-{line}"));
            } else {
                after.push(line.clone());
                hunk.push(format!(" {line}"));
            }
            before.push(line);
        }
        let patch = format!(
            "{HEADER}@@ -1,{} +1,{} @@\n{}\n",
            before.len(),
            after.len(),
            hunk.join("\n")
        );
        let start = std::time::Instant::now();
        let (d, r) = apply_to(&[("index.js", &(before.join("\n") + "\n"))], &patch);
        r.unwrap();
        assert!(read(&d, "index.js") == after.join("\n") + "\n");
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn creation_deletion_rename_and_mode() {
        let patch = "diff --git a/newfile.txt b/newfile.txt\nnew file mode 100644\n--- /dev/null\n+++ b/newfile.txt\n@@ -0,0 +0,0 @@\n";
        let (d, r) = apply_to(&[(".keep", "")], patch);
        r.unwrap();
        assert_eq!(read(&d, "newfile.txt"), "");
        let patch = "diff --git a/dir/new.txt b/dir/new.txt\nnew file mode 100755\n--- /dev/null\n+++ b/dir/new.txt\n@@ -0,0 +1,2 @@\n+a\n+b\n\\ No newline at end of file\n";
        let (d, r) = apply_to(&[(".keep", "")], patch);
        r.unwrap();
        assert_eq!(read(&d, "dir/new.txt"), "a\nb");
        let patch = "diff --git a/old.txt b/moved/new.txt\nsimilarity index 100%\nrename from old.txt\nrename to moved/new.txt\n";
        let (d, r) = apply_to(&[("old.txt", "x\n")], patch);
        r.unwrap();
        assert_eq!(read(&d, "moved/new.txt"), "x\n");
        assert!(!d.path().join("old.txt").exists());
        let patch = "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";
        let (d, r) = apply_to(&[("gone.txt", "bye\n")], patch);
        r.unwrap();
        assert!(!d.path().join("gone.txt").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let patch = "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
            let (d, r) = apply_to(&[("run.sh", "#!/bin/sh\n")], patch);
            r.unwrap();
            assert_eq!(
                std::fs::metadata(d.path().join("run.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn deleting_more_lines_than_the_file_has() {
        let mut body = vec![" only line".to_owned()];
        body.extend((0..9).map(|i| format!("-deleted {i}")));
        let patch = format!(
            "diff --git a/target.txt b/target.txt\n--- a/target.txt\n+++ b/target.txt\n@@ -1,10 +1,1 @@\n{}\n",
            body.join("\n")
        );
        let (_d, r) = apply_to(&[("target.txt", "only line\n")], &patch);
        assert_eq!(
            r.unwrap_err().to_string(),
            "hunk #1 does not apply to target.txt (expected at line 1)"
        );
    }

    #[test]
    fn stays_inside_the_patch_dir() {
        let edit = |p: &str| {
            format!("diff --git a/{p} b/{p}\n--- a/{p}\n+++ b/{p}\n@@ -1 +1 @@\n-SAFE\n+PWNED\n")
        };
        let (d, r) = apply_to(&[("real.txt", "SAFE\n")], &edit("./real.txt"));
        r.unwrap();
        assert_eq!(read(&d, "real.txt"), "PWNED\n");
        for evil in ["../outside.txt", "/etc/outside.txt", "a/../../outside.txt"] {
            let (_d, r) = apply_to(&[("real.txt", "SAFE\n")], &edit(evil));
            assert!(r.is_err(), "{evil}");
        }
        assert!(!is_safe_patch_path(b"a\0/../b"));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("target.txt"), "SAFE\n").unwrap();
            let dir = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path().join("target.txt"), dir.path().join("link"))
                .unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("evildir")).unwrap();
            std::fs::write(dir.path().join("real.txt"), "SAFE\n").unwrap();
            std::fs::hard_link(dir.path().join("real.txt"), dir.path().join("hard.txt")).unwrap();
            for p in ["link", "evildir/target.txt", "hard.txt"] {
                let text = edit(p);
                let parsed = parse_patch_file(text.as_bytes()).unwrap();
                assert!(parsed.apply(dir.path()).is_err(), "{p}");
            }
            assert_eq!(
                std::fs::read_to_string(outside.path().join("target.txt")).unwrap(),
                "SAFE\n"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("real.txt")).unwrap(),
                "SAFE\n"
            );
        }
    }

    #[test]
    fn parse_errors_and_old_style_patches() {
        assert_eq!(
            parse_patch_file(b"@@").unwrap_err().err,
            ParseErr::bad_header_line
        );
        assert_eq!(
            parse_patch_file(b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,1 @@\n-a\n")
                .unwrap_err()
                .err,
            ParseErr::hunk_header_integrity_check_failed
        );
        assert_eq!(
            parse_patch_file(b"diff --git a/x b/x\nGIT binary patch\n")
                .unwrap_err()
                .err,
            ParseErr::git_binary_patch_not_supported
        );
        assert!(parse_patch_file(b"").unwrap().ops.is_empty());
        // patch-package's old style: no `diff --git` lines.
        let old = "--- a/a.js\n+++ b/a.js\n@@ -1 +1 @@\n-x\n+y\n--- a/b.js\n+++ b/b.js\n@@ -1 +1 @@\n-p\n+q\n";
        let (d, r) = apply_to(&[("a.js", "x\n"), ("b.js", "p\n")], old);
        r.unwrap();
        assert_eq!(
            (read(&d, "a.js"), read(&d, "b.js")),
            ("y\n".to_owned(), "q\n".to_owned())
        );
        // A deleted line that begins with "-- a/" inside an incomplete hunk is a deletion.
        let tricky = "--- a/a.js\n+++ b/a.js\n@@ -1,2 +1 @@\n--- a/not-a-header\n x\n";
        let (d, r) = apply_to(&[("a.js", "-- a/not-a-header\nx\n")], tricky);
        r.unwrap();
        assert_eq!(read(&d, "a.js"), "x\n");
        assert_eq!(
            parse_diff_line_paths(b"diff --git a/plan b/run.sh b/plan b/run.sh"),
            Some((&b"plan b/run.sh"[..], &b"plan b/run.sh"[..]))
        );
        assert_eq!(
            parse_diff_line_paths(b"diff --git a/x.js b/y.js"),
            Some((&b"x.js"[..], &b"y.js"[..]))
        );
        assert!(parse_patch_file(b"diff --git a/x b/x\n--- \n+++ \n").is_ok());
        assert_eq!(
            hash_patch(b"abc"),
            u64::from_le_bytes([0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a])
        );
    }

    #[test]
    fn touched_paths_and_warnings() {
        let p = "diff --git a/a b/b\nrename from a\nrename to b\ndiff --git a/c.bin b/c.bin\nBinary files a/c.bin and b/c.bin differ\n";
        let parsed = parse_patch_file(p.as_bytes()).unwrap();
        let mut touched: Vec<String> = parsed.touched_paths().into_iter().collect();
        touched.sort();
        assert_eq!(touched, vec!["a", "b"]);
        assert_eq!(parsed.skipped_warnings().len(), 1);
    }
}
