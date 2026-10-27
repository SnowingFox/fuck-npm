//! `bun.lockb`, bun's binary lockfile (a port of the reader in bun
//! `install/lockfile/bun.lockb.rs`, `Buffers.rs` and `Package.rs` `serializer::load`), and
//! the writer (`serializer::save`), for projects that keep bun.lockb.
//!
//! Layout (little-endian, 64-bit): a header, the format version (2 or 3), a meta hash, the
//! package table as one column per field (name, name hash, resolution, dependency slice,
//! resolution slice, meta, bin, scripts), six length-prefixed arrays (trees, hoisted
//! dependencies, resolutions, dependencies, external strings, string bytes) and optional
//! tagged sections (workspace versions, trusted dependencies, overrides, patches, catalogs,
//! configVersion, scoped overrides, package extensions). Strings are bun's 8-byte
//! `semver.String`: inline when short, else an offset/length into the string bytes.
//!
//! Dependencies are bound by the stored resolution ids, so the reader does not need the
//! hoisted tree; the writer stores it for bun.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use fnpm_types::overrides::PackageSelector;
use fnpm_types::{
    Behavior, Bin, ConfigVersion, CpuSet, DepEdge, HashAlgorithm, Integrity, LibcSet, LockGraph,
    OsSet, OverrideRule, OverrideSelector, Package, PackageId, PatchedDependency,
    PlatformConstraints, Resolution, ScriptFlags,
};

use crate::migration::is_safe_resolved_tag;
use crate::{Error, Result};

const HEADER: &[u8] = b"#!/usr/bin/env bun\nbun-lockfile-format-v0\n";
const FORMAT_V2: u32 = 2;
const FORMAT_CURRENT: u32 = 3;

const fn tag(bytes: &[u8; 8]) -> u64 {
    u64::from_le_bytes(*bytes)
}
const HAS_PATCHED_DEPENDENCIES: u64 = tag(b"pAtChEdD");
const HAS_WORKSPACE_PACKAGE_IDS: u64 = tag(b"wOrKsPaC");
const HAS_TRUSTED_DEPENDENCIES: u64 = tag(b"tRuStEDd");
const HAS_EMPTY_TRUSTED_DEPENDENCIES: u64 = tag(b"eMpTrUsT");
const HAS_OVERRIDES: u64 = tag(b"oVeRriDs");
const HAS_CATALOGS: u64 = tag(b"cAtAlOgS");
const HAS_CONFIG_VERSION: u64 = tag(b"cNfGvRsN");
const HAS_SCOPED_OVERRIDES: u64 = tag(b"sCoPdOvR");
const HAS_PACKAGE_EXTENSIONS: u64 = tag(b"pKgExTnS");

const INVALID_ID: u32 = u32::MAX;
const DEPENDENCY_SIZE: usize = 26;
const META_SIZE: usize = 88;
const BIN_SIZE: usize = 20;
const SCRIPTS_SIZE: usize = 49;
const FIELD_COUNT: u64 = 8;

/// Resolution tags (bun `resolution::Tag`).
mod res_tag {
    pub const ROOT: u8 = 1;
    pub const NPM: u8 = 2;
    pub const FOLDER: u8 = 4;
    pub const LOCAL_TARBALL: u8 = 8;
    pub const GITHUB: u8 = 16;
    pub const GIT: u8 = 32;
    pub const SYMLINK: u8 = 64;
    pub const WORKSPACE: u8 = 72;
    pub const REMOTE_TARBALL: u8 = 80;
}

fn err(message: impl Into<String>) -> Error {
    Error::Parse {
        path: PathBuf::from("bun.lockb"),
        message: message.into(),
    }
}

fn invalid() -> Error {
    err("InvalidLockfile")
}

/// bun's `Wyhash11` with seed 0 (`semver.String.Builder.string_hash`), which keys the name
/// hashes stored in bun.lockb.
pub(crate) fn string_hash(input: &[u8]) -> u64 {
    const P: [u64; 5] = [
        0xa0761d6478bd642f,
        0xe7037ed1a0b428db,
        0x8ebc6af09c88c6e3,
        0x589965cc75374cc3,
        0x1d8e4e27c47d124f,
    ];
    fn mum(a: u64, b: u64) -> u64 {
        let r = u128::from(a) * u128::from(b);
        ((r >> 64) ^ r) as u64
    }
    fn mix0(a: u64, b: u64, seed: u64) -> u64 {
        mum(a ^ seed ^ P[0], b ^ seed ^ P[1])
    }
    fn mix1(a: u64, b: u64, seed: u64) -> u64 {
        mum(a ^ seed ^ P[2], b ^ seed ^ P[3])
    }
    fn le(d: &[u8]) -> u64 {
        let mut b = [0u8; 8];
        b[..d.len()].copy_from_slice(d);
        u64::from_le_bytes(b)
    }
    fn swapped8(d: &[u8]) -> u64 {
        (le(&d[0..4]) << 32) | le(&d[4..8])
    }
    /// The 1..=8 byte reads of wyhash's tail.
    fn partial(d: &[u8]) -> u64 {
        match d.len() {
            1 => le(&d[..1]),
            2 => le(&d[..2]),
            3 => (le(&d[..2]) << 8) | le(&d[2..3]),
            4 => le(&d[..4]),
            5 => (le(&d[..4]) << 8) | le(&d[4..5]),
            6 => (le(&d[..4]) << 16) | le(&d[4..6]),
            7 => (le(&d[..4]) << 24) | (le(&d[4..6]) << 8) | le(&d[6..7]),
            _ => swapped8(d),
        }
    }
    let mut seed = 0u64;
    let aligned = input.len() - input.len() % 32;
    for block in input[..aligned].as_chunks::<32>().0 {
        seed = mix0(le(&block[0..8]), le(&block[8..16]), seed)
            ^ mix1(le(&block[16..24]), le(&block[24..32]), seed);
    }
    let k = &input[aligned..];
    seed = match k.len() {
        0 => seed,
        1..=8 => mix0(partial(k), P[4], seed),
        9..=16 => mix0(swapped8(k), partial(&k[8..]), seed),
        17..=24 => mix0(swapped8(k), swapped8(&k[8..]), seed) ^ mix1(partial(&k[16..]), P[4], seed),
        _ => {
            mix0(swapped8(k), swapped8(&k[8..]), seed)
                ^ mix1(swapped8(&k[16..]), partial(&k[24..]), seed)
        }
    };
    mum(seed ^ input.len() as u64, P[4])
}

struct Stream<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Stream<'a> {
    fn u64(&mut self) -> Result<u64> {
        let bytes = self
            .buf
            .get(self.pos..self.pos + 8)
            .ok_or_else(|| err("lockfile is missing data"))?;
        self.pos += 8;
        Ok(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self
            .buf
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| err("lockfile is missing data"))?;
        self.pos += 4;
        Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
    }

    /// bun `Buffers::read_array`: absolute `[start, end)` positions, then the payload.
    fn array(&mut self, size: usize, align: usize) -> Result<&'a [u8]> {
        let field = self.pos;
        let start = self.u64()?;
        if start == 0xDEAD_BEEF || start == 0 || start < field as u64 {
            return Err(err("CorruptLockfile"));
        }
        let end = self.u64()?;
        if end == 0xDEAD_BEEF || end == 0 || start > end || end > self.buf.len() as u64 {
            return Err(err("CorruptLockfile"));
        }
        self.pos = end as usize;
        let len = (end - start) as usize;
        if len == 0 {
            return Ok(&[]);
        }
        if !(start as usize).is_multiple_of(align) || !len.is_multiple_of(size) {
            return Err(err("CorruptLockfile"));
        }
        Ok(&self.buf[start as usize..end as usize])
    }

    /// Consumes the next 8 bytes if they are `tag`.
    fn section_is(&mut self, total: u64, tag: u64) -> Result<bool> {
        if total.saturating_sub(self.pos as u64) <= 8 {
            return Ok(false);
        }
        if self.u64()? == tag {
            return Ok(true);
        }
        self.pos -= 8;
        Ok(false)
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// bun `semver.String::slice`: empty, inline (top bit of the last byte clear, up to the
/// first NUL), or an offset and length into `string_bytes`. Out-of-range pointers read as
/// empty, as in bun.
fn sem_str<'b>(raw: &'b [u8], string_bytes: &'b [u8]) -> Result<&'b str> {
    let bytes: &[u8] = if raw[7] & 0x80 == 0 {
        let len = raw.iter().position(|&b| b == 0).unwrap_or(8);
        &raw[..len]
    } else {
        let bits = u64::from_le_bytes(raw[..8].try_into().expect("8 bytes")) & ((1u64 << 63) - 1);
        let off = (bits & 0xffff_ffff) as usize;
        let len = (bits >> 32) as usize;
        string_bytes
            .get(off..off.saturating_add(len))
            .unwrap_or_default()
    };
    std::str::from_utf8(bytes).map_err(|_| err("lockfile string is not UTF-8"))
}

/// A semver version as bun stores it: `major.minor.patch` (u64, or u32 in format 2), then the
/// prerelease and build tags as external strings.
fn version_at(b: &[u8], at: usize, wide: bool, string_bytes: &[u8]) -> Result<String> {
    let (major, minor, patch, tags) = if wide {
        (
            u64_at(b, at),
            u64_at(b, at + 8),
            u64_at(b, at + 16),
            at + 24,
        )
    } else {
        (
            u64::from(u32_at(b, at)),
            u64::from(u32_at(b, at + 4)),
            u64::from(u32_at(b, at + 8)),
            at + 16,
        )
    };
    let pre = sem_str(&b[tags..tags + 8], string_bytes)?;
    let build = sem_str(&b[tags + 16..tags + 24], string_bytes)?;
    let mut v = format!("{major}.{minor}.{patch}");
    if !pre.is_empty() {
        v.push('-');
        v.push_str(pre);
    }
    if !build.is_empty() {
        v.push('+');
        v.push_str(build);
    }
    Ok(v)
}

fn integrity_at(b: &[u8]) -> Option<Integrity> {
    let alg = match b[0] {
        1 => HashAlgorithm::Sha1,
        2 => HashAlgorithm::Sha256,
        3 => HashAlgorithm::Sha384,
        4 => HashAlgorithm::Sha512,
        _ => return None,
    };
    Integrity::from_digest(alg, &b[1..1 + alg.digest_len()]).ok()
}

/// One decoded dependency row.
struct Dep {
    name: String,
    behavior: Behavior,
    literal: String,
}

fn dependency(row: &[u8], string_bytes: &[u8]) -> Result<Dep> {
    Ok(Dep {
        name: sem_str(&row[0..8], string_bytes)?.to_owned(),
        behavior: Behavior(row[16] & 0b0111_1110),
        literal: sem_str(&row[18..26], string_bytes)?.to_owned(),
    })
}

fn dependencies(array: &[u8], string_bytes: &[u8]) -> Result<Vec<Dep>> {
    array
        .as_chunks::<DEPENDENCY_SIZE>()
        .0
        .iter()
        .map(|row| dependency(row, string_bytes))
        .collect()
}

fn names(array: &[u8], string_bytes: &[u8]) -> Result<Vec<String>> {
    array
        .as_chunks::<8>()
        .0
        .iter()
        .map(|raw| sem_str(raw, string_bytes).map(str::to_owned))
        .collect()
}

/// Reads a bun.lockb. Format 3 is current; format 2 (u32 semver numbers) is migrated, as
/// bun does. Older formats are rejected like bun (`OutdatedLockfileVersion`).
pub fn parse(bytes: &[u8]) -> Result<LockGraph> {
    if bytes.len() < HEADER.len() || &bytes[..HEADER.len()] != HEADER {
        return Err(invalid());
    }
    let mut s = Stream {
        buf: bytes,
        pos: HEADER.len(),
    };
    let format = s.u32()?;
    if format > FORMAT_CURRENT {
        return Err(err("UnexpectedLockfileVersion"));
    }
    if format < FORMAT_CURRENT && format != FORMAT_V2 {
        return Err(err("OutdatedLockfileVersion"));
    }
    let wide = format == FORMAT_CURRENT;
    s.pos += 32; // meta hash
    let total = s.u64()?;
    if total > bytes.len() as u64 {
        return Err(err("LockfileIsMissingData"));
    }
    let total_usize = total as usize;

    // The package table.
    let count = s.u64()?;
    if count > u64::from(u32::MAX) - 1 {
        return Err(err("Lockfile validation failed: list is impossibly long"));
    }
    let count = count as usize;
    if s.u64()? != 8 {
        return Err(err("Lockfile validation failed: alignment mismatch"));
    }
    let fields = s.u64()?;
    if fields != FIELD_COUNT && fields != FIELD_COUNT - 1 {
        return Err(err(
            "Lockfile validation failed: unexpected number of package fields",
        ));
    }
    let begin = s.u64()? as usize;
    let end = s.u64()? as usize;
    if begin > total_usize || end > total_usize || begin > end {
        return Err(err(
            "Lockfile validation failed: invalid package list range",
        ));
    }
    s.pos = begin;
    let resolution_size = if wide { 72 } else { 64 };
    let sizes = [
        8,
        8,
        resolution_size,
        8,
        8,
        META_SIZE,
        BIN_SIZE,
        SCRIPTS_SIZE,
    ];
    let mut columns: [&[u8]; 8] = [&[]; 8];
    for (field, size) in sizes.iter().enumerate() {
        let len = size * count;
        let end_pos = s.pos + len;
        if end_pos <= end {
            columns[field] = &bytes[s.pos..end_pos];
            s.pos = end_pos;
        } else if field == 7 {
            columns[field] = &[];
        } else {
            return Err(err(
                "Lockfile validation failed: invalid package list range",
            ));
        }
    }
    let [
        name_col,
        _hash_col,
        res_col,
        deps_col,
        resolutions_col,
        meta_col,
        bin_col,
        scripts_col,
    ] = columns;
    let mut bin_map_end = 0usize;
    for raw in res_col.chunks_exact(resolution_size) {
        if !matches!(
            raw[0],
            res_tag::ROOT
                | res_tag::NPM
                | res_tag::FOLDER
                | res_tag::LOCAL_TARBALL
                | res_tag::GITHUB
                | res_tag::GIT
                | res_tag::SYMLINK
                | res_tag::WORKSPACE
                | res_tag::REMOTE_TARBALL
        ) {
            return Err(err("Lockfile validation failed: invalid resolution tag"));
        }
    }
    for raw in meta_col.as_chunks::<META_SIZE>().0 {
        if raw[0] > 2 || raw[85] > 2 {
            return Err(err("Lockfile validation failed: invalid package meta"));
        }
    }
    for raw in bin_col.as_chunks::<BIN_SIZE>().0 {
        if raw[0] > 4 {
            return Err(err("Lockfile validation failed: invalid bin tag"));
        }
        if raw[0] == 4 {
            let (off, len) = (u32_at(raw, 4) as usize, u32_at(raw, 8) as usize);
            if len % 2 != 0 {
                return Err(invalid());
            }
            bin_map_end = bin_map_end.max(off + len);
        }
    }
    for raw in scripts_col.as_chunks::<SCRIPTS_SIZE>().0 {
        if raw[48] > 1 {
            return Err(err("Lockfile validation failed: invalid package scripts"));
        }
    }

    // The buffers.
    let trees_raw = s.array(20, 1)?;
    let hoisted_raw = s.array(4, 4)?;
    let resolutions_raw = s.array(4, 4)?;
    let deps_raw = s.array(DEPENDENCY_SIZE, 1)?;
    let extern_strings = s.array(16, 8)?;
    let string_bytes = s.array(1, 1)?;
    if s.u64()? != 0 {
        return Err(err("Lockfile is malformed (expected 0 at the end)"));
    }
    let deps = dependencies(deps_raw, string_bytes)?;
    let mut resolutions: Vec<u32> = resolutions_raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    if resolutions.len() != deps.len() || bin_map_end > extern_strings.len() / 16 {
        return Err(invalid());
    }
    check_trees(trees_raw, hoisted_raw, &resolutions, &deps, count)?;
    for (id, dep) in resolutions.iter_mut().zip(&deps) {
        if *id as usize >= count && *id != INVALID_ID {
            // Writers before v0.6.11 left the id of a peer they did not resolve uninitialized.
            if !dep.behavior.is_peer() {
                return Err(invalid());
            }
            *id = INVALID_ID;
        }
    }

    // Packages.
    let mut graph = LockGraph::new(Package::new("", "", Resolution::Root));
    graph.packages.clear();
    graph.config_version = ConfigVersion::V0;
    let mut slices: Vec<(usize, usize)> = Vec::with_capacity(count);
    for i in 0..count {
        let name = sem_str(&name_col[i * 8..i * 8 + 8], string_bytes)?.to_owned();
        let res = &res_col[i * resolution_size..(i + 1) * resolution_size];
        let meta = &meta_col[i * META_SIZE..(i + 1) * META_SIZE];
        if u32_at(meta, 8) as usize != i {
            return Err(invalid());
        }
        let integrity = integrity_at(&meta[20..85]);
        let str_at = |at: usize| sem_str(&res[at..at + 8], string_bytes).map(str::to_owned);
        let mut version = String::new();
        let resolution = match res[0] {
            res_tag::ROOT => Resolution::Root,
            res_tag::NPM => {
                let url = str_at(8)?;
                version = version_at(res, 16, wide, string_bytes)?;
                let tarball_url = (!url.is_empty()
                    && !crate::migration::url_is_under_registry(
                        &url,
                        fnpm_config::DEFAULT_REGISTRY,
                    ))
                .then_some(url);
                Resolution::Npm {
                    version: version.clone(),
                    tarball_url,
                    integrity,
                }
            }
            res_tag::FOLDER => Resolution::Folder { path: str_at(8)? },
            res_tag::LOCAL_TARBALL => Resolution::LocalTarball {
                path: str_at(8)?,
                integrity,
            },
            res_tag::REMOTE_TARBALL => Resolution::RemoteTarball {
                url: str_at(8)?,
                integrity,
            },
            res_tag::SYMLINK => Resolution::Symlink { path: str_at(8)? },
            res_tag::WORKSPACE => Resolution::Workspace { path: str_at(8)? },
            t @ (res_tag::GIT | res_tag::GITHUB) => {
                let (owner, repo, committish, resolved) =
                    (str_at(8)?, str_at(16)?, str_at(24)?, str_at(32)?);
                if !resolved.is_empty() && !is_safe_resolved_tag(&resolved) {
                    return Err(err(format!(
                        "Invalid git dependency tag \"{resolved}\" in bun.lockb"
                    )));
                }
                if t == res_tag::GIT {
                    let url = crate::migration::git_display_url(&repo);
                    Resolution::Git {
                        url,
                        committish,
                        resolved_commit: resolved,
                        integrity,
                    }
                } else {
                    Resolution::Github {
                        owner,
                        repo,
                        committish,
                        resolved_commit: resolved,
                        integrity,
                    }
                }
            }
            _ => unreachable!("validated above"),
        };
        if i == 0 && resolution != Resolution::Root {
            return Err(invalid());
        }
        let mut package = Package::new(name, version, resolution);
        let os = u16::from_le_bytes([meta[4], meta[5]]);
        let cpu = u16::from_le_bytes([meta[2], meta[3]]);
        package.platform = PlatformConstraints {
            os: OsSet(os & OsSet::ALL.0),
            cpu: CpuSet(cpu & CpuSet::ALL.0),
            libc: LibcSet(meta[6] & LibcSet::ALL.0),
        };
        package.meta.has_install_script = meta[85] == 2;
        let bin = &bin_col[i * BIN_SIZE..(i + 1) * BIN_SIZE];
        package.bin = match bin[0] {
            1 => Bin::File {
                path: sem_str(&bin[4..12], string_bytes)?.to_owned(),
            },
            2 => Bin::NamedFile {
                name: sem_str(&bin[4..12], string_bytes)?.to_owned(),
                path: sem_str(&bin[12..20], string_bytes)?.to_owned(),
            },
            3 => Bin::Dir {
                path: sem_str(&bin[4..12], string_bytes)?.to_owned(),
            },
            4 => {
                let (off, len) = (u32_at(bin, 4) as usize, u32_at(bin, 8) as usize);
                let mut entries = Vec::with_capacity(len / 2);
                for pair in extern_strings[off * 16..(off + len) * 16]
                    .as_chunks::<32>()
                    .0
                {
                    entries.push((
                        sem_str(&pair[0..8], string_bytes)?.to_owned(),
                        sem_str(&pair[16..24], string_bytes)?.to_owned(),
                    ));
                }
                Bin::Map { entries }
            }
            _ => Bin::None,
        };
        if let Some(scripts) = scripts_col.get(i * SCRIPTS_SIZE..(i + 1) * SCRIPTS_SIZE) {
            let mut flags = ScriptFlags::default();
            for (k, (_, flag)) in ScriptFlags::NAMES.iter().enumerate() {
                if !sem_str(&scripts[k * 8..k * 8 + 8], string_bytes)?.is_empty() {
                    flags.0 |= flag.0;
                }
            }
            package.meta.scripts = flags;
        }
        let d = &deps_col[i * 8..i * 8 + 8];
        let r = &resolutions_col[i * 8..i * 8 + 8];
        let (d_off, d_len, r_off, r_len) = (
            u32_at(d, 0) as usize,
            u32_at(d, 4) as usize,
            u32_at(r, 0) as usize,
            u32_at(r, 4) as usize,
        );
        if d_off + d_len > deps.len()
            || r_off + r_len > deps.len()
            || d_len != r_len
            || (r_off != d_off && d_len != 0)
        {
            return Err(invalid());
        }
        slices.push((d_off, d_len));
        graph.packages.push(package);
    }
    if graph.packages.is_empty() {
        return Err(invalid());
    }
    for (i, (off, len)) in slices.into_iter().enumerate() {
        let mut edges = Vec::with_capacity(len);
        for k in off..off + len {
            let dep = &deps[k];
            if crate::migration::is_workspace_only(dep.behavior) {
                continue;
            }
            let target = (resolutions[k] != INVALID_ID).then_some(PackageId(resolutions[k]));
            edges.push(DepEdge {
                name: dep.name.clone(),
                literal: dep.literal.clone(),
                behavior: dep.behavior,
                target,
            });
        }
        graph.packages[i].dependencies = edges;
    }

    read_sections(&mut s, total, wide, string_bytes, &mut graph)?;
    super::npm::finish_graph(&mut graph);
    // bun keeps one package per (name, resolution); a file that breaks that is corrupt.
    graph
        .validate()
        .map_err(|e| err(format!("InvalidLockfile: {e}")))?;
    Ok(graph)
}

/// A `node_modules` folder of the stored hoisted tree (bun `Tree::External`).
struct TreeRow {
    id: u32,
    dependency_id: u32,
    parent: u32,
    off: u32,
    len: u32,
}

const TREE_INVALID_ID: u32 = u32::MAX;
const ROOT_DEP_ID: u32 = INVALID_ID - 1;

/// bun `Buffers::load` (the legacy tree conversion) and `ids_and_windows_are_in_range`: the
/// stored hoisted tree is not used to bind dependencies, but bun rejects a lockfile whose tree
/// is out of range, cyclic or names an unsafe folder, and so does fnpm.
fn check_trees(
    trees_raw: &[u8],
    hoisted_raw: &[u8],
    resolutions: &[u32],
    deps: &[Dep],
    packages_len: usize,
) -> Result<()> {
    let mut trees: Vec<TreeRow> = trees_raw
        .as_chunks::<20>()
        .0
        .iter()
        .map(|t| TreeRow {
            id: u32_at(t, 0),
            dependency_id: u32_at(t, 4),
            parent: u32_at(t, 8),
            off: u32_at(t, 12),
            len: u32_at(t, 16),
        })
        .collect();
    let mut hoisted: Vec<u32> = hoisted_raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();

    // Writers before v0.5.7 stored package ids where dependency ids belong: each mention of a
    // package is the next dependency that resolves to it.
    if trees
        .first()
        .is_some_and(|t| t.dependency_id != ROOT_DEP_ID)
    {
        let mut first = vec![INVALID_ID; packages_len];
        let mut next = vec![INVALID_ID; resolutions.len()];
        for (dependency_id, &package_id) in resolutions.iter().enumerate().rev() {
            if let Some(head) = first.get_mut(package_id as usize) {
                next[dependency_id] = *head;
                *head = dependency_id as u32;
            }
        }
        let take = |heads: &mut [u32], id: &mut u32| -> Result<()> {
            if *id == 0 {
                *id = ROOT_DEP_ID;
            } else if *id != INVALID_ID {
                let head = heads
                    .get_mut(*id as usize)
                    .filter(|head| **head != INVALID_ID)
                    .ok_or_else(|| err("LockfileIsMissingResolutionData"))?;
                *id = *head;
                *head = next[*id as usize];
            }
            Ok(())
        };
        let mut heads = first.clone();
        for tree in &mut trees {
            take(&mut heads, &mut tree.dependency_id)?;
        }
        for id in &mut hoisted {
            take(&mut first, id)?;
        }
        // Those writers also left unused slots and gave the root no id.
        let mut kept = 0u32;
        let new_ids: Vec<u32> = trees
            .iter()
            .map(|tree| {
                let id = kept;
                kept += u32::from(tree.id != TREE_INVALID_ID);
                id
            })
            .collect();
        trees.retain(|tree| tree.id != TREE_INVALID_ID);
        for tree in &mut trees {
            for id in [&mut tree.id, &mut tree.parent] {
                if *id != TREE_INVALID_ID {
                    *id = *new_ids.get(*id as usize).ok_or_else(invalid)?;
                }
            }
        }
        if let Some(root) = trees.first_mut() {
            root.dependency_id = ROOT_DEP_ID;
        }
    }

    let fits = |off: u32, len: u32, buffer_len: usize| off as usize + len as usize <= buffer_len;
    let hoisted_ok = hoisted.iter().all(|&id| {
        deps.get(id as usize)
            .is_some_and(|row| fnpm_types::name::is_safe_install_folder_name(&row.name))
    });
    // A tree follows its parent, which rules out cycles.
    let trees_ok = trees.iter().enumerate().all(|(position, tree)| {
        tree.id as usize == position
            && fits(tree.off, tree.len, hoisted.len())
            && if position == 0 {
                tree.parent == TREE_INVALID_ID
            } else {
                (tree.parent as usize) < position && (tree.dependency_id as usize) < deps.len()
            }
    });
    if hoisted_ok && trees_ok {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn u64s(array: &[u8]) -> Vec<u64> {
    array
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect()
}

/// The name a `trustedDependencies` entry matches for an installed package: the package's
/// name for registry packages, the dependency's alias otherwise (bun `trusted_name`).
fn trust_candidates(graph: &LockGraph) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for package in &graph.packages {
        for edge in &package.dependencies {
            let Some(target) = edge.target.and_then(|t| graph.get(t)) else {
                continue;
            };
            let name = if matches!(target.resolution, Resolution::Npm { .. }) {
                &target.name
            } else {
                &edge.name
            };
            names.insert(name.clone());
        }
    }
    names
}

fn selector(name: &str, range: &str) -> PackageSelector {
    PackageSelector {
        name: name.to_owned(),
        range: range.to_owned(),
    }
}

fn read_sections(
    s: &mut Stream<'_>,
    total: u64,
    wide: bool,
    string_bytes: &[u8],
    graph: &mut LockGraph,
) -> Result<()> {
    if s.section_is(total, HAS_WORKSPACE_PACKAGE_IDS)? {
        let hashes = u64s(s.array(8, 8)?);
        let version_size = if wide { 56 } else { 48 };
        let versions_raw = s.array(version_size, 8)?;
        if hashes.len() * version_size != versions_raw.len() {
            return Err(invalid());
        }
        let path_hashes = s.array(8, 8)?;
        let paths = s.array(8, 1)?;
        if path_hashes.len() != paths.len() {
            return Err(invalid());
        }
        let mut versions: HashMap<u64, String> = HashMap::new();
        for (k, hash) in hashes.iter().enumerate() {
            versions.insert(
                *hash,
                version_at(versions_raw, k * version_size, wide, string_bytes)?,
            );
        }
        for package in &mut graph.packages {
            if matches!(package.resolution, Resolution::Workspace { .. })
                && let Some(version) = versions.get(&string_hash(package.name.as_bytes()))
            {
                package.version = version.clone();
            }
        }
    }

    let remaining = total.saturating_sub(s.pos as u64);
    if remaining >= 8 {
        let next = s.u64()?;
        if remaining > 8 && next == HAS_TRUSTED_DEPENDENCIES {
            let hashes: BTreeSet<u32> = s
                .array(4, 4)?
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c))
                .collect();
            // Only the truncated hashes are stored; names come from the installed packages. A
            // list that names none of them records nothing, as bun.lock (which writes only the
            // installed names) would.
            let names: BTreeSet<String> = trust_candidates(graph)
                .into_iter()
                .filter(|n| hashes.contains(&(string_hash(n.as_bytes()) as u32)))
                .collect();
            graph.trusted_dependencies = (!names.is_empty()).then_some(names);
        } else if next == HAS_EMPTY_TRUSTED_DEPENDENCIES {
            graph.trusted_dependencies = Some(BTreeSet::new());
        } else {
            s.pos -= 8;
        }
    }

    if s.section_is(total, HAS_OVERRIDES)? {
        let hashes = s.array(8, 8)?;
        let deps = dependencies(s.array(DEPENDENCY_SIZE, 1)?, string_bytes)?;
        if hashes.len() / 8 != deps.len() {
            return Err(invalid());
        }
        for dep in deps {
            let rule = OverrideRule {
                selector: OverrideSelector {
                    parent: None,
                    target: selector(&dep.name, ""),
                },
                value: dep.literal,
            };
            graph.overrides.push(rule);
        }
    }

    if s.section_is(total, HAS_PATCHED_DEPENDENCIES)? {
        let hashes = u64s(s.array(8, 8)?);
        let patches = s.array(24, 8)?;
        if hashes.len() * 24 != patches.len() {
            return Err(invalid());
        }
        let mut by_hash: HashMap<u64, String> = HashMap::new();
        for package in &graph.packages {
            let version = match &package.resolution {
                Resolution::Npm { .. } | Resolution::Workspace { .. } => package.version.clone(),
                other => other.to_string(),
            };
            let key = format!("{}@{version}", package.name);
            by_hash.insert(string_hash(key.as_bytes()), key);
        }
        for (k, hash) in hashes.iter().enumerate() {
            let raw = &patches[k * 24..(k + 1) * 24];
            let path = sem_str(&raw[0..8], string_bytes)?.to_owned();
            let patch_hash = match raw[15] {
                0 => Some(format!("{:x}", u64_at(raw, 16))),
                1 => None,
                _ => return Err(invalid()),
            };
            // A patch for a package that is not in the lockfile cannot be named.
            if let Some(key) = by_hash.get(hash) {
                graph.patched_dependencies.insert(
                    key.clone(),
                    PatchedDependency {
                        path,
                        hash: patch_hash,
                    },
                );
            }
        }
    }

    if s.section_is(total, HAS_CATALOGS)? {
        let default_names = names(s.array(8, 1)?, string_bytes)?;
        let default_deps = dependencies(s.array(DEPENDENCY_SIZE, 1)?, string_bytes)?;
        if default_names.len() != default_deps.len() {
            return Err(invalid());
        }
        graph.catalogs.default = default_names
            .into_iter()
            .zip(default_deps)
            .map(|(n, d)| (n, d.literal))
            .collect();
        let catalog_names = names(s.array(8, 1)?, string_bytes)?;
        for catalog in catalog_names {
            let dep_names = names(s.array(8, 1)?, string_bytes)?;
            let deps = dependencies(s.array(DEPENDENCY_SIZE, 1)?, string_bytes)?;
            if dep_names.len() != deps.len() {
                return Err(invalid());
            }
            let entries: BTreeMap<String, String> = dep_names
                .into_iter()
                .zip(deps)
                .map(|(n, d)| (n, d.literal))
                .collect();
            graph
                .catalogs
                .named
                .entry(catalog)
                .or_default()
                .extend(entries);
        }
    }

    if s.section_is(total, HAS_CONFIG_VERSION)? {
        graph.config_version = match s.u64()? {
            0 => ConfigVersion::V0,
            1 => ConfigVersion::V1,
            _ => return Err(invalid()),
        };
    }

    if s.section_is(total, HAS_SCOPED_OVERRIDES)? {
        let parents = dependencies(s.array(DEPENDENCY_SIZE, 1)?, string_bytes)?;
        let ranges = dependencies(s.array(DEPENDENCY_SIZE, 1)?, string_bytes)?;
        let deps = dependencies(s.array(DEPENDENCY_SIZE, 1)?, string_bytes)?;
        if parents.len() != ranges.len() || parents.len() != deps.len() {
            return Err(invalid());
        }
        for ((parent, range), dep) in parents.into_iter().zip(ranges).zip(deps) {
            let parent = (!parent.name.is_empty()).then(|| selector(&parent.name, &parent.literal));
            let rule = OverrideRule {
                selector: OverrideSelector {
                    parent,
                    target: selector(&dep.name, &range.literal),
                },
                value: dep.literal,
            };
            match graph
                .overrides
                .iter_mut()
                .find(|r| r.selector == rule.selector)
            {
                Some(existing) => *existing = rule,
                None => graph.overrides.push(rule),
            }
        }
    }

    if s.section_is(total, HAS_PACKAGE_EXTENSIONS)? {
        // Which extensions were applied; the rules themselves live in package.json.
        let _ = s.array(8, 8)?;
    }
    Ok(())
}

// ─── writing ────────────────────────────────────────────────────────────────────────────────

/// bun's `semver.String` pool: every string goes into `bytes` once, short ones inline.
#[derive(Default)]
struct Strings {
    bytes: Vec<u8>,
    at: HashMap<String, u64>,
}

impl Strings {
    /// The 8-byte `semver.String` for `s` (bun `String.Builder.append`).
    fn add(&mut self, s: &str) -> [u8; 8] {
        let b = s.as_bytes();
        let inline = b.len() < 8 || (b.len() == 8 && b[7] & 0x80 == 0);
        if inline && !b.contains(&0) {
            let mut raw = [0u8; 8];
            raw[..b.len()].copy_from_slice(b);
            return raw;
        }
        let off = *self.at.entry(s.to_owned()).or_insert_with(|| {
            let off = self.bytes.len() as u64;
            self.bytes.extend_from_slice(b);
            off
        });
        (off | (b.len() as u64) << 32 | 1 << 63).to_le_bytes()
    }

    /// bun's `ExternalString`: the string, then its hash.
    fn external(&mut self, s: &str) -> [u8; 16] {
        let mut raw = [0u8; 16];
        raw[..8].copy_from_slice(&self.add(s));
        raw[8..].copy_from_slice(&string_hash(s.as_bytes()).to_le_bytes());
        raw
    }

    /// A semver version as format 3 stores it, or `None` when `v` is not one.
    fn version(&mut self, v: &str) -> Option<[u8; 56]> {
        let (core, build) = v.split_once('+').unwrap_or((v, ""));
        let (core, pre) = core.split_once('-').unwrap_or((core, ""));
        let mut parts = core.split('.').map(|n| n.parse::<u64>().ok());
        let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let mut raw = [0u8; 56];
        raw[0..8].copy_from_slice(&major.to_le_bytes());
        raw[8..16].copy_from_slice(&minor.to_le_bytes());
        raw[16..24].copy_from_slice(&patch.to_le_bytes());
        if !pre.is_empty() {
            raw[24..40].copy_from_slice(&self.external(pre));
        }
        if !build.is_empty() {
            raw[40..56].copy_from_slice(&self.external(build));
        }
        Some(raw)
    }

    /// bun `Dependency.External`: name, name hash, behavior, version tag, literal.
    fn dependency(&mut self, name: &str, behavior: Behavior, tag: u8, literal: &str) -> [u8; 26] {
        let mut row = [0u8; DEPENDENCY_SIZE];
        row[0..8].copy_from_slice(&self.add(name));
        row[8..16].copy_from_slice(&string_hash(name.as_bytes()).to_le_bytes());
        row[16] = behavior.0 & 0b0111_1110;
        row[17] = tag;
        row[18..26].copy_from_slice(&self.add(literal));
        row
    }
}

/// bun's `Dependency.Version.Tag` of a literal.
fn version_tag(literal: &str) -> u8 {
    crate::flat::Spec::parse(literal).tag as u8
}

/// The output stream with bun's positioned writes.
struct Out(Vec<u8>);

impl Out {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn align8(&mut self) {
        while !self.0.len().is_multiple_of(8) {
            self.0.push(0);
        }
    }

    fn patch_u64(&mut self, at: usize, v: u64) {
        self.0[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// bun `Buffers::write_array`: `[start, end)`, the type-name prefix, then the payload
    /// aligned to 8.
    fn array(&mut self, bytes: &[u8], prefix: &str) {
        let field = self.0.len();
        self.u64(0xDEAD_BEEF);
        self.u64(0xDEAD_BEEF);
        self.0.extend_from_slice(prefix.as_bytes());
        if !bytes.is_empty() {
            self.align8();
        }
        let start = self.0.len() as u64;
        self.0.extend_from_slice(bytes);
        let end = self.0.len() as u64;
        self.patch_u64(field, start);
        self.patch_u64(field + 8, end);
    }
}

/// The tagged sections, written after the buffers (their arrays hold absolute positions).
#[derive(Default)]
struct Sections(Vec<Op>);

enum Op {
    U64(u64),
    Array(Vec<u8>, &'static str),
}

impl Sections {
    fn u64(&mut self, v: u64) {
        self.0.push(Op::U64(v));
    }

    fn array(&mut self, bytes: &[u8], prefix: &'static str) {
        self.0.push(Op::Array(bytes.to_vec(), prefix));
    }
}

const PREFIX_U64: &str = "\n<u64> 8 sizeof, 8 alignof\n";
const PREFIX_U32: &str = "\n<u32> 4 sizeof, 4 alignof\n";
const PREFIX_DEP_EXTERNAL: &str = "\n<[26]u8> 26 sizeof, 1 alignof\n";
const PREFIX_SEMVER_VERSION: &str = "\n<semver.Version.Version> 56 sizeof, 8 alignof\n";
const PREFIX_SEMVER_STRING: &str = "\n<semver.String> 8 sizeof, 1 alignof\n";
const PREFIX_PATCHED_DEP: &str = "\n<install.lockfile.PatchedDep> 24 sizeof, 8 alignof\n";

/// The 72-byte format-3 resolution of a package.
fn resolution_bytes(res: &Resolution, version: &str, s: &mut Strings) -> [u8; 72] {
    let mut raw = [0u8; 72];
    let put = |raw: &mut [u8; 72], at: usize, v: &str, s: &mut Strings| {
        raw[at..at + 8].copy_from_slice(&s.add(v));
    };
    match res {
        Resolution::Root => raw[0] = res_tag::ROOT,
        Resolution::Npm { tarball_url, .. } => {
            raw[0] = res_tag::NPM;
            put(&mut raw, 8, tarball_url.as_deref().unwrap_or(""), s);
            if let Some(v) = s.version(version) {
                raw[16..72].copy_from_slice(&v);
            }
        }
        Resolution::Folder { path } => {
            raw[0] = res_tag::FOLDER;
            put(&mut raw, 8, path, s);
        }
        Resolution::LocalTarball { path, .. } => {
            raw[0] = res_tag::LOCAL_TARBALL;
            put(&mut raw, 8, path, s);
        }
        Resolution::RemoteTarball { url, .. } => {
            raw[0] = res_tag::REMOTE_TARBALL;
            put(&mut raw, 8, url, s);
        }
        Resolution::Symlink { path } => {
            raw[0] = res_tag::SYMLINK;
            put(&mut raw, 8, path, s);
        }
        Resolution::Workspace { path } => {
            raw[0] = res_tag::WORKSPACE;
            put(&mut raw, 8, path, s);
        }
        Resolution::Git {
            url,
            committish,
            resolved_commit,
            ..
        } => {
            raw[0] = res_tag::GIT;
            put(&mut raw, 16, url, s);
            put(&mut raw, 24, committish, s);
            put(&mut raw, 32, resolved_commit, s);
        }
        Resolution::Github {
            owner,
            repo,
            committish,
            resolved_commit,
            ..
        } => {
            raw[0] = res_tag::GITHUB;
            put(&mut raw, 8, owner, s);
            put(&mut raw, 16, repo, s);
            put(&mut raw, 24, committish, s);
            put(&mut raw, 32, resolved_commit, s);
        }
    }
    raw
}

/// The 65 bytes of bun's `Integrity`: the algorithm, then the digest.
fn integrity_bytes(i: Option<&Integrity>) -> [u8; 65] {
    let mut raw = [0u8; 65];
    if let Some(i) = i {
        raw[0] = match i.algorithm() {
            HashAlgorithm::Sha1 => 1,
            HashAlgorithm::Sha256 => 2,
            HashAlgorithm::Sha384 => 3,
            HashAlgorithm::Sha512 => 4,
        };
        raw[1..1 + i.digest().len()].copy_from_slice(i.digest());
    }
    raw
}

/// Writes a bun.lockb (format 3) the way bun 1.x does: the package columns, the buffers of the
/// hoisted install tree, then the tagged sections.
pub fn serialize(graph: &LockGraph) -> Result<Vec<u8>> {
    let mut flat = crate::flat::Flat::new(graph);
    let hoisted = crate::tree::resolve(&mut flat);
    if let Some(cycle) = hoisted.cycle {
        return Err(Error::DependencyCycle(cycle));
    }
    let mut s = Strings::default();
    let count = graph.packages.len();

    // The dependency rows, in the flat view's per-package order.
    let mut deps: Vec<u8> = Vec::with_capacity(flat.deps.len() * DEPENDENCY_SIZE);
    for d in &flat.deps {
        // bun's root lists each workspace member with its path as the literal.
        let literal = match &d.workspace_path {
            Some(path) if d.edge.is_none() => path.as_ref(),
            _ => d.literal,
        };
        deps.extend_from_slice(&s.dependency(d.name, d.behavior, d.version.tag as u8, literal));
    }
    let resolutions: Vec<u8> = flat
        .resolutions
        .iter()
        .flat_map(|r| r.to_le_bytes())
        .collect();

    // The install tree.
    let mut trees: Vec<u8> = Vec::with_capacity(hoisted.trees.len() * 20);
    let mut hoisted_ids: Vec<u8> = Vec::new();
    let mut off = 0u32;
    for (i, t) in hoisted.trees.iter().enumerate() {
        let rows = hoisted.rows.get(i).map_or(&[][..], Vec::as_slice);
        let (dependency_id, parent) = if i == 0 {
            (INVALID_ID - 1, INVALID_ID)
        } else {
            (t.dependency_id, t.parent)
        };
        for v in [i as u32, dependency_id, parent, off, rows.len() as u32] {
            trees.extend_from_slice(&v.to_le_bytes());
        }
        off += rows.len() as u32;
        for d in rows {
            hoisted_ids.extend_from_slice(&d.to_le_bytes());
        }
    }

    // The package columns.
    let mut extern_strings: Vec<u8> = Vec::new();
    let mut columns: [Vec<u8>; 8] = Default::default();
    for (i, p) in graph.packages.iter().enumerate() {
        columns[0].extend_from_slice(&s.add(&p.name));
        columns[1].extend_from_slice(&string_hash(p.name.as_bytes()).to_le_bytes());
        columns[2].extend_from_slice(&resolution_bytes(&p.resolution, &p.version, &mut s));
        let (b, e) = flat.lists[i];
        let slice = [b.to_le_bytes(), (e - b).to_le_bytes()].concat();
        columns[3].extend_from_slice(&slice);
        columns[4].extend_from_slice(&slice);
        let mut meta = [0u8; META_SIZE];
        // bun's `Origin`: local for the root, npm for every other package.
        meta[0] = u8::from(i != 0);
        meta[2..4].copy_from_slice(&p.platform.cpu.0.to_le_bytes());
        meta[4..6].copy_from_slice(&p.platform.os.0.to_le_bytes());
        meta[6] = p.platform.libc.0;
        meta[8..12].copy_from_slice(&(i as u32).to_le_bytes());
        meta[20..85].copy_from_slice(&integrity_bytes(p.resolution.integrity()));
        meta[85] = if p.meta.has_install_script { 2 } else { 1 };
        columns[5].extend_from_slice(&meta);
        let mut bin = [0u8; BIN_SIZE];
        match &p.bin {
            Bin::None => {}
            Bin::File { path } => {
                bin[0] = 1;
                bin[4..12].copy_from_slice(&s.add(path));
            }
            Bin::NamedFile { name, path } => {
                bin[0] = 2;
                bin[4..12].copy_from_slice(&s.add(name));
                bin[12..20].copy_from_slice(&s.add(path));
            }
            Bin::Dir { path } => {
                bin[0] = 3;
                bin[4..12].copy_from_slice(&s.add(path));
            }
            Bin::Map { entries } => {
                bin[0] = 4;
                let at = (extern_strings.len() / 16) as u32;
                for (name, path) in entries {
                    extern_strings.extend_from_slice(&s.external(name));
                    extern_strings.extend_from_slice(&s.external(path));
                }
                bin[4..8].copy_from_slice(&at.to_le_bytes());
                bin[8..12].copy_from_slice(&(2 * entries.len() as u32).to_le_bytes());
            }
        }
        columns[6].extend_from_slice(&bin);
        // No script text (bun reads it from package.json when it installs); bun marks the
        // project's own packages as read.
        let mut scripts = [0u8; SCRIPTS_SIZE];
        scripts[48] = u8::from(p.resolution.is_local_tree());
        columns[7].extend_from_slice(&scripts);
    }

    // The tagged sections, built before the string bytes are final.
    let mut sections = Sections::default();
    let members: Vec<&Package> = graph
        .workspaces
        .iter()
        .filter(|(path, _)| !path.is_empty())
        .filter_map(|(_, id)| graph.get(*id))
        .collect();
    let versions: Vec<(u64, [u8; 56])> = members
        .iter()
        .filter_map(|m| Some((string_hash(m.name.as_bytes()), s.version(&m.version)?)))
        .collect();
    let paths: Vec<(u64, [u8; 8])> = graph
        .workspaces
        .iter()
        .filter(|(path, _)| !path.is_empty())
        .filter_map(|(path, id)| {
            let m = graph.get(*id)?;
            Some((string_hash(m.name.as_bytes()), s.add(path)))
        })
        .collect();
    if !versions.is_empty() {
        sections.u64(HAS_WORKSPACE_PACKAGE_IDS);
        let keys: Vec<u8> = versions.iter().flat_map(|v| v.0.to_le_bytes()).collect();
        let values: Vec<u8> = versions.iter().flat_map(|v| v.1).collect();
        sections.array(&keys, PREFIX_U64);
        sections.array(&values, PREFIX_SEMVER_VERSION);
        let keys: Vec<u8> = paths.iter().flat_map(|v| v.0.to_le_bytes()).collect();
        let values: Vec<u8> = paths.iter().flat_map(|v| v.1).collect();
        sections.array(&keys, PREFIX_U64);
        sections.array(&values, PREFIX_SEMVER_STRING);
    }
    match &graph.trusted_dependencies {
        Some(names) if !names.is_empty() => {
            sections.u64(HAS_TRUSTED_DEPENDENCIES);
            let keys: Vec<u8> = names
                .iter()
                .flat_map(|n| (string_hash(n.as_bytes()) as u32).to_le_bytes())
                .collect();
            sections.array(&keys, PREFIX_U32);
        }
        Some(_) => sections.u64(HAS_EMPTY_TRUSTED_DEPENDENCIES),
        None => {}
    }
    let (flat_rules, scoped): (Vec<&OverrideRule>, Vec<&OverrideRule>) = graph
        .overrides
        .iter()
        .partition(|r| r.selector.parent.is_none() && r.selector.target.range.is_empty());
    if !flat_rules.is_empty() {
        sections.u64(HAS_OVERRIDES);
        let keys: Vec<u8> = flat_rules
            .iter()
            .flat_map(|r| string_hash(r.selector.target.name.as_bytes()).to_le_bytes())
            .collect();
        let values: Vec<u8> = flat_rules
            .iter()
            .flat_map(|r| {
                let name = &r.selector.target.name;
                s.dependency(name, Behavior(0), version_tag(&r.value), &r.value)
            })
            .collect();
        sections.array(&keys, PREFIX_U64);
        sections.array(&values, PREFIX_DEP_EXTERNAL);
    }
    if !graph.patched_dependencies.is_empty() {
        sections.u64(HAS_PATCHED_DEPENDENCIES);
        let keys: Vec<u8> = graph
            .patched_dependencies
            .keys()
            .flat_map(|k| string_hash(k.as_bytes()).to_le_bytes())
            .collect();
        let mut values: Vec<u8> = Vec::new();
        for patch in graph.patched_dependencies.values() {
            let mut raw = [0u8; 24];
            raw[0..8].copy_from_slice(&s.add(&patch.path));
            match patch
                .hash
                .as_deref()
                .and_then(|h| u64::from_str_radix(h, 16).ok())
            {
                Some(h) => raw[16..24].copy_from_slice(&h.to_le_bytes()),
                None => raw[15] = 1,
            }
            values.extend_from_slice(&raw);
        }
        sections.array(&keys, PREFIX_U64);
        sections.array(&values, PREFIX_PATCHED_DEP);
    }
    let catalogs = &graph.catalogs;
    if !catalogs.default.is_empty() || !catalogs.named.is_empty() {
        sections.u64(HAS_CATALOGS);
        let entries = |map: &BTreeMap<String, String>, sections: &mut Sections, s: &mut Strings| {
            let names: Vec<u8> = map.keys().flat_map(|n| s.add(n)).collect();
            let deps: Vec<u8> = map
                .iter()
                .flat_map(|(n, v)| s.dependency(n, Behavior(0), version_tag(v), v))
                .collect();
            sections.array(&names, PREFIX_SEMVER_STRING);
            sections.array(&deps, PREFIX_DEP_EXTERNAL);
        };
        entries(&catalogs.default, &mut sections, &mut s);
        let groups: Vec<u8> = catalogs.named.keys().flat_map(|n| s.add(n)).collect();
        sections.array(&groups, PREFIX_SEMVER_STRING);
        for map in catalogs.named.values() {
            entries(map, &mut sections, &mut s);
        }
    }
    sections.u64(HAS_CONFIG_VERSION);
    sections.u64(u64::from(u8::from(graph.config_version)));
    if !scoped.is_empty() {
        sections.u64(HAS_SCOPED_OVERRIDES);
        let mut parents: Vec<u8> = Vec::new();
        let mut ranges: Vec<u8> = Vec::new();
        let mut values: Vec<u8> = Vec::new();
        for r in &scoped {
            let target = &r.selector.target;
            let parent = r
                .selector
                .parent
                .as_ref()
                .map_or([0u8; DEPENDENCY_SIZE], |p| {
                    s.dependency(&p.name, Behavior(0), version_tag(&p.range), &p.range)
                });
            parents.extend_from_slice(&parent);
            ranges.extend_from_slice(&s.dependency(
                &target.name,
                Behavior(0),
                version_tag(&target.range),
                &target.range,
            ));
            values.extend_from_slice(&s.dependency(
                &target.name,
                Behavior(0),
                version_tag(&r.value),
                &r.value,
            ));
        }
        sections.array(&parents, PREFIX_DEP_EXTERNAL);
        sections.array(&ranges, PREFIX_DEP_EXTERNAL);
        sections.array(&values, PREFIX_DEP_EXTERNAL);
    }

    // The file.
    let mut out = Out(Vec::with_capacity(
        512 + columns.iter().map(Vec::len).sum::<usize>() + deps.len() + s.bytes.len(),
    ));
    out.0.extend_from_slice(HEADER);
    out.u32(FORMAT_CURRENT);
    out.0.extend_from_slice(&crate::hash::meta_hash(graph));
    let total_at = out.0.len();
    out.u64(0);
    out.u64(count as u64);
    out.u64(8);
    out.u64(FIELD_COUNT);
    let begin_at = out.0.len();
    out.u64(0);
    out.u64(0);
    out.align8();
    let begin = out.0.len() as u64;
    for column in &columns {
        out.0.extend_from_slice(column);
    }
    let end = out.0.len() as u64;
    out.patch_u64(begin_at, begin);
    out.patch_u64(begin_at + 8, end);
    out.array(&trees, "\n<install.lockfile.Tree> 20 sizeof, 4 alignof\n");
    out.array(&hoisted_ids, PREFIX_U32);
    out.array(&resolutions, PREFIX_U32);
    out.array(&deps, PREFIX_DEP_EXTERNAL);
    out.array(
        &extern_strings,
        "\n<semver.ExternalString.ExternalString> 16 sizeof, 8 alignof\n",
    );
    out.array(&s.bytes, "\n<u8> 1 sizeof, 1 alignof\n");
    out.u64(0);
    for op in sections.0 {
        match op {
            Op::U64(v) => out.u64(v),
            Op::Array(bytes, prefix) => out.array(&bytes, prefix),
        }
    }
    let total = out.0.len() as u64;
    out.patch_u64(total_at, total);
    out.0.extend_from_slice(&[0u8; 144]);
    Ok(out.0)
}
