//! `.npmrc` (npm `ini` semantics, bun `src/ini/lib.rs`): parsing, the settings bun reads from
//! it, and the per-URL credential lines collapsed across every file like npm's config map.

use std::path::{Path, PathBuf};

use crate::auth::{RegistryKey, RegistrySource, base64_decode_lenient, normalize_key};
use crate::files::{InstallFile, resolve_path};
use crate::ini::{self, IniObject, IniValue, trim_js};
use crate::{Env, LinkerSetting, Result};

/// One parsed `.npmrc`: flat `key = value` pairs in file order (later wins), env-expanded.
/// Sections (`[foo]`) prefix keys with `foo.` like npm's ini; array values (`key[]`) yield one
/// entry per item. [`tree`](Self::tree) is the object npm's `ini.decode` returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Npmrc {
    /// `key = value` pairs, file order.
    pub entries: Vec<(String, String)>,
    tree: IniObject,
    path: Option<PathBuf>,
}

fn flatten(prefix: &str, obj: &IniObject, out: &mut Vec<(String, String)>) {
    for (k, v) in obj.iter() {
        let key = if prefix.is_empty() {
            k.to_owned()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            IniValue::Object(o) => flatten(&key, o, out),
            IniValue::Array(items) => {
                for item in items {
                    match item {
                        IniValue::Object(o) => flatten(&key, o, out),
                        other => out.push((key.clone(), other.to_string())),
                    }
                }
            }
            other => out.push((key, other.to_string())),
        }
    }
}

impl Npmrc {
    /// Parses text. Values: quoted strings unescaped, `key[] = v` appends, `true`/`false`
    /// stay strings in [`entries`](Self::entries) (callers interpret), `${VAR}` expanded from
    /// `env`. Never fails: like npm, unreadable lines are skipped.
    pub fn parse(text: &str, env: &Env) -> Result<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let tree = ini::parse(text, env);
        let mut entries = Vec::new();
        flatten("", &tree, &mut entries);
        Ok(Self {
            entries,
            tree,
            path: None,
        })
    }

    /// Reads and parses `path`. Like npm (and bun), a file that cannot be read counts as
    /// missing: `Ok(None)`.
    pub fn load(path: &Path, env: &Env) -> Result<Option<Self>> {
        let Ok(bytes) = std::fs::read(path) else {
            return Ok(None);
        };
        // bun converts a file that starts with the UTF-16LE byte order mark (Notepad's
        // "Unicode") to UTF-8 first.
        let text = match bytes.strip_prefix(&[0xff, 0xfe]) {
            Some(utf16) => std::borrow::Cow::Owned(String::from_utf16_lossy(
                &utf16
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| u16::from_le_bytes(c))
                    .collect::<Vec<_>>(),
            )),
            None => String::from_utf8_lossy(&bytes),
        };
        let mut rc = Self::parse(&text, env)?;
        rc.path = Some(path.to_path_buf());
        Ok(Some(rc))
    }

    /// The last value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// All values of `key[]` (or repeated `key`).
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The decoded object, with JSON types (`true`, `null`, quoted JSON arrays) preserved.
    pub fn tree(&self) -> &IniObject {
        &self.tree
    }

    /// The file this was read from.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// npm's Boolean coercion of a config value (`@npmcli/config` parse-field, then nopt).
pub fn npmrc_bool(value: &IniValue) -> Option<bool> {
    match value {
        IniValue::Bool(b) => Some(*b),
        IniValue::Null => Some(false),
        IniValue::Number(n) => Some(*n != 0.0),
        IniValue::Object(_) => Some(true),
        IniValue::String(s) => Some(npm_config_bool(s)),
        IniValue::Array(_) => None,
    }
}

/// [`npmrc_bool`] of a string, which is all `npm_config_<key>` in the environment can hold:
/// empty is `true`, `false`/`null`/`undefined` are `false`, a numeric string is
/// `!!Number(value)`, anything else is `true`.
pub fn npm_config_bool(value: &str) -> bool {
    let value = trim_js(value);
    if value.is_empty() {
        return true;
    }
    if matches!(value, "false" | "null" | "undefined") {
        return false;
    }
    js_string_to_number(value) != 0.0
}

/// ECMAScript `StringToNumber`, enough to tell zero from non-zero and NaN.
pub(crate) fn js_string_to_number(text: &str) -> f64 {
    let text = trim_js(text);
    if text.is_empty() {
        return 0.0;
    }
    let radix = match text.get(..2) {
        Some("0x" | "0X") => 16,
        Some("0o" | "0O") => 8,
        Some("0b" | "0B") => 2,
        _ => 0,
    };
    if radix != 0 {
        let digits = &text[2..];
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return f64::NAN;
        }
        return digits.chars().fold(0.0, |acc, c| {
            acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
        });
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == "Infinity" {
        return if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let valid = !unsigned.is_empty()
        && unsigned
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        && unsigned
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit() || b == b'.');
    if !valid {
        return f64::NAN;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

/// The per-registry option of a `//<key>:<opt>=` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigOpt {
    Auth,
    AuthToken,
    Username,
    Password,
    Email,
    Certfile,
    Keyfile,
}

impl ConfigOpt {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "_auth" => Self::Auth,
            "_authToken" => Self::AuthToken,
            "username" => Self::Username,
            "_password" => Self::Password,
            "email" => Self::Email,
            "certfile" => Self::Certfile,
            "keyfile" => Self::Keyfile,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Auth => "_auth",
            Self::AuthToken => "_authToken",
            Self::Username => "username",
            Self::Password => "_password",
            Self::Email => "email",
            Self::Certfile => "certfile",
            Self::Keyfile => "keyfile",
        }
    }
}

/// One `//<key>:<opt>=<value>` line. The lines of every `.npmrc` go into one list, as npm
/// collapses its files into one map.
#[derive(Debug, Clone)]
struct ConfigItem {
    key: String,
    opt: ConfigOpt,
    value: String,
    file: usize,
}

/// `word` spells a credential option once case, `_` and `-` are ignored.
fn folds_to_credential_option(word: &str) -> bool {
    let fold = |s: &str| -> String {
        s.chars()
            .filter(|&c| c != '_' && c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let folded = fold(word);
    !folded.is_empty()
        && ["_authToken", "_auth", "username", "_password"]
            .iter()
            .any(|n| fold(n) == folded)
}

/// Patterns of `public-hoist-pattern` / `hoist-pattern`: a string or an array of strings.
/// Anything else drops the setting.
fn patterns_of(value: &IniValue) -> Option<Vec<String>> {
    match value {
        IniValue::Array(items) => items
            .iter()
            .map(|i| i.as_str().map(str::to_owned))
            .collect(),
        IniValue::String(s) => Some(vec![s.clone()]),
        _ => None,
    }
}

/// Applies one file's options, `registry=` and `@scope:registry=` lines to `install`, and
/// collects its `//host/…:<opt>=` lines onto `configs`.
fn parse_npmrc_into(
    install: &mut InstallFile,
    rc: &Npmrc,
    file: usize,
    home: Option<&Path>,
    configs: &mut Vec<ConfigItem>,
    warnings: &mut Vec<String>,
) {
    let out = rc.tree();
    let display = rc
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".npmrc".to_owned());
    // Relative paths in a `.npmrc` are resolved against the directory that contains it.
    let dir = rc
        .path()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();

    if let Some(IniValue::String(url)) = out.get("registry") {
        let mut registry = RegistrySource::from_url(url);
        registry.declared_in_npmrc = true;
        install.registry = Some(registry);
    }

    match out.get("cache") {
        Some(IniValue::String(s)) => install.cache_dir = Some(resolve_path(&dir, s, home)),
        Some(IniValue::Bool(b)) => install.disable_cache = Some(!b),
        _ => {}
    }

    match out.get("dry-run") {
        Some(IniValue::String(s)) => install.dry_run = Some(s == "true"),
        Some(IniValue::Bool(b)) => install.dry_run = Some(*b),
        _ => {}
    }

    let ca = out.get("ca");
    let cafile = out.get("cafile");
    // `ca` and `cafile` are one setting, so a file that sets either one replaces both.
    if ca.is_some() || cafile.is_some() {
        install.ca = None;
        install.cafile = None;
    }
    match ca {
        Some(IniValue::String(s)) => install.ca = Some(vec![s.clone()]),
        Some(IniValue::Array(items)) => {
            install.ca = Some(
                items
                    .iter()
                    .filter_map(|i| i.as_str().map(str::to_owned))
                    .collect(),
            );
        }
        _ => {}
    }
    if let Some(IniValue::String(s)) = cafile {
        install.cafile = Some(resolve_path(&dir, s, home));
    }

    for (key, save) in [("omit", false), ("include", true)] {
        let Some(value) = out.get(key) else { continue };
        let items: &[IniValue] = match value {
            IniValue::Array(items) => items,
            other => std::slice::from_ref(other),
        };
        for item in items {
            match item.as_str() {
                Some("dev") => install.dev = Some(save),
                Some("peer") => install.peer = Some(save),
                Some("optional") => install.optional = Some(save),
                _ => {}
            }
        }
    }

    for (key, slot) in [
        ("ignore-scripts", &mut install.ignore_scripts),
        (
            "link-workspace-packages",
            &mut install.link_workspace_packages,
        ),
        ("save-exact", &mut install.exact),
        ("provenance", &mut install.provenance),
        ("hoist", &mut install.hoist),
        ("strict-ssl", &mut install.strict_ssl),
    ] {
        if let Some(value) = out.get(key).and_then(npmrc_bool) {
            *slot = Some(value);
        }
    }

    if let Some(IniValue::String(s)) = out.get("install-strategy") {
        match s.as_str() {
            "hoisted" => install.linker = Some(LinkerSetting::Hoisted),
            "linked" => install.linker = Some(LinkerSetting::Isolated),
            // `nested` and `shallow` have no equivalent.
            _ => {}
        }
    }
    // yarn & pnpm option
    if let Some(IniValue::String(s)) = out.get("node-linker") {
        match s.as_str() {
            "pnpm" | "isolated" => install.linker = Some(LinkerSetting::Isolated),
            "node-modules" | "hoisted" => install.linker = Some(LinkerSetting::Hoisted),
            _ => {}
        }
    }

    for (key, slot) in [
        ("public-hoist-pattern", &mut install.public_hoist_pattern),
        ("hoist-pattern", &mut install.hoist_pattern),
    ] {
        if let Some(value) = out.get(key) {
            *slot = patterns_of(value);
        }
    }

    for (key, slot) in [
        ("proxy", &mut install.proxy),
        ("https-proxy", &mut install.https_proxy),
    ] {
        match out.get(key) {
            Some(IniValue::String(s)) if !s.is_empty() => *slot = Some(s.clone()),
            // `proxy=false` / `proxy=null` turn a proxy off.
            Some(IniValue::Bool(false) | IniValue::Null) => *slot = Some(String::new()),
            _ => {}
        }
    }
    if let Some(value) = out.get("noproxy") {
        let items: &[IniValue] = match value {
            IniValue::Array(items) => items,
            other => std::slice::from_ref(other),
        };
        let list = items
            .iter()
            .filter_map(IniValue::as_str)
            .flat_map(|s| s.split([',', ' ']))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        install.noproxy = Some(list);
    }

    if let Some(value) = out.get("minimum-release-age") {
        // npm's value is in minutes (npm 11.x `min-release-age` / pnpm `minimum-release-age`).
        let minutes = match value {
            IniValue::Number(n) => Some(*n),
            IniValue::String(s) => Some(js_string_to_number(s)),
            _ => None,
        };
        if let Some(m) = minutes.filter(|m| m.is_finite() && *m >= 0.0) {
            install.minimum_release_age_ms = Some((m * 60_000.0).round() as u64);
        }
    }

    let scopes = install.scopes.get_or_insert_with(Default::default);
    for (key, value) in out.iter() {
        // A known option without a string value (`=true`, no `=`) is not a typo.
        let IniValue::String(value) = value else {
            continue;
        };

        if let Some(scope) = key
            .strip_prefix('@')
            .and_then(|k| k.strip_suffix(":registry"))
        {
            let mut registry = RegistrySource::from_url(value);
            registry.declared_in_npmrc = true;
            scopes.insert(scope.to_owned(), registry);
            continue;
        }

        // `//<key>:<option>`. No option name holds a colon.
        if !key.starts_with("//") {
            continue;
        }
        let Some(colon) = key.rfind(':') else {
            continue;
        };
        if colon < 2 {
            continue;
        }
        let word = &key[colon + 1..];
        let Some(opt) = ConfigOpt::parse(word) else {
            // A `/` right before the colon: in `//host/:_auth:dXNl…`, the word is the credential.
            if key.as_bytes()[colon - 1] == b'/'
                && !word.contains('/')
                && folds_to_credential_option(word)
            {
                // bun points at the line, like its other config diagnostics.
                let warning = match rc.path().and_then(|p| key_location(p, key)) {
                    Some((line, col)) => format!(
                        "ignoring unknown .npmrc option {word}\n    at {display}:{line}:{col}"
                    ),
                    None => format!("{display}: ignoring unknown .npmrc option {word}"),
                };
                warnings.push(warning);
            }
            continue;
        };
        if matches!(opt, ConfigOpt::Certfile | ConfigOpt::Keyfile) {
            warnings.push(format!(
                "{display}: {} is not supported; ignoring this .npmrc option",
                opt.name()
            ));
            continue;
        }
        configs.push(ConfigItem {
            key: normalize_key(&key[2..colon]),
            opt,
            value: value.clone(),
            file,
        });
    }
}

/// The 1-based line and column of the last `key=` line of the file at `path`. Read again only
/// for a diagnostic, so parsing keeps no text around.
fn key_location(path: &Path, key: &str) -> Option<(usize, usize)> {
    let text = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&text);
    let mut found = None;
    for (i, line) in text.lines().enumerate() {
        let start = line.len() - line.trim_start().len();
        if let Some((k, _)) = line[start..].split_once('=')
            && k.trim_end() == key
        {
            found = Some((i + 1, start + 1));
        }
    }
    found
}

/// npm's config is a flat map, so a key repeated across files collapses to the last one read.
fn lookup<'a>(configs: &'a [ConfigItem], key: &str, opt: ConfigOpt) -> Option<&'a ConfigItem> {
    configs.iter().rev().find(|c| c.opt == opt && c.key == key)
}

/// npm's `Buffer.from(value, "base64")`. A value with CR, LF or NUL is kept as it is so the
/// credential check refuses it.
fn decode_password(value: &str) -> String {
    if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return value.to_owned();
    }
    String::from_utf8_lossy(&base64_decode_lenient(value)).into_owned()
}

/// Follows the order of npm's `hasAuth`, without `certfile`/`keyfile` (no mTLS).
fn credentials_of_key(configs: &[ConfigItem], key: &str) -> Option<RegistrySource> {
    // npm's `opts[k]` truthiness test, applied after the collapse.
    let truthy = |opt| {
        lookup(configs, key, opt)
            .map(|c| c.value.as_str())
            .filter(|v| !v.is_empty())
    };
    let mut credentials = RegistrySource::default();
    if let Some(token) = truthy(ConfigOpt::AuthToken) {
        credentials.token = token.to_owned();
    } else if let Some(auth) = truthy(ConfigOpt::Auth) {
        credentials.auth = auth.to_owned();
    } else {
        credentials.username = truthy(ConfigOpt::Username)?.to_owned();
        credentials.password = decode_password(truthy(ConfigOpt::Password)?);
    }
    Some(credentials)
}

/// The `email` configured for a key (the deepest one along the walk wins at lookup time).
fn emails(configs: &[ConfigItem]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for c in configs.iter().filter(|c| c.opt == ConfigOpt::Email) {
        match out.iter_mut().find(|(k, _)| *k == c.key) {
            Some(slot) => slot.1 = c.value.clone(),
            None => out.push((c.key.clone(), c.value.clone())),
        }
    }
    out
}

fn url_or_default(url: &str) -> &str {
    if url.is_empty() {
        crate::DEFAULT_REGISTRY
    } else {
        url
    }
}

/// Fills `install.url_auth` (one entry per key with a complete credential, after every file
/// was collapsed into one map) and diagnoses an empty `_auth` that names a known registry.
fn resolve_url_auth(
    install: &mut InstallFile,
    bunfig: &InstallFile,
    configs: &[ConfigItem],
    names: &[String],
    warnings: &mut Vec<String>,
) {
    install.url_auth.clear();
    for c in configs {
        if install.url_auth.iter().any(|(k, _)| *k == c.key) {
            continue;
        }
        if let Some(credentials) = credentials_of_key(configs, &c.key) {
            install.url_auth.push((c.key.clone(), credentials));
        }
    }
    install.emails = emails(configs);

    let empty: Vec<&ConfigItem> = configs
        .iter()
        .filter(|item| {
            item.opt == ConfigOpt::Auth
                && item.value.is_empty()
                && lookup(configs, &item.key, ConfigOpt::Auth)
                    .is_some_and(|c| std::ptr::eq(c, *item))
        })
        .collect();
    if empty.is_empty() {
        return;
    }
    let default_url = install.registry.as_ref().map_or("", |r| r.url.as_str());
    let mut urls: Vec<&str> = vec![url_or_default(default_url)];
    urls.extend(
        install
            .scopes
            .iter()
            .flat_map(|s| s.values())
            .map(|r| r.url.as_str()),
    );
    // No `.npmrc` line applies to a registry that the config file gave credentials too.
    urls.extend(
        bunfig
            .registry
            .iter()
            .chain(bunfig.scopes.iter().flat_map(|s| s.values()))
            .filter(|r| !r.has_credentials())
            .map(|r| url_or_default(&r.url)),
    );
    let registries: Vec<RegistryKey> = urls.into_iter().map(RegistryKey::from_url).collect();
    for item in empty {
        // Without its trailing `/` the key is a distinct one.
        if registries
            .iter()
            .any(|r| item.key == r.key() || item.key == r.key()[..r.key().len() - 1])
        {
            let file = names.get(item.file).map_or(".npmrc", String::as_str);
            warnings.push(format!("{file}: ignoring empty _auth value"));
        }
    }
}

/// Reads every `.npmrc` in `paths` (lowest precedence first) into one settings layer.
/// `bunfig` only tells the diagnostics which registries exist. Like npm, skips a file that
/// cannot be read.
pub(crate) fn load_npmrc_files(
    paths: &[PathBuf],
    env: &Env,
    bunfig: &InstallFile,
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) -> InstallFile {
    let mut install = InstallFile::default();
    let mut configs = Vec::new();
    let mut names = Vec::new();
    for path in paths {
        let Ok(Some(rc)) = Npmrc::load(path, env) else {
            continue;
        };
        names.push(path.display().to_string());
        parse_npmrc_into(
            &mut install,
            &rc,
            names.len() - 1,
            home,
            &mut configs,
            warnings,
        );
    }
    resolve_url_auth(&mut install, bunfig, &configs, &names, warnings);
    install
}

/// [`load_npmrc_files`] for in-memory text (bun `iniInternals.loadNpmrc` parity).
#[cfg(test)]
pub(crate) fn settings_from_text(text: &str, env: &Env, warnings: &mut Vec<String>) -> InstallFile {
    let mut install = InstallFile::default();
    let mut configs = Vec::new();
    if let Ok(rc) = Npmrc::parse(text, env) {
        parse_npmrc_into(
            &mut install,
            &rc,
            0,
            env.home().as_deref(),
            &mut configs,
            warnings,
        );
    }
    resolve_url_auth(
        &mut install,
        &InstallFile::default(),
        &configs,
        &[".npmrc".to_owned()],
        warnings,
    );
    install
}

#[cfg(test)]
#[path = "tests/npmrc.rs"]
mod tests;
