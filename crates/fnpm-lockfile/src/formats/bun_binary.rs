//! `bun.lockb`, bun's binary lockfile (a port of the reader in bun
//! `install/lockfile/bun.lockb.rs`, `Buffers.rs` and `Package.rs` `serializer::load`).
//! Read-only: fnpm migrates from it and never writes it.
//!
//! Layout (little-endian, 64-bit): a header, the format version (2 or 3), a meta hash, the
//! package table as one column per field (name, name hash, resolution, dependency slice,
//! resolution slice, meta, bin, scripts), six length-prefixed arrays (trees, hoisted
//! dependencies, resolutions, dependencies, external strings, string bytes) and optional
//! tagged sections (workspace versions, trusted dependencies, overrides, patches, catalogs,
//! configVersion, scoped overrides, package extensions). Strings are bun's 8-byte
//! `semver.String`: inline when short, else an offset/length into the string bytes.
//!
//! Dependencies are bound by the stored resolution ids, so the hoisted tree is not needed.

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
    let _trees = s.array(20, 1)?;
    let _hoisted = s.array(4, 4)?;
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
    Ok(graph)
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
            // Only the truncated hashes are stored; names come from the installed packages.
            let names = trust_candidates(graph);
            graph.trusted_dependencies = Some(
                names
                    .into_iter()
                    .filter(|n| hashes.contains(&(string_hash(n.as_bytes()) as u32)))
                    .collect(),
            );
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
