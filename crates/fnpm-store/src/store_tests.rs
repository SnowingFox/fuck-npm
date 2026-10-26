//! Store-level tests: real npm tarballs (from bun's `test/cli/install`), malicious archives,
//! concurrency, streaming, dedupe, prune and packs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fnpm_types::{HashAlgorithm, Integrity};

use crate::testutil::*;
use crate::{Error, ImportOptions, PackageKey, PruneOptions, Store, Tuning};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

fn key_for(name: &str, version: &str, tgz: &[u8]) -> PackageKey {
    PackageKey {
        name: name.into(),
        version: version.into(),
        integrity: Integrity::compute(HashAlgorithm::Sha512, tgz),
    }
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with_tuning(&dir.path().join("store/v2"), Tuning::default()).unwrap();
    (dir, store)
}

fn tmp_is_empty(store: &Store) -> bool {
    std::fs::read_dir(store.tmp_dir()).unwrap().next().is_none()
}

fn paths(entry: &crate::PackageEntry) -> Vec<String> {
    entry.index.files.iter().map(|f| f.path.clone()).collect()
}

#[cfg(unix)]
fn nlink(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().nlink()
}

#[cfg(unix)]
fn ino(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().ino()
}

fn read_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    crate::pack::walk_files(root)
        .unwrap()
        .into_iter()
        .map(|f| {
            let data = std::fs::read(root.join(&f.path)).unwrap();
            (f.path, data)
        })
        .collect()
}

#[test]
fn imports_real_npm_tarballs() {
    let (_t, store) = store();
    let tgz = fixture("left-pad-1.3.0.tgz");
    let key = key_for("left-pad", "1.3.0", &tgz);
    let entry = store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    assert_eq!(entry.root, store.package_dir(&key).join("package"));
    assert_eq!(
        paths(&entry),
        vec![
            ".travis.yml",
            "COPYING",
            "README.md",
            "index.d.ts",
            "index.js",
            "package.json",
            "perf/O(n).js",
            "perf/es6Repeat.js",
            "perf/perf.js",
            "test.js"
        ]
    );
    assert_eq!(entry.index.dirs, vec!["perf"]);
    assert!(
        entry
            .index
            .package_json
            .as_deref()
            .unwrap()
            .contains("\"left-pad\"")
    );
    assert_eq!(
        std::fs::metadata(entry.root.join("test.js")).unwrap().len(),
        4005
    );
    assert_eq!(
        entry.index.total_size(),
        646 + 58 + 502 + 302 + 1469 + 871 + 4005 + 216 + 241 + 1442
    );
    // Every file was written once, into the package itself: nothing else in the store links
    // to it, and there is no blob directory.
    #[cfg(unix)]
    for f in &entry.index.files {
        assert_eq!(nlink(&entry.root.join(&f.path)), 1, "{}", f.path);
    }
    assert!(!store.root().join("files").exists());
    // A hit reads the index.
    let again = store.get(&key).unwrap().unwrap();
    assert_eq!(*again.index, *entry.index);
    assert!(tmp_is_empty(&store));
}

#[test]
fn mac_metadata_files_and_vendor_pax_keys() {
    // actual-package-2.0.0.tgz has `._package` at the root (no second component), AppleDouble
    // files and LIBARCHIVE.xattr pax keys.
    let (_t, store) = store();
    let tgz = fixture("actual-package-2.0.0.tgz");
    let entry = store
        .import_tarball_bytes(
            &key_for("actual-package", "2.0.0", &tgz),
            &tgz,
            ImportOptions::default(),
        )
        .unwrap();
    assert_eq!(
        paths(&entry),
        vec!["._cli.js", "._package.json", "cli.js", "package.json"]
    );
    let cli = entry.index.file("cli.js").unwrap();
    assert_eq!(cli.mode, 0o755);
    assert!(cli.is_executable());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(entry.root.join("cli.js"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
}

#[test]
fn all_fixtures_extract_like_tar() {
    let (_t, store) = store();
    for (name, version, expected) in [
        ("bar", "0.0.2", vec!["package.json"]),
        ("baz", "0.0.3", vec!["index.js", "package.json"]),
        (
            "is-odd",
            "1.0.0",
            vec!["LICENSE", "README.md", "index.js", "package.json"],
        ),
        (
            "is-even",
            "1.0.0",
            vec!["LICENSE", "README.md", "index.js", "package.json"],
        ),
    ] {
        let tgz = fixture(&format!("{name}-{version}.tgz"));
        let entry = store
            .import_tarball_bytes(
                &key_for(name, version, &tgz),
                &tgz,
                ImportOptions::default(),
            )
            .unwrap();
        assert_eq!(paths(&entry), expected, "{name}");
        // Same bytes as the archive.
        let mut gz = flate2::read::GzDecoder::new(&tgz[..]);
        let mut tar = Vec::new();
        std::io::Read::read_to_end(&mut gz, &mut tar).unwrap();
        for f in &entry.index.files {
            let on_disk = std::fs::read(entry.root.join(&f.path)).unwrap();
            assert!(
                tar.windows(on_disk.len().max(1))
                    .any(|w| w == on_disk.as_slice()),
                "{name}/{}",
                f.path
            );
        }
    }
    assert_eq!(store.status().unwrap().packages, 4);
}

/// A package whose `lib/` and `dist/` hold the same bytes, plus near-misses that must not be
/// shared: same bytes with another mode, same size with other bytes.
fn duplicated_package(name: &str, shared: &[u8]) -> Vec<u8> {
    let mut other = shared.to_vec();
    other[0] ^= 1;
    TarBuilder::new()
        .file(
            "package/package.json",
            format!(r#"{{"name":"{name}"}}"#).as_bytes(),
        )
        .file("package/lib/big.bin", shared)
        .file("package/dist/big.bin", shared)
        .file("package/esm/big.bin", shared)
        .file_mode("package/bin/big.bin", shared, 0o755)
        .file("package/other.bin", &other)
        .file("package/a/empty", b"")
        .file("package/b/empty", b"")
        .end()
        .tgz()
}

fn check_duplicated_package(entry: &crate::PackageEntry, shared: &[u8]) {
    let at = |p: &str| entry.root.join(p);
    for p in ["lib/big.bin", "dist/big.bin", "esm/big.bin", "bin/big.bin"] {
        assert_eq!(std::fs::read(at(p)).unwrap(), shared, "{p}");
    }
    assert_ne!(std::fs::read(at("other.bin")).unwrap(), shared);
    assert_eq!(std::fs::read(at("b/empty")).unwrap(), b"");
    #[cfg(unix)]
    {
        assert_eq!(ino(&at("lib/big.bin")), ino(&at("dist/big.bin")));
        assert_eq!(ino(&at("lib/big.bin")), ino(&at("esm/big.bin")));
        assert_eq!(nlink(&at("lib/big.bin")), 3);
        assert_ne!(ino(&at("lib/big.bin")), ino(&at("bin/big.bin")));
        assert_ne!(ino(&at("lib/big.bin")), ino(&at("other.bin")));
        assert_eq!(ino(&at("a/empty")), ino(&at("b/empty")));
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str| std::fs::metadata(at(p)).unwrap().permissions().mode() & 0o111;
        assert_eq!(mode("lib/big.bin"), 0);
        assert_ne!(mode("bin/big.bin"), 0);
    }
    assert_eq!(entry.index.files.len(), 8);
}

#[test]
fn identical_files_in_a_package_share_an_inode() {
    let (_t, store) = store();
    let shared = incompressible(100_000, 5);
    let a = duplicated_package("a", &shared);
    let ea = store
        .import_tarball_bytes(&key_for("a", "1.0.0", &a), &a, ImportOptions::default())
        .unwrap();
    check_duplicated_package(&ea, &shared);
    // Another package with the same bytes gets its own copy: packages never share files.
    let b = duplicated_package("b", &shared);
    let eb = store
        .import_tarball_bytes(&key_for("b", "1.0.0", &b), &b, ImportOptions::default())
        .unwrap();
    check_duplicated_package(&eb, &shared);
    #[cfg(unix)]
    assert_ne!(
        ino(&ea.root.join("lib/big.bin")),
        ino(&eb.root.join("lib/big.bin"))
    );
    let status = store.status().unwrap();
    // Per package: package.json, big.bin, bin/big.bin, other.bin, empty.
    assert_eq!(status.files, 10);
    assert!(
        status.bytes >= 600_000 && status.bytes < 800_000,
        "{}",
        status.bytes
    );
    // Equal bytes in a separate file still consume another file, unlike a hard link.
    let separate = ea.root.join("lib/big.bin");
    std::fs::remove_file(&separate).unwrap();
    std::fs::copy(ea.root.join("dist/big.bin"), &separate).unwrap();
    assert_eq!(store.status().unwrap().files, 11);
}

#[test]
fn identical_files_share_an_inode_when_streamed() {
    let shared = incompressible(300_000, 9);
    let tgz = duplicated_package("s", &shared);
    let rt = runtime();
    let (_t, store) = store();
    let key = key_for("s", "1.0.0", &tgz);
    let tuning = Tuning {
        streaming_min_size: 1,
        drain_threshold: 7_000,
        ..Tuning::default()
    };
    let store = Store::open_with_tuning(store.root(), tuning).unwrap();
    let entry = rt
        .block_on(store.import_tarball(&key, body(&tgz, 4096), ImportOptions::default()))
        .unwrap();
    check_duplicated_package(&entry, &shared);
    assert!(tmp_is_empty(&store));
}

#[test]
fn integrity_mismatch_is_rejected_and_leaves_nothing() {
    let (_t, store) = store();
    let tgz = fixture("baz-0.0.3.tgz");
    let other = fixture("bar-0.0.2.tgz");
    let bad = key_for("baz", "0.0.3", &other);
    let err = store
        .import_tarball_bytes(&bad, &tgz, ImportOptions::default())
        .unwrap_err();
    match &err {
        Error::Integrity {
            package,
            expected,
            actual,
        } => {
            assert_eq!(package, "baz");
            assert_eq!(expected, &bad.integrity.to_sri());
            assert_eq!(
                actual,
                &Integrity::compute(HashAlgorithm::Sha512, &tgz).to_sri()
            );
        }
        e => panic!("{e}"),
    }
    assert!(store.get(&bad).unwrap().is_none());
    assert!(!store.package_dir(&bad).exists());
    assert!(tmp_is_empty(&store));
    // --no-verify installs it anyway.
    let entry = store
        .import_tarball_bytes(&bad, &tgz, ImportOptions { skip_verify: true })
        .unwrap();
    assert_eq!(paths(&entry), vec!["index.js", "package.json"]);
}

#[test]
fn verifies_every_algorithm() {
    let (_t, store) = store();
    let tgz = fixture("bar-0.0.2.tgz");
    for algorithm in [
        HashAlgorithm::Sha1,
        HashAlgorithm::Sha256,
        HashAlgorithm::Sha384,
        HashAlgorithm::Sha512,
    ] {
        let key = PackageKey {
            name: "bar".into(),
            version: "0.0.2".into(),
            integrity: Integrity::compute(algorithm, &tgz),
        };
        store
            .import_tarball_bytes(&key, &tgz, ImportOptions::default())
            .unwrap();
        assert!(store.get(&key).unwrap().is_some());
    }
}

#[test]
fn unpinned_import_computes_sha512() {
    let (_t, store) = store();
    let tgz = fixture("baz-0.0.3.tgz");
    let (key, entry) = store
        .import_tarball_bytes_unpinned("baz", "0.0.3", &tgz)
        .unwrap();
    assert_eq!(
        key.integrity,
        Integrity::compute(HashAlgorithm::Sha512, &tgz)
    );
    assert_eq!(store.get(&key).unwrap().unwrap().root, entry.root);
}

#[test]
fn concurrent_imports_of_one_key_all_succeed() {
    let (_t, store) = store();
    let tgz = Arc::new(fixture("left-pad-1.3.0.tgz"));
    let key = key_for("left-pad", "1.3.0", &tgz);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (store, tgz, key) = (store.clone(), tgz.clone(), key.clone());
            std::thread::spawn(move || {
                store
                    .import_tarball_bytes(&key, &tgz, ImportOptions::default())
                    .unwrap()
            })
        })
        .collect();
    let entries: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for e in &entries {
        assert_eq!(e.root, entries[0].root);
        assert_eq!(e.index.files.len(), 10);
    }
    assert!(tmp_is_empty(&store));
    assert_eq!(std::fs::read_dir(store.pkgs_dir()).unwrap().count(), 1);
}

#[test]
fn incomplete_or_damaged_entries_are_replaced() {
    let (_t, store) = store();
    let tgz = fixture("baz-0.0.3.tgz");
    let key = key_for("baz", "0.0.3", &tgz);
    let dir = store.package_dir(&key);
    std::fs::create_dir_all(dir.join("package")).unwrap();
    std::fs::write(dir.join("package/leftover"), "x").unwrap();
    assert!(store.get(&key).unwrap().is_none());
    let entry = store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    assert!(!entry.root.join("leftover").exists());
    std::fs::write(dir.join("index.bin"), b"FNPMIDX\x01garbage").unwrap();
    assert!(store.get(&key).unwrap().is_none());
    store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    assert!(store.get(&key).unwrap().is_some());
}

#[test]
fn scoped_names_and_patched_dir_names() {
    let tgz = fixture("bar-0.0.2.tgz");
    let key = key_for("@types/node", "20.1.0", &tgz);
    let hex = key.integrity.to_hex();
    assert_eq!(key.dir_name(), format!("@types+node@20.1.0-{}", &hex[..16]));
    assert_eq!(
        key.patched_dir_name(0xabc),
        format!("@types+node@20.1.0-{}_patch_hash=abc", &hex[..16])
    );
    let (_t, store) = store();
    let e = store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    assert!(e.root.starts_with(store.pkgs_dir().join(key.dir_name())));
}

/// Every file under `root`, relative, for "nothing escaped" checks.
fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_path_buf());
            }
        }
    }
    walk(root, root, &mut out);
    out
}

#[test]
fn malicious_paths_never_leave_the_package() {
    let (t, store) = store();
    let mut b = TarBuilder::new();
    b.file("package/package.json", b"{}");
    for evil in [
        "package/../climb-1.txt",
        "package/../../../climb-3.txt",
        "package/nested/../../climb-nested.txt",
        "package/a/../climb-inside.txt",
        "/etc/climb-abs",
        "package//../climb-slash",
        "package/C:/climb-drive",
        "package/..\\climb-backslash",
    ] {
        b.file(evil, evil.as_bytes());
    }
    // Symlinks and hard links are skipped, and a file "through" one is just a file.
    b.entry(
        header_with(b"package/link", 0, b'2', |h| {
            h[157..161].copy_from_slice(b"/etc")
        }),
        b"",
    );
    b.entry(
        header_with(b"package/hard", 0, b'1', |h| {
            h[157..168].copy_from_slice(b"/etc/passwd")
        }),
        b"",
    );
    b.file("package/link/passwd", b"not through the link");
    b.end();
    let tgz = b.tgz();
    let entry = store
        .import_tarball_bytes(
            &key_for("evil", "1.0.0", &tgz),
            &tgz,
            ImportOptions::default(),
        )
        .unwrap();
    assert_eq!(
        paths(&entry),
        vec!["etc/climb-abs", "link/passwd", "package.json"]
    );
    assert!(
        std::fs::symlink_metadata(entry.root.join("link"))
            .unwrap()
            .is_dir()
    );
    let everything = all_files(t.path());
    let allowed = entry.root.join("etc/climb-abs");
    let allowed = allowed.strip_prefix(t.path()).unwrap();
    assert!(
        everything.iter().all(|p| {
            !p.components()
                .any(|c| c.as_os_str().to_string_lossy().contains("climb"))
                || p == allowed
        }),
        "{everything:?}"
    );
}

#[test]
fn file_and_directory_with_one_name_is_an_error() {
    let (_t, store) = store();
    for tgz in [
        TarBuilder::new()
            .file("package/a", b"file")
            .file("package/a/b", b"inside")
            .end()
            .tgz(),
        TarBuilder::new()
            .file("package/a/b", b"inside")
            .file("package/a", b"file")
            .end()
            .tgz(),
    ] {
        let err = store
            .import_tarball_bytes(&key_for("c", "1.0.0", &tgz), &tgz, ImportOptions::default())
            .unwrap_err();
        assert!(
            matches!(&err, Error::InvalidTarball { package, message } if package == "c" && message.contains("both a file and a directory")),
            "{err}"
        );
        assert!(tmp_is_empty(&store));
    }
}

#[test]
fn duplicate_entries_last_one_wins() {
    let (_t, store) = store();
    let tgz = TarBuilder::new()
        .file("package/a.js", b"first")
        .file("package/b.js", b"b")
        .file("package/a.js", b"second")
        .end()
        .tgz();
    let e = store
        .import_tarball_bytes(&key_for("d", "1.0.0", &tgz), &tgz, ImportOptions::default())
        .unwrap();
    assert_eq!(paths(&e), vec!["a.js", "b.js"]);
    assert_eq!(std::fs::read(e.root.join("a.js")).unwrap(), b"second");
}

#[test]
fn broken_archives_fail_and_leave_no_staging() {
    let (_t, store) = store();
    let mut lie = TarBuilder::new().file("package/package.json", b"{}").tar();
    lie.extend_from_slice(&header("package/big.bin", 16 * 1024 * 1024, b'0'));
    lie.extend_from_slice(&[0x78; 100]);
    lie.extend_from_slice(&pad(100));
    lie.extend_from_slice(&[0; 1024]);
    let corrupt = {
        let mut gz = TarBuilder::new()
            .file("package/x", &incompressible(50_000, 2))
            .end()
            .tgz();
        let mid = gz.len() / 2;
        gz[mid..mid + 64].fill(0x55);
        gz
    };
    let truncated = {
        let gz = TarBuilder::new()
            .file("package/x", &incompressible(50_000, 2))
            .end()
            .tgz();
        gz[..gz.len() - 20].to_vec()
    };
    for (what, body) in [
        ("size lie", gzip(&lie)),
        ("not gzip", b"this is not a gzip stream".to_vec()),
        ("corrupt", corrupt),
        ("truncated", truncated),
        ("empty", Vec::new()),
    ] {
        let err = store
            .import_tarball_bytes(
                &key_for("p", "1.0.0", &body),
                &body,
                ImportOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTarball { .. }), "{what}: {err}");
        assert!(tmp_is_empty(&store), "{what}");
    }
    assert_eq!(std::fs::read_dir(store.pkgs_dir()).unwrap().count(), 0);
}

#[test]
fn plain_tar_and_multi_member_gzip() {
    let (_t, store) = store();
    let tar = TarBuilder::new()
        .file("package/package.json", b"{}")
        .file("package/i.js", b"i")
        .end()
        .tar();
    let e = store
        .import_tarball_bytes(
            &key_for("plain", "1.0.0", &tar),
            &tar,
            ImportOptions::default(),
        )
        .unwrap();
    assert_eq!(paths(&e), vec!["i.js", "package.json"]);
    // The tar split over two gzip members: libdeflate cannot take it, the streaming decoder can.
    let mut two = gzip(&tar[..700]);
    two.extend(gzip(&tar[700..]));
    let e = store
        .import_tarball_bytes(
            &key_for("multi", "1.0.0", &two),
            &two,
            ImportOptions::default(),
        )
        .unwrap();
    assert_eq!(paths(&e), vec!["i.js", "package.json"]);
    // Bytes after the gzip stream are ignored (but part of the integrity).
    let mut trailing = gzip(&tar);
    trailing.extend_from_slice(b"\0\0\0junk");
    let e = store
        .import_tarball_bytes(
            &key_for("trail", "1.0.0", &trailing),
            &trailing,
            ImportOptions::default(),
        )
        .unwrap();
    assert_eq!(paths(&e), vec!["i.js", "package.json"]);
}

/// The entries of bun's streaming test: package.json, index.js, a long pax path and 48
/// incompressible chunks (> 2 MiB compressed).
fn stream_entries() -> Vec<(String, Vec<u8>)> {
    let mut entries = vec![
        ("package.json".to_owned(), b"{\"name\":\"stream-pkg\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n".to_vec()),
        ("index.js".to_owned(), b"module.exports = 'ok';\n".to_vec()),
        (
            "very/deeply/nested/directory/structure/that/exceeds/the/one/hundred/byte/ustar/limit/long-name-file.txt".to_owned(),
            b"long path ok\n".to_vec(),
        ),
    ];
    for i in 0..48 {
        entries.push((
            format!("data/chunk-{i}.bin"),
            incompressible(48 * 1024, i + 1),
        ));
    }
    entries
}

fn stream_tgz(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut b = TarBuilder::new();
    for (p, d) in entries {
        b.file(&format!("package/{p}"), d);
    }
    b.end().tgz()
}

fn body(
    tgz: &[u8],
    chunk: usize,
) -> impl futures_util::Stream<Item = std::io::Result<bytes::Bytes>> + Send + Unpin + use<> {
    let chunks: Vec<std::io::Result<bytes::Bytes>> = tgz
        .chunks(chunk)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();
    futures_util::stream::iter(chunks)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn check_stream_entries(root: &Path, entries: &[(String, Vec<u8>)]) {
    for (p, d) in entries {
        assert_eq!(&std::fs::read(root.join(p)).unwrap(), d, "{p}");
    }
}

#[test]
fn streaming_and_buffered_give_the_same_package() {
    let entries = stream_entries();
    let tgz = stream_tgz(&entries);
    assert!(tgz.len() > 2 * 1024 * 1024);
    let rt = runtime();
    for (label, tuning, chunk) in [
        ("streaming", Tuning::default(), 4096),
        (
            "streaming, tiny chunks, drain 1",
            Tuning {
                drain_threshold: 1,
                ..Tuning::default()
            },
            1200,
        ),
        (
            "streaming, first chunk < gzip header",
            Tuning {
                streaming_min_size: 1,
                drain_threshold: 1,
                ..Tuning::default()
            },
            2,
        ),
        (
            "buffered (min size above body)",
            Tuning {
                streaming_min_size: tgz.len() as u64 + 1,
                ..Tuning::default()
            },
            65536,
        ),
        (
            "buffered (streaming disabled)",
            Tuning {
                disable_streaming: true,
                ..Tuning::default()
            },
            65536,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_with_tuning(&dir.path().join("s"), tuning).unwrap();
        let key = key_for("stream-pkg", "1.0.0", &tgz);
        // Two-byte chunks for the whole body would be slow; only the head is that small.
        let head = if chunk == 2 { 64 } else { 0 };
        let mut pieces: Vec<std::io::Result<bytes::Bytes>> = tgz[..head]
            .chunks(2)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        pieces.extend(
            tgz[head..]
                .chunks(chunk.max(1200))
                .map(|c| Ok(bytes::Bytes::copy_from_slice(c))),
        );
        let entry = rt
            .block_on(store.import_tarball(
                &key,
                futures_util::stream::iter(pieces),
                ImportOptions::default(),
            ))
            .unwrap();
        check_stream_entries(&entry.root, &entries);
        assert_eq!(entry.index.files.len(), 51, "{label}");
        assert_eq!(
            store.get(&key).unwrap().unwrap().index,
            entry.index,
            "{label}"
        );
        assert!(tmp_is_empty(&store), "{label}");
    }
}

#[test]
fn streaming_rejects_a_bad_integrity() {
    let entries = stream_entries();
    let tgz = stream_tgz(&entries);
    let rt = runtime();
    let (_t, store) = store();
    let key = key_for("stream-pkg", "1.0.0", b"another tarball");
    let err = rt
        .block_on(store.import_tarball(&key, body(&tgz, 4096), ImportOptions::default()))
        .unwrap_err();
    assert!(matches!(err, Error::Integrity { .. }), "{err}");
    assert!(tmp_is_empty(&store));
    assert_eq!(std::fs::read_dir(store.pkgs_dir()).unwrap().count(), 0);
}

#[test]
fn streaming_body_error_is_reported_and_cleaned_up() {
    let entries = stream_entries();
    let tgz = stream_tgz(&entries);
    let rt = runtime();
    let (_t, store) = store();
    let key = key_for("stream-pkg", "1.0.0", &tgz);
    let mut pieces: Vec<std::io::Result<bytes::Bytes>> = tgz[..tgz.len() / 2 + 100_000]
        .chunks(65536)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();
    pieces.push(Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "connection closed",
    )));
    let err = rt
        .block_on(store.import_tarball(
            &key,
            futures_util::stream::iter(pieces),
            ImportOptions::default(),
        ))
        .unwrap_err();
    assert!(
        matches!(&err, Error::Io { source, .. } if source.kind() == std::io::ErrorKind::ConnectionReset),
        "{err}"
    );
    // The drain that was running finishes and removes its staging dir.
    for _ in 0..200 {
        if tmp_is_empty(&store) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(tmp_is_empty(&store));
}

#[test]
fn streaming_unpinned_and_truncated() {
    let entries = stream_entries();
    let tgz = stream_tgz(&entries);
    let rt = runtime();
    let (_t, store) = store();
    let (key, entry) = rt
        .block_on(store.import_tarball_unpinned("stream-pkg", "1.0.0", body(&tgz, 30_000)))
        .unwrap();
    assert_eq!(
        key.integrity,
        Integrity::compute(HashAlgorithm::Sha512, &tgz)
    );
    check_stream_entries(&entry.root, &entries);
    let cut = &tgz[..tgz.len() - 1000];
    let err = rt
        .block_on(store.import_tarball_unpinned("cut", "1.0.0", body(cut, 30_000)))
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTarball { .. }), "{err}");
    assert!(tmp_is_empty(&store));
}

#[test]
fn large_files_stream_to_disk() {
    let big = incompressible(9 * 1024 * 1024, 11);
    let a = TarBuilder::new()
        .file("package/package.json", b"{}")
        .file("package/big.wasm", &big)
        .end()
        .tgz();
    let b = TarBuilder::new()
        .file("package/package.json", b"{\"v\":2}")
        .file("package/big.wasm", &big)
        .end()
        .tgz();
    let rt = runtime();
    let (_t, store) = store();
    let ka = key_for("w", "1.0.0", &a);
    let kb = key_for("w", "2.0.0", &b);
    let ea = rt
        .block_on(store.import_tarball(&ka, body(&a, 256 * 1024), ImportOptions::default()))
        .unwrap();
    let eb = rt
        .block_on(store.import_tarball(&kb, body(&b, 256 * 1024), ImportOptions::default()))
        .unwrap();
    assert_eq!(std::fs::read(ea.root.join("big.wasm")).unwrap(), big);
    assert_eq!(std::fs::read(eb.root.join("big.wasm")).unwrap(), big);
    assert_eq!(ea.index.file("big.wasm").unwrap().size, big.len() as u64);
    assert_eq!(paths(&eb), vec!["big.wasm", "package.json"]);
}

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (p, d) in files {
        let path = root.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, d).unwrap();
    }
}

#[test]
fn directory_import_and_pack_integrity() {
    let (t, store) = store();
    let src = t.path().join("checkout");
    write_files(
        &src,
        &[
            ("package.json", r#"{"name":"g","version":"1.0.0"}"#),
            ("lib/a.js", "a"),
            ("b.js", "b"),
        ],
    );
    let i1 = Store::pack_integrity(&src).unwrap();
    assert_eq!(i1, Store::pack_integrity(&src).unwrap());
    assert_eq!(i1.algorithm(), HashAlgorithm::Sha512);
    let key = PackageKey {
        name: "g".into(),
        version: "1.0.0".into(),
        integrity: i1,
    };
    let e = store.import_directory(&key, &src).unwrap();
    assert_eq!(paths(&e), vec!["b.js", "lib/a.js", "package.json"]);
    assert!(e.index.package_json.as_deref().unwrap().contains("\"g\""));
    // The integrity follows the contents.
    std::fs::write(src.join("b.js"), "changed").unwrap();
    assert_ne!(Store::pack_integrity(&src).unwrap(), i1);
}

const PATCH: &str = "diff --git a/index.js b/index.js
index 832d92223a9ec491364ee10dcbe3ad495446ab80..7e079f817cba5aa9ca4b1c1dd6e2b3c1d5d4d3b1 100644
--- a/index.js
+++ b/index.js
@@ -1,3 +1,3 @@
 const a = 1;
-module.exports = 'original';
+module.exports = 'patched';
 // end
diff --git a/added.txt b/added.txt
new file mode 100644
index 0000000000000000000000000000000000000000..2e65efe2a145dda7ee51d1741299f848e5bf752e
--- /dev/null
+++ b/added.txt
@@ -0,0 +1 @@
+new file
diff --git a/remove.txt b/remove.txt
deleted file mode 100644
index 2e65efe2a145dda7ee51d1741299f848e5bf752e..0000000000000000000000000000000000000000
--- a/remove.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/logo.png b/logo.png
index 1111111..2222222 100644
Binary files a/logo.png and b/logo.png differ
";

#[test]
fn patched_copies_leave_the_base_package_alone() {
    let (_t, store) = store();
    let tgz = TarBuilder::new()
        .file("package/package.json", br#"{"name":"p","version":"1.0.0"}"#)
        .file(
            "package/index.js",
            b"const a = 1;\nmodule.exports = 'original';\n// end\n",
        )
        .file("package/remove.txt", b"bye\n")
        .file("package/keep.txt", b"keep\n")
        .end()
        .tgz();
    let key = key_for("p", "1.0.0", &tgz);
    let base = store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    let hash = crate::patch::hash_patch(PATCH.as_bytes());
    assert!(store.get_patched(&key, hash).unwrap().is_none());
    let patched = store.import_patched(&key, &base, PATCH, hash).unwrap();
    assert_eq!(patched.warnings.len(), 1);
    assert!(
        patched.warnings[0].starts_with("skipping logo.png: binary file, content not in patch")
    );
    let root = &patched.entry.root;
    assert_eq!(
        std::fs::read_to_string(root.join("index.js")).unwrap(),
        "const a = 1;\nmodule.exports = 'patched';\n// end\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("added.txt")).unwrap(),
        "new file\n"
    );
    assert!(!root.join("remove.txt").exists());
    assert_eq!(
        paths(&patched.entry),
        vec!["added.txt", "index.js", "keep.txt", "package.json"]
    );
    // The base package is unchanged.
    assert_eq!(
        std::fs::read_to_string(base.root.join("index.js")).unwrap(),
        "const a = 1;\nmodule.exports = 'original';\n// end\n"
    );
    assert!(base.root.join("remove.txt").exists());
    #[cfg(unix)]
    assert_eq!(
        ino(&root.join("keep.txt")),
        ino(&base.root.join("keep.txt"))
    );
    let got = store.get_patched(&key, hash).unwrap().unwrap();
    assert_eq!(got.root, *root);
    assert!(got.root.starts_with(store.patched_package_dir(&key, hash)));
    // The unpatched entry is still what `get` returns.
    assert_eq!(store.get(&key).unwrap().unwrap().root, base.root);
    // A patch that does not apply is an error and leaves nothing.
    let bad = "diff --git a/index.js b/index.js\n--- a/index.js\n+++ b/index.js\n@@ -1,1 +1,1 @@\n-nothing like this\n+x\n";
    let err = store.import_patched(&key, &base, bad, 7).unwrap_err();
    assert!(matches!(err, Error::Patch { .. }), "{err}");
    assert!(tmp_is_empty(&store));
}

#[cfg(unix)]
#[test]
fn materialize_with_each_backend() {
    use crate::fsops::{Backend, StickyBackend, materialize_package};
    let (t, store) = store();
    let tgz = fixture("left-pad-1.3.0.tgz");
    let e = store
        .import_tarball_bytes(
            &key_for("left-pad", "1.3.0", &tgz),
            &tgz,
            ImportOptions::default(),
        )
        .unwrap();
    let want = read_tree(&e.root);

    let hard = StickyBackend::new(Backend::Hardlink);
    let dst = t.path().join("nm/hard/left-pad");
    let stats = materialize_package(&e.root, &e.index, &dst, &hard).unwrap();
    assert_eq!((stats.files, stats.hardlinked), (10, 10));
    assert_eq!(read_tree(&dst), want);
    assert_eq!(ino(&dst.join("index.js")), ino(&e.root.join("index.js")));
    // Again over the existing tree (EEXIST is replaced).
    materialize_package(&e.root, &e.index, &dst, &hard).unwrap();

    let copy = StickyBackend::new(Backend::Copyfile);
    let dst = t.path().join("nm/copy/left-pad");
    let stats = materialize_package(&e.root, &e.index, &dst, &copy).unwrap();
    assert_eq!(stats.copied, 10);
    assert_eq!(read_tree(&dst), want);
    assert_eq!(nlink(&dst.join("index.js")), 1);

    // ext4 cannot clone: the clone backend degrades (to hardlink) once, for the process.
    let clone = StickyBackend::new(Backend::Clonefile);
    let dst = t.path().join("nm/clone/left-pad");
    let stats = materialize_package(&e.root, &e.index, &dst, &clone).unwrap();
    assert_eq!(stats.files, 10);
    assert_eq!(read_tree(&dst), want);

    let sym = StickyBackend::new(Backend::Symlink);
    let dst = t.path().join("nm/sym/left-pad");
    materialize_package(&e.root, &e.index, &dst, &sym).unwrap();
    assert!(
        std::fs::symlink_metadata(dst.join("index.js"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read_tree(&e.root), want);
}

#[cfg(target_os = "macos")]
#[test]
fn clonefile_preserves_source_symlinks() {
    use crate::fsops::{Backend, StickyBackend, clone_dir, place_file};
    use std::os::unix::fs::symlink;

    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join("target.js"), "module.exports = 1;").unwrap();
    let src = t.path().join("source.js");
    symlink("target.js", &src).unwrap();
    let dst = t.path().join("cloned.js");
    let sticky = StickyBackend::new(Backend::Clonefile);
    assert_eq!(place_file(&src, &dst, &sticky).unwrap(), Backend::Clonefile);
    assert_eq!(std::fs::read_link(&dst).unwrap(), Path::new("target.js"));
    // EEXIST takes the same no-follow path after replacing the old destination.
    assert_eq!(place_file(&src, &dst, &sticky).unwrap(), Backend::Clonefile);
    assert_eq!(std::fs::read_link(&dst).unwrap(), Path::new("target.js"));

    std::fs::create_dir(t.path().join("target-dir")).unwrap();
    let src = t.path().join("source-dir");
    symlink("target-dir", &src).unwrap();
    let dst = t.path().join("cloned-dir");
    assert!(clone_dir(&src, &dst).unwrap());
    assert_eq!(std::fs::read_link(&dst).unwrap(), Path::new("target-dir"));
}

#[cfg(target_os = "linux")]
#[test]
fn cross_device_hardlink_falls_back_to_copy_and_sticks() {
    use crate::fsops::{Backend, StickyBackend, materialize_package};
    use std::os::unix::fs::MetadataExt;
    let shm = Path::new("/dev/shm");
    let (t, store) = store();
    if !shm.is_dir()
        || std::fs::metadata(shm).unwrap().dev() == std::fs::metadata(t.path()).unwrap().dev()
    {
        return;
    }
    let tgz = fixture("is-odd-1.0.0.tgz");
    let e = store
        .import_tarball_bytes(
            &key_for("is-odd", "1.0.0", &tgz),
            &tgz,
            ImportOptions::default(),
        )
        .unwrap();
    let other = tempfile::tempdir_in(shm).unwrap();
    let sticky = StickyBackend::new(Backend::Hardlink);
    let stats =
        materialize_package(&e.root, &e.index, &other.path().join("is-odd"), &sticky).unwrap();
    assert_eq!(stats.copied, 4);
    assert_eq!(sticky.current(), Backend::Copyfile);
    assert_eq!(read_tree(&other.path().join("is-odd")), read_tree(&e.root));
}

#[cfg(unix)]
#[test]
fn copy_never_truncates_a_hard_link_of_the_source() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src.js");
    let dst = t.path().join("dst.js");
    std::fs::write(&src, "module.exports = 1;").unwrap();
    std::fs::hard_link(&src, &dst).unwrap();
    crate::fsops::copy_file(&src, &dst).unwrap();
    assert_eq!(
        std::fs::read_to_string(&src).unwrap(),
        "module.exports = 1;"
    );
    assert_eq!(
        std::fs::read_to_string(&dst).unwrap(),
        "module.exports = 1;"
    );
    assert_eq!(nlink(&src), 1);
    // Modes are kept.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
    crate::fsops::copy_file(&src, &dst).unwrap();
    assert_eq!(
        std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
        0o755
    );
    // Large files go through copy_file_range.
    let big = incompressible(3 << 20, 4);
    std::fs::write(&src, &big).unwrap();
    crate::fsops::copy_file(&src, &dst).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), big);
}

#[cfg(unix)]
#[test]
fn symlink_dir_is_relative_and_idempotent() {
    use crate::fsops::symlink_dir;
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("store/links/a/node_modules/a");
    std::fs::create_dir_all(&target).unwrap();
    let link = t.path().join("proj/node_modules/.fnpm/a");
    symlink_dir(&target, &link).unwrap();
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        PathBuf::from("../../../store/links/a/node_modules/a")
    );
    symlink_dir(&target, &link).unwrap();
    // Something else in the way is replaced.
    let other = t.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    symlink_dir(&other, &link).unwrap();
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        PathBuf::from("../../../other")
    );
    std::fs::remove_file(&link).unwrap();
    std::fs::create_dir_all(link.join("x")).unwrap();
    symlink_dir(&target, &link).unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        crate::fsops::relative_path(Path::new("/a/b"), Path::new("/a/b")),
        PathBuf::from(".")
    );
    assert_eq!(
        crate::fsops::relative_path(Path::new("/a/b/c"), Path::new("/a/x")),
        PathBuf::from("../../x")
    );
}

#[test]
fn write_atomic_replaces_whole_files() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("state");
    crate::fsops::write_atomic(&p, b"one").unwrap();
    crate::fsops::write_atomic(&p, b"two").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"two");
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 1);
}

#[test]
fn sticky_backend_only_degrades() {
    use crate::fsops::{Backend, StickyBackend};
    let s = StickyBackend::new(Backend::Clonefile);
    s.degrade(Backend::Copyfile);
    s.degrade(Backend::Hardlink);
    assert_eq!(s.current(), Backend::Copyfile);
    assert_eq!(
        crate::fsops::default_backend(),
        if cfg!(target_os = "macos") {
            Backend::Clonefile
        } else {
            Backend::Hardlink
        }
    );
}

fn import_fixture(store: &Store, name: &str, version: &str) -> (PackageKey, crate::PackageEntry) {
    let tgz = fixture(&format!("{name}-{version}.tgz"));
    let key = key_for(name, version, &tgz);
    let e = store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    (key, e)
}

#[cfg(unix)]
#[test]
fn prune_keeps_what_live_projects_use() {
    let (t, store) = store();
    let (odd, odd_e) = import_fixture(&store, "is-odd", "1.0.0");
    let (even, _) = import_fixture(&store, "is-even", "1.0.0");
    let (pad, _) = import_fixture(&store, "left-pad", "1.3.0");
    // A global store entry used by the project, which links another one.
    let links = store.links_dir();
    std::fs::create_dir_all(links.join("is-odd@1.0.0-aaaa/node_modules")).unwrap();
    std::fs::create_dir_all(links.join("dep@1.0.0-bbbb/node_modules/dep")).unwrap();
    std::fs::create_dir_all(links.join("orphan@1.0.0-cccc/node_modules/orphan")).unwrap();
    std::os::unix::fs::symlink(
        links.join("dep@1.0.0-bbbb/node_modules/dep"),
        links.join("is-odd@1.0.0-aaaa/node_modules/dep"),
    )
    .unwrap();
    let project = t.path().join("app");
    std::fs::create_dir_all(project.join("node_modules/.fnpm")).unwrap();
    crate::fsops::symlink_dir(
        &links.join("is-odd@1.0.0-aaaa"),
        &project.join("node_modules/.fnpm/is-odd@1.0.0"),
    )
    .unwrap();
    store
        .set_project_packages(&project, [odd.dir_name()])
        .unwrap();
    let gone = t.path().join("deleted-project");
    std::fs::create_dir_all(&gone).unwrap();
    store
        .set_project_packages(&gone, [even.dir_name()])
        .unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    // A hard link in node_modules keeps a file alive after its package is pruned.
    let pad_entry = store.get(&pad).unwrap().unwrap();
    let installed = project.join("node_modules/left-pad-index.js");
    std::fs::hard_link(pad_entry.root.join("index.js"), &installed).unwrap();

    let dry = crate::prune::prune_with_grace(
        &store,
        PruneOptions { dry_run: true },
        std::time::Duration::ZERO,
    )
    .unwrap();
    assert_eq!((dry.removed_links, dry.removed_packages), (1, 2));
    assert!(store.get(&even).unwrap().is_some());

    let report =
        crate::prune::prune_with_grace(&store, PruneOptions::default(), std::time::Duration::ZERO)
            .unwrap();
    assert_eq!((report.removed_links, report.removed_packages), (1, 2));
    assert!(report.removed_files > 0 && report.bytes_freed > 0);
    // The file hard linked into node_modules is not counted as freed: is-even has 4 files,
    // left-pad 10 of which index.js is still linked, plus each package's index.bin.
    assert_eq!(report.removed_files, 4 + 9 + 2);
    assert!(store.get(&odd).unwrap().is_some());
    assert!(store.get(&even).unwrap().is_none());
    assert!(store.get(&pad).unwrap().is_none());
    assert!(links.join("is-odd@1.0.0-aaaa").exists());
    assert!(links.join("dep@1.0.0-bbbb").exists());
    assert!(!links.join("orphan@1.0.0-cccc").exists());
    assert_eq!(std::fs::read_to_string(&installed).unwrap().len(), 1469);
    // The files of the kept package are still there.
    for f in &odd_e.index.files {
        assert!(odd_e.root.join(&f.path).exists());
    }
    let status = store.status().unwrap();
    assert_eq!((status.packages, status.links), (1, 2));
    assert_eq!(status.files, 4);
    let projects = std::fs::read_to_string(store.root().join("projects.json")).unwrap();
    assert!(crate::sys::is_staging_name(&crate::sys::random_name("")));
    assert!(!crate::sys::is_staging_name("my-own-file"));
    assert!(!projects.contains("deleted-project"));
}

#[test]
fn a_project_without_a_package_list_keeps_every_package() {
    let (t, store) = store();
    let (odd, _) = import_fixture(&store, "is-odd", "1.0.0");
    let project = t.path().join("app");
    std::fs::create_dir_all(&project).unwrap();
    store.register_project(&project).unwrap();
    store.register_project(&project).unwrap();
    let r =
        crate::prune::prune_with_grace(&store, PruneOptions::default(), std::time::Duration::ZERO)
            .unwrap();
    assert_eq!(r.removed_packages, 0);
    assert!(store.get(&odd).unwrap().is_some());
    // The default grace period protects fresh entries even when nothing references them.
    store
        .set_project_packages(&project, Vec::<String>::new())
        .unwrap();
    assert_eq!(
        store
            .prune(PruneOptions::default())
            .unwrap()
            .removed_packages,
        0
    );
    assert_eq!(
        crate::prune::prune_with_grace(&store, PruneOptions::default(), std::time::Duration::ZERO)
            .unwrap()
            .removed_packages,
        1
    );
}

#[cfg(unix)]
fn age(path: &Path, days: u64) {
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    let times = std::fs::FileTimes::new()
        .set_accessed(when)
        .set_modified(when);
    std::fs::File::open(path).unwrap().set_times(times).unwrap();
}

#[cfg(unix)]
#[test]
fn cache_prune_by_age() {
    let (_t, store) = store();
    let (old, _) = import_fixture(&store, "is-odd", "1.0.0");
    let (fresh, _) = import_fixture(&store, "is-even", "1.0.0");
    let dir = store.package_dir(&old);
    age(&dir.join("index.bin"), 40);
    age(&dir, 40);
    let dry = store.prune_older_than(30, true).unwrap();
    assert_eq!((dry.checked, dry.removed), (2, 1));
    assert_eq!(dry.removed_names, vec![old.dir_name()]);
    assert_eq!(
        dry.lines().last().unwrap(),
        "Run without --dry-run to remove them."
    );
    // Reading the index counts as a use, including on filesystems without read atime updates.
    let index_modified = std::fs::metadata(dir.join("index.bin"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(store.get(&old).unwrap().is_some());
    assert_eq!(
        std::fs::metadata(dir.join("index.bin"))
            .unwrap()
            .modified()
            .unwrap(),
        index_modified
    );
    assert_eq!(store.prune_older_than(30, true).unwrap().removed, 0);
    age(&dir.join("index.bin"), 40);
    let r = store.prune_older_than(30, false).unwrap();
    assert_eq!(r.removed, 1);
    assert!(
        r.lines()[0].starts_with("Removed 1 package older than 30 days ("),
        "{:?}",
        r.lines()
    );
    assert!(store.get(&old).unwrap().is_none());
    assert!(store.get(&fresh).unwrap().is_some());
    let none = store.prune_older_than(30, false).unwrap();
    assert_eq!(
        none.lines(),
        vec!["Done! Checked 1 package, none older than 30 days (nothing to prune)".to_owned()]
    );
}

#[test]
fn cache_pack_roundtrip_and_tampering() {
    let (t, store) = store();
    let (a, ea) = import_fixture(&store, "left-pad", "1.3.0");
    let (b, _) = import_fixture(&store, "actual-package", "2.0.0");
    let missing = key_for("nope", "1.0.0", b"never imported");
    let pack_path = t.path().join("out/cache.pack");
    let dry = crate::cache_pack::pack(
        &store,
        &[a.clone(), b.clone(), missing.clone()],
        &pack_path,
        true,
    )
    .unwrap();
    assert!(!pack_path.exists());
    let summary =
        crate::cache_pack::pack(&store, &[b.clone(), a.clone(), missing], &pack_path, false)
            .unwrap();
    assert_eq!(summary, dry);
    assert_eq!(
        (summary.packages, summary.files, summary.missing),
        (2, 14, 1)
    );
    assert!(summary.lines("cache.pack", false)[1].contains("skipping 1 package not in cache"));

    let other_dir = tempfile::tempdir().unwrap();
    let other = Store::open_with_tuning(other_dir.path(), Tuning::default()).unwrap();
    let dry = crate::cache_pack::unpack(&other, &pack_path, true).unwrap();
    assert_eq!((dry.packages, dry.created), (2, 2));
    assert!(other.get(&a).unwrap().is_none());
    let u = crate::cache_pack::unpack(&other, &pack_path, false).unwrap();
    assert_eq!((u.packages, u.created, u.already_cached), (2, 2, 0));
    let got = other.get(&a).unwrap().unwrap();
    assert_eq!(*got.index, *ea.index);
    assert_eq!(read_tree(&got.root), read_tree(&ea.root));
    // Again: nothing is replaced.
    let u = crate::cache_pack::unpack(&other, &pack_path, false).unwrap();
    assert_eq!((u.created, u.already_cached), (0, 2));
    assert!(tmp_is_empty(&other));

    let bytes = std::fs::read(&pack_path).unwrap();
    for (what, damaged) in [
        ("flipped data byte", {
            let mut d = bytes.clone();
            let i = d.len() - 40;
            d[i] ^= 1;
            d
        }),
        ("truncated", bytes[..bytes.len() - 5].to_vec()),
        ("trailing", {
            let mut d = bytes.clone();
            d.push(0);
            d
        }),
        ("magic", {
            let mut d = bytes.clone();
            d[0] = b'X';
            d
        }),
    ] {
        let third_dir = tempfile::tempdir().unwrap();
        let third = Store::open_with_tuning(third_dir.path(), Tuning::default()).unwrap();
        let p = third_dir.path().join("bad.pack");
        std::fs::write(&p, &damaged).unwrap();
        let err = crate::cache_pack::unpack(&third, &p, false).unwrap_err();
        assert!(matches!(err, Error::Refused { .. }), "{what}: {err}");
        assert_eq!(
            std::fs::read_dir(third.pkgs_dir()).unwrap().count(),
            0,
            "{what}"
        );
        assert!(tmp_is_empty(&third), "{what}");
    }
}

#[cfg(unix)]
#[test]
fn tmpdir_override_only_on_the_same_filesystem() {
    let t = tempfile::tempdir().unwrap();
    let staging = t.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let s = Store::open_with_tuning(
        &t.path().join("s"),
        Tuning {
            tmpdir: Some(staging.clone()),
            ..Tuning::default()
        },
    )
    .unwrap();
    assert_eq!(s.tmp_dir(), staging);
    let s = Store::open_with_tuning(
        &t.path().join("s2"),
        Tuning {
            tmpdir: Some(PathBuf::from("relative")),
            ..Tuning::default()
        },
    )
    .unwrap();
    assert_eq!(s.tmp_dir(), t.path().join("s2/tmp"));
}

/// `cargo test --release -p fnpm-store perf_ -- --ignored --nocapture`
#[test]
#[ignore]
fn perf_extract_and_get() {
    let mut b = TarBuilder::new();
    b.file(
        "package/package.json",
        br#"{"name":"perf","version":"1.0.0"}"#,
    );
    let mut total = 0;
    for i in 0..100 {
        let mut body = String::new();
        let mut x: u32 = i + 1;
        while body.len() < 40_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            body.push_str(&format!(
                "export function f{}(a, b) {{ return a * {} + b - {}; }}\n",
                x % 9973,
                x % 101,
                x % 37
            ));
        }
        total += body.len();
        b.file(&format!("package/lib/m{i}.js"), body.as_bytes());
    }
    let tgz = b.end().tgz();
    eprintln!(
        "tarball: {} files, {} KiB uncompressed, {} KiB compressed",
        101,
        total / 1024,
        tgz.len() / 1024
    );
    let (_t, store) = store();
    let rounds = 30;
    let start = std::time::Instant::now();
    let mut keys = Vec::new();
    for r in 0..rounds {
        let key = PackageKey {
            name: format!("perf{r}"),
            version: "1.0.0".into(),
            integrity: Integrity::compute(HashAlgorithm::Sha512, &tgz),
        };
        store
            .import_tarball_bytes(&key, &tgz, ImportOptions::default())
            .unwrap();
        keys.push(key);
    }
    eprintln!(
        "import (warm store): {:?} per tarball",
        start.elapsed() / rounds
    );
    // Cold: a fresh store each time.
    let mut cold = std::time::Duration::ZERO;
    for _ in 0..10 {
        let (_t2, fresh) = super::store_tests::store();
        let key = key_for("perf", "1.0.0", &tgz);
        let s = std::time::Instant::now();
        fresh
            .import_tarball_bytes(&key, &tgz, ImportOptions::default())
            .unwrap();
        cold += s.elapsed();
    }
    eprintln!("import (cold store): {:?} per tarball", cold / 10);
    let start = std::time::Instant::now();
    for _ in 0..100 {
        for k in &keys {
            assert!(store.get(k).unwrap().is_some());
        }
    }
    eprintln!("get: {:?}", start.elapsed() / (100 * rounds));
    let start = std::time::Instant::now();
    let mut inflated = 0;
    for _ in 0..rounds {
        let tar = crate::gzip::inflate_oneshot(&tgz).unwrap();
        inflated += tar.len();
        crate::gzip::recycle(tar);
    }
    eprintln!(
        "inflate alone: {:?} ({} MiB/s)",
        start.elapsed() / rounds,
        inflated as f64 / start.elapsed().as_secs_f64() / 1048576.0
    );
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        std::hint::black_box(Integrity::compute(HashAlgorithm::Sha512, &tgz));
    }
    eprintln!("sha512 of the tgz: {:?}", start.elapsed() / rounds);
    let sticky = crate::fsops::StickyBackend::new(crate::fsops::Backend::Hardlink);
    let e = store.get(&keys[0]).unwrap().unwrap();
    let start = std::time::Instant::now();
    for i in 0..rounds {
        crate::fsops::materialize_package(
            &e.root,
            &e.index,
            &_t.path().join(format!("nm/{i}")),
            &sticky,
        )
        .unwrap();
    }
    eprintln!(
        "materialize (hardlink, 101 files): {:?}",
        start.elapsed() / rounds
    );
}

#[test]
#[ignore]
fn perf_components() {
    let data = incompressible(4 << 20, 3);
    for _ in 0..3 {
        let s = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(ring::digest::digest(&ring::digest::SHA512, &data));
        }
        eprintln!("ring sha512: {:.0} MiB/s", 40.0 / s.elapsed().as_secs_f64());
        let small: Vec<Vec<u8>> = (0..1000)
            .map(|i| incompressible(2000 + i, i as u32 + 1))
            .collect();
        let s = std::time::Instant::now();
        for f in &small {
            std::hint::black_box(ring::digest::digest(&ring::digest::SHA512, f));
        }
        eprintln!("ring sha512 x1000 ~2.5KB files: {:?}", s.elapsed());
        let s = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(xxhash_rust::xxh3::xxh3_128(&data));
        }
        eprintln!("xxh3-128: {:.0} MiB/s", 40.0 / s.elapsed().as_secs_f64());
        let s = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(ring::digest::digest(&ring::digest::SHA256, &data));
        }
        eprintln!("ring sha256: {:.0} MiB/s", 40.0 / s.elapsed().as_secs_f64());
    }
}

fn count_packages(store: &Store) -> usize {
    std::fs::read_dir(store.pkgs_dir()).unwrap().count()
}

#[test]
fn hostile_names_and_versions_stay_inside_pkgs() {
    let (_t, store) = store();
    let tgz = fixture("left-pad-1.3.0.tgz");
    for (name, version) in [
        ("evil", "1/../../../../escape"),
        ("evil", "..\\..\\..\\escape"),
        ("@s/../../x", "1.0.0"),
        ("a:b", "1.0.0\0<>|?*\"\n"),
    ] {
        let key = key_for(name, version, &tgz);
        let dir_name = key.dir_name();
        assert!(
            !dir_name.contains(['/', '\\', '\0', ':', '\n']),
            "{dir_name:?}"
        );
        let dir = store.package_dir(&key);
        assert_eq!(dir.parent().unwrap(), store.pkgs_dir());
        let entry = store
            .import_tarball_bytes(&key, &tgz, ImportOptions::default())
            .unwrap();
        assert!(entry.root.starts_with(store.pkgs_dir()));
        assert!(store.get(&key).unwrap().is_some());
    }
    // Ordinary names keep their documented form.
    let key = key_for("@types/node", "20.1.0+build.1", &tgz);
    assert!(key.dir_name().starts_with("@types+node@20.1.0+build.1-"));
    // Nothing was created next to `pkgs/`.
    let mut top: Vec<String> = std::fs::read_dir(store.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    top.sort();
    assert_eq!(top, ["links", "pkgs", "tmp"]);
}

#[test]
fn buffered_integrity_failure_wins_over_a_broken_archive() {
    let (_t, store) = store();
    let key = key_for("x", "1.0.0", b"the real tarball");
    for body in [
        &b"not a tarball at all, definitely not"[..],
        &gzip(b"garbage")[..],
    ] {
        let err = store
            .import_tarball_bytes(&key, body, ImportOptions::default())
            .unwrap_err();
        assert!(matches!(err, Error::Integrity { .. }), "{err}");
        // With --no-verify the archive error is what is left.
        let err = store
            .import_tarball_bytes(&key, body, ImportOptions { skip_verify: true })
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTarball { .. }), "{err}");
    }
    assert!(tmp_is_empty(&store));
}

fn pack_record(out: &mut Vec<u8>, kind: u8, path: &str, mode: u32, size: u64) {
    out.push(kind);
    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(&mode.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
}

#[test]
fn cache_unpack_package_shorter_than_its_integrity_record_is_refused() {
    let (t, store) = store();
    let (a, _) = import_fixture(&store, "left-pad", "1.3.0");
    let sri = a.integrity.to_sri();
    let mut pack = b"FNPMCACHEPACKv1\n".to_vec();
    // The package claims an empty body, but its integrity record follows anyway.
    pack_record(&mut pack, 1, &a.dir_name(), 0, 0);
    pack_record(&mut pack, 6, "", 0, sri.len() as u64);
    pack.extend_from_slice(sri.as_bytes());
    pack_record(&mut pack, 0, "", 0, 0);
    let p = t.path().join("short.pack");
    std::fs::write(&p, &pack).unwrap();
    // The package is already cached, which used to skip `end - pos` bytes and underflow.
    for dry_run in [true, false] {
        let err = crate::cache_pack::unpack(&store, &p, dry_run).unwrap_err();
        assert!(matches!(err, Error::Refused { .. }), "{err}");
    }
}

#[test]
fn cache_unpack_dry_run_writes_nothing() {
    let (t, store) = store();
    let (a, _) = import_fixture(&store, "left-pad", "1.3.0");
    let pack_path = t.path().join("cache.pack");
    crate::cache_pack::pack(&store, std::slice::from_ref(&a), &pack_path, false).unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let other = Store::open_with_tuning(other_dir.path(), Tuning::default()).unwrap();
    let dry = crate::cache_pack::unpack(&other, &pack_path, true).unwrap();
    assert_eq!((dry.packages, dry.created), (1, 1));
    assert!(dry.bytes > 0);
    assert_eq!(count_packages(&other), 0);
    assert!(tmp_is_empty(&other));
    assert!(other.get(&a).unwrap().is_none());
    let real = crate::cache_pack::unpack(&other, &pack_path, false).unwrap();
    assert_eq!(real.bytes, dry.bytes);
    assert_eq!(count_packages(&other), 1);
}

#[test]
fn cache_unpack_file_and_directory_with_one_name_is_refused() {
    let (t, store) = store();
    let tgz = fixture("left-pad-1.3.0.tgz");
    let key = key_for("left-pad", "1.3.0", &tgz);
    let sri = key.integrity.to_sri();
    for dry_run in [true, false] {
        let mut body = Vec::new();
        pack_record(&mut body, 6, "", 0, sri.len() as u64);
        body.extend_from_slice(sri.as_bytes());
        pack_record(&mut body, 2, "a", 0o644, 1);
        body.push(b'x');
        pack_record(&mut body, 2, "a/b", 0o644, 1);
        body.push(b'y');
        let name = key.dir_name();
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        hasher.update(name.as_bytes());
        hasher.update(&body);
        let mut sum = Vec::new();
        pack_record(&mut sum, 5, "", 0, 8);
        hasher.update(&sum);
        let digest = hasher.digest();
        body.extend_from_slice(&sum);
        body.extend_from_slice(&digest.to_le_bytes());
        let mut pack = b"FNPMCACHEPACKv1\n".to_vec();
        pack_record(&mut pack, 1, &name, 0, body.len() as u64);
        pack.extend_from_slice(&body);
        pack_record(&mut pack, 0, "", 0, 0);
        let p = t.path().join("conflict.pack");
        std::fs::write(&p, &pack).unwrap();
        let err = crate::cache_pack::unpack(&store, &p, dry_run).unwrap_err();
        assert!(matches!(err, Error::Refused { .. }), "{err}");
        assert!(store.get(&key).unwrap().is_none());
        assert!(tmp_is_empty(&store));
    }
}

#[test]
fn patch_paths_with_dot_components_still_apply() {
    let (_t, store) = store();
    let (key, base) = import_fixture(&store, "left-pad", "1.3.0");
    let original = std::fs::read_to_string(base.root.join("index.js")).unwrap();
    let first = original.lines().next().unwrap();
    let patch = format!(
        "diff --git a/./index.js b/./index.js\n--- a/./index.js\n+++ b/./index.js\n@@ -1,1 +1,1 @@\n-{first}\n+// patched\n"
    );
    let patched = store.import_patched(&key, &base, &patch, 7).unwrap();
    let text = std::fs::read_to_string(patched.entry.root.join("index.js")).unwrap();
    assert!(text.starts_with("// patched\n"), "{text}");
    // The base package is untouched.
    assert_eq!(
        std::fs::read_to_string(base.root.join("index.js")).unwrap(),
        original
    );
}

#[test]
fn streamed_duplicate_entries_last_one_wins_across_batches() {
    // The first `a.js` lands in an early write batch; the later entries come after many other
    // files, so they are written after it (and a large duplicate replaces the earlier one).
    let big1 = incompressible(9 * 1024 * 1024, 21);
    let big2 = incompressible(9 * 1024 * 1024, 22);
    let mut b = TarBuilder::new();
    b.file("package/package.json", b"{}");
    b.file("package/a.js", b"first");
    b.file("package/big.bin", &big1);
    for i in 0..100 {
        b.file(&format!("package/f{i}.js"), &incompressible(4000, i + 1));
    }
    b.file("package/a.js", b"second");
    b.file("package/big.bin", &big2);
    let tgz = b.end().tgz();
    let rt = runtime();
    let (_t, store) = store();
    let store = Store::open_with_tuning(
        store.root(),
        Tuning {
            streaming_min_size: 1,
            drain_threshold: 16 * 1024,
            ..Tuning::default()
        },
    )
    .unwrap();
    let key = key_for("dup", "1.0.0", &tgz);
    let entry = rt
        .block_on(store.import_tarball(&key, body(&tgz, 8192), ImportOptions::default()))
        .unwrap();
    assert_eq!(std::fs::read(entry.root.join("a.js")).unwrap(), b"second");
    assert_eq!(std::fs::read(entry.root.join("big.bin")).unwrap(), big2);
    assert_eq!(entry.index.files.len(), 103);
    assert_eq!(entry.index.file("a.js").unwrap().size, 6);
    // No temporary file of the large duplicate is left in the package.
    assert_eq!(all_files(&entry.root).len(), 103);
    assert!(tmp_is_empty(&store));
}

#[test]
fn concurrent_streamed_imports_of_one_key_publish_one_complete_package() {
    let shared = incompressible(300_000, 31);
    let tgz = duplicated_package("c", &shared);
    let key = key_for("c", "1.0.0", &tgz);
    let (_t, store) = store();
    let store = Store::open_with_tuning(
        store.root(),
        Tuning {
            streaming_min_size: 1,
            drain_threshold: 32 * 1024,
            ..Tuning::default()
        },
    )
    .unwrap();
    let roots: Vec<PathBuf> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let (store, key, tgz) = (store.clone(), key.clone(), tgz.clone());
                s.spawn(move || {
                    runtime()
                        .block_on(store.import_tarball(
                            &key,
                            body(&tgz, 4096),
                            ImportOptions::default(),
                        ))
                        .unwrap()
                        .root
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(roots.iter().all(|r| *r == roots[0]));
    let entry = store.get(&key).unwrap().unwrap();
    check_duplicated_package(&entry, &shared);
    assert_eq!(store.status().unwrap().packages, 1);
    assert!(tmp_is_empty(&store));
}

#[test]
fn an_index_of_the_previous_layout_is_not_trusted() {
    let (_t, store) = store();
    let (key, entry) = import_fixture(&store, "left-pad", "1.3.0");
    let index = store.package_dir(&key).join("index.bin");
    let mut bytes = std::fs::read(&index).unwrap();
    // Format 1 (with per-file BLAKE3 digests) had another magic.
    bytes[7] = 1;
    std::fs::write(&index, &bytes).unwrap();
    assert!(store.get(&key).unwrap().is_none());
    // A reimport replaces it.
    let tgz = fixture("left-pad-1.3.0.tgz");
    let again = store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    assert_eq!(*again.index, *entry.index);
    assert!(store.get(&key).unwrap().is_some());
}

#[test]
fn a_crashed_import_leaves_nothing_a_later_install_trusts() {
    let (_t, store) = store();
    let tgz = fixture("left-pad-1.3.0.tgz");
    let key = key_for("left-pad", "1.3.0", &tgz);
    // A crash after files were written but before `index.bin`: the package dir exists with
    // some files and no index.
    let dir = store.package_dir(&key);
    std::fs::create_dir_all(dir.join("package")).unwrap();
    std::fs::write(dir.join("package/index.js"), "truncat").unwrap();
    assert!(store.get(&key).unwrap().is_none());
    let entry = store
        .import_tarball_bytes(&key, &tgz, ImportOptions::default())
        .unwrap();
    assert_eq!(
        std::fs::metadata(entry.root.join("index.js"))
            .unwrap()
            .len(),
        1469
    );
    assert_eq!(paths(&entry).len(), 10);
}

#[cfg(unix)]
#[test]
fn streamed_replacement_does_not_share_the_wrong_executable_mode() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, store) = store();
    let mut tar = TarBuilder::new();
    tar.file_mode("package/a", b"shared", 0o644);
    tar.file_mode("package/a", b"shared", 0o755);
    tar.file_mode("package/b", b"shared", 0o644);
    tar.end();
    let staging = store.new_staging().unwrap();
    let mut extractor = crate::extract::StreamExtractor::new(staging.clone(), None);
    // Each replacement settles before the next header arrives, so the dedupe lookup
    // retains a candidate path whose mode has since changed.
    for chunk in tar.buf.chunks(1024) {
        extractor.feed(chunk).unwrap();
    }
    let (_, built) = extractor.finish().unwrap();
    assert_eq!(built.files.len(), 2);
    for (name, executable) in [("a", true), ("b", false)] {
        let path = staging.package_root().join(name);
        assert_eq!(std::fs::read(&path).unwrap(), b"shared");
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0,
            executable
        );
    }
}

#[cfg(unix)]
#[test]
fn concurrent_staging_writes_share_one_inode() {
    let (_t, store) = store();
    let staging = store.new_staging().unwrap();
    // Match the race between independent streaming write batches: every thread sees the
    // same previously unseen content, and the first write is long enough to overlap them.
    let data = incompressible(1024 * 1024, 47);
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for i in 0..8 {
            let (staging, data, barrier) = (&staging, &data, &barrier);
            scope.spawn(move || {
                barrier.wait();
                staging
                    .add_bytes(&format!("file-{i}"), 0o644, data)
                    .unwrap();
            });
        }
    });
    let first = staging.package_root().join("file-0");
    for i in 0..8 {
        let path = staging.package_root().join(format!("file-{i}"));
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(
            ino(&first),
            ino(&path),
            "file-{i} must share the first write"
        );
    }
}
