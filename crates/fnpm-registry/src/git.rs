//! `git` dependencies, through the `git` executable (bun `git_runner.rs`, `repository.rs`).

use std::path::Path;

use fnpm_types::spec::Committish;

use crate::Result;

/// `git ls-remote` the URL and pick the commit: a ref name, a full/abbreviated SHA, the highest
/// tag satisfying `#semver:`, or `HEAD`.
pub async fn resolve_commit(_url: &str, _committish: &Committish) -> Result<String> {
    todo!()
}

/// Clones (or fetches into the bare cache at `<cache_dir>/git/<hash>`) and checks out `sha`
/// into `dest`, without `.git`. `path:` committish parts select a subdirectory.
pub async fn checkout(_cache_dir: &Path, _url: &str, _sha: &str, _subdir: Option<&str>, _dest: &Path) -> Result<()> {
    todo!()
}
