//! The security scanner (bun `install/PackageManager/security_scanner.rs` and
//! `scanner-entry.ts`, docs `pm/security-scanner-api.mdx`).
//!
//! `[install.security] scanner = "<package or path>"` names a module that exports
//! `scanner = { version: "1", scan({ packages }) }`. After resolution and before anything is
//! linked, every package that the install brings in is sent to it as
//! `{name, version, requestedRange, tarball}` (credentials stripped), in a node process running
//! `scanner-entry.mjs`. Its advisories are printed with the dependency path that reaches the
//! package; a fatal one aborts the install, warnings ask `[y/N]` on a terminal and cancel
//! otherwise. `fnpm pm scan` runs it over the whole lockfile.

use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(unix)]
use std::io::Read;
use std::io::{IsTerminal, Write};
use std::path::Path;
#[cfg(not(unix))]
use std::path::PathBuf;
use std::time::Instant;

use fnpm_config::{Config, LogLevel};
use fnpm_types::{LockGraph, PackageId, Resolution};
use serde_json::Value;

use crate::{Error, Result};

const ENTRY: &str = include_str!("scanner-entry.mjs");

/// How bad an advisory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Aborts the install.
    Fatal,
    /// Asks before going on.
    Warn,
}

/// One advisory of the scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advisory {
    /// `fatal` or `warn`.
    pub level: Level,
    /// The package it is about.
    pub package: String,
    /// `url`.
    pub url: Option<String>,
    /// `description`.
    pub description: Option<String>,
    /// The packages from the importer to this one.
    pub path: Option<Vec<PackageId>>,
}

/// What the scanner said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanResults {
    /// In the scanner's order.
    pub advisories: Vec<Advisory>,
    /// How many are fatal.
    pub fatal: usize,
    /// How many are warnings.
    pub warn: usize,
}

/// One package sent to the scanner, with how it was reached.
struct Collected {
    id: PackageId,
    /// From the importer (root or workspace) to the package.
    path: Vec<PackageId>,
    /// The specifier of the edge that reached it.
    literal: Option<String>,
}

/// Everything but the project itself (root and workspace members).
fn is_scannable(r: &Resolution) -> bool {
    !matches!(r, Resolution::Root | Resolution::Workspace { .. })
}

/// The packages to scan, breadth first. `seeds` limits the scan to those packages (and what
/// they depend on); `None` scans everything the root and the workspace members depend on.
fn collect(graph: &LockGraph, seeds: Option<&[PackageId]>) -> Vec<Collected> {
    let mut seen: HashSet<PackageId> = HashSet::new();
    let mut queue: VecDeque<(PackageId, Vec<PackageId>, Option<String>)> = VecDeque::new();
    let importer = |id: PackageId| {
        id == PackageId::ROOT
            || matches!(
                graph.packages[id.index()].resolution,
                Resolution::Workspace { .. }
            )
    };
    let mut enqueue =
        |queue: &mut VecDeque<_>, path: &[PackageId], target: Option<PackageId>, literal: &str| {
            let Some(t) = target else { return };
            if !seen.insert(t) {
                return;
            }
            let mut p = path.to_vec();
            p.push(t);
            queue.push_back((t, p, Some(literal.to_owned())));
        };
    match seeds {
        None => {
            for id in graph.ids().filter(|&id| importer(id)) {
                for e in &graph.packages[id.index()].dependencies {
                    enqueue(&mut queue, &[id], e.target, &e.literal);
                }
            }
        }
        Some(seeds) => {
            for &seed in seeds {
                'importers: for id in graph.ids().filter(|&id| importer(id)) {
                    for e in &graph.packages[id.index()].dependencies {
                        if e.target == Some(seed) {
                            enqueue(&mut queue, &[id], e.target, &e.literal);
                            break 'importers;
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    while let Some((id, path, literal)) = queue.pop_front() {
        let p = &graph.packages[id.index()];
        for e in &p.dependencies {
            enqueue(&mut queue, &path, e.target, &e.literal);
        }
        if is_scannable(&p.resolution) {
            out.push(Collected { id, path, literal });
        }
    }
    out
}

/// `scheme://user:pass@host/path?query#fragment` without the userinfo, and with `query` also
/// without the query (bun `append_without_url_credentials` / `append_without_url_userinfo`).
fn strip_url(text: &str, query: bool) -> String {
    let Some(scheme_end) = text.find("://") else {
        return text.to_owned();
    };
    let rest = &text[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let mut tail = rest[authority_end..].to_owned();
    if query && let Some(q) = tail.find('?') {
        let frag = tail[q..].find('#').map(|f| q + f);
        tail = match frag {
            Some(f) => format!("{}{}", &tail[..q], &tail[f..]),
            None => tail[..q].to_owned(),
        };
    }
    format!("{}{host}{tail}", &text[..scheme_end + 3])
}

fn without_credentials(text: &str) -> String {
    strip_url(text, true)
}

/// `<registry>/<name>/-/<unscoped>-<version>.tgz`.
fn default_tarball(registry: &str, name: &str, version: &str) -> String {
    let unscoped = name.rsplit('/').next().unwrap_or(name);
    format!(
        "{}/{name}/-/{unscoped}-{version}.tgz",
        registry.trim_end_matches('/')
    )
}

/// The JSON array of packages that the scanner gets.
fn payload(
    graph: &LockGraph,
    collected: &[Collected],
    config: &Config,
    github_api: &str,
) -> String {
    let mut items = Vec::with_capacity(collected.len());
    for c in collected {
        let p = &graph.packages[c.id.index()];
        let (version, tarball) = match &p.resolution {
            Resolution::Npm {
                version,
                tarball_url,
                ..
            } => {
                let url = tarball_url.clone().unwrap_or_else(|| {
                    default_tarball(&config.registry_for(&p.name).url, &p.name, version)
                });
                (version.clone(), strip_url(&url, false))
            }
            Resolution::Github {
                owner,
                repo,
                committish,
                resolved_commit,
                ..
            } => {
                let commit = if resolved_commit.is_empty() {
                    committish.as_str()
                } else {
                    let prefix = format!("{owner}-{repo}-");
                    resolved_commit
                        .strip_prefix(&prefix)
                        .unwrap_or(resolved_commit)
                };
                let version = without_credentials(&format!("github:{owner}/{repo}#{commit}"));
                let api = github_api.trim_end_matches('/');
                (
                    version,
                    without_credentials(&format!("{api}/repos/{owner}/{repo}/tarball/{commit}")),
                )
            }
            Resolution::LocalTarball { path, .. } => {
                let path = if path.starts_with(['.', '/']) {
                    path.clone()
                } else {
                    format!("./{path}")
                };
                (path.clone(), path)
            }
            Resolution::RemoteTarball { url, .. } => {
                let url = without_credentials(url);
                (url.clone(), url)
            }
            other => (without_credentials(&other.to_string()), String::new()),
        };
        let requested = match &c.literal {
            Some(l) => without_credentials(l),
            None => version.clone(),
        };
        items.push(serde_json::json!({
            "name": p.name,
            "version": version,
            "requestedRange": requested,
            "tarball": tarball,
        }));
    }
    serde_json::to_string_pretty(&Value::Array(items)).unwrap_or_else(|_| "[]".into())
}

/// bun `escape_control_chars`: C0, DEL, C1 and bidi controls as escapes. `multiline` keeps
/// `\t`, `\n` and `\r\n`.
pub(crate) fn escape_control(text: &str, multiline: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let pass = match c {
            '\t' | '\n' => multiline,
            '\r' => multiline && chars.peek() == Some(&'\n'),
            '\0'..='\x1f' | '\x7f' | '\u{80}'..='\u{9f}' => false,
            '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => false,
            _ => true,
        };
        if pass {
            out.push(c);
            continue;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push_str(&format!("\\u{:04x}", c as u32)),
        }
    }
    out
}

/// stderr in bun's `error: ` format.
fn err(message: &str) {
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if color {
        eprintln!("\x1b[31merror\x1b[0m: {message}");
    } else {
        eprintln!("error: {message}");
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// A failed scan: the details were printed.
fn failed(_bun_error_name: &str) -> Error {
    Error::Reported
}

/// The scanner, run once over `collected`. `Ok(None)` is bun's `NeedsInstall` (not used yet:
/// the scanner must be installed).
struct Run<'a> {
    config: &'a Config,
    scanner: &'a str,
    root: &'a Path,
    graph: &'a LockGraph,
    collected: Vec<Collected>,
    github_api: String,
    env: &'a fnpm_config::Env,
    json: bool,
}

#[cfg(not(unix))]
fn temp_file(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fnpm-scanner-{}-{n}-{}-{tag}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ))
}

#[cfg(not(unix))]
struct Cleanup(Vec<PathBuf>);

#[cfg(not(unix))]
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(unix)]
fn exchange(
    cmd: &mut std::process::Command,
    json: &[u8],
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>)> {
    use std::net::Shutdown;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;

    let (mut input, child_input) = UnixStream::pair()?;
    let (mut output, child_output) = UnixStream::pair()?;
    let source_in = rustix::io::fcntl_dupfd_cloexec(&child_input, 5)?;
    let source_out = rustix::io::fcntl_dupfd_cloexec(&child_output, 5)?;
    let input_fd = source_in.as_raw_fd();
    let output_fd = source_out.as_raw_fd();
    // Sources above 4 prevent one dup2 from overwriting the other's source.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(input_fd, 4) == -1 || libc::dup2(output_fd, 3) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(source_in);
    drop(source_out);
    drop(child_input);
    drop(child_output);
    let write_result = input.write_all(json);
    let _ = input.shutdown(Shutdown::Write);
    let mut data = Vec::new();
    let read_result = output.read_to_end(&mut data);
    let status = child.wait()?;
    read_result?;
    if data.is_empty()
        && let Err(error) = write_result
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(error);
    }
    Ok((status, data))
}

#[cfg(not(unix))]
fn exchange(
    cmd: &mut std::process::Command,
    json: &[u8],
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>)> {
    let input = temp_file("in");
    let output = temp_file("out");
    let mut paths = Cleanup(Vec::new());
    for path in [&input, &output] {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        paths.0.push(path.clone());
        if path == &input {
            file.write_all(json)?;
        }
    }
    cmd.env("FNPM_SCANNER_INPUT", &input)
        .env("FNPM_SCANNER_OUTPUT", &output);
    let status = cmd.status()?;
    let data = std::fs::read(&output)?;
    Ok((status, data))
}

impl Run<'_> {
    fn exec(&self) -> Result<ScanResults> {
        let verbose = self.config.log_level == LogLevel::Verbose;
        if verbose {
            eprintln!("[SecurityProvider] Running at '{}'", self.scanner);
            eprintln!(
                "[SecurityProvider] top_level_dir: '{}'",
                self.root.display()
            );
        }
        let start = Instant::now();
        let json = payload(self.graph, &self.collected, self.config, &self.github_api);
        let code = ENTRY
            .replacen(
                "__SUPPRESS_ERROR__",
                if verbose { "false" } else { "true" },
                1,
            )
            .replacen("__SCANNER_MODULE__", &json_string(self.scanner), 1);
        let node = self.env.get_not_empty("FNPM_NODE").unwrap_or("node");
        let mut cmd = std::process::Command::new(node);
        cmd.env_clear().envs(self.env.iter());
        match self.config.network.tls.ca_store.as_deref() {
            Some("system") => {
                cmd.arg("--use-system-ca");
            }
            Some("openssl") => {
                cmd.arg("--use-openssl-ca");
            }
            Some("bundled") => {
                cmd.arg("--use-bundled-ca");
            }
            _ => {}
        }
        cmd.args(["--no-warnings", "--input-type=module", "-e", &code])
            .current_dir(self.root);
        if self.json {
            cmd.stdout(std::process::Stdio::from(std::io::stderr()));
        }
        let (status, data) = exchange(&mut cmd, json.as_bytes()).map_err(|e| {
            err(&format!(
                "failed to start the security scanner with node: {e}"
            ));
            failed("SpawnFailed")
        })?;
        if data.is_empty() {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    err(&format!(
                        "Security scanner terminated by signal {} without sending data",
                        signal_name(sig)
                    ));
                    return Err(failed("NoSecurityScanData"));
                }
            }
            err(&format!(
                "Security scanner exited with code {} without sending data",
                status.code().unwrap_or(1)
            ));
            return Err(failed("NoSecurityScanData"));
        }
        let message: Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => {
                err("Security scanner sent invalid JSON: SyntaxError");
                if data.len() < 1000 {
                    err(&format!("Response: {}", String::from_utf8_lossy(&data)));
                }
                return Err(failed("InvalidIPCMessage"));
            }
        };
        let Some(obj) = message.as_object() else {
            err("Security scanner IPC message must be a JSON object");
            return Err(failed("InvalidIPCFormat"));
        };
        let Some(kind) = obj.get("type") else {
            err("Security scanner IPC message missing 'type' field");
            return Err(failed("MissingIPCType"));
        };
        let Some(kind) = kind.as_str() else {
            err("Security scanner IPC 'type' must be a string");
            return Err(failed("InvalidIPCType"));
        };
        if kind == "error" {
            let Some(code) = obj.get("code") else {
                err("Security scanner error missing 'code' field");
                return Err(failed("MissingErrorCode"));
            };
            let Some(code) = code.as_str() else {
                err("Security scanner error 'code' must be a string");
                return Err(failed("InvalidErrorCode"));
            };
            let (what, name) = match code {
                "MODULE_NOT_FOUND" => {
                    let name = self.scanner;
                    if !is_package_path(name) {
                        err(&format!(
                            "Security scanner '{name}' is configured in bunfig.toml but the file could not be found.\n  Please check that the file exists and the path is correct."
                        ));
                        return Err(failed("SecurityScannerNotFound"));
                    }
                    err(&format!(
                        "Security scanner '{name}' is configured in bunfig.toml but is not installed.\n  To install it, run: fnpm add --dev {name}"
                    ));
                    return Err(failed("SecurityScannerNotInDependencies"));
                }
                "INVALID_VERSION" => ("error", "InvalidScannerVersion"),
                "SCAN_FAILED" => ("failed", "ScannerFailed"),
                other => {
                    err(&format!("Unknown security scanner error code: {other}"));
                    return Err(failed("UnknownErrorCode"));
                }
            };
            if let Some(m) = obj.get("message").and_then(Value::as_str) {
                err(&format!("Security scanner {what}: {m}"));
            }
            return Err(failed(name));
        } else if kind != "result" {
            err(&format!("Unknown security scanner message type: {kind}"));
            return Err(failed("UnknownMessageType"));
        }

        let elapsed = start.elapsed().as_millis();
        let scanned = self.collected.len();
        if verbose {
            match status.code() {
                Some(code) => eprintln!(
                    "[SecurityProvider] {} with exit code {code} [{elapsed}ms]",
                    if code == 0 { "Completed" } else { "Failed" }
                ),
                None => eprintln!("[SecurityProvider] Completed with unknown status [{elapsed}ms]"),
            }
        } else if self.config.log_level != LogLevel::Silent && elapsed >= 1000 {
            let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            let line = format!(
                "{}[{}] Scanning {scanned} package{} took {elapsed}ms",
                if color { "⏳" } else { "" },
                self.scanner,
                if scanned == 1 { "" } else { "s" }
            );
            if color {
                eprintln!("\x1b[2m{line}\x1b[0m");
            } else {
                eprintln!("{line}");
            }
        }

        let Some(advisories) = obj.get("advisories") else {
            err("Security scanner result missing 'advisories' field");
            return Err(failed("MissingAdvisoriesField"));
        };
        let advisories = self.parse_advisories(advisories)?;
        if !status.success() {
            err(&format!(
                "Security scanner failed with exit code: {}",
                status.code().unwrap_or(1)
            ));
            return Err(failed("SecurityScannerFailed"));
        }
        let fatal = advisories
            .iter()
            .filter(|a| a.level == Level::Fatal)
            .count();
        let warn = advisories.len() - fatal;
        Ok(ScanResults {
            advisories,
            fatal,
            warn,
        })
    }

    fn parse_advisories(&self, value: &Value) -> Result<Vec<Advisory>> {
        let Some(items) = value.as_array() else {
            err(&format!(
                "Security scanner 'advisories' field must be an array, got: {}",
                type_name(value)
            ));
            return Err(failed("InvalidAdvisoriesFormat"));
        };
        let paths: HashMap<PackageId, &Vec<PackageId>> =
            self.collected.iter().map(|c| (c.id, &c.path)).collect();
        let mut out = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let Some(item) = item.as_object() else {
                err(&format!(
                    "Security advisory at index {i} must be an object, got: {}",
                    type_name(item)
                ));
                return Err(failed("InvalidAdvisoryFormat"));
            };
            let Some(name) = item.get("package") else {
                err(&format!(
                    "Security advisory at index {i} missing required 'package' field"
                ));
                return Err(failed("MissingPackageField"));
            };
            let Some(name) = name.as_str() else {
                err(&format!(
                    "Security advisory at index {i} 'package' field must be a string"
                ));
                return Err(failed("InvalidPackageField"));
            };
            if name.is_empty() {
                err(&format!(
                    "Security advisory at index {i} 'package' field cannot be empty"
                ));
                return Err(failed("EmptyPackageField"));
            }
            let string_or_null = |field: &str, error: &str| -> Result<Option<String>> {
                match item.get(field) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) => Ok(Some(s.clone())),
                    Some(_) => {
                        err(&format!(
                            "Security advisory at index {i} '{field}' field must be a string or null"
                        ));
                        Err(failed(error))
                    }
                }
            };
            let description = string_or_null("description", "InvalidDescriptionField")?;
            let url = string_or_null("url", "InvalidUrlField")?;
            let Some(level) = item.get("level") else {
                err(&format!(
                    "Security advisory at index {i} missing required 'level' field"
                ));
                return Err(failed("MissingLevelField"));
            };
            let Some(level) = level.as_str() else {
                err(&format!(
                    "Security advisory at index {i} 'level' field must be a string"
                ));
                return Err(failed("InvalidLevelField"));
            };
            let level = match level {
                "fatal" => Level::Fatal,
                "warn" => Level::Warn,
                other => {
                    err(&format!(
                        "Security advisory at index {i} 'level' field must be 'fatal' or 'warn', got: '{other}'"
                    ));
                    return Err(failed("InvalidLevelValue"));
                }
            };
            let path = self
                .graph
                .ids()
                .find(|id| self.graph.packages[id.index()].name == name)
                .and_then(|id| paths.get(&id).map(|p| (*p).clone()));
            out.push(Advisory {
                level,
                package: name.to_owned(),
                url,
                description,
                path,
            });
        }
        Ok(out)
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "e_null",
        Value::Bool(_) => "e_boolean",
        Value::Number(_) => "e_number",
        Value::String(_) => "e_string",
        Value::Array(_) => "e_array",
        Value::Object(_) => "e_object",
    }
}

/// bun `is_package_path`: a path, not a package name.
fn is_package_path(name: &str) -> bool {
    !(name.starts_with('.') || name.starts_with('/') || Path::new(name).is_absolute())
}

#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        6 => "SIGABRT".into(),
        9 => "SIGKILL".into(),
        11 => "SIGSEGV".into(),
        15 => "SIGTERM".into(),
        n => format!("signal {n}"),
    }
}

/// Where the scan runs and what it covers.
pub(crate) struct Scan<'a> {
    /// The project's configuration (`security_scanner` must be set).
    pub config: &'a Config,
    /// The project root, the scanner's working directory.
    pub root: &'a Path,
    /// The graph that would be installed.
    pub graph: &'a LockGraph,
    /// `None` scans everything; otherwise the packages the command asked for.
    pub seeds: Option<Vec<PackageId>>,
    /// The environment (for `GITHUB_API_URL`).
    pub env: &'a fnpm_config::Env,
    /// Reserve stdout for a structured command result.
    pub json: bool,
}

fn bootstrap(s: &Scan<'_>, name: &str) -> Result<()> {
    if !is_package_path(name)
        || !fnpm_types::name::is_npm_package_name(name)
        || s.root.join("node_modules").join(name).exists()
    {
        return Ok(());
    }
    let Some(edge) = s.graph.packages[0]
        .dependencies
        .iter()
        .find(|e| e.name == name)
    else {
        return Ok(());
    };
    let Some(id) = edge.target else { return Ok(()) };
    if s.config.log_level != LogLevel::Silent {
        let line = "Attempting to install security scanner from npm...";
        if s.json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
    let mut graph = s.graph.clone();
    graph.workspaces.clear();
    graph.packages[0].dependencies = vec![edge.clone()];
    graph.packages[0].dependencies[0].behavior = fnpm_types::Behavior::PROD;
    let mut selected = HashSet::new();
    let mut queue = VecDeque::from([id]);
    while let Some(id) = queue.pop_front() {
        if !selected.insert(id) {
            continue;
        }
        queue.extend(
            graph.packages[id.index()]
                .dependencies
                .iter()
                .filter_map(|e| e.target),
        );
    }
    for p in &mut graph.packages {
        if matches!(p.resolution, Resolution::Workspace { .. }) {
            p.dependencies.clear();
        }
    }
    let identity = selected
        .iter()
        .map(|id| {
            let p = &graph.packages[id.index()];
            format!("{}:{}:{:?}", id.0, p.key(), p.resolution)
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join("\n");
    let cache = s.config.cache_dir.join("security-scanners").join(format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64(identity.as_bytes())
    ));
    std::fs::create_dir_all(&cache).map_err(Error::io(&cache))?;
    let lock_path = cache.join("install.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(Error::io(&lock_path))?;
    lock.lock().map_err(Error::io(&lock_path))?;
    let mut config = s.config.clone();
    config.production = false;
    config.omit = fnpm_config::Omit::default();
    config.ignore_scripts = true;
    let config = std::sync::Arc::new(config);
    let store = fnpm_store::Store::open(&config.store_dir)?;
    let sink: std::sync::Arc<dyn fnpm_types::EventSink> = std::sync::Arc::new(fnpm_types::NoopSink);
    crate::runtime().block_on(async {
        let client = fnpm_registry::RegistryClient::with_env(config.clone(), s.env.clone())?;
        let fetcher = crate::fetch::Fetcher::new(
            store.clone(),
            client,
            s.root.to_owned(),
            config.cache_dir.clone(),
            config.verify_integrity,
            sink.clone(),
        );
        for id in &selected {
            let p = &graph.packages[id.index()];
            if !crate::fetch::needs_content(&p.resolution) {
                continue;
            }
            fetcher
                .ensure_package(p)
                .wait()
                .await
                .map_err(|e| Error::Fetch {
                    package: p.key(),
                    message: e.message,
                    code: e.code,
                })?;
        }
        let linker = fnpm_linker::Linker::new(config.clone(), store);
        let layout = linker.plan(&graph, fnpm_types::NodeLinker::Hoisted);
        let source = fetcher.source(&graph);
        linker.link(
            &cache,
            &graph,
            &layout,
            &source,
            &fnpm_linker::LinkOptions {
                threads: 2,
                global_store: false,
                ..Default::default()
            },
            &*sink,
        )?;
        Ok::<_, Error>(())
    })?;
    let dest = s.root.join("node_modules").join(name);
    let parent = dest
        .parent()
        .ok_or_else(|| Error::usage("invalid scanner package name"))?;
    let mut path = s.root.to_owned();
    for part in Path::new("node_modules")
        .join(name)
        .parent()
        .unwrap_or(Path::new("node_modules"))
        .components()
    {
        path.push(part);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
            return Err(Error::usage(
                "scanner installation path contains a symbolic link",
            ));
        }
    }
    std::fs::create_dir_all(parent).map_err(Error::io(parent))?;
    fnpm_linker::fsutil::replace_symlink_atomic(&cache.join("node_modules").join(name), &dest)
        .map_err(Error::io(&dest))?;
    if s.config.log_level != LogLevel::Silent {
        let line = "Security scanner installed successfully.";
        if s.json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
    Ok(())
}

/// Runs the configured scanner. Errors were printed (bun's lines); the error is the last one.
pub(crate) fn scan(s: &Scan<'_>) -> Result<ScanResults> {
    let Some(scanner) = s.config.security_scanner.as_deref() else {
        return Ok(ScanResults::default());
    };
    // The scanner must be a dependency of the root, not of a workspace member.
    for p in &s.graph.packages {
        if matches!(p.resolution, Resolution::Workspace { .. })
            && p.dependencies.iter().any(|e| e.name == scanner)
        {
            err(
                "security scanner cannot be a dependency of a workspace package. It must be a direct dependency of the root package.",
            );
            return Err(Error::Reported);
        }
    }
    bootstrap(s, scanner)?;
    let run = Run {
        config: s.config,
        env: s.env,
        json: s.json,
        scanner,
        root: s.root,
        graph: s.graph,
        collected: collect(s.graph, s.seeds.as_deref()),
        github_api: s
            .env
            .get_not_empty("GITHUB_API_URL")
            .unwrap_or("https://api.github.com")
            .to_owned(),
    };
    run.exec()
}

/// bun `print_security_advisories` (stdout).
pub(crate) fn format_advisories(graph: &LockGraph, results: &ScanResults, color: bool) -> String {
    let paint = |code: &str, text: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    };
    let mut out = String::new();
    if results.advisories.is_empty() {
        return out;
    }
    for a in &results.advisories {
        out.push('\n');
        let package = escape_control(&a.package, false);
        match a.level {
            Level::Fatal => out.push_str(&format!("  {}: {package}\n", paint("31", "FATAL"))),
            Level::Warn => out.push_str(&format!("  {}: {package}\n", paint("33", "WARNING"))),
        }
        if let Some(path) = &a.path {
            if path.len() > 1 {
                let names: Vec<String> = path[..path.len() - 1]
                    .iter()
                    .map(|id| escape_control(&graph.packages[id.index()].name, false))
                    .collect();
                out.push_str(&format!(
                    "    {}{}\n",
                    paint("2", &format!("via {} › ", names.join(" › "))),
                    paint("31", &package)
                ));
            } else {
                out.push_str(&format!("    {}\n", paint("2", "(direct dependency)")));
            }
        }
        if let Some(d) = a.description.as_deref().filter(|d| !d.is_empty()) {
            out.push_str(&format!("    {}\n", escape_control(d, true)));
        }
        if let Some(u) = a.url.as_deref().filter(|u| !u.is_empty()) {
            out.push_str(&format!("    {}\n", paint("36", &escape_control(u, false))));
        }
    }
    out.push('\n');
    let total = results.fatal + results.warn;
    let s = |n: usize| if n == 1 { "" } else { "s" };
    let summary = if total == 1 {
        if results.fatal == 1 {
            format!("1 advisory ({})", paint("31", "1 fatal"))
        } else {
            format!("1 advisory ({})", paint("33", "1 warning"))
        }
    } else if results.fatal > 0 && results.warn > 0 {
        format!(
            "{total} advisories ({}, {})",
            paint("31", &format!("{} fatal", results.fatal)),
            paint(
                "33",
                &format!("{} warning{}", results.warn, s(results.warn))
            )
        )
    } else if results.fatal > 0 {
        format!(
            "{total} advisories ({})",
            paint("31", &format!("{} fatal", results.fatal))
        )
    } else {
        format!(
            "{total} advisories ({})",
            paint(
                "33",
                &format!("{} warning{}", results.warn, s(results.warn))
            )
        )
    };
    out.push_str(&if color {
        format!("\x1b[1m{summary}\x1b[0m\n")
    } else {
        format!("{summary}\n")
    });
    out
}

/// bun `prompt_for_warnings`: `[y/N]` on a terminal; without one the install is cancelled.
pub(crate) fn prompt_for_warnings(json: bool) -> bool {
    let mut stdout: Box<dyn Write> = if json {
        Box::new(std::io::stderr())
    } else {
        Box::new(std::io::stdout())
    };
    let tty = std::io::stdin().is_terminal()
        || std::env::var_os("FNPM_INTERNAL_SECURITY_PROMPT_ASSUME_TTY").is_some();
    if !tty {
        let _ = write!(
            stdout,
            "\nSecurity warnings found. Cannot prompt for confirmation (no TTY).\nInstallation cancelled.\n"
        );
        let _ = stdout.flush();
        return false;
    }
    let _ = write!(stdout, "\nSecurity warnings found. Continue anyway? [y/N] ");
    let _ = stdout.flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let yes = matches!(line.trim_end_matches(['\n', '\r']), "y" | "Y");
    if yes {
        let _ = write!(stdout, "\nContinuing with installation...\n\n");
    } else {
        let _ = write!(stdout, "\nInstallation cancelled.\n");
    }
    let _ = stdout.flush();
    yes
}

/// The install's scan, after resolution and before linking: prints the advisories, aborts on a
/// fatal one, asks about warnings.
pub(crate) fn scan_for_install(s: &Scan<'_>) -> Result<()> {
    let results = scan(s)?;
    if results.advisories.is_empty() {
        return Ok(());
    }
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut stdout: Box<dyn Write> = if s.json {
        Box::new(std::io::stderr())
    } else {
        Box::new(std::io::stdout())
    };
    let _ = stdout.write_all(format_advisories(s.graph, &results, color).as_bytes());
    if results.fatal > 0 {
        let line = "Installation aborted due to fatal security advisories";
        let _ = if color {
            writeln!(stdout, "\x1b[31m{line}\x1b[0m")
        } else {
            writeln!(stdout, "{line}")
        };
        let _ = stdout.flush();
        return Err(Error::Reported);
    }
    let _ = stdout.flush();
    if results.warn > 0 && !prompt_for_warnings(s.json) {
        return Err(Error::Reported);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn scanner_ipc_keeps_stdio_separate_and_drains_large_messages() {
        let mut cmd = std::process::Command::new("node");
        cmd.args([
            "--input-type=module",
            "-e",
            r#"
            import fs from 'node:fs';
            const input = fs.readFileSync(4, 'utf8');
            fs.writeSync(1, 'scanner stdout');
            fs.writeFileSync(3, input);
        "#,
        ]);
        cmd.stdout(std::process::Stdio::null());
        let input = vec![b'x'; 1024 * 1024];
        let (status, output) = exchange(&mut cmd, &input).unwrap();
        assert!(status.success());
        assert_eq!(output, input);
    }

    #[test]
    fn credentials_are_stripped() {
        assert_eq!(
            without_credentials(
                "git+https://x-access-token:s3cret@example.com/org/repo.git?token=s3cret#main"
            ),
            "git+https://example.com/org/repo.git#main"
        );
        assert_eq!(
            without_credentials("git+ssh://git@example.com/org/repo.git#main"),
            "git+ssh://example.com/org/repo.git#main"
        );
        assert_eq!(
            strip_url(
                "https://user:s3cret@registry.example.com/signed/-/signed-1.0.0.tgz?signature=abc#fragment",
                false
            ),
            "https://registry.example.com/signed/-/signed-1.0.0.tgz?signature=abc#fragment"
        );
        assert_eq!(without_credentials("^1.0.0"), "^1.0.0");
    }

    #[test]
    fn control_characters() {
        assert_eq!(
            escape_control("left-pad\x1b[2K\rsafe", false),
            r"left-pad\x1b[2K\rsafe"
        );
        assert_eq!(
            escape_control("first line\n\tsecond\x1b[1A", true),
            "first line\n\tsecond\\x1b[1A"
        );
    }

    #[test]
    fn summary_lines() {
        let graph = LockGraph::new(fnpm_types::Package::new("app", "1.0.0", Resolution::Root));
        let adv = |level| Advisory {
            level,
            package: "x".into(),
            url: None,
            description: None,
            path: None,
        };
        let results = ScanResults {
            advisories: vec![adv(Level::Fatal), adv(Level::Warn), adv(Level::Warn)],
            fatal: 1,
            warn: 2,
        };
        let text = format_advisories(&graph, &results, false);
        assert!(
            text.ends_with("3 advisories (1 fatal, 2 warnings)\n"),
            "{text}"
        );
        assert!(text.contains("  FATAL: x\n"));
    }
}
