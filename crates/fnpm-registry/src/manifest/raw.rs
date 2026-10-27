//! Builds the model from sonic-rs's DOM (one SIMD pass over the bytes into an arena), reading
//! every field leniently like bun: a value of an unexpected type is ignored rather than
//! failing the document.

use std::collections::{BTreeMap, HashMap};

use fnpm_types::{Bin, Integrity, PeerMeta, PlatformConstraints};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use super::{Dist, PackageManifest, VersionManifest};
use crate::semver_ext;

pub(crate) struct ParseError {
    pub name: String,
    pub message: String,
}

/// Unix-ms publish time of a JSON `time` value (bun `publish_timestamp_ms`): no time
/// (`null`/`false`/`0`/`""`) is 0; a string that is not a date and any other value never pass.
pub(crate) fn publish_time_of(value: &Value) -> u64 {
    if value.is_null() || value.as_bool() == Some(false) || value.as_f64() == Some(0.0) {
        return 0;
    }
    match value.as_str() {
        Some("") => 0,
        Some(s) => crate::date::parse_iso8601_ms(s)
            .or_else(|| crate::date::parse_rfc2822_ms(s))
            .unwrap_or(u64::MAX),
        None => u64::MAX,
    }
}

fn get<'v>(obj: &'v Value, key: &str) -> Option<&'v Value> {
    obj.as_object()?.get(&key)
}

/// `(key, value)` pairs of an object whose value is a string.
fn string_pairs(v: Option<&Value>) -> Vec<(String, String)> {
    let Some(obj) = v.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(obj.len());
    for (k, v) in obj.iter() {
        if let Some(s) = v.as_str() {
            out.push((k.to_owned(), s.to_owned()));
        }
    }
    out
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    string_pairs(v).into_iter().collect()
}

/// A string, or the strings of an array (`os` / `cpu` / `libc` / `bundleDependencies`).
fn list_of(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(v) if v.is_str() => vec![v.as_str().unwrap_or_default().to_owned()],
        Some(v) => v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|i| i.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

fn number_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
}

/// The fields of a version object that an install reads, found in one pass over its keys
/// (a full document's versions carry many more).
#[derive(Default)]
struct Fields<'v> {
    slots: [Option<&'v Value>; FIELDS.len()],
}

const FIELDS: [&str; 17] = [
    "dependencies",
    "optionalDependencies",
    "peerDependencies",
    "peerDependenciesMeta",
    "bundleDependencies",
    "bundledDependencies",
    "bin",
    "directories",
    "os",
    "cpu",
    "libc",
    "dist",
    "hasInstallScript",
    "scripts",
    "deprecated",
    "engines",
    "license",
];

impl<'v> Fields<'v> {
    fn of(raw: Option<&'v Value>) -> Self {
        let mut f = Self::default();
        if let Some(obj) = raw.and_then(Value::as_object) {
            for (k, v) in obj.iter() {
                // The first occurrence wins, like an object lookup.
                if let Some(i) = FIELDS.iter().position(|name| *name == k)
                    && f.slots[i].is_none()
                {
                    f.slots[i] = Some(v);
                }
            }
        }
        f
    }

    fn get(&self, key: &str) -> Option<&'v Value> {
        FIELDS
            .iter()
            .position(|name| *name == key)
            .and_then(|i| self.slots[i])
    }
}

fn build_version(version: fnpm_semver::Version, raw: Option<&Value>) -> VersionManifest {
    let fields = Fields::of(raw);
    let field = |key: &str| fields.get(key);
    let dependencies = string_pairs(field("dependencies"));
    let optional_dependencies = string_pairs(field("optionalDependencies"));
    let mut peer_dependencies = string_pairs(field("peerDependencies"));
    let mut peer_dependencies_meta = BTreeMap::new();
    if let Some(meta) = field("peerDependenciesMeta").and_then(Value::as_object) {
        for (peer, m) in meta.iter() {
            let optional = get(m, "optional").and_then(Value::as_bool) == Some(true);
            if optional && !peer_dependencies.iter().any(|(n, _)| n == peer) {
                peer_dependencies.push((peer.to_owned(), "*".to_owned()));
            }
            peer_dependencies_meta.insert(peer.to_owned(), PeerMeta { optional });
        }
    }

    let bundle = field("bundleDependencies").or_else(|| field("bundledDependencies"));
    let bundle_dependencies = match bundle {
        Some(b) if b.as_bool() == Some(true) => dependencies
            .iter()
            .chain(&optional_dependencies)
            .map(|(n, _)| n.clone())
            .collect(),
        Some(b) if b.is_array() => list_of(Some(b)),
        _ => Vec::new(),
    };

    let mut bin = match field("bin") {
        Some(b) if b.is_object() => Bin::from_map(string_pairs(Some(b))),
        Some(b) => match b.as_str() {
            Some(s) if !s.is_empty() => Bin::from_file(s),
            _ => Bin::None,
        },
        None => Bin::None,
    };
    if bin.is_none()
        && let Some(dir) = field("directories")
            .and_then(|d| get(d, "bin"))
            .and_then(Value::as_str)
        && !dir.is_empty()
    {
        bin = Bin::from_dir(dir);
    }

    let platform = PlatformConstraints::from_package_json(
        &list_of(field("os")),
        &list_of(field("cpu")),
        &list_of(field("libc")),
    );

    let scripts = string_map(field("scripts"));
    let has_install_script = match field("hasInstallScript").and_then(Value::as_bool) {
        Some(b) => b,
        None => ["preinstall", "install", "postinstall"]
            .iter()
            .any(|s| scripts.get(*s).is_some_and(|v| !v.is_empty())),
    };

    let deprecated = match field("deprecated") {
        Some(d) if d.as_bool() == Some(true) => Some("true".to_owned()),
        Some(d) => d.as_str().filter(|s| !s.is_empty()).map(str::to_owned),
        None => None,
    };

    let license = match field("license") {
        Some(l) if l.is_object() => get(l, "type").and_then(Value::as_str).map(str::to_owned),
        Some(l) => l.as_str().filter(|s| !s.is_empty()).map(str::to_owned),
        None => None,
    };

    let dist_obj = field("dist").filter(|d| d.is_object());
    let dist_str = |key: &str| dist_obj.and_then(|d| get(d, key)).and_then(Value::as_str);
    let dist = Dist {
        tarball: dist_str("tarball").unwrap_or_default().to_owned(),
        integrity: Integrity::from_dist(dist_str("integrity"), dist_str("shasum")),
        file_count: number_of(dist_obj.and_then(|d| get(d, "fileCount")))
            .map(|n| n.min(f64::from(u32::MAX)) as u32),
        unpacked_size: number_of(dist_obj.and_then(|d| get(d, "unpackedSize"))).map(|n| n as u64),
    };

    VersionManifest {
        version,
        dependencies,
        optional_dependencies,
        peer_dependencies,
        peer_dependencies_meta,
        bundle_dependencies,
        bin,
        platform,
        dist,
        has_install_script,
        deprecated,
        engines: string_map(field("engines")),
        scripts,
        license,
    }
}

pub(crate) fn parse(name: &str, json: &[u8]) -> Result<PackageManifest, ParseError> {
    let err = |message: String| ParseError {
        name: name.to_owned(),
        message,
    };
    let doc: Value = sonic_rs::from_slice(json).map_err(|e| err(e.to_string()))?;
    let Some(root) = doc.as_object() else {
        return Err(err("the registry document is not an object".to_owned()));
    };
    if let Some(message) = root.get(&"error").and_then(Value::as_str) {
        return Err(err(format!("npm error: {message}")));
    }
    let modified = match root.get(&"modified") {
        None => None,
        Some(m) if m.is_null() => None,
        Some(m) => match m.as_str() {
            Some(s) => Some(s.to_owned()),
            None => return Err(err("\"modified\" is not a string".to_owned())),
        },
    };

    let time_obj = root.get(&"time").and_then(Value::as_object);
    let mut time: BTreeMap<String, u64> = BTreeMap::new();
    let mut time_index: HashMap<&str, &Value> = HashMap::new();
    if let Some(t) = time_obj {
        time_index.reserve(t.len());
        for (key, value) in t.iter() {
            time_index.entry(key).or_insert(value);
            if semver_ext::parse_version(key).is_none() {
                time.entry(key.to_owned())
                    .or_insert_with(|| publish_time_of(value));
            }
        }
    }

    let mut versions = Vec::new();
    if let Some(vs) = root.get(&"versions").and_then(Value::as_object) {
        versions.reserve(vs.len());
        for (key, raw) in vs.iter() {
            let Some(version) = semver_ext::parse_version(key) else {
                continue;
            };
            if let Some(value) = time_index.get(key) {
                time.insert(semver_ext::format(&version), publish_time_of(value));
            }
            versions.push(build_version(version, Some(raw).filter(|r| r.is_object())));
        }
    }
    // Usually the registry sorts them, but not always (bun #6611). Stable, so equal keys keep
    // document order.
    if !versions
        .windows(2)
        .all(|w| semver_ext::cmp(&w[0].version, &w[1].version).is_le())
    {
        versions.sort_by(|a, b| semver_ext::cmp(&a.version, &b.version));
    }

    let dist_tags = root
        .get(&"dist-tags")
        .and_then(Value::as_object)
        .map(|tags| {
            tags.iter()
                .filter_map(|(k, v)| Some((k.to_owned(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();

    Ok(PackageManifest {
        name: name.to_owned(),
        dist_tags,
        versions,
        time,
        modified,
        etag: None,
        last_modified: None,
        fetched_at_ms: 0,
        max_age_secs: 0,
        is_full: false,
    })
}
