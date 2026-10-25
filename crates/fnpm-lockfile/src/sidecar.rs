//! `node_modules/.fnpm/lock.bin`: an rkyv image of the [`LockGraph`] keyed by the lockfile's
//! xxh3 hash, so a warm install skips JSONC parsing and re-binding.
//!
//! The archived type is a private mirror of `LockGraph` owned by this module; `fnpm-types`
//! stays serde-only. File layout: 8-byte magic, `u32` image version, `u32` reserved, `u64`
//! lockfile hash (all little-endian), then the archive. Anything unexpected is a miss.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use fnpm_types::{
    Behavior, Bin, BundledDependencies, Catalogs, ConfigVersion, CpuSet, DepEdge, HashAlgorithm,
    Integrity, LibcSet, LockGraph, NodeLinker, OsSet, OverrideRule, OverrideSelector, Package,
    PackageExtension, PackageId, PackageMeta, PackageSelector, PatchedDependency, PeerMeta,
    PlatformConstraints, Resolution, ScriptFlags,
};
use rkyv::{Archive, Deserialize, Serialize};

use crate::{Error, Result};

/// The sidecar's path relative to the project directory.
pub const SIDECAR: &str = "node_modules/.fnpm/lock.bin";

const MAGIC: &[u8; 8] = b"FNPMLCK\0";
/// Bump when mirror types or cached binding semantics change.
const IMAGE_VERSION: u32 = 2;
const HEADER: usize = 24;

#[derive(Archive, Serialize, Deserialize)]
struct Image {
    lockfile_version: u32,
    config_version: u8,
    /// 0 none, 1 hoisted, 2 isolated.
    node_linker: u8,
    packages: Vec<Pkg>,
    workspaces: Vec<(String, u32)>,
    catalog: Vec<(String, String)>,
    catalogs: Vec<(String, Vec<(String, String)>)>,
    overrides: Vec<Ovr>,
    patched: Vec<(String, String, Option<String>)>,
    trusted: Option<Vec<String>>,
    extensions: Vec<(String, Ext)>,
}

#[derive(Archive, Serialize, Deserialize)]
struct Ovr {
    parent: Option<(String, String)>,
    target: (String, String),
    value: String,
}

#[derive(Archive, Serialize, Deserialize)]
struct Ext {
    dependencies: Vec<(String, String)>,
    optional_dependencies: Vec<(String, String)>,
    peer_dependencies: Vec<(String, String)>,
    optional_peers: Vec<(String, bool)>,
}

#[derive(Archive, Serialize, Deserialize)]
struct Integ {
    algorithm: u8,
    digest: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize)]
enum Res {
    Root,
    Npm {
        version: String,
        tarball_url: Option<String>,
        integrity: Option<Integ>,
    },
    Git {
        url: String,
        committish: String,
        resolved_commit: String,
        integrity: Option<Integ>,
    },
    Github {
        owner: String,
        repo: String,
        committish: String,
        resolved_commit: String,
        integrity: Option<Integ>,
    },
    RemoteTarball {
        url: String,
        integrity: Option<Integ>,
    },
    LocalTarball {
        path: String,
        integrity: Option<Integ>,
    },
    Folder(String),
    Symlink(String),
    Workspace(String),
}

#[derive(Archive, Serialize, Deserialize)]
enum BinI {
    None,
    File(String),
    NamedFile(String, String),
    Map(Vec<(String, String)>),
    Dir(String),
}

#[derive(Archive, Serialize, Deserialize)]
struct Edge {
    name: String,
    literal: String,
    behavior: u8,
    /// `u32::MAX` for unresolved.
    target: u32,
}

#[derive(Archive, Serialize, Deserialize)]
struct Pkg {
    name: String,
    version: String,
    resolution: Res,
    dependencies: Vec<Edge>,
    peer_meta: Vec<(String, bool)>,
    bin: BinI,
    os: u16,
    cpu: u16,
    libc: u8,
    scripts: u8,
    has_install_script: bool,
    deprecated: Option<String>,
    engines: Vec<(String, String)>,
    /// 0 none, 1 all(false), 2 all(true), 3 list.
    bundled_kind: u8,
    bundled_list: Vec<String>,
    bundled: bool,
    license: Option<String>,
    man: Vec<String>,
}

fn alg_to(a: HashAlgorithm) -> u8 {
    match a {
        HashAlgorithm::Sha1 => 1,
        HashAlgorithm::Sha256 => 2,
        HashAlgorithm::Sha384 => 3,
        HashAlgorithm::Sha512 => 4,
    }
}

fn alg_from(a: u8) -> Option<HashAlgorithm> {
    Some(match a {
        1 => HashAlgorithm::Sha1,
        2 => HashAlgorithm::Sha256,
        3 => HashAlgorithm::Sha384,
        4 => HashAlgorithm::Sha512,
        _ => return None,
    })
}

fn integ_to(i: &Option<Integrity>) -> Option<Integ> {
    i.as_ref().map(|i| Integ {
        algorithm: alg_to(i.algorithm()),
        digest: i.digest().to_vec(),
    })
}

fn integ_from(i: Option<Integ>) -> Option<Option<Integrity>> {
    match i {
        None => Some(None),
        Some(i) => Some(Some(
            Integrity::from_digest(alg_from(i.algorithm)?, &i.digest).ok()?,
        )),
    }
}

fn pairs(m: &BTreeMap<String, String>) -> Vec<(String, String)> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn to_image(g: &LockGraph) -> Image {
    let packages = g
        .packages
        .iter()
        .map(|p| {
            let resolution = match &p.resolution {
                Resolution::Root => Res::Root,
                Resolution::Npm {
                    version,
                    tarball_url,
                    integrity,
                } => Res::Npm {
                    version: version.clone(),
                    tarball_url: tarball_url.clone(),
                    integrity: integ_to(integrity),
                },
                Resolution::Git {
                    url,
                    committish,
                    resolved_commit,
                    integrity,
                } => Res::Git {
                    url: url.clone(),
                    committish: committish.clone(),
                    resolved_commit: resolved_commit.clone(),
                    integrity: integ_to(integrity),
                },
                Resolution::Github {
                    owner,
                    repo,
                    committish,
                    resolved_commit,
                    integrity,
                } => Res::Github {
                    owner: owner.clone(),
                    repo: repo.clone(),
                    committish: committish.clone(),
                    resolved_commit: resolved_commit.clone(),
                    integrity: integ_to(integrity),
                },
                Resolution::RemoteTarball { url, integrity } => Res::RemoteTarball {
                    url: url.clone(),
                    integrity: integ_to(integrity),
                },
                Resolution::LocalTarball { path, integrity } => Res::LocalTarball {
                    path: path.clone(),
                    integrity: integ_to(integrity),
                },
                Resolution::Folder { path } => Res::Folder(path.clone()),
                Resolution::Symlink { path } => Res::Symlink(path.clone()),
                Resolution::Workspace { path } => Res::Workspace(path.clone()),
            };
            let bin = match &p.bin {
                Bin::None => BinI::None,
                Bin::File { path } => BinI::File(path.clone()),
                Bin::NamedFile { name, path } => BinI::NamedFile(name.clone(), path.clone()),
                Bin::Map { entries } => BinI::Map(entries.clone()),
                Bin::Dir { path } => BinI::Dir(path.clone()),
            };
            let (bundled_kind, bundled_list) = match &p.meta.bundled_dependencies {
                BundledDependencies::None => (0, Vec::new()),
                BundledDependencies::All(false) => (1, Vec::new()),
                BundledDependencies::All(true) => (2, Vec::new()),
                BundledDependencies::List(l) => (3, l.clone()),
            };
            Pkg {
                name: p.name.clone(),
                version: p.version.clone(),
                resolution,
                dependencies: p
                    .dependencies
                    .iter()
                    .map(|e| Edge {
                        name: e.name.clone(),
                        literal: e.literal.clone(),
                        behavior: e.behavior.bits(),
                        target: e.target.map_or(u32::MAX, |t| t.0),
                    })
                    .collect(),
                peer_meta: p
                    .peer_meta
                    .iter()
                    .map(|(k, v)| (k.clone(), v.optional))
                    .collect(),
                bin,
                os: p.platform.os.0,
                cpu: p.platform.cpu.0,
                libc: p.platform.libc.0,
                scripts: p.meta.scripts.0,
                has_install_script: p.meta.has_install_script,
                deprecated: p.meta.deprecated.clone(),
                engines: pairs(&p.meta.engines),
                bundled_kind,
                bundled_list,
                bundled: p.meta.bundled,
                license: p.meta.license.clone(),
                man: p.meta.man.clone(),
            }
        })
        .collect();
    Image {
        lockfile_version: g.lockfile_version,
        config_version: u8::from(g.config_version),
        node_linker: match g.node_linker {
            None => 0,
            Some(NodeLinker::Hoisted) => 1,
            Some(NodeLinker::Isolated) => 2,
        },
        packages,
        workspaces: g.workspaces.iter().map(|(k, v)| (k.clone(), v.0)).collect(),
        catalog: pairs(&g.catalogs.default),
        catalogs: g
            .catalogs
            .named
            .iter()
            .map(|(k, v)| (k.clone(), pairs(v)))
            .collect(),
        overrides: g
            .overrides
            .iter()
            .map(|r| Ovr {
                parent: r
                    .selector
                    .parent
                    .as_ref()
                    .map(|p| (p.name.clone(), p.range.clone())),
                target: (
                    r.selector.target.name.clone(),
                    r.selector.target.range.clone(),
                ),
                value: r.value.clone(),
            })
            .collect(),
        patched: g
            .patched_dependencies
            .iter()
            .map(|(k, v)| (k.clone(), v.path.clone(), v.hash.clone()))
            .collect(),
        trusted: g
            .trusted_dependencies
            .as_ref()
            .map(|t| t.iter().cloned().collect()),
        extensions: g
            .package_extensions
            .iter()
            .map(|(k, e)| {
                (
                    k.clone(),
                    Ext {
                        dependencies: pairs(&e.dependencies),
                        optional_dependencies: pairs(&e.optional_dependencies),
                        peer_dependencies: pairs(&e.peer_dependencies),
                        optional_peers: e
                            .peer_dependencies_meta
                            .iter()
                            .map(|(k, m)| (k.clone(), m.optional))
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}

fn from_image(i: Image) -> Option<LockGraph> {
    let n = i.packages.len() as u32;
    let mut packages = Vec::with_capacity(i.packages.len());
    for p in i.packages {
        let resolution = match p.resolution {
            Res::Root => Resolution::Root,
            Res::Npm {
                version,
                tarball_url,
                integrity,
            } => Resolution::Npm {
                version,
                tarball_url,
                integrity: integ_from(integrity)?,
            },
            Res::Git {
                url,
                committish,
                resolved_commit,
                integrity,
            } => Resolution::Git {
                url,
                committish,
                resolved_commit,
                integrity: integ_from(integrity)?,
            },
            Res::Github {
                owner,
                repo,
                committish,
                resolved_commit,
                integrity,
            } => Resolution::Github {
                owner,
                repo,
                committish,
                resolved_commit,
                integrity: integ_from(integrity)?,
            },
            Res::RemoteTarball { url, integrity } => Resolution::RemoteTarball {
                url,
                integrity: integ_from(integrity)?,
            },
            Res::LocalTarball { path, integrity } => Resolution::LocalTarball {
                path,
                integrity: integ_from(integrity)?,
            },
            Res::Folder(path) => Resolution::Folder { path },
            Res::Symlink(path) => Resolution::Symlink { path },
            Res::Workspace(path) => Resolution::Workspace { path },
        };
        let mut dependencies = Vec::with_capacity(p.dependencies.len());
        for e in p.dependencies {
            let target = match e.target {
                u32::MAX => None,
                t if t < n => Some(PackageId(t)),
                _ => return None,
            };
            dependencies.push(DepEdge {
                name: e.name,
                literal: e.literal,
                behavior: Behavior(e.behavior),
                target,
            });
        }
        packages.push(Package {
            name: p.name,
            version: p.version,
            resolution,
            dependencies,
            peer_meta: p
                .peer_meta
                .into_iter()
                .map(|(k, o)| (k, PeerMeta { optional: o }))
                .collect(),
            bin: match p.bin {
                BinI::None => Bin::None,
                BinI::File(path) => Bin::File { path },
                BinI::NamedFile(name, path) => Bin::NamedFile { name, path },
                BinI::Map(entries) => Bin::Map { entries },
                BinI::Dir(path) => Bin::Dir { path },
            },
            platform: PlatformConstraints {
                os: OsSet(p.os),
                cpu: CpuSet(p.cpu),
                libc: LibcSet(p.libc),
            },
            meta: PackageMeta {
                scripts: ScriptFlags(p.scripts),
                has_install_script: p.has_install_script,
                deprecated: p.deprecated,
                engines: p.engines.into_iter().collect(),
                bundled_dependencies: match p.bundled_kind {
                    0 => BundledDependencies::None,
                    1 => BundledDependencies::All(false),
                    2 => BundledDependencies::All(true),
                    _ => BundledDependencies::List(p.bundled_list),
                },
                bundled: p.bundled,
                license: p.license,
                man: p.man,
            },
        });
    }
    if packages.is_empty() {
        return None;
    }
    let mut workspaces = BTreeMap::new();
    for (k, v) in i.workspaces {
        if v >= n {
            return None;
        }
        workspaces.insert(k, PackageId(v));
    }
    Some(LockGraph {
        lockfile_version: i.lockfile_version,
        config_version: ConfigVersion::try_from(i.config_version).ok()?,
        node_linker: match i.node_linker {
            1 => Some(NodeLinker::Hoisted),
            2 => Some(NodeLinker::Isolated),
            _ => None,
        },
        packages,
        workspaces,
        catalogs: Catalogs {
            default: i.catalog.into_iter().collect(),
            named: i
                .catalogs
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().collect()))
                .collect(),
        },
        overrides: i
            .overrides
            .into_iter()
            .map(|o| OverrideRule {
                selector: OverrideSelector {
                    parent: o
                        .parent
                        .map(|(name, range)| PackageSelector { name, range }),
                    target: PackageSelector {
                        name: o.target.0,
                        range: o.target.1,
                    },
                },
                value: o.value,
            })
            .collect(),
        patched_dependencies: i
            .patched
            .into_iter()
            .map(|(k, path, hash)| (k, PatchedDependency { path, hash }))
            .collect(),
        trusted_dependencies: i.trusted.map(|t| t.into_iter().collect::<BTreeSet<_>>()),
        package_extensions: i
            .extensions
            .into_iter()
            .map(|(k, e)| {
                (
                    k,
                    PackageExtension {
                        dependencies: e.dependencies.into_iter().collect(),
                        optional_dependencies: e.optional_dependencies.into_iter().collect(),
                        peer_dependencies: e.peer_dependencies.into_iter().collect(),
                        peer_dependencies_meta: e
                            .optional_peers
                            .into_iter()
                            .map(|(k, o)| (k, PeerMeta { optional: o }))
                            .collect(),
                    },
                )
            })
            .collect(),
    })
}

/// Encodes a sidecar image (header + archive).
pub fn encode(lock_hash: u64, graph: &LockGraph) -> Result<Vec<u8>> {
    let archive =
        rkyv::to_bytes::<rkyv::rancor::Error>(&to_image(graph)).map_err(|e| Error::Io {
            path: SIDECAR.into(),
            source: std::io::Error::other(e.to_string()),
        })?;
    let mut out = Vec::with_capacity(HEADER + archive.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&IMAGE_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&lock_hash.to_le_bytes());
    out.extend_from_slice(&archive);
    Ok(out)
}

/// Decodes a sidecar image; `None` unless the magic, version and key match and the archive
/// validates.
pub fn decode(bytes: &[u8], lock_hash: u64) -> Option<LockGraph> {
    if bytes.len() < HEADER || &bytes[..8] != MAGIC {
        return None;
    }
    if u32::from_le_bytes(bytes[8..12].try_into().ok()?) != IMAGE_VERSION {
        return None;
    }
    if u64::from_le_bytes(bytes[16..24].try_into().ok()?) != lock_hash {
        return None;
    }
    let mut aligned = rkyv::util::AlignedVec::<16>::with_capacity(bytes.len() - HEADER);
    aligned.extend_from_slice(&bytes[HEADER..]);
    let image = rkyv::from_bytes::<Image, rkyv::rancor::Error>(&aligned).ok()?;
    // `from_image` checks every id is in range; the image was written from a valid graph.
    from_image(image)
}

/// `Some` only when the stored key equals `lock_hash` and the format version matches.
pub fn load(project_dir: &Path, lock_hash: u64) -> Option<LockGraph> {
    let bytes = std::fs::read(project_dir.join(SIDECAR)).ok()?;
    decode(&bytes, lock_hash)
}

/// The sidecar exists with this key and image version (only the header is read).
pub fn is_fresh(project_dir: &Path, lock_hash: u64) -> bool {
    use std::io::Read as _;
    let mut header = [0u8; HEADER];
    std::fs::File::open(project_dir.join(SIDECAR))
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok()
        && &header[..8] == MAGIC
        && header[8..12] == IMAGE_VERSION.to_le_bytes()
        && header[16..24] == lock_hash.to_le_bytes()
}

/// Writes the sidecar atomically, creating `node_modules/.fnpm` if needed.
pub fn save(project_dir: &Path, lock_hash: u64, graph: &LockGraph) -> Result<()> {
    let path = project_dir.join(SIDECAR);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
    }
    let bytes = encode(lock_hash, graph)?;
    crate::write_atomic(&path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LockGraph {
        let mut g = LockGraph::new(Package::new("app", "", Resolution::Root));
        let i = Integrity::compute(HashAlgorithm::Sha512, b"x");
        let mut a = Package::new("a", "1.0.0", Resolution::npm("1.0.0", Some(i)));
        a.bin = Bin::Map {
            entries: vec![("x".into(), "x.js".into()), ("y".into(), "y.js".into())],
        };
        a.platform.os = OsSet::from_list(&["linux"]);
        a.meta.deprecated = Some("old".into());
        a.meta.bundled_dependencies = BundledDependencies::List(vec!["q".into()]);
        let a = g.push(a);
        let ws = g.add_workspace(Package::new(
            "lib",
            "0.1.0",
            Resolution::Workspace {
                path: "packages/lib".into(),
            },
        ));
        let mut e = DepEdge::new("a", "^1", Behavior::PROD);
        e.target = Some(a);
        let mut w = DepEdge::new("lib", "workspace:*", Behavior::PROD);
        w.target = Some(ws);
        g.root_mut().dependencies = vec![e, w, DepEdge::new("gone", "^1", Behavior::OPTIONAL)];
        g.push(Package::new(
            "g",
            "",
            Resolution::Github {
                owner: "o".into(),
                repo: "r".into(),
                committish: "abc".into(),
                resolved_commit: "o-r-abc".into(),
                integrity: None,
            },
        ));
        g.catalogs.default.insert("react".into(), "^18".into());
        g.catalogs
            .named
            .entry("old".into())
            .or_default()
            .insert("react".into(), "^17".into());
        g.overrides.push(OverrideRule {
            selector: OverrideSelector::parse("a>b").unwrap().unwrap(),
            value: "1".into(),
        });
        g.trusted_dependencies = Some(["a".to_owned()].into());
        g.patched_dependencies.insert(
            "a@1.0.0".into(),
            PatchedDependency {
                path: "p.patch".into(),
                hash: Some("ff".into()),
            },
        );
        g.package_extensions
            .insert("a@1.0.0".into(), PackageExtension::default());
        g.node_linker = Some(NodeLinker::Isolated);
        g
    }

    #[test]
    fn round_trip_and_key_check() {
        let g = sample();
        let bytes = encode(42, &g).unwrap();
        assert_eq!(decode(&bytes, 42).as_ref(), Some(&g));
        assert!(decode(&bytes, 43).is_none());
        let mut bad = bytes.clone();
        bad[8] = 9;
        assert!(decode(&bad, 42).is_none());
        assert!(decode(&bytes[..bytes.len() / 2], 42).is_none());
        assert!(decode(b"short", 42).is_none());
    }

    #[test]
    fn save_and_load_files() {
        let dir = tempfile::tempdir().unwrap();
        let g = sample();
        assert!(!is_fresh(dir.path(), 7));
        save(dir.path(), 7, &g).unwrap();
        assert!(is_fresh(dir.path(), 7) && !is_fresh(dir.path(), 8));
        assert_eq!(load(dir.path(), 7), Some(g));
        assert!(load(dir.path(), 8).is_none());
    }
}
