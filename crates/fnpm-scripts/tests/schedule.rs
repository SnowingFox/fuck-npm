//! Dependency-order gating and install planning.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use common::*;
use fnpm_scripts::schedule::{PlanOptions, dependencies, levels, plan_install};
use fnpm_scripts::{Job, Optimizer, TrustPolicy};
use fnpm_types::{Behavior, PackageId, Platform};
use serde_json::json;

fn job(id: PackageId) -> Job {
    Job {
        package: id,
        name: format!("p{}", id.0),
        version: "1.0.0".into(),
        dir: PathBuf::from("/x"),
        package_json: PathBuf::from("/x/package.json"),
        scripts: vec![("postinstall".into(), "true".into())],
        optional: false,
        foreground: false,
    }
}

#[test]
fn nearest_jobs_through_packages_without_scripts() {
    // root -> a(job) -> plain -> b(job) -> c(job)
    let mut g = root("app");
    let a = add(&mut g, npm("a", "1.0.0"));
    let plain = add(&mut g, npm("plain", "1.0.0"));
    let b = add(&mut g, npm("b", "1.0.0"));
    let c = add(&mut g, npm("c", "1.0.0"));
    edge(&mut g, PackageId::ROOT, "a", a);
    edge(&mut g, a, "plain", plain);
    edge(&mut g, plain, "b", b);
    edge(&mut g, b, "c", c);
    let jobs = vec![job(a), job(b), job(c), job(PackageId::ROOT)];
    let deps = dependencies(&g, &jobs);
    assert_eq!(deps[0], [1], "a waits for b, not for c through b");
    assert_eq!(deps[1], [2]);
    assert!(deps[2].is_empty());
    assert_eq!(deps[3], [0, 1, 2], "the root waits for everything");
    let lv = levels(jobs, &deps);
    let names: Vec<Vec<String>> = lv
        .iter()
        .map(|l| l.iter().map(|j| j.name.clone()).collect())
        .collect();
    assert_eq!(names, [vec!["p4"], vec!["p3"], vec!["p1"], vec!["p0"]]);
}

#[test]
fn cycles_do_not_wait_for_each_other() {
    let mut g = root("app");
    let a = add(&mut g, npm("a", "1.0.0"));
    let b = add(&mut g, npm("b", "1.0.0"));
    let c = add(&mut g, npm("c", "1.0.0"));
    edge(&mut g, a, "b", b);
    edge(&mut g, b, "a", a);
    edge(&mut g, b, "c", c);
    let deps = dependencies(&g, &[job(a), job(b), job(c)]);
    assert_eq!(deps[0], [2]);
    assert_eq!(deps[1], [2]);
    assert!(deps[2].is_empty());
}

#[test]
fn plan_install_selects_trusted_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().to_path_buf();
    let mut g = root("app");
    g.trusted_dependencies = Some(
        [
            "trusted".to_string(),
            "opt".to_string(),
            "sharp".to_string(),
        ]
        .into_iter()
        .collect(),
    );
    let trusted = add(&mut g, with_scripts(npm("trusted", "1.0.0")));
    let untrusted = add(&mut g, with_scripts(npm("untrusted", "1.0.0")));
    let no_scripts = add(&mut g, npm("no-scripts", "1.0.0"));
    let opt = add(&mut g, with_scripts(npm("opt", "1.0.0")));
    let sharp = add(&mut g, with_scripts(npm("sharp", "0.33.1")));
    edge(&mut g, PackageId::ROOT, "trusted", trusted);
    edge(&mut g, PackageId::ROOT, "untrusted", untrusted);
    edge(&mut g, PackageId::ROOT, "no-scripts", no_scripts);
    edge(&mut g, PackageId::ROOT, "sharp", sharp);
    edge_with(&mut g, trusted, "opt", opt, Behavior::OPTIONAL);
    write_pkg(
        &p,
        json!({"scripts": {"postinstall": "echo root", "test": "x"}}),
    );
    for (name, scripts) in [
        ("trusted", json!({"postinstall": "echo t"})),
        (
            "untrusted",
            json!({"preinstall": "echo u", "postinstall": "echo u"}),
        ),
        ("no-scripts", json!({})),
        ("opt", json!({"install": "echo o"})),
        ("sharp", json!({"install": "echo s"})),
    ] {
        write_pkg(
            &p.join("node_modules").join(name),
            json!({"name": name, "scripts": scripts}),
        );
    }
    let graph = g.clone();
    let names: Vec<String> = graph.packages.iter().map(|x| x.name.clone()).collect();
    let locate = |id: PackageId| {
        Some(if id == PackageId::ROOT {
            p.clone()
        } else {
            p.join("node_modules").join(&names[id.index()])
        })
    };
    let empty = BTreeSet::new();
    let reg = |_: &str| fnpm_config::DEFAULT_REGISTRY;
    let opts = PlanOptions {
        ignore_scripts: false,
        platform: Platform::current(),
        trusted_from_requests: &empty,
        registry_for: &reg,
        installed: None,
        members: None,
    };
    let plan = plan_install(
        &graph,
        &TrustPolicy::from_graph(&graph, []),
        &Optimizer::default(),
        &opts,
        &locate,
    )
    .unwrap();
    let jobs: Vec<(&str, bool, bool)> = plan
        .jobs
        .iter()
        .map(|j| (j.name.as_str(), j.optional, j.foreground))
        .collect();
    assert_eq!(
        jobs,
        [
            ("app", false, true),
            ("trusted", false, false),
            ("opt", true, false)
        ]
    );
    assert_eq!(
        plan.jobs[0].scripts,
        [("postinstall".to_string(), "echo root".to_string())]
    );
    assert_eq!(plan.blocked.len(), 1);
    assert_eq!(plan.blocked[0].name, "untrusted");
    assert_eq!(plan.blocked_scripts(), 2);
    assert_eq!(plan.ignored, [sharp]);
    assert_eq!(plan.deps[0], [1, 2], "root last");
    assert_eq!(
        plan.deps[1],
        [2],
        "trusted waits for its optional dependency"
    );

    let ignore = PlanOptions {
        ignore_scripts: true,
        ..opts
    };
    let none = plan_install(
        &graph,
        &TrustPolicy::from_graph(&graph, []),
        &Optimizer::default(),
        &ignore,
        &locate,
    )
    .unwrap();
    assert!(none.jobs.is_empty() && none.blocked.is_empty());
}
