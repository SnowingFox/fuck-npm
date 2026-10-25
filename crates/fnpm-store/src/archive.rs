//! Extracting archives that are not npm packages (templates, GitHub tarballs) to a plain
//! directory, with bun's symlink protections (`libarchive/lib.rs` `extract_to_dir`):
//!
//! - the first path component is stripped, `..` is resolved and anything that would climb out
//!   is skipped (the generic [`TarParser`] mode);
//! - a symlink whose target is absolute or climbs above the extraction root is skipped
//!   ([`is_symlink_target_safe`]);
//! - symlinks are created only after every file and directory is written, so no entry can be
//!   written through one; and each link is created only if its parent directory is still the
//!   directory that was there before (same device and inode), so a link created earlier cannot
//!   redirect a later one.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::gzip::{self, GzipStream};
use crate::tar::{EntryKind, TarEvent, TarOptions, TarParser, is_symlink_target_safe};
use crate::{Error, Result};

/// Options of [`extract_to_dir`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractOptions {
    /// Do not create symlinks at all (bun does this for GitHub tarballs, as npm does).
    pub skip_symlinks: bool,
}

/// What [`extract_to_dir`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractReport {
    /// Files written.
    pub files: u32,
    /// Directories created.
    pub dirs: u32,
    /// Symlinks created.
    pub symlinks: u32,
    /// Why entries were skipped (unsafe symlinks, parents that changed, over-long paths).
    pub warnings: Vec<String>,
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

struct Sink<'a> {
    dest: &'a Path,
    opts: ExtractOptions,
    current: Option<(PathBuf, std::fs::File)>,
    links: Vec<(String, String)>,
    report: ExtractReport,
}

impl Sink<'_> {
    fn on_event(&mut self, event: TarEvent<'_>) -> Result<()> {
        match event {
            TarEvent::Entry(h) => {
                let path = self.dest.join(&h.path);
                match h.kind {
                    EntryKind::Directory => {
                        std::fs::create_dir_all(&path).map_err(io_at(&path))?;
                        self.report.dirs += 1;
                    }
                    EntryKind::File => {
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent).map_err(io_at(parent))?;
                        }
                        // A symlink planted at the path is replaced, not followed.
                        if std::fs::symlink_metadata(&path)
                            .is_ok_and(|m| m.file_type().is_symlink())
                        {
                            std::fs::remove_file(&path).map_err(io_at(&path))?;
                        }
                        let mut o = std::fs::OpenOptions::new();
                        o.write(true).create(true).truncate(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            o.mode(h.mode).custom_flags(libc::O_NOFOLLOW);
                        }
                        let file = o.open(&path).map_err(io_at(&path))?;
                        self.current = Some((path, file));
                        self.report.files += 1;
                    }
                }
            }
            TarEvent::Data(d) => {
                if let Some((path, file)) = self.current.as_mut() {
                    file.write_all(d).map_err(io_at(path))?;
                }
            }
            TarEvent::EntryEnd => self.current = None,
            TarEvent::Symlink { path, target } => {
                if self.opts.skip_symlinks {
                    return Ok(());
                }
                if is_symlink_target_safe(&path, &target) {
                    self.links.push((path, target));
                } else {
                    self.report.warnings.push(format!(
                        "Skipping symlink with unsafe target: {} -> {}",
                        crate::tar::escape_control(&path),
                        crate::tar::escape_control(&target)
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn dir_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path).ok()?;
    m.is_dir().then(|| (m.dev(), m.ino()))
}

#[cfg(unix)]
fn create_deferred_symlinks(dest: &Path, links: &[(String, String)], report: &mut ExtractReport) {
    let parents: Vec<Option<(u64, u64)>> = links
        .iter()
        .map(|(path, _)| {
            let parent = Path::new(path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())?;
            let _ = std::fs::create_dir_all(dest.join(parent));
            dir_identity(&dest.join(parent))
        })
        .collect();
    for ((path, target), expected) in links.iter().zip(parents) {
        let has_parent = Path::new(path)
            .parent()
            .is_some_and(|p| !p.as_os_str().is_empty());
        if has_parent {
            let parent = dest.join(Path::new(path).parent().unwrap_or(Path::new("")));
            if expected.is_none() || dir_identity(&parent) != expected {
                report.warnings.push(format!(
                    "Skipping symlink whose parent directory changed during extraction: {}",
                    crate::tar::escape_control(path)
                ));
                continue;
            }
        }
        match std::os::unix::fs::symlink(target, dest.join(path)) {
            Ok(()) => report.symlinks += 1,
            Err(_) => report.warnings.push(format!(
                "Skipping symlink that could not be created: {} -> {}",
                crate::tar::escape_control(path),
                crate::tar::escape_control(target)
            )),
        }
    }
}

#[cfg(not(unix))]
fn create_deferred_symlinks(_dest: &Path, links: &[(String, String)], report: &mut ExtractReport) {
    for (path, _) in links {
        report.warnings.push(format!(
            "Skipping symlink on this platform: {}",
            crate::tar::escape_control(path)
        ));
    }
}

/// Extracts a (gzipped) tar held in memory below `dest` (created if missing).
pub fn extract_to_dir(archive: &[u8], dest: &Path, opts: ExtractOptions) -> Result<ExtractReport> {
    std::fs::create_dir_all(dest).map_err(io_at(dest))?;
    let mut parser = TarParser::with_options(TarOptions::generic());
    let mut sink = Sink {
        dest,
        opts,
        current: None,
        links: Vec::new(),
        report: ExtractReport::default(),
    };
    if gzip::is_gzip(archive) {
        let mut g = GzipStream::new();
        g.feed(archive, &mut |out| {
            parser.feed_with(out, |ev| sink.on_event(ev))
        })?;
        g.finish(&mut |out| parser.feed_with(out, |ev| sink.on_event(ev)))?;
    } else {
        parser.feed_with(archive, |ev| sink.on_event(ev))?;
    }
    sink.report.warnings.extend(parser.take_warnings());
    parser.finish()?;
    let Sink {
        links, mut report, ..
    } = sink;
    create_deferred_symlinks(dest, &links, &mut report);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[cfg(unix)]
    #[test]
    fn symlinks_are_checked_and_created_last() {
        let tgz = TarBuilder::new()
            .entry(header("root/", 0, b'5'), b"")
            .file("root/lib/real.js", b"real")
            .entry(
                header_with(b"root/lib/ok.js", 0, b'2', |h| {
                    h[157..164].copy_from_slice(b"real.js")
                }),
                b"",
            )
            .entry(
                header_with(b"root/up.js", 0, b'2', |h| {
                    h[157..171].copy_from_slice(b"lib/../real.js")
                }),
                b"",
            )
            .entry(
                header_with(b"root/escape", 0, b'2', |h| {
                    h[157..161].copy_from_slice(b"/tmp")
                }),
                b"",
            )
            .entry(
                header_with(b"root/climb", 0, b'2', |h| {
                    h[157..166].copy_from_slice(b"../../etc")
                }),
                b"",
            )
            // A link at `dir`, then a file "through" it: the file lands in a real directory.
            .entry(
                header_with(b"root/dir", 0, b'2', |h| {
                    h[157..160].copy_from_slice(b"lib")
                }),
                b"",
            )
            .file("root/dir/through.js", b"t")
            .end()
            .tgz();
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("out");
        let r = extract_to_dir(&tgz, &dest, ExtractOptions::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("lib/ok.js")).unwrap(),
            "real"
        );
        assert!(
            std::fs::symlink_metadata(dest.join("lib/ok.js"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!dest.join("escape").exists());
        assert!(!dest.join("climb").exists());
        assert!(
            std::fs::symlink_metadata(dest.join("dir"))
                .unwrap()
                .is_dir()
        );
        assert!(!dest.join("lib/through.js").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("dir/through.js")).unwrap(),
            "t"
        );
        assert_eq!(r.files, 2);
        assert!(
            r.warnings
                .iter()
                .filter(|w| w.contains("unsafe target"))
                .count()
                == 3,
            "{:?}",
            r.warnings
        );
        // `dir` is a real directory now, so its link could not be created.
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("could not be created: dir")),
            "{:?}",
            r.warnings
        );
        let r = extract_to_dir(
            &tgz,
            &t.path().join("nolinks"),
            ExtractOptions {
                skip_symlinks: true,
            },
        )
        .unwrap();
        assert_eq!(r.symlinks, 0);
    }
}
