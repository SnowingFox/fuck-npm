//! `fnpm publish` (bun `runtime/cli/publish_command.rs`): pack the package in memory (or read
//! a tarball), apply `publishConfig`, refuse what bun refuses (provenance, a private package,
//! restricted access without a scope, a version-like tag, a prerelease without a tag), then
//! PUT it with the two-factor flows of `fnpm-registry`, and run `publish` / `postpublish`.
//! `-r` / `--filter` pack every selected workspace first, then publish them dependencies first.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fnpm_config::{Config, LogLevel};
use fnpm_registry::{OtpPrompt, PublishOptions, PublishPackage, RegistryClient};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;

use super::Ctx;
use super::pack::json::{self, Json};
use super::pack::out::{FResult, Fatal, Out, escape_control, fmt_size};
use super::pack::{
    self, Exit, NameError, PackOptions, Packed, PublishHooks, XResult, find_package_json, header,
    publish_name_error, published_version,
};
use crate::{CoreResult, Result};

/// `fnpm publish`.
pub(crate) async fn run(cx: &Ctx) -> Result<CoreResult> {
    let out = Out::new();
    header(cx, &out, "publish");
    let code = match publish(cx, &out).await {
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

/// What the command line says about the publish.
#[derive(Clone)]
struct Cli {
    tag: Option<String>,
    access: Option<String>,
    otp: Option<String>,
    auth_type: Option<String>,
    provenance: Option<bool>,
    tolerate_republish: bool,
    registry_from_cli: bool,
    dry_run: bool,
    workspaces: bool,
    log_level: LogLevel,
    node_version: Option<String>,
    user_agent: String,
}

fn breaks_header_line(value: &str) -> bool {
    value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
}

fn parse_cli(cx: &Ctx, out: &Out) -> FResult<Cli> {
    let access = cx.string("access");
    if let Some(a) = &access
        && a != "public"
        && a != "restricted"
    {
        return Err(out.error(&format!("invalid `access` value: '{a}'")));
    }
    let otp = cx.string("otp");
    if otp.as_deref().is_some_and(breaks_header_line) {
        return Err(out.error("invalid `otp` value: must not contain a newline or NUL byte"));
    }
    let auth_type = cx.string("authType");
    if let Some(t) = &auth_type
        && t != "web"
        && t != "legacy"
    {
        return Err(out.error(&format!("invalid `auth-type` value: '{t}'")));
    }
    let provenance = if cx.opt("provenanceFile").is_some() {
        Some(true)
    } else {
        match cx.opt("provenance") {
            Some(serde_json::Value::Bool(b)) => Some(*b),
            Some(serde_json::Value::String(s)) => Some(fnpm_config::npmrc::npm_config_bool(s)),
            _ if cx.flag("noProvenance") => Some(false),
            _ => None,
        }
    };
    let version = cx
        .string("cliVersion")
        .unwrap_or_else(|| crate::version().to_owned());
    Ok(Cli {
        tag: cx.string("tag").filter(|t| !t.is_empty()),
        access,
        otp: otp.filter(|o| !o.is_empty()),
        auth_type,
        provenance,
        tolerate_republish: cx.flag("tolerateRepublish"),
        registry_from_cli: cx.pm.registry.is_some(),
        dry_run: cx.pm.dry_run,
        workspaces: cx.flag("recursive") || !cx.strings("filter").is_empty(),
        log_level: cx.pm.log_level,
        node_version: cx.string("nodeVersion"),
        user_agent: format!("fnpm/{version}"),
    })
}

/// The registries and settings one package is published with: the configuration with its
/// `publishConfig` applied over what the command line says.
struct Target {
    base: Config,
    config: Config,
    cli: Cli,
    env: fnpm_config::Env,
    tag: Option<String>,
    access: Option<String>,
    folder_bins: Option<serde_json::Value>,
    folder_bin_warning: Option<String>,
}

impl Target {
    fn new(base: Config, cli: Cli, env: fnpm_config::Env) -> Self {
        Self {
            config: base.clone(),
            tag: cli.tag.clone(),
            access: cli.access.clone(),
            folder_bins: None,
            folder_bin_warning: None,
            base,
            cli,
            env,
        }
    }

    /// bun `provenance_asked_by`: the first source that says anything decides.
    fn provenance_asked_by(&self, json: &Json) -> Option<&'static str> {
        let in_package_json = json
            .get("publishConfig")
            .and_then(|c| c.get("provenance"))
            .and_then(|v| match v {
                Json::Bool(b) => Some(*b),
                Json::Null => Some(false),
                Json::Number(n) => Some(*n != 0.0 && !n.is_nan()),
                Json::Object(_) => Some(true),
                Json::String(s) => Some(fnpm_config::npmrc::npm_config_bool(s)),
                Json::Array(..) => None,
            });
        let in_env = ["NPM_CONFIG_PROVENANCE", "npm_config_provenance"]
            .iter()
            .filter_map(|k| self.env.get(k).filter(|v| !v.is_empty()))
            .map(fnpm_config::npmrc::npm_config_bool)
            .max();
        let npmrc = if in_env.is_none() {
            self.base.provenance
        } else {
            None
        };
        [
            ("--provenance", self.cli.provenance),
            (
                "\"provenance\" in the publishConfig of package.json",
                in_package_json,
            ),
            ("NPM_CONFIG_PROVENANCE", in_env),
            ("\"provenance\" in .npmrc", npmrc),
        ]
        .into_iter()
        .find_map(|(name, on)| Some((name, on?)))
        .and_then(|(name, on)| on.then_some(name))
    }

    /// Applies `publishConfig`, then refuses provenance, restricted access without a scope and
    /// a private package (bun `apply_publish_config` in `pack_command.rs`).
    fn apply_config(&mut self, out: &Out, json: &Json, name: &str) -> FResult<()> {
        let is_scoped = fnpm_types::name::is_scoped_package_name(name)
            .map_err(|_| publish_name_error(out, NameError::InvalidName))?;
        self.apply_publish_config(out, json, name)?;
        if let Some(asked_by) = self.provenance_asked_by(json) {
            return Err(out.error(&format!(
                "{asked_by} is not supported yet: fnpm publish cannot generate provenance statements"
            )));
        }
        if self.access.as_deref() == Some("restricted") && !is_scoped {
            return Err(out.error("unable to restrict access to unscoped package"));
        }
        if json.get("private").is_some_and(Json::is_truthy) {
            return Err(out.error("attempted to publish a private package"));
        }
        Ok(())
    }

    fn apply_publish_config(&mut self, out: &Out, json: &Json, name: &str) -> FResult<()> {
        let mut config = self.base.clone();
        let settings = config
            .apply_publish_config(&json.to_value(), name, self.cli.registry_from_cli)
            .map_err(|e| out.error(&config_error(&e)))?;
        self.config = config;
        self.tag = self
            .cli
            .tag
            .clone()
            .or(settings.tag.filter(|t| !t.is_empty()));
        self.access = self.cli.access.clone().or(settings.access);
        Ok(())
    }

    fn refuse_tag_for(&self, out: &Out, version: &str) -> FResult<()> {
        let tag = self.tag.as_deref().unwrap_or("");
        if !tag.is_empty() && fnpm_types::spec::infer_tag(tag) == fnpm_types::spec::SpecTag::Npm {
            return Err(out.error(&format!(
                "invalid tag \"{}\": cannot be a version or range",
                escape_control(tag)
            )));
        }
        let pre = fnpm_semver::Version::parse(version).is_ok_and(|v| !v.pre.is_empty());
        if tag.is_empty() && pre {
            return Err(Fatal(format!(
                "{}\n{} pass a tag, for example {}, or {} to publish it as \"latest\" anyway",
                out.error(&format!(
                    "prerelease needs a tag: {} would become \"latest\"",
                    escape_control(version)
                ))
                .0,
                out.paint_err("34", "note:"),
                out.paint_err("1", "fnpm publish --tag next"),
                out.paint_err("1", "--tag latest"),
            )));
        }
        Ok(())
    }

    fn client(&self, out: &Out) -> FResult<RegistryClient> {
        RegistryClient::with_env(Arc::new(self.config.clone()), self.env.clone())
            .map_err(|e| out.error(&e.to_string()))
    }

    fn has_credentials(&self, name: &str) -> bool {
        self.config.registry_for(name).auth.is_some()
    }

    fn options(&self) -> PublishOptions {
        PublishOptions {
            tag: self.tag.clone(),
            access: self.access.clone(),
            auth_type: self.cli.auth_type.clone(),
            otp: self.cli.otp.clone(),
            workspaces: self.cli.workspaces,
            user_agent: Some(self.cli.user_agent.clone()),
        }
    }
}

fn config_error(e: &fnpm_config::Error) -> String {
    match e {
        fnpm_config::Error::InvalidValue { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

impl PublishHooks for Target {
    fn apply(&mut self, out: &Out, json: &Json, name: &str) -> FResult<()> {
        self.apply_config(out, json, name)
    }

    fn refuse_tag(&mut self, out: &Out, version: &str) -> FResult<()> {
        self.refuse_tag_for(out, version)
    }

    fn prepare_manifest(&mut self, out: &Out, json: &Json, dir: &Path) -> FResult<()> {
        use super::pack::walk::{self, BinType, Kind};
        self.folder_bins = None;
        self.folder_bin_warning = None;
        if walk::non_empty_bin(json).is_some() {
            return Ok(());
        }
        let Some(typed) = json
            .get("directories")
            .and_then(|d| d.get("bin"))
            .and_then(Json::as_str)
        else {
            return Ok(());
        };
        let Some(path) = walk::bin_subpath(typed, BinType::Dir) else {
            return Ok(());
        };
        let kind = walk::bin_kind_without_following_symlinks(out, dir, &path)?;
        if kind != Some(Kind::Dir) {
            let why = if kind.is_none() {
                "does not exist"
            } else {
                "is not a directory"
            };
            self.folder_bin_warning = Some(format!(
                "bin directory '{}' {why}",
                String::from_utf8_lossy(&path)
            ));
            return Ok(());
        }
        let mut props = serde_json::Map::new();
        let mut dirs = vec![path];
        while let Some(subpath) = dirs.pop() {
            for entry in walk::read_dir(out, &dir.join(walk::bytes_path(&subpath)), &subpath)? {
                let mut path = subpath.clone();
                path.push(b'/');
                path.extend_from_slice(&entry.name);
                props.insert(
                    String::from_utf8_lossy(&entry.name).into_owned(),
                    serde_json::Value::String(String::from_utf8_lossy(&path).into_owned()),
                );
                if entry.kind == Kind::Dir {
                    dirs.push(path);
                }
            }
        }
        self.folder_bins = Some(serde_json::Value::Object(props));
        Ok(())
    }
}

/// The terminal side of the two-factor flows.
struct Prompt {
    out: Out,
}

/// The URL that ENTER opens; `None` once the login is over. One thread waits for ENTER per
/// process, since a thread blocked on stdin cannot be stopped.
static AUTH_URL: Mutex<Option<String>> = Mutex::new(None);
static WAITS_FOR_ENTER: AtomicBool = AtomicBool::new(false);

fn press_enter_to_open_in_browser() {
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) if buf[..n].contains(&b'\n') => break,
            Ok(_) => {}
        }
    }
    WAITS_FOR_ENTER.store(false, Ordering::SeqCst);
    let Some(url) = AUTH_URL.lock().take() else {
        return;
    };
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    // Looked up on PATH only: a relative program name is never run from the current
    // directory.
    let _ = std::process::Command::new(opener)
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .status();
}

impl OtpPrompt for Prompt {
    fn web_login(&self, auth_url: &str) {
        let shown = escape_control(auth_url);
        let is_web = (auth_url.starts_with("http://") || auth_url.starts_with("https://"))
            && shown == auth_url;
        let mut text = if is_web {
            format!(
                "\nAuthenticate your account at (press {} to open in browser):\n\n",
                self.out.paint("1", "ENTER")
            )
        } else {
            "\nAuthenticate your account at:\n\n".to_owned()
        };
        let [h, v, tl, tr, bl, br] = if self.out.color_out {
            ["─", "│", "┌", "┐", "└", "┘"]
        } else {
            ["-", "|", "|", "|", "|", "|"]
        };
        let line = h.repeat(shown.chars().count() + 2);
        text.push_str(&format!(
            "{tl}{line}{tr}\n{v} {} {v}\n{bl}{line}{br}\n",
            self.out.paint("1", &shown)
        ));
        // bun's prettyln of the heading ends with a blank line it does not print twice.
        let text = text.replacen(":\n\n", ":\n", 1);
        self.out.print(&text);
        if is_web {
            *AUTH_URL.lock() = Some(auth_url.to_owned());
            if !WAITS_FOR_ENTER.swap(true, Ordering::SeqCst) {
                let _ = std::thread::Builder::new()
                    .name("fnpm-web-login".into())
                    .spawn(press_enter_to_open_in_browser);
            }
        }
    }

    fn read_otp(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            self.out
                .print("\nThis operation requires a one-time password.\nEnter OTP: ");
            tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                match std::io::stdin().lock().read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(line.trim().to_owned()),
                }
            })
            .await
            .ok()
            .flatten()
        })
    }

    fn notice(&self, notice: &str) {
        print_notice(&self.out, notice);
    }
}

fn print_notice(out: &Out, notice: &str) {
    out.eprint(&format!(
        "\n{} {}\n",
        out.paint_err("34", "note:"),
        escape_control(notice)
    ));
}

fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// bun `Npm::response_error` and the other ways a publish request fails.
fn publish_failure(out: &Out, e: &fnpm_registry::Error) -> Fatal {
    use fnpm_registry::Error as E;
    let status_line = |status: u16, url: &str| {
        let reason = reason(status);
        format!(
            "\n{}{}{}: {}",
            out.paint_err("31", &status.to_string()),
            if reason.is_empty() { "" } else { " " },
            reason,
            escape_control(url)
        )
    };
    match e {
        E::Registry {
            url,
            status,
            message,
        } => Fatal(format!(
            "{}\n\n - {}",
            status_line(*status, url),
            escape_control(message)
        )),
        E::Status { url, status } => Fatal(status_line(*status, url)),
        E::NeedAuth(m) => out.error(m),
        E::WebLogin(m) => out.named("WebLogin", m),
        other => match tls_code(&other.to_string()) {
            Some(code) => out.named(code, "failed to publish package"),
            None => out.error(&format!("failed to publish package: {other}")),
        },
    }
}

/// The OpenSSL name of a certificate failure, as bun and node report it.
fn tls_code(message: &str) -> Option<&'static str> {
    if message.contains("CaUsedAsEndEntity") || message.contains("SelfSigned") {
        Some("DEPTH_ZERO_SELF_SIGNED_CERT")
    } else if message.contains("UnknownIssuer") {
        Some("UNABLE_TO_GET_ISSUER_CERT_LOCALLY")
    } else if message.contains("NotValidForName") {
        Some("ERR_TLS_CERT_ALTNAME_INVALID")
    } else if message.contains("Expired") {
        Some("CERT_HAS_EXPIRED")
    } else {
        None
    }
}

/// What is published: the package, its tarball and the manifest the registry gets.
struct Publishing {
    name: String,
    version: String,
    tarball: Vec<u8>,
    manifest_json: String,
    publish_script: Option<String>,
    postpublish_script: Option<String>,
    package_json: Option<PathBuf>,
}

/// The registry URL for display and for `dist.tarball`: no userinfo, no trailing slash.
fn registry_href(client: &RegistryClient, name: &str) -> String {
    client
        .publish_registry_href(name)
        .trim_end_matches('/')
        .to_owned()
}

/// bun `normalized_package`, printed minified.
#[allow(clippy::too_many_arguments)]
fn normalized_manifest(
    out: &Out,
    target: &Target,
    client: &RegistryClient,
    manifest: &Json,
    name: &str,
    version: &str,
    tarball: &[u8],
    files: &[String],
    readme: Option<&(String, String)>,
    package_dir: Option<&Path>,
) -> String {
    let mut value = manifest.to_value();
    let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let warnings = fnpm_registry::publish::normalize_manifest(
        &mut value,
        name,
        version,
        tarball,
        &format!("{}/", registry_href(client, name)),
        readme.map(|(f, c)| (f.as_str(), c.as_str())),
        &file_refs,
    );
    if let Some(node) = &target.cli.node_version
        && let Some(obj) = value.as_object_mut()
    {
        obj.insert(
            "_nodeVersion".into(),
            serde_json::Value::String(node.clone()),
        );
    }
    if let Some(bins) = &target.folder_bins
        && let Some(object) = value.as_object_mut()
    {
        object.insert("bin".into(), bins.clone());
    }
    let mut warnings = warnings;
    if target.folder_bins.is_some() || target.folder_bin_warning.is_some() {
        warnings.retain(|w| !w.starts_with("bin directory '"));
    }
    warnings.extend(target.folder_bin_warning.iter().cloned());
    for mut w in warnings {
        // A folder's `directories.bin` that is there but is no directory (a symlink, a file).
        if let Some(dir) = package_dir
            && let Some(bin) = w
                .strip_prefix("bin directory '")
                .and_then(|r| r.strip_suffix("' does not exist"))
            && let Ok(Some(kind)) =
                super::pack::walk::bin_kind_without_following_symlinks(out, dir, bin.as_bytes())
            && kind != super::pack::walk::Kind::Dir
        {
            w = format!("bin directory '{bin}' is not a directory");
        }
        out.eprintln(&format!(
            "{} {}",
            out.paint_err("33", "warn:"),
            escape_control(&w)
        ));
    }
    serde_json::to_string(&value).unwrap_or_default()
}

/// Sends the package. `Ok(false)`: `--tolerate-republish` found the version on the registry.
async fn send(
    out: &Out,
    target: &Target,
    client: &RegistryClient,
    p: &Publishing,
) -> XResult<bool> {
    if !target.has_credentials(&p.name) {
        return Err(out
            .error(&format!(
                "missing authentication (run {})",
                out.paint_err("36", "`fnpm x npm login`")
            ))
            .into());
    }
    if target.cli.tolerate_republish {
        let version = fnpm_registry::publish::without_build_tag(&p.version);
        let notice = |n: &str| print_notice(out, n);
        if client
            .version_exists_noticed(&p.name, version, &notice)
            .await
        {
            out.eprintln(&format!(
                "{} Registry already knows about version {}; skipping.",
                out.paint_err("33", "warn:"),
                escape_control(version)
            ));
            return Ok(false);
        }
    }
    let label = |s: &str| out.paint("1;34", s);
    out.print(&format!(
        "{}: {}\n{}: {}\n{}: {}/\n",
        label("Tag"),
        escape_control(target.tag.as_deref().unwrap_or("latest")),
        label("Access"),
        target.access.as_deref().unwrap_or("default"),
        label("Registry"),
        escape_control(&registry_href(client, &p.name)),
    ));
    if target.cli.dry_run {
        return Ok(true);
    }
    let pkg = PublishPackage {
        name: &p.name,
        version: &p.version,
        manifest_json: &p.manifest_json,
        tarball: &p.tarball,
    };
    let prompt = Prompt { out: *out };
    let result = client.publish(&pkg, &target.options(), &prompt).await;
    // The login is over: ENTER no longer opens its page.
    AUTH_URL.lock().take();
    match result {
        Ok(published) => {
            if let Some(n) = published.notice {
                print_notice(out, &n);
            }
            Ok(true)
        }
        Err(e) => Err(publish_failure(out, &e).into()),
    }
}

fn print_published(out: &Out, target: &Target, p: &Publishing) {
    out.print(&format!(
        "\n{} {}@{}{}\n",
        out.paint("32", " +"),
        escape_control(&p.name),
        escape_control(fnpm_registry::publish::without_build_tag(&p.version)),
        if target.cli.dry_run { " (dry-run)" } else { "" }
    ));
}

/// The ` + name@version` line, then `publish` and `postpublish`.
async fn finish_directory_publish(
    cx: &Ctx,
    target: &Target,
    p: &Publishing,
    ignore_scripts: bool,
) -> XResult<()> {
    let out = Out::new();
    print_published(&out, target, p);
    if ignore_scripts {
        return Ok(());
    }
    let Some(package_json) = &p.package_json else {
        return Ok(());
    };
    let dir = package_json.parent().unwrap_or(Path::new("/"));
    let project_dir =
        fnpm_workspace::Workspace::find_root(dir).unwrap_or_else(|_| dir.to_path_buf());
    let env = vec![
        ("npm_command".to_owned(), "publish".to_owned()),
        (
            "npm_package_json".to_owned(),
            package_json.to_string_lossy().into_owned(),
        ),
        ("npm_package_name".to_owned(), p.name.clone()),
        ("npm_package_version".to_owned(), p.version.clone()),
    ];
    let silent = target.cli.log_level == LogLevel::Silent;
    for (name, script) in [
        ("publish", &p.publish_script),
        ("postpublish", &p.postpublish_script),
    ] {
        if let Some(script) = script {
            pack::run_lifecycle(cx, dir, &project_dir, name, script, &env, silent).await?;
        }
    }
    Ok(())
}

fn pack_options(cx: &Ctx, cli: &Cli) -> PackOptions {
    PackOptions {
        dry_run: cli.dry_run,
        ignore_scripts: pack::ignore_scripts(cx),
        log_level: cli.log_level,
        destination: Vec::new(),
        filename: Vec::new(),
        gzip_level: cx.string("gzipLevel"),
    }
}

/// Packs the package of `package_json` for `target`.
async fn pack_for_publish(
    cx: &Ctx,
    out: &Out,
    target: &mut Target,
    package_json: &Path,
) -> XResult<Publishing> {
    let options = pack_options(cx, &target.cli);
    let packed: Packed = pack::pack(cx, out, &options, package_json, Some(target)).await?;
    let manifest_json = if target.cli.dry_run {
        String::new()
    } else {
        let client = target.client(out)?;
        normalized_manifest(
            out,
            target,
            &client,
            &packed.manifest,
            &packed.name,
            &packed.version,
            &packed.tarball,
            &packed.files,
            packed.readme.as_ref(),
            package_json.parent(),
        )
    };
    Ok(Publishing {
        name: packed.name,
        version: packed.version,
        tarball: packed.tarball,
        manifest_json,
        publish_script: packed.publish_script,
        postpublish_script: packed.postpublish_script,
        package_json: Some(package_json.to_path_buf()),
    })
}

struct TarEntry {
    path: String,
    size: u64,
    data: Option<Vec<u8>>,
}

/// The entries of a gzipped (or plain) npm tarball, first component stripped; the contents
/// of the top-level `package.json` and README are kept.
fn read_tarball_entries(out: &Out, bytes: &[u8]) -> FResult<Vec<TarEntry>> {
    let mut tar = Vec::new();
    let data: &[u8] = if bytes.starts_with(&[0x1f, 0x8b]) {
        flate2::read::MultiGzDecoder::new(bytes)
            .read_to_end(&mut tar)
            .map_err(|e| out.error(&format!("failed to read tarball: {e}")))?;
        &tar
    } else {
        bytes
    };
    let mut entries: Vec<TarEntry> = Vec::new();
    let mut parser = fnpm_store::tar::TarParser::new();
    let mut keep = false;
    parser
        .feed(data, &mut |event| {
            use fnpm_store::tar::TarEvent;
            match event {
                TarEvent::Entry(h) => {
                    keep = !h.path.contains('/')
                        && (h.path.eq_ignore_ascii_case("package.json")
                            || pack::is_readme_filename(&h.path));
                    entries.push(TarEntry {
                        path: h.path,
                        size: h.size,
                        data: keep.then(Vec::new),
                    });
                }
                TarEvent::Data(d) => {
                    if keep && let Some(last) = entries.last_mut().and_then(|e| e.data.as_mut()) {
                        last.extend_from_slice(d);
                    }
                }
                _ => {}
            }
            Ok(())
        })
        .map_err(|e| out.error(&format!("failed to read tarball: {e}")))?;
    Ok(entries)
}

/// `fnpm publish <tarball>`.
async fn from_tarball(cx: &Ctx, out: &Out, target: &mut Target, tarball_path: &str) -> XResult<()> {
    let cwd = super::pack::walk::os_bytes(cx.req.cwd.as_os_str());
    let read = match pack::tarball::join_abs(&cwd, tarball_path.as_bytes()) {
        Some(abs) => std::fs::read(super::pack::walk::bytes_path(&abs)),
        None => Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG)),
    };
    let bytes = read.map_err(|e| {
        out.sys(
            &e,
            "open",
            &format!("failed to read tarball: '{tarball_path}'"),
        )
    })?;
    let entries = read_tarball_entries(out, &bytes)?;
    let mut text = String::from("\n");
    let mut unpacked = 0u64;
    let mut files = Vec::new();
    let mut package_json: Option<String> = None;
    let mut readme: Option<(String, String)> = None;
    for entry in &entries {
        unpacked += entry.size;
        text.push_str(&format!(
            "{} {} {}\n",
            out.paint("1;36", "packed"),
            fmt_size(entry.size),
            escape_control(&entry.path)
        ));
        files.push(entry.path.clone());
        if let Some(data) = &entry.data {
            if entry.path.eq_ignore_ascii_case("package.json") {
                if package_json.is_none() {
                    package_json = Some(String::from_utf8_lossy(data).into_owned());
                }
            } else if readme.is_none() {
                readme = Some((
                    entry.path.clone(),
                    String::from_utf8_lossy(data).into_owned(),
                ));
            }
        }
    }
    out.print(&text);
    let Some(text) = package_json else {
        return Err(out
            .error(&format!(
                "failed to find package.json in tarball '{tarball_path}'"
            ))
            .into());
    };
    let json = json::parse(&text).map_err(|e| {
        Fatal(format!(
            "{} {}\n{}",
            out.paint_err("31", "error:"),
            e.message,
            out.error("failed to parse tarball package.json").0
        ))
    })?;
    if json.get("private").is_some_and(Json::is_truthy) {
        return Err(out.error("attempted to publish a private package").into());
    }
    let name = match json.get("name").and_then(Json::as_str) {
        Some(n) => n.to_owned(),
        None => return Err(publish_name_error(out, NameError::MissingName).into()),
    };
    let is_scoped = fnpm_types::name::is_scoped_package_name(&name)
        .map_err(|_| publish_name_error(out, NameError::InvalidName))?;
    target.apply_publish_config(out, &json, &name)?;
    if let Some(asked_by) = target.provenance_asked_by(&json) {
        return Err(out
            .error(&format!(
                "{asked_by} is not supported yet: fnpm publish cannot generate provenance statements"
            ))
            .into());
    }
    if target.access.as_deref() == Some("restricted") && !is_scoped {
        return Err(out
            .error("unable to restrict access to unscoped package")
            .into());
    }
    let version = match json.get("version").and_then(Json::as_str) {
        Some(v) => v,
        None => return Err(publish_name_error(out, NameError::MissingVersion).into()),
    };
    let version = published_version(version)
        .ok_or_else(|| publish_name_error(out, NameError::InvalidVersion))?;
    target.refuse_tag_for(out, &version)?;

    use sha1::Digest as _;
    let shasum: [u8; 20] = sha1::Sha1::digest(&bytes).into();
    let integrity: [u8; 64] = sha2::Sha512::digest(&bytes).into();
    let client = target.client(out)?;
    let manifest_json = normalized_manifest(
        out,
        target,
        &client,
        &json,
        &name,
        &version,
        &bytes,
        &files,
        readme.as_ref(),
        None,
    );
    if !matches!(target.cli.log_level, LogLevel::Silent | LogLevel::Quiet) {
        let label = |s: &str| out.paint("1;34", s);
        out.print(&format!(
            "\n{}: {}\n{}: {}\n{}: sha512-{}[...]{}\n{}: {}\n{}: {}\n",
            label("Total files"),
            entries.len(),
            label("Shasum"),
            hex::encode(shasum),
            label("Integrity"),
            &pack::integrity_b64(&integrity)[..13],
            {
                let b = pack::integrity_b64(&integrity);
                b[b.len() - 15..].to_owned()
            },
            label("Unpacked size"),
            fmt_size(unpacked),
            label("Packed size"),
            fmt_size(bytes.len() as u64),
        ));
    }
    let p = Publishing {
        name,
        version,
        tarball: bytes,
        manifest_json,
        publish_script: None,
        postpublish_script: None,
        package_json: None,
    };
    if send(out, target, &client, &p).await? {
        print_published(out, target, &p);
    }
    Ok(())
}

async fn publish(cx: &Ctx, out: &Out) -> XResult<()> {
    let cli = parse_cli(cx, out)?;
    let Some(package_json) = find_package_json(&cx.req.cwd) else {
        return Err(out.error("missing package.json, nothing to publish").into());
    };
    let project = cx
        .project_anywhere()
        .map_err(|e| out.error(&format!("failed to initialize fnpm install: {e}")))?;
    let base = (*project.config).clone();
    let mut target = Target::new(base, cli.clone(), cx.env.clone());

    if let Some(tarball) = cx.req.positionals.first() {
        if cli.workspaces {
            return Err(out
                .error("--recursive and --filter cannot be used with a tarball path")
                .into());
        }
        return from_tarball(cx, out, &mut target, tarball).await;
    }
    if cli.workspaces {
        return publish_workspaces(cx, out, target, &project.root_dir).await;
    }
    let ignore_scripts = pack::ignore_scripts(cx);
    let p = pack_for_publish(cx, out, &mut target, &package_json).await?;
    let client = target.client(out)?;
    if send(out, &target, &client, &p).await? {
        finish_directory_publish(cx, &target, &p, ignore_scripts).await?;
    }
    Ok(())
}

/// `-r` / `--filter`: every selected workspace in dependency order. Private ones and versions
/// the registry has are skipped; all are packed before the first is published.
async fn publish_workspaces(cx: &Ctx, out: &Out, base: Target, root_dir: &Path) -> XResult<()> {
    let (ws, _) = fnpm_workspace::Workspace::load_with(root_dir, &Default::default())
        .map_err(|e| out.error(&format!("failed to select workspaces: {e}")))?;
    let filters = cx.strings("filter");
    let options = fnpm_workspace::SelectOptions::default();
    let selected: Vec<usize> = if filters.is_empty() {
        std::iter::once(usize::MAX)
            .chain(0..ws.members.len())
            .collect()
    } else {
        let selectors =
            fnpm_workspace::Selector::parse_all(&filters).map_err(|e| out.error(&e.to_string()))?;
        let selection = fnpm_workspace::select_with(&ws, &selectors, &cx.req.cwd, &options);
        if selection.selected.is_empty() {
            let patterns: Vec<&str> = filters.iter().map(String::as_str).collect();
            return Err(out
                .error(&fnpm_workspace::unmatched_message(&patterns))
                .into());
        }
        selection.selected
    };
    let order = ws.dependency_order(&selected, &options);
    let ignore_scripts = pack::ignore_scripts(cx);
    let mut packed: Vec<(Target, Publishing)> = Vec::new();
    for index in order {
        let manifest_path = if index == usize::MAX {
            ws.root_dir.join("package.json")
        } else {
            ws.members[index].dir.join("package.json")
        };
        let (_, json) = pack::read_package_json(out, &manifest_path)?;
        let name = json
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let version = json
            .get("version")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        if json.get("private").is_some_and(Json::is_truthy) {
            let shown = if name.is_empty() {
                manifest_path.to_string_lossy().into_owned()
            } else {
                name.clone()
            };
            out.println(&out.paint("2", &format!("skipped {}: private", escape_control(&shown))));
            continue;
        }
        let mut target = Target::new(base.base.clone(), base.cli.clone(), base.env.clone());
        if !name.is_empty()
            && let Some(v) = published_version(&version)
        {
            let v = fnpm_registry::publish::without_build_tag(&v).to_owned();
            target.apply_publish_config(out, &json, &name)?;
            if !target.has_credentials(&name) {
                return Err(out
                    .error(&format!(
                        "missing authentication (run {})",
                        out.paint_err("36", "`fnpm x npm login`")
                    ))
                    .into());
            }
            let client = target.client(out)?;
            let notice = |n: &str| print_notice(out, n);
            if client.version_exists_noticed(&name, &v, &notice).await {
                out.println(&out.paint(
                    "2",
                    &format!(
                        "skipped {}@{}: already on the registry",
                        escape_control(&name),
                        escape_control(&v)
                    ),
                ));
                continue;
            }
            target = Target::new(base.base.clone(), base.cli.clone(), base.env.clone());
        }
        out.println(&format!("\n{}", out.paint("1;35", &escape_control(&name))));
        let p = pack_for_publish(cx, out, &mut target, &manifest_path).await?;
        packed.push((target, p));
    }
    for (target, p) in &packed {
        out.println(&format!(
            "\n{}",
            out.paint("1;35", &escape_control(&p.name))
        ));
        let client = target.client(out)?;
        if send(out, target, &client, p).await? {
            finish_directory_publish(cx, target, p, ignore_scripts).await?;
        }
    }
    Ok(())
}
