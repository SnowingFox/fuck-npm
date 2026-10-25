//! On-disk manifest cache: `<cache_dir>/metadata/v1/<xxh3-64 hex of registry url + "\0" + name>.rkyv`.
//!
//! The file is a 16-byte header followed by an rkyv archive of a flat record. Dependency lists
//! that several versions share are stored once (bun's `ExternalStringMapDeduper`), so a package
//! with 2000 versions that rarely change their dependencies costs little more than its version
//! list. Loading is one read plus validation, with no JSON parsing. Writes go to a temporary file
//! in the same directory and are renamed into place, so a reader never sees half a file.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fnpm_semver::{Identifier, Version};
use fnpm_types::{
    Bin, CpuSet, HashAlgorithm, Integrity, LibcSet, OsSet, PeerMeta, PlatformConstraints,
};
use rkyv::rancor;
use rkyv::util::AlignedVec;

use crate::Result;
use crate::manifest::{Dist, PackageManifest, VersionManifest};

/// File magic plus format version. Bump the last bytes when [`Record`] changes.
const MAGIC: &[u8; 16] = b"fnpm-manifest\x00\x01\x00";

/// The manifest cache of one cache directory.
#[derive(Debug, Clone)]
pub struct ManifestCache {
    dir: PathBuf,
}

#[derive(rkyv::Archive, rkyv::Serialize)]
struct Record {
    registry: String,
    name: String,
    dist_tags: Vec<(String, String)>,
    time: Vec<(String, u64)>,
    modified: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    fetched_at_ms: u64,
    max_age_secs: u32,
    is_full: bool,
    /// Shared dependency lists; index 0 is the empty list.
    lists: Vec<Vec<(String, String)>>,
    versions: Vec<RecordVersion>,
}

#[derive(rkyv::Archive, rkyv::Serialize)]
enum RecordIdent {
    Num(u64),
    Alpha(String),
}

#[derive(rkyv::Archive, rkyv::Serialize)]
enum RecordBin {
    None,
    File(String),
    Named(String, String),
    Map(Vec<(String, String)>),
    Dir(String),
}

#[derive(rkyv::Archive, rkyv::Serialize)]
struct RecordVersion {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<RecordIdent>,
    build: String,
    dependencies: u32,
    optional_dependencies: u32,
    peer_dependencies: u32,
    peer_meta: Vec<(String, bool)>,
    bundle: Vec<String>,
    bin: RecordBin,
    os: u16,
    cpu: u16,
    libc: u8,
    tarball: String,
    /// Algorithm (0..=3 = sha1, sha256, sha384, sha512) and digest.
    integrity: Option<(u8, Vec<u8>)>,
    file_count: Option<u32>,
    unpacked_size: Option<u64>,
    has_install_script: bool,
    deprecated: Option<String>,
    engines: Vec<(String, String)>,
    scripts: Vec<(String, String)>,
    license: Option<String>,
}

fn algorithm_code(a: HashAlgorithm) -> u8 {
    match a {
        HashAlgorithm::Sha1 => 0,
        HashAlgorithm::Sha256 => 1,
        HashAlgorithm::Sha384 => 2,
        HashAlgorithm::Sha512 => 3,
    }
}

fn algorithm_from_code(c: u8) -> Option<HashAlgorithm> {
    Some(match c {
        0 => HashAlgorithm::Sha1,
        1 => HashAlgorithm::Sha256,
        2 => HashAlgorithm::Sha384,
        3 => HashAlgorithm::Sha512,
        _ => return None,
    })
}

fn to_record(registry: &str, m: &PackageManifest) -> Record {
    let mut lists: Vec<Vec<(String, String)>> = vec![Vec::new()];
    let mut seen: HashMap<&[(String, String)], u32> = HashMap::new();
    seen.insert(&[], 0);
    let mut versions = Vec::with_capacity(m.versions.len());
    let mut slots: Vec<[u32; 3]> = Vec::with_capacity(m.versions.len());
    for v in &m.versions {
        let mut slot = [0u32; 3];
        for (i, list) in [
            &v.dependencies,
            &v.optional_dependencies,
            &v.peer_dependencies,
        ]
        .into_iter()
        .enumerate()
        {
            slot[i] = *seen.entry(list.as_slice()).or_insert_with(|| {
                lists.push(list.clone());
                (lists.len() - 1) as u32
            });
        }
        slots.push(slot);
    }
    for (v, slot) in m.versions.iter().zip(slots) {
        versions.push(RecordVersion {
            major: v.version.major,
            minor: v.version.minor,
            patch: v.version.patch,
            pre: v
                .version
                .pre
                .iter()
                .map(|p| match p {
                    Identifier::Numeric(n) => RecordIdent::Num(*n),
                    Identifier::AlphaNumeric(s) => RecordIdent::Alpha(s.to_string()),
                })
                .collect(),
            build: v.version.build.to_string(),
            dependencies: slot[0],
            optional_dependencies: slot[1],
            peer_dependencies: slot[2],
            peer_meta: v
                .peer_dependencies_meta
                .iter()
                .map(|(k, m)| (k.clone(), m.optional))
                .collect(),
            bundle: v.bundle_dependencies.clone(),
            bin: match &v.bin {
                Bin::None => RecordBin::None,
                Bin::File { path } => RecordBin::File(path.clone()),
                Bin::NamedFile { name, path } => RecordBin::Named(name.clone(), path.clone()),
                Bin::Map { entries } => RecordBin::Map(entries.clone()),
                Bin::Dir { path } => RecordBin::Dir(path.clone()),
            },
            os: v.platform.os.0,
            cpu: v.platform.cpu.0,
            libc: v.platform.libc.0,
            tarball: v.dist.tarball.clone(),
            integrity: v
                .dist
                .integrity
                .map(|i| (algorithm_code(i.algorithm()), i.digest().to_vec())),
            file_count: v.dist.file_count,
            unpacked_size: v.dist.unpacked_size,
            has_install_script: v.has_install_script,
            deprecated: v.deprecated.clone(),
            engines: v
                .engines
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            scripts: v
                .scripts
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            license: v.license.clone(),
        });
    }
    Record {
        registry: registry.to_owned(),
        name: m.name.clone(),
        dist_tags: m
            .dist_tags
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        time: m.time.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        modified: m.modified.clone(),
        etag: m.etag.clone(),
        last_modified: m.last_modified.clone(),
        fetched_at_ms: m.fetched_at_ms,
        max_age_secs: m.max_age_secs,
        is_full: m.is_full,
        lists,
        versions,
    }
}

fn pairs(
    list: &rkyv::vec::ArchivedVec<
        rkyv::tuple::ArchivedTuple2<rkyv::string::ArchivedString, rkyv::string::ArchivedString>,
    >,
) -> Vec<(String, String)> {
    list.iter()
        .map(|p| (p.0.as_str().to_owned(), p.1.as_str().to_owned()))
        .collect()
}

fn opt_string(s: &rkyv::option::ArchivedOption<rkyv::string::ArchivedString>) -> Option<String> {
    s.as_ref().map(|s| s.as_str().to_owned())
}

/// Builds the manifest straight from the archive: every string is allocated once, in its
/// final place.
fn from_archived(r: &ArchivedRecord) -> Option<PackageManifest> {
    let list = |i: u32| r.lists.get(i as usize).map(pairs);
    let mut versions = Vec::with_capacity(r.versions.len());
    for v in r.versions.iter() {
        let mut version = Version::new(
            v.major.to_native(),
            v.minor.to_native(),
            v.patch.to_native(),
        );
        if !v.pre.is_empty() {
            version.pre = v
                .pre
                .iter()
                .map(|p| match p {
                    ArchivedRecordIdent::Num(n) => Identifier::Numeric(n.to_native()),
                    ArchivedRecordIdent::Alpha(s) => Identifier::AlphaNumeric(s.as_str().into()),
                })
                .collect();
        }
        if !v.build.is_empty() {
            version.build = v.build.as_str().into();
        }
        let integrity = match v.integrity.as_ref() {
            Some(t) => Some(Integrity::from_digest(algorithm_from_code(t.0)?, &t.1).ok()?),
            None => None,
        };
        versions.push(VersionManifest {
            version,
            dependencies: list(v.dependencies.to_native())?,
            optional_dependencies: list(v.optional_dependencies.to_native())?,
            peer_dependencies: list(v.peer_dependencies.to_native())?,
            peer_dependencies_meta: v
                .peer_meta
                .iter()
                .map(|p| (p.0.as_str().to_owned(), PeerMeta { optional: p.1 }))
                .collect(),
            bundle_dependencies: v.bundle.iter().map(|s| s.as_str().to_owned()).collect(),
            bin: match &v.bin {
                ArchivedRecordBin::None => Bin::None,
                ArchivedRecordBin::File(path) => Bin::File {
                    path: path.as_str().to_owned(),
                },
                ArchivedRecordBin::Named(name, path) => Bin::NamedFile {
                    name: name.as_str().to_owned(),
                    path: path.as_str().to_owned(),
                },
                ArchivedRecordBin::Map(entries) => Bin::Map {
                    entries: pairs(entries),
                },
                ArchivedRecordBin::Dir(path) => Bin::Dir {
                    path: path.as_str().to_owned(),
                },
            },
            platform: PlatformConstraints {
                os: OsSet(v.os.to_native()),
                cpu: CpuSet(v.cpu.to_native()),
                libc: LibcSet(v.libc),
            },
            dist: Dist {
                tarball: v.tarball.as_str().to_owned(),
                integrity,
                file_count: v.file_count.as_ref().map(|n| n.to_native()),
                unpacked_size: v.unpacked_size.as_ref().map(|n| n.to_native()),
            },
            has_install_script: v.has_install_script,
            deprecated: opt_string(&v.deprecated),
            engines: pairs(&v.engines).into_iter().collect(),
            scripts: pairs(&v.scripts).into_iter().collect(),
            license: opt_string(&v.license),
        });
    }
    Some(PackageManifest {
        name: r.name.as_str().to_owned(),
        dist_tags: pairs(&r.dist_tags).into_iter().collect::<BTreeMap<_, _>>(),
        versions,
        time: r
            .time
            .iter()
            .map(|t| (t.0.as_str().to_owned(), t.1.to_native()))
            .collect(),
        modified: opt_string(&r.modified),
        etag: opt_string(&r.etag),
        last_modified: opt_string(&r.last_modified),
        fetched_at_ms: r.fetched_at_ms.to_native(),
        max_age_secs: r.max_age_secs.to_native(),
        is_full: r.is_full,
    })
}

/// Serialises a manifest into the cache file format (header + archive).
pub(crate) fn encode(registry_url: &str, manifest: &PackageManifest) -> Result<AlignedVec> {
    let record = to_record(registry_url, manifest);
    let mut out = AlignedVec::<16>::new();
    out.extend_from_slice(MAGIC);
    let archived =
        rkyv::to_bytes::<rancor::Error>(&record).map_err(|e| io::Error::other(e.to_string()))?;
    out.extend_from_slice(&archived);
    Ok(out)
}

/// Reads a manifest back; `None` for a wrong header, a corrupt archive, or another registry.
pub(crate) fn decode(registry_url: &str, name: &str, bytes: &[u8]) -> Option<PackageManifest> {
    let payload = bytes.strip_prefix(MAGIC.as_slice())?;
    let check = |bytes: &[u8]| -> Option<PackageManifest> {
        let record = rkyv::access::<ArchivedRecord, rancor::Error>(bytes).ok()?;
        if record.registry.as_str() != registry_url || record.name.as_str() != name {
            return None;
        }
        from_archived(record)
    };
    // `read_aligned` hands over a 16-aligned buffer, so the payload after the 16-byte header is
    // aligned too; copy only for bytes from elsewhere.
    if payload.as_ptr().align_offset(16) == 0 {
        check(payload)
    } else {
        let mut aligned = AlignedVec::<16>::with_capacity(payload.len());
        aligned.extend_from_slice(payload);
        check(&aligned)
    }
}

fn read_aligned(path: &Path) -> io::Result<AlignedVec<16>> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    let mut buf = AlignedVec::<16>::with_capacity(len);
    buf.resize(len, 0);
    file.read_exact(&mut buf)?;
    Ok(buf)
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ManifestCache {
    /// The cache under `dir` (the `metadata/v1` directory itself). Nothing is created until the
    /// first store.
    pub fn open(dir: &Path) -> Result<Self> {
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// `<cache_dir>/metadata/v1` for a configured cache directory.
    pub fn for_cache_dir(cache_dir: &Path) -> Self {
        Self {
            dir: cache_dir.join("metadata").join("v1"),
        }
    }

    /// The directory the entries live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The entry path of a package on a registry.
    pub fn path_for(&self, registry_url: &str, name: &str) -> PathBuf {
        let mut h = xxhash_rust::xxh3::Xxh3::new();
        h.update(registry_url.as_bytes());
        h.update(b"\0");
        h.update(name.as_bytes());
        self.dir.join(format!("{:016x}.rkyv", h.digest()))
    }

    /// The cached manifest, regardless of age. A corrupt entry is deleted.
    pub fn load(&self, registry_url: &str, name: &str) -> Option<Arc<PackageManifest>> {
        let path = self.path_for(registry_url, name);
        let bytes = match read_aligned(&path) {
            Ok(bytes) => bytes,
            Err(_) => return None,
        };
        match decode(registry_url, name, &bytes) {
            Some(m) => Some(Arc::new(m)),
            None => {
                // Another registry hashing to the same name would be astronomically unlikely;
                // anything else that fails is unusable and is refetched.
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    /// Writes an entry atomically. Off the hot path: called from a blocking task.
    pub fn store(&self, registry_url: &str, manifest: &PackageManifest) -> Result<()> {
        let bytes = encode(registry_url, manifest)?;
        let path = self.path_for(registry_url, &manifest.name);
        let tmp = self.dir.join(format!(
            ".tmp-{}-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
            path.file_name().and_then(|n| n.to_str()).unwrap_or("entry"),
        ));
        let write = |tmp: &Path| -> io::Result<()> {
            let mut file = fs::File::create(tmp)?;
            file.write_all(&bytes)?;
            drop(file);
            fs::rename(tmp, &path)
        };
        match write(&tmp) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.dir)?;
                write(&tmp).inspect_err(|_| {
                    let _ = fs::remove_file(&tmp);
                })?;
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.into())
            }
        }
    }

    /// `fnpm cache clean`: removes every entry (and leftover temporary files); returns bytes freed.
    pub fn clear(&self) -> Result<u64> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut freed = 0;
        for entry in entries {
            let entry = entry?;
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".rkyv") || name.starts_with(".tmp-") {
                match fs::remove_file(entry.path()) {
                    Ok(()) => freed += meta.len(),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(freed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PackageManifest {
        let json = br#"{
            "name": "pkg",
            "modified": "2024-01-01T00:00:00.000Z",
            "dist-tags": { "latest": "1.1.0", "next": "2.0.0-rc.1+b" },
            "time": { "1.0.0": "2020-01-01T00:00:00.000Z", "created": "2019-01-01T00:00:00.000Z" },
            "versions": {
                "1.0.0": { "dependencies": { "a": "^1", "b": "^2" }, "dist": { "tarball": "https://r/pkg/-/pkg-1.0.0.tgz", "integrity": "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg==" } },
                "1.1.0": { "dependencies": { "a": "^1", "b": "^2" }, "peerDependencies": { "p": "*" }, "peerDependenciesMeta": { "p": { "optional": true } }, "bin": { "x": "x.js", "y": "y.js" }, "os": ["!win32"], "deprecated": "old" },
                "2.0.0-rc.1+b": { "optionalDependencies": { "o": "1" }, "directories": { "bin": "./bin" }, "dist": { "shasum": "0123456789abcdef0123456789abcdef01234567" }, "hasInstallScript": true, "engines": { "node": ">=18" }, "scripts": { "install": "x" }, "license": "MIT" }
            }
        }"#;
        let mut m = PackageManifest::parse("pkg", json).unwrap();
        m.etag = Some("\"abc\"".into());
        m.last_modified = Some("Mon, 01 Jan 2024 00:00:00 GMT".into());
        m.fetched_at_ms = 123;
        m.max_age_secs = 300;
        m.is_full = true;
        m
    }

    #[test]
    fn round_trips_every_field() {
        let m = manifest();
        let bytes = encode("https://r/", &m).unwrap();
        let back = decode("https://r/", "pkg", &bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn shares_identical_dependency_lists() {
        let m = manifest();
        let record = to_record("https://r/", &m);
        // empty, {a,b}, {p}, {o}
        assert_eq!(record.lists.len(), 4);
        assert_eq!(
            record.versions[0].dependencies,
            record.versions[1].dependencies
        );
    }

    #[test]
    fn rejects_other_registry_name_and_garbage() {
        let m = manifest();
        let bytes = encode("https://r/", &m).unwrap();
        assert!(decode("https://other/", "pkg", &bytes).is_none());
        assert!(decode("https://r/", "other", &bytes).is_none());
        assert!(decode("https://r/", "pkg", b"garbage").is_none());
        let mut truncated = bytes.to_vec();
        truncated.truncate(bytes.len() / 2);
        assert!(decode("https://r/", "pkg", &truncated).is_none());
    }

    #[test]
    fn store_load_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ManifestCache::for_cache_dir(tmp.path());
        assert!(cache.load("https://r/", "pkg").is_none());
        assert_eq!(cache.clear().unwrap(), 0);
        let m = manifest();
        cache.store("https://r/", &m).unwrap();
        let path = cache.path_for("https://r/", "pkg");
        assert!(path.starts_with(tmp.path().join("metadata").join("v1")));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 16 + ".rkyv".len());
        assert_eq!(*cache.load("https://r/", "pkg").unwrap(), m);
        assert_ne!(
            cache.path_for("https://r/", "pkg"),
            cache.path_for("https://s/", "pkg")
        );
        // Overwrite in place.
        let mut m2 = m.clone();
        m2.fetched_at_ms = 999;
        cache.store("https://r/", &m2).unwrap();
        assert_eq!(cache.load("https://r/", "pkg").unwrap().fetched_at_ms, 999);
        let size = fs::metadata(&path).unwrap().len();
        assert_eq!(cache.clear().unwrap(), size);
        assert!(cache.load("https://r/", "pkg").is_none());
    }

    #[test]
    fn corrupt_entry_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ManifestCache::for_cache_dir(tmp.path());
        cache.store("https://r/", &manifest()).unwrap();
        let path = cache.path_for("https://r/", "pkg");
        let mut bytes = fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes.truncate(mid);
        fs::write(&path, &bytes).unwrap();
        assert!(cache.load("https://r/", "pkg").is_none());
        assert!(!path.exists());
    }
}
