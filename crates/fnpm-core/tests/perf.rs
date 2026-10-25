//! Timings of the install pipeline on a synthetic project (150 registry packages, 4 levels)
//! against the in-process registry. Prints the numbers; asserts only generous bounds so the
//! test is stable on loaded machines (`cargo test --release -p fnpm-core --test perf --
//! --nocapture` for the figures in docs).

mod common;

use std::time::{Duration, Instant};

use common::{Pkg, Project, Registry};
use fnpm_core::options::InstallCommand;
use serde_json::json;

/// `n` packages: `p0..p9` are direct dependencies, every `pi` depends on `p(2i+10)` and
/// `p(2i+11)` while they exist.
fn fixture(n: usize) -> Vec<Pkg> {
    (0..n)
        .map(|i| {
            let mut p = Pkg::new(&format!("p{i}"), "1.0.0");
            for c in [2 * i + 10, 2 * i + 11] {
                if c < n {
                    p = p.dep(&format!("p{c}"), "^1.0.0");
                }
            }
            for f in 0..8 {
                p.files
                    .push((format!("lib/f{f}.js"), format!("// {i} {f}\n").repeat(40)));
            }
            p
        })
        .collect()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipeline_timings() {
    let reg = Registry::start(&fixture(150)).await;
    let deps: serde_json::Map<String, serde_json::Value> = (0..10)
        .map(|i| (format!("p{i}"), json!("^1.0.0")))
        .collect();
    let p = Project::new(&reg, json!({ "name": "bench", "dependencies": deps }));

    let t = Instant::now();
    let s = p.install().await;
    let cold = t.elapsed();
    assert_eq!(s.installed_count, 150);

    let t = Instant::now();
    let s = p.install().await;
    let noop = t.elapsed();
    assert!(s.noop);

    // Best of 20 no-ops (the steady state the launcher sees).
    let mut best = Duration::MAX;
    for _ in 0..20 {
        let t = Instant::now();
        assert!(p.install().await.noop);
        best = best.min(t.elapsed());
    }

    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    let t = Instant::now();
    let s = p.install().await;
    let warm = t.elapsed();
    assert_eq!(s.installed_count, 150);

    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    let mut o = p.opts(InstallCommand::Install, &[]);
    o.pm.frozen_lockfile = true;
    let t = Instant::now();
    p.run(o).await.unwrap();
    let ci = t.elapsed();

    // Fast path alone.
    let config = fnpm_config::Config::load(&fnpm_config::LoadOptions {
        cwd: p.dir.clone(),
        env: p.env(),
        ..Default::default()
    })
    .unwrap();
    let hash = fnpm_core::config_hash(&config);
    let meta = fnpm_core::fast_path::meta_dir(&config.cache_dir, &p.dir);
    let t = Instant::now();
    for _ in 0..100 {
        let _ = fnpm_core::fast_path::check(&p.dir, &meta, hash);
    }
    let check = t.elapsed() / 100;

    eprintln!(
        "fnpm-core 150 packages: cold {:.1} ms, warm (store hit) {:.1} ms, frozen {:.1} ms, no-op first {:.2} ms, no-op best {:.2} ms, fast_path::check {:.3} ms",
        ms(cold),
        ms(warm),
        ms(ci),
        ms(noop),
        ms(best),
        ms(check)
    );
    if !cfg!(debug_assertions) {
        assert!(best < Duration::from_millis(20), "no-op {best:?}");
        assert!(check < Duration::from_millis(5), "check {check:?}");
    }
}
