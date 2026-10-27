//! `.bin` links (bun `install/bin.rs`).
//!
//! POSIX: a relative symlink `<bin dir>/<name> -> ../<pkg>/<target>`, the target made
//! executable (`0o777 & !umask`, only when its mode differs) and a `#!...\r\n` shebang rewritten
//! to `\n`. Windows: cmd-shim compatible `<name>.cmd`, `<name>.ps1` and `<name>` (sh) files.
//! The first package to claim a name in one bin directory keeps it ([`BinLinker`] remembers
//! the names it has seen; callers link direct dependencies first, see [`LinkOrder`]). Targets
//! that leave the package (absolute, drive letters, NUL, `..` climbing, or a symlinked parent
//! that resolves outside) are skipped like npm's `bin-links` and bun do.

use std::collections::HashSet;
#[cfg(unix)]
use std::io::Write;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fnpm_types::{Bin, ResolutionTag};

use crate::fsutil::{self, Kind};
use crate::{Error, Result};

/// One link that was written (or found already correct).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedBin {
    /// The command name (`.bin/<name>`).
    pub name: String,
    /// The absolute path of the file it runs.
    pub target: PathBuf,
}

/// The mode of a bin target: `0o777 & !umask`, read once.
///
/// Reading the umask with `umask(2)` briefly sets it to 0 for the whole process, so files and
/// directories other threads create in that window would be world-writable. Linux reads it
/// from `/proc/self/status` instead; elsewhere [`crate::Linker::link`] calls this before any
/// link thread starts (bun `ensure_umask`).
pub fn exec_mode() -> u32 {
    static MODE: OnceLock<u32> = OnceLock::new();
    *MODE.get_or_init(|| 0o777 & !current_umask())
}

#[cfg(unix)]
fn current_umask() -> u32 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(m) = umask_from_proc() {
        return m;
    }
    // SAFETY: `umask` has no memory-safety preconditions; the value is restored at once.
    let prev = unsafe { libc::umask(0) };
    // SAFETY: as above.
    unsafe { libc::umask(prev) };
    #[allow(clippy::useless_conversion)] // `mode_t` is u16 on macOS.
    let prev = u32::from(prev);
    prev & 0o777
}

#[cfg(not(unix))]
fn current_umask() -> u32 {
    0
}

/// The `Umask:` line of `/proc/self/status` (Linux 4.7+), which reading does not change.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn umask_from_proc() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|l| l.strip_prefix("Umask:"))?;
    u32::from_str_radix(line.trim(), 8).ok().map(|m| m & 0o777)
}

/// npm's bin name normalisation (bun `normalized_bin_name`): the part after the last `/`, `\`
/// or `:`; empty for `.`, `..`, empty names and names with control characters.
pub fn normalized_bin_name(name: &str) -> &str {
    let base = name.rsplit(['/', '\\', ':']).next().unwrap_or(name);
    if matches!(base, "" | "." | "..") || fnpm_types::name::contains_control_character(base) {
        return "";
    }
    base
}

/// True when a `bin` target resolves outside the package: absolute, a drive letter or other
/// `:` in the first component, NUL, or `..` climbing above the start (CVE-2019-16775).
pub fn bin_target_escapes_package_dir(target: &str) -> bool {
    let first = target.split(['/', '\\']).next().unwrap_or("");
    if first.contains(':') {
        return true;
    }
    path_climbs_out(target)
}

/// Absolute, or `..` components lead above the directory it starts in.
pub fn path_climbs_out(target: &str) -> bool {
    if target.starts_with('/') || target.starts_with('\\') || Path::new(target).is_absolute() {
        return true;
    }
    if target.contains('\0') {
        return true;
    }
    let mut depth: isize = 0;
    for c in target.split(['/', '\\']) {
        match c {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

/// A target with a directory part may reach through a symlinked directory inside the package.
fn needs_resolved_containment_check(target: &str) -> bool {
    let mut parts = target.split(['/', '\\']).filter(|s| !s.is_empty());
    let Some(first) = parts.next() else {
        return false;
    };
    first == "." || first == ".." || parts.next().is_some()
}

/// The order in which the packages of one `node_modules` link their bins (bun `LinkOrder`):
/// direct dependencies of the root or a workspace first, then registry/git/tarball packages
/// before local directories, then by dependency name. Sorting by this key makes "first claimant
/// wins" deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinkOrder<'a> {
    /// Neither the root nor a workspace declares the dependency.
    pub transitive: bool,
    /// A workspace, `file:` or `link:` directory.
    pub local_directory: bool,
    /// The dependency (alias) name.
    pub name: &'a str,
}

impl<'a> LinkOrder<'a> {
    /// Builds the sort key.
    pub fn new(direct: bool, resolution: ResolutionTag, name: &'a str) -> Self {
        Self {
            transitive: !direct,
            local_directory: matches!(
                resolution,
                ResolutionTag::Root
                    | ResolutionTag::Workspace
                    | ResolutionTag::Folder
                    | ResolutionTag::Symlink
            ),
            name,
        }
    }
}

/// Options for one package's bins.
#[derive(Debug, Clone, Copy, Default)]
pub struct BinOptions<'a> {
    /// Native binlink redirect: resolve targets inside this platform package instead (bun
    /// `resolve_bin_target`), falling back to the package itself when nothing is found there.
    pub redirect_dir: Option<&'a Path>,
    /// The package was (re)materialised in this run: always fix the target's mode and shebang.
    /// When false and the link already existed with the same text, the target is left alone.
    pub refresh: bool,
    /// Where the files really are, when `package_dir` (which the link text is computed from)
    /// does not resolve yet, e.g. a global-store dependency that is still in its staging
    /// directory. Existence checks, `chmod` and directory listing use this path.
    pub real_dir: Option<&'a Path>,
    /// Like `real_dir`, for `redirect_dir`.
    pub real_redirect_dir: Option<&'a Path>,
}

/// Links bins into one bin directory, remembering which names are taken.
#[derive(Debug)]
pub struct BinLinker {
    bin_dir: PathBuf,
    seen: HashSet<String>,
    /// Global bin dir: only links that lead into this directory may be replaced.
    owner: Option<PathBuf>,
    #[cfg(not(windows))]
    created_dir: bool,
}

/// The outcome of linking one name.
enum One {
    Linked,
    Skipped,
    Missing,
}

impl BinLinker {
    /// A linker for `bin_dir` (e.g. `node_modules/.bin`), created on first use.
    pub fn new(bin_dir: impl Into<PathBuf>) -> Self {
        Self {
            bin_dir: bin_dir.into(),
            seen: HashSet::new(),
            owner: None,
            #[cfg(not(windows))]
            created_dir: false,
        }
    }

    /// A linker for the global bin directory, which also holds files fnpm did not create:
    /// existing entries are only replaced when they are links into `global_node_modules`, and
    /// every name already present is treated as taken (bun pre-seeds `seen`).
    pub fn global(bin_dir: impl Into<PathBuf>, global_node_modules: impl Into<PathBuf>) -> Self {
        let mut l = Self::new(bin_dir);
        l.owner = Some(global_node_modules.into());
        l
    }

    /// Marks every name already in the bin directory as taken (bun `see_names_in`).
    pub fn seed_existing(&mut self) {
        let Ok(rd) = std::fs::read_dir(&self.bin_dir) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let name = name
                .strip_suffix(".cmd")
                .or_else(|| name.strip_suffix(".ps1"))
                .map(str::to_owned)
                .unwrap_or(name);
            self.seen.insert(name);
        }
    }

    /// Marks `name` as taken without linking it.
    pub fn claim(&mut self, name: &str) {
        self.seen.insert(name.to_owned());
    }

    /// Whether `name` is already taken in this directory.
    pub fn is_taken(&self, name: &str) -> bool {
        self.seen.contains(name)
    }

    /// The bin directory.
    pub fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }

    /// Links the bins of the package installed at `package_dir` whose folder name in
    /// `node_modules` is `package_name` (the alias). Links every bin it can; the first error
    /// is returned after the rest were attempted.
    pub fn link(
        &mut self,
        package_name: &str,
        bin: &Bin,
        package_dir: &Path,
        opts: BinOptions<'_>,
    ) -> Result<Vec<LinkedBin>> {
        let real = opts.real_dir.unwrap_or(package_dir);
        if let Some(redirect) = opts.redirect_dir {
            let mut out = Vec::new();
            let mut err = None;
            let rr = opts.real_redirect_dir.unwrap_or(redirect);
            let missing = self.link_from(
                package_name,
                bin,
                (redirect, rr),
                true,
                opts.refresh,
                &mut out,
                &mut err,
            );
            if missing || err.is_some() {
                // Retry without the redirect (bun: "retrying without native bin link").
                out.clear();
                err = None;
                self.link_from(
                    package_name,
                    bin,
                    (package_dir, real),
                    false,
                    opts.refresh,
                    &mut out,
                    &mut err,
                );
            }
            return match err {
                Some(e) => Err(e),
                None => Ok(out),
            };
        }
        let mut out = Vec::new();
        let mut err = None;
        self.link_from(
            package_name,
            bin,
            (package_dir, real),
            false,
            opts.refresh,
            &mut out,
            &mut err,
        );
        match err {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// Returns true when a target was missing. `dirs` is `(dir for the link text, real dir)`.
    #[allow(clippy::too_many_arguments)]
    fn link_from(
        &mut self,
        package_name: &str,
        bin: &Bin,
        dirs: (&Path, &Path),
        redirect: bool,
        refresh: bool,
        out: &mut Vec<LinkedBin>,
        err: &mut Option<Error>,
    ) -> bool {
        let (text_dir, real_dir) = dirs;
        let mut missing = false;
        let mut one = |this: &mut Self, name: &str, target: &str, err: &mut Option<Error>| {
            if name.is_empty() || target.is_empty() || bin_target_escapes_package_dir(target) {
                return;
            }
            let target = target.trim_end_matches(['/', '\\']);
            let rel_target = if redirect {
                let real = resolve_redirect_target(real_dir, target, name);
                real.strip_prefix(real_dir)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| PathBuf::from(target))
            } else {
                PathBuf::from(target)
            };
            let check = needs_resolved_containment_check(target);
            match this.link_one(
                name,
                &text_dir.join(&rel_target),
                &real_dir.join(&rel_target),
                real_dir,
                check,
                refresh,
            ) {
                Ok(One::Linked) => out.push(LinkedBin {
                    name: name.to_owned(),
                    target: text_dir.join(&rel_target),
                }),
                Ok(One::Skipped) => {}
                Ok(One::Missing) => missing = true,
                Err(e) => {
                    err.get_or_insert(e);
                }
            }
        };
        match bin {
            Bin::None => {}
            Bin::File { path } => {
                let name = fnpm_types::name::unscoped_package_name(package_name);
                one(self, normalized_bin_name(name), path, err);
            }
            Bin::NamedFile { name, path } => one(self, normalized_bin_name(name), path, err),
            Bin::Map { entries } => {
                for (name, path) in entries {
                    one(self, normalized_bin_name(name), path, err);
                }
            }
            Bin::Dir { path } => {
                if path.is_empty() || bin_target_escapes_package_dir(path) {
                    return false;
                }
                let sub = path.trim_end_matches(['/', '\\']);
                let dir = real_dir.join(sub);
                let rd = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
                    Err(e) => {
                        err.get_or_insert(Error::Io {
                            path: dir,
                            source: e,
                        });
                        return false;
                    }
                };
                let mut names: Vec<String> = Vec::new();
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    if !(ft.is_file() || ft.is_symlink()) {
                        continue;
                    }
                    if let Some(n) = e.file_name().to_str() {
                        names.push(n.to_owned());
                    }
                }
                // readdir order is filesystem-dependent; sort for determinism.
                names.sort();
                for n in names {
                    let text = text_dir.join(sub).join(&n);
                    match self.link_one(&n, &text, &dir.join(&n), real_dir, true, refresh) {
                        Ok(One::Linked) => out.push(LinkedBin {
                            name: n,
                            target: text,
                        }),
                        Ok(One::Skipped) => {}
                        Ok(One::Missing) => missing = true,
                        Err(e) => {
                            err.get_or_insert(e);
                        }
                    }
                }
            }
        }
        missing
    }

    /// `text_target`: the path the link text is computed from; `real_target`: the file.
    fn link_one(
        &mut self,
        name: &str,
        text_target: &Path,
        real_target: &Path,
        package_dir: &Path,
        check: bool,
        refresh: bool,
    ) -> Result<One> {
        let Ok(target_meta) = std::fs::metadata(real_target) else {
            return Ok(One::Missing);
        };
        if check && resolved_parent_escapes(package_dir, real_target) {
            return Ok(One::Skipped);
        }
        if self.seen.contains(name) {
            // The name is taken, and `fnpm x <this package>` still runs the file itself.
            if refresh {
                make_executable(real_target, &target_meta);
            }
            return Ok(One::Skipped);
        }
        self.seen.insert(name.to_owned());
        let dest = self.bin_dir.join(name);
        match self.create(text_target, &dest) {
            Ok(written) => {
                if written || refresh {
                    make_executable(real_target, &target_meta);
                }
                Ok(One::Linked)
            }
            Err(e) => {
                self.seen.remove(name);
                Err(Error::Io {
                    path: dest,
                    source: e,
                })
            }
        }
    }

    /// Creates the link (or shims). `Ok(false)` when an identical link was already there.
    #[cfg(not(windows))]
    fn create(&mut self, abs_target: &Path, dest: &Path) -> io::Result<bool> {
        let rel = fsutil::relative(&self.bin_dir, abs_target);
        let mut result = std::os::unix::fs::symlink(&rel, dest);
        if self.owner.is_none()
            && !self.created_dir
            && matches!(&result, Err(e) if e.kind() == io::ErrorKind::NotFound)
        {
            fsutil::create_dir_all(&self.bin_dir)?;
            self.created_dir = true;
            result = std::os::unix::fs::symlink(&rel, dest);
        }
        let Err(err) = result else { return Ok(true) };
        let code = err.raw_os_error().unwrap_or(0);
        if !matches!(
            code,
            libc::EEXIST | libc::EPERM | libc::EACCES | libc::EOPNOTSUPP
        ) {
            return Err(err);
        }
        match std::fs::read_link(dest) {
            Ok(existing) if existing == rel => return Ok(false),
            Ok(existing) => {
                if let Some(owner) = &self.owner
                    && !link_leads_into(dest, &existing, owner)
                {
                    return Err(io::Error::from_raw_os_error(libc::EEXIST));
                }
                fsutil::remove_link(dest)?;
            }
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                if self.owner.is_some() {
                    return Err(io::Error::from_raw_os_error(libc::EEXIST));
                }
                // A directory in the `.bin` of a symlinked `node_modules` is not a linker's.
                if fsutil::kind(dest) == fsutil::Kind::Dir
                    && self
                        .bin_dir
                        .parent()
                        .is_some_and(fsutil::is_linked_node_modules)
                {
                    return Err(fsutil::in_the_way(dest));
                }
                // pnpm writes its commands as files; a directory goes too.
                fsutil::remove_any(dest)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && code == libc::EEXIST => {}
            Err(_) => return Err(err),
        }
        std::os::unix::fs::symlink(&rel, dest).map(|()| true)
    }

    /// Windows: `.cmd`, `.ps1` and sh shims (cmd-shim). Rewrites only when the content changed.
    #[cfg(windows)]
    fn create(&mut self, abs_target: &Path, dest: &Path) -> io::Result<bool> {
        fsutil::create_dir_all(&self.bin_dir)?;
        let rel = fsutil::relative(&self.bin_dir, abs_target);
        let first_line = read_first_line(abs_target);
        let shims = shim::generate(&rel.to_string_lossy(), first_line.as_deref());
        let mut written = false;
        for (ext, content) in [(".cmd", &shims.cmd), (".ps1", &shims.ps1), ("", &shims.sh)] {
            let mut p = dest.as_os_str().to_owned();
            p.push(ext);
            let p = PathBuf::from(p);
            if std::fs::read(&p).is_ok_and(|old| old == content.as_bytes()) {
                continue;
            }
            std::fs::write(&p, content)?;
            written = true;
        }
        Ok(written)
    }
}

/// Lexically: does the symlink at `dest` with text `link` point inside `dir`?
pub fn link_leads_into(dest: &Path, link: &Path, dir: &Path) -> bool {
    let base = dest.parent().unwrap_or(Path::new(""));
    let resolved = fsutil::normalize(&base.join(link));
    let dir = fsutil::normalize(dir);
    resolved != dir && resolved.starts_with(&dir)
}

/// bun `resolved_target_parent_escapes_package_dir`: the real parent of the target must be
/// inside the real package directory.
fn resolved_parent_escapes(package_dir: &Path, abs_target: &Path) -> bool {
    let Ok(real_pkg) = std::fs::canonicalize(package_dir) else {
        return true;
    };
    let Some(parent) = abs_target.parent() else {
        return true;
    };
    let Ok(real_parent) = std::fs::canonicalize(parent) else {
        return true;
    };
    !real_parent.starts_with(&real_pkg)
}

/// bun `resolve_bin_target` with a native redirect: try `<dir>/<target>`, `<dir>/<name>`,
/// `<dir>/<basename(target)>` (only when the target has a directory part) and
/// `<dir>/<name>.exe`; the first that exists wins, else `<dir>/<target>`.
fn resolve_redirect_target(dir: &Path, target: &str, name: &str) -> PathBuf {
    let base = target.rsplit(['/', '\\']).next().unwrap_or(target);
    let in_subdir = base.len() != target.len();
    let exe = if !name.is_empty() && !name.ends_with(".exe") {
        format!("{name}.exe")
    } else {
        String::new()
    };
    let at_root = if in_subdir { base } else { "" };
    for candidate in [target, name, at_root, exe.as_str()] {
        if candidate.is_empty() {
            continue;
        }
        let p = dir.join(candidate);
        if p.exists() {
            return p;
        }
    }
    dir.join(target)
}

/// `chmod` to [`exec_mode`] when the mode differs, then fix a CRLF shebang. A file with more
/// than one hard link is shared with the store (and other projects): it is left alone when it
/// is already executable, and otherwise replaced by a private copy before its mode changes, so
/// the blob in the store keeps its mode.
fn make_executable(abs_target: &Path, meta: &std::fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = exec_mode();
        let current = meta.mode() & 0o777;
        // bun uses `lchmod`: a target that is itself a symlink is never chmod-ed through, so a
        // package cannot make a file outside of it executable (or world-readable).
        let is_link =
            std::fs::symlink_metadata(abs_target).map_or(true, |m| m.file_type().is_symlink());
        if current != mode && !is_link {
            let shared = meta.nlink() > 1;
            if !(shared && current & 0o111 != 0) {
                if shared {
                    let _ = break_hardlink(abs_target);
                }
                let _ = std::fs::set_permissions(abs_target, std::fs::Permissions::from_mode(mode));
            }
        }
        normalize_shebang(abs_target, meta.len());
    }
    #[cfg(not(unix))]
    {
        let _ = (abs_target, meta);
    }
}

/// Replaces `p` by a copy of itself (tmp file + rename) so it no longer shares an inode.
#[cfg(unix)]
fn break_hardlink(p: &Path) -> io::Result<()> {
    let tmp = tmp_sibling(p);
    std::fs::copy(p, &tmp)?;
    match std::fs::rename(&tmp, p) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(unix)]
fn tmp_sibling(p: &Path) -> PathBuf {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    p.with_file_name(format!(
        "{}{name}-{:016x}",
        fsutil::STAGING_PREFIX,
        fsutil::random_suffix()
    ))
}

/// Rewrites `#!...\r\n` to `#!...\n` atomically (tmp + rename), keeping the mode.
#[cfg(unix)]
fn normalize_shebang(abs_target: &Path, size: u64) {
    let mut head = [0u8; 2048];
    let n = {
        let Ok(mut f) = std::fs::File::open(abs_target) else {
            return;
        };
        let mut n = 0usize;
        let want = (size as usize).min(head.len());
        while n < want {
            match f.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return,
            }
        }
        n
    };
    let chunk = &head[..n];
    if chunk.len() < 5 || chunk[0] != b'#' || chunk[1] != b'!' {
        return;
    }
    let Some(newline) = chunk.iter().position(|&c| c == b'\n') else {
        return;
    };
    if newline == 0 || chunk[newline - 1] != b'\r' {
        return;
    }
    let Ok(content) = std::fs::read(abs_target) else {
        return;
    };
    // The file may have changed since the first read (another link thread normalised the
    // same shared target): check the bytes that are actually rewritten.
    if !content.starts_with(b"#!")
        || content.get(newline) != Some(&b'\n')
        || content[newline - 1] != b'\r'
    {
        return;
    }
    let Ok(meta) = std::fs::metadata(abs_target) else {
        return;
    };
    let tmp = tmp_sibling(abs_target);
    let write = || -> io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&content[..newline - 1])?;
        f.write_all(b"\n")?;
        f.write_all(&content[newline + 1..])?;
        drop(f);
        std::fs::set_permissions(&tmp, meta.permissions())?;
        std::fs::rename(&tmp, abs_target)
    };
    if write().is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(any(windows, test))]
fn read_first_line(p: &Path) -> Option<String> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut buf = [0u8; 2048];
    let n = f.read(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let trimmed = text.trim_start();
    Some(trimmed.split(['\r', '\n']).next().unwrap_or("").to_owned())
}

/// Links `bin` of the package at `package_dir` (folder name `package_name`) into `bin_dir`,
/// replacing whatever has those names. For first-claimant-wins across packages use
/// [`BinLinker`]. `Bin::Dir` lists the directory (the only readdir the linker does).
pub fn link_bins(
    package_name: &str,
    bin: &Bin,
    package_dir: &Path,
    bin_dir: &Path,
) -> Result<Vec<LinkedBin>> {
    BinLinker::new(bin_dir).link(
        package_name,
        bin,
        package_dir,
        BinOptions {
            refresh: true,
            ..BinOptions::default()
        },
    )
}

/// Removes the links in `bin_dir` that point into `package_dir` (`fnpm remove`, relinks).
/// Returns how many were removed.
pub fn unlink_bins(package_dir: &Path, bin_dir: &Path) -> Result<u32> {
    let rd = match std::fs::read_dir(bin_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(Error::Io {
                path: bin_dir.to_path_buf(),
                source: e,
            });
        }
    };
    let pkg = fsutil::normalize(package_dir);
    let mut removed = 0;
    for e in rd.flatten() {
        let p = e.path();
        if fsutil::kind(&p) == Kind::Symlink {
            let Ok(link) = std::fs::read_link(&p) else {
                continue;
            };
            if link_leads_into(&p, &link, &pkg) {
                fsutil::remove_link(&p).map_err(|source| Error::Io {
                    path: p.clone(),
                    source,
                })?;
                removed += 1;
            }
        } else if cfg!(windows) && p.extension().is_some_and(|x| x == "cmd" || x == "ps1") {
            let rel = fsutil::relative(bin_dir, &pkg)
                .to_string_lossy()
                .replace('/', "\\");
            if std::fs::read_to_string(&p).is_ok_and(|s| s.contains(&rel)) {
                let _ = std::fs::remove_file(&p);
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Removes links in `bin_dir` whose target is gone, unless the package that should contain the
/// target is still installed (its build step may create the file). Returns how many were
/// removed (bun `prune_bins`).
pub fn prune_dangling_bins(bin_dir: &Path) -> u32 {
    let Ok(rd) = std::fs::read_dir(bin_dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in rd.flatten() {
        let p = e.path();
        if fsutil::kind(&p) != Kind::Symlink || p.exists() {
            continue;
        }
        let Ok(link) = std::fs::read_link(&p) else {
            continue;
        };
        let resolved = fsutil::normalize(&bin_dir.join(&link));
        if let Some(pkg) = package_dir_of(&resolved)
            && pkg.exists()
        {
            continue;
        }
        if fsutil::remove_link(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The package folder of a path: the one or two (scoped) components after the last
/// `node_modules` component.
fn package_dir_of(p: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = p.components().collect();
    let last_nm = comps
        .iter()
        .rposition(|c| c.as_os_str() == "node_modules")?;
    let first = comps.get(last_nm + 1)?;
    let mut out: PathBuf = comps[..=last_nm + 1]
        .iter()
        .map(|c| c.as_os_str())
        .collect();
    if first.as_os_str().to_string_lossy().starts_with('@') {
        out.push(comps.get(last_nm + 2)?.as_os_str());
    }
    Some(out)
}
/// cmd-shim compatible Windows shims (npm `cmd-shim` 6). Pure string generation, so it is
/// tested on every platform.
pub mod shim {
    /// The three files written per bin on Windows.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Shims {
        /// `<name>.cmd`
        pub cmd: String,
        /// `<name>.ps1`
        pub ps1: String,
        /// `<name>` (sh, for Git Bash / Cygwin / MSYS)
        pub sh: String,
    }

    /// The interpreter for a target without a shebang, by extension (cmd-shim
    /// `extensionToProgramMap`).
    fn program_for_extension(target: &str) -> Option<&'static str> {
        let ext = target
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())?;
        match ext.as_str() {
            "js" | "cjs" | "mjs" => Some("node"),
            "cmd" | "bat" => Some("cmd"),
            "ps1" => Some("pwsh"),
            "sh" => Some("sh"),
            _ => None,
        }
    }

    /// `(variables, program, args)` from a shebang line (cmd-shim `shebangExpr`):
    /// `#!\s*(?:/usr/bin/env\s+(?:-S\s+)?((?:[^ \t=]+=[^ \t=]+\s+)*))?([^ \t]+)(.*)`.
    pub fn parse_shebang(line: &str) -> Option<(String, String, String)> {
        const WS: [char; 2] = [' ', '\t'];
        let mut rest = line.strip_prefix("#!")?.trim_start_matches(WS);
        let mut vars = String::new();
        if let Some(after) = rest.strip_prefix("/usr/bin/env")
            && after.starts_with(WS)
        {
            let mut r = after.trim_start_matches(WS);
            if let Some(s) = r.strip_prefix("-S")
                && s.starts_with(WS)
            {
                r = s.trim_start_matches(WS);
            }
            // Leading NAME=value assignments, each followed by whitespace.
            loop {
                let tok_end = r.find(WS).unwrap_or(r.len());
                let tok = &r[..tok_end];
                let is_assign = tok
                    .split_once('=')
                    .is_some_and(|(k, v)| !k.is_empty() && !v.is_empty() && !v.contains('='));
                if !(is_assign && tok_end < r.len()) {
                    break;
                }
                vars.push_str(tok);
                vars.push(' ');
                r = r[tok_end..].trim_start_matches(WS);
            }
            rest = r;
        }
        let prog_end = rest.find(WS).unwrap_or(rest.len());
        let prog = &rest[..prog_end];
        if prog.is_empty() {
            return None;
        }
        Some((vars, prog.to_owned(), rest[prog_end..].to_owned()))
    }

    /// `KEY=value KEY2=v` to cmd `@SET` lines (cmd-shim `convertToSetCommands`).
    fn set_commands(vars: &str) -> String {
        let mut out = String::new();
        for v in vars.split_whitespace() {
            if let Some((k, val)) = v.split_once('=') {
                out.push_str(&format!("@SET {k}={val}\r\n"));
            }
        }
        out
    }

    /// Generates the shims for `rel_target` (relative to the bin directory, `/` or `\`
    /// separated) whose first line is `first_line`.
    pub fn generate(rel_target: &str, first_line: Option<&str>) -> Shims {
        let sh_target_path = rel_target.replace('\\', "/");
        let win_target = sh_target_path.replace('/', "\\");
        let parsed = first_line.and_then(parse_shebang);
        let (vars, prog, args) = match parsed {
            Some((v, p, a)) => (v, Some(p), a),
            None => (
                String::new(),
                program_for_extension(&sh_target_path).map(str::to_owned),
                String::new(),
            ),
        };
        let args = args.trim().to_owned();

        let lines = |ls: &[&str], eol: &str| -> String {
            let mut s = String::new();
            for l in ls {
                s.push_str(l);
                s.push_str(eol);
            }
            s
        };
        let sh_header = lines(
            &[
                "#!/bin/sh",
                "basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")",
                "",
                "case `uname` in",
                "    *CYGWIN*|*MINGW*|*MSYS*)",
                "        if command -v cygpath > /dev/null 2>&1; then",
                "            basedir=`cygpath -w \"$basedir\"`",
                "        fi",
                "    ;;",
                "esac",
                "",
            ],
            "\n",
        );
        let ps1_header = lines(
            &[
                "#!/usr/bin/env pwsh",
                "$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent",
                "",
                "$exe=\"\"",
                "if ($PSVersionTable.PSVersion -lt \"6.0\" -or $IsWindows) {",
                "  # Fix case when both the Windows and Linux builds of Node",
                "  # are installed in the same directory",
                "  $exe=\".exe\"",
                "}",
            ],
            "\n",
        );
        let cmd_head = lines(
            &[
                "@ECHO off",
                "GOTO start",
                ":find_dp0",
                "SET dp0=%~dp0",
                "EXIT /b",
                ":start",
                "SETLOCAL",
                "CALL :find_dp0",
            ],
            "\r\n",
        );
        let sh_target = format!("\"$basedir/{sh_target_path}\"");

        match prog {
            Some(prog) => {
                let long_prog = format!("\"%dp0%\\{prog}.exe\"");
                let sh_long_prog = format!("\"$basedir/{prog}\"");
                let pwsh_long_prog = format!("\"$basedir/{prog}$exe\"");
                let pwsh_prog = format!("\"{prog}$exe\"");
                let target = format!("\"%dp0%\\{win_target}\"");
                let mut cmd = cmd_head;
                cmd.push_str(&set_commands(&vars));
                cmd.push_str(&lines(
                    &[
                        "",
                        &format!("IF EXIST {long_prog} ("),
                        &format!("  SET \"_prog={}\"", long_prog.trim_matches('"')),
                        ") ELSE (",
                        &format!("  SET \"_prog={prog}\""),
                        "  SET PATHEXT=%PATHEXT:;.JS;=;%",
                        ")",
                        "",
                        &format!("endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\" {args} {target} %*"),
                    ],
                    "\r\n",
                ));
                let mut sh = sh_header;
                sh.push_str(&lines(
                    &[
                        &format!("if [ -x {sh_long_prog} ]; then"),
                        &format!("  exec {vars}{sh_long_prog} {args} {sh_target} \"$@\""),
                        "else ",
                        &format!("  exec {vars}{prog} {args} {sh_target} \"$@\""),
                        "fi",
                    ],
                    "\n",
                ));
                let mut ps1 = ps1_header;
                ps1.push_str(&lines(
                    &[
                        "$ret=0",
                        &format!("if (Test-Path {pwsh_long_prog}) {{"),
                        "  # Support pipeline input",
                        "  if ($MyInvocation.ExpectingInput) {",
                        &format!("    $input | & {pwsh_long_prog} {args} {sh_target} $args"),
                        "  } else {",
                        &format!("    & {pwsh_long_prog} {args} {sh_target} $args"),
                        "  }",
                        "  $ret=$LASTEXITCODE",
                        "} else {",
                        "  # Support pipeline input",
                        "  if ($MyInvocation.ExpectingInput) {",
                        &format!("    $input | & {pwsh_prog} {args} {sh_target} $args"),
                        "  } else {",
                        &format!("    & {pwsh_prog} {args} {sh_target} $args"),
                        "  }",
                        "  $ret=$LASTEXITCODE",
                        "}",
                        "exit $ret",
                    ],
                    "\n",
                ));
                Shims { cmd, ps1, sh }
            }
            None => {
                let mut cmd = cmd_head;
                cmd.push_str(&format!("\"%dp0%\\{win_target}\" {args} %*\r\n"));
                let mut sh = sh_header;
                sh.push_str(&format!("exec {sh_target} {args} \"$@\"\n"));
                let mut ps1 = ps1_header;
                ps1.push_str(&lines(
                    &[
                        "# Support pipeline input",
                        "if ($MyInvocation.ExpectingInput) {",
                        &format!("  $input | & {sh_target} {args} $args"),
                        "} else {",
                        &format!("  & {sh_target} {args} $args"),
                        "}",
                        "exit $LASTEXITCODE",
                    ],
                    "\n",
                ));
                Shims { cmd, ps1, sh }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(normalized_bin_name("babel"), "babel");
        assert_eq!(normalized_bin_name("@scope/tool"), "tool");
        assert_eq!(normalized_bin_name("a\\b"), "b");
        assert_eq!(normalized_bin_name("c:d"), "d");
        assert_eq!(normalized_bin_name(".."), "");
        assert_eq!(normalized_bin_name("."), "");
        assert_eq!(normalized_bin_name("a/"), "");
        assert_eq!(normalized_bin_name("x\u{7}"), "");
    }

    #[test]
    fn escapes() {
        for bad in [
            "/etc/passwd",
            "../x",
            "a/../../x",
            "C:foo",
            "C:..\\evil",
            "\\abs",
            "a\0b",
            "..",
        ] {
            assert!(bin_target_escapes_package_dir(bad), "{bad}");
        }
        for ok in ["cli.js", "./bin/x", "a/../b", "bin/x:y", "a/b/../c"] {
            assert!(!bin_target_escapes_package_dir(ok), "{ok}");
        }
        assert!(needs_resolved_containment_check("bin/x"));
        assert!(needs_resolved_containment_check("./x"));
        assert!(!needs_resolved_containment_check("x"));
    }

    #[test]
    fn link_order() {
        let mut v = [
            LinkOrder::new(false, ResolutionTag::Npm, "a"),
            LinkOrder::new(true, ResolutionTag::Workspace, "b"),
            LinkOrder::new(true, ResolutionTag::Npm, "z"),
            LinkOrder::new(true, ResolutionTag::Npm, "c"),
        ];
        v.sort();
        let names: Vec<_> = v.iter().map(|o| o.name).collect();
        assert_eq!(names, ["c", "z", "b", "a"]);
    }

    #[cfg(unix)]
    fn pkg(root: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = fsutil::join_name(&root.join("node_modules"), name);
        std::fs::create_dir_all(&dir).unwrap();
        for (p, c) in files {
            let f = dir.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
        }
        dir
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn all_shapes_link_relatively() {
        let t = tempfile::tempdir().unwrap();
        let bin_dir = t.path().join("node_modules/.bin");
        let a = pkg(t.path(), "@s/a", &[("cli.js", "#!/usr/bin/env node\n")]);
        let r = link_bins(
            "@s/a",
            &Bin::File {
                path: "cli.js".into(),
            },
            &a,
            &bin_dir,
        )
        .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(
            std::fs::read_link(bin_dir.join("a")).unwrap(),
            PathBuf::from("../@s/a/cli.js")
        );
        assert_eq!(mode(&a.join("cli.js")), exec_mode());

        let b = pkg(t.path(), "b", &[("x.js", ""), ("y.js", "")]);
        let map = Bin::Map {
            entries: vec![
                ("bx".into(), "x.js".into()),
                ("@o/by".into(), "./y.js".into()),
            ],
        };
        link_bins("b", &map, &b, &bin_dir).unwrap();
        assert_eq!(
            std::fs::read_link(bin_dir.join("bx")).unwrap(),
            PathBuf::from("../b/x.js")
        );
        assert_eq!(
            std::fs::read_link(bin_dir.join("by")).unwrap(),
            PathBuf::from("../b/y.js")
        );

        let c = pkg(
            t.path(),
            "c",
            &[("bin/one", ""), ("bin/two", ""), ("bin/sub/three", "")],
        );
        let r = link_bins("c", &Bin::Dir { path: "bin".into() }, &c, &bin_dir).unwrap();
        let names: Vec<_> = r.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["one", "two"]);
        assert_eq!(
            std::fs::read_link(bin_dir.join("one")).unwrap(),
            PathBuf::from("../c/bin/one")
        );
        assert!(!bin_dir.join("sub").exists());

        let d = pkg(t.path(), "d", &[("d.js", "")]);
        link_bins(
            "d",
            &Bin::NamedFile {
                name: "dee".into(),
                path: "d.js".into(),
            },
            &d,
            &bin_dir,
        )
        .unwrap();
        assert!(bin_dir.join("dee").exists());
    }

    #[cfg(unix)]
    #[test]
    fn first_claimant_wins_and_missing_targets_are_skipped() {
        let t = tempfile::tempdir().unwrap();
        let bin_dir = t.path().join("node_modules/.bin");
        let a = pkg(t.path(), "a", &[("a.js", "")]);
        let b = pkg(t.path(), "b", &[("b.js", "")]);
        let mut l = BinLinker::new(&bin_dir);
        let named = |p: &str| Bin::NamedFile {
            name: "tool".into(),
            path: p.into(),
        };
        assert_eq!(
            l.link("a", &named("a.js"), &a, BinOptions::default())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            l.link("b", &named("b.js"), &b, BinOptions::default())
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            std::fs::read_link(bin_dir.join("tool")).unwrap(),
            PathBuf::from("../a/a.js")
        );
        // A missing target leaves no dangling link and does not take the name.
        let mut l = BinLinker::new(&bin_dir);
        assert!(
            l.link(
                "b",
                &Bin::NamedFile {
                    name: "nope".into(),
                    path: "missing.js".into()
                },
                &b,
                BinOptions::default()
            )
            .unwrap()
            .is_empty()
        );
        assert!(!l.is_taken("nope"));
        assert!(std::fs::symlink_metadata(bin_dir.join("nope")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replaces_foreign_links_and_files_and_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let bin_dir = t.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::os::unix::fs::symlink("../elsewhere/x", bin_dir.join("tool")).unwrap();
        std::fs::write(bin_dir.join("other"), "pnpm shim").unwrap();
        let a = pkg(t.path(), "a", &[("a.js", ""), ("o.js", "")]);
        let map = Bin::Map {
            entries: vec![
                ("tool".into(), "a.js".into()),
                ("other".into(), "o.js".into()),
            ],
        };
        link_bins("a", &map, &a, &bin_dir).unwrap();
        assert_eq!(
            std::fs::read_link(bin_dir.join("tool")).unwrap(),
            PathBuf::from("../a/a.js")
        );
        assert_eq!(
            std::fs::read_link(bin_dir.join("other")).unwrap(),
            PathBuf::from("../a/o.js")
        );
        link_bins("a", &map, &a, &bin_dir).unwrap();
        assert_eq!(
            std::fs::read_link(bin_dir.join("tool")).unwrap(),
            PathBuf::from("../a/a.js")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_targets_are_skipped() {
        let t = tempfile::tempdir().unwrap();
        let bin_dir = t.path().join("node_modules/.bin");
        std::fs::write(t.path().join("secret"), "s").unwrap();
        let a = pkg(t.path(), "a", &[("ok.js", "")]);
        std::os::unix::fs::symlink(t.path(), a.join("up")).unwrap();
        let map = Bin::Map {
            entries: vec![
                ("x".into(), "../../secret".into()),
                ("y".into(), "/etc/passwd".into()),
                ("z".into(), "up/secret".into()),
                ("ok".into(), "ok.js".into()),
            ],
        };
        let r = link_bins("a", &map, &a, &bin_dir).unwrap();
        let names: Vec<_> = r.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["ok"]);
        assert_eq!(mode(&t.path().join("secret")) & 0o111, 0);
    }

    #[cfg(unix)]
    #[test]
    fn crlf_shebang_is_normalized() {
        let t = tempfile::tempdir().unwrap();
        let bin_dir = t.path().join("node_modules/.bin");
        let a = pkg(
            t.path(),
            "a",
            &[("cli.js", "#!/usr/bin/env node\r\nconsole.log(1)\r\n")],
        );
        link_bins(
            "a",
            &Bin::File {
                path: "cli.js".into(),
            },
            &a,
            &bin_dir,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(a.join("cli.js")).unwrap(),
            "#!/usr/bin/env node\nconsole.log(1)\r\n"
        );
        assert_eq!(mode(&a.join("cli.js")), exec_mode());
        // Without a shebang nothing changes.
        let b = pkg(t.path(), "b", &[("cli.js", "x\r\ny")]);
        link_bins(
            "b",
            &Bin::File {
                path: "cli.js".into(),
            },
            &b,
            &bin_dir,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(b.join("cli.js")).unwrap(), "x\r\ny");
        assert_eq!(
            read_first_line(&a.join("cli.js")).as_deref(),
            Some("#!/usr/bin/env node")
        );
    }

    #[cfg(unix)]
    #[test]
    fn shared_executable_files_keep_their_inode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let t = tempfile::tempdir().unwrap();
        let blob = t.path().join("blob");
        std::fs::write(&blob, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o755)).unwrap();
        let a = pkg(t.path(), "a", &[]);
        std::fs::hard_link(&blob, a.join("cli")).unwrap();
        link_bins(
            "a",
            &Bin::File { path: "cli".into() },
            &a,
            &t.path().join("node_modules/.bin"),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&blob).unwrap().ino(),
            std::fs::metadata(a.join("cli")).unwrap().ino()
        );
        // A shared non-executable file gets a private copy.
        let blob2 = t.path().join("blob2");
        std::fs::write(&blob2, "x").unwrap();
        std::fs::set_permissions(&blob2, std::fs::Permissions::from_mode(0o644)).unwrap();
        let b = pkg(t.path(), "b", &[]);
        std::fs::hard_link(&blob2, b.join("cli")).unwrap();
        link_bins(
            "b",
            &Bin::File { path: "cli".into() },
            &b,
            &t.path().join("node_modules/.bin"),
        )
        .unwrap();
        assert_ne!(
            std::fs::metadata(&blob2).unwrap().ino(),
            std::fs::metadata(b.join("cli")).unwrap().ino()
        );
        assert_eq!(mode(&blob2), 0o644);
        assert_eq!(mode(&b.join("cli")), exec_mode());
    }

    #[cfg(unix)]
    #[test]
    fn native_redirect_candidates() {
        let t = tempfile::tempdir().unwrap();
        let bin_dir = t.path().join("node_modules/.bin");
        let esbuild = pkg(t.path(), "esbuild", &[("bin/esbuild", "js shim")]);
        let native = pkg(t.path(), "@esbuild/linux-x64", &[("bin/esbuild", "ELF")]);
        let bin = Bin::NamedFile {
            name: "esbuild".into(),
            path: "bin/esbuild".into(),
        };
        let mut l = BinLinker::new(&bin_dir);
        l.link(
            "esbuild",
            &bin,
            &esbuild,
            BinOptions {
                redirect_dir: Some(&native),
                refresh: true,
                ..BinOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(bin_dir.join("esbuild")).unwrap(),
            PathBuf::from("../@esbuild/linux-x64/bin/esbuild")
        );
        // Binary at the package root under the bin name.
        let cc = pkg(t.path(), "cc", &[("bin/claude.exe", "")]);
        let ccn = pkg(t.path(), "cc-linux", &[("claude", "")]);
        let bin = Bin::NamedFile {
            name: "claude".into(),
            path: "bin/claude.exe".into(),
        };
        let mut l = BinLinker::new(&bin_dir);
        l.link(
            "cc",
            &bin,
            &cc,
            BinOptions {
                redirect_dir: Some(&ccn),
                refresh: true,
                ..BinOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(bin_dir.join("claude")).unwrap(),
            PathBuf::from("../cc-linux/claude")
        );
        // Nothing in the platform package: falls back to the package itself.
        let empty = pkg(t.path(), "empty-native", &[]);
        let mut l = BinLinker::new(&bin_dir);
        let bin = Bin::NamedFile {
            name: "fallback".into(),
            path: "bin/esbuild".into(),
        };
        l.link(
            "esbuild",
            &bin,
            &esbuild,
            BinOptions {
                redirect_dir: Some(&empty),
                refresh: true,
                ..BinOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(bin_dir.join("fallback")).unwrap(),
            PathBuf::from("../esbuild/bin/esbuild")
        );
        assert_eq!(
            resolve_redirect_target(&ccn, "x/claude", "zz"),
            ccn.join("claude")
        );
    }

    #[cfg(unix)]
    #[test]
    fn global_bins_respect_foreign_files() {
        let t = tempfile::tempdir().unwrap();
        let gbin = t.path().join("bin");
        std::fs::create_dir_all(&gbin).unwrap();
        std::fs::write(gbin.join("fnpm"), "real program").unwrap();
        std::os::unix::fs::symlink("/somewhere/else", gbin.join("tool")).unwrap();
        let nm = t.path().join("node_modules");
        let a = pkg(t.path(), "a", &[("a.js", ""), ("b.js", "")]);
        let mut l = BinLinker::global(&gbin, &nm);
        let map = Bin::Map {
            entries: vec![
                ("fnpm".into(), "a.js".into()),
                ("tool".into(), "b.js".into()),
            ],
        };
        assert!(l.link("a", &map, &a, BinOptions::default()).is_err());
        assert_eq!(
            std::fs::read_to_string(gbin.join("fnpm")).unwrap(),
            "real program"
        );
        assert_eq!(
            std::fs::read_link(gbin.join("tool")).unwrap(),
            PathBuf::from("/somewhere/else")
        );
        let mut l = BinLinker::global(&gbin, &nm);
        l.seed_existing();
        assert!(l.is_taken("fnpm"));
        // A link into the global node_modules is replaced.
        std::fs::remove_file(gbin.join("tool")).unwrap();
        std::os::unix::fs::symlink("../node_modules/old/x", gbin.join("tool")).unwrap();
        let mut l = BinLinker::global(&gbin, &nm);
        l.link(
            "a",
            &Bin::NamedFile {
                name: "tool".into(),
                path: "b.js".into(),
            },
            &a,
            BinOptions::default(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(gbin.join("tool")).unwrap(),
            PathBuf::from("../node_modules/a/b.js")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unlink_and_prune() {
        let t = tempfile::tempdir().unwrap();
        let bin_dir = t.path().join("node_modules/.bin");
        let a = pkg(t.path(), "a", &[("a.js", "")]);
        let b = pkg(t.path(), "b", &[("b.js", "")]);
        link_bins(
            "a",
            &Bin::File {
                path: "a.js".into(),
            },
            &a,
            &bin_dir,
        )
        .unwrap();
        link_bins(
            "b",
            &Bin::File {
                path: "b.js".into(),
            },
            &b,
            &bin_dir,
        )
        .unwrap();
        assert_eq!(unlink_bins(&a, &bin_dir).unwrap(), 1);
        assert!(std::fs::symlink_metadata(bin_dir.join("a")).is_err());
        assert!(bin_dir.join("b").exists());
        // Dangling, package still there: kept. Package gone: removed.
        std::fs::remove_file(b.join("b.js")).unwrap();
        assert_eq!(prune_dangling_bins(&bin_dir), 0);
        std::fs::remove_dir_all(&b).unwrap();
        assert_eq!(prune_dangling_bins(&bin_dir), 1);
        assert_eq!(unlink_bins(&a, &t.path().join("nope")).unwrap(), 0);
        assert_eq!(
            package_dir_of(Path::new("/p/node_modules/@s/n/bin/x")),
            Some(PathBuf::from("/p/node_modules/@s/n"))
        );
        assert_eq!(
            package_dir_of(Path::new("/p/node_modules/.fnpm/k/node_modules/n/x")),
            Some(PathBuf::from("/p/node_modules/.fnpm/k/node_modules/n"))
        );
    }

    #[test]
    fn shebang_parsing() {
        assert_eq!(
            parse("#!/usr/bin/env node"),
            Some(("".into(), "node".into(), "".into()))
        );
        assert_eq!(
            parse("#!/usr/bin/env -S node --flag"),
            Some(("".into(), "node".into(), " --flag".into()))
        );
        assert_eq!(
            parse("#!/usr/bin/env A=1 B=2 node x"),
            Some(("A=1 B=2 ".into(), "node".into(), " x".into()))
        );
        assert_eq!(
            parse("#! /bin/sh -e"),
            Some(("".into(), "/bin/sh".into(), " -e".into()))
        );
        assert_eq!(parse("no shebang"), None);
        assert_eq!(parse("#!"), None);
    }

    fn parse(s: &str) -> Option<(String, String, String)> {
        shim::parse_shebang(s)
    }

    #[test]
    fn shims_with_interpreter() {
        let s = shim::generate("../pkg/cli.js", Some("#!/usr/bin/env node"));
        assert!(s.cmd.starts_with("@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n"));
        assert!(s.cmd.contains("IF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n"));
        assert!(
            s.cmd
                .ends_with("\"%_prog%\"  \"%dp0%\\..\\pkg\\cli.js\" %*\r\n")
        );
        assert!(!s.cmd.contains("../pkg"));
        assert!(s.sh.starts_with("#!/bin/sh\n"));
        assert!(s.sh.contains("if [ -x \"$basedir/node\" ]; then\n  exec \"$basedir/node\"  \"$basedir/../pkg/cli.js\" \"$@\"\nelse \n  exec node  \"$basedir/../pkg/cli.js\" \"$@\"\nfi\n"));
        assert!(s.ps1.starts_with(
            "#!/usr/bin/env pwsh\n$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent\n"
        ));
        assert!(s.ps1.contains("if (Test-Path \"$basedir/node$exe\") {"));
        assert!(
            s.ps1
                .contains("& \"node$exe\"  \"$basedir/../pkg/cli.js\" $args")
        );
        assert!(s.ps1.ends_with("exit $ret\n"));
    }

    #[test]
    fn shims_with_env_vars_and_args() {
        let s = shim::generate(
            "../x/run",
            Some("#!/usr/bin/env -S NODE_ENV=production node --harmony"),
        );
        assert!(
            s.cmd
                .contains("CALL :find_dp0\r\n@SET NODE_ENV=production\r\n\r\nIF EXIST")
        );
        assert!(
            s.cmd
                .contains("\"%_prog%\" --harmony \"%dp0%\\..\\x\\run\" %*")
        );
        assert!(s.sh.contains("exec NODE_ENV=production \"$basedir/node\" --harmony"));
        // A value with `=` is not an assignment for cmd-shim's regex: it becomes the program.
        assert_eq!(
            parse("#!/usr/bin/env A=b=c node"),
            Some(("".into(), "A=b=c".into(), " node".into()))
        );
    }

    #[test]
    fn shims_by_extension_and_without_program() {
        let s = shim::generate("..\\p\\cli.mjs", None);
        assert!(s.cmd.contains("SET \"_prog=node\""));
        assert!(s.sh.contains("\"$basedir/../p/cli.mjs\""));
        let s = shim::generate("../p/tool.exe", None);
        assert!(
            s.cmd
                .ends_with(":find_dp0\r\n\"%dp0%\\..\\p\\tool.exe\"  %*\r\n")
        );
        assert!(s.sh.ends_with("exec \"$basedir/../p/tool.exe\"  \"$@\"\n"));
        assert!(s.ps1.ends_with("exit $LASTEXITCODE\n"));
        let s = shim::generate("../p/run.sh", None);
        assert!(s.sh.contains("\"$basedir/sh\""));
    }
}
