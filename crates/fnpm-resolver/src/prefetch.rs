//! Look-ahead prefetch: names to request before the resolver reaches them.
//!
//! Two sources: the previous resolve's manifest order saved in `<cache_dir>/prefetch/<hash of
//! project dir>.txt`, and speculative transitive names from cached manifests (bun
//! `PackageManagerEnqueue.rs:1246,1503`).

use std::path::Path;

pub fn load_hints(_cache_dir: &Path, _project_dir: &Path) -> Vec<String> {
    todo!()
}

pub fn save_hints(_cache_dir: &Path, _project_dir: &Path, _names_in_request_order: &[String]) {
    todo!()
}
