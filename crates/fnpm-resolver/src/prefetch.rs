//! Look-ahead hints: the manifest request order of the previous resolve of a project, saved in
//! `<cache_dir>/prefetch/<16 hex>.txt` (xxh3-64 of the project directory). A cold resolve of the
//! same project requests these manifests up front, before the queue reaches them. Speculative
//! transitive requests from arriving manifests live in the resolver (bun
//! `PackageManagerEnqueue.rs:1246,1503`).

use std::io::Write;
use std::path::{Path, PathBuf};

/// At most this many hints are kept.
const MAX_HINTS: usize = 20_000;

fn hints_path(cache_dir: &Path, project_dir: &Path) -> PathBuf {
    let hash = xxhash_rust::xxh3::xxh3_64(project_dir.to_string_lossy().as_bytes());
    cache_dir.join("prefetch").join(format!("{hash:016x}.txt"))
}

/// The saved request order, or nothing (missing or unreadable files are ignored; invalid names
/// are skipped).
pub fn load_hints(cache_dir: &Path, project_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(hints_path(cache_dir, project_dir)) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|n| !n.is_empty() && fnpm_types::name::is_npm_package_name(n))
        .take(MAX_HINTS)
        .map(str::to_owned)
        .collect()
}

/// Saves the request order (atomically: a temporary file renamed over the old one). Failures
/// are ignored; hints are an optimisation.
pub fn save_hints(cache_dir: &Path, project_dir: &Path, names_in_request_order: &[String]) {
    let path = hints_path(cache_dir, project_dir);
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut text = String::new();
    for name in names_in_request_order.iter().take(MAX_HINTS) {
        text.push_str(name);
        text.push('\n');
    }
    if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return;
    }
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    ));
    let written = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(text.as_bytes()));
    if written.is_err() || std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let a = Path::new("/projects/a");
        let b = Path::new("/projects/b");
        assert!(load_hints(dir.path(), a).is_empty());
        save_hints(
            dir.path(),
            a,
            &["react".into(), "@types/node".into(), "react-dom".into()],
        );
        assert_eq!(
            load_hints(dir.path(), a),
            ["react", "@types/node", "react-dom"]
        );
        assert!(load_hints(dir.path(), b).is_empty());
        save_hints(dir.path(), a, &["left-pad".into()]);
        assert_eq!(load_hints(dir.path(), a), ["left-pad"]);
    }

    #[test]
    fn invalid_names_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = hints_path(dir.path(), Path::new("/x"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "ok\n\n../evil\nAlso Bad\n@scope/fine\n").unwrap();
        assert_eq!(
            load_hints(dir.path(), Path::new("/x")),
            ["ok", "@scope/fine"]
        );
    }
}
