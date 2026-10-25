//! `fnpm pm pack` / `fnpm pack` (bun `runtime/cli/pack_command.rs`), and the packing half of
//! `fnpm publish`. The output streams to stdout and stderr in order with the lifecycle
//! scripts' own output, as bun prints it (see `out`).

pub(crate) mod bundle;
pub(crate) mod glob;
pub(crate) mod json;
pub(crate) mod manifests;
pub(crate) mod out;
pub(crate) mod tarball;
pub(crate) mod walk;

use std::path::{Path, PathBuf};

use fnpm_config::LogLevel;

use self::json::Json;
use self::out::{FResult, Fatal, Out, fmt_size, show};
use self::walk::{Walker, os_bytes};
use super::Ctx;
use crate::{CoreResult, Result};

/// What `pack` hands to publish.
pub(crate) struct Packed {
    pub name: String,
    pub version: String,
    /// The gzipped tarball (empty for a dry run).
    pub tarball: Vec<u8>,
    pub shasum: [u8; 20],
    pub integrity: [u8; 64],
    /// The packed package.json, specs replaced.
    pub manifest: Json,
    /// The tarball's paths below `package/`.
    pub files: Vec<String>,
    /// The first README of the package directory: file name and contents.
    pub readme: Option<(String, String)>,
    pub publish_script: Option<String>,
    pub postpublish_script: Option<String>,
    pub package_dir: PathBuf,
}

/// Publish's checks, run by `pack` where bun runs them.
pub(crate) trait PublishHooks: Send {
    /// After package.json is read, and again after the scripts re-read it: `publishConfig`,
    /// the provenance, restricted-access and private-package refusals.
    fn apply(&mut self, out: &Out, json: &Json, name: &str) -> FResult<()>;
    /// After the scripts: the tag must not be a version, a prerelease needs a tag.
    fn refuse_tag(&mut self, out: &Out, version: &str) -> FResult<()>;
}

pub(crate) struct PackOptions {
    pub dry_run: bool,
    pub ignore_scripts: bool,
    pub log_level: LogLevel,
    /// Absolute, or empty.
    pub destination: Vec<u8>,
    /// Absolute, or empty.
    pub filename: Vec<u8>,
    pub gzip_level: Option<String>,
}

impl PackOptions {
    fn quiet(&self) -> bool {
        matches!(self.log_level, LogLevel::Silent | LogLevel::Quiet)
    }
}

#[derive(Default)]
struct Stats {
    unpacked_size: u64,
    total_files: usize,
    packed_size: u64,
    bundled_deps: usize,
}

/// The package.json `fnpm pack` packs: the nearest one from `cwd` up.
pub(crate) fn find_package_json(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .map(|dir| dir.join("package.json"))
        .find(|p| p.is_file())
}

/// The header line, `fnpm <command> v<version>`.
pub(crate) fn header(cx: &Ctx, out: &Out, command: &str) {
    let version = cx
        .string("cliVersion")
        .unwrap_or_else(|| crate::version().to_owned());
    out.println(&format!(
        "{}{}",
        out.paint("1", &format!("fnpm {command} ")),
        out.paint("2", &format!("v{version}"))
    ));
}

/// `fnpm pm pack`.
pub(crate) async fn run(cx: &Ctx) -> Result<CoreResult> {
    let out = Out::new();
    let code = match pm_pack(cx, &out).await {
        Ok(()) => 0,
        Err(Exit::Fatal(f)) => {
            out.eprintln(&f.0);
            1
        }
        Err(Exit::Code(code)) => code,
    };
    Ok(CoreResult {
        exit_code: code,
        data: None,
        lines: None,
    })
}

/// How a command that prints as it goes ends early.
pub(crate) enum Exit {
    /// Print this and exit 1.
    Fatal(Fatal),
    /// A script failed; it has printed why.
    Code(i32),
}

impl From<Fatal> for Exit {
    fn from(f: Fatal) -> Self {
        Exit::Fatal(f)
    }
}

pub(crate) type XResult<T> = std::result::Result<T, Exit>;

pub(crate) fn no_package_json(out: &Out, cwd: &Path) -> Fatal {
    Fatal(format!(
        "{}\n{} Run \"fnpm init\" to initialize a project",
        out.error(&format!(
            "No package.json was found for directory \"{}\"",
            cwd.display()
        ))
        .0,
        out.paint_err("34", "note:")
    ))
}

async fn pm_pack(cx: &Ctx, out: &Out) -> XResult<()> {
    let cwd = cx.req.cwd.clone();
    let Some(package_json) = find_package_json(&cwd) else {
        return Err(no_package_json(out, &cwd).into());
    };
    let log_level = cx.pm.log_level;
    if !matches!(log_level, LogLevel::Silent | LogLevel::Quiet) {
        header(cx, out, "pack");
    }
    let filename = cx.string("filename").unwrap_or_default();
    let destination = cx.string("destination").unwrap_or_default();
    let trim = |s: &str| s.trim_end_matches('/').to_owned();
    if !filename.is_empty() && !destination.is_empty() {
        return Err(out
            .error(&format!(
                "cannot use both filename and destination at the same time with tarball: filename \"{}\" and destination \"{}\"",
                trim(&filename),
                trim(&destination)
            ))
            .into());
    }
    let cwd_bytes = os_bytes(cwd.as_os_str());
    let absolute = |option: &str, what: &str| -> FResult<Vec<u8>> {
        if option.is_empty() {
            return Ok(Vec::new());
        }
        tarball::join_abs(&cwd_bytes, option.as_bytes())
            .ok_or_else(|| out.error(&format!("archive {what} too long: \"{}\"", trim(option))))
    };
    let options = PackOptions {
        dry_run: cx.pm.dry_run,
        ignore_scripts: ignore_scripts(cx),
        log_level,
        filename: absolute(&filename, "filename")?,
        destination: absolute(&destination, "destination name")?,
        gzip_level: cx.string("gzipLevel"),
    };
    pack(cx, out, &options, &package_json, None).await?;
    Ok(())
}

pub(crate) fn ignore_scripts(cx: &Ctx) -> bool {
    cx.pm.ignore_scripts
        || cx
            .project_anywhere()
            .map(|p| p.config.ignore_scripts)
            .unwrap_or(false)
}

/// Reads and parses a package.json the way bun's pack does (exits on failure).
pub(crate) fn read_package_json(out: &Out, path: &Path) -> FResult<(String, Json)> {
    let text = std::fs::read(path).map_err(|e| {
        out.named(
            &out::errno_name(&e),
            &format!("failed to read package.json: {}", path.display()),
        )
    })?;
    let text = String::from_utf8_lossy(&text).into_owned();
    match json::parse(&text) {
        Ok(json) => Ok((text, json)),
        Err(e) => Err(Fatal(format!(
            "{}\n{} {}\n    at {}:{}:{}",
            out.named(
                "ParserError",
                &format!("failed to parse package.json: {}", path.display())
            )
            .0,
            out.paint_err("31", "error:"),
            e.message,
            path.display(),
            e.line,
            e.column
        ))),
    }
}

fn has_unsafe_tarball_filename_part(value: &str) -> bool {
    value.split('/').any(|c| c == "." || c == "..")
        || value.bytes().any(|b| matches!(b, b'\\' | b':' | 0))
}

/// Why the name or version cannot be packed.
pub(crate) enum NameError {
    MissingName,
    InvalidName,
    MissingVersion,
    InvalidVersion,
}

pub(crate) fn name_and_version(json: &Json) -> std::result::Result<(String, String), NameError> {
    let field = |key: &str, missing: NameError, invalid: NameError| {
        let value = json.get(key).ok_or(missing)?;
        let value = value.as_str().ok_or(invalid)?;
        if value.is_empty() || has_unsafe_tarball_filename_part(value) {
            let invalid = match key {
                "name" => NameError::InvalidName,
                _ => NameError::InvalidVersion,
            };
            return Err(invalid);
        }
        Ok(value.to_owned())
    };
    let name = field("name", NameError::MissingName, NameError::InvalidName)?;
    let version = field(
        "version",
        NameError::MissingVersion,
        NameError::InvalidVersion,
    )?;
    Ok((name, version))
}

fn pack_name_error(out: &Out, e: NameError) -> Fatal {
    match e {
        NameError::MissingName | NameError::MissingVersion => {
            out.error("package.json must have `name` and `version` fields")
        }
        NameError::InvalidName | NameError::InvalidVersion => {
            out.error("package.json `name` and `version` fields must be non-empty strings")
        }
    }
}

pub(crate) fn publish_name_error(out: &Out, e: NameError) -> Fatal {
    out.error(match e {
        NameError::MissingName => "missing `name` string in package.json",
        NameError::MissingVersion => "missing `version` string in package.json",
        NameError::InvalidName => {
            "package.json `name` and `version` fields must be non-empty strings"
        }
        NameError::InvalidVersion => "package.json `version` is not a valid semver version",
    })
}

/// `version` as the registry gets it (`v1.0.0` is `1.0.0`); `None` unless it is one complete
/// semver version.
pub(crate) fn published_version(version: &str) -> Option<String> {
    let parsed = fnpm_semver::Version::parse(version).ok()?;
    let printed = parsed.to_string();
    let trimmed = version.trim();
    let prefix = trimmed.strip_suffix(printed.as_str())?;
    (!prefix.bytes().any(|b| b.is_ascii_digit())).then_some(printed)
}

/// The scripts of `json` by name.
fn script<'a>(json: &'a Json, name: &str) -> Option<&'a str> {
    json.get("scripts")?.get(name)?.as_str()
}

/// Runs lifecycle script `name` of the package in `dir` in the foreground.
pub(crate) async fn run_lifecycle(
    cx: &Ctx,
    dir: &Path,
    project_dir: &Path,
    name: &str,
    command: &str,
    env: &[(String, String)],
    silent: bool,
) -> XResult<()> {
    let mut opts = super::scripts::run_options(cx, project_dir);
    opts.silent = silent;
    opts.env.extend(env.iter().cloned());
    match fnpm_scripts::run::run_lifecycle_script_foreground(dir, name, command, &opts).await {
        Ok(0) => Ok(()),
        Ok(code) => {
            if code != 2 && !silent {
                eprintln!("{}", fnpm_scripts::run::exit_message(name, code));
            }
            Err(Exit::Code(code))
        }
        Err(fnpm_scripts::Error::Signaled { signal, .. }) => Err(Exit::Code(128 + signal)),
        Err(fnpm_scripts::Error::Spawn { .. }) => Err(Exit::Fatal(Fatal(format!(
            "error: failed to find shell executable to run {name} script"
        )))),
        Err(e) => Err(Exit::Fatal(
            Out::new().error(&format!("failed to run {name} script: {e}")),
        )),
    }
}

fn print_summary(
    out: &Out,
    options: &PackOptions,
    stats: &Stats,
    shasum: Option<&[u8; 20]>,
    integrity: Option<&[u8; 64]>,
) {
    if options.quiet() {
        return;
    }
    let label = |s: &str| out.paint("1;34", s);
    let mut text = format!("\n{}: {}\n", label("Total files"), stats.total_files);
    if let Some(shasum) = shasum {
        text.push_str(&format!("{}: {}\n", label("Shasum"), hex::encode(shasum)));
    }
    if let Some(integrity) = integrity {
        text.push_str(&format!(
            "{}: {}\n",
            label("Integrity"),
            short_integrity(integrity)
        ));
    }
    text.push_str(&format!(
        "{}: {}\n",
        label("Unpacked size"),
        fmt_size(stats.unpacked_size)
    ));
    if stats.packed_size > 0 {
        text.push_str(&format!(
            "{}: {}\n",
            label("Packed size"),
            fmt_size(stats.packed_size)
        ));
    }
    if stats.bundled_deps > 0 {
        text.push_str(&format!(
            "{}: {}\n",
            label("Bundled deps"),
            stats.bundled_deps
        ));
    }
    out.print(&text);
}

pub(crate) fn integrity_sri(integrity: &[u8; 64]) -> String {
    use base64::Engine as _;
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(integrity)
    )
}

fn short_integrity(integrity: &[u8; 64]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(integrity);
    format!(
        "sha512-{}[...]{}",
        &encoded[..13],
        &encoded[encoded.len() - 15..]
    )
}

fn print_tarball_path(out: &Out, options: &PackOptions, path: &str) {
    let mut text = String::new();
    if !options.quiet() {
        text.push('\n');
    }
    text.push_str(&out::escape_control(path));
    text.push('\n');
    out.print(&text);
}

fn print_packed_list(
    out: &Out,
    options: &PackOptions,
    rows: &[(u64, Vec<u8>)],
    bundled: &[String],
) {
    if options.quiet() {
        return;
    }
    let mut text = String::from("\n");
    for (size, path) in rows {
        text.push_str(&format!(
            "{} {} {}\n",
            out.paint("1;36", "packed"),
            fmt_size(*size),
            show(path)
        ));
    }
    for name in bundled {
        text.push_str(&format!(
            "{} {}\n",
            out.paint("1;32", "bundled"),
            out::escape_control(name)
        ));
    }
    out.print(&text);
}

fn file_mode(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

/// Packs the package of `package_json`. With `publish`, the tarball stays in memory and the
/// publish checks run; otherwise it is written as `fnpm pm pack` writes it.
pub(crate) async fn pack(
    cx: &Ctx,
    out: &Out,
    options: &PackOptions,
    package_json: &Path,
    mut publish: Option<&mut dyn PublishHooks>,
) -> XResult<Packed> {
    let for_publish = publish.is_some();
    let name_error = |e| {
        if for_publish {
            publish_name_error(out, e)
        } else {
            pack_name_error(out, e)
        }
    };
    let (mut text, mut json) = read_package_json(out, package_json)?;
    let (mut name, mut version) = name_and_version(&json).map_err(name_error)?;
    if let Some(hooks) = publish.as_deref_mut() {
        hooks.apply(out, &json, &name)?;
    }
    let package_dir = package_json
        .parent()
        .unwrap_or(Path::new("/"))
        .to_path_buf();
    let project_dir =
        fnpm_workspace::Workspace::find_root(&package_dir).unwrap_or_else(|_| package_dir.clone());
    let silent = options.log_level == LogLevel::Silent;
    let script_env = |name: &str, version: &str| {
        vec![
            ("npm_command".to_owned(), "pack".to_owned()),
            (
                "npm_package_json".to_owned(),
                package_json.to_string_lossy().into_owned(),
            ),
            ("npm_package_name".to_owned(), name.to_owned()),
            ("npm_package_version".to_owned(), version.to_owned()),
        ]
    };

    let mut ran_scripts = false;
    let mut postpack = None;
    let mut publish_script = None;
    let mut postpublish_script = None;
    if !options.ignore_scripts && matches!(json.get("scripts"), Some(Json::Object(_))) {
        let before: &[&str] = if for_publish {
            &["prepublishOnly", "prepack", "prepare"]
        } else {
            &["prepack", "prepare"]
        };
        let env = script_env(&name, &version);
        for &hook in before {
            if let Some(command) = script(&json, hook) {
                ran_scripts = true;
                run_lifecycle(cx, &package_dir, &project_dir, hook, command, &env, silent).await?;
            }
        }
        postpack = script(&json, "postpack").map(str::to_owned);
        if for_publish {
            publish_script = script(&json, "publish").map(str::to_owned);
            postpublish_script = script(&json, "postpublish").map(str::to_owned);
        }
    }

    // The scripts may have edited any package.json of the workspace.
    if ran_scripts {
        (text, json) = read_package_json(out, package_json)?;
        (name, version) = name_and_version(&json).map_err(name_error)?;
        if let Some(hooks) = publish.as_deref_mut() {
            hooks.apply(out, &json, &name)?;
        }
    }
    if let Some(hooks) = publish.as_deref_mut() {
        version =
            published_version(&version).ok_or_else(|| name_error(NameError::InvalidVersion))?;
        hooks.refuse_tag(out, &version)?;
    }

    let bundled_names = bundle::get_bundled_deps(out, &json)?;
    let mut workspace_manifests = if manifests::needs_workspace_manifests(&json) {
        Some(manifests::WorkspaceManifests::load(out, &project_dir)?)
    } else {
        None
    };

    // bun's resolver lists the package root once before the walk; the README scan of a publish
    // reads that listing.
    let root_entries = match walk::list_dir(&package_dir) {
        Ok(entries) => entries,
        Err(walk::ListError::Open(e)) => {
            return Err(out
                .named(
                    &out::errno_name(&e),
                    &format!("failed to open root directory: {}", package_dir.display()),
                )
                .into());
        }
        Err(walk::ListError::Read(e, syscall)) => {
            return Err(out
                .sys(&e, syscall, "failed to read directory \".\" for packing")
                .into());
        }
    };
    let readme = if for_publish {
        find_readme(&package_dir, &root_entries)
    } else {
        None
    };

    let walker = Walker {
        out,
        verbose: options.log_level == LogLevel::Verbose,
    };
    let (queue, bins) = walker.published_files(&package_dir, &json)?;

    let root_real = std::fs::canonicalize(&package_dir).unwrap_or_else(|_| package_dir.clone());
    let project_real = std::fs::canonicalize(&project_dir).unwrap_or_else(|_| project_dir.clone());
    let bundled = bundle::iterate_bundled_deps(bundle::BundleInput {
        walker: &walker,
        names: bundled_names,
        package_json: &json,
        root_path: &root_real,
        project_path: &project_real,
        manifests: &mut workspace_manifests,
        project_dir: &project_dir,
    })?;

    let mut manifest = json.clone();
    manifests::substitute(
        out,
        workspace_manifests.as_ref(),
        &package_dir,
        &mut manifest,
    )?;
    let edited = json::print(
        &manifest,
        json::guess_indentation(text.as_bytes()),
        text.ends_with('\n'),
    );

    let mut stats = Stats {
        total_files: queue.len() + bundled.files.len() + 1,
        bundled_deps: bundled.count,
        ..Default::default()
    };
    let prints_bare = options.destination.is_empty() && options.filename.is_empty();
    let bare_filename = tarball::tarball_name(&name, &version);
    let workspace_bytes = os_bytes(package_dir.as_os_str());
    let mut files: Vec<String> = Vec::with_capacity(stats.total_files);
    files.push("package.json".to_owned());
    files.extend(
        queue
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned()),
    );
    files.extend(
        bundled
            .files
            .iter()
            .map(|f| String::from_utf8_lossy(&f.path).into_owned()),
    );

    let package_json_meta = std::fs::metadata(package_json)
        .map_err(|e| out.named(&out::errno_name(&e), "failed to stat package.json"))?;

    if options.dry_run {
        let mut rows = vec![(package_json_meta.len(), b"package.json".to_vec())];
        stats.unpacked_size += package_json_meta.len();
        for item in &queue {
            let meta =
                std::fs::metadata(package_dir.join(walk::bytes_path(item))).map_err(|e| {
                    out.sys(
                        &e,
                        "stat",
                        &format!("failed to stat file: \"{}\"", show(item)),
                    )
                })?;
            stats.unpacked_size += meta.len();
            rows.push((meta.len(), item.clone()));
        }
        print_packed_list(out, options, &rows, &bundled.names);
        if !for_publish {
            if prints_bare {
                print_tarball_path(out, options, &bare_filename);
            } else {
                let (dest, _) = tarball::tarball_destination(
                    out,
                    &options.destination,
                    &options.filename,
                    &workspace_bytes,
                    &bare_filename,
                )?;
                print_tarball_path(out, options, &String::from_utf8_lossy(&dest));
            }
        }
        print_summary(out, options, &stats, None, None);
        if let Some(command) = &postpack {
            run_lifecycle(
                cx,
                &package_dir,
                &project_dir,
                "postpack",
                command,
                &script_env(&name, &version),
                silent,
            )
            .await?;
        }
        return Ok(Packed {
            name,
            version,
            tarball: Vec::new(),
            shasum: [0; 20],
            integrity: [0; 64],
            manifest,
            files,
            readme,
            publish_script,
            postpublish_script,
            package_dir,
        });
    }

    let level = options.gzip_level.clone().unwrap_or_else(|| "9".to_owned());
    let level_num = match level.as_bytes() {
        [d @ b'0'..=b'9'] => u32::from(d - b'0'),
        _ => {
            return Err(out
                .error(&format!(
                    "compression level must be between 0 and 9, received {level}"
                ))
                .into());
        }
    };

    let dest: Option<PathBuf> = if for_publish {
        None
    } else {
        let (dest, dir_end) = tarball::tarball_destination(
            out,
            &options.destination,
            &options.filename,
            &workspace_bytes,
            &bare_filename,
        )?;
        if dir_end > 0 {
            let _ = std::fs::create_dir_all(walk::bytes_path(&dest[..dir_end]));
        }
        Some(walk::bytes_path(&dest))
    };
    let typed_filename = !options.filename.is_empty();
    let opened = dest
        .as_deref()
        .and_then(|d| tarball::open_tarball(d, typed_filename));
    let (sink, temp) = match opened {
        Some((file, temp)) => (tarball::Sink::File(file), Some(temp)),
        None => (tarball::Sink::Memory(Vec::new()), None),
    };
    tarball::ignore_sigxfsz();
    let mut writer = tarball::TarWriter::new(sink, level_num);
    let shown_dest = || {
        dest.clone()
            .unwrap_or_else(|| PathBuf::from(format!("{name}-{version}.tgz")))
    };
    let entry_error = |e: tarball::EntryError, path: &[u8]| -> Fatal {
        match e {
            tarball::EntryError::Write(e) => tarball::fail_write_tarball(out, &e, &shown_dest()),
            tarball::EntryError::Read(e, syscall) => out.sys(
                &e,
                syscall,
                &format!("failed to read file: \"{}\"", show(path)),
            ),
            tarball::EntryError::Shrank => {
                out.error(&format!("file shrank while packing: \"{}\"", show(path)))
            }
        }
    };

    writer
        .add_bytes(
            b"package.json",
            file_mode(&package_json_meta),
            edited.as_bytes(),
        )
        .map_err(|e| entry_error(e, b"package.json"))?;
    stats.unpacked_size += edited.len() as u64;

    let mut rows: Vec<(u64, Vec<u8>)> = vec![(edited.len() as u64, b"package.json".to_vec())];
    let mut buf = vec![0u8; 256 * 1024];
    for item in &queue {
        let path = package_dir.join(walk::bytes_path(item));
        let mut file = std::fs::File::open(&path).map_err(|e| {
            out.sys(
                &e,
                "open",
                &format!("failed to open file: \"{}\"", show(item)),
            )
        })?;
        let meta = file.metadata().map_err(|e| {
            out.sys(
                &e,
                "fstat",
                &format!("failed to stat file: \"{}\"", show(item)),
            )
        })?;
        rows.push((meta.len(), item.clone()));
        let mut mode = file_mode(&meta);
        if walk::is_package_bin(&bins, item) {
            mode |= 0o111;
        }
        let written = writer
            .add_file(item, mode, meta.len(), &mut file, &mut buf)
            .map_err(|e| entry_error(e, item))?;
        stats.unpacked_size += written;
    }
    for item in &bundled.files {
        let mut file = std::fs::File::open(&item.source).map_err(|e| {
            out.sys(
                &e,
                "open",
                &format!("failed to open file: \"{}\"", show(&item.path)),
            )
        })?;
        let meta = file.metadata().map_err(|e| {
            out.sys(
                &e,
                "fstat",
                &format!("failed to stat file: \"{}\"", show(&item.path)),
            )
        })?;
        let mut mode = file_mode(&meta);
        if walk::is_package_bin(&bins, &item.path) {
            mode |= 0o111;
        }
        match &item.edited {
            Some(edited) => {
                writer
                    .add_bytes(&item.path, mode, edited)
                    .map_err(|e| entry_error(e, &item.path))?;
                stats.unpacked_size += edited.len() as u64;
            }
            None => {
                let written = writer
                    .add_file(&item.path, mode, meta.len(), &mut file, &mut buf)
                    .map_err(|e| entry_error(e, &item.path))?;
                stats.unpacked_size += written;
            }
        }
    }
    let finished = writer
        .finish()
        .map_err(|e| tarball::fail_write_tarball(out, &e, &shown_dest()))?;
    stats.packed_size = finished.packed_size;

    let mut tarball_bytes = Vec::new();
    match (finished.sink, temp, &dest) {
        (tarball::Sink::File(file), Some(temp), Some(dest)) => {
            tarball::finish_tarball(out, file, temp, dest, typed_filename)?;
        }
        (tarball::Sink::Memory(bytes), _, Some(dest)) => {
            tarball::write_tarball_in_place(out, dest, &bytes, typed_filename)?;
        }
        (tarball::Sink::Memory(bytes), _, None) => tarball_bytes = bytes,
        _ => {}
    }

    print_packed_list(out, options, &rows, &bundled.names);
    if let Some(dest) = &dest {
        if prints_bare {
            print_tarball_path(out, options, &bare_filename);
        } else {
            print_tarball_path(out, options, &dest.to_string_lossy());
        }
    }
    print_summary(
        out,
        options,
        &stats,
        Some(&finished.shasum),
        Some(&finished.integrity),
    );

    if let Some(command) = &postpack {
        out.print("\n");
        run_lifecycle(
            cx,
            &package_dir,
            &project_dir,
            "postpack",
            command,
            &script_env(&name, &version),
            silent,
        )
        .await?;
    }

    Ok(Packed {
        name,
        version,
        tarball: tarball_bytes,
        shasum: finished.shasum,
        integrity: finished.integrity,
        manifest,
        files,
        readme,
        publish_script,
        postpublish_script,
        package_dir,
    })
}

/// npm's `{README,README.*}`, case-insensitively: the first regular file `readdir` returns.
pub(crate) fn is_readme_filename(name: &str) -> bool {
    name.len() >= 6
        && name[..6].eq_ignore_ascii_case("README")
        && (name.len() == 6 || name.as_bytes()[6] == b'.')
}

fn find_readme(dir: &Path, entries: &[walk::Entry]) -> Option<(String, String)> {
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        if entry.kind == walk::Kind::File && is_readme_filename(&name) {
            let contents = std::fs::read(dir.join(walk::bytes_path(&entry.name))).ok()?;
            return Some((name, String::from_utf8_lossy(&contents).into_owned()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(published_version("v1.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(
            published_version(" 1.0.0-beta.1 ").as_deref(),
            Some("1.0.0-beta.1")
        );
        assert_eq!(published_version("01.0.0"), None);
        assert_eq!(published_version("1.0"), None);
        assert_eq!(published_version("1.0.0+b").as_deref(), Some("1.0.0+b"));
    }

    #[test]
    fn names() {
        let ok = json::parse(r#"{"name":"a","version":"1.0.0"}"#).unwrap();
        assert!(name_and_version(&ok).is_ok());
        let bad = json::parse(r#"{"name":"../../x","version":"1.0.0"}"#).unwrap();
        assert!(matches!(
            name_and_version(&bad),
            Err(NameError::InvalidName)
        ));
        let missing = json::parse(r#"{"version":"1.0.0"}"#).unwrap();
        assert!(matches!(
            name_and_version(&missing),
            Err(NameError::MissingName)
        ));
        assert!(is_readme_filename("readme.md") && !is_readme_filename("READMEX"));
    }
}
