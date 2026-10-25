//! The store through its public API, as the core and the linker use it.

use std::path::Path;

use fnpm_store::fsops::{Backend, StickyBackend, materialize_package};
use fnpm_store::{ImportOptions, PackageKey, Store, Tuning};
use fnpm_types::{HashAlgorithm, Integrity};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

#[test]
fn import_get_link() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with_tuning(&dir.path().join("store/v1"), Tuning::default()).unwrap();
    let tgz = fixture("is-odd-1.0.0.tgz");
    let key = PackageKey {
        name: "is-odd".into(),
        version: "1.0.0".into(),
        integrity: Integrity::compute(HashAlgorithm::Sha512, &tgz),
    };
    assert!(store.get(&key).unwrap().is_none());
    let entry = store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    let hit = store.get(&key).unwrap().unwrap();
    assert_eq!(hit.root, entry.root);
    assert!(
        hit.index
            .package_json
            .as_deref()
            .unwrap()
            .contains("is-odd")
    );

    let dst = dir.path().join("app/node_modules/is-odd");
    let sticky = StickyBackend::new(fnpm_store::fsops::default_backend());
    let stats = materialize_package(&hit.root, &hit.index, &dst, &sticky).unwrap();
    assert_eq!(stats.files, 4);
    assert!(dst.join("index.js").is_file());

    store
        .set_project_packages(&dir.path().join("app"), [key.dir_name()])
        .unwrap();
    let status = store.status().unwrap();
    assert_eq!((status.packages, status.files), (1, 4));
    assert_ne!(sticky.current(), Backend::Symlink);
}

#[test]
fn streamed_import_on_a_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with_tuning(
        dir.path(),
        Tuning {
            streaming_min_size: 1,
            drain_threshold: 64,
            ..Tuning::default()
        },
    )
    .unwrap();
    let tgz = fixture("left-pad-1.3.0.tgz");
    let key = PackageKey {
        name: "left-pad".into(),
        version: "1.3.0".into(),
        integrity: Integrity::compute(HashAlgorithm::Sha1, &tgz),
    };
    let chunks: Vec<std::io::Result<bytes::Bytes>> = tgz
        .chunks(100)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let entry = rt
        .block_on(store.import_tarball(
            &key,
            futures_util::stream::iter(chunks),
            ImportOptions::default(),
        ))
        .unwrap();
    assert_eq!(entry.index.files.len(), 10);
    assert_eq!(
        std::fs::read(entry.root.join("test.js")).unwrap().len(),
        4005
    );
}
