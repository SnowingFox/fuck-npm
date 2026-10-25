//! Which files of a package go into its tarball (bun `pack_command.rs`): the bins, then
//! either the `files` list or the whole tree minus the default ignores and the `.npmignore`
//! (else `.gitignore`) of every directory. `package.json`, `README*`, `LICENSE*`/`LICENCE*`
//! at the root are always packed, `CHANGELOG*` at the root is never ignored, and `.git`,
//! `.npmrc` and the root's lockfiles are never packed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::glob::glob_match;
use super::json::Json;
use super::out::{FResult, Out, show};

/// The paths to archive, relative to the package root, sorted by bytes and without
/// duplicates (bun's `PackQueue` pops them in this order).
pub(crate) type PackQueue = BTreeSet<Vec<u8>>;

const ROOT_DEFAULT_IGNORE_PATTERNS: &[&[u8]] = &[
    b"package-lock.json",
    b"yarn.lock",
    b"pnpm-lock.yaml",
    b"bun.lockb",
    b"bun.lock",
    b"fnpm.lock",
];

/// `(pattern, can_override)`: `files` can re-include the patterns with `true`.
const DEFAULT_IGNORE_PATTERNS: &[(&[u8], bool)] = &[
    (b".*.swp", true),
    (b"._*", true),
    (b".DS_Store", true),
    (b".git", false),
    (b".gitignore", true),
    (b".hg", true),
    (b".npmignore", true),
    (b".npmrc", false),
    (b".lock-wscript", true),
    (b".svn", true),
    (b".wafpickle-*", true),
    (b"CVS", true),
    (b"npm-debug.log", true),
    (b".env.production", true),
    (b"bunfig.toml", true),
    (b"fnpm.toml", true),
    // The temporary tarball of a pack that runs at the same time, or was killed.
    (b".fnpm-pack.*.tmp", false),
];

/// What a directory entry is, without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Other,
}

pub(crate) struct Entry {
    pub name: Vec<u8>,
    pub kind: Kind,
}

#[cfg(unix)]
pub(crate) fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(not(unix))]
pub(crate) fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
pub(crate) fn bytes_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
pub(crate) fn bytes_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

pub(crate) fn kind_of(meta: &std::fs::Metadata) -> Kind {
    let t = meta.file_type();
    if t.is_file() {
        Kind::File
    } else if t.is_dir() {
        Kind::Dir
    } else {
        Kind::Other
    }
}

/// Lists `dir`. A failure is fatal: a tarball without the files that were not read would be
/// wrong. A directory removed after it was opened has no entries.
pub(crate) fn read_dir(out: &Out, dir: &Path, subpath: &[u8]) -> FResult<Vec<Entry>> {
    let shown = if subpath.is_empty() {
        ".".to_owned()
    } else {
        show(subpath)
    };
    list_dir(dir).map_err(|e| match e {
        ListError::Open(e) => out.named(
            &super::out::errno_name(&e),
            &format!("failed to open directory \"{shown}\" for packing"),
        ),
        ListError::Read(e, syscall) => out.sys(
            &e,
            syscall,
            &format!("failed to read directory \"{shown}\" for packing"),
        ),
    })
}

pub(crate) enum ListError {
    Open(std::io::Error),
    Read(std::io::Error, &'static str),
}

/// The entries of `dir` with their kinds (`lstat` where the file system does not report one).
#[cfg(target_os = "linux")]
pub(crate) fn list_dir(dir: &Path) -> Result<Vec<Entry>, ListError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(dir)
        .map_err(ListError::Open)?;
    let fd = file.as_raw_fd();
    let mut buf = vec![0u8; 8192];
    let mut entries = Vec::new();
    loop {
        // SAFETY: `buf` is valid for `buf.len()` bytes and `fd` is an open directory.
        let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::ENOENT) => break,
                _ => return Err(ListError::Read(e, "getdents64")),
            }
        }
        if n == 0 {
            break;
        }
        let mut off = 0usize;
        while off < n as usize {
            let rec = &buf[off..];
            let reclen = u16::from_ne_bytes([rec[16], rec[17]]) as usize;
            let d_type = rec[18];
            let name_bytes = &rec[19..reclen];
            let name_len = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            let name = name_bytes[..name_len].to_vec();
            off += reclen;
            if name == b"." || name == b".." {
                continue;
            }
            let kind = match d_type {
                libc::DT_REG => Kind::File,
                libc::DT_DIR => Kind::Dir,
                libc::DT_UNKNOWN => match std::fs::symlink_metadata(dir.join(bytes_path(&name))) {
                    Ok(meta) => kind_of(&meta),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(ListError::Read(e, "lstat")),
                },
                _ => Kind::Other,
            };
            entries.push(Entry { name, kind });
        }
    }
    Ok(entries)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn list_dir(dir: &Path) -> Result<Vec<Entry>, ListError> {
    let iter = std::fs::read_dir(dir).map_err(ListError::Open)?;
    let mut entries = Vec::new();
    for entry in iter {
        let entry = entry.map_err(|e| ListError::Read(e, "readdir"))?;
        let kind = match entry.file_type() {
            Ok(t) if t.is_file() => Kind::File,
            Ok(t) if t.is_dir() => Kind::Dir,
            Ok(_) => Kind::Other,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(ListError::Read(e, "lstat")),
        };
        entries.push(Entry {
            name: os_bytes(&entry.file_name()),
            kind,
        });
    }
    Ok(entries)
}

pub(crate) fn join_sub(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        return name.to_vec();
    }
    let mut out = Vec::with_capacity(dir.len() + 1 + name.len());
    out.extend_from_slice(dir);
    out.push(b'/');
    out.extend_from_slice(name);
    out
}

/// bun `normalize_buf::<Posix>`: `.` and empty segments dropped, `..` removes the previous
/// one (and is dropped at the start of a relative path), a trailing `/` kept; `\` also
/// separates. An empty path is `.`.
pub(crate) fn normalize_posix(s: &[u8]) -> Vec<u8> {
    if s.is_empty() {
        return b".".to_vec();
    }
    let allow_above_root = s[0] == b'/';
    let trailing = s.last() == Some(&b'/');
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    let mut buf: Vec<u8> = Vec::with_capacity(s.len());
    let mut dotdot = 0usize;
    let n = s.len();
    let mut r = 0;
    while r < n {
        if is_sep(s[r]) {
            r += 1;
            continue;
        }
        if s[r] == b'.' && (r + 1 == n || is_sep(s[r + 1])) {
            r += 1;
            continue;
        }
        if s[r..].starts_with(b"..") && (r + 2 == n || is_sep(s[r + 2])) {
            r += 2;
            if buf.len() > dotdot {
                let mut i = buf.len() - 1;
                while i > dotdot && !is_sep(buf[i]) {
                    i -= 1;
                }
                buf.truncate(i);
            } else if allow_above_root {
                if !buf.is_empty() {
                    buf.extend_from_slice(b"/..");
                } else {
                    buf.extend_from_slice(b"..");
                }
                dotdot = buf.len();
            }
            continue;
        }
        if !buf.is_empty() && !is_sep(buf[buf.len() - 1]) {
            buf.push(b'/');
        }
        while r < n && !is_sep(s[r]) {
            buf.push(s[r]);
            r += 1;
        }
    }
    if trailing && !buf.is_empty() && buf[buf.len() - 1] != b'/' {
        buf.push(b'/');
    }
    buf
}

/// A pattern of `files`, `.npmignore` or `.gitignore`.
#[derive(Debug, Clone)]
pub(crate) struct Pattern {
    /// With a leading `!` when negated.
    pub glob: Vec<u8>,
    /// Had a leading or middle `/`: matched against the path, not the name.
    pub rel_path: bool,
    /// Had a trailing `/`: only matches directories.
    pub dirs_only: bool,
    pub negated: bool,
}

impl Pattern {
    pub fn parse(pattern: &[u8]) -> Option<Pattern> {
        let mut remain = pattern;
        while remain.first() == Some(&b'!') {
            remain = &remain[1..];
        }
        let negated = remain.len() != pattern.len();
        let mut dirs_only = false;
        let mut rel_path = false;
        if let Some(trimmed) = remain.strip_suffix(b"/") {
            dirs_only = true;
            remain = trimmed;
        }
        if let Some(trimmed) = remain.strip_prefix(b"/") {
            rel_path = true;
            remain = trimmed;
        } else if remain
            .iter()
            .position(|&b| b == b'/')
            .is_some_and(|i| i != remain.len() - 1)
        {
            rel_path = true;
        }
        if remain.is_empty() {
            return None;
        }
        // In npm and git this `!` negates and the `(` is a plain character.
        let negation: &[u8] = match negated {
            true if remain.starts_with(b"(") => b"!\\",
            true => b"!",
            false => b"",
        };
        Some(Pattern {
            glob: [negation, remain].concat(),
            rel_path,
            dirs_only,
            negated,
        })
    }

    /// The pattern without its `!`.
    pub fn as_positive(&self) -> Pattern {
        Pattern {
            glob: self.glob[1..].to_vec(),
            negated: false,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreKind {
    Default,
    Npmignore,
    Gitignore,
    PackageJson,
}

impl IgnoreKind {
    pub fn name(self) -> &'static str {
        match self {
            IgnoreKind::Default => "default",
            IgnoreKind::Npmignore => ".npmignore",
            IgnoreKind::Gitignore => ".gitignore",
            IgnoreKind::PackageJson => "package.json",
        }
    }
}

struct IgnorePatterns {
    list: Vec<Pattern>,
    kind: IgnoreKind,
    depth: usize,
    has_rel_path: bool,
}

/// git's `trim_trailing_spaces`: `foo\ ` keeps its space.
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
    let mut last_space: Option<usize> = None;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' => {
                if last_space.is_none() {
                    last_space = Some(i);
                }
            }
            b'\\' => {
                i += 1;
                if i == line.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    match last_space {
        Some(end) => &line[..end],
        None => line,
    }
}

/// The `.npmignore` of `dir`, else its `.gitignore`. A file that cannot be read is fatal,
/// since the tarball would get the files it ignores.
fn read_ignore_file(out: &Out, dir: &Path, depth: usize) -> FResult<Option<IgnorePatterns>> {
    for kind in [IgnoreKind::Npmignore, IgnoreKind::Gitignore] {
        let path = dir.join(kind.name());
        let fail = |action: &str, e: &std::io::Error| {
            let dir_shown = dir.to_string_lossy();
            out.named(
                &super::out::errno_name(e),
                &format!(
                    "failed to {action} {} at: \"{}/{}\"",
                    kind.name(),
                    dir_shown.trim_end_matches('/'),
                    kind.name()
                ),
            )
        };
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(fail("open", &e)),
        };
        let mut contents = Vec::new();
        use std::io::Read;
        (&file)
            .read_to_end(&mut contents)
            .map_err(|e| fail("read", &e))?;
        let body = contents
            .strip_prefix(b"\xef\xbb\xbf".as_slice())
            .unwrap_or(&contents);
        let mut list = Vec::new();
        let mut has_rel_path = false;
        for line in body.split(|&b| b == b'\n') {
            if line.starts_with(b"#") {
                continue;
            }
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(parsed) = Pattern::parse(trim_trailing_spaces(line)) else {
                continue;
            };
            has_rel_path |= parsed.rel_path;
            list.push(parsed);
        }
        return Ok((!list.is_empty()).then_some(IgnorePatterns {
            list,
            kind,
            depth,
            has_rel_path,
        }));
    }
    Ok(None)
}

fn strings_eql(a: &[u8], b: &[u8]) -> bool {
    if cfg!(target_os = "linux") {
        a == b
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

/// `name` itself, or `name` with an extension.
pub(crate) fn is_special_file_or_variant(filename: &[u8], name: &[u8]) -> bool {
    if filename.len() < name.len() {
        return false;
    }
    let (stem, extension) = filename.split_at(name.len());
    strings_eql(stem, name)
        && (extension.is_empty() || (extension.len() > 1 && extension[0] == b'.'))
}

/// Packed even if ignored or not in `files`.
pub(crate) fn is_unconditionally_included_file(filename: &[u8]) -> bool {
    filename.len() > 5
        && (strings_eql(filename, b"package.json")
            || is_special_file_or_variant(filename, b"LICENSE")
            || is_special_file_or_variant(filename, b"LICENCE")
            || is_special_file_or_variant(filename, b"README"))
}

/// The default ignores that nothing can re-include.
pub(crate) fn is_unconditionally_excluded(name: &[u8], depth: usize) -> Option<&'static [u8]> {
    let root_only: &[&[u8]] = if depth == 1 {
        ROOT_DEFAULT_IGNORE_PATTERNS
    } else {
        &[]
    };
    let strict = DEFAULT_IGNORE_PATTERNS
        .iter()
        .filter(|(_, can_override)| !can_override)
        .map(|(p, _)| *p);
    root_only
        .iter()
        .copied()
        .chain(strict)
        .find(|pattern| glob_match(pattern, name).matches)
}

/// bun's `is_excluded` below the root without ignore files: every default ignore applies.
pub(crate) fn default_ignore(name: &[u8]) -> Option<&'static [u8]> {
    DEFAULT_IGNORE_PATTERNS
        .iter()
        .map(|(p, _)| *p)
        .find(|pattern| glob_match(pattern, name).matches)
}

/// The pattern that excluded the walked-only directory an entry is in.
pub(crate) type Inherited = Option<(Vec<u8>, IgnoreKind)>;

enum Exclusion {
    Included,
    Excluded(Vec<u8>, IgnoreKind),
    /// An excluded directory with a negated pattern below it: walked, but its entries stay
    /// excluded unless a negation names them.
    WalkOnly(Vec<u8>, IgnoreKind),
}

/// True if a negated pattern could match a path below the directory `dir_rel`.
fn negation_reaches_below(pattern: &Pattern, dir_rel: &[u8]) -> bool {
    if !pattern.rel_path {
        return false;
    }
    let mut glob_segments = pattern.glob[1..].split(|&b| b == b'/');
    for dir_name in dir_rel.split(|&b| b == b'/') {
        match glob_segments.next() {
            Some(b"**") => return true,
            Some(segment) if glob_match(segment, dir_name).matches => {}
            _ => return false,
        }
    }
    glob_segments.next().is_some()
}

fn is_excluded(
    name: &[u8],
    kind: Kind,
    subpath: &[u8],
    depth: usize,
    ignores: &[IgnorePatterns],
    inherited: &Inherited,
) -> Exclusion {
    if depth == 1
        && (is_unconditionally_included_file(name)
            || is_special_file_or_variant(name, b"CHANGELOG"))
    {
        return Exclusion::Included;
    }
    if let Some(pattern) = is_unconditionally_excluded(name, depth) {
        return Exclusion::Excluded(pattern.to_vec(), IgnoreKind::Default);
    }
    let (mut ignored, mut ignore_pattern, mut ignore_kind) = match inherited {
        Some((pattern, kind)) => (true, pattern.clone(), *kind),
        None => (false, Vec::new(), IgnoreKind::Npmignore),
    };
    let mut walk_only = false;
    for &(pattern, can_override) in DEFAULT_IGNORE_PATTERNS {
        if can_override && glob_match(pattern, name).matches {
            ignored = true;
            ignore_pattern = pattern.to_vec();
            ignore_kind = IgnoreKind::Default;
            break;
        }
    }
    for ignore in ignores {
        let mut rel: &[u8] = subpath;
        if ignore.has_rel_path {
            for _ in 1..ignore.depth {
                if let Some(sep) = rel.iter().position(|&b| b == b'/') {
                    rel = &rel[sep + 1..];
                }
            }
        }
        for pattern in &ignore.list {
            if pattern.dirs_only && kind != Kind::Dir {
                continue;
            }
            let target = if pattern.rel_path { rel } else { name };
            let result = glob_match(&pattern.glob, target);
            if result.negated {
                if !result.matches {
                    ignored = false;
                } else if ignored && kind == Kind::Dir && negation_reaches_below(pattern, rel) {
                    walk_only = true;
                }
            } else if result.matches {
                ignored = true;
                walk_only = false;
                ignore_pattern = pattern.glob.clone();
                ignore_kind = ignore.kind;
            }
        }
    }
    if !ignored {
        Exclusion::Included
    } else if walk_only {
        Exclusion::WalkOnly(ignore_pattern, ignore_kind)
    } else {
        Exclusion::Excluded(ignore_pattern, ignore_kind)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinType {
    File,
    Dir,
}

#[derive(Debug, Clone)]
pub(crate) struct BinInfo {
    pub path: Vec<u8>,
    pub ty: BinType,
}

/// npm: an empty `"bin"` (`""`, `{}`) counts as absent, so `"directories.bin"` applies.
pub(crate) fn non_empty_bin(json: &Json) -> Option<&Json> {
    json.get("bin").filter(|bin| match bin {
        Json::String(s) => !s.is_empty(),
        Json::Object(o) => !o.is_empty(),
        _ => true,
    })
}

pub(crate) fn is_package_root_or_outside(p: &[u8]) -> bool {
    p.is_empty() || p == b"." || p.first() == Some(&b'/') || p == b".." || p.starts_with(b"../")
}

pub(crate) fn bin_subpath(value: &str, ty: BinType) -> Option<Vec<u8>> {
    let normalized = normalize_posix(value.as_bytes());
    let subpath = match ty {
        BinType::Dir => {
            let mut n = normalized;
            while n.last() == Some(&b'/') {
                n.pop();
            }
            n
        }
        BinType::File if normalized.ends_with(b"/") || normalized == b"package.json" => {
            return None;
        }
        BinType::File => normalized,
    };
    let never_packed = subpath
        .split(|&b| b == b'/')
        .any(|name| is_unconditionally_excluded(name, 2).is_some());
    (!is_package_root_or_outside(&subpath) && !never_packed).then_some(subpath)
}

pub(crate) fn get_package_bins(json: &Json) -> Vec<BinInfo> {
    let mut bins = Vec::new();
    let mut add = |value: &Json, ty: BinType| {
        if let Some(s) = value.as_str()
            && let Some(path) = bin_subpath(s, ty)
        {
            bins.push(BinInfo { path, ty });
        }
    };
    if let Some(bin) = non_empty_bin(json) {
        match bin {
            Json::Object(props) => {
                for (_, value) in props {
                    add(value, BinType::File);
                }
            }
            other => add(other, BinType::File),
        }
    } else if let Some(Json::Object(directories)) = json.get("directories")
        && let Some((_, bin)) = directories.iter().find(|(k, _)| k == "bin")
    {
        add(bin, BinType::Dir);
    }
    bins
}

/// What `bin_path` is below `root`, without following symlinks; `None` if it is missing or
/// a parent component is not a real directory.
pub(crate) fn bin_kind_without_following_symlinks(
    out: &Out,
    root: &Path,
    bin_path: &[u8],
) -> FResult<Option<Kind>> {
    let mut bin_path = bin_path;
    while bin_path.last() == Some(&b'/') {
        bin_path = &bin_path[..bin_path.len() - 1];
    }
    let ends = bin_path
        .iter()
        .enumerate()
        .filter_map(|(i, &c)| (c == b'/').then_some(i))
        .chain([bin_path.len()]);
    for end in ends {
        let abs = root.join(bytes_path(&bin_path[..end]));
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if is_absent(&e) => break,
            Err(e) => {
                return Err(out.sys(
                    &e,
                    "lstat",
                    &format!("failed to stat file: \"{}\"", show(bin_path)),
                ));
            }
        };
        let kind = kind_of(&meta);
        if end == bin_path.len() {
            return Ok(Some(kind));
        }
        if kind != Kind::Dir {
            break;
        }
    }
    Ok(None)
}

/// Nothing exists at the path (`ELOOP` comes from a cycle of symlinks).
pub(crate) fn is_absent(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::NotFound {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        return matches!(
            code,
            libc::ENOENT | libc::ENOTDIR | libc::ENAMETOOLONG | libc::ELOOP
        );
    }
    false
}

fn is_listed_bin(bins: &[BinInfo], ty: BinType, subpath: &[u8]) -> bool {
    bins.iter().any(|bin| bin.ty == ty && bin.path == subpath)
}

/// True if a file at `path` gets `+x` (a listed bin, or a file directly in `directories.bin`).
pub(crate) fn is_package_bin(bins: &[BinInfo], path: &[u8]) -> bool {
    bins.iter().any(|bin| match bin.ty {
        BinType::File => bin.path == path,
        BinType::Dir => path
            .strip_prefix(bin.path.as_slice())
            .and_then(|rest| rest.strip_prefix(b"/"))
            .is_some_and(|name| !name.is_empty() && !name.contains(&b'/')),
    })
}

/// A directory to walk: its path on disk, its path in the tarball, its depth (1 = root).
pub(crate) struct DirInfo {
    pub abs: PathBuf,
    pub subpath: Vec<u8>,
    pub depth: usize,
}

/// Walks the tree, verbose prints the ignored entries.
pub(crate) struct Walker<'a> {
    pub out: &'a Out,
    pub verbose: bool,
}

impl Walker<'_> {
    pub fn print_ignored(&self, kind: IgnoreKind, pattern: &[u8], subpath: &[u8], entry: Kind) {
        if !self.verbose {
            return;
        }
        self.out.println(&format!(
            "{} {} {}{}",
            self.out.paint("34", "ignore"),
            self.out
                .paint("2", &format!("[{}:{}]", kind.name(), show(pattern))),
            show(subpath),
            if entry == Kind::Dir { "/" } else { "" }
        ));
    }

    /// The whole tree below `root`, minus the ignores. `files_excludes` is set when `root` is
    /// a directory that is packed whole: one that `files` includes (with the negated entries
    /// of `files`), or a bundled folder that has no package.json.
    pub fn project_tree(
        &self,
        queue: &mut PackQueue,
        bins: &[BinInfo],
        files_excludes: Option<&[Pattern]>,
        root: DirInfo,
    ) -> FResult<()> {
        let root_depth = root.depth;
        let mut ignores: Vec<IgnorePatterns> = Vec::new();
        if let Some(excludes) = files_excludes.filter(|e| !e.is_empty()) {
            ignores.push(IgnorePatterns {
                list: excludes.iter().map(Pattern::as_positive).collect(),
                kind: IgnoreKind::PackageJson,
                depth: 1,
                has_rel_path: false,
            });
        }
        let mut dirs: Vec<(DirInfo, Inherited)> = vec![(root, None)];
        while let Some((dir, inherited)) = dirs.pop() {
            while ignores.last().is_some_and(|last| last.depth >= dir.depth) {
                ignores.pop();
            }
            // Opened before its ignore file is read, as bun opens it.
            let entries = read_dir(self.out, &dir.abs, &dir.subpath)?;
            if let Some(patterns) = read_ignore_file(self.out, &dir.abs, dir.depth)? {
                ignores.push(patterns);
            }
            for entry in entries {
                if entry.kind == Kind::Other {
                    continue;
                }
                let name = &entry.name;
                let subpath = join_sub(&dir.subpath, name);
                if dir.depth == 1 {
                    if (name == b"package.json" && entry.kind == Kind::File)
                        || name == b"node_modules"
                    {
                        continue;
                    }
                } else if files_excludes.is_some()
                    && dir.depth == root_depth
                    && entry.kind == Kind::Dir
                    && name == b"node_modules"
                {
                    continue;
                }
                let inherit = match is_excluded(
                    name, entry.kind, &subpath, dir.depth, &ignores, &inherited,
                ) {
                    Exclusion::Included => None,
                    Exclusion::WalkOnly(pattern, kind) => Some((pattern, kind)),
                    Exclusion::Excluded(pattern, kind) => {
                        self.print_ignored(kind, &pattern, &subpath, entry.kind);
                        continue;
                    }
                };
                if entry.kind == Kind::File {
                    if !is_listed_bin(bins, BinType::File, &subpath) {
                        queue.insert(subpath);
                    }
                } else if !is_listed_bin(bins, BinType::Dir, &subpath) {
                    let abs = dir.abs.join(bytes_path(name));
                    dirs.push((
                        DirInfo {
                            abs,
                            subpath,
                            depth: dir.depth + 1,
                        },
                        inherit,
                    ));
                }
            }
        }
        Ok(())
    }

    /// The tree as `files` selects it.
    fn included_project_tree(
        &self,
        queue: &mut PackQueue,
        bins: &[BinInfo],
        includes: &[Pattern],
        excludes: &[Pattern],
        root: &Path,
    ) -> FResult<()> {
        let mut dirs = vec![DirInfo {
            abs: root.to_path_buf(),
            subpath: Vec::new(),
            depth: 1,
        }];
        let mut included_dirs = Vec::new();
        while let Some(dir) = dirs.pop() {
            for entry in read_dir(self.out, &dir.abs, &dir.subpath)? {
                if entry.kind == Kind::Other {
                    continue;
                }
                let name = &entry.name;
                let subpath = join_sub(&dir.subpath, name);
                let mut included = false;
                let mut unconditional = false;
                if dir.depth == 1 {
                    if name == b"package.json" || name == b"node_modules" {
                        continue;
                    }
                    if entry.kind == Kind::File && is_unconditionally_included_file(name) {
                        included = true;
                        unconditional = true;
                    }
                }
                if let Some(pattern) = is_unconditionally_excluded(name, dir.depth) {
                    self.print_ignored(IgnoreKind::Default, pattern, &subpath, entry.kind);
                    continue;
                }
                if !included {
                    for include in includes {
                        if include.dirs_only && entry.kind != Kind::Dir {
                            continue;
                        }
                        if glob_match(&include.glob, &subpath).matches {
                            included = true;
                        }
                    }
                }
                if included && !unconditional {
                    for exclude in excludes {
                        if exclude.dirs_only && entry.kind != Kind::Dir {
                            continue;
                        }
                        let result = glob_match(&exclude.glob, &subpath);
                        if result.negated && !result.matches {
                            included = false;
                        }
                    }
                }
                if entry.kind == Kind::File {
                    if included && !is_listed_bin(bins, BinType::File, &subpath) {
                        queue.insert(subpath);
                    }
                    continue;
                }
                if is_listed_bin(bins, BinType::Dir, &subpath) {
                    continue;
                }
                let sub = DirInfo {
                    abs: dir.abs.join(bytes_path(name)),
                    subpath,
                    depth: dir.depth + 1,
                };
                if included {
                    included_dirs.push(sub);
                } else {
                    dirs.push(sub);
                }
            }
        }
        for dir in included_dirs {
            self.project_tree(queue, bins, Some(excludes), dir)?;
        }
        Ok(())
    }

    /// Everything the tarball gets besides package.json and the bundled dependencies: bins,
    /// then either the `files` list or the whole tree minus ignores.
    pub fn published_files(&self, root: &Path, json: &Json) -> FResult<(PackQueue, Vec<BinInfo>)> {
        let mut queue = PackQueue::new();
        let bins = get_package_bins(json);
        for bin in &bins {
            let kind = bin_kind_without_following_symlinks(self.out, root, &bin.path)?;
            match bin.ty {
                BinType::File if kind == Some(Kind::File) => {
                    queue.insert(bin.path.clone());
                }
                BinType::Dir if kind == Some(Kind::Dir) => {
                    self.project_tree(
                        &mut queue,
                        &[],
                        None,
                        DirInfo {
                            abs: root.join(bytes_path(&bin.path)),
                            subpath: bin.path.clone(),
                            depth: 2,
                        },
                    )?;
                }
                _ => {}
            }
        }
        let Some(files) = json.get("files") else {
            self.project_tree(
                &mut queue,
                &bins,
                None,
                DirInfo {
                    abs: root.to_path_buf(),
                    subpath: Vec::new(),
                    depth: 1,
                },
            )?;
            return Ok((queue, bins));
        };
        let fail = || {
            self.out
                .error("expected `files` to be an array of string values")
        };
        let Json::Array(items, _) = files else {
            return Err(fail());
        };
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        for item in items {
            let Some(s) = item.as_str() else {
                return Err(fail());
            };
            match Pattern::parse(&normalize_posix(s.as_bytes())) {
                Some(p) if p.negated => excludes.push(p),
                Some(p) => includes.push(p),
                None => {}
            }
        }
        self.included_project_tree(&mut queue, &bins, &includes, &excludes, root)?;
        Ok((queue, bins))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize() {
        assert_eq!(normalize_posix(b"./dist"), b"dist");
        assert_eq!(normalize_posix(b"./////src//index.ts"), b"src/index.ts");
        assert_eq!(normalize_posix(b"!./subdir"), b"!./subdir");
        assert_eq!(normalize_posix(b"a/../b/"), b"b/");
        assert_eq!(normalize_posix(b"../x"), b"x");
        assert_eq!(normalize_posix(b"./"), b"");
        assert_eq!(normalize_posix(b""), b".");
    }

    #[test]
    fn patterns() {
        let p = Pattern::parse(b"!(a).txt").unwrap();
        assert!(p.negated && p.glob == b"!\\(a).txt" && !p.rel_path);
        let p = Pattern::parse(b"/dist/").unwrap();
        assert!(p.rel_path && p.dirs_only && p.glob == b"dist");
        let p = Pattern::parse(b"a/b").unwrap();
        assert!(p.rel_path);
        assert!(Pattern::parse(b"/").is_none());
        assert_eq!(trim_trailing_spaces(b"spaced\\ "), b"spaced\\ ");
        assert_eq!(trim_trailing_spaces(b"notes.txt  "), b"notes.txt");
    }

    #[test]
    fn special_files() {
        assert!(is_unconditionally_included_file(b"README.md"));
        assert!(is_unconditionally_included_file(b"LICENCE"));
        assert!(!is_unconditionally_included_file(b"READMEX"));
        assert!(is_unconditionally_excluded(b"package-lock.json", 1).is_some());
        assert!(is_unconditionally_excluded(b"package-lock.json", 2).is_none());
        assert!(is_unconditionally_excluded(b".git", 3).is_some());
    }
}
