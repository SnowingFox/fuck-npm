//! pnpm-lock.yaml migration and v9 export, against bun's fixtures and expectations
//! (`test/cli/install/migration/pnpm-*.test.ts`).

mod common;

use std::path::Path;

use common::*;
use fnpm_lockfile::formats::foreign::Migration;
use fnpm_lockfile::formats::pnpm;
use fnpm_types::{Behavior, ConfigVersion, LockGraph, Resolution};
use serde_json::{Value, json};

const REGISTRY: &str = "http://localhost:1234/";

fn run(dir: &Path) -> fnpm_lockfile::Result<Migration> {
    let ws = workspace(dir);
    let files = ScopedFiles(dir.to_path_buf());
    let mut o = options(&files);
    o.registry = REGISTRY.into();
    let bytes = std::fs::read(dir.join("pnpm-lock.yaml")).unwrap();
    pnpm::migrate(&bytes, &ws, &o)
}

fn fixture(name: &str) -> Migration {
    run(&fixtures().join("pnpm").join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn fixture_err(name: &str) -> String {
    run(&fixtures().join("pnpm").join(name))
        .unwrap_err()
        .to_string()
}

fn project(files: &[(&str, String)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    dir
}

fn ids(g: &LockGraph) -> Vec<String> {
    let mut v: Vec<String> = g.packages.iter().skip(1).map(bun_id).collect();
    v.sort();
    v
}

fn has(g: &LockGraph, id: &str) -> bool {
    g.packages.iter().any(|p| bun_id(p) == id)
}

fn pkg<'g>(g: &'g LockGraph, id: &str) -> &'g fnpm_types::Package {
    g.packages
        .iter()
        .find(|p| bun_id(p) == id)
        .unwrap_or_else(|| panic!("no {id} in {:?}", ids(g)))
}

fn target<'g>(
    g: &'g LockGraph,
    owner: &fnpm_types::Package,
    name: &str,
) -> &'g fnpm_types::Package {
    let e = owner
        .dependency(name)
        .unwrap_or_else(|| panic!("no edge {name}"));
    &g.packages[e
        .target
        .unwrap_or_else(|| panic!("{name} unresolved"))
        .index()]
}

fn root_edit(m: &Migration, root: Value) -> Value {
    let mut root = root;
    for e in m.write_back.iter().filter(|e| e.path.is_empty()) {
        e.apply_to(&mut root);
    }
    root
}

#[test]
fn basic_matches_bun_snapshot() {
    let m = fixture("basic");
    let lock = snapshot("pnpm-migration.test.ts.snap", "basic: bun.lock 1");
    assert_matches_bun_lock(&m.graph, &lock, REGISTRY);
    assert_eq!(m.graph.config_version, ConfigVersion::V1);
    // the root's peer-only `a-dep` sits under the importer's dependencies; it stays a peer
    let root = m.graph.root();
    assert_eq!(
        root.dependencies
            .iter()
            .filter(|e| e.name == "a-dep")
            .count(),
        1
    );
    let a_dep = root.dependency("a-dep").unwrap();
    assert!(a_dep.behavior.is_peer());
    assert_eq!(
        a_dep
            .target
            .map(|t| bun_id(&m.graph.packages[t.index()]))
            .as_deref(),
        Some("a-dep@1.0.1")
    );
}

#[test]
fn git_and_userinfo_tarball_references_match_bun_snapshot() {
    let m = fixture("v9-git-references");
    let lock = snapshot(
        "pnpm-lock-v9.test.ts.snap",
        "pnpm-lock.yaml v9 v9 git and userinfo-tarball references migrate: bun.lock 1",
    );
    assert_matches_bun_lock(&m.graph, &lock, REGISTRY);
    let root = m.graph.root();
    assert_eq!(
        root.dependency("pkg-a").unwrap().literal,
        "git+ssh://git@example.com/org/pkg-a.git#v1"
    );
    assert_eq!(
        root.dependency("hue").unwrap().literal,
        "github:org/hue#ec3d1d1"
    );
}

#[test]
fn alias_whose_dep_path_is_a_file_matches_bun_snapshot() {
    let m = fixture("v9-alias-non-registry-dep-path");
    let lock = snapshot(
        "pnpm-lock-v9.test.ts.snap",
        "pnpm-lock.yaml v9 snapshot alias whose dep-path version is a file: directory or tarball: bun.lock 1",
    );
    assert_matches_bun_lock(&m.graph, &lock, REGISTRY);
    // as outer/package.json declares it, not pnpm's lockfile-relative `file:shared/config`
    assert_eq!(
        pkg(&m.graph, "outer@file:outer")
            .dependency("config")
            .unwrap()
            .literal,
        "file:../shared/config"
    );
}

#[test]
fn errors_name_what_is_missing() {
    for (fixture, expected) in [
        (
            "v9-missing-package-entry",
            "pnpm-lock.yaml has no package entry 'no-deps@1.0.0' for dependency 'no-deps' of importer '.'",
        ),
        (
            "v9-missing-package-entry-workspace",
            "pnpm-lock.yaml has no package entry 'no-deps@1.0.0' for dependency 'no-deps' of importer 'packages/a'",
        ),
        (
            "v9-missing-package-entry-transitive",
            "pnpm-lock.yaml has no package entry 'no-deps@9.9.9' for dependency 'no-deps' of package 'pkg-a'",
        ),
        (
            "v9-missing-importer-package-json",
            "pnpm-lock.yaml lists importer 'packages/gone' but 'packages/gone/package.json' does not exist",
        ),
        (
            "v9-git-subdirectory",
            "pnpm-lock.yaml package 'pkg@git+ssh://git@example.com/org/monorepo.git#cba04669e621b85fbdb33371604de1a2898e68e9&path:packages/pkg' is a git sub-directory dependency",
        ),
        ("version-number-dot", "unsupported lockfileVersion 5.4"),
    ] {
        let err = fixture_err(fixture);
        assert!(err.contains(expected), "{fixture}: {err}");
    }
}

#[test]
fn alias_in_optional_dependencies() {
    let m = fixture("v9-alias-in-optional-dependencies");
    let g = &m.graph;
    let one_dep = pkg(g, "one-dep@1.0.0");
    let e = one_dep.dependency("aliased-no-deps").unwrap();
    assert_eq!(e.literal, "npm:no-deps@2.0.0");
    assert_eq!(e.behavior, Behavior::OPTIONAL);
    assert_eq!(
        bun_id(target(g, one_dep, "aliased-no-deps")),
        "no-deps@2.0.0"
    );
}

#[test]
fn multi_document_and_runtime_entries() {
    let m = fixture("v9-multi-document");
    assert!(has(
        &m.graph,
        "pkg-a@git+ssh://git@example.com/org/pkg-a.git#0123456789abcdef0123456789abcdef01234567"
    ));
    assert!(
        !ids(&m.graph)
            .iter()
            .any(|i| i.contains("plugin-better-defaults"))
    );

    let m = fixture("v9-runtime-entries");
    assert!(m.warnings.iter().any(|w| w == "skipping \"node@runtime:22.0.0\" from pnpm-lock.yaml: runtime dependencies are not supported"));
    assert!(!ids(&m.graph).iter().any(|i| i.starts_with("node@")));
    assert!(m.graph.root().dependency("node").is_none());

    let dir = project(&[
        ("package.json", json!({ "name": "empty-main-document" }).to_string()),
        ("pnpm-lock.yaml", "---\nlockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    configDependencies: {}\n\n---\n".into()),
    ]);
    assert!(
        run(dir.path())
            .unwrap_err()
            .to_string()
            .contains("pnpm-lock.yaml root must be an object")
    );
}

const IS_POSITIVE: &str =
    "is-positive@github:kevva/is-positive#97edff6f525f192a3f83cea1944765f769ae2678";

#[test]
fn patched_git_hosted_packages() {
    for form in ["legacy", "bare"] {
        let dir = fixtures()
            .join("pnpm")
            .join(format!("v9-patched-git-hosted-{form}"));
        let m = run(&dir).unwrap();
        assert!(has(&m.graph, IS_POSITIVE), "{form}");
        assert_eq!(
            m.graph.patched_dependencies[IS_POSITIVE].path, "patches/is-positive@3.1.0.patch",
            "{form}"
        );
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap())
                .unwrap();
        let after = root_edit(&m, raw);
        assert_eq!(
            after["patchedDependencies"],
            json!({ IS_POSITIVE: "patches/is-positive@3.1.0.patch" }),
            "{form}"
        );
    }
    // a bare hash whose patch is not in the config is skipped with a warning
    let dir = tempfile::tempdir().unwrap();
    for f in ["package.json", "pnpm-lock.yaml"] {
        std::fs::copy(
            fixtures().join("pnpm/v9-patched-git-hosted-bare").join(f),
            dir.path().join(f),
        )
        .unwrap();
    }
    let m = run(dir.path()).unwrap();
    assert!(m.warnings.iter().any(|w| w == "skipping patch \"is-positive@3.1.0\" from pnpm-lock.yaml: not in patchedDependencies of package.json or pnpm-workspace.yaml"));
    assert!(m.graph.patched_dependencies.is_empty());
}

#[test]
fn patch_bare_hash_registry() {
    let dir = fixtures().join("pnpm/v9-patch-bare-hash-registry");
    let m = run(&dir).unwrap();
    assert_eq!(
        m.graph.patched_dependencies["no-deps@1.0.1"].path,
        "patches/no-deps.patch"
    );
    let raw: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        root_edit(&m, raw)["patchedDependencies"],
        json!({ "no-deps@1.0.1": "patches/no-deps.patch" })
    );
}

#[test]
fn catalog_default() {
    let m = fixture("v9-catalog-default");
    assert!(has(&m.graph, "no-deps@1.0.1"));
    assert_eq!(m.graph.catalogs.get("", "no-deps"), Some("^1.0.0"));
    // without pnpm-workspace.yaml the lockfile's catalogs section is enough
    let dir = tempfile::tempdir().unwrap();
    for f in ["package.json", "pnpm-lock.yaml"] {
        std::fs::copy(
            fixtures().join("pnpm/v9-catalog-default").join(f),
            dir.path().join(f),
        )
        .unwrap();
    }
    let m = run(dir.path()).unwrap();
    assert_eq!(m.graph.catalogs.get("default", "no-deps"), Some("^1.0.0"));
    // an importer `catalog:` reference without a catalogs section is reported
    let text = std::fs::read_to_string(dir.path().join("pnpm-lock.yaml")).unwrap();
    let start = text.find("catalogs:").unwrap();
    let end = text.find("importers:").unwrap();
    std::fs::write(
        dir.path().join("pnpm-lock.yaml"),
        format!("{}{}", &text[..start], &text[end..]),
    )
    .unwrap();
    let err = run(dir.path()).unwrap_err().to_string();
    assert!(
        err.contains("pnpm-lock.yaml catalog 'default' missing entry for dependency 'no-deps'"),
        "{err}"
    );
}

#[test]
fn reference_shapes() {
    let m = fixture("v9-reference-shapes");
    let g = &m.graph;
    for id in [
        "@types/no-deps@1.0.0",
        "@types/no-deps@2.0.0",
        "@types/is-number@1.0.0",
        "no-deps@1.0.1",
        "tb@tb-1.0.0.tgz",
    ] {
        assert!(has(g, id), "{id}: {:?}", ids(g));
    }
    let one_dep = pkg(g, "one-dep@1.0.0");
    assert_eq!(
        one_dep.dependency("nd").unwrap().literal,
        "npm:no-deps@1.0.1"
    );
    assert_eq!(
        one_dep.dependency("tnd").unwrap().literal,
        "npm:@types/no-deps@2.0.0"
    );
    assert_eq!(
        one_dep.dependency("tb").unwrap().literal,
        "file:tb-1.0.0.tgz"
    );
    assert_eq!(
        bun_id(target(g, one_dep, "@types/no-deps")),
        "@types/no-deps@1.0.0"
    );
    // the peer suffix binds @types/no-deps' peer
    let types = pkg(g, "@types/no-deps@1.0.0");
    assert_eq!(
        bun_id(target(g, types, "@types/is-number")),
        "@types/is-number@1.0.0"
    );
}

#[test]
fn local_tarballs() {
    let m = fixture("v9-local-tarballs");
    let g = &m.graph;
    for id in [
        "tar-pkg@../tar-pkg-1.0.0.tgz",
        "tar-gz-pkg@../tar-gz-pkg-1.0.0.tar.gz",
        "plain@../plain-1.0.0.tar",
        "upper@../UPPER-1.0.0.TGZ",
    ] {
        let p = pkg(g, id);
        assert!(
            matches!(
                &p.resolution,
                Resolution::LocalTarball {
                    integrity: Some(_),
                    ..
                }
            ),
            "{id}"
        );
    }
    for file in ["up-1.0.0.TGZ", "mixed-1.0.0.Tar.Gz", "plain-1.0.0.tar"] {
        let lock = format!(
            "lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      pkg:\n        specifier: file:vendor/{file}\n        version: file:vendor/{file}\n\npackages:\n\n  pkg@file:vendor/{file}:\n    resolution: {{integrity: sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==}}\n    version: 1.0.0\n\nsnapshots:\n\n  pkg@file:vendor/{file}: {{}}\n"
        );
        let dir = project(&[
            ("package.json", json!({ "name": "tarball-spelling", "dependencies": { "pkg": format!("file:vendor/{file}") } }).to_string()),
            ("pnpm-lock.yaml", lock),
        ]);
        let m = run(dir.path()).unwrap();
        assert!(
            has(&m.graph, &format!("pkg@vendor/{file}")),
            "{file}: {:?}",
            ids(&m.graph)
        );
    }
}

#[test]
fn file_directory_with_nested_file_dependency() {
    let m = fixture("v9-file-directory");
    let g = &m.graph;
    assert_eq!(
        g.root().dependency("sub-dep").unwrap().literal,
        "file:./sub-dep"
    );
    let sub = pkg(g, "sub-dep@file:sub-dep");
    // stored relative to sub-dep, the way sub-dep/package.json declares it
    assert_eq!(
        sub.dependency("nested-child").unwrap().literal,
        "file:child"
    );
    assert_eq!(
        bun_id(target(g, sub, "nested-child")),
        "nested-child@file:sub-dep/child"
    );
}

#[test]
fn codeload_git_urls_and_orphans() {
    let m = fixture("v9-codeload-tarballs");
    assert!(has(
        &m.graph,
        "is-negative@github:kevva/is-negative#1d7e288222b53a0cab90a331f1865220ec29560c"
    ));
    assert!(has(
        &m.graph,
        "camelcase@github:denolib/camelcase#aeb6b15f9c9957c8fa56f9731e914c4d8a6d2f2b"
    ));
    assert!(
        !ids(&m.graph)
            .iter()
            .any(|i| i.contains("codeload.github.com"))
    );

    let m = fixture("v9-git-urls-and-orphan");
    assert!(has(
        &m.graph,
        "a@git+ssh://git@example.com:2222/org/a.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    assert!(has(
        &m.graph,
        "b@git+https://TOKEN:x-oauth-basic@github.com/foo/bar.git#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    ));
    assert!(!ids(&m.graph).iter().any(|i| i.contains("orphan")));
}

#[test]
fn injected_workspaces_resolve_to_the_workspace() {
    let m = fixture("v9-injected-workspace");
    assert!(has(&m.graph, "foo@workspace:packages/foo"));
    assert!(!ids(&m.graph).iter().any(|i| i.starts_with("foo@file:")));

    let m = fixture("v9-peer-variant-missing-resolution");
    let g = &m.graph;
    assert!(has(g, "pkg-a@workspace:packages/pkg-a"));
    assert!(has(g, "peer@workspace:packages/peer"));
    assert!(!has(g, "peer@1.0.0"));
    assert!(!ids(g).iter().any(|i| i.starts_with("pkg-a@file:")));
}

#[test]
fn snapshot_only_file_variants_are_rebuilt() {
    let m = fixture("v9-snapshot-only-file-variants");
    let g = &m.graph;
    assert!(has(g, "dir@workspace:packages/dir"));
    let local = pkg(g, "local@file:vendor/local");
    assert_eq!(local.dependency("no-deps").unwrap().literal, "1.0.0");
    assert!(
        !ids(g)
            .iter()
            .any(|i| i.starts_with("dir@file:") || i.starts_with("tb@"))
    );
}

#[test]
fn peer_variants_merge_and_dedupe() {
    let m = fixture("v9-peer-variant-merge");
    let g = &m.graph;
    let has_peer = pkg(g, "has-peer@file:vendor/has-peer");
    let peer = has_peer.dependency("peer").unwrap();
    assert!(peer.behavior.is_peer() && !peer.behavior.is_optional());
    assert_eq!(peer.literal, "*");
    // the second snapshot variant carries the binding the first lacks
    assert_eq!(
        peer.target
            .map(|t| bun_id(&g.packages[t.index()]))
            .as_deref(),
        Some("peer@file:vendor/peer")
    );
    assert_eq!(
        g.packages.iter().filter(|p| p.name == "has-peer").count(),
        1
    );

    let m = fixture("v9-peer-range-dedupe");
    let g = &m.graph;
    let peer_deps = pkg(g, "peer-deps@1.0.0");
    let e = peer_deps.dependency("no-deps").unwrap();
    assert_eq!(e.literal, "*");
    // bound to the highest version, like bun.lock's loader does
    assert_eq!(
        e.target.map(|t| bun_id(&g.packages[t.index()])).as_deref(),
        Some("no-deps@1.0.1")
    );
}

#[test]
fn importer_peers_come_from_package_json() {
    let m = fixture("v9-importer-peers");
    let g = &m.graph;
    assert!(
        !m.warnings.iter().any(|w| w.contains("skipping peer")),
        "{:?}",
        m.warnings
    );
    let root: Vec<(String, String, bool, bool)> = g
        .root()
        .dependencies
        .iter()
        .map(|e| {
            (
                e.name.clone(),
                e.literal.clone(),
                e.behavior.is_peer(),
                e.behavior.is_optional_peer(),
            )
        })
        .collect();
    assert_eq!(
        root,
        [
            ("@types/is-number".into(), "*".into(), true, true),
            ("@types/no-deps".into(), "*".into(), true, true),
            ("no-deps".into(), "^1.0.0".into(), false, false),
            ("no-deps".into(), "^1.0.0".into(), true, false),
            ("peer-deps-fixed".into(), "^1.0.0".into(), true, false),
        ]
    );
    let a = &g.packages[g.workspaces["packages/a"].index()];
    let edges: Vec<(&str, &str, bool)> = a
        .dependencies
        .iter()
        .map(|e| (e.name.as_str(), e.literal.as_str(), e.behavior.is_peer()))
        .collect();
    assert_eq!(edges, [("no-deps", "^1.0.0", true)]);
}

#[test]
fn link_with_a_semver_specifier_resolves_to_the_workspace() {
    let m = fixture("v9-link-semver-specifier");
    let g = &m.graph;
    assert!(has(g, "common@workspace:shared/common"));
    assert!(!ids(g).iter().any(|i| i.contains("@link:")));
    let web = &g.packages[g.workspaces["apps/web"].index()];
    let e = web.dependency("common").unwrap();
    assert_eq!(e.target, Some(g.workspaces["shared/common"]));
    assert!(e.behavior.is_workspace());
}

#[test]
fn root_package_link_resolution() {
    let m = fixture("root-package-link-resolution");
    m.graph.validate().unwrap();
    for p in &m.graph.packages {
        for e in &p.dependencies {
            if let Some(t) = e.target {
                assert!(t.index() < m.graph.len());
            }
        }
    }
}

#[test]
fn multi_line_resolution_mappings_migrate_identically() {
    let src = fixtures().join("pnpm/v9-git-references");
    let plain = run(&src).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(src.join("package.json"), dir.path().join("package.json")).unwrap();
    let text = std::fs::read_to_string(src.join("pnpm-lock.yaml")).unwrap();
    let one_line = "    resolution: {tarball: https://token@tarballs.example.com/priv-1.0.0.tgz}\n";
    assert!(text.contains(one_line));
    let formatted = text.replace(one_line, "    resolution:\n      {\n        tarball: https://token@tarballs.example.com/priv-1.0.0.tgz,\n      }\n");
    std::fs::write(dir.path().join("pnpm-lock.yaml"), formatted).unwrap();
    let again = run(dir.path()).unwrap();
    assert_eq!(ids(&again.graph), ids(&plain.graph));
}

#[test]
fn workspace_yaml_and_pnpm_block_settings_move_to_package_json() {
    let lock = "lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

catalogs:
  default:
    no-deps:
      specifier: ^1.0.0
      version: 1.0.1

overrides:
  one-dep>no-deps: 1.0.0
  removed: '-'

importers:

  .:
    dependencies:
      no-deps:
        specifier: 'catalog:'
        version: 1.0.1
      lib:
        specifier: workspace:*
        version: link:packages/lib
      linked:
        specifier: link:vendor/linked
        version: link:vendor/linked

  packages/lib:
    dependencies:
      no-deps:
        specifier: 'catalog:'
        version: 1.0.1

packages:

  no-deps@1.0.1:
    resolution: {integrity: sha512-3X6cn4+UJdXJuLPu11v8i/fGLe2PdI6v1yKTELam04lY5esCAFdG/qQts6N6rLrL6g1YRq+MKBAwxbmUQk355A==}

snapshots:

  no-deps@1.0.1: {}
";
    let workspace_yaml = "packages:
  - packages/*
catalog:
  no-deps: ^1.0.0
  unused: 2.10
catalogs:
  react17:
    react: ^17.0.2
overrides:
  foo: 1.0.0
onlyBuiltDependencies:
  - esbuild
  - sharp
packageExtensions:
  'a@1':
    peerDependenciesMeta:
      b:
        optional: true
";
    let root_json = json!({
        "name": "pnpm-settings",
        "dependencies": { "no-deps": "catalog:", "lib": "workspace:*", "linked": "link:vendor/linked" },
        "pnpm": { "overrides": { "bar": "2.0.0" }, "onlyBuiltDependencies": ["esbuild", "core-js"] },
        "trustedDependencies": ["already"]
    });
    let dir = project(&[
        ("package.json", root_json.to_string()),
        (
            "packages/lib/package.json",
            json!({ "name": "lib", "version": "1.0.0", "dependencies": { "no-deps": "catalog:" } })
                .to_string(),
        ),
        (
            "vendor/linked/package.json",
            json!({ "name": "linked", "version": "1.0.0" }).to_string(),
        ),
        ("pnpm-workspace.yaml", workspace_yaml.into()),
        ("pnpm-lock.yaml", lock.into()),
    ]);
    let m = run(dir.path()).unwrap();
    let after = root_edit(&m, root_json.clone());
    assert_eq!(
        after["workspaces"],
        json!({ "packages": ["packages/*"], "catalog": { "no-deps": "^1.0.0", "unused": "2.10" }, "catalogs": { "react17": { "react": "^17.0.2" } } })
    );
    assert_eq!(
        after["overrides"],
        json!({ "bar": "2.0.0", "foo": "1.0.0" })
    );
    assert_eq!(
        after["trustedDependencies"],
        json!(["already", "esbuild", "core-js", "sharp"])
    );
    assert_eq!(
        after["packageExtensions"],
        json!({ "a@1": { "peerDependenciesMeta": { "b": { "optional": true } } } })
    );
    assert_eq!(
        m.write_back[0].changes,
        [
            "copied pnpm.overrides to overrides",
            "copied pnpm-workspace.yaml to workspaces",
            "copied pnpm-workspace.yaml overrides to overrides",
            "copied pnpm-workspace.yaml packageExtensions to packageExtensions",
            "copied pnpm.onlyBuiltDependencies to trustedDependencies",
            "copied pnpm-workspace.yaml onlyBuiltDependencies to trustedDependencies",
        ]
    );
    // the fields set are only the changed ones
    let keys: Vec<&str> = m.write_back[0]
        .fields
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(
        keys,
        [
            "trustedDependencies",
            "overrides",
            "workspaces",
            "packageExtensions"
        ]
    );

    let g = &m.graph;
    // lockfile overrides (the `-` removal is skipped) with their selector
    assert_eq!(g.overrides.len(), 1);
    assert_eq!(
        g.overrides[0].selector.parent.as_ref().unwrap().name,
        "one-dep"
    );
    // catalogs: the lockfile's, plus what pnpm-workspace.yaml declares
    assert_eq!(g.catalogs.get("", "no-deps"), Some("^1.0.0"));
    assert_eq!(g.catalogs.get("", "unused"), Some("2.10"));
    assert_eq!(g.catalogs.get("react17", "react"), Some("^17.0.2"));
    let trusted: Vec<&str> = g
        .trusted_dependencies
        .as_ref()
        .unwrap()
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(trusted, ["already", "core-js", "esbuild", "sharp"]);
    assert!(g.package_extensions["a@1"].peer_dependencies_meta["b"].optional);
    // workspace and link edges
    let root = g.root();
    let lib = root.dependency("lib").unwrap();
    assert_eq!(lib.target, Some(g.workspaces["packages/lib"]));
    assert!(lib.behavior.is_workspace());
    assert_eq!(
        target(g, root, "linked").resolution,
        Resolution::Symlink {
            path: "./vendor/linked".into()
        }
    );
    assert_eq!(root.dependency("no-deps").unwrap().literal, "catalog:");
    assert_eq!(bun_id(target(g, root, "no-deps")), "no-deps@1.0.1");
}

#[test]
fn registry_qualified_versions() {
    let lock = "lockfileVersion: '9.0'

importers:

  .:
    dependencies:
      no-deps:
        specifier: ^1.0.0
        version: work:1.0.1

packages:

  no-deps@work:1.0.1:
    resolution: {integrity: sha512-3X6cn4+UJdXJuLPu11v8i/fGLe2PdI6v1yKTELam04lY5esCAFdG/qQts6N6rLrL6g1YRq+MKBAwxbmUQk355A==}

snapshots:

  no-deps@work:1.0.1: {}
";
    let dir = project(&[
        (
            "package.json",
            json!({ "name": "named", "dependencies": { "no-deps": "^1.0.0" } }).to_string(),
        ),
        (
            "pnpm-workspace.yaml",
            "namedRegistries:\n  work: https://user:pw@npm.work.example/\n".into(),
        ),
        ("pnpm-lock.yaml", lock.into()),
    ]);
    let m = run(dir.path()).unwrap();
    let p = target(&m.graph, m.graph.root(), "no-deps");
    assert!(
        matches!(&p.resolution, Resolution::Npm { version, tarball_url: Some(u), .. }
        if version == "1.0.1" && u == "https://npm.work.example/no-deps/-/no-deps-1.0.1.tgz")
    );
    assert!(m.warnings.iter().any(|w| {
        w.contains("fetching pnpm registry \"work\" packages from https://npm.work.example/")
    }));
}

#[test]
fn version_is_a_number_and_too_old_versions() {
    for (version, ok) in [
        ("9.0", true),
        ("'9.0'", true),
        ("7", true),
        ("6.0", false),
        ("'5.4'", false),
        ("10.0", true),
    ] {
        let lock = format!("lockfileVersion: {version}\n\nimporters:\n\n  .: {{}}\n");
        let dir = project(&[("package.json", "{}".into()), ("pnpm-lock.yaml", lock)]);
        let result = run(dir.path());
        assert_eq!(result.is_ok(), ok, "{version}: {result:?}");
        if version == "10.0" {
            assert!(result.unwrap().warnings[0].contains("migrating it as 9.0"));
        }
    }
}

// -- export ------------------------------------------------------------------------

/// `(id, integrity, (edge name, literal, target id))` of one package.
type PackageView = (
    String,
    Option<String>,
    Vec<(String, String, Option<String>)>,
);

fn round_trip(dir: &Path) {
    let m = run(dir).unwrap();
    let text = pnpm::serialize(&m.graph).unwrap();
    assert!(text.starts_with("lockfileVersion: '9.0'\n"), "{text}");
    let out = tempfile::tempdir().unwrap();
    std::fs::copy(dir.join("package.json"), out.path().join("package.json")).unwrap();
    for m in workspace(dir).members {
        let to = out.path().join(&m.path);
        std::fs::create_dir_all(&to).unwrap();
        std::fs::copy(
            dir.join(&m.path).join("package.json"),
            to.join("package.json"),
        )
        .unwrap();
    }
    std::fs::write(out.path().join("pnpm-lock.yaml"), &text).unwrap();
    if dir.join("pnpm-workspace.yaml").exists() {
        std::fs::copy(
            dir.join("pnpm-workspace.yaml"),
            out.path().join("pnpm-workspace.yaml"),
        )
        .unwrap();
    }
    let again = run(out.path()).unwrap_or_else(|e| panic!("{}: {e}\n{text}", dir.display()));
    let view = |g: &LockGraph| {
        let mut v: Vec<PackageView> = g
            .packages
            .iter()
            .map(|p| {
                let mut deps: Vec<(String, String, Option<String>)> = p
                    .dependencies
                    .iter()
                    .map(|e| {
                        (
                            e.name.clone(),
                            e.literal.clone(),
                            e.target.map(|t| bun_id(&g.packages[t.index()])),
                        )
                    })
                    .collect();
                deps.sort();
                (
                    bun_id(p),
                    p.resolution.integrity().map(|i| i.to_sri()),
                    deps,
                )
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        view(&again.graph),
        view(&m.graph),
        "{}\n{text}",
        dir.display()
    );
    assert_eq!(pnpm::serialize(&again.graph).unwrap(), text, "stable");
}

#[test]
fn export_round_trips_fixtures() {
    for name in [
        "basic",
        "v9-reference-shapes",
        "v9-peer-variant-merge",
        "v9-peer-range-dedupe",
        "v9-file-directory",
        "v9-local-tarballs",
        "v9-alias-in-optional-dependencies",
        "v9-link-semver-specifier",
        "v9-catalog-default",
    ] {
        round_trip(&fixtures().join("pnpm").join(name));
    }
}

#[test]
fn export_format() {
    let m = fixture("basic");
    let text = pnpm::serialize(&m.graph).unwrap();
    let expected = "lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      no-deps:
        specifier: ~1.0.0
        version: 1.0.1
    devDependencies:
      a-dep-b:
        specifier: 1.0.0
        version: 1.0.0
    optionalDependencies:
      b-dep-a:
        specifier: 1.0.0
        version: 1.0.0

packages:

  a-dep-b@1.0.0:
    resolution: {integrity: sha512-PW1l4ruYaxcIw4rMkOVzb9zcR2srZhTPv2H2aH7QFc7vVxkD7EEMGHg1GPT8ycLFb8vriydUXEPwOy1FcbodaQ==}

  a-dep@1.0.1:
    resolution: {integrity: sha512-6nmTaPgO2U/uOODqOhbjbnaB4xHuZ+UB7AjKUA3g2dT4WRWeNxgp0dC8Db4swXSnO5/uLLUdFmUJKINNBO/3wg==}

  b-dep-a@1.0.0:
    resolution: {integrity: sha512-1owp4Wy5QE893BGgjDQGZm9Oayk38MA++fXmPTQA1WY/NFQv7CcCVpK2Ht/4mU4KejDeHOxaAj7qbzv1dSQA2w==}

  no-deps@1.0.1:
    resolution: {integrity: sha512-3X6cn4+UJdXJuLPu11v8i/fGLe2PdI6v1yKTELam04lY5esCAFdG/qQts6N6rLrL6g1YRq+MKBAwxbmUQk355A==}

snapshots:

  a-dep-b@1.0.0:
    dependencies:
      b-dep-a: 1.0.0

  a-dep@1.0.1: {}

  b-dep-a@1.0.0:
    dependencies:
      a-dep-b: 1.0.0

  no-deps@1.0.1: {}
";
    assert_eq!(text, expected);
}

fn workspace_config(
    dir: &Path,
) -> (
    Option<fnpm_lockfile::formats::foreign::PackageJsonWriteBack>,
    Vec<String>,
) {
    let ws = workspace(dir);
    let files = ScopedFiles(dir.to_path_buf());
    pnpm::migrate_workspace_config(&ws, &options(&files))
}

/// bun `migrate_pnpm_workspace_config`: no pnpm-lock.yaml, but pnpm-workspace.yaml settings
/// still move into package.json.
#[test]
fn workspace_config_without_lockfile() {
    let root_json = json!({
        "name": "root",
        "pnpm": { "overrides": { "foo": "1.0.0" }, "onlyBuiltDependencies": ["esbuild"] }
    });
    let dir = project(&[
        ("package.json", root_json.to_string()),
        (
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\ncatalog:\n  react: ^18.0.0\nonlyBuiltDependencies:\n  - sharp\n".into(),
        ),
    ]);
    let (edit, warnings) = workspace_config(dir.path());
    assert!(warnings.is_empty(), "{warnings:?}");
    let edit = edit.expect("an edit");
    assert_eq!(edit.path, "");
    let mut after = root_json.clone();
    edit.apply_to(&mut after);
    assert_eq!(
        after["workspaces"],
        json!({ "packages": ["packages/*"], "catalog": { "react": "^18.0.0" } })
    );
    assert_eq!(after["overrides"], json!({ "foo": "1.0.0" }));
    assert_eq!(after["trustedDependencies"], json!(["esbuild", "sharp"]));
    assert_eq!(
        edit.changes,
        [
            "copied pnpm.overrides to overrides",
            "copied pnpm-workspace.yaml to workspaces",
            "copied pnpm.onlyBuiltDependencies to trustedDependencies",
            "copied pnpm-workspace.yaml onlyBuiltDependencies to trustedDependencies",
        ]
    );
}

#[test]
fn workspace_config_skipped_when_already_migrated_or_absent() {
    // a root with `workspaces` was migrated already
    let dir = project(&[
        (
            "package.json",
            json!({ "name": "root", "workspaces": ["a"] }).to_string(),
        ),
        ("pnpm-workspace.yaml", "packages:\n  - packages/*\n".into()),
    ]);
    assert!(workspace_config(dir.path()).0.is_none());
    // no pnpm-workspace.yaml: `package.json#pnpm` is left alone, as bun does
    let dir = project(&[(
        "package.json",
        json!({ "name": "root", "pnpm": { "overrides": { "foo": "1" } } }).to_string(),
    )]);
    assert!(workspace_config(dir.path()).0.is_none());
    // a broken pnpm-workspace.yaml warns and still copies `package.json#pnpm`
    let dir = project(&[
        (
            "package.json",
            json!({ "name": "root", "pnpm": { "overrides": { "foo": "1" } } }).to_string(),
        ),
        ("pnpm-workspace.yaml", "packages: [\n".into()),
    ]);
    let (edit, warnings) = workspace_config(dir.path());
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        edit.unwrap().changes,
        ["copied pnpm.overrides to overrides"]
    );
}
