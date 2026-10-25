//! Performance budgets from `docs/ARCHITECTURE.md#fnpm-resolver`: a warm resolve of 1.5k
//! packages under 40 ms, and an unchanged project with a lockfile under 2 ms. Run with
//! `cargo test --release -p fnpm-resolver --test perf -- --ignored --nocapture`.

mod common;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::time::{Duration, Instant};

use common::*;
use fnpm_resolver::Mode;
use serde_json::{Map, Value, json};

/// 1500 packages in layers: every package depends on four packages of the next layer (with
/// ranges that share versions), each has three versions, plus some peers.
fn synthetic(h: &Harness) -> fnpm_workspace::Workspace {
    let layers = 6;
    let width = 250;
    for layer in 0..layers {
        for i in 0..width {
            let name = format!("p{layer}-{i}");
            let mut deps = Map::new();
            let mut peers = Map::new();
            if layer + 1 < layers {
                for k in 0..4 {
                    deps.insert(
                        format!("p{}-{}", layer + 1, (i * 7 + k * 13) % width),
                        Value::String(format!("^1.{}", k % 2)),
                    );
                }
                if i % 10 == 0 {
                    peers.insert(format!("p{}-{}", layer + 1, (i + 1) % width), json!("^1"));
                }
            }
            let body = json!({"dependencies": deps, "peerDependencies": peers});
            h.registry.add(
                &name,
                &[
                    ("1.0.0", body.clone()),
                    ("1.1.0", body.clone()),
                    ("1.2.0", body),
                ],
            );
        }
    }
    let mut root = Map::new();
    for i in 0..width {
        root.insert(format!("p0-{i}"), json!("^1"));
    }
    workspace(json!({"name": "big", "dependencies": root}), &[])
}

fn best_of<F: FnMut() -> Duration>(n: usize, mut f: F) -> Duration {
    (0..n).map(|_| f()).min().unwrap_or_default()
}

#[test]
#[ignore = "timing; run in release"]
fn warm_resolve_of_1500_packages() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let h = Harness::new();
    let ws = synthetic(&h);
    let first = rt.block_on(h.install(&ws));
    let n = first.graph.len();
    assert!(n >= 1500, "{n}");
    let cold = best_of(5, || {
        let t = Instant::now();
        rt.block_on(h.install(&ws));
        t.elapsed()
    });
    let noop = best_of(20, || {
        let t = Instant::now();
        let out = rt
            .block_on(h.run(&ws, Some(&first.graph), Mode::Normal))
            .unwrap();
        assert!(!out.changed);
        t.elapsed()
    });
    let frozen = best_of(20, || {
        let t = Instant::now();
        rt.block_on(h.run(&ws, Some(&first.graph), Mode::Frozen))
            .unwrap();
        t.elapsed()
    });
    println!("packages={n} warm={cold:?} noop={noop:?} frozen={frozen:?}");
    assert!(
        cold < Duration::from_millis(40),
        "warm resolve took {cold:?}"
    );
    assert!(noop < Duration::from_millis(2), "no-op took {noop:?}");
}

/// 1500 packages where every package below the first layer declares peers that the layer above
/// provides (plus optional peers nobody provides): the peer pass dominates.
fn peer_heavy(h: &Harness) -> fnpm_workspace::Workspace {
    let layers = 6;
    let width = 250;
    for layer in 0..layers {
        for i in 0..width {
            let name = format!("q{layer}-{i}");
            let mut deps = Map::new();
            let mut peers = Map::new();
            let mut meta = Map::new();
            if layer + 1 < layers {
                for k in 0..4 {
                    deps.insert(
                        format!("q{}-{}", layer + 1, (i * 7 + k * 13) % width),
                        json!("^1"),
                    );
                }
            }
            if layer > 0 {
                for k in 0..2 {
                    peers.insert(format!("q{}-{}", layer - 1, (i + k) % width), json!("^1"));
                }
                meta.insert(format!("nobody-{i}"), json!({"optional": true}));
            }
            let body = json!({"dependencies": deps, "peerDependencies": peers, "peerDependenciesMeta": meta});
            h.registry
                .add(&name, &[("1.0.0", body.clone()), ("1.1.0", body)]);
        }
    }
    let mut root = Map::new();
    for i in 0..width {
        root.insert(format!("q0-{i}"), json!("^1"));
    }
    workspace(json!({"name": "peers", "dependencies": root}), &[])
}

#[test]
#[ignore = "timing; run in release"]
fn peer_heavy_and_incremental_resolves() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let h = Harness::new();
    let ws = peer_heavy(&h);
    let first = rt.block_on(h.install(&ws));
    if let Some(path) = std::env::var_os("FNPM_PERF_DUMP") {
        std::fs::write(path, serde_json::to_string_pretty(&first.graph).unwrap()).unwrap();
    }
    let cold = best_of(5, || {
        let t = Instant::now();
        rt.block_on(h.install(&ws));
        t.elapsed()
    });
    // One new root dependency: everything else is reused from the lockfile.
    h.registry.add("extra", &[("1.0.0", json!({}))]);
    let mut root = ws.root.raw.clone();
    root["dependencies"]["extra"] = json!("^1");
    let changed = workspace(root, &[]);
    let incremental = best_of(5, || {
        let t = Instant::now();
        let out = rt
            .block_on(h.run(&changed, Some(&first.graph), Mode::Normal))
            .unwrap();
        assert!(out.changed);
        t.elapsed()
    });
    println!(
        "packages={} peer_heavy={cold:?} incremental={incremental:?}",
        first.graph.len()
    );
    assert!(
        cold < Duration::from_millis(80),
        "peer-heavy resolve took {cold:?}"
    );
    assert!(
        incremental < Duration::from_millis(40),
        "incremental resolve took {incremental:?}"
    );
}
