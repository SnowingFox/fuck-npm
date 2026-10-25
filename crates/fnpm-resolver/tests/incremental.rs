//! Reusing a previous graph: the no-op path, `--frozen-lockfile`, partial re-resolution,
//! `fnpm update` (names, `--latest`, held-back rules), dedupe and look-ahead hints. Ported from
//! `bun-install.test.ts` (lockfile reuse), `bun-update.test.ts`, `bun-update-transitive.test.ts`,
//! `bun-dedupe.test.ts` and the frozen-lockfile cases.

mod common;

use common::*;
use fnpm_resolver::{Error, Mode, dedupe};
use fnpm_types::{DependencyGroup, PackageId, Resolution};
use serde_json::json;

fn registry(h: &Harness) {
    h.registry
        .add(
            "a",
            &[
                ("1.0.0", json!({"dependencies": {"c": "^1"}})),
                ("1.1.0", json!({"dependencies": {"c": "^1"}})),
                ("2.0.0", no_deps()),
            ],
        )
        .add("b", &[("1.0.0", json!({"dependencies": {"c": "^1.0.0"}}))])
        .add("c", &[("1.0.0", no_deps()), ("1.1.0", no_deps())]);
}

#[tokio::test]
async fn an_unchanged_project_needs_no_manifests() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(json!({"dependencies": {"a": "^1", "b": "1.0.0"}}), &[]);
    let first = h.install(&ws).await;
    let before = h.registry.requests().len();
    let (again, events) = h
        .run_full(&ws, Some(&first.graph), Mode::Normal, vec![], vec![])
        .await
        .unwrap();
    assert!(!again.changed);
    assert_eq!(again.graph, first.graph);
    assert_eq!(h.registry.requests().len(), before);
    // Every storable package is still streamed for the fetcher.
    assert_eq!(events.len(), 3);
}

#[tokio::test]
async fn frozen_installs_report_every_change() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(
        json!({"dependencies": {"a": "^1", "b": "1.0.0"}}),
        &[("packages/w", json!({"name": "w", "version": "1.0.0"}))],
    );
    let first = h.install(&ws).await.graph;
    assert!(
        !h.run(&ws, Some(&first), Mode::Frozen)
            .await
            .unwrap()
            .changed
    );
    let ws2 = workspace(
        json!({"dependencies": {"a": "^1.1", "c": "1"}, "devDependencies": {"b": "1.0.0"}, "overrides": {"c": "1.0.0"}}),
        &[("packages/w", json!({"name": "w", "version": "1.0.1"}))],
    );
    match h.run(&ws2, Some(&first), Mode::Frozen).await {
        Err(Error::FrozenLockfile { changes }) => assert_eq!(
            changes,
            [
                "a changed from ^1 to ^1.1",
                "b is in different dependency groups",
                "c was added",
                "version of workspace packages/w changed from 1.0.0 to 1.0.1",
                "overrides changed",
            ]
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        h.run(&ws, None, Mode::Frozen).await,
        Err(Error::FrozenLockfile { .. })
    ));
    let err = h.run(&ws, Some(&first), Mode::Frozen).await;
    assert!(err.is_ok());
    let (_, _) = (DependencyGroup::Prod, PackageId::ROOT);
}

#[tokio::test]
async fn locked_versions_are_kept_while_they_satisfy() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(json!({"dependencies": {"a": "^1"}}), &[]);
    let first = h.install(&ws).await.graph;
    assert_eq!(root_dep(&first, "a").as_deref(), Some("1.1.0"));
    // The registry gets newer versions; an unrelated addition must not move `a` or `c`.
    h.registry.add(
        "a",
        &[
            ("1.0.0", json!({"dependencies": {"c": "^1"}})),
            ("1.1.0", json!({"dependencies": {"c": "^1"}})),
            ("1.9.0", no_deps()),
        ],
    );
    h.registry.add(
        "c",
        &[
            ("1.0.0", no_deps()),
            ("1.1.0", no_deps()),
            ("1.5.0", no_deps()),
        ],
    );
    h.registry
        .add("new", &[("1.0.0", json!({"dependencies": {"c": "^1"}}))]);
    let before = h.registry.requests();
    let ws = workspace(json!({"dependencies": {"a": "^1", "new": "1"}}), &[]);
    let out = h.run(&ws, Some(&first), Mode::Normal).await.unwrap();
    assert!(out.changed);
    assert_eq!(root_dep(&out.graph, "a").as_deref(), Some("1.1.0"));
    // `new`'s `c@^1` shares the locked 1.1.0 instead of the newer 1.5.0.
    assert_eq!(versions(&out.graph, "c"), ["1.1.0"]);
    let after = h.registry.requests();
    let new_requests: Vec<_> = after[before.len()..].to_vec();
    assert_eq!(new_requests, ["new"]);
}

#[tokio::test]
async fn a_changed_range_is_resolved_again() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(json!({"dependencies": {"a": "^1"}}), &[]);
    let first = h.install(&ws).await.graph;
    let ws = workspace(json!({"dependencies": {"a": "^2"}}), &[]);
    let out = h.run(&ws, Some(&first), Mode::Normal).await.unwrap();
    assert_eq!(root_dep(&out.graph, "a").as_deref(), Some("2.0.0"));
    // `c` was only needed by a@1: dropped.
    assert_eq!(keys(&out.graph), ["a@2.0.0"]);
}

#[tokio::test]
async fn removals_drop_unreachable_packages() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(json!({"dependencies": {"a": "^1", "b": "1.0.0"}}), &[]);
    let first = h.install(&ws).await.graph;
    let (out, _) = h
        .run_full(
            &ws,
            Some(&first),
            Mode::Normal,
            vec![],
            vec![("".into(), "a".into())],
        )
        .await
        .unwrap();
    assert_eq!(keys(&out.graph), ["b@1.0.0", "c@1.1.0"]);
}

#[tokio::test]
async fn update_moves_within_ranges() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(json!({"dependencies": {"a": "^1", "b": "1.0.0"}}), &[]);
    let first = h.install(&ws).await.graph;
    h.registry.add(
        "a",
        &[
            ("1.0.0", no_deps()),
            ("1.1.0", json!({"dependencies": {"c": "^1"}})),
            ("1.2.0", json!({"dependencies": {"c": "^1"}})),
            ("2.0.0", no_deps()),
        ],
    );
    h.registry.add(
        "c",
        &[
            ("1.0.0", no_deps()),
            ("1.1.0", no_deps()),
            ("1.3.0", no_deps()),
        ],
    );
    // `fnpm update a`: only a moves; c stays locked.
    let out = h
        .run(
            &ws,
            Some(&first),
            Mode::Update {
                names: vec!["a".into()],
                latest: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(root_dep(&out.graph, "a").as_deref(), Some("1.2.0"));
    assert_eq!(versions(&out.graph, "c"), ["1.1.0"]);
    // `fnpm update c` updates a transitive dependency.
    let out = h
        .run(
            &ws,
            Some(&first),
            Mode::Update {
                names: vec!["c".into()],
                latest: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(versions(&out.graph, "c"), ["1.3.0"]);
    assert_eq!(root_dep(&out.graph, "a").as_deref(), Some("1.1.0"));
    // `fnpm update` updates the direct dependencies; never widening the ranges.
    let out = h
        .run(
            &ws,
            Some(&first),
            Mode::Update {
                names: vec![],
                latest: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(root_dep(&out.graph, "a").as_deref(), Some("1.2.0"));
}

#[tokio::test]
async fn update_latest_ignores_the_range_but_never_downgrades() {
    let h = Harness::new();
    h.registry.add_full(packument(
        "x",
        &[
            ("1.0.0", no_deps()),
            ("2.0.0", no_deps()),
            ("3.0.0-beta.1", no_deps()),
        ],
        &[("latest", "2.0.0")],
        &[],
    ));
    let ws = workspace(json!({"dependencies": {"x": "^1"}}), &[]);
    let first = h.install(&ws).await.graph;
    let out = h
        .run(
            &ws,
            Some(&first),
            Mode::Update {
                names: vec![],
                latest: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(root_dep(&out.graph, "x").as_deref(), Some("2.0.0"));
    // A dependency ahead of `latest` stays.
    let ws = workspace(json!({"dependencies": {"x": "3.0.0-beta.1"}}), &[]);
    let first = h.install(&ws).await.graph;
    let out = h
        .run(
            &ws,
            Some(&first),
            Mode::Update {
                names: vec![],
                latest: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(root_dep(&out.graph, "x").as_deref(), Some("3.0.0-beta.1"));
}

#[tokio::test]
async fn update_holds_patched_versions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.patch"), "patch").unwrap();
    let h = Harness::new();
    h.registry.add("x", &[("1.0.0", no_deps())]);
    let root = json!({"dependencies": {"x": "^1"}, "patchedDependencies": {"x@1.0.0": "x.patch"}});
    let ws = workspace_at(dir.path().to_path_buf(), root, &[]);
    let first = h.install(&ws).await.graph;
    h.registry
        .add("x", &[("1.0.0", no_deps()), ("1.1.0", no_deps())]);
    let out = h
        .run(
            &ws,
            Some(&first),
            Mode::Update {
                names: vec!["x".into()],
                latest: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(root_dep(&out.graph, "x").as_deref(), Some("1.0.0"));
    assert!(
        out.warnings
            .iter()
            .any(|w| w == "kept x@1.0.0 (patched, v1.1.0 available)"),
        "{:?}",
        out.warnings
    );
}

#[tokio::test]
async fn update_keeps_unfetchable_git_packages_with_a_warning() {
    let h = Harness::new();
    let sha = "c".repeat(40);
    h.fetcher.commit("https://git.test/r.git", "", &sha);
    h.fetcher.package(
        &format!("git:https://git.test/r.git#{sha}"),
        json!({"name": "r", "version": "1.0.0"}),
    );
    let ws = workspace(
        json!({"dependencies": {"r": "git+https://git.test/r.git"}}),
        &[],
    );
    let first = h.install(&ws).await.graph;
    h.fetcher.commits.lock().clear();
    let out = h
        .run(
            &ws,
            Some(&first),
            Mode::Update {
                names: vec![],
                latest: false,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(&dep(&out.graph, PackageId::ROOT, "r").unwrap().resolution, Resolution::Git { resolved_commit, .. } if *resolved_commit == sha)
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.ends_with("was not checked for updates")),
        "{:?}",
        out.warnings
    );
}

#[tokio::test]
async fn requests_with_a_previous_graph() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(json!({"dependencies": {"a": "^1"}}), &[]);
    let first = h.install(&ws).await.graph;
    let (out, _) = h
        .run_full(
            &ws,
            Some(&first),
            Mode::Normal,
            vec![common::request("", "b", "latest", DependencyGroup::Dev)],
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(out.graph.packages[out.requests[0].1.index()].name, "b");
    assert!(out.graph.root().dependency("b").unwrap().behavior.is_dev());
}

#[tokio::test]
async fn new_duplicates_are_collapsed() {
    let h = Harness::new();
    // `x` asks for ^1.0.0 first and gets 1.2.0 (latest); `y` pins 1.0.0 later. The appended
    // 1.2.0 is collapsed onto 1.0.0 because one version satisfies both.
    h.registry
        .add("x", &[("1.0.0", json!({"dependencies": {"d": "^1.0.0"}}))])
        .add("y", &[("1.0.0", json!({"dependencies": {"d": "1.0.0"}}))])
        .add("d", &[("1.0.0", no_deps()), ("1.2.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"x": "1.0.0", "y": "1.0.0"}}), &[]);
    let g = h.install(&ws).await.graph;
    assert_eq!(versions(&g, "d"), ["1.0.0"]);
    g.validate().unwrap();
}

#[tokio::test]
async fn dedupe_command_removes_duplicates() {
    let h = Harness::new();
    h.registry
        .add("x", &[("1.0.0", json!({"dependencies": {"d": "^1.0.0"}}))])
        .add("d", &[("1.0.0", no_deps()), ("1.5.0", no_deps())]);
    let ws = workspace(json!({"dependencies": {"x": "1.0.0", "d": "^1.0.0"}}), &[]);
    let mut g = h.install(&ws).await.graph;
    // Simulate an older lockfile where x got 1.0.0 and the root 1.5.0.
    let old = g.packages.len();
    let d15 = id_of(&g, "d", "1.5.0");
    let mut d10 = fnpm_types::Package::new("d", "1.0.0", Resolution::npm("1.0.0", None));
    d10.dependencies.clear();
    let d10 = g.push(d10);
    let x = id_of(&g, "x", "1.0.0");
    g.packages[x.index()]
        .dependencies
        .iter_mut()
        .find(|e| e.name == "d")
        .unwrap()
        .target = Some(d10);
    assert_eq!(g.packages.len(), old + 1);
    let report = dedupe(&mut g);
    assert_eq!(report.rows.len(), 1);
    assert_eq!(
        (
            report.rows[0].name.as_str(),
            report.rows[0].from.as_str(),
            report.rows[0].to.as_slice()
        ),
        ("d", "1.0.0", &["1.5.0".to_owned()][..])
    );
    assert!(!report.rows[0].downgrade);
    assert_eq!(versions(&g, "d"), ["1.5.0"]);
    let _ = d15;
    g.validate().unwrap();
    // Nothing more to do.
    assert!(dedupe(&mut g).rows.is_empty());
}

#[tokio::test]
async fn dedupe_keeps_patched_versions() {
    let mut g = fnpm_types::LockGraph::new(fnpm_types::Package::new("app", "", Resolution::Root));
    let x = g.push(fnpm_types::Package::new(
        "x",
        "1.0.0",
        Resolution::npm("1.0.0", None),
    ));
    let d1 = g.push(fnpm_types::Package::new(
        "d",
        "1.0.0",
        Resolution::npm("1.0.0", None),
    ));
    let d2 = g.push(fnpm_types::Package::new(
        "d",
        "1.5.0",
        Resolution::npm("1.5.0", None),
    ));
    let mut e = fnpm_types::DepEdge::new("d", "^1.0.0", fnpm_types::Behavior::PROD);
    e.target = Some(d1);
    g.packages[x.index()].dependencies.push(e);
    let mut r1 = fnpm_types::DepEdge::new("x", "1.0.0", fnpm_types::Behavior::PROD);
    r1.target = Some(x);
    let mut r2 = fnpm_types::DepEdge::new("d", "^1.5.0", fnpm_types::Behavior::PROD);
    r2.target = Some(d2);
    g.root_mut().dependencies = vec![r2, r1];
    let mut patched = g.clone();
    patched.patched_dependencies.insert(
        "d@1.0.0".into(),
        fnpm_types::PatchedDependency {
            path: "p".into(),
            hash: None,
        },
    );
    assert_eq!(dedupe(&mut patched).rows.len(), 0);
    assert_eq!(dedupe(&mut g).rows.len(), 1);
}

#[tokio::test]
async fn prefetch_hints_are_saved_and_requested_up_front() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(json!({"dependencies": {"a": "^1", "b": "1.0.0"}}), &[]);
    h.install(&ws).await;
    // The save runs on a blocking task.
    for _ in 0..100 {
        if !fnpm_resolver::prefetch::load_hints(&h.config.cache_dir, &ws.root_dir).is_empty() {
            break;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let hints = fnpm_resolver::prefetch::load_hints(&h.config.cache_dir, &ws.root_dir);
    assert_eq!(hints, ["a", "b", "c"]);
    // A fresh cold resolve asks for every hinted manifest before resolving anything.
    let h2 = Harness::new();
    registry(&h2);
    let mut h2 = h2;
    h2.config.cache_dir = h.config.cache_dir.clone();
    let ws = workspace(json!({"dependencies": {"b": "1.0.0"}}), &[]);
    h2.install(&ws).await;
    let requests = h2.registry.requests();
    assert_eq!(&requests[..3], ["a", "b", "c"]);
}

#[tokio::test]
async fn an_invalid_previous_graph_is_ignored() {
    let h = Harness::new();
    registry(&h);
    let ws = workspace(json!({"dependencies": {"b": "1.0.0"}}), &[]);
    let mut bad = h.install(&ws).await.graph;
    bad.packages[0].dependencies[0].target = Some(PackageId(99));
    let out = h.run(&ws, Some(&bad), Mode::Normal).await.unwrap();
    assert!(out.changed);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("ignoring the lockfile"))
    );
    out.graph.validate().unwrap();
}
