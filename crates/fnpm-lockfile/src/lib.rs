//! Lockfiles: `fnpm.lock` read/write, the binary sidecar, the hoisted tree algorithm, and
//! converters from/to bun.lock, bun.lockb, package-lock.json / npm-shrinkwrap.json (v1-v3),
//! yarn.lock v1, yarn berry and pnpm-lock.yaml (lockfileVersion 7-11).
//! See `docs/ARCHITECTURE.md#fnpm-lockfile`.

mod bun_hash;
mod compare;
mod flat;
mod fxhash;
mod json_out;
mod jsonc;
mod semver;
mod text;

pub mod formats;
pub mod hash;
pub mod migration;
pub mod print;
pub mod sidecar;
pub mod tree;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use fnpm_types::{ConfigVersion, LockGraph};
use fnpm_workspace::Workspace;

pub use compare::{LockDiff, diff, out_of_sync};
pub use hash::{meta_hash, meta_hash_input};

/// Any error of this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A lockfile that does not parse. `message` starts with `line:column:` for text formats.
    #[error("{path}: {message}")]
    Parse {
        /// The file (or its name).
        path: PathBuf,
        /// What is wrong, in bun's wording.
        message: String,
    },
    /// The format is read-only (bun.lockb, yarn berry).
    #[error("{format:?} lockfiles cannot be written")]
    Unsupported {
        /// The format that was asked for.
        format: Format,
    },
    /// Reading or writing a file failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// Packages that would need a nested copy of each other forever (bun's
    /// `cannot install dependency cycle into node_modules`).
    #[error("cannot install dependency cycle into node_modules: {0}")]
    DependencyCycle(String),
}

/// The allocator production builds use (fnpm-napi), so timing tests measure the same.
#[cfg(test)]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// This crate's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The lockfile's file name.
pub const FNPM_LOCK: &str = "fnpm.lock";

/// The text lockfile version a freshly resolved graph should carry
/// (`LockGraph::lockfile_version`) so the writer picks it the way bun picks it for a new
/// lockfile: 3 with scoped overrides, else 2, or 1 when a package breaks a v2 invariant.
/// A loaded lockfile keeps its version (v0 is written as v1).
pub const TEXT_LOCKFILE_VERSION_CURRENT: u32 = 3;

/// A lockfile format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// `fnpm.lock`.
    Fnpm,
    /// `bun.lock` (text).
    BunText,
    /// `bun.lockb` (binary, read-only).
    BunBinary,
    /// package-lock.json or npm-shrinkwrap.json.
    Npm,
    /// yarn.lock v1.
    YarnV1,
    /// yarn.lock from yarn 2+ (read-only).
    YarnBerry,
    /// pnpm-lock.yaml.
    Pnpm,
}

impl Format {
    /// File names in load priority order (bun `migration.rs`).
    pub const PRIORITY: [(Format, &'static str); 8] = [
        (Format::Fnpm, "fnpm.lock"),
        (Format::BunText, "bun.lock"),
        (Format::BunBinary, "bun.lockb"),
        (Format::Npm, "npm-shrinkwrap.json"),
        (Format::Npm, "package-lock.json"),
        (Format::YarnV1, "yarn.lock"),
        (Format::YarnBerry, "yarn.lock"),
        (Format::Pnpm, "pnpm-lock.yaml"),
    ];

    /// The usual file name of this format.
    pub fn file_name(self) -> &'static str {
        match self {
            Format::Fnpm => FNPM_LOCK,
            Format::BunText => "bun.lock",
            Format::BunBinary => "bun.lockb",
            Format::Npm => "package-lock.json",
            Format::YarnV1 | Format::YarnBerry => "yarn.lock",
            Format::Pnpm => "pnpm-lock.yaml",
        }
    }

    /// The format of a file by name and, for `yarn.lock`, contents (berry has `__metadata`).
    pub fn detect(file_name: &str, bytes: &[u8]) -> Option<Format> {
        Some(match file_name {
            "fnpm.lock" => Format::Fnpm,
            "bun.lock" => Format::BunText,
            "bun.lockb" => Format::BunBinary,
            "package-lock.json" | "npm-shrinkwrap.json" => Format::Npm,
            "yarn.lock" => {
                if is_yarn_berry(bytes) {
                    Format::YarnBerry
                } else {
                    Format::YarnV1
                }
            }
            "pnpm-lock.yaml" => Format::Pnpm,
            _ => return None,
        })
    }
}

/// yarn berry lockfiles have a `__metadata:` block; v1 never does.
fn is_yarn_berry(bytes: &[u8]) -> bool {
    memchr::memmem::find(bytes, b"\n__metadata:").is_some() || bytes.starts_with(b"__metadata:")
}

/// Registry information the text reader uses for lockfileVersion 2's tarball check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseOptions {
    /// The default registry URL; `None` is `https://registry.npmjs.org/`.
    pub default_registry: Option<String>,
    /// `@scope` (without `@`) to registry URL.
    pub scope_registries: BTreeMap<String, String>,
}

impl ParseOptions {
    fn registry_for(&self, name: &str) -> String {
        let scope = name
            .strip_prefix('@')
            .and_then(|n| n.split_once('/'))
            .map(|(s, _)| s);
        if let Some(url) = scope.and_then(|s| self.scope_registries.get(s)) {
            return url.clone();
        }
        self.default_registry
            .clone()
            .unwrap_or_else(|| text::DEFAULT_REGISTRY.to_owned())
    }
}

/// Reads bun.lock-format text; errors carry `line:column`.
pub(crate) fn text_parse(
    bytes: &[u8],
    file: &str,
    opts: &ParseOptions,
) -> Result<(LockGraph, Vec<String>)> {
    let src = std::str::from_utf8(bytes).map_err(|e| Error::Parse {
        path: file.into(),
        message: format!("invalid UTF-8 at byte {}", e.valid_up_to()),
    })?;
    let registry_for = |name: &str| opts.registry_for(name);
    let popts = text::parse::ParseOptions {
        registry_for: Some(&registry_for),
    };
    let at = |pos: u32, message: &str| {
        let (line, col) = jsonc::line_col(src, pos);
        format!("{line}:{col}: {message}")
    };
    match text::parse::parse(src, popts) {
        Ok(p) => Ok((
            p.graph,
            p.warnings.iter().map(|w| at(w.pos, &w.message)).collect(),
        )),
        Err(e) => Err(Error::Parse {
            path: file.into(),
            message: at(e.pos, &e.message),
        }),
    }
}

/// The result of [`load`].
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The resolved graph.
    pub graph: LockGraph,
    /// The format of the file it came from.
    pub format: Format,
    /// The file it came from.
    pub path: PathBuf,
    /// Not fnpm.lock: the graph came from another tool and the file is left in place.
    pub migrated: bool,
    /// xxh3-64 of the file bytes (sidecar and state-file key).
    pub hash: u64,
    /// Problems that did not stop loading: ignored malformed integrities in old lockfiles,
    /// and higher-priority foreign lockfiles that failed to migrate.
    pub warnings: Vec<String>,
}

/// Options for [`load_with`].
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// `install.lockfile.path`: read this file instead of searching `dir`.
    pub lockfile_path: Option<PathBuf>,
    /// Registry information for the text reader.
    pub parse: ParseOptions,
    /// Skip the sidecar (always parse the text).
    pub no_sidecar: bool,
    /// Where the sidecar is; `None` is `dir/`[`sidecar::SIDECAR`].
    pub sidecar_path: Option<PathBuf>,
}

/// xxh3-64 of lockfile bytes.
pub fn content_hash(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Finds and reads the highest-priority lockfile in `dir`. Uses the sidecar when its key
/// matches the file hash. `yarn.lock` is sniffed for v1 vs berry (`__metadata`).
pub fn load(dir: &Path, workspace: &Workspace) -> Result<Option<Loaded>> {
    load_with(dir, workspace, &LoadOptions::default())
}

/// [`load`] with options. fnpm.lock and bun.lock errors are returned (the install reports
/// `Ignoring lockfile`); a foreign lockfile that fails to migrate is skipped with a warning
/// and the next one is tried, and its error is returned only if none loads.
pub fn load_with(dir: &Path, workspace: &Workspace, opts: &LoadOptions) -> Result<Option<Loaded>> {
    let candidates: Vec<(Option<Format>, PathBuf)> = match &opts.lockfile_path {
        Some(p) => vec![(
            None,
            if p.is_absolute() {
                p.clone()
            } else {
                dir.join(p)
            },
        )],
        None => {
            let mut v: Vec<(Option<Format>, PathBuf)> = Vec::new();
            for (format, name) in Format::PRIORITY {
                if format == Format::YarnBerry {
                    continue;
                }
                v.push((Some(format), dir.join(name)));
            }
            v
        }
    };
    let mut warnings = Vec::new();
    let mut first_error: Option<Error> = None;
    for (format, path) in candidates {
        let Some(bytes) = read(&path)? else { continue };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let format = match format {
            Some(Format::YarnV1) => Format::detect("yarn.lock", &bytes).unwrap_or(Format::YarnV1),
            Some(f) => f,
            None => Format::detect(name, &bytes).unwrap_or(Format::Fnpm),
        };
        let hash = content_hash(&bytes);
        let migrated = format != Format::Fnpm;
        if format == Format::Fnpm
            && !opts.no_sidecar
            && let Some(graph) = match &opts.sidecar_path {
                Some(p) => sidecar::load_at(p, hash),
                None => sidecar::load(dir, hash),
            }
        {
            return Ok(Some(Loaded {
                graph,
                format,
                path,
                migrated,
                hash,
                warnings,
            }));
        }
        let parsed = match format {
            Format::Fnpm | Format::BunText => text_parse(&bytes, name, &opts.parse),
            f => parse(f, &bytes, workspace).map(|g| (g, Vec::new())),
        };
        match parsed {
            Ok((mut graph, w)) => {
                warnings.extend(w);
                match format {
                    Format::Npm | Format::YarnV1 | Format::YarnBerry => {
                        graph.config_version = ConfigVersion::V0
                    }
                    Format::Pnpm => graph.config_version = ConfigVersion::V1,
                    _ => {}
                }
                return Ok(Some(Loaded {
                    graph,
                    format,
                    path,
                    migrated,
                    hash,
                    warnings,
                }));
            }
            Err(e) if matches!(format, Format::Fnpm | Format::BunText | Format::BunBinary) => {
                return Err(e);
            }
            Err(e) => {
                warnings.push(format!("failed to migrate lockfile from {name}: {e}"));
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// Parses a lockfile of a known format. Converters keep resolved versions exactly; data a
/// format lacks (bin, os/cpu, hasInstallScript) is left default and filled from the store's
/// package.json during install, with no manifest fetch.
pub fn parse(format: Format, bytes: &[u8], workspace: &Workspace) -> Result<LockGraph> {
    match format {
        Format::Fnpm => formats::fnpm::parse(bytes),
        // The shared text reader that `load` uses (bun.lock and fnpm.lock are one format).
        Format::BunText => text_parse(bytes, "bun.lock", &ParseOptions::default()).map(|(g, _)| g),
        Format::BunBinary => formats::bun_binary::parse(bytes),
        Format::Npm => formats::npm::parse(bytes, workspace),
        Format::YarnV1 => formats::yarn_v1::parse(bytes, workspace),
        Format::YarnBerry => formats::yarn_berry::parse(bytes, workspace),
        Format::Pnpm => formats::pnpm::parse(bytes, workspace),
    }
}

/// Serialises to a format (`fnpm-lock export`). bun.lockb and yarn berry are read-only.
pub fn serialize(format: Format, graph: &LockGraph) -> Result<Vec<u8>> {
    match format {
        Format::Fnpm => Ok(formats::fnpm::serialize(graph).into_bytes()),
        // bun's dialect of the shared writer: byte-identical with every bun.lock in bun's test
        // snapshots (`text::roundtrip_tests`).
        Format::BunText => Ok(text::write::write(graph, text::Dialect::Bun)
            .text
            .into_bytes()),
        Format::Npm => formats::npm::serialize(graph).map(String::into_bytes),
        Format::YarnV1 => Ok(formats::yarn_v1::serialize(graph).into_bytes()),
        Format::Pnpm => formats::pnpm::serialize(graph).map(String::into_bytes),
        Format::BunBinary | Format::YarnBerry => Err(Error::Unsupported { format }),
    }
}

/// Writes `fnpm.lock` atomically (temp + rename) and refreshes the sidecar. Returns the new
/// file hash. Skips the write when the bytes are unchanged.
pub fn save(dir: &Path, graph: &LockGraph) -> Result<u64> {
    save_prepared(dir, &prepare_save(graph)?)
}

/// What [`save`] writes, computed without touching the disk (so it can run beside other work).
#[derive(Debug, Clone)]
pub struct PreparedSave {
    text: String,
    hash: u64,
    /// The sidecar of the text, `None` if it could not be built (it is a cache).
    sidecar: Option<Vec<u8>>,
}

impl PreparedSave {
    /// The hash of the lockfile text ([`content_hash`]).
    pub fn hash(&self) -> u64 {
        self.hash
    }
}

/// Serialises `graph` and builds the sidecar of the result. The sidecar caches what loading
/// the file gives (bindings and hoisting re-derived the way a reload derives them), so it is
/// built from a parse of the text, never from `graph` itself: a warm load and a cold one
/// never differ.
pub fn prepare_save(graph: &LockGraph) -> Result<PreparedSave> {
    let text = formats::fnpm::try_serialize(graph)?;
    let hash = content_hash(text.as_bytes());
    let sidecar = text_parse(text.as_bytes(), FNPM_LOCK, &ParseOptions::default())
        .ok()
        .and_then(|(loaded, _)| sidecar::encode(hash, &loaded).ok());
    Ok(PreparedSave {
        text,
        hash,
        sidecar,
    })
}

/// Writes a [`prepare_save`] result: fnpm.lock (atomic, skipped when the bytes are unchanged)
/// and the sidecar (when it is not fresh). A sidecar failure never fails the save.
pub fn save_prepared(dir: &Path, prepared: &PreparedSave) -> Result<u64> {
    save_prepared_with(dir, prepared, Some(&dir.join(sidecar::SIDECAR)))
}

/// [`save`] with the sidecar at `sidecar_path` (`None`: no sidecar).
pub fn save_with(dir: &Path, graph: &LockGraph, sidecar_path: Option<&Path>) -> Result<u64> {
    save_prepared_with(dir, &prepare_save(graph)?, sidecar_path)
}

/// Writes a prepared lockfile with an explicitly located sidecar cache.
pub fn save_prepared_with(
    dir: &Path,
    prepared: &PreparedSave,
    sidecar_path: Option<&Path>,
) -> Result<u64> {
    let PreparedSave {
        text,
        hash,
        sidecar,
    } = prepared;
    let path = dir.join(FNPM_LOCK);
    let unchanged = read(&path)?.is_some_and(|old| old == text.as_bytes());
    if !unchanged {
        write_atomic(&path, text.as_bytes())?;
    }
    if let Some(sidecar_path) = sidecar_path
        && (!unchanged || !sidecar::is_fresh_at(sidecar_path, *hash))
        && let Some(bytes) = sidecar
    {
        let _ = sidecar::write_at(sidecar_path, bytes);
    }
    Ok(*hash)
}

/// Writes `yarn.lock` v1 next to the lockfile (`--yarn`, `install.lockfile.print = "yarn"`),
/// skipping the write when the bytes are unchanged.
pub fn save_yarn(dir: &Path, graph: &LockGraph, opts: &print::YarnOptions) -> Result<()> {
    let text = print::yarn_v1(graph, opts);
    let path = dir.join("yarn.lock");
    if read(&path)?.is_some_and(|old| old == text.as_bytes()) {
        return Ok(());
    }
    write_atomic(&path, text.as_bytes())
}

/// Writes `bytes` to `path` through a temporary file in the same directory and a rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lockfile");
    let tmp = dir.join(format!(
        ".{name}.{}-{:x}.tmp",
        std::process::id(),
        content_hash(bytes) & 0xffff_ffff
    ));
    let io = |source| Error::Io {
        path: tmp.clone(),
        source,
    };
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o666);
        }
        let mut f = opts.open(&tmp).map_err(io)?;
        f.write_all(bytes).map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        Error::Io {
            path: path.to_owned(),
            source,
        }
    })
}

#[cfg(test)]
mod lib_tests;

#[cfg(test)]
mod fuzz_tests;
