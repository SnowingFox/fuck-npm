//! Package comparisons. Reads package trees without installing or running their scripts.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use fnpm_store::tar::{EntryKind, TarEvent, TarParser};
use fnpm_types::{HashAlgorithm, Integrity, Resolution};
use serde_json::{Value, json};

use super::{Ctx, ok};
use crate::{CoreResult, Error, Result};

#[derive(Clone)]
enum Side {
    Registry {
        name: String,
        version: String,
        integrity: Option<Integrity>,
    },
    Local(String),
}

#[derive(Default)]
struct Tree {
    label: String,
    files: BTreeMap<String, File>,
    manifest: Value,
}

struct File {
    bytes: Vec<u8>,
    mode: u32,
}

fn local(spec: &str) -> bool {
    spec.starts_with(['.', '/', '\\'])
        || spec.starts_with("~/")
        || spec.starts_with("file:")
        || (cfg!(windows) && spec.as_bytes().get(1) == Some(&b':'))
}

fn classify(spec: &str) -> Side {
    if local(spec) {
        return Side::Local(spec.to_owned());
    }
    let at = spec.rfind('@').filter(|&at| at > 0);
    let (name, version) = at.map_or((spec, ""), |at| (&spec[..at], &spec[at + 1..]));
    Side::Registry {
        name: name.to_owned(),
        version: version.to_owned(),
        integrity: None,
    }
}

fn valid_name(name: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.len() <= 214
            && !p.starts_with(['.', '_'])
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._~".contains(&b))
    };
    name.strip_prefix('@').map_or_else(
        || part(name),
        |s| s.split_once('/').is_some_and(|(a, b)| part(a) && part(b)),
    )
}

fn package_name(tree: &Tree) -> Result<String> {
    tree.manifest
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| valid_name(n))
        .map(str::to_owned)
        .ok_or_else(|| Error::usage("package.json must have a valid npm package name"))
}

fn installed(cx: &Ctx, name: &str) -> Option<Side> {
    let (project, _, Some(graph)) = cx.load().ok()? else {
        return None;
    };
    let member = cx
        .req
        .cwd
        .strip_prefix(&project.root_dir)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    let importer = graph.packages.iter().find(|p| matches!(&p.resolution, Resolution::Workspace { path } if path.trim_end_matches('/') == member.trim_end_matches('/')))
        .unwrap_or(&graph.packages[0]);
    let target = importer
        .dependencies
        .iter()
        .find(|e| e.name == name)?
        .target?;
    let p = graph.packages.get(target.index())?;
    let Resolution::Npm {
        version, integrity, ..
    } = &p.resolution
    else {
        return None;
    };
    Some(Side::Registry {
        name: p.name.clone(),
        version: version.clone(),
        integrity: *integrity,
    })
}

fn registry(name: String, version: String) -> Side {
    Side::Registry {
        name,
        version,
        integrity: None,
    }
}

fn bare_version(spec: &str) -> bool {
    !local(spec)
        && (spec == "latest" || fnpm_types::spec::infer_tag(spec) == fnpm_types::SpecTag::Npm)
}

fn resolve(cx: &Ctx, args: &[String]) -> Result<(Side, Side)> {
    match args {
        [] => Ok((
            registry(String::new(), "latest".into()),
            Side::Local(".".into()),
        )),
        [one] => match classify(one) {
            Side::Registry { name, version, .. } => {
                if let Some((a, b)) = version.split_once("..") {
                    return Ok((registry(name.clone(), a.into()), registry(name, b.into())));
                }
                let left = installed(cx, &name).ok_or_else(|| Error::usage(format!("{name} is not in this project's lockfile; give two versions to compare, e.g. `fnpm pm diff {name}@1.0.0 {name}@2.0.0`")))?;
                Ok((left, registry(name, version)))
            }
            other => Ok((registry(String::new(), "latest".into()), other)),
        },
        [a, b] => {
            let mut left = classify(a);
            if let Side::Registry { name, version, .. } = &left
                && version.is_empty()
                && let Some(found) = installed(cx, name)
            {
                left = found;
            }
            let right = if bare_version(b) {
                let name = match &left {
                    Side::Registry { name, .. } => name.clone(),
                    _ => String::new(),
                };
                registry(name, b.clone())
            } else {
                classify(b)
            };
            Ok((left, right))
        }
        _ => Err(Error::usage("expected at most two package specs")),
    }
}

fn path_of(cx: &Ctx, spec: &str) -> PathBuf {
    let spec = spec.strip_prefix("file:").unwrap_or(spec);
    if let Some(tail) = spec.strip_prefix("~/") {
        return PathBuf::from(
            cx.env
                .get("HOME")
                .or_else(|| cx.env.get("USERPROFILE"))
                .unwrap_or("."),
        )
        .join(tail);
    }
    cx.req.cwd.join(spec)
}

fn read_dir(root: &Path, dir: &Path, prefix: &str, tree: &mut Tree, depth: usize) -> Result<()> {
    if depth > 128 {
        return Err(Error::usage("package folder is nested too deeply"));
    }
    for entry in std::fs::read_dir(dir).map_err(Error::io(dir))? {
        let entry = entry.map_err(Error::io(dir))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::usage("package file name is not UTF-8"))?;
        if matches!(
            name.as_str(),
            "node_modules"
                | ".git"
                | ".hg"
                | ".svn"
                | ".fnpm"
                | "fnpm.lock"
                | "bun.lock"
                | "bun.lockb"
        ) {
            continue;
        }
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(Error::io(&path))?;
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if meta.is_dir() {
            read_dir(root, &path, &rel, tree, depth + 1)?;
        } else if meta.is_file() || meta.is_symlink() {
            let canonical = std::fs::canonicalize(&path).map_err(Error::io(&path))?;
            if !canonical.starts_with(root) || !canonical.is_file() {
                continue;
            }
            let bytes = std::fs::read(&canonical).map_err(Error::io(&path))?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&canonical)
                    .map_err(Error::io(&path))?
                    .permissions()
                    .mode()
                    & 0o777
            };
            #[cfg(not(unix))]
            let mode = 0o644;
            tree.files.insert(rel, File { bytes, mode });
        }
    }
    Ok(())
}

// Bound both decompressed input and retained file content. Streaming avoids holding a
// second copy of the entire archive while the tree is built.
const MAX_DIFF_BYTES: u64 = 512 << 20;

fn read_tar_stream(mut input: impl Read, tree: &mut Tree, limit: u64) -> Result<()> {
    let mut parser = TarParser::new();
    let mut current = None;
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = input
            .read(&mut buf)
            .map_err(|e| Error::usage(format!("invalid tarball: {e}")))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > limit {
            return Err(Error::usage("package archive exceeds the diff size limit"));
        }
        parser.feed(&buf[..n], &mut |event| {
            match event {
                TarEvent::Entry(header) if header.kind == EntryKind::File => {
                    current = Some(header.path.clone());
                    tree.files.insert(
                        header.path,
                        File {
                            bytes: Vec::new(),
                            mode: header.mode,
                        },
                    );
                }
                TarEvent::Data(data) => {
                    if let Some(file) = current.as_ref().and_then(|p| tree.files.get_mut(p)) {
                        file.bytes.extend_from_slice(data);
                    }
                }
                TarEvent::EntryEnd => {
                    current = None;
                }
                _ => {}
            }
            Ok(())
        })?;
    }
    parser.finish()?;
    Ok(())
}

fn read_tar(bytes: &[u8], tree: &mut Tree) -> Result<()> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        read_tar_stream(flate2::read::GzDecoder::new(bytes), tree, MAX_DIFF_BYTES)
    } else {
        read_tar_stream(bytes, tree, MAX_DIFF_BYTES)
    }
}

fn binary(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

fn load_manifest(tree: &mut Tree) {
    tree.manifest = tree
        .files
        .get("package.json")
        .and_then(|f| serde_json::from_slice(&f.bytes).ok())
        .unwrap_or(Value::Null);
}

fn select_version(doc: &Value, spec: &str) -> Option<String> {
    let spec = if spec.is_empty() { "latest" } else { spec };
    if let Some(v) = doc
        .get("dist-tags")
        .and_then(|tags| tags.get(spec))
        .and_then(Value::as_str)
    {
        return Some(v.to_owned());
    }
    let versions = doc.get("versions")?.as_object()?;
    if versions.contains_key(spec) {
        return Some(spec.to_owned());
    }
    if fnpm_types::spec::infer_tag(spec) != fnpm_types::SpecTag::Npm {
        return None;
    }
    let range = fnpm_semver::Range::parse(spec).ok()?;
    versions
        .keys()
        .filter_map(|v| {
            fnpm_semver::Version::parse(v)
                .ok()
                .map(|parsed| (parsed, v))
        })
        .filter(|(v, _)| range.satisfies(v))
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(_, s)| s.clone())
}

async fn materialize(cx: &Ctx, project: &crate::project::Project, side: &Side) -> Result<Tree> {
    let mut tree = Tree::default();
    match side {
        Side::Local(spec) => {
            tree.label = spec.clone();
            let path = path_of(cx, spec);
            if path.is_dir() {
                let root = std::fs::canonicalize(&path).map_err(Error::io(&path))?;
                read_dir(&root, &root, "", &mut tree, 0)?;
            } else {
                read_tar(&std::fs::read(&path).map_err(Error::io(&path))?, &mut tree)?;
            }
            load_manifest(&mut tree);
        }
        Side::Registry {
            name,
            version,
            integrity,
        } => {
            if !valid_name(name) {
                return Err(Error::usage(
                    "package.json must have a valid npm package name",
                ));
            }
            let client = project.client()?;
            let doc = client.view(name).await?;
            let version = select_version(&doc, version)
                .ok_or_else(|| Error::usage(format!("no version matching {name}@{version}")))?;
            let manifest = &doc["versions"][&version];
            let dist = &manifest["dist"];
            let tarball = dist["tarball"]
                .as_str()
                .ok_or_else(|| Error::usage(format!("missing tarball for {name}@{version}")))?;
            let data = client.tarball_bytes(tarball).await?;
            let expected = (*integrity).or_else(|| {
                Integrity::from_dist(dist["integrity"].as_str(), dist["shasum"].as_str())
            });
            if expected.is_some_and(|i| !i.verify(&data)) {
                return Err(Error::usage(format!(
                    "Integrity check failed for tarball: {name}@{version}"
                )));
            }
            read_tar(&data, &mut tree)?;
            load_manifest(&mut tree);
            tree.manifest = manifest.clone();
            tree.label = format!("{name}@{version}");
        }
    }
    Ok(tree)
}

fn matches(filter: &str, path: &str) -> bool {
    let filter = filter.trim_start_matches("./").trim_end_matches('/');
    if path == filter || path.starts_with(&format!("{filter}/")) {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    if !filter.contains('/') && filter == base {
        return true;
    }
    globset::Glob::new(filter).is_ok_and(|g| {
        let g = g.compile_matcher();
        g.is_match(path) || (!filter.contains('/') && g.is_match(base))
    })
}

#[derive(Clone, Copy)]
struct Op<'a> {
    kind: char,
    text: &'a str,
}

fn operations<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    let mut prefix = 0;
    while prefix < a.len().min(b.len()) && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (a.len() - prefix).min(b.len() - prefix)
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut out: Vec<_> = a[..prefix]
        .iter()
        .map(|s| Op { kind: ' ', text: s })
        .collect();
    if (am.len() + 1).saturating_mul(bm.len() + 1) <= 4_000_000 {
        let width = bm.len() + 1;
        let mut table = vec![0u32; (am.len() + 1) * width];
        for i in (0..am.len()).rev() {
            for j in (0..bm.len()).rev() {
                table[i * width + j] = if am[i] == bm[j] {
                    1 + table[(i + 1) * width + j + 1]
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < am.len() || j < bm.len() {
            if i < am.len() && j < bm.len() && am[i] == bm[j] {
                out.push(Op {
                    kind: ' ',
                    text: am[i],
                });
                i += 1;
                j += 1;
            } else if i < am.len()
                && (j == bm.len() || table[(i + 1) * width + j] >= table[i * width + j + 1])
            {
                out.push(Op {
                    kind: '-',
                    text: am[i],
                });
                i += 1;
            } else {
                out.push(Op {
                    kind: '+',
                    text: bm[j],
                });
                j += 1;
            }
        }
    } else {
        out.extend(am.iter().map(|s| Op { kind: '-', text: s }));
        out.extend(bm.iter().map(|s| Op { kind: '+', text: s }));
    }
    out.extend(
        a[a.len() - suffix..]
            .iter()
            .map(|s| Op { kind: ' ', text: s }),
    );
    out
}

fn patch(a: &[u8], b: &[u8], context: usize) -> (String, usize, usize) {
    let a = String::from_utf8_lossy(a);
    let b = String::from_utf8_lossy(b);
    let (al, bl): (Vec<_>, Vec<_>) = (
        a.split_inclusive('\n').collect(),
        b.split_inclusive('\n').collect(),
    );
    let ops = operations(&al, &bl);
    let added = ops.iter().filter(|o| o.kind == '+').count();
    let removed = ops.iter().filter(|o| o.kind == '-').count();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (i, op) in ops.iter().enumerate().filter(|(_, o)| o.kind != ' ') {
        let _ = op;
        let (start, end) = (
            i.saturating_sub(context),
            (i.saturating_add(context).saturating_add(1)).min(ops.len()),
        );
        if let Some(last) = spans.last_mut()
            && start <= last.1
        {
            last.1 = end;
            continue;
        }
        spans.push((start, end));
    }
    let mut out = String::new();
    for (start, end) in spans {
        let before_a = ops[..start].iter().filter(|o| o.kind != '+').count();
        let before_b = ops[..start].iter().filter(|o| o.kind != '-').count();
        let count_a = ops[start..end].iter().filter(|o| o.kind != '+').count();
        let count_b = ops[start..end].iter().filter(|o| o.kind != '-').count();
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            before_a + usize::from(count_a > 0),
            count_a,
            before_b + usize::from(count_b > 0),
            count_b
        ));
        for op in &ops[start..end] {
            out.push(op.kind);
            out.push_str(op.text);
            if !op.text.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    (out, added, removed)
}

fn printable(text: &str) -> String {
    crate::security::escape_control(text, false)
}

fn quoted(prefix: &str, path: &str) -> String {
    let name = format!("{prefix}{path}");
    if name
        .bytes()
        .any(|b| b < 32 || b == b'"' || b == b'\\' || b >= 127)
    {
        let mut out = String::from("\"");
        for b in name.bytes() {
            match b {
                b'"' => out.push_str("\\\""),
                b'\\' => out.push_str("\\\\"),
                32..=126 => out.push(b as char),
                _ => out.push_str(&format!("\\{b:03o}")),
            }
        }
        out.push('"');
        out
    } else {
        name
    }
}

fn sha(bytes: &[u8]) -> String {
    Integrity::compute(HashAlgorithm::Sha256, bytes).to_hex()
}

fn notes(left: &Tree, right: &Tree) -> Vec<String> {
    let mut notes = Vec::new();
    for key in ["preinstall", "install", "postinstall", "prepare"] {
        let old = left.manifest.get("scripts").and_then(|v| v.get(key));
        let new = right.manifest.get("scripts").and_then(|v| v.get(key));
        if old == new {
            continue;
        }
        if let Some(v) = new.and_then(Value::as_str) {
            notes.push(format!(
                "{key} script {}: {}",
                if old.is_some() { "changed" } else { "added" },
                printable(v)
            ));
        } else if old.is_some() {
            notes.push(format!("{key} script removed"));
        }
    }
    for group in ["dependencies", "optionalDependencies", "peerDependencies"] {
        let old = left.manifest.get(group).and_then(Value::as_object);
        let new = right.manifest.get(group).and_then(Value::as_object);
        if let Some(new) = new {
            let added: Vec<_> = new
                .iter()
                .filter(|(k, _)| old.is_none_or(|o| !o.contains_key(*k)))
                .map(|(k, v)| format!("{k}@{}", v.as_str().unwrap_or("")))
                .collect();
            if !added.is_empty() {
                notes.push(printable(&format!("{group} added: {}", added.join(", "))));
            }
            for (k, v) in new {
                if let Some(previous) = old.and_then(|o| o.get(k))
                    && previous != v
                {
                    notes.push(printable(&format!(
                        "{group} {k}: {} → {}",
                        previous.as_str().unwrap_or(""),
                        v.as_str().unwrap_or("")
                    )));
                }
            }
        }
    }
    for key in [
        "main", "bin", "exports", "engines", "license", "types", "type",
    ] {
        let (a, b) = (left.manifest.get(key), right.manifest.get(key));
        if a == b {
            continue;
        }
        let display = |v: &Value| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        };
        notes.push(printable(&match (a, b) {
            (Some(a), Some(b)) => format!("{key} changed: {} → {}", display(a), display(b)),
            (None, Some(b)) => format!("{key} added: {}", display(b)),
            _ => format!("{key} removed"),
        }));
    }
    for (path, new) in &right.files {
        let old = left.files.get(path);
        if old.is_none() && binary(&new.bytes) {
            notes.push(format!(
                "new binary file {} ({} bytes)",
                printable(path),
                new.bytes.len()
            ));
        }
        if path.ends_with(".map")
            || ![".js", ".ts", ".mjs", ".cjs", ".jsx", ".tsx"]
                .iter()
                .any(|ext| path.ends_with(ext))
        {
            continue;
        }
        let b = String::from_utf8_lossy(&new.bytes);
        let a = old
            .map(|o| String::from_utf8_lossy(&o.bytes))
            .unwrap_or_default();
        let ai = crate::analyze::specifiers(&a);
        let mut added = Vec::new();
        for name in crate::analyze::specifiers(&b)
            .into_iter()
            .collect::<BTreeSet<_>>()
        {
            if ai.contains(&name) {
                continue;
            }
            if matches!(
                name.trim_start_matches("node:"),
                "child_process"
                    | "fs"
                    | "fs/promises"
                    | "net"
                    | "http"
                    | "https"
                    | "vm"
                    | "worker_threads"
            ) {
                notes.push(format!(
                    "now imports {} ({})",
                    printable(&name),
                    printable(path)
                ));
            } else if crate::analyze::package_of(&name).is_some() {
                added.push(printable(&name));
            }
        }
        if !added.is_empty() {
            notes.push(format!("new module imports: {}", added.join(", ")));
        }
        for api in [
            "eval(",
            "Function(",
            "exec(",
            "execSync(",
            "spawn(",
            "spawnSync(",
        ] {
            let delta = b
                .matches(api)
                .count()
                .saturating_sub(a.matches(api).count());
            if delta > 0 {
                notes.push(format!("+{delta} {api}) ({})", printable(path)));
            }
        }
        if b.chars().any(|c| matches!(c,'\x1b'|'\r'|'\u{80}'..='\u{9f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')) {notes.push(format!("terminal control characters in {}",printable(path)));}
    }
    notes
}

pub(crate) async fn run(cx: &Ctx) -> Result<CoreResult> {
    let mut args = cx.strings("diff");
    let mut filters = Vec::new();
    if args.len() > 2 {
        return Err(Error::usage("--diff may be given at most twice"));
    }
    if args.is_empty() {
        for arg in &cx.req.positionals {
            if let Some(filter) = arg.strip_prefix(':') {
                filters.push(filter.to_owned());
                continue;
            }
            if args.len() < 2 {
                args.push(arg.clone());
            } else {
                filters.push(arg.clone());
            }
        }
    } else {
        filters.extend(cx.req.positionals.iter().cloned());
    }
    for arg in &mut args {
        if !local(arg)
            && !arg.contains("://")
            && !arg.starts_with("npm:")
            && let Some((spec, filter)) = arg.split_once(':')
        {
            filters.push(filter.to_owned());
            *arg = spec.to_owned();
        }
    }
    if args.len() == 2 && args[0].contains("..") && !local(&args[0]) {
        filters.push(args.remove(1));
    }
    let (mut a, mut b) = resolve(cx, &args)?;
    let project = cx.project_anywhere()?;
    let left;
    let right;
    if matches!(&a, Side::Registry {name,..} if name.is_empty()) {
        right = materialize(cx, &project, &b).await?;
        if let Side::Registry { name, .. } = &mut a {
            *name = package_name(&right)?;
        }
        left = materialize(cx, &project, &a).await?;
    } else {
        left = materialize(cx, &project, &a).await?;
        if let Side::Registry { name, .. } = &mut b
            && name.is_empty()
        {
            *name = package_name(&left)?;
        }
        right = materialize(cx, &project, &b).await?;
    }
    let paths: BTreeSet<_> = left.files.keys().chain(right.files.keys()).collect();
    if !filters.is_empty() && !paths.iter().any(|p| filters.iter().any(|f| matches(f, p))) {
        return Err(Error::usage(format!(
            "no file in either side matches {}",
            filters.join(", ")
        )));
    }
    let context = cx.number("unified").unwrap_or(3).min(1_000_000) as usize;
    let names = cx.flag("nameOnly");
    let stat = cx.flag("stat");
    let raw = cx.flag("raw") || cx.flag("unformatted");
    let mut files = Vec::new();
    let mut text = format!(
        "{} → {}\n\n",
        printable(&left.label),
        printable(&right.label)
    );
    let (mut added, mut deleted, mut la, mut lr, mut formatting) = (0, 0, 0, 0, 0);
    let width = paths.iter().map(|p| p.len()).max().unwrap_or(0);
    for path in paths {
        if !filters.is_empty() && !filters.iter().any(|f| matches(f, path)) {
            continue;
        }
        let (a, b) = (left.files.get(path), right.files.get(path));
        if let (Some(a), Some(b)) = (a, b)
            && a.bytes == b.bytes
            && a.mode == b.mode
        {
            continue;
        }
        let (old, new) = (
            a.map_or(&[][..], |f| &f.bytes),
            b.map_or(&[][..], |f| &f.bytes),
        );
        let status = if a.is_none() {
            added += 1;
            "added"
        } else if b.is_none() {
            deleted += 1;
            "deleted"
        } else {
            "modified"
        };
        let binary = binary(old) || binary(new);
        let source_map = path.ends_with(".map");
        let formatting_only = !raw
            && a.is_some()
            && b.is_some()
            && path.ends_with(".json")
            && serde_json::from_slice::<Value>(old)
                .ok()
                .zip(serde_json::from_slice::<Value>(new).ok())
                .is_some_and(|(a, b)| a == b);
        formatting += usize::from(formatting_only);
        let (hunks, nadd, nremove) = if binary {
            (String::new(), 0, 0)
        } else {
            patch(old, new, context)
        };
        la += nadd;
        lr += nremove;
        let mut file = json!({"path":path,"status":status,"binary":binary,"sourceMap":source_map,"formattingOnly":formatting_only,"linesAdded":nadd,"linesRemoved":nremove,"bytesBefore":old.len(),"bytesAfter":new.len()});
        if let (Some(a), Some(b)) = (a, b)
            && a.mode != b.mode
        {
            file["modeBefore"] = json!(format!("{:o}", a.mode));
            file["modeAfter"] = json!(format!("{:o}", b.mode));
        }
        if binary {
            if a.is_some() {
                file["sha256Before"] = json!(sha(old));
            }
            if b.is_some() {
                file["sha256After"] = json!(sha(new));
            }
        }
        if !names && !stat && !binary && !source_map {
            file["patch"] = json!(hunks);
        }
        files.push(file);
        if names {
            text.push_str(&format!(
                "{} {}\n",
                match status {
                    "added" => 'A',
                    "deleted" => 'D',
                    _ => 'M',
                },
                printable(path)
            ));
            continue;
        }
        if stat {
            text.push_str(&if binary {
                format!(
                    " {:width$} | bin   {} → {} bytes\n",
                    printable(path),
                    old.len(),
                    new.len()
                )
            } else {
                format!(
                    " {:width$} | {:5} {}{}\n",
                    printable(path),
                    nadd + nremove,
                    "+".repeat(nadd.min(40)),
                    "-".repeat(nremove.min(40))
                )
            });
            continue;
        }
        let (pa, pb) = (quoted("a/", path), quoted("b/", path));
        if binary {
            text.push_str(&format!(
                "diff --fnpm {pa} {pb}\n{}Binary files differ ({} → {} bytes, sha256 {} → {})\n",
                if a.is_none() {
                    "new file\n"
                } else if b.is_none() {
                    "deleted file\n"
                } else {
                    ""
                },
                old.len(),
                new.len(),
                if a.is_some() {
                    sha(old)[..12].to_owned()
                } else {
                    "none".into()
                },
                if b.is_some() {
                    sha(new)[..12].to_owned()
                } else {
                    "none".into()
                }
            ));
            continue;
        }
        text.push_str(&format!("diff --git {pa} {pb}\n"));
        if let (Some(a), Some(b)) = (a, b) {
            if a.mode != b.mode {
                text.push_str(&format!(
                    "old mode 100{:03o}\nnew mode 100{:03o}\n",
                    a.mode, b.mode
                ));
            }
        } else {
            text.push_str(&format!(
                "{} file mode 100{:03o}\n",
                if a.is_none() { "new" } else { "deleted" },
                a.or(b).map_or(0o644, |f| f.mode)
            ));
        }
        if !hunks.is_empty() {
            text.push_str(&format!(
                "--- {}\n+++ {}\n",
                if a.is_some() { &pa } else { "/dev/null" },
                if b.is_some() { &pb } else { "/dev/null" }
            ));
            text.push_str(&hunks);
        }
    }
    let notes = notes(&left, &right);
    text.push_str(&format!(
        "\n{} → {}\n{} files changed, {added} added, {deleted} removed  (+{la} -{lr} lines)\n",
        printable(&left.label),
        printable(&right.label),
        files.len() - added - deleted
    ));
    for note in &notes {
        text.push_str(&format!("  ! {note}\n"));
    }
    let data = json!({"from":left.label,"to":right.label,"files":files,"notes":notes,"totals":{"files":files.len(),"added":added,"deleted":deleted,"linesAdded":la,"linesRemoved":lr,"formattingOnly":formatting}});
    Ok(ok(
        data,
        text.trim_end_matches('\n')
            .split('\n')
            .map(str::to_owned)
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_decompression_is_bounded_and_nul_files_are_binary() {
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&vec![0; 64 * 1024]).unwrap();
        let bytes = gzip.finish().unwrap();
        let error = read_tar_stream(
            flate2::read::GzDecoder::new(&bytes[..]),
            &mut Tree::default(),
            1024,
        )
        .unwrap_err();
        assert!(error.to_string().contains("size limit"));
        assert!(binary(b"hello\0world"));
        assert!(!binary(b"hello\nworld"));
    }

    #[test]
    fn hunks_handle_context_and_missing_newlines() {
        assert_eq!(
            patch(b"one\ntwo\nthree\n", b"one\nnew\nthree\n", 1),
            ("@@ -1,3 +1,3 @@\n one\n-two\n+new\n three\n".into(), 1, 1)
        );
        assert_eq!(
            patch(b"", b"new", 3).0,
            "@@ -0,0 +1,1 @@\n+new\n\\ No newline at end of file\n"
        );
    }
    #[test]
    fn explicit_versions_and_ranges_do_not_require_dist_tags() {
        let doc = json!({ "versions": { "1.0.0": {}, "1.2.0": {}, "2.0.0": {} } });
        assert_eq!(select_version(&doc, "1.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(select_version(&doc, "^1").as_deref(), Some("1.2.0"));
        assert_eq!(select_version(&doc, "latest"), None);
    }
    #[test]
    fn filters_and_names_are_validated() {
        assert!(matches("*.js", "dist/a.js"));
        assert!(matches("dist/", "dist/a.js"));
        assert!(!matches("dist/a.js", "outside/a.js"));
        assert!(valid_name("@scope/name"));
        assert!(!valid_name("../../evil"));
        assert!(!valid_name("evil\x1b[2J"));
        assert_eq!(quoted("a/", "a\n.js"), "\"a/a\\012.js\"");
    }
}
