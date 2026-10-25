//! bun.lockb reading, against bun's binary fixtures (`test/cli/install/fixtures/*.lockb*`)
//! and the corruptions `bun-lockb.test.ts` checks.

use fnpm_types::{Bin, ConfigVersion, LockGraph, PackageId, Resolution};

use super::support::install_tests;
use crate::formats::bun_binary::{self, string_hash};

fn fixture(name: &str) -> Option<Vec<u8>> {
    let install = install_tests()?;
    Some(std::fs::read(install.join(name)).expect("fixture"))
}

fn by_name<'g>(g: &'g LockGraph, name: &str) -> Vec<&'g fnpm_types::Package> {
    g.packages.iter().filter(|p| p.name == name).collect()
}

struct Layout {
    count: usize,
    resolution: usize,
    bin: usize,
    resolution_size: usize,
}

/// The package columns of a lockfile (`layout()` in bun-lockb.test.ts).
fn layout(lockb: &[u8]) -> Layout {
    let u64_at = |at: usize| u64::from_le_bytes(lockb[at..at + 8].try_into().unwrap()) as usize;
    let format = u32::from_le_bytes(lockb[42..46].try_into().unwrap());
    let resolution_size = if format == 2 { 64 } else { 72 };
    let count = u64_at(86);
    let begin = u64_at(110);
    Layout {
        count,
        resolution: begin + 16 * count,
        bin: begin + (8 + 8 + resolution_size + 8 + 8 + 88) * count,
        resolution_size,
    }
}

#[test]
fn wyhash11_matches_bun_name_hashes() {
    // `name_hash` values from bun's migrate-bun-lockb-v2 snapshot.
    assert_eq!(string_hash(b"migrate-bun-lockb-v2"), 4533838613693112248);
    assert_eq!(string_hash(b"jquery"), 265418099762006574);
    assert_eq!(string_hash(b"is-even"), 7770159972461911348);
    // Lengths across every tail arm and a full 32-byte round give distinct values.
    let mut seen = std::collections::HashSet::new();
    let text = b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-_.";
    for n in 0..text.len() {
        assert!(
            seen.insert(string_hash(&text[..n])),
            "collision at length {n}"
        );
    }
}

#[test]
fn reads_format_2_lockfile() {
    let Some(bytes) = fixture("fixtures/bun.lockb.v2") else {
        return;
    };
    let g = bun_binary::parse(&bytes).unwrap();
    g.validate().unwrap();
    assert_eq!(g.packages.len(), 7);
    assert_eq!(g.root().name, "migrate-bun-lockb-v2");
    assert_eq!(g.config_version, ConfigVersion::V0);
    let jquery = by_name(&g, "jquery")[0];
    assert_eq!(jquery.version, "3.7.1");
    assert_eq!(
        jquery.resolution.integrity().unwrap().to_sri(),
        "sha512-m4avr8yL8kmFN8psrbFFFmB/If14iN5o9nw/NgnnM+kybDJpRsAynV2BsfpTYrTRysYUdADVD7CkUUizgkpLfg=="
    );
    // A tarball at the default registry is the default location.
    assert!(matches!(
        jquery.resolution,
        Resolution::Npm {
            tarball_url: None,
            ..
        }
    ));
    let chain: Vec<String> = ["is-even", "is-odd", "is-number", "kind-of", "is-buffer"]
        .iter()
        .map(|n| format!("{n}@{}", by_name(&g, n)[0].version))
        .collect();
    assert_eq!(
        chain,
        [
            "is-even@1.0.0",
            "is-odd@0.1.2",
            "is-number@3.0.0",
            "kind-of@3.2.2",
            "is-buffer@1.1.6"
        ]
    );
    let root_edges: Vec<(&str, &str)> = g
        .root()
        .dependencies
        .iter()
        .map(|e| (e.name.as_str(), e.literal.as_str()))
        .collect();
    assert_eq!(root_edges, [("is-even", "^1.0.0"), ("jquery", "~3.7.1")]);
}

#[test]
fn reads_workspaces_trust_overrides_and_catalogs() {
    let Some(bytes) = fixture("fixtures/bun.lockb.v2-most-features") else {
        return;
    };
    let g = bun_binary::parse(&bytes).unwrap();
    g.validate().unwrap();
    assert_eq!(g.root().name, "migrate-everything");
    assert_eq!(
        g.workspaces.keys().collect::<Vec<_>>(),
        ["", "packages/pkg1", "packages/pkg2"]
    );
    let pkg1 = &g.packages[g.workspaces["packages/pkg1"].index()];
    assert_eq!(pkg1.name, "pkg-wat");
    assert!(
        pkg1.dependencies
            .iter()
            .any(|e| e.name == "pkg-wat-2" && e.behavior.is_workspace())
    );
    // Only hashes are stored; the name comes from an installed package.
    assert_eq!(g.trusted_dependencies, Some(["esbuild".to_owned()].into()));
    assert_eq!(g.overrides.len(), 1);
    assert_eq!(g.overrides[0].selector.target.name, "scheduler");
    assert_eq!(g.overrides[0].value, "0.20.0");
    assert_eq!(
        g.catalogs.default.get("react").map(String::as_str),
        Some(">19.0.0")
    );
    let root = g.root();
    let lodash = root.dependency("lodash").unwrap();
    assert!(lodash.behavior.is_optional_peer() && lodash.target.is_none());
    assert!(root.dependency("is-number").unwrap().behavior.is_optional());
    assert!(root.dependency("zod").unwrap().behavior.is_dev());
    let react = root.dependency("react").unwrap();
    assert_eq!(react.literal, "catalog:");
    assert_eq!(g.packages[react.target.unwrap().index()].version, "19.1.1");
    // Platform-specific esbuild binaries keep their constraints.
    let linux_x64 = by_name(&g, "@esbuild/linux-x64")[0];
    assert_eq!(linux_x64.platform.os.names().collect::<Vec<_>>(), ["linux"]);
    assert_eq!(linux_x64.platform.cpu.names().collect::<Vec<_>>(), ["x64"]);
    assert!(
        by_name(&g, "esbuild")[0].meta.needs_scripts() || !by_name(&g, "esbuild")[0].bin.is_none()
    );
}

#[test]
fn reads_bun_0_5_lockfile_with_unresolved_peer() {
    // Writers of that time left unused tree slots, stored package ids in the tree, and left
    // the resolution of a peer they did not resolve uninitialized.
    let Some(bytes) = fixture("fixtures/bun.lockb.v0.5.2") else {
        return;
    };
    let g = bun_binary::parse(&bytes).unwrap();
    g.validate().unwrap();
    let react_dom = by_name(&g, "react-dom")[0];
    let peer = react_dom.dependency("react").unwrap();
    assert!(peer.behavior.is_peer());
    assert_eq!(peer.target, None);
    let react = g.root().dependency("react").unwrap().target.unwrap();
    assert_eq!(g.packages[react.index()].version, "18.2.0");
    assert_eq!(by_name(&g, "@types/react").len(), 2);
}

#[test]
fn reads_config_version_and_peers() {
    let Some(bytes) = fixture("fixtures/bun.lockb.v1.4.2-peers") else {
        return;
    };
    let g = bun_binary::parse(&bytes).unwrap();
    assert_eq!(g.config_version, ConfigVersion::V1);
    let zz10 = by_name(&g, "zz-10")[0];
    let zz9 = zz10.dependency("zz-9").unwrap();
    assert!(zz9.behavior.is_peer());
    assert_eq!(g.packages[zz9.target.unwrap().index()].version, "1.0.1");
}

#[test]
fn reads_unnamed_folders_and_bins() {
    if let Some(bytes) = fixture("fixtures/unnamed-folder-dependency.lockb") {
        let g = bun_binary::parse(&bytes).unwrap();
        assert_eq!(
            g.packages[1].resolution,
            Resolution::Folder {
                path: "anon".into()
            }
        );
        assert_eq!(g.packages[1].name, "");
        assert_eq!(
            g.packages[2].resolution,
            Resolution::Folder {
                path: "leaf".into()
            }
        );
    }
    if let Some(bytes) = fixture("bun.lockb.bin-linking") {
        let g = bun_binary::parse(&bytes).unwrap();
        g.validate().unwrap();
        assert_eq!(g.packages.len(), 113);
        let ts = by_name(&g, "typescript")[0];
        assert_eq!(
            ts.bin,
            Bin::Map {
                entries: vec![
                    ("tsc".into(), "bin/tsc".into()),
                    ("tsserver".into(), "bin/tsserver".into())
                ]
            }
        );
        assert_eq!(
            by_name(&g, "tsd")[0].bin,
            Bin::NamedFile {
                name: "tsd".into(),
                path: "dist/cli.js".into()
            }
        );
    }
    for name in [
        "fixtures/workspace-name-dist-tag.lockb",
        "fixtures/workspace-name-optional-link.lockb",
        "fixtures/workspace-name-optional-peer.lockb",
        "fixtures/workspace-name-optional-range.lockb",
        "fixtures/invalid-optional-peer.lockb",
    ] {
        if let Some(bytes) = fixture(name) {
            let g = bun_binary::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            g.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
}

#[test]
fn rejects_corrupt_lockfiles() {
    assert!(bun_binary::parse(b"").is_err());
    assert!(bun_binary::parse(b"#!/usr/bin/env bun\nbun-lockfile-format-v1\n").is_err());
    let Some(bytes) = fixture("fixtures/bun.lockb.v2") else {
        return;
    };
    // Truncated.
    assert!(bun_binary::parse(&bytes[..bytes.len() / 2]).is_err());
    // An unknown format version.
    let mut newer = bytes.clone();
    newer[42] = 4;
    assert!(
        bun_binary::parse(&newer)
            .unwrap_err()
            .to_string()
            .contains("UnexpectedLockfileVersion")
    );
    let mut older = bytes.clone();
    older[42] = 1;
    assert!(
        bun_binary::parse(&older)
            .unwrap_err()
            .to_string()
            .contains("OutdatedLockfileVersion")
    );
    // 0 is the state before a package is resolved, 100 a placeholder: nothing installs them.
    for tag in [0u8, 100] {
        for name in [
            "fixtures/bun.lockb.v2",
            "fixtures/unnamed-folder-dependency.lockb",
        ] {
            let mut lockb = fixture(name).unwrap();
            let l = layout(&lockb);
            lockb[l.resolution + l.resolution_size] = tag;
            let e = bun_binary::parse(&lockb).unwrap_err().to_string();
            assert!(e.contains("invalid resolution tag"), "{name} {tag}: {e}");
        }
    }
}

#[test]
fn rejects_corrupt_bin_maps() {
    let Some(base) = fixture("bun.lockb.bin-linking") else {
        return;
    };
    let l = layout(&base);
    let bin = (0..l.count)
        .map(|id| l.bin + id * 20)
        .find(|&b| base[b] == 4)
        .expect("a map bin");
    assert!(u32::from_le_bytes(base[bin + 8..bin + 12].try_into().unwrap()) > 3);
    // Ends past the end of the buffer.
    let mut past = base.clone();
    past[bin + 4..bin + 8].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
    assert!(
        bun_binary::parse(&past)
            .unwrap_err()
            .to_string()
            .contains("InvalidLockfile")
    );
    // A name without a target.
    let mut odd = base.clone();
    odd[bin + 8..bin + 12].copy_from_slice(&3u32.to_le_bytes());
    assert!(
        bun_binary::parse(&odd)
            .unwrap_err()
            .to_string()
            .contains("InvalidLockfile")
    );
    assert!(bun_binary::parse(&base).is_ok());
}

#[test]
fn catalog_row_with_plain_literal_loads() {
    // A catalog dependency row whose literal is not a catalog reference still loads.
    let Some(mut lockb) = fixture("fixtures/bun.lockb.v2-most-features") else {
        return;
    };
    let row = (0..lockb.len() - 26)
        .find(|&i| lockb[i + 17] == 9 && &lockb[i + 18..i + 26] == b"catalog:")
        .expect("catalog row");
    lockb[row + 21..row + 26].fill(0);
    let g = bun_binary::parse(&lockb).unwrap();
    assert!(g.root().dependencies.iter().any(|e| e.literal == "cat"));
    let _ = PackageId::ROOT;
}
