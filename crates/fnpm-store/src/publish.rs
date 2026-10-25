//! Moving a finished staging directory into `pkgs/` (bun `install/cache_publish.rs`).
//!
//! The first writer wins: `rename` only replaces an empty directory, so a published package is
//! never replaced. If the rename fails and a complete package is already there, ours is
//! discarded and theirs used. An incomplete one (no valid `index.bin`, e.g. left by a crash)
//! is removed and the rename retried, at most four times in all.

use std::io;
use std::path::Path;
#[cfg(windows)]
use std::time::{Duration, Instant};

/// What happened to a staged directory.
pub(crate) enum Published<T> {
    /// Ours is now at the destination.
    Ours,
    /// Another writer published first; this is what `is_complete` returned for it.
    Theirs(T),
}

const MAX_ATTEMPTS: u32 = 4;

/// Renames `staged` to `dest`. `is_complete` inspects whatever is at `dest` after a failed
/// rename.
pub(crate) fn publish<T>(
    staged: &Path,
    dest: &Path,
    windows_retry_ms: u64,
    is_complete: impl Fn(&Path) -> Option<T>,
) -> io::Result<Published<T>> {
    let mut attempts = 0;
    let mut emptying_err: Option<io::Error> = None;
    loop {
        let err = match rename_with_retry(staged, dest, windows_retry_ms) {
            Ok(()) => return Ok(Published::Ours),
            Err(e) => e,
        };
        attempts += 1;
        // Pin the directory before inspecting it: another recovery may replace the path
        // after the check. Cleanup must only empty the directory we actually inspected.
        let incomplete = open_incomplete(dest);
        if incomplete.is_ok()
            && let Some(theirs) = is_complete(dest)
        {
            return Ok(Published::Theirs(theirs));
        }
        if attempts == MAX_ATTEMPTS {
            return Err(emptying_err.unwrap_or(err));
        }
        match incomplete {
            Ok(dir) => {
                emptying_err = empty_incomplete(&dir, dest).err();
                drop(dir);
                // A replacement published meanwhile is nonempty and cannot be removed.
                let _ = std::fs::remove_dir(dest);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Nothing there: the parent may be missing.
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            Err(e) => {
                // unlink never removes a directory, including a concurrently published one.
                if std::fs::remove_file(dest).is_err() {
                    emptying_err = Some(e);
                }
            }
        }
    }
}

// Unix cleanup stays relative to the directory opened before is_complete, like bun's
// cache_publish::StagedEntry. A pathname-based remove_dir_all can erase a winning import.
#[cfg(unix)]
fn open_incomplete(path: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)
}

#[cfg(unix)]
fn empty_incomplete(dir: &std::os::fd::OwnedFd, _path: &Path) -> io::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let mut entries = rustix::fs::Dir::read_from(dir)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        match rustix::fs::openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(child) => {
                empty_incomplete(&child, Path::new(""))?;
                match rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Err(_) => match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(e) => return Err(e.into()),
            },
        }
    }
    Ok(())
}

// On Windows, denying FILE_SHARE_DELETE pins the pathname while it is inspected and emptied.
#[cfg(windows)]
fn open_incomplete(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .custom_flags(0x02000000 | 0x00200000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
        .open(path)?;
    if !file.metadata()?.is_dir() || file.metadata()?.file_type().is_symlink() {
        return Err(io::Error::from(io::ErrorKind::NotADirectory));
    }
    Ok(file)
}

#[cfg(windows)]
fn empty_incomplete(_dir: &std::fs::File, path: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        crate::sys::remove_tree(&entry?.path())?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn rename_with_retry(from: &Path, to: &Path, _windows_retry_ms: u64) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Windows cannot rename a directory while a virus scanner has one of its new files open; all
/// waits share one budget (`FNPM_INSTALL_WINDOWS_RENAME_RETRY_MS`, default 5000 ms), 10 ms more
/// each time up to 100 ms, as graceful-fs does.
#[cfg(windows)]
fn rename_with_retry(from: &Path, to: &Path, budget_ms: u64) -> io::Result<()> {
    let started = Instant::now();
    let mut backoff = 0u64;
    loop {
        let err = match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if err.kind() != io::ErrorKind::PermissionDenied || to.exists() {
            return Err(err);
        }
        let waited = started.elapsed().as_millis() as u64;
        if waited >= budget_ms {
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "{err} (gave up after retrying for {waited}ms; usually another process such as antivirus has a file in the directory open. Set FNPM_INSTALL_WINDOWS_RENAME_RETRY_MS to wait longer)"
                ),
            ));
        }
        backoff = (backoff + 10).min(100);
        std::thread::sleep(Duration::from_millis(backoff));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(dest: &Path) -> Option<String> {
        std::fs::read_to_string(dest.join("done")).ok()
    }

    #[test]
    fn first_writer_wins() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("pkgs/a");
        let ours = dir.path().join("s1");
        std::fs::create_dir(&ours).unwrap();
        std::fs::write(ours.join("done"), "first").unwrap();
        assert!(matches!(
            publish(&ours, &dest, 0, complete).unwrap(),
            Published::Ours
        ));
        let second = dir.path().join("s2");
        std::fs::create_dir(&second).unwrap();
        std::fs::write(second.join("done"), "second").unwrap();
        match publish(&second, &dest, 0, complete).unwrap() {
            Published::Theirs(t) => assert_eq!(t, "first"),
            Published::Ours => panic!("replaced a published package"),
        }
        assert_eq!(std::fs::read_to_string(dest.join("done")).unwrap(), "first");
    }

    #[test]
    fn incomplete_entry_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("partial"), "x").unwrap();
        let ours = dir.path().join("s");
        std::fs::create_dir(&ours).unwrap();
        std::fs::write(ours.join("done"), "new").unwrap();
        assert!(matches!(
            publish(&ours, &dest, 0, complete).unwrap(),
            Published::Ours
        ));
        assert_eq!(complete(&dest).as_deref(), Some("new"));
        assert!(!dest.join("partial").exists());
        // A file in the way is removed too.
        let dest2 = dir.path().join("b");
        std::fs::write(&dest2, "file").unwrap();
        let ours = dir.path().join("s3");
        std::fs::create_dir(&ours).unwrap();
        std::fs::write(ours.join("done"), "x").unwrap();
        assert!(matches!(
            publish(&ours, &dest2, 0, complete).unwrap(),
            Published::Ours
        ));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_never_deletes_a_concurrently_published_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("partial"), "incomplete").unwrap();
        let ours = dir.path().join("ours");
        std::fs::create_dir(&ours).unwrap();
        std::fs::write(ours.join("done"), "ours").unwrap();
        let moved = dir.path().join("old-incomplete");
        let once = std::sync::atomic::AtomicBool::new(false);
        let result = publish(&ours, &dest, 0, |d| {
            let found = complete(d);
            if !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                // Model a second recovery/import between our completeness check and cleanup.
                std::fs::rename(d, &moved).unwrap();
                std::fs::create_dir(d).unwrap();
                std::fs::write(d.join("done"), "theirs").unwrap();
            }
            found
        })
        .unwrap();
        assert!(matches!(result, Published::Theirs(ref value) if value == "theirs"));
        assert_eq!(complete(&dest).as_deref(), Some("theirs"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_unlinks_a_symlink_without_using_or_emptying_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("done"), "outside").unwrap();
        let dest = dir.path().join("a");
        std::os::unix::fs::symlink(&outside, &dest).unwrap();
        let ours = dir.path().join("ours");
        std::fs::create_dir(&ours).unwrap();
        std::fs::write(ours.join("done"), "ours").unwrap();
        assert!(matches!(
            publish(&ours, &dest, 0, complete).unwrap(),
            Published::Ours
        ));
        assert_eq!(complete(&dest).as_deref(), Some("ours"));
        assert_eq!(complete(&outside).as_deref(), Some("outside"));
        assert!(!dest.symlink_metadata().unwrap().file_type().is_symlink());
    }
}
