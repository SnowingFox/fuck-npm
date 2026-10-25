//! package-lock.json migration and export, on small lockfiles that each exercise one rule of
//! bun's `npm_lock.rs`.

use fnpm_types::{Behavior, Bin, ConfigVersion, LockGraph, Package, PackageId, Resolution};
use serde_json::{Value, json};

use super::support::{empty_workspace, tree_from_npm_locations, workspace};
use crate::formats::npm;
use crate::migration::{MigrateOptions, Report, migrate_dir};

const SHA: &str = "sha512-nne9/IiQ/hzIhY6pdDnbBtz7DjPTKrY00P/zvPSm5pOFkl6xuGrGnXn/VtTNNfNtAfZ9/1RtehkszU9qcTii0Q==";

fn lock(packages: Value) -> Vec<u8> {
    serde_json::to_vec(
        &json!({ "name": "app", "lockfileVersion": 3, "requires": true, "packages": packages }),
    )
    .unwrap()
}

fn migrate(packages: Value) -> (LockGraph, Report) {
    npm::migrate(
        &lock(packages),
        &empty_workspace(),
        &MigrateOptions::default(),
        "package-lock.json",
        None,
    )
    .unwrap()
}

fn find<'g>(g: &'g LockGraph, name: &str) -> &'g Package {
    g.packages
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("no {name}"))
}

fn target<'g>(g: &'g LockGraph, from: &Package, dep: &str) -> Option<&'g Package> {
    from.dependency(dep)
        .and_then(|e| e.target)
        .map(|t| &g.packages[t.index()])
}

#[test]
fn rejects_unsupported_versions_and_shapes() {
    let ws = empty_workspace();
    let o = MigrateOptions::default();
    for (doc, message) in [
        (
            json!({ "lockfileVersion": 1, "dependencies": {} }),
            "unsupported lockfileVersion 1",
        ),
        (
            json!({ "lockfileVersion": 5, "packages": { "": {} } }),
            "unsupported lockfileVersion 5",
        ),
        (json!({ "packages": { "": {} } }), "InvalidNPMLockfile"),
        (
            json!({ "lockfileVersion": "3", "packages": { "": {} } }),
            "InvalidNPMLockfile",
        ),
        (
            json!({ "lockfileVersion": 3, "packages": { "node_modules/a": {}, "": {} } }),
            "InvalidNPMLockfile",
        ),
        (json!({ "lockfileVersion": 3 }), "InvalidNPMLockfile"),
        (
            json!({ "lockfileVersion": 3, "packages": { "": [] } }),
            "InvalidNPMLockfile",
        ),
        (
            json!({ "lockfileVersion": 3, "packages": { "": { "link": true } } }),
            "InvalidNPMLockfile",
        ),
        (
            json!({ "lockfileVersion": 3, "packages": { "": { "workspaces": "a" } } }),
            "InvalidNPMLockfile",
        ),
        (
            json!({ "lockfileVersion": 3, "packages": { "": { "dependencies": { "a": "1" } }, "node_modules/a": { "version": "1.0.0", "hasInstallScript": "yes" } } }),
            "hasInstallScript",
        ),
        (
            json!({ "lockfileVersion": 3, "packages": { "": { "dependencies": { "a": "1" } }, "node_modules/a": { "version": "1.0.0", "bin": "cli.js" } } }),
            "bin",
        ),
        (
            json!({ "lockfileVersion": 3, "packages": { "": { "dependencies": { "a": "github:u/r" } }, "node_modules/a": { "version": "1.0.0", "resolved": "git+ssh://git@github.com/u/r.git" } } }),
            "no safe commit",
        ),
    ] {
        let bytes = serde_json::to_vec(&doc).unwrap();
        let e = npm::migrate(&bytes, &ws, &o, "package-lock.json", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains(message), "{doc}: {e}");
    }
    // Version 2 and 4 migrate.
    for v in [2, 4] {
        let bytes =
            serde_json::to_vec(&json!({ "lockfileVersion": v, "packages": { "": {} } })).unwrap();
        assert!(npm::migrate(&bytes, &ws, &o, "package-lock.json", None).is_ok());
    }
}

#[test]
fn registry_packages_and_tarball_urls() {
    let (g, report) = migrate(json!({
        "": { "name": "app", "version": "1.0.0", "dependencies": { "a": "^1.0.0", "b": "^1.0.0", "c": "^1.0.0", "d": "^1.0.0" } },
        "node_modules/a": { "version": "1.0.0", "resolved": "https://registry.npmjs.org/a/-/a-1.0.0.tgz", "integrity": SHA, "license": "MIT" },
        "node_modules/b": { "version": "1.0.0", "resolved": "https://mirror.example/b/-/b-1.0.0.tgz", "integrity": SHA },
        "node_modules/c": { "version": "1.0.0", "resolved": "https://mirror.example/c/-/c-1.0.0.tgz" },
        "node_modules/d": { "version": "1.0.0" }
    }));
    g.validate().unwrap();
    assert_eq!(g.config_version, ConfigVersion::V0);
    assert_eq!(g.root().name, "app");
    let a = find(&g, "a");
    assert_eq!(
        a.resolution,
        Resolution::Npm {
            version: "1.0.0".into(),
            tarball_url: None,
            integrity: Some(SHA.parse().unwrap())
        }
    );
    assert_eq!(a.meta.license.as_deref(), Some("MIT"));
    // A mirror with integrity keeps its URL.
    assert!(
        matches!(&find(&g, "b").resolution, Resolution::Npm { tarball_url: Some(u), .. } if u.starts_with("https://mirror.example/"))
    );
    // Without integrity it cannot be verified, so the configured registry's copy is used.
    let c = find(&g, "c");
    assert!(matches!(
        &c.resolution,
        Resolution::Npm {
            tarball_url: None,
            integrity: None,
            ..
        }
    ));
    let c_id = PackageId(g.packages.iter().position(|p| p.name == "c").unwrap() as u32);
    assert_eq!(report.missing_integrity, vec![c_id]);
    assert_eq!(
        report.warnings,
        [
            "node_modules/c: missing integrity, downloading from configured registry instead of https://mirror.example/c/-/c-1.0.0.tgz"
        ]
    );
    // No `resolved`: the configured registry's default location.
    assert!(matches!(
        &find(&g, "d").resolution,
        Resolution::Npm {
            tarball_url: None,
            integrity: None,
            ..
        }
    ));
    let custom = MigrateOptions {
        registry: "https://npm.corp.example/".into(),
        ..MigrateOptions::default()
    };
    let bytes = lock(
        json!({ "": { "dependencies": { "d": "1" } }, "node_modules/d": { "version": "1.0.0" } }),
    );
    let (g, _) = npm::migrate(
        &bytes,
        &empty_workspace(),
        &custom,
        "package-lock.json",
        None,
    )
    .unwrap();
    assert!(
        matches!(&find(&g, "d").resolution, Resolution::Npm { tarball_url: Some(u), .. } if u == "https://npm.corp.example/d/-/d-1.0.0.tgz")
    );
}

#[test]
fn nested_lookup_dedupe_and_bundles() {
    let (g, _) = migrate(json!({
        "": { "dependencies": { "a": "1", "b": "1" } },
        "node_modules/a": { "version": "1.0.0", "integrity": SHA, "dependencies": { "c": "^2" }, "bundleDependencies": ["c"] },
        "node_modules/a/node_modules/c": { "version": "2.0.0", "inBundle": true, "dependencies": { "d": "1" } },
        "node_modules/b": { "version": "1.0.0", "integrity": SHA, "dependencies": { "c": "^1", "d": "1" } },
        "node_modules/c": { "version": "1.0.0", "integrity": SHA },
        "node_modules/b/node_modules/a": { "version": "1.0.0", "integrity": SHA },
        "node_modules/d": { "version": "1.0.0", "integrity": SHA },
        "node_modules/unused": { "version": "1.0.0", "integrity": SHA },
        "node_modules/ext": { "version": "1.0.0", "extraneous": true }
    }));
    let a = find(&g, "a");
    let c2 = target(&g, a, "c").unwrap();
    assert_eq!(c2.version, "2.0.0");
    assert!(a.dependency("c").unwrap().behavior.is_bundled());
    assert!(c2.meta.bundled);
    // The bundled copy's dependency walks up to the root's node_modules.
    assert_eq!(target(&g, c2, "d").unwrap().version, "1.0.0");
    let b = find(&g, "b");
    assert_eq!(target(&g, b, "c").unwrap().version, "1.0.0");
    // The copy of `a` under `b` is the same package.
    assert_eq!(g.packages.iter().filter(|p| p.name == "a").count(), 1);
    assert!(
        g.packages
            .iter()
            .all(|p| p.name != "unused" && p.name != "ext")
    );
}

#[test]
fn groups_peers_and_duplicates() {
    let (g, _) = migrate(json!({
        "": {
            "dependencies": { "a": "1", "dup": "1" },
            "optionalDependencies": { "dup": "1", "gone": "1" },
            "devDependencies": { "dev": "1" },
            "peerDependencies": { "p": "1", "q": "1", "missing": "1" },
            "peerDependenciesMeta": { "q": { "optional": true } }
        },
        "node_modules/a": { "version": "1.0.0", "integrity": SHA,
            "dependencies": { "x": "1" }, "peerDependencies": { "x": "1", "p": "1" },
            "optionalDependencies": { "x": "1" } },
        "node_modules/x": { "version": "1.0.0", "integrity": SHA },
        "node_modules/dup": { "version": "1.0.0", "integrity": SHA, "optional": true },
        "node_modules/dev": { "version": "1.0.0", "integrity": SHA, "dev": true },
        "node_modules/p": { "version": "1.0.0", "integrity": SHA }
    }));
    let root = g.root();
    let dups: Vec<_> = root
        .dependencies
        .iter()
        .filter(|e| e.name == "dup")
        .collect();
    assert_eq!(dups.len(), 1, "optionalDependencies replaces dependencies");
    assert!(dups[0].behavior.is_optional());
    assert!(
        root.dependency("gone").unwrap().target.is_none(),
        "a missing optional stays unresolved"
    );
    assert!(root.dependency("dev").unwrap().behavior.is_dev());
    let q = root.dependency("q").unwrap();
    assert!(q.behavior.is_optional_peer() && q.target.is_none());
    assert!(
        root.dependency("missing").is_none(),
        "an unresolved required root peer is left for the resolver"
    );
    assert!(root.dependency("p").unwrap().target.is_some());
    let a = find(&g, "a");
    // A registry package's peer that duplicates a dependency is skipped; an optional one
    // replaces it.
    let xs: Vec<_> = a.dependencies.iter().filter(|e| e.name == "x").collect();
    assert_eq!(xs.len(), 1);
    assert!(xs[0].behavior.is_optional());
    assert!(a.dependency("p").unwrap().behavior.is_peer());

    // `--frozen-lockfile` keeps the unresolved root peer.
    let frozen = MigrateOptions {
        frozen_lockfile: true,
        ..MigrateOptions::default()
    };
    let bytes = lock(json!({ "": { "peerDependencies": { "missing": "1" } } }));
    let (g, _) = npm::migrate(
        &bytes,
        &empty_workspace(),
        &frozen,
        "package-lock.json",
        None,
    )
    .unwrap();
    assert!(
        g.root()
            .dependency("missing")
            .is_some_and(|e| e.target.is_none())
    );
}

#[test]
fn platform_bins_and_scripts() {
    let (g, _) = migrate(json!({
        "": { "dependencies": { "a": "1", "b": "1", "c": "1" } },
        "node_modules/a": { "version": "1.0.0", "integrity": SHA, "os": "linux", "cpu": ["x64", "arm64"], "libc": ["glibc"],
            "bin": { "a": "cli.js" }, "hasInstallScript": true, "deprecated": "old" },
        "node_modules/b": { "version": "1.0.0", "integrity": SHA, "os": ["!win32"], "bin": { "b1": "1.js", "b2": "2.js" } },
        "node_modules/c": { "version": "1.0.0", "integrity": SHA, "bin": { "other": "x.js" } }
    }));
    let a = find(&g, "a");
    assert_eq!(a.platform.os.names().collect::<Vec<_>>(), ["linux"]);
    assert_eq!(a.platform.cpu.names().collect::<Vec<_>>(), ["arm64", "x64"]);
    assert_eq!(a.platform.libc.names().collect::<Vec<_>>(), ["glibc"]);
    assert_eq!(
        a.bin,
        Bin::File {
            path: "cli.js".into()
        }
    );
    assert!(a.meta.has_install_script);
    assert_eq!(a.meta.deprecated.as_deref(), Some("old"));
    let b = find(&g, "b");
    assert!(
        !b.platform.os.names().any(|n| n == "win32") && b.platform.os.names().any(|n| n == "linux")
    );
    assert!(matches!(&b.bin, Bin::Map { entries } if entries.len() == 2));
    assert_eq!(
        find(&g, "c").bin,
        Bin::NamedFile {
            name: "other".into(),
            path: "x.js".into()
        }
    );
}

#[test]
fn git_github_tarball_and_folder_resolutions() {
    let commit = "3754568bfd43a841d2d72d7fb54598635aea8fa4";
    let (g, _) = migrate(json!({
        "": { "dependencies": {
            "gh": "github:substack/minimist", "gl": "git+https://gitlab.com/u/r.git", "tgz": "https://example.com/t.tgz",
            "local": "file:vendor/l.tgz", "dir": "file:dir", "copied": "file:copied"
        } },
        "node_modules/gh": { "version": "1.2.0", "resolved": format!("git+ssh://git@github.com/substack/minimist.git#{commit}"), "integrity": SHA },
        "node_modules/gl": { "version": "1.0.0", "resolved": format!("git+https://gitlab.com/u/r.git#{commit}") },
        "node_modules/tgz": { "version": "1.0.0", "resolved": "https://example.com/t.tgz", "integrity": SHA },
        "node_modules/local": { "version": "1.0.0", "resolved": "file:vendor/l.tgz", "integrity": SHA },
        "node_modules/dir": { "resolved": "dir", "link": true },
        "dir": { "name": "dir", "version": "0.0.1", "dependencies": { "a": "1" } },
        "node_modules/a": { "version": "1.0.0", "integrity": SHA },
        "node_modules/copied": { "version": "1.0.0", "resolved": "file:copied" }
    }));
    assert_eq!(
        find(&g, "gh").resolution,
        Resolution::Github {
            owner: "substack".into(),
            repo: "minimist".into(),
            committish: commit.into(),
            resolved_commit: commit.into(),
            integrity: Some(SHA.parse().unwrap())
        }
    );
    assert!(
        matches!(&find(&g, "gl").resolution, Resolution::Git { url, resolved_commit, .. } if url == "https://gitlab.com/u/r.git" && resolved_commit == commit)
    );
    assert!(
        matches!(&find(&g, "tgz").resolution, Resolution::RemoteTarball { url, integrity: Some(_) } if url == "https://example.com/t.tgz")
    );
    assert!(
        matches!(&find(&g, "local").resolution, Resolution::LocalTarball { path, .. } if path == "vendor/l.tgz")
    );
    let dir = find(&g, "dir");
    assert_eq!(dir.resolution, Resolution::Folder { path: "dir".into() });
    assert!(target(&g, dir, "a").is_some());
    // `--install-links` copies are left for the install to resolve.
    assert!(g.root().dependency("copied").is_none());
}

#[test]
fn workspaces_from_link_entries() {
    let ws = workspace(
        json!({ "name": "root", "workspaces": ["packages/*"] }),
        &[
            (
                "packages/lib",
                json!({ "name": "@app/lib", "version": "0.1.0" }),
            ),
            ("packages/new", json!({ "name": "new" })),
        ],
    );
    let bytes = lock(json!({
        "": { "name": "root", "workspaces": ["packages/*"], "dependencies": { "@app/lib": "*" } },
        "packages/lib": { "name": "@app/lib", "version": "0.1.0", "dependencies": { "a": "^1" } },
        "packages/lib/node_modules/a": { "version": "1.0.0", "integrity": SHA },
        "node_modules/@app/lib": { "resolved": "packages/lib", "link": true }
    }));
    let (g, report) = npm::migrate(
        &bytes,
        &ws,
        &MigrateOptions::default(),
        "package-lock.json",
        None,
    )
    .unwrap();
    g.validate().unwrap();
    assert_eq!(
        g.workspaces.keys().collect::<Vec<_>>(),
        ["", "packages/lib"]
    );
    let lib = &g.packages[g.workspaces["packages/lib"].index()];
    assert_eq!(
        (lib.name.as_str(), lib.version.as_str()),
        ("@app/lib", "0.1.0")
    );
    assert_eq!(
        lib.resolution,
        Resolution::Workspace {
            path: "packages/lib".into()
        }
    );
    assert!(
        g.root()
            .dependency("@app/lib")
            .unwrap()
            .behavior
            .contains(Behavior::WORKSPACE)
    );
    assert_eq!(target(&g, lib, "a").unwrap().version, "1.0.0");
    assert_eq!(
        report.verbose,
        ["workspace \"new\" (packages/new) not in package-lock.json, resolving from package.json"]
    );
}

#[test]
fn migrate_dir_prefers_shrinkwrap_and_falls_through() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(
        json!({ "trustedDependencies": ["a"], "patchedDependencies": { "a@1.0.0": "patches/a.patch" } }),
        &[],
    );
    let package_lock = lock(
        json!({ "": { "dependencies": { "a": "1" } }, "node_modules/a": { "version": "1.0.0", "integrity": SHA } }),
    );
    std::fs::write(dir.path().join("package-lock.json"), &package_lock).unwrap();
    std::fs::write(
        dir.path().join("npm-shrinkwrap.json"),
        br#"{ "lockfileVersion": 1, "dependencies": {} }"#,
    )
    .unwrap();
    let migrated = migrate_dir(dir.path(), &ws, &MigrateOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(migrated.path.file_name().unwrap(), "package-lock.json");
    assert_eq!(
        migrated.report.warnings,
        [
            "cannot migrate npm-shrinkwrap.json: unsupported lockfileVersion 1, trying package-lock.json"
        ]
    );
    assert_eq!(
        migrated.graph.trusted_dependencies,
        Some(["a".to_owned()].into())
    );
    assert_eq!(
        migrated.graph.patched_dependencies["a@1.0.0"].path,
        "patches/a.patch"
    );

    std::fs::write(dir.path().join("npm-shrinkwrap.json"), &package_lock).unwrap();
    let migrated = migrate_dir(dir.path(), &ws, &MigrateOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(migrated.path.file_name().unwrap(), "npm-shrinkwrap.json");
    assert!(migrated.report.warnings.is_empty());

    let empty = tempfile::tempdir().unwrap();
    assert!(
        migrate_dir(empty.path(), &ws, &MigrateOptions::default())
            .unwrap()
            .is_none()
    );
    std::fs::write(empty.path().join("package-lock.json"), b"{ not json").unwrap();
    assert!(migrate_dir(empty.path(), &ws, &MigrateOptions::default()).is_err());
}

#[test]
fn exports_lockfile_version_3() {
    let ws = workspace(
        json!({ "name": "root", "version": "1.0.0", "workspaces": ["packages/lib"] }),
        &[("packages/lib", json!({ "name": "lib", "version": "0.1.0" }))],
    );
    let bytes = lock(json!({
        "": { "name": "root", "version": "1.0.0", "workspaces": ["packages/lib"],
              "dependencies": { "a": "^1", "lib": "*" }, "devDependencies": { "d": "1" }, "optionalDependencies": { "o": "1" } },
        "packages/lib": { "name": "lib", "version": "0.1.0" },
        "node_modules/lib": { "resolved": "packages/lib", "link": true },
        "node_modules/a": { "version": "1.0.0", "resolved": "https://registry.npmjs.org/a/-/a-1.0.0.tgz", "integrity": SHA,
            "dependencies": { "b": "^2" }, "bin": { "a": "cli.js" }, "os": ["darwin"] },
        "node_modules/a/node_modules/b": { "version": "2.0.0", "integrity": SHA },
        "node_modules/d": { "version": "1.0.0", "integrity": SHA, "dev": true },
        "node_modules/o": { "version": "1.0.0", "integrity": SHA, "optional": true, "hasInstallScript": true }
    }));
    let (graph, _, locations) = npm::migrate_with_locations(
        &bytes,
        &ws,
        &MigrateOptions::default(),
        "package-lock.json",
        None,
    )
    .unwrap();
    let tree = tree_from_npm_locations(&graph, &locations);
    let out = npm::serialize_with_tree(&graph, &tree, fnpm_config::DEFAULT_REGISTRY).unwrap();
    assert!(out.ends_with("}\n"));
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["lockfileVersion"], 3);
    assert_eq!(doc["name"], "root");
    let p = &doc["packages"];
    assert_eq!(p[""]["workspaces"], json!(["packages/lib"]));
    assert_eq!(p[""]["devDependencies"], json!({ "d": "1" }));
    assert_eq!(
        p["node_modules/lib"],
        json!({ "resolved": "packages/lib", "link": true })
    );
    assert_eq!(
        p["packages/lib"],
        json!({ "name": "lib", "version": "0.1.0" })
    );
    assert_eq!(
        p["node_modules/a"]["resolved"],
        "https://registry.npmjs.org/a/-/a-1.0.0.tgz"
    );
    assert_eq!(p["node_modules/a"]["integrity"], SHA);
    assert_eq!(p["node_modules/a"]["bin"], json!({ "a": "cli.js" }));
    assert_eq!(p["node_modules/a"]["os"], json!(["darwin"]));
    assert_eq!(p["node_modules/a/node_modules/b"]["version"], "2.0.0");
    assert_eq!(p["node_modules/d"]["dev"], true);
    assert_eq!(p["node_modules/o"]["optional"], true);
    assert_eq!(p["node_modules/o"]["hasInstallScript"], true);
    assert!(p["node_modules/a"].get("dev").is_none());
    // And it migrates back to the same graph.
    let (again, _) = npm::migrate(
        out.as_bytes(),
        &ws,
        &MigrateOptions::default(),
        "package-lock.json",
        None,
    )
    .unwrap();
    assert_eq!(
        super::support::normalize(&again),
        super::support::normalize(&graph)
    );
}
