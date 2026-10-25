#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// A temporary project. Files are `(relative path, contents)`.
pub fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (path, contents) in files {
        write(dir.path(), path, contents);
    }
    dir
}

pub fn write(root: &Path, path: &str, contents: &str) {
    let p = root.join(path);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, contents).expect("write");
}

/// `{"name": name}` plus `extra` fields.
pub fn pkg(name: &str, extra: Value) -> String {
    let mut v = json!({ "name": name });
    if let Value::Object(m) = extra {
        v.as_object_mut().unwrap().extend(m);
    }
    v.to_string()
}

pub fn canon(p: &Path) -> PathBuf {
    p.to_path_buf()
}
