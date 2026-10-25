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
        if let Some(theirs) = is_complete(dest) {
            return Ok(Published::Theirs(theirs));
        }
        if attempts == MAX_ATTEMPTS {
            return Err(emptying_err.unwrap_or(err));
        }
        match std::fs::symlink_metadata(dest) {
            Ok(m) if m.is_dir() => {
                emptying_err = std::fs::remove_dir_all(dest).err();
            }
            Ok(_) => {
                let _ = std::fs::remove_file(dest);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Nothing there: the parent may be missing.
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            Err(e) => emptying_err = Some(e),
        }
    }
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
}
