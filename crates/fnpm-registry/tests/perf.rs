//! Timing of the manifest hot paths on a synthetic 1 MB+ packument. Ignored by default:
//! `cargo test --release -p fnpm-registry --test perf -- --ignored --nocapture`.

use std::time::Instant;

// The napi addon runs with mimalloc; allocation-heavy paths are measured the same way.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use fnpm_registry::{ManifestCache, PackageManifest};

fn big_packument(versions: usize) -> String {
    let mut v = Vec::new();
    for i in 0..versions {
        let (major, minor, patch) = (i / 100, (i / 10) % 10, i % 10);
        // Dependencies change every 20 versions, like real packages.
        let generation = i / 20;
        v.push(format!(
            r#""{major}.{minor}.{patch}":{{"name":"big","version":"{major}.{minor}.{patch}","dependencies":{{"dep-a":"^{generation}.0.0","dep-b":"~1.2.{generation}","dep-c":"^3.0.0","dep-d":"^4.1.0","dep-e":"5.x"}},"peerDependencies":{{"react":">=16"}},"bin":{{"big":"bin/big.js"}},"engines":{{"node":">=14"}},"dist":{{"tarball":"https://registry.npmjs.org/big/-/big-{major}.{minor}.{patch}.tgz","integrity":"sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg==","shasum":"da39a3ee5e6b4b0d3255bfef95601890afd80709","fileCount":12,"unpackedSize":45678}}}}"#
        ));
    }
    format!(
        r#"{{"name":"big","dist-tags":{{"latest":"9.9.9"}},"modified":"2024-01-01T00:00:00.000Z","versions":{{{}}}}}"#,
        v.join(",")
    )
}

fn time<T>(label: &str, iters: u32, mut f: impl FnMut() -> T) -> T {
    let mut out = f();
    let start = Instant::now();
    for _ in 0..iters {
        out = f();
    }
    let per = start.elapsed() / iters;
    eprintln!("{label}: {per:?}");
    out
}

#[test]
#[ignore]
fn manifest_hot_paths() {
    let json = big_packument(1000);
    eprintln!("packument: {} KiB, 1000 versions", json.len() / 1024);
    time("baseline: sonic_rs::Value", 20, || {
        sonic_rs::from_slice::<sonic_rs::Value>(json.as_bytes()).unwrap()
    });
    time("baseline: serde_json::Value", 20, || {
        serde_json::from_slice::<serde_json::Value>(json.as_bytes()).unwrap()
    });
    time("baseline: sonic_rs IgnoredAny", 20, || {
        sonic_rs::from_slice::<serde::de::IgnoredAny>(json.as_bytes()).unwrap()
    });
    let m = time("parse (sonic-rs)", 20, || {
        PackageManifest::parse("big", json.as_bytes()).unwrap()
    });
    assert_eq!(m.versions.len(), 1000);
    let tmp = tempfile::tempdir().unwrap();
    let cache = ManifestCache::for_cache_dir(tmp.path());
    time("store (rkyv + atomic write)", 20, || {
        cache.store("https://registry.npmjs.org/", &m).unwrap()
    });
    let size = std::fs::metadata(cache.path_for("https://registry.npmjs.org/", "big"))
        .unwrap()
        .len();
    eprintln!("cache entry: {} KiB", size / 1024);
    let loaded = time("load (read + validate + materialise)", 50, || {
        cache.load("https://registry.npmjs.org/", "big").unwrap()
    });
    assert_eq!(*loaded, m);
}
