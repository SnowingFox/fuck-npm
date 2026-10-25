//! yarn 2+ ("berry") `yarn.lock` migration (bun `install/yarn_berry.rs` and
//! `yarn_berry/builtin_extensions.rs`). Read-only: fnpm never writes berry lockfiles.
//!
//! Protocols: `npm:`, `file:`, `portal:` (copied as a folder), `link:`, `patch:` (folded into
//! `patchedDependencies`), git (`#commit=`), `http(s):` tarballs, `workspace:`. A pin fnpm cannot
//! keep fails the migration with bun's message instead of being silently re-resolved.

mod builtin_extensions;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use fnpm_types::overrides::parse_package_segment;
use fnpm_types::spec::{is_tarball, link_path_for_lockfile};
use fnpm_types::{
    Behavior, Bin, ConfigVersion, CpuSet, DepEdge, DependencySpec, LibcSet, LockGraph, OsSet,
    Package, PackageId, PlatformConstraints, Resolution, SpecTag, infer_tag,
};
use fnpm_workspace::Workspace;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde_json::{Map, Value};

use super::foreign::yaml::{self, Node};
use super::foreign::{
    self, DiskFiles, MigrateOptions, Migration, PackageJsonWriteBack, manifest, semver,
};
use crate::{Error, Result};

const FILE: &str = "yarn.lock";

/// Migrates with the process environment and the real filesystem.
pub fn parse(bytes: &[u8], workspace: &Workspace) -> Result<LockGraph> {
    let files = DiskFiles;
    Ok(migrate(bytes, workspace, &MigrateOptions::from_process(&files))?.graph)
}

fn fail(ws: &Workspace, message: impl Into<String>) -> Error {
    foreign::parse_error(ws, FILE, message)
}

/// The text with control characters escaped, for messages.
fn shown(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_debug().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// A `<<` merge key (or an escape that spells `<`): YAML parsers expand it, yarn reads it as an
/// ordinary key.
fn may_hold_merge_key(data: &str) -> bool {
    if data.contains("<<") {
        return true;
    }
    let b = data.as_bytes();
    let mut i = 0;
    while let Some(off) = memchr::memchr(b'\\', &b[i..]) {
        i += off + 1;
        let hex_len = match b.get(i) {
            Some(b'x') => 2,
            Some(b'u') => 4,
            Some(b'U') => 8,
            Some(b'\n' | b'\r') => return true,
            _ => continue,
        };
        let Some(hex) = b.get(i + 1..i + 1 + hex_len) else {
            continue;
        };
        let code = hex
            .iter()
            .try_fold(0u32, |n, &c| (c as char).to_digit(16).map(|v| n * 16 + v));
        if code == Some(u32::from(b'<')) {
            return true;
        }
    }
    false
}

/// Text with a malformed escape is returned as written; it then matches nothing.
fn decoded(s: &str) -> String {
    percent_decode_str(s)
        .decode_utf8()
        .map_or_else(|_| s.to_owned(), |c| c.into_owned())
}

/// encodeURIComponent.
const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

fn encode_component(s: &str) -> String {
    utf8_percent_encode(s, URI_COMPONENT).to_string()
}

/// `name@rest`, keeping a leading scope `@`.
fn split_locator(spec: &str) -> Option<(&str, &str)> {
    let from = usize::from(spec.starts_with('@'));
    let i = spec[from..].find('@')? + from;
    Some((&spec[..i], &spec[i + 1..]))
}

type Params = Vec<(String, String)>;

/// `npm:1.2.3::__archiveUrl=...` gives (`npm:1.2.3`, params).
fn split_reference_params(reference: &str) -> (&str, Params) {
    let Some((head, tail)) = reference.split_once("::") else {
        return (reference, Vec::new());
    };
    let params = tail
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_owned(), decoded(v)))
        .collect();
    (head, params)
}

fn param<'p>(params: &'p Params, key: &str) -> Option<&'p str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// An RFC 3986 `scheme:` prefix (`npm:`, `workspace:`, `git+ssh:`), not `@scope/x` or `1.2.3`.
fn has_protocol(spec: &str) -> bool {
    match spec.find(':') {
        Some(i) if i > 0 => {
            let b = spec.as_bytes();
            b[0].is_ascii_alphabetic()
                && b[..i]
                    .iter()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        }
        _ => false,
    }
}

/// `npm:^1.2.3` gives `^1.2.3`; the alias `npm:other@^1` is unchanged.
fn strip_npm_protocol(spec: &str) -> &str {
    let Some(rest) = spec.strip_prefix("npm:") else {
        return spec;
    };
    let is_alias = split_locator(rest).is_some_and(|(n, _)| !n.is_empty());
    if is_alias { spec } else { rest }
}

/// `os=darwin & cpu=arm64`, `(os=linux | os=win32) & cpu=x64`, `... & libc=glibc`.
fn parse_conditions(cond: &str) -> PlatformConstraints {
    let (mut os, mut cpu, mut libc) = (Vec::new(), Vec::new(), Vec::new());
    for alt in cond.split(['&', '|']) {
        let alt = alt.trim_matches([' ', '(', ')', '\t']);
        if let Some(v) = alt.strip_prefix("os=") {
            os.push(v);
        } else if let Some(v) = alt.strip_prefix("cpu=") {
            cpu.push(v);
        } else if let Some(v) = alt.strip_prefix("libc=") {
            libc.push(v);
        }
    }
    PlatformConstraints {
        os: if os.is_empty() {
            OsSet::ALL
        } else {
            OsSet::from_list(&os)
        },
        cpu: if cpu.is_empty() {
            CpuSet::ALL
        } else {
            CpuSet::from_list(&cpu)
        },
        libc: if libc.is_empty() {
            LibcSet::NONE
        } else {
            LibcSet::from_list(&libc)
        },
    }
}

/// A boolean as yarn reads it from the environment or a quoted value.
fn yarn_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
struct Extension {
    /// `name@range` as written
    key: String,
    dependencies: Vec<(String, String)>,
    peer_dependencies: Vec<(String, String)>,
    optional_peers: Vec<String>,
}

#[derive(Debug, Default)]
struct YarnRc {
    /// catalog group (`""` for the default `catalog:`) to name to range, in file order
    catalogs: Vec<(String, Vec<(String, String)>)>,
    registry: Option<String>,
    /// `npmScopes.<scope>.npmRegistryServer`, keyed without the `@`
    scope_registries: BTreeMap<String, String>,
    extensions: Vec<Extension>,
    /// `enableTransparentWorkspaces`; unless false, a range a workspace's version satisfies
    /// resolves to that workspace.
    transparent_workspaces: Option<bool>,
}

/// The names under `meta_key` (`dependenciesMeta` / `peerDependenciesMeta`) with
/// `optional: true`.
fn optional_names<'n>(entry: &'n Node, meta_key: &str) -> Vec<&'n str> {
    entry
        .get_or_empty(meta_key)
        .entries()
        .iter()
        .filter(|(_, v)| v.get("optional").and_then(Node::as_bool) == Some(true))
        .filter_map(|(k, _)| k.text())
        .collect()
}

fn ranges_of(value: &Node, group: &str) -> Vec<(String, String)> {
    value
        .get_or_empty(group)
        .entries()
        .iter()
        .filter_map(|(n, r)| Some((n.text()?.to_owned(), r.text()?.to_owned())))
        .collect()
}

/// The first value found wins: `YARN_*` variables, then the rc file of the project, of each
/// parent folder, and of the home folder. Catalogs only come from the project's rc file.
fn read_yarnrc(ws: &Workspace, options: &MigrateOptions<'_>) -> Result<YarnRc> {
    let mut rc = YarnRc {
        registry: options
            .env_var("YARN_NPM_REGISTRY_SERVER")
            .map(str::to_owned),
        transparent_workspaces: options
            .env_var("YARN_ENABLE_TRANSPARENT_WORKSPACES")
            .and_then(yarn_bool),
        ..Default::default()
    };
    let rc_name = options.env_var("YARN_RC_FILENAME").unwrap_or(".yarnrc.yml");
    let mut folder: Option<&Path> = Some(&ws.root_dir);
    let mut in_project = true;
    let mut read_home = false;
    while let Some(dir) = folder {
        read_yarnrc_file(ws, options, &mut rc, dir, rc_name, in_project)?;
        in_project = false;
        read_home |= options.home.as_deref() == Some(dir);
        folder = dir.parent();
    }
    if let (Some(home), false) = (&options.home, read_home) {
        read_yarnrc_file(ws, options, &mut rc, home, rc_name, false)?;
    }
    // Every rule is copied to package.json, so the next install has to read it the same way.
    for extension in &mut rc.extensions {
        for (name, range) in extension
            .dependencies
            .iter_mut()
            .chain(&mut extension.peer_dependencies)
        {
            if let Some(group) = range.strip_prefix("catalog:") {
                let found = rc
                    .catalogs
                    .iter()
                    .find(|(g, _)| g == group)
                    .and_then(|(_, m)| m.iter().find(|(n, _)| n == name));
                let Some((_, in_catalog)) = found else {
                    return Err(fail(
                        ws,
                        format!(
                            "packageExtensions \"{}\" in .yarnrc.yml adds \"{}@{}\", but the catalog has no \"{}\"",
                            shown(&extension.key),
                            shown(name),
                            shown(range),
                            shown(name)
                        ),
                    ));
                };
                *range = in_catalog.clone();
            }
            // package.json would read `npm:^1` as an alias of a package named `^1`.
            *range = strip_npm_protocol(range).to_owned();
        }
        if let Some(problem) = extension_problem(extension) {
            return Err(fail(
                ws,
                format!(
                    "packageExtensions \"{}\" in .yarnrc.yml: {problem}",
                    shown(&extension.key)
                ),
            ));
        }
    }
    Ok(rc)
}

/// Why an install would ignore this rule (bun `package_extensions::rule_problem`).
fn extension_problem(extension: &Extension) -> Option<&'static str> {
    let selector = parse_package_segment(&extension.key).ok()?;
    if !selector.range.is_empty() && semver::Range::parse(&selector.range).is_none() {
        return Some("the key's version range is not valid");
    }
    for (_, range) in extension
        .dependencies
        .iter()
        .chain(&extension.peer_dependencies)
    {
        let supported = match infer_tag(range) {
            SpecTag::Npm | SpecTag::DistTag | SpecTag::Git | SpecTag::Github => true,
            SpecTag::Tarball => fnpm_types::spec::is_remote_tarball(range),
            _ => false,
        };
        if !supported {
            return Some("an extension can only add registry, git or remote tarball dependencies");
        }
    }
    None
}

fn read_yarnrc_file(
    ws: &Workspace,
    options: &MigrateOptions<'_>,
    rc: &mut YarnRc,
    folder: &Path,
    rc_name: &str,
    in_project: bool,
) -> Result<()> {
    let path: PathBuf = folder.join(rc_name);
    let Some(data) = options.files.read(&path) else {
        return Ok(());
    };
    let root = yaml::parse_last(&data)
        .map_err(|_| fail(ws, format!("{} is not valid YAML", path.display())))?;
    if in_project {
        let mut add_group = |group: &str, obj: &Node| {
            if !obj.is_map() {
                return;
            }
            let index = match rc.catalogs.iter().position(|(g, _)| g == group) {
                Some(i) => i,
                None => {
                    rc.catalogs.push((group.to_owned(), Vec::new()));
                    rc.catalogs.len() - 1
                }
            };
            let map = &mut rc.catalogs[index].1;
            for (name, value) in obj.entries() {
                let (Some(name), Some(range)) = (name.text(), value.text()) else {
                    continue;
                };
                // yarn allows `npm:^1.0.0` here; in an fnpm catalog that would be an alias.
                let range = strip_npm_protocol(range).to_owned();
                match map.iter_mut().find(|(n, _)| n == name) {
                    Some(slot) => slot.1 = range,
                    None => map.push((name.to_owned(), range)),
                }
            }
        };
        if let Some(catalog) = root.get("catalog") {
            add_group("", catalog);
        }
        for (group, value) in root.get_or_empty("catalogs").entries() {
            if let Some(group) = group.text() {
                add_group(group, value);
            }
        }
    }
    for (key, value) in root.get_or_empty("packageExtensions").entries() {
        let Some(key) = key.text() else { continue };
        if rc.extensions.iter().any(|e| e.key == key) {
            continue;
        }
        rc.extensions.push(Extension {
            key: key.to_owned(),
            dependencies: ranges_of(value, "dependencies"),
            peer_dependencies: ranges_of(value, "peerDependencies"),
            optional_peers: optional_names(value, "peerDependenciesMeta")
                .into_iter()
                .map(str::to_owned)
                .collect(),
        });
    }
    if rc.transparent_workspaces.is_none() {
        rc.transparent_workspaces = root
            .get("enableTransparentWorkspaces")
            .and_then(|v| v.as_bool().or_else(|| v.text().and_then(yarn_bool)));
    }
    let registry_of = |owner: &Node| {
        owner
            .get_str("npmRegistryServer")
            .filter(|u| !u.is_empty())
            .map(str::to_owned)
    };
    if rc.registry.is_none() {
        rc.registry = registry_of(&root);
    }
    for (scope, value) in root.get_or_empty("npmScopes").entries() {
        if let (Some(scope), Some(url)) = (scope.as_str(), registry_of(value)) {
            rc.scope_registries.entry(scope.to_owned()).or_insert(url);
        }
    }
    Ok(())
}
/// `source` is a `&`-separated list of `~/x.patch` (relative to the project) and `./x.patch`
/// (relative to the `::locator=` package). Builtin patches are skipped. `None` when a relative
/// path's owner is not a project folder.
fn project_patch_paths(source: &str, params: &Params) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for one in source.split('&') {
        let one = one.strip_prefix("optional!").unwrap_or(one);
        if one.is_empty() || one.contains("builtin<") {
            continue;
        }
        if let Some(rest) = one.strip_prefix("~/") {
            paths.push(rest.to_owned());
        } else if one.starts_with('/') {
            paths.push(one.to_owned());
        } else {
            paths.push(foreign::join_posix(&owner_dir(params)?, one));
        }
    }
    Some(paths)
}

/// `<percent-encoded locator>#<source>::params` into the decoded locator, the decoded source,
/// and the params.
fn decode_patch_spec(rest: &str) -> (String, String, Params) {
    let (spec, params) = split_reference_params(rest);
    let (inner, source) = spec.split_once('#').unwrap_or((spec, ""));
    (decoded(inner), decoded(source), params)
}

/// The key of `locator_to_entry`: the locator without `::params`, percent-decoded.
fn locator_key(name: &str, reference: &str) -> String {
    let (head, _) = split_reference_params(reference);
    format!("{name}@{}", decoded(head))
}

/// `file:./x#./x` gives (`file:`, `./x`), also for `portal:` and `link:`.
fn split_folder_protocol(head: &str) -> Option<(&'static str, &str)> {
    let (protocol, path) = ["file:", "portal:", "link:"]
        .into_iter()
        .find_map(|p| head.strip_prefix(p).map(|r| (p, r)))?;
    Some((protocol, path.split_once('#').map_or(path, |(p, _)| p)))
}

/// The folder of a decoded `::locator=` value: `name@workspace:path` gives `path`, and
/// `name@portal:./p::locator=<owner>` gives `<owner dir>/p`.
fn locator_dir(locator: &str, depth: u8) -> Option<String> {
    let (_, reference) = split_locator(locator)?;
    if let Some(path) = reference.strip_prefix("workspace:") {
        return Some(if path == "." {
            String::new()
        } else {
            path.to_owned()
        });
    }
    let (head, params) = split_reference_params(reference);
    let (_, path) = split_folder_protocol(head)?;
    if is_tarball(path) {
        return None;
    }
    let base = match param(&params, "locator") {
        Some(owner) if depth < 16 => locator_dir(owner, depth + 1)?,
        Some(_) => return None,
        None => String::new(),
    };
    Some(foreign::join_posix(&base, path))
}

/// The folder a reference's relative paths start from: its `::locator=` package, else the root.
fn owner_dir(params: &Params) -> Option<String> {
    param(params, "locator").map_or(Some(String::new()), |owner| locator_dir(owner, 0))
}

/// The URL without `user:password@`, because tarball URLs built from it go into the lockfile.
fn registry_without_auth(href: &str) -> String {
    let Some((scheme, rest)) = href.split_once("://") else {
        return href.to_owned();
    };
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..host_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => href.to_owned(),
    }
}

fn same_registry(a: &str, b: &str) -> bool {
    foreign::url_is_under_registry(a, b) && foreign::url_is_under_registry(b, a)
}

/// The registry yarn's configuration names for `name` has to be the one fnpm uses.
fn registry_for(
    ws: &Workspace,
    options: &MigrateOptions<'_>,
    rc: &YarnRc,
    name: &str,
) -> Result<String> {
    let configured = registry_without_auth(options.registry_for(name));
    let scope = name
        .strip_prefix('@')
        .and_then(|n| n.split_once('/'))
        .map(|(s, _)| s);
    let from_yarn: Option<&str> = match scope.and_then(|s| rc.scope_registries.get(s)) {
        Some(url) => Some(url),
        // yarn's builtin default for the `@jsr` scope
        None if scope == Some("jsr") => Some("https://npm.jsr.io"),
        None => rc.registry.as_deref(),
    };
    let Some(url) = from_yarn else {
        return Ok(configured);
    };
    if url.contains("${") {
        return Err(fail(
            ws,
            format!(
                "yarn's registry for \"{}\" uses an environment variable (\"{}\"); fnpm cannot tell if it is the registry fnpm is configured with",
                shown(name),
                registry_without_auth(url)
            ),
        ));
    }
    let canonical = |url: &str| {
        let url = registry_without_auth(url);
        if same_registry(&url, "https://registry.yarnpkg.com") {
            foreign::DEFAULT_REGISTRY.to_owned()
        } else {
            url
        }
    };
    if same_registry(&canonical(url), &canonical(&configured)) {
        return Ok(configured);
    }
    Err(fail(
        ws,
        format!(
            "yarn fetches \"{}\" from {} and fnpm is configured to fetch it from {}; add the registry to fnpm.toml or .npmrc",
            shown(name),
            registry_without_auth(url),
            configured
        ),
    ))
}

/// yarn always locks git dependencies with `#commit=<sha>`; the prefixes cover hand edits.
fn is_git_reference(head: &str) -> bool {
    let url = head.split_once('#').map_or(head, |(u, _)| u);
    head.contains("#commit=")
        || ["git+", "git://", "github:", "ssh://", "git@"]
            .iter()
            .any(|p| head.starts_with(p))
        || ((url.starts_with("https://") || url.starts_with("http://")) && url.ends_with(".git"))
}

/// `github:o/r#commit=sha` and `[git+]<url>#commit=sha`, pinned to the commit yarn locked.
fn git_resolution(ws: &Workspace, name: &str, head: &str) -> Result<Resolution> {
    let (url, fragment) = head.split_once('#').unwrap_or((head, ""));
    let mut commit = fragment;
    for kv in fragment.split('&') {
        if let Some(c) = kv.strip_prefix("commit=") {
            commit = c;
        } else if kv.starts_with("workspace=") || kv.starts_with("cwd=") {
            return Err(fail(
                ws,
                format!(
                    "yarn.lock entry \"{}@{}\" is a package inside a git repository, which fnpm does not support",
                    shown(name),
                    shown(head)
                ),
            ));
        }
    }
    let repo = match url.strip_prefix("github:") {
        Some(gh) => format!("https://github.com/{gh}.git"),
        None => url.strip_prefix("git+").unwrap_or(url).to_owned(),
    };
    // A branch or tag is left for the install to resolve.
    let is_sha = commit.len() == 40 && commit.bytes().all(|c| c.is_ascii_hexdigit());
    Ok(Resolution::Git {
        url: repo,
        committish: commit.to_owned(),
        resolved_commit: if is_sha {
            commit.to_owned()
        } else {
            String::new()
        },
        integrity: None,
    })
}

#[derive(Debug, Clone)]
struct Parent {
    name: String,
    range: String,
}

/// `[parent[@range]/]name[@range]`. `None` for keys yarn ignores: globs, and keys with more
/// than one parent.
fn resolution_key_parts(key: &str) -> Option<(Option<Parent>, &str)> {
    fn next_segment(rest: &str) -> (&str, &str) {
        let scope = if rest.starts_with('@') {
            rest.find('/').map_or(rest.len(), |i| i + 1)
        } else {
            0
        };
        match rest[scope..].find('/') {
            Some(i) => (&rest[..scope + i], &rest[scope + i + 1..]),
            None => (rest, ""),
        }
    }
    if key.starts_with("*/") || key.starts_with("**/") {
        return None;
    }
    let (first, rest) = next_segment(key);
    if rest.is_empty() {
        return (!first.is_empty()).then_some((None, first));
    }
    let (descriptor, extra) = next_segment(rest);
    if first.is_empty() || descriptor.is_empty() || !extra.is_empty() {
        return None;
    }
    let (name, range) = split_locator(first).unwrap_or((first, ""));
    Some((
        Some(Parent {
            name: name.to_owned(),
            range: range.to_owned(),
        }),
        descriptor,
    ))
}

/// Matches `name`, `name@<literal>` and `name@npm:<literal>`; the literal is compared as
/// written and as its catalog's range.
fn resolution_pattern_matches(
    pattern: &str,
    name: &str,
    original_literal: &str,
    literal: &str,
) -> bool {
    let Some(rest) = pattern.strip_prefix(name) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let Some(range) = rest.strip_prefix('@') else {
        return false;
    };
    let plain = range.strip_prefix("npm:").unwrap_or(range);
    [original_literal, literal]
        .iter()
        .any(|l| range == *l || plain == strip_npm_protocol(l))
}

/// package.json is written before fnpm.lock is saved, so a migration that runs again can find
/// ranges it already rewrote.
fn range_before_rewrite(range: &str) -> Option<String> {
    if let Some(path) = range.strip_prefix("file:") {
        return Some(format!("portal:{path}"));
    }
    range
        .strip_prefix("link:./")
        .map(|path| format!("link:{path}"))
}

/// yarn normalizes a rule's value and binds it to the root workspace; the resulting
/// descriptor is a key in yarn.lock.
fn resolution_target_entry(
    name: &str,
    target: &str,
    root_locator: &str,
    descriptors: &HashMap<String, usize>,
) -> Option<usize> {
    let rewritten = range_before_rewrite(target);
    let spellings = [
        Some(("", target)),
        (!has_protocol(target)).then_some(("npm:", target)),
        rewritten.as_deref().map(|r| ("", r)),
    ];
    spellings.into_iter().flatten().find_map(|(prefix, rest)| {
        let key = format!("{name}@{prefix}{rest}");
        descriptors
            .get(&key)
            .or_else(|| descriptors.get(&format!("{key}::locator={root_locator}")))
            .copied()
    })
}

/// Rewrites a range only yarn understands into what fnpm reads for a dependency called
/// `name`: `portal:p` into `file:p`, a `patch:` into the range it patches. `None` when `spec`
/// needs no rewrite.
fn yarn_only_range_for_bun(name: &str, spec: &str) -> Option<String> {
    let mut patched: Option<String> = None;
    loop {
        let current = patched.as_deref().unwrap_or(spec);
        let Some(rest) = current.strip_prefix("patch:") else {
            break;
        };
        // `ms@npm%3A2.1.3#~/.yarn/patches/ms.patch` gives `2.1.3`, under another name `npm:ms@2.1.3`
        let (inner, _, _) = decode_patch_spec(rest);
        let (inner_name, inner_ref) = split_locator(&inner).unwrap_or((name, &inner));
        let (range, _) = split_reference_params(inner_ref);
        let plain = strip_npm_protocol(range);
        patched = Some(if inner_name != name && !has_protocol(plain) {
            format!("npm:{inner_name}@{plain}")
        } else {
            plain.to_owned()
        });
    }
    let range = patched.as_deref().unwrap_or(spec);
    if let Some(path) = range.strip_prefix("portal:").filter(|p| !p.is_empty()) {
        return Some(format!("file:{path}"));
    }
    range
        .strip_prefix("link:")
        .filter(|path| fnpm_types::name::is_npm_package_name(path))
        .map(|path| format!("link:./{path}"))
        .or(patched)
        .or_else(|| {
            let plain = strip_npm_protocol(spec);
            (plain.len() != spec.len()).then(|| plain.to_owned())
        })
}

/// (dependency name, package.json range fnpm reads differently, its rewrite)
type Rewrites = Vec<(String, String, String)>;

fn add_rewrite(rewrites: &mut Rewrites, name: &str, spec: &str, new_spec: &str) {
    if !rewrites.iter().any(|(n, f, _)| n == name && f == spec) {
        rewrites.push((name.to_owned(), spec.to_owned(), new_spec.to_owned()));
    }
}

/// The dependencies `packageExtensions` add to one entry.
#[derive(Default)]
struct Extended<'a> {
    dependencies: Vec<(&'a str, &'a str)>,
    peer_dependencies: Vec<(&'a str, &'a str)>,
    optional_peers: Vec<&'a str>,
    /// From yarn's builtin list. Nothing in package.json corresponds to these, so they are the
    /// package's own dependencies.
    builtin_dependencies: Vec<(&'a str, &'a str)>,
    builtin_optional_peers: Vec<&'a str>,
}

/// Does an extension key (`name@range`) apply to `name` at `version`? Prereleases match, like
/// in yarn; an empty range matches every version.
fn key_matches(key: &str, name: &str, version: &semver::Version) -> bool {
    parse_package_segment(key).is_ok_and(|selector| {
        selector.name == name
            && (selector.range.is_empty()
                || semver::Range::parse(&selector.range)
                    .is_some_and(|r| r.satisfies(version, true)))
    })
}

/// The extensions of every `name@range` that matches the entry's version.
fn extensions_of<'a>(
    rc: &'a YarnRc,
    descriptors: &HashMap<String, usize>,
    name: &str,
    entry: &Node,
) -> Extended<'a> {
    let mut extended = Extended::default();
    let Some((version, _)) = entry
        .get("version")
        .and_then(Node::text)
        .and_then(semver::parse_version)
    else {
        return extended;
    };
    // yarn registers its own list before those of `.yarnrc.yml`; the first to add a name wins.
    for &(key, dependencies, optional_peers) in builtin_extensions::BUILTIN {
        if !key_matches(key, name, &version) {
            continue;
        }
        for &(dep_name, dep_range) in dependencies {
            // The list differs by release, and yarn.lock does not name the release that wrote it.
            let is_locked = ["@npm:", "@"]
                .iter()
                .any(|sep| descriptors.contains_key(&format!("{dep_name}{sep}{dep_range}")));
            let is_added = extended
                .builtin_dependencies
                .iter()
                .any(|(n, _)| *n == dep_name);
            if is_locked && !is_added {
                extended.builtin_dependencies.push((dep_name, dep_range));
            }
        }
        extended
            .builtin_optional_peers
            .extend(optional_peers.iter().copied());
    }
    for extension in &rc.extensions {
        if !key_matches(&extension.key, name, &version) {
            continue;
        }
        for (dep_name, dep_range) in &extension.dependencies {
            let taken = extended
                .dependencies
                .iter()
                .chain(&extended.builtin_dependencies)
                .any(|(n, _)| n == dep_name);
            if !taken {
                extended.dependencies.push((dep_name, dep_range));
            }
        }
        for (dep_name, dep_range) in &extension.peer_dependencies {
            if !extended
                .peer_dependencies
                .iter()
                .any(|(n, _)| n == dep_name)
            {
                extended.peer_dependencies.push((dep_name, dep_range));
            }
        }
        extended
            .optional_peers
            .extend(extension.optional_peers.iter().map(String::as_str));
    }
    extended
}

fn edge(name: &str, spec: &str, behavior: Behavior) -> DepEdge {
    DepEdge::new(name, strip_npm_protocol(spec), behavior)
}

/// The edges of a lockfile entry: its `dependencies` / `peerDependencies` with
/// `dependenciesMeta` / `peerDependenciesMeta`, plus what extensions add.
fn entry_edges(
    ws: &Workspace,
    entry_name: &str,
    entry: &Node,
    extended: &Extended<'_>,
) -> Result<Vec<DepEdge>> {
    let optional_deps = optional_names(entry, "dependenciesMeta");
    let mut optional_peers = optional_names(entry, "peerDependenciesMeta");
    for &name in &extended.builtin_optional_peers {
        if !optional_peers.contains(&name) {
            optional_peers.push(name);
        }
    }
    for &name in &extended.optional_peers {
        if !optional_peers.contains(&name) {
            optional_peers.push(name);
        }
    }
    let mut edges = Vec::new();
    for (group, base, optional) in [
        ("dependencies", Behavior::PROD, &optional_deps),
        ("peerDependencies", Behavior::PEER, &optional_peers),
    ] {
        for (k, v) in entry.get_or_empty(group).entries() {
            let (Some(name), Some(spec)) = (k.text(), v.text()) else {
                return Err(fail(
                    ws,
                    format!(
                        "yarn.lock entry \"{}\" has a dependency that is not a string",
                        shown(entry_name)
                    ),
                ));
            };
            let mut behavior = base;
            if optional.contains(&name) {
                behavior = if behavior.is_peer() {
                    behavior | Behavior::OPTIONAL
                } else {
                    Behavior::OPTIONAL
                };
            }
            edges.push(edge(name, spec, behavior));
        }
    }
    let declares =
        |group: &str, name: &str| entry.get(group).is_some_and(|d| d.get(name).is_some());
    // An extension only adds what the package does not declare itself.
    for &(name, range) in extended
        .dependencies
        .iter()
        .chain(&extended.builtin_dependencies)
    {
        if !declares("dependencies", name) {
            edges.push(edge(name, range, Behavior::PROD));
        }
    }
    for &(name, range) in &extended.peer_dependencies {
        if !declares("peerDependencies", name) {
            let behavior = if optional_peers.contains(&name) {
                Behavior::PEER | Behavior::OPTIONAL
            } else {
                Behavior::PEER
            };
            edges.push(edge(name, range, behavior));
        }
    }
    // peers listed only in peerDependenciesMeta
    for &name in &optional_peers {
        if !declares("peerDependencies", name)
            && !extended.peer_dependencies.iter().any(|(n, _)| *n == name)
        {
            edges.push(edge(name, "*", Behavior::PEER | Behavior::OPTIONAL));
        }
    }
    Ok(edges)
}

/// Yarn's range of a manifest dependency where fnpm reads another one.
#[derive(Default, Clone)]
struct EdgeNotes {
    /// The range yarn installs for the name, when it differs from the edge's literal.
    yarn_range: Option<String>,
    /// The name is in several dependency groups with different ranges.
    in_several_groups: bool,
}

/// A root or workspace package.json's edges, with the same groups and duplicate handling as
/// bun's `Package::parse` (an optional dependency replaces the first entry only in the root).
fn manifest_edges(
    manifest: &Value,
    is_root: bool,
    rewrites: &mut Rewrites,
) -> (Vec<DepEdge>, Vec<EdgeNotes>) {
    let optional_peers = manifest::optional_peer_names(manifest);
    let mut edges: Vec<DepEdge> = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    // name to the range yarn installs for it: devDependencies, else optionalDependencies, else
    // dependencies
    let mut yarn_ranges: HashMap<&str, &str> = HashMap::new();
    let mut in_dev: HashSet<&str> = HashSet::new();
    let mut declared: HashMap<&str, &str> = HashMap::new();
    let mut several: HashSet<&str> = HashSet::new();
    for (group, base) in manifest::GROUPS {
        for (name, spec) in manifest::string_entries(manifest, group) {
            let mut behavior = base;
            let mut replaces = None;
            if behavior.is_peer() {
                if optional_peers.contains(&name) {
                    behavior |= Behavior::OPTIONAL;
                }
            } else {
                match seen.get(name) {
                    Some(&first) => {
                        if is_root && behavior.is_optional() {
                            replaces = Some(first);
                        }
                        if declared.get(name).is_some_and(|f| *f != spec) {
                            several.insert(name);
                        }
                    }
                    None => {
                        seen.insert(name, edges.len());
                        declared.insert(name, spec);
                    }
                }
                if behavior.is_dev() {
                    in_dev.insert(name);
                    yarn_ranges.insert(name, spec);
                } else if !in_dev.contains(name) {
                    yarn_ranges.insert(name, spec);
                }
            }
            let mut literal = spec.to_owned();
            if let Some(new_spec) = yarn_only_range_for_bun(name, spec) {
                add_rewrite(rewrites, name, spec, &new_spec);
                literal = new_spec;
            }
            let e = edge(name, &literal, behavior);
            match replaces {
                Some(i) => edges[i] = e,
                None => edges.push(e),
            }
        }
    }
    let peers = manifest::string_entries(manifest, "peerDependencies");
    for name in &optional_peers {
        if !peers.iter().any(|(n, _)| n == name) {
            edges.push(edge(name, "*", Behavior::PEER | Behavior::OPTIONAL));
        }
    }
    let notes = edges
        .iter()
        .map(|e| {
            if e.behavior.is_peer() {
                return EdgeNotes::default();
            }
            let yarn_range = yarn_ranges
                .get(e.name.as_str())
                .filter(|r| **r != e.literal)
                .map(|r| (*r).to_owned());
            EdgeNotes {
                yarn_range,
                in_several_groups: several.contains(e.name.as_str()),
            }
        })
        .collect();
    (edges, notes)
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Workspace,
    Package,
    /// A `patch:` locator; it shares the package id of the locator it patches.
    Patch,
}

struct Entry<'n> {
    kind: Kind,
    node: &'n Node,
    name: &'n str,
    /// the text after `name@` in `resolution`
    reference: &'n str,
    package: Option<PackageId>,
}

/// The yarn berry migration. See the module docs.
pub fn migrate(bytes: &[u8], ws: &Workspace, options: &MigrateOptions<'_>) -> Result<Migration> {
    let data = std::str::from_utf8(bytes).map_err(|_| fail(ws, "yarn.lock is not UTF-8"))?;
    if may_hold_merge_key(data) {
        return Err(fail(
            ws,
            "yarn.lock has a \"<<\" merge key, which yarn and fnpm read differently",
        ));
    }
    let root = yaml::parse_last(data)
        .map_err(|e| fail(ws, format!("yarn.lock is not valid YAML: {e}")))?;
    if !root.is_map() {
        return Err(fail(ws, "yarn.lock must be a YAML mapping"));
    }
    let Some(version) = root
        .get("__metadata")
        .and_then(|m| m.get("version"))
        .and_then(Node::text)
    else {
        return Err(fail(ws, "yarn.lock is missing __metadata.version"));
    };
    // 4 is yarn 2.0's lockfile and 10 the newest (yarn 4)
    if !matches!(version, "4" | "5" | "6" | "7" | "8" | "9" | "10") {
        return Err(fail(
            ws,
            format!("yarn.lock version {} is not supported", shown(version)),
        ));
    }
    if let Some(key) = yaml::duplicate_key(&root, 3) {
        return Err(fail(
            ws,
            format!(
                "yarn.lock has the key \"{}\" more than once in one mapping",
                shown(&key)
            ),
        ));
    }
    let rc = read_yarnrc(ws, options)?;
    let mut warnings = Vec::new();

    // -- 1. index the lockfile entries -------------------------------------
    let mut entries: Vec<Entry<'_>> = Vec::with_capacity(root.entries().len());
    // descriptor as yarn writes it (`name@npm:^1`) to entry
    let mut descriptors: HashMap<String, usize> = HashMap::new();
    // locator without `::params` (`name@npm:1.2.3`) to entry
    let mut locators: HashMap<String, usize> = HashMap::new();
    // workspace path in the lockfile to entry
    let mut lockfile_workspaces: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, value) in root.entries() {
        let Some(key) = key.text() else {
            return Err(fail(ws, "yarn.lock has a key that is not a string"));
        };
        if key == "__metadata" {
            continue;
        }
        if !value.is_map() {
            return Err(fail(
                ws,
                format!("yarn.lock entry \"{}\" is not a mapping", shown(key)),
            ));
        }
        let Some(resolution) = value.get_str("resolution") else {
            return Err(fail(
                ws,
                format!("yarn.lock entry \"{}\" has no resolution", shown(key)),
            ));
        };
        let Some((name, reference)) = split_locator(resolution).filter(|(n, _)| !n.is_empty())
        else {
            return Err(fail(
                ws,
                format!(
                    "yarn.lock entry \"{}\" has an invalid resolution \"{}\"",
                    shown(key),
                    shown(resolution)
                ),
            ));
        };
        let idx = entries.len();
        let kind = if let Some(path) = reference.strip_prefix("workspace:") {
            lockfile_workspaces.insert(path, idx);
            Kind::Workspace
        } else if reference.starts_with("patch:") {
            Kind::Patch
        } else {
            Kind::Package
        };
        entries.push(Entry {
            kind,
            node: value,
            name,
            reference,
            package: None,
        });
        // yarn splits on `,` and the spaces around it; a space at the end of the key is kept.
        let parts: Vec<&str> = key.split(',').collect();
        for (n, desc) in parts.iter().enumerate() {
            let mut desc = *desc;
            if n > 0 {
                desc = desc.trim_start_matches(' ');
            }
            if n + 1 < parts.len() {
                desc = desc.trim_end_matches(' ');
            }
            if desc.is_empty() {
                continue;
            }
            if descriptors.insert(desc.to_owned(), idx).is_some() {
                return Err(fail(
                    ws,
                    format!("yarn.lock has more than one entry for \"{}\"", shown(desc)),
                ));
            }
        }
        locators.insert(locator_key(name, reference), idx);
    }
    let Some(&root_entry) = lockfile_workspaces.get(".") else {
        return Err(fail(
            ws,
            "yarn.lock has no root workspace entry (\"@workspace:.\")",
        ));
    };
    for path in lockfile_workspaces.keys() {
        if *path != "." && ws.member_by_path(path).is_none() {
            return Err(fail(
                ws,
                format!(
                    "yarn.lock workspace \"{}\" is not one of the package.json \"workspaces\"",
                    shown(path)
                ),
            ));
        }
    }

    // -- 2. root + workspaces ----------------------------------------------------
    let root_manifest = &ws.root.raw;
    // The rules in file order, as (parent, `name[@range]`, target). yarn skips those it cannot
    // parse.
    let mut rules: Vec<(Option<Parent>, String, String)> = Vec::new();
    for (k, v) in manifest::string_entries(root_manifest, "resolutions") {
        if let Some((parent, pattern)) = resolution_key_parts(k) {
            rules.push((parent, pattern.to_owned(), v.to_owned()));
        }
    }
    let mut root_rewrites: Rewrites = Vec::new();
    // fnpm gets a patch through `patchedDependencies` and the pinned range as the override
    for (_, pattern, target) in &rules {
        let name = fnpm_types::name::split_name_and_maybe_version(pattern).0;
        if let Some(new_spec) = yarn_only_range_for_bun(name, target) {
            add_rewrite(&mut root_rewrites, name, target, &new_spec);
        }
    }

    let mut root_pkg = Package::new(
        manifest::get_str(root_manifest, "name").unwrap_or_default(),
        manifest::get_str(root_manifest, "version").unwrap_or_default(),
        Resolution::Root,
    );
    let (edges, root_notes) = manifest_edges(root_manifest, true, &mut root_rewrites);
    root_pkg.dependencies = edges;
    root_pkg.peer_meta = manifest::peer_meta(root_manifest);
    root_pkg.bin = manifest::bin(root_manifest);
    root_pkg.platform = manifest::platform(root_manifest);
    let mut graph = LockGraph::new(root_pkg);
    // Catalogs must resolve while edges are bound.
    for (group, names) in &rc.catalogs {
        let target = if group.is_empty() {
            &mut graph.catalogs.default
        } else {
            graph.catalogs.named.entry(group.clone()).or_default()
        };
        for (name, range) in names {
            target.insert(name.clone(), range.clone());
        }
    }
    let mut index = graph.index();
    let mut notes: HashMap<usize, Vec<EdgeNotes>> = HashMap::new();
    notes.insert(0, root_notes);
    entries[root_entry].package = Some(PackageId::ROOT);
    let mut ws_rewrites: Vec<(String, Rewrites)> = Vec::new();
    for member in &ws.members {
        let mut rewrites = Vec::new();
        let raw = &member.manifest.raw;
        let mut pkg = Package::new(
            member.name.clone(),
            member.version.clone(),
            Resolution::Workspace {
                path: member.path.clone(),
            },
        );
        let (edges, member_notes) = manifest_edges(raw, false, &mut rewrites);
        pkg.dependencies = edges;
        pkg.peer_meta = manifest::peer_meta(raw);
        pkg.bin = manifest::bin(raw);
        let id = foreign::intern(&mut graph, &mut index, pkg);
        notes.insert(id.index(), member_notes);
        if let Some(&idx) = lockfile_workspaces.get(member.path.as_str()) {
            entries[idx].package = Some(id);
        }
        ws_rewrites.push((member.path.clone(), rewrites));
    }

    // -- 3. third-party packages -------------------------------------------------
    let mut entry_of_package: HashMap<PackageId, usize> = HashMap::new();
    for (i, entry) in entries.iter_mut().enumerate() {
        if entry.kind != Kind::Package {
            continue;
        }
        let (name, reference, node) = (entry.name, entry.reference, entry.node);
        let (head, params) = split_reference_params(reference);
        let resolution = if let Some(version) = head.strip_prefix("npm:") {
            let Some(v) = semver::exact_version(version) else {
                return Err(fail(
                    ws,
                    format!(
                        "yarn.lock entry \"{}\" has an invalid version",
                        shown(reference)
                    ),
                ));
            };
            let configured = registry_without_auth(options.registry_for(name));
            let tarball_url = match param(&params, "__archiveUrl") {
                Some(url) => {
                    // The integrity comes from the registry fnpm is configured with; if yarn
                    // uses another one, report that.
                    if !foreign::url_is_under_registry(url, &configured) {
                        registry_for(ws, options, &rc, name)?;
                    }
                    Some(url.to_owned())
                }
                None => {
                    let registry = registry_for(ws, options, &rc, name)?;
                    Some(foreign::default_tarball_url(
                        &registry,
                        name,
                        &v.to_string(),
                    ))
                }
            };
            let default = foreign::default_tarball_url(&configured, name, &v.to_string());
            Resolution::Npm {
                version: v.to_string(),
                tarball_url: tarball_url.filter(|u| *u != default),
                integrity: None,
            }
        } else if let Some((protocol, path)) = split_folder_protocol(head) {
            // relative to the package that declared it (`::locator=name@workspace:path`)
            let Some(base) = owner_dir(&params) else {
                return Err(fail(
                    ws,
                    format!(
                        "yarn.lock entry \"{}@{}\" is a path inside a package that is not a folder of the project",
                        shown(name),
                        shown(reference)
                    ),
                ));
            };
            let joined = foreign::join_posix(&base, path);
            if protocol == "link:" {
                Resolution::Symlink {
                    path: link_path_for_lockfile(&joined).into_owned(),
                }
            } else if is_tarball(&joined) {
                Resolution::LocalTarball {
                    path: joined,
                    integrity: None,
                }
            } else {
                if protocol == "portal:" {
                    warnings.push(format!(
                        "migrating \"{}@portal:{}\" as \"file:{}\": folder is copied, not linked (make it a workspace to keep it linked)",
                        shown(name),
                        shown(path),
                        shown(&joined)
                    ));
                }
                Resolution::Folder {
                    path: if joined.is_empty() {
                        ".".into()
                    } else {
                        joined
                    },
                }
            }
        } else if is_git_reference(head) {
            git_resolution(ws, name, head)?
        } else if head.starts_with("https://") || head.starts_with("http://") {
            Resolution::RemoteTarball {
                url: head.to_owned(),
                integrity: None,
            }
        } else {
            // exec:, custom protocols from plugins, ...
            return Err(fail(
                ws,
                format!(
                    "yarn.lock entry \"{}@{}\" uses a protocol fnpm does not support",
                    shown(name),
                    shown(reference)
                ),
            ));
        };
        let mut pkg = Package::new(
            name,
            node.get("version").and_then(Node::text).unwrap_or_default(),
            resolution,
        );
        let extended = extensions_of(&rc, &descriptors, name, node);
        pkg.dependencies = entry_edges(ws, name, node, &extended)?;
        pkg.peer_meta = pkg
            .dependencies
            .iter()
            .filter(|e| e.behavior.is_optional_peer())
            .map(|e| (e.name.clone(), fnpm_types::PeerMeta { optional: true }))
            .collect();
        pkg.bin = entry_bin(node);
        if let Some(cond) = node.get_str("conditions") {
            pkg.platform = parse_conditions(cond);
        }
        let before = graph.len();
        let id = foreign::intern(&mut graph, &mut index, pkg);
        entry.package = Some(id);
        if graph.len() > before {
            entry_of_package.insert(id, i);
        }
    }

    // -- patches: a patch can wrap another, so repeat until every chain reached a package ---
    let mut patched: Vec<(String, String)> = Vec::new(); // (`name@version`, patch path)
    let mut stacked: Vec<(String, usize, String)> = Vec::new(); // (`name@version`, files, first)
    let mut patched_packages: HashSet<PackageId> = HashSet::new();
    let mut pending: Vec<usize> = (0..entries.len())
        .filter(|&i| entries[i].kind == Kind::Patch)
        .collect();
    loop {
        let before = pending.len();
        let mut still_pending = Vec::new();
        for &i in &pending {
            let name = entries[i].name;
            let (inner, source, params) =
                decode_patch_spec(&entries[i].reference["patch:".len()..]);
            let (inner_name, inner_reference) = split_locator(&inner).unwrap_or((name, &inner));
            let inner_key = locator_key(inner_name, inner_reference);
            let Some(&inner_idx) = locators.get(&inner_key) else {
                return Err(fail(
                    ws,
                    format!(
                        "yarn.lock patch \"{}\" patches \"{}\", which is not in the lockfile",
                        shown(entries[i].reference),
                        shown(&inner_key)
                    ),
                ));
            };
            let Some(pid) = entries[inner_idx].package else {
                if entries[inner_idx].kind == Kind::Patch {
                    still_pending.push(i);
                }
                continue;
            };
            entries[i].package = Some(pid);
            patched_packages.insert(pid);
            let paths = project_patch_paths(&source, &params);
            let outside = match &paths {
                None => Some(source.clone()),
                Some(paths) => paths
                    .iter()
                    .find(|p| {
                        p.is_empty()
                            || p.as_str() == "."
                            || p.contains('\0')
                            || p.starts_with('/')
                            || p.split(['/', '\\']).any(|x| x == "..")
                    })
                    .cloned(),
            };
            if let Some(path) = outside {
                return Err(fail(
                    ws,
                    format!(
                        "yarn.lock patch file \"{}\" is not a file inside the project",
                        shown(&path)
                    ),
                ));
            }
            let mut paths = paths.unwrap_or_default();
            if paths.is_empty() {
                continue;
            }
            let pkg = &graph.packages[pid.index()];
            if pkg.resolution.is_local_tree() {
                return Err(fail(
                    ws,
                    format!(
                        "yarn.lock patches \"{}\" with \"{}\"; fnpm does not patch packages installed from a project folder",
                        shown(entries[inner_idx].name),
                        shown(&paths[0])
                    ),
                ));
            }
            let key = format!("{}@{}", entries[inner_idx].name, pkg.resolution);
            if paths.len() > 1 {
                if !stacked.iter().any(|(k, _, _)| *k == key) {
                    stacked.push((key, paths.len(), paths.swap_remove(0)));
                }
                continue;
            }
            let path = paths.swap_remove(0);
            match patched.iter().find(|(k, _)| *k == key) {
                None => patched.push((key, path)),
                Some((_, existing)) if *existing != path => {
                    return Err(fail(
                        ws,
                        format!(
                            "yarn.lock patches \"{}\" with both \"{}\" and \"{}\"; fnpm applies one patch per package",
                            shown(&key),
                            shown(existing),
                            shown(&path)
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
        if still_pending.is_empty() || still_pending.len() == before {
            break;
        }
        pending = still_pending;
    }
    match stacked.as_slice() {
        [] => {}
        [(key, count, first)] => {
            return Err(fail(
                ws,
                format!(
                    "yarn.lock patches \"{}\" with {count} files (\"{}\", ...); fnpm applies one patch per package, merge them into one file",
                    shown(key),
                    shown(first)
                ),
            ));
        }
        several => {
            let names: Vec<String> = several
                .iter()
                .map(|(k, c, _)| format!("\"{}\" with {c}", shown(k)))
                .collect();
            return Err(fail(
                ws,
                format!(
                    "yarn.lock applies several patch files to {} packages ({}). fnpm supports one per package, merge them",
                    several.len(),
                    names.join(", ")
                ),
            ));
        }
    }

    // -- 4. bind dependency edges --------------------------------------------------
    let root_name = entries[root_entry].name;
    let root_locator = encode_component(&format!("{root_name}@workspace:."));
    let workspace_by_name: HashMap<String, PackageId> = graph
        .workspaces
        .values()
        .filter(|id| id.0 != 0)
        .map(|id| (graph.packages[id.index()].name.clone(), *id))
        .collect();
    for pkg_index in 0..graph.len() {
        let pid = PackageId(pkg_index as u32);
        // this package's yarn locator, as (name, reference)
        let owner: Option<(String, String)> = match &graph.packages[pkg_index].resolution {
            Resolution::Root | Resolution::Workspace { .. } => {
                let path = match &graph.packages[pkg_index].resolution {
                    Resolution::Workspace { path } => path.as_str(),
                    _ => ".",
                };
                // yarn names an unnamed workspace itself (`root-workspace-0b6124`)
                let name = lockfile_workspaces.get(path).map_or_else(
                    || graph.packages[pkg_index].name.clone(),
                    |&i| entries[i].name.to_owned(),
                );
                Some((name, format!("workspace:{path}")))
            }
            _ => entry_of_package
                .get(&pid)
                .map(|&i| (entries[i].name.to_owned(), entries[i].reference.to_owned())),
        };
        let owner_name = owner.as_ref().map_or("", |(n, _)| n.as_str()).to_owned();
        // relative protocols (`file:`, `portal:`, `link:`) are keyed by the declaring package
        let owner_locator = owner
            .as_ref()
            .map(|(n, r)| encode_component(&format!("{n}@{r}")));
        let is_patched = patched_packages.contains(&pid);
        let package_notes = notes.get(&pkg_index).cloned().unwrap_or_default();
        let edge_count = graph.packages[pkg_index].dependencies.len();
        for ei in 0..edge_count {
            let dep = graph.packages[pkg_index].dependencies[ei].clone();
            let note = package_notes.get(ei).cloned().unwrap_or_default();
            let yarn_range = note.yarn_range.as_deref();
            let dep_spec = dep.spec().ok();
            let is_workspace_spec = matches!(dep_spec, Some(DependencySpec::Workspace { .. }));
            // The range yarn installs for the name can be the `catalog:` of another group.
            let catalog: Option<String> = match yarn_range.and_then(|r| r.strip_prefix("catalog:"))
            {
                Some(group) => Some(group.trim().to_owned()),
                None if yarn_range.is_none() => dep
                    .literal
                    .strip_prefix("catalog:")
                    .map(|g| g.trim().to_owned()),
                None => None,
            };
            let catalog_range =
                catalog.and_then(|g| graph.catalogs.get(&g, &dep.name).map(str::to_owned));
            let literal: String = catalog_range
                .or_else(|| yarn_range.map(str::to_owned))
                .unwrap_or_else(|| dep.literal.clone());
            let lookup = |key: &str| descriptors.get(key).copied();

            // Like in yarn, the first matching rule in file order wins, before any descriptor
            // lookup.
            let rule = if dep.behavior.is_peer() || dep.behavior.is_workspace() {
                None
            } else {
                rules.iter().find(|(parent, pattern, _)| {
                    parent.as_ref().is_none_or(|p| {
                        owner.as_ref().is_some_and(|(owner_name, reference)| {
                            p.name == *owner_name
                                && (p.range.is_empty()
                                    || (!is_patched
                                        && if has_protocol(&p.range) {
                                            p.range == *reference
                                        } else {
                                            reference.strip_prefix("npm:") == Some(p.range.as_str())
                                        }))
                        })
                    }) && resolution_pattern_matches(pattern, &dep.name, &dep.literal, &literal)
                })
            };
            let mut found: Option<usize>;
            let mut bound_workspace: Option<PackageId> = None;
            if let Some((_, _, target)) = rule {
                found = resolution_target_entry(&dep.name, target, &root_locator, &descriptors);
            } else {
                // 1. exactly as written (workspace:, patch:, npm: aliases, URLs)
                found = lookup(&format!("{}@{literal}", dep.name));
                // 2. how yarn normalizes a bare range or tag
                if found.is_none() && !has_protocol(&literal) {
                    let key = if matches!(dep_spec, Some(DependencySpec::Github(_)))
                        && !literal.starts_with("github:")
                    {
                        format!("{}@github:{literal}", dep.name)
                    } else {
                        format!(
                            "{}@npm:{}",
                            dep.name,
                            if literal.is_empty() { "*" } else { &literal }
                        )
                    };
                    found = lookup(&key);
                }
                // 3. relative protocols are keyed per declaring package
                if let (None, Some(owner)) = (found, &owner_locator) {
                    let before = range_before_rewrite(&literal);
                    found = std::iter::once(literal.as_str())
                        .chain(before.as_deref())
                        .find_map(|range| {
                            lookup(&format!("{}@{range}::locator={owner}", dep.name))
                        });
                }
                // 4. workspaces by name (any `workspace:` range)
                if found.is_none() && (dep.behavior.is_workspace() || is_workspace_spec) {
                    bound_workspace = workspace_by_name.get(&dep.name).copied();
                }
                // 5. a range without a key that a workspace's version satisfies, as yarn does
                if found.is_none()
                    && bound_workspace.is_none()
                    && matches!(dep_spec, Some(DependencySpec::Npm { .. }))
                    && !dep.behavior.is_peer()
                    && rc.transparent_workspaces != Some(false)
                {
                    bound_workspace = foreign::linked_workspace(
                        &graph,
                        options.link_workspace_packages,
                        &dep.name,
                        &dep.literal,
                    );
                }
            }
            if let Some(ws_id) = bound_workspace {
                let e = &mut graph.packages[pkg_index].dependencies[ei];
                e.target = Some(ws_id);
                e.behavior |= Behavior::WORKSPACE;
                continue;
            }
            match found.and_then(|i| entries[i].package) {
                Some(mut target) => {
                    // fnpm links every workspace into the root's node_modules under its name.
                    if pkg_index == 0
                        && !dep.behavior.is_workspace()
                        && !dep.behavior.is_peer()
                        && !matches!(
                            graph.packages[target.index()].resolution,
                            Resolution::Workspace { .. }
                        )
                        && let Some(&ws_id) = workspace_by_name.get(&dep.name)
                    {
                        let ws_path = match &graph.packages[ws_id.index()].resolution {
                            Resolution::Workspace { path } => path.clone(),
                            _ => String::new(),
                        };
                        // a path to the workspace's own folder is the workspace
                        let same_folder = match &graph.packages[target.index()].resolution {
                            Resolution::Folder { path } => *path == ws_path,
                            Resolution::Symlink { path } => {
                                *path == link_path_for_lockfile(&ws_path)
                            }
                            _ => false,
                        };
                        if !same_folder {
                            return Err(fail(
                                ws,
                                format!(
                                    "\"{}\" is a workspace, and yarn.lock pins the root package's \"{}@{}\" to a package that is not that workspace; fnpm installs the workspace in its place",
                                    shown(&dep.name),
                                    shown(&dep.name),
                                    shown(&dep.literal)
                                ),
                            ));
                        }
                        target = ws_id;
                    }
                    if note.in_several_groups && !own_range_accepts(&graph, &dep, target) {
                        return Err(fail(
                            ws,
                            format!(
                                "\"{}\" is in several dependency groups of \"{}\": yarn uses \"{}\" for all, but yarn.lock version does not satisfy \"{}\"",
                                shown(&dep.name),
                                shown(&owner_name),
                                shown(&literal),
                                shown(&dep.literal)
                            ),
                        ));
                    }
                    let is_ws = matches!(
                        graph.packages[target.index()].resolution,
                        Resolution::Workspace { .. }
                    );
                    let e = &mut graph.packages[pkg_index].dependencies[ei];
                    e.target = Some(target);
                    if is_ws && is_workspace_spec {
                        e.behavior |= Behavior::WORKSPACE;
                    }
                }
                // a peer is bound at install time, to a package another edge brought in
                None if dep.behavior.is_peer() || dep.behavior.is_workspace() => {}
                None => {
                    return Err(fail(
                        ws,
                        format!(
                            "yarn.lock missing \"{}@{}\", a dependency of \"{}\"",
                            shown(&dep.name),
                            shown(&dep.literal),
                            shown(&owner_name)
                        ),
                    ));
                }
            }
        }
    }
    // Peers that yarn did not record a binding for are bound like bun.lock's loader binds them.
    for pkg_index in 0..graph.len() {
        for ei in 0..graph.packages[pkg_index].dependencies.len() {
            let dep = &graph.packages[pkg_index].dependencies[ei];
            if dep.target.is_some() || !dep.behavior.is_peer() || dep.behavior.is_optional_peer() {
                continue;
            }
            if foreign::is_overridden(&graph, &dep.name) {
                continue;
            }
            let target = foreign::bind_peer_by_range(&graph, &dep.name, &dep.literal);
            graph.packages[pkg_index].dependencies[ei].target = target;
        }
    }

    // Without an integrity, a tarball would be installed unverified: fnpm fetches the
    // integrity from the configured registry on install, which cannot serve a package yarn
    // downloads from elsewhere.
    let reached = graph.reachable(Behavior(0xff), None);
    for (i, p) in graph.packages.iter().enumerate() {
        if let (
            true,
            Resolution::Npm {
                tarball_url: Some(url),
                ..
            },
        ) = (reached[i], &p.resolution)
        {
            let configured = registry_without_auth(options.registry_for(&p.name));
            if !foreign::url_is_under_registry(url, &configured) {
                return Err(fail(
                    ws,
                    format!(
                        "yarn.lock fetches \"{}@{}\" from {}, outside the configured registry ({}): cannot get integrity",
                        shown(&p.name),
                        p.resolution,
                        shown(url),
                        configured
                    ),
                ));
            }
        }
    }

    // -- 5. package.json edits ---------------------------------------------------------
    let mut write_back = Vec::new();
    if let Some(edit) = edit_package_json("", root_manifest, &root_rewrites, &patched, Some(&rc)) {
        write_back.push(edit);
    }
    for (path, rewrites) in &ws_rewrites {
        if let Some(member) = ws.member_by_path(path)
            && let Some(edit) = edit_package_json(path, &member.manifest.raw, rewrites, &[], None)
        {
            write_back.push(edit);
        }
    }
    for (key, path) in &patched {
        graph.patched_dependencies.insert(
            key.clone(),
            fnpm_types::PatchedDependency {
                path: path.clone(),
                hash: None,
            },
        );
    }
    // Parsed after the rewrites, so the next install does not see them as changed.
    let effective = foreign::effective_root(root_manifest, &write_back);
    foreign::apply_root_settings(&mut graph, &effective);
    foreign::finish(&mut graph, ConfigVersion::V0);
    Ok(Migration {
        graph,
        write_back,
        warnings,
    })
}

/// The edge's own range (or its catalog's) accepts the package yarn locked for the name.
fn own_range_accepts(graph: &LockGraph, dep: &DepEdge, target: PackageId) -> bool {
    let own = foreign::resolve_catalog(graph, &dep.name, &dep.literal).unwrap_or(&dep.literal);
    let target = &graph.packages[target.index()];
    match (DependencySpec::parse(&dep.name, own), &target.resolution) {
        (Ok(DependencySpec::Npm { range, .. }), Resolution::Npm { version, .. }) => {
            semver::exact_version(version).is_some_and(|v| {
                semver::Range::parse(&range).is_some_and(|r| r.satisfies(&v, false))
            })
        }
        (Ok(DependencySpec::Workspace { .. }), Resolution::Workspace { .. })
        | (Ok(DependencySpec::DistTag { .. }), Resolution::Npm { .. }) => true,
        _ => false,
    }
}

/// An entry's `bin` mapping.
fn entry_bin(node: &Node) -> Bin {
    match node.get("bin") {
        Some(Node::Map(entries)) => Bin::from_map(
            entries
                .iter()
                .filter_map(|(k, v)| Some((k.text()?.to_owned(), v.text()?.to_owned()))),
        ),
        Some(other) => other.as_str().map_or(Bin::None, Bin::from_file),
        None => Bin::None,
    }
}

fn sorted_object(entries: &[(String, String)]) -> Value {
    let mut pairs: Vec<&(String, String)> = entries.iter().collect();
    pairs.sort();
    Value::Object(
        pairs
            .into_iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    )
}

/// `packageExtensions`, in the shape .yarnrc.yml has them.
fn extensions_object(extensions: &[Extension]) -> Value {
    let mut rules = Map::new();
    for extension in extensions {
        let mut fields = Map::new();
        for (group, ranges) in [
            ("dependencies", &extension.dependencies),
            ("peerDependencies", &extension.peer_dependencies),
        ] {
            if !ranges.is_empty() {
                fields.insert(
                    group.to_owned(),
                    Value::Object(
                        ranges
                            .iter()
                            .map(|(n, r)| (n.clone(), Value::String(r.clone())))
                            .collect(),
                    ),
                );
            }
        }
        if !extension.optional_peers.is_empty() {
            let meta: Map<String, Value> = extension
                .optional_peers
                .iter()
                .map(|n| (n.clone(), serde_json::json!({ "optional": true })))
                .collect();
            fields.insert("peerDependenciesMeta".to_owned(), Value::Object(meta));
        }
        rules.insert(extension.key.clone(), Value::Object(fields));
    }
    Value::Object(rules)
}

/// The edits bun makes to one package.json; existing keys are left alone.
fn edit_package_json(
    path: &str,
    json: &Value,
    rewrites: &Rewrites,
    patched: &[(String, String)],
    rc: Option<&YarnRc>,
) -> Option<PackageJsonWriteBack> {
    let catalogs = rc
        .map(|rc| &rc.catalogs)
        .filter(|c| c.iter().any(|(_, m)| !m.is_empty()));
    let extensions: &[Extension] = rc.map_or(&[], |rc| rc.extensions.as_slice());
    if (rewrites.is_empty() && patched.is_empty() && catalogs.is_none() && extensions.is_empty())
        || !json.is_object()
    {
        return None;
    }
    let mut fields: Vec<(String, Value)> = Vec::new();
    let mut changes: Vec<String> = Vec::new();
    let mut any = false;
    for group in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
        "resolutions",
    ] {
        let Some(Value::Object(obj)) = json.get(group) else {
            continue;
        };
        let mut obj = obj.clone();
        let mut changed = false;
        for (key, value) in obj.iter_mut() {
            let Some(spec) = value.as_str() else { continue };
            // the dependency a `resolutions` key names is its last `name[@range]`
            let name = if group == "resolutions" {
                resolution_key_parts(key).map_or(key.as_str(), |(_, p)| {
                    fnpm_types::name::split_name_and_maybe_version(p).0
                })
            } else {
                key.as_str()
            };
            if let Some((_, _, new_spec)) = rewrites.iter().find(|(n, f, _)| n == name && f == spec)
            {
                *value = Value::String(new_spec.clone());
                changed = true;
            }
        }
        if changed {
            fields.push((group.to_owned(), Value::Object(obj)));
            any = true;
        }
    }
    if any {
        changes.push("rewrote patch:/portal:/link:/npm: ranges".into());
    }
    if !patched.is_empty() {
        // Keep what the user already declared for fnpm and add yarn's entries.
        match json.get("patchedDependencies") {
            Some(Value::Object(existing)) => {
                let mut obj = existing.clone();
                let mut added = false;
                for (key, p) in patched {
                    if !obj.contains_key(key) {
                        obj.insert(key.clone(), Value::String(p.clone()));
                        added = true;
                    }
                }
                if added {
                    fields.push(("patchedDependencies".into(), Value::Object(obj)));
                    changes.push("added patchedDependencies".into());
                }
            }
            None => {
                let obj: Map<String, Value> = patched
                    .iter()
                    .map(|(k, p)| (k.clone(), Value::String(p.clone())))
                    .collect();
                fields.push(("patchedDependencies".into(), Value::Object(obj)));
                changes.push("added patchedDependencies".into());
            }
            Some(_) => {}
        }
    }
    if let Some(catalogs) = catalogs {
        let default = catalogs.iter().find(|(g, m)| g.is_empty() && !m.is_empty());
        let mut named: Vec<&(String, Vec<(String, String)>)> = catalogs
            .iter()
            .filter(|(g, m)| !g.is_empty() && !m.is_empty())
            .collect();
        named.sort_by(|a, b| a.0.cmp(&b.0));
        let has_key = |key: &str| {
            json.get(key).is_some()
                || json
                    .get("workspaces")
                    .is_some_and(|w| w.is_object() && w.get(key).is_some())
        };
        let want_default = default.is_some() && !has_key("catalog");
        let want_named = !named.is_empty() && !has_key("catalogs");
        if want_default || want_named {
            // fnpm reads catalogs from the `workspaces` object, so an array becomes `{ packages }`.
            let mut workspaces = match json.get("workspaces") {
                Some(Value::Object(existing)) => existing.clone(),
                Some(packages @ Value::Array(_)) => {
                    let mut m = Map::new();
                    m.insert("packages".into(), packages.clone());
                    m
                }
                _ => Map::new(),
            };
            if let (true, Some((_, default))) = (want_default, default) {
                workspaces.insert("catalog".into(), sorted_object(default));
            }
            if want_named {
                let groups: Map<String, Value> = named
                    .iter()
                    .map(|(g, m)| (g.clone(), sorted_object(m)))
                    .collect();
                workspaces.insert("catalogs".into(), Value::Object(groups));
            }
            fields.push(("workspaces".into(), Value::Object(workspaces)));
            changes.push("copied .yarnrc.yml catalogs to workspaces".into());
        }
    }
    if !extensions.is_empty() && json.get("packageExtensions").is_none() {
        fields.push(("packageExtensions".into(), extensions_object(extensions)));
        changes.push("copied .yarnrc.yml packageExtensions to packageExtensions".into());
    }
    (!changes.is_empty()).then(|| PackageJsonWriteBack {
        path: path.to_owned(),
        fields,
        changes,
    })
}
