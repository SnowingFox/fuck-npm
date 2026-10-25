//! `fnpm run --parallel` / `--sequential` (bun multi_run.rs, bun-run-parallel tests).
#![cfg(unix)]

mod common;

use std::path::PathBuf;

use common::*;
use fnpm_scripts::multi::{MultiOptions, MultiPackage, matches_glob, plan, run};
use fnpm_scripts::{Error, RunOptions};

fn pkg(dir: PathBuf, prefix: Option<&str>, scripts: &[(&str, &str)]) -> MultiPackage {
    MultiPackage {
        label_prefix: prefix.map(str::to_owned),
        package_json: Some(dir.join("package.json")),
        dir,
        scripts: scripts
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn glob_matching() {
    assert!(matches_glob("build:*", "build:js"));
    assert!(matches_glob("*", "anything"));
    assert!(matches_glob("*:js", "build:js"));
    assert!(matches_glob("a*b*c", "aXXbYYc"));
    assert!(!matches_glob("build:*", "test:js"));
    assert!(!matches_glob("abc", "abcd"));
    assert!(matches_glob("", ""));
}

#[test]
fn plans_pre_main_post_and_globs() {
    let p = pkg(
        PathBuf::from("/p"),
        None,
        &[
            ("prebuild", "echo pre"),
            ("build", "npm run x"),
            ("postbuild", "echo post"),
            ("lint:a", "a"),
            ("lint:b", "b"),
        ],
    );
    let out = plan(
        &names(&["build", "lint:*", "echo raw", "./x.js"]),
        &[p],
        MultiOptions::default(),
    )
    .unwrap();
    let got: Vec<(&str, &str, usize)> = out
        .iter()
        .map(|s| (s.label.as_str(), s.command.as_str(), s.group))
        .collect();
    assert_eq!(
        got,
        [
            ("build", "echo pre", 0),
            ("build", "fnpm run x", 0),
            ("build", "echo post", 0),
            ("lint:a", "a", 1),
            ("lint:b", "b", 2),
            ("echo raw", "echo raw", 3),
            ("./x.js", "node ./x.js", 4),
        ]
    );
    assert_eq!(out[0].name, "prebuild");
    assert_eq!(out[5].name, "");
}

#[test]
fn plan_errors() {
    let p = pkg(PathBuf::from("/p"), None, &[("a", "1")]);
    let msg = |r: fnpm_scripts::Result<Vec<_>>| match r {
        Err(Error::Message(m)) => m,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        msg(plan(&[], std::slice::from_ref(&p), MultiOptions::default())),
        "--parallel/--sequential requires at least one script name"
    );
    assert_eq!(
        msg(plan(
            &names(&["z*"]),
            std::slice::from_ref(&p),
            MultiOptions::default()
        )),
        "No scripts match pattern \"z*\""
    );
    let mut nojson = p.clone();
    nojson.package_json = None;
    assert_eq!(
        msg(plan(&names(&["a*"]), &[nojson], MultiOptions::default())),
        "Cannot use glob pattern \"a*\" without package.json scripts"
    );
}

#[test]
fn workspace_mode_labels_and_missing_scripts() {
    let a = pkg(PathBuf::from("/w/b"), Some("pkg-b"), &[("test", "tb")]);
    let b = pkg(
        PathBuf::from("/w/a"),
        Some("pkg-a"),
        &[("test", "ta"), ("lint", "la")],
    );
    let out = plan(
        &names(&["test"]),
        &[a.clone(), b.clone()],
        MultiOptions::default(),
    )
    .unwrap();
    let labels: Vec<&str> = out.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, ["pkg-a:test", "pkg-b:test"]);
    let err = plan(
        &names(&["lint"]),
        &[a.clone(), b.clone()],
        MultiOptions {
            workspaces: true,
            ..Default::default()
        },
    );
    assert!(
        matches!(err, Err(Error::Message(m)) if m == "Missing \"lint\" script in package \"pkg-b\"")
    );
    let ok = plan(
        &names(&["lint"]),
        &[a.clone(), b.clone()],
        MultiOptions {
            workspaces: true,
            if_present: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ok.len(), 1);
    let none = plan(&names(&["nope"]), &[a, b], MultiOptions::default());
    assert!(matches!(none, Err(Error::Message(_))));
}

fn dir() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let d = std::fs::canonicalize(t.path()).unwrap();
    write_pkg(&d, serde_json::json!({"name": "m"}));
    (t, d)
}

#[tokio::test]
async fn parallel_runs_everything_and_reports_the_failure() {
    let (_t, d) = dir();
    let p = pkg(
        d.clone(),
        None,
        &[("a", "touch a.txt"), ("b", "exit 3"), ("c", "touch c.txt")],
    );
    let scripts = plan(
        &names(&["a", "b", "c"]),
        &[p],
        MultiOptions {
            no_exit_on_error: true,
            ..Default::default()
        },
    )
    .unwrap();
    let code = run(
        scripts,
        &RunOptions::new(&d),
        MultiOptions {
            no_exit_on_error: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(code, 3);
    assert!(d.join("a.txt").exists() && d.join("c.txt").exists());
}

#[tokio::test]
async fn sequential_runs_in_order_and_stops_on_error() {
    let (_t, d) = dir();
    let p = pkg(
        d.clone(),
        None,
        &[
            ("a", "echo a >> log"),
            ("b", "echo b >> log; exit 2"),
            ("c", "echo c >> log"),
        ],
    );
    let mo = MultiOptions {
        sequential: true,
        ..Default::default()
    };
    let scripts = plan(&names(&["a", "b", "c"]), std::slice::from_ref(&p), mo).unwrap();
    let code = run(scripts, &RunOptions::new(&d), mo).await.unwrap();
    assert_eq!(code, 2);
    assert_eq!(read(&d.join("log")), "a\nb\n");
    // --no-exit-on-error keeps going with the next group.
    std::fs::remove_file(d.join("log")).unwrap();
    let mo = MultiOptions {
        sequential: true,
        no_exit_on_error: true,
        ..Default::default()
    };
    let scripts = plan(&names(&["a", "b", "c"]), &[p], mo).unwrap();
    assert_eq!(run(scripts, &RunOptions::new(&d), mo).await.unwrap(), 2);
    assert_eq!(read(&d.join("log")), "a\nb\nc\n");
}

#[tokio::test]
async fn a_failed_pre_script_skips_its_main_script() {
    let (_t, d) = dir();
    let p = pkg(
        d.clone(),
        None,
        &[
            ("prex", "exit 1"),
            ("x", "touch x.txt"),
            ("y", "touch y.txt"),
        ],
    );
    let mo = MultiOptions {
        no_exit_on_error: true,
        ..Default::default()
    };
    let scripts = plan(&names(&["x", "y"]), &[p], mo).unwrap();
    assert_eq!(run(scripts, &RunOptions::new(&d), mo).await.unwrap(), 1);
    assert!(!d.join("x.txt").exists());
    assert!(d.join("y.txt").exists());
}

#[tokio::test]
async fn success_is_zero_and_signals_map_to_128_plus() {
    let (_t, d) = dir();
    let p = pkg(d.clone(), None, &[("ok", "true"), ("die", "kill -TERM $$")]);
    let scripts = plan(
        &names(&["ok"]),
        std::slice::from_ref(&p),
        MultiOptions::default(),
    )
    .unwrap();
    assert_eq!(
        run(scripts, &RunOptions::new(&d), MultiOptions::default())
            .await
            .unwrap(),
        0
    );
    let scripts = plan(&names(&["die"]), &[p], MultiOptions::default()).unwrap();
    assert_eq!(
        run(scripts, &RunOptions::new(&d), MultiOptions::default())
            .await
            .unwrap(),
        128 + 15
    );
}

#[tokio::test]
async fn a_background_grandchild_holding_the_pipes_does_not_hang_the_run() {
    let (_t, d) = dir();
    // `sleep` inherits the script stdout/stderr and keeps them open after the shell exits.
    let p = pkg(d.clone(), None, &[("bg", "sleep 8 & echo started")]);
    let scripts = plan(&names(&["bg"]), &[p], MultiOptions::default()).unwrap();
    let started = std::time::Instant::now();
    let code = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run(scripts, &RunOptions::new(&d), MultiOptions::default()),
    )
    .await
    .expect("the run finishes when the script exits")
    .unwrap();
    assert_eq!(code, 0);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}
