//! Reading the parts of a package.json (`serde_json::Value`, key order preserved) that the
//! converters need, with the semantics of bun's `Package::parse`.

use std::collections::BTreeMap;

use fnpm_types::{Behavior, Bin, CpuSet, DepEdge, LibcSet, OsSet, PeerMeta, PlatformConstraints};
use serde_json::Value;

/// The string entries of the object `value[key]`, in file order. Non-string values are skipped.
pub(crate) fn string_entries<'a>(value: &'a Value, key: &str) -> Vec<(&'a str, &'a str)> {
    match value.get(key) {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(k, v)| Some((k.as_str(), v.as_str()?)))
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn get_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The names under `peerDependenciesMeta` with `optional: true`, in file order.
pub(crate) fn optional_peer_names(value: &Value) -> Vec<&str> {
    match value.get("peerDependenciesMeta") {
        Some(Value::Object(map)) => map
            .iter()
            .filter(|(_, meta)| meta.get("optional").and_then(Value::as_bool) == Some(true))
            .map(|(k, _)| k.as_str())
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) const GROUPS: [(&str, Behavior); 4] = [
    ("dependencies", Behavior::PROD),
    ("devDependencies", Behavior::DEV),
    ("optionalDependencies", Behavior::OPTIONAL),
    ("peerDependencies", Behavior::PEER),
];

/// The declared dependencies of a root or workspace package.json, as bun's `Package::parse`
/// reads them: groups in the order dependencies, devDependencies, optionalDependencies,
/// peerDependencies; an `optionalDependencies` entry replaces an earlier entry of the same name;
/// a name in `peerDependenciesMeta` (optional) that `peerDependencies` lacks becomes an optional
/// `*` peer.
pub(crate) fn manifest_edges(manifest: &Value) -> Vec<DepEdge> {
    let optional_peers = optional_peer_names(manifest);
    let mut edges: Vec<DepEdge> = Vec::new();
    for (group, behavior) in GROUPS {
        for (name, spec) in string_entries(manifest, group) {
            let mut behavior = behavior;
            if behavior.is_peer() {
                if optional_peers.contains(&name) {
                    behavior |= Behavior::OPTIONAL;
                }
                edges.push(DepEdge::new(name, spec, behavior));
                continue;
            }
            if behavior.is_optional()
                && let Some(existing) = edges
                    .iter_mut()
                    .find(|e| e.name == name && !e.behavior.is_peer())
            {
                *existing = DepEdge::new(name, spec, behavior);
                continue;
            }
            edges.push(DepEdge::new(name, spec, behavior));
        }
    }
    let declared_peers = string_entries(manifest, "peerDependencies");
    for name in optional_peers {
        if !declared_peers.iter().any(|(n, _)| *n == name) {
            edges.push(DepEdge::new(name, "*", Behavior::PEER | Behavior::OPTIONAL));
        }
    }
    edges
}

/// `peerDependenciesMeta` as the graph stores it.
pub(crate) fn peer_meta(manifest: &Value) -> BTreeMap<String, PeerMeta> {
    optional_peer_names(manifest)
        .into_iter()
        .map(|n| (n.to_owned(), PeerMeta { optional: true }))
        .collect()
}

/// `bin`, else `directories.bin`.
pub(crate) fn bin(manifest: &Value) -> Bin {
    match manifest.get("bin") {
        Some(Value::String(path)) => Bin::from_file(path),
        Some(Value::Object(map)) => Bin::from_map(
            map.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))),
        ),
        _ => match manifest
            .get("directories")
            .and_then(|d| d.get("bin"))
            .and_then(Value::as_str)
        {
            Some(dir) => Bin::from_dir(dir),
            None => Bin::None,
        },
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// `os`, `cpu` and `libc`.
pub(crate) fn platform(manifest: &Value) -> PlatformConstraints {
    let os = string_list(manifest.get("os"));
    let cpu = string_list(manifest.get("cpu"));
    let libc = string_list(manifest.get("libc"));
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
        libc: LibcSet::from_package_list(&libc),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edges_follow_package_parse() {
        let m = json!({
            "dependencies": { "a": "^1", "b": "^2", "x": 3 },
            "devDependencies": { "a": "^1.5", "c": "1" },
            "optionalDependencies": { "b": "^2.1" },
            "peerDependencies": { "p": ">=1", "a": "*" },
            "peerDependenciesMeta": { "p": { "optional": true }, "q": { "optional": true }, "r": {} }
        });
        let edges = manifest_edges(&m);
        let got: Vec<(&str, &str, Behavior)> = edges
            .iter()
            .map(|e| (e.name.as_str(), e.literal.as_str(), e.behavior))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a", "^1", Behavior::PROD),
                ("b", "^2.1", Behavior::OPTIONAL),
                ("a", "^1.5", Behavior::DEV),
                ("c", "1", Behavior::DEV),
                ("p", ">=1", Behavior::PEER | Behavior::OPTIONAL),
                ("a", "*", Behavior::PEER),
                ("q", "*", Behavior::PEER | Behavior::OPTIONAL),
            ]
        );
        assert_eq!(peer_meta(&m).len(), 2);
    }

    #[test]
    fn bins_and_platform() {
        assert_eq!(
            bin(&json!({ "bin": "cli.js" })),
            Bin::File {
                path: "cli.js".into()
            }
        );
        assert_eq!(
            bin(&json!({ "bin": { "x": "x.js" } })),
            Bin::NamedFile {
                name: "x".into(),
                path: "x.js".into()
            }
        );
        assert_eq!(
            bin(&json!({ "directories": { "bin": "bin" } })),
            Bin::Dir { path: "bin".into() }
        );
        assert_eq!(bin(&json!({})), Bin::None);
        assert!(platform(&json!({})).is_any());
        assert!(!platform(&json!({ "os": ["darwin"] })).is_any());
    }
}
