//! `.bin` links (bun `bin.rs`): POSIX relative symlinks with `chmod +x` and CRLF shebang
//! fix-up; Windows `.cmd` / `.ps1` / sh shims. First package to claim a name wins, in link
//! order (direct dependencies before transitive).

use std::path::Path;

use fnpm_types::Bin;

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedBin {
    pub name: String,
    pub target: std::path::PathBuf,
}

/// Links `bin` of the package installed at `package_dir` into `bin_dir`.
/// `Bin::Dir` lists the directory (the only readdir the linker does).
pub fn link_bins(_package_name: &str, _bin: &Bin, _package_dir: &Path, _bin_dir: &Path) -> Result<Vec<LinkedBin>> {
    todo!()
}

/// Removes bins in `bin_dir` that point into `package_dir` (`fnpm remove`, relinks).
pub fn unlink_bins(_package_dir: &Path, _bin_dir: &Path) -> Result<u32> {
    todo!()
}
