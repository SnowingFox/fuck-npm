//! `workspace:`, `link-workspace-packages`, `file:`, `link:`, tarballs, git and GitHub. Ported
//! from `bun-workspaces.test.ts`, `bun-install-link-path.test.ts`, `bun-link.test.ts`,
//! `bun-install-git-deps.test.ts` and the tarball cases of `bun-install.test.ts`.

mod common;

use common::*;
use fnpm_resolver::{Error, Mode, Resolved};
use fnpm_types::{PackageId, Resolution};
use serde_json::json;

fn members() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "packages/a",
            json!({"name": "a", "version": "1.0.0", "dependencies": {"b": "workspace:*"}}),
        ),
        ("packages/b", json!({"name": "b", "version": "2.3.0"})),
    ]
}

#[tokio::test]
async fn workspace_protocol_links_members() {
    let h = Harness::new();
    let ws = workspace(
        json!({"name": "root", "dependencies": {"a": "workspace:^", "b": "workspace:~"}}),
        &members(),
    );
    let g = h.install(&ws).await.graph;
    assert_eq!(root_dep(&g, "a").as_deref(), Some("workspace:packages/a"));
    let a = g.workspaces["packages/a"];
    assert_eq!(
        dep(&g, a, "b").unwrap().resolution,
        Resolution::Workspace {
            path: "packages/b".into()
        }
    );
    let edge = g.root().dependency("a").unwrap();
    assert!(edge.behavior.is_workspace());
    assert!(keys(&g).iter().all(|k| k.contains("workspace:")));
}

#[tokio::test]
async fn workspace_ranges_must_match() {
    let h = Harness::new();
    let ws = workspace(
        json!({"dependencies": {"b": "workspace:^2.0.0"}}),
        &members(),
    );
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "b").as_deref(),
        Some("workspace:packages/b")
    );
    let ws = workspace(
        json!({"dependencies": {"b": "workspace:^3.0.0"}}),
        &members(),
    );
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("No matching version for workspace dependency \"b\""),
        "{err}"
    );
}

#[tokio::test]
async fn workspace_alias_path_and_root_self_reference() {
    let h = Harness::new();
    let ws = workspace(
        json!({"name": "root", "version": "1.0.0", "dependencies": {"alias": "workspace:b@^2", "p": "workspace:./packages/a", "me": "workspace:*"}}),
        &members(),
    );
    let ws = {
        let mut ws = ws;
        // `me` names the root itself (bun links `.`).
        ws.root.dependencies.retain(|(n, _)| n != "me");
        ws.root
            .dependencies
            .push(("root".into(), "workspace:*".into()));
        ws
    };
    let g = h.install(&ws).await.graph;
    assert_eq!(dep(&g, PackageId::ROOT, "alias").unwrap().name, "b");
    assert_eq!(dep(&g, PackageId::ROOT, "p").unwrap().name, "a");
    assert_eq!(
        g.root().dependency("root").unwrap().target,
        Some(PackageId::ROOT)
    );
}

#[tokio::test]
async fn missing_workspace_is_an_error() {
    let h = Harness::new();
    let ws = workspace(json!({"dependencies": {"nope": "workspace:*"}}), &members());
    assert!(
        matches!(h.run(&ws, None, Mode::Normal).await, Err(Error::WorkspaceNotFound { name }) if name == "nope")
    );
}

#[tokio::test]
async fn link_workspace_packages_links_satisfying_ranges() {
    let h = Harness::new();
    h.registry
        .add("b", &[("2.3.0", no_deps()), ("3.0.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"b": "^2"}}), &members());
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "b").as_deref(),
        Some("workspace:packages/b")
    );
    // Outside the member's version the registry is used.
    let ws = workspace(json!({"dependencies": {"b": "^3"}}), &members());
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "b").as_deref(),
        Some("3.0.0")
    );
    // Turned off, the registry is used even when the range matches.
    let mut h = Harness::new();
    h.config.link_workspace_packages = false;
    h.registry.add("b", &[("2.3.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"b": "^2"}}), &members());
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "b").as_deref(),
        Some("2.3.0")
    );
}

#[tokio::test]
async fn a_registry_package_depending_on_a_member_links_it() {
    let h = Harness::new();
    h.registry
        .add("x", &[("1.0.0", json!({"dependencies": {"b": "^2.0.0"}}))]);
    let ws = workspace(json!({"dependencies": {"x": "1.0.0"}}), &members());
    let g = h.install(&ws).await.graph;
    let x = id_of(&g, "x", "1.0.0");
    assert_eq!(
        dep(&g, x, "b").unwrap().resolution,
        Resolution::Workspace {
            path: "packages/b".into()
        }
    );
}

#[tokio::test]
async fn same_name_dist_tag_falls_back_to_the_workspace() {
    let h = Harness::new();
    let ws = workspace(json!({"dependencies": {"b": "latest"}}), &members());
    // The registry has no `b`: the workspace is used.
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "b").as_deref(),
        Some("workspace:packages/b")
    );
}

#[tokio::test]
async fn file_folders_are_read_and_their_dependencies_resolved() {
    let h = Harness::new();
    h.fetcher.folder("/project/vendor/lib", json!({"name": "lib", "version": "0.1.0", "dependencies": {"dep": "^1", "nested": "file:../nested"}}));
    h.fetcher.folder(
        "/project/vendor/nested",
        json!({"name": "nested", "version": "0.0.1", "dependencies": {"never": "^1"}}),
    );
    h.registry.add("dep", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"lib": "file:./vendor/lib", "compat": "file://../vendor/lib"}}),
        &[],
    );
    let ws = {
        let mut ws = ws;
        ws.root.dependencies.retain(|(n, _)| n != "compat");
        ws
    };
    let g = h.install(&ws).await.graph;
    let lib = dep(&g, PackageId::ROOT, "lib").unwrap();
    assert_eq!(
        lib.resolution,
        Resolution::Folder {
            path: "vendor/lib".into()
        }
    );
    let lib_id = id_of(&g, "lib", "0.1.0");
    assert_eq!(dep(&g, lib_id, "dep").unwrap().version, "1.0.0");
    assert_eq!(
        dep(&g, lib_id, "nested").unwrap().resolution,
        Resolution::Folder {
            path: "vendor/nested".into()
        }
    );
    // A folder declared by a folder is not read: its dependencies are not resolved (bun).
    assert!(dep(&g, lib_id, "nested").unwrap().dependencies.is_empty());
    assert_eq!(h.registry.request_count("never"), 0);
}

#[tokio::test]
async fn file_without_package_json_is_an_error() {
    let h = Harness::new();
    let ws = workspace(json!({"dependencies": {"z": "file:./missing"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "package.json not found for \"file:./missing\" dependency \"z\""
    );
}

#[tokio::test]
async fn workspace_wins_over_a_same_name_folder_in_the_root() {
    let h = Harness::new();
    h.fetcher.folder(
        "/project/elsewhere",
        json!({"name": "b", "version": "9.9.9"}),
    );
    let ws = workspace(
        json!({"dependencies": {"b": "file:./elsewhere"}}),
        &members(),
    );
    assert_eq!(
        root_dep(&h.install(&ws).await.graph, "b").as_deref(),
        Some("workspace:packages/b")
    );
}

#[tokio::test]
async fn folders_of_published_packages_are_not_read() {
    let h = Harness::new();
    h.registry.add(
        "pub",
        &[(
            "1.0.0",
            json!({"dependencies": {"inner": "file:./inner", "escape": "file:../../etc"}}),
        )],
    );
    let ws = workspace(json!({"dependencies": {"pub": "1.0.0"}}), &[]);
    // An escaping path of a published package is refused; an inner one is kept as written.
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(err.to_string().contains("file:../../etc"), "{err}");
    h.registry.add(
        "pub2",
        &[("1.0.0", json!({"dependencies": {"inner": "file:./inner"}}))],
    );
    let ws = workspace(json!({"dependencies": {"pub2": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    let p = id_of(&g, "pub2", "1.0.0");
    assert_eq!(
        dep(&g, p, "inner").unwrap().resolution,
        Resolution::Folder {
            path: "./inner".into()
        }
    );
}

#[tokio::test]
async fn link_paths_and_registrations() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("app");
    std::fs::create_dir_all(root.join("libs/l")).unwrap();
    let mut h = Harness::new();
    h.config.global_dir = dir.path().join("global");
    std::fs::create_dir_all(h.config.global_dir.join("node_modules/registered")).unwrap();
    h.fetcher.folder(
        root.join("libs/l"),
        json!({"name": "l", "version": "1.0.0", "dependencies": {"never": "^1"}}),
    );
    let ws = workspace_at(
        root.clone(),
        json!({"dependencies": {"l": "link:libs/l", "registered": "link:registered"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    let l = dep(&g, PackageId::ROOT, "l").unwrap();
    assert_eq!(
        l.resolution,
        Resolution::Symlink {
            path: "./libs/l".into()
        }
    );
    // A symlinked package's dependencies are not installed.
    assert!(l.dependencies.is_empty());
    assert_eq!(
        dep(&g, PackageId::ROOT, "registered").unwrap().resolution,
        Resolution::Symlink {
            path: "registered".into()
        }
    );

    let ws = workspace_at(
        root.clone(),
        json!({"dependencies": {"x": "link:nope"}}),
        &[],
    );
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(
        err.to_string().contains("Package \"x\" is not linked"),
        "{err}"
    );
    let ws = workspace_at(root, json!({"dependencies": {"x": "link:./nowhere"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(
        err.to_string().contains("not found for linked dependency"),
        "{err}"
    );
}

#[tokio::test]
async fn published_packages_may_not_link_to_paths() {
    let h = Harness::new();
    h.registry.add(
        "evil",
        &[(
            "1.0.0",
            json!({"dependencies": {"x": "link:../../secrets"}}),
        )],
    );
    let ws = workspace(json!({"dependencies": {"evil": "1.0.0"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(
        err.to_string().contains(
            "only the root package.json, a workspace, or a top-level override may link to a path"
        ),
        "{err}"
    );
}

#[tokio::test]
async fn remote_tarballs_are_fetched_for_their_package_json() {
    let h = Harness::new();
    h.fetcher.package(
        "https://cdn.test/t-1.0.0.tgz",
        json!({"name": "t", "version": "1.0.0", "dependencies": {"dep": "^1"}}),
    );
    h.registry.add("dep", &[("1.0.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"t": "https://cdn.test/t-1.0.0.tgz"}}),
        &[],
    );
    let (out, events) = h
        .run_full(&ws, None, Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    let t = dep(&out.graph, PackageId::ROOT, "t").unwrap();
    assert!(
        matches!(&t.resolution, Resolution::RemoteTarball { url, integrity: Some(_) } if url == "https://cdn.test/t-1.0.0.tgz")
    );
    let t_id = id_of(&out.graph, "t", "1.0.0");
    assert_eq!(dep(&out.graph, t_id, "dep").unwrap().version, "1.0.0");
    assert!(
        events
            .iter()
            .any(|Resolved::Package { name, .. }| name == "t")
    );
}

#[tokio::test]
async fn local_tarballs_relative_to_the_project() {
    let h = Harness::new();
    h.fetcher.package(
        "/project/pkgs/x.tgz",
        json!({"name": "x", "version": "0.2.0"}),
    );
    let ws = workspace(json!({"dependencies": {"x": "./pkgs/x.tgz"}}), &[]);
    let g = h.install(&ws).await.graph;
    assert!(
        matches!(&dep(&g, PackageId::ROOT, "x").unwrap().resolution, Resolution::LocalTarball { path, .. } if path == "./pkgs/x.tgz")
    );
    // A published package may not name a local tarball.
    h.registry.add(
        "pub",
        &[("1.0.0", json!({"dependencies": {"y": "file:y.tgz"}}))],
    );
    let ws = workspace(json!({"dependencies": {"pub": "1.0.0"}}), &[]);
    let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("local tarballs are only allowed in this project's package.json files"),
        "{err}"
    );
}

#[tokio::test]
async fn git_dependencies_resolve_commits() {
    let h = Harness::new();
    let sha = "a".repeat(40);
    h.fetcher
        .commit("ssh://git@host.test/org/repo.git", "v1", &sha);
    h.fetcher.package(
        &format!("git:ssh://git@host.test/org/repo.git#{sha}"),
        json!({"name": "repo", "version": "1.0.0"}),
    );
    let ws = workspace(
        json!({"dependencies": {"r": "git+ssh://git@host.test/org/repo.git#v1"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    match &dep(&g, PackageId::ROOT, "r").unwrap().resolution {
        Resolution::Git {
            url,
            committish,
            resolved_commit,
            ..
        } => {
            assert_eq!(
                (url.as_str(), committish.as_str(), resolved_commit.as_str()),
                ("ssh://git@host.test/org/repo.git", "v1", sha.as_str())
            );
        }
        r => panic!("{r:?}"),
    }
}

#[tokio::test]
async fn a_remote_answering_with_something_other_than_a_commit_is_an_error() {
    let h = Harness::new();
    h.fetcher
        .commit("https://git.test/r.git", "", "../../../escape")
        .commit("github:owner/repo", "main", "../escape");
    // A git remote must answer with a commit; a GitHub archive is named by its root directory
    // (bun), which must be a name a folder may have.
    for (spec, refusal) in [
        ("git+https://git.test/r.git", "is not a commit"),
        ("github:owner/repo#main", "is not a valid folder name"),
    ] {
        let ws = workspace(json!({"dependencies": {"r": spec}}), &[]);
        let err = h.run(&ws, None, Mode::Normal).await.unwrap_err();
        assert!(err.to_string().contains(refusal), "{spec}: {err}");
    }
    assert!(h.fetcher.fetches.lock().is_empty());
}

#[tokio::test]
async fn github_dependencies_resolve_commits() {
    let h = Harness::new();
    let sha = "b".repeat(40);
    h.fetcher.commit("github:owner/repo", "main", &sha);
    h.fetcher.package(
        &format!("github:owner/repo#{sha}"),
        json!({"name": "repo", "version": "3.0.0"}),
    );
    let ws = workspace(
        json!({"dependencies": {"repo": "owner/repo#main", "again": "github:owner/repo#main"}}),
        &[],
    );
    let (out, _) = h
        .run_full(&ws, None, Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    let g = &out.graph;
    assert!(
        matches!(&dep(g, PackageId::ROOT, "repo").unwrap().resolution, Resolution::Github { resolved_commit, .. } if *resolved_commit == sha)
    );
    // Both edges share one package and one fetch.
    assert_eq!(g.packages.iter().filter(|p| p.name == "repo").count(), 1);
    assert_eq!(h.fetcher.fetches.lock().len(), 1);
}

#[tokio::test]
async fn failed_remote_fetches() {
    let h = Harness::new();
    let ws = workspace(
        json!({"dependencies": {"x": "https://cdn.test/missing.tgz"}}),
        &[],
    );
    assert!(h.run(&ws, None, Mode::Normal).await.is_err());
    let ws = workspace(
        json!({"optionalDependencies": {"x": "https://cdn.test/missing.tgz"}}),
        &[],
    );
    let out = h.install(&ws).await;
    assert!(out.graph.root().dependency("x").unwrap().target.is_none());
}

#[tokio::test]
async fn remote_packages_do_not_block_the_queue() {
    let h = Harness::new();
    h.fetcher.package(
        "https://cdn.test/r.tgz",
        json!({"name": "r", "version": "1.0.0", "dependencies": {"shared": "^1"}}),
    );
    h.registry
        .add(
            "a",
            &[("1.0.0", json!({"dependencies": {"shared": "^1.0.0"}}))],
        )
        .add("shared", &[("1.0.0", no_deps()), ("1.1.0", no_deps())]);
    let ws = workspace(
        json!({"dependencies": {"r": "https://cdn.test/r.tgz", "a": "1"}}),
        &[],
    );
    let g = h.install(&ws).await.graph;
    assert_eq!(versions(&g, "shared"), ["1.1.0"]);
}
