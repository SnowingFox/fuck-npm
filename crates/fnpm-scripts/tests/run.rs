//! `fnpm run` machinery (ports of bun-run.test.ts / run_command.rs behaviour).
#![cfg(unix)]

mod common;

use std::path::PathBuf;

use common::*;
use fnpm_scripts::run::{
    RunOutcome, RunRequest, binary_exit_message, exit_message, run_script, scripts_of,
};
use fnpm_scripts::{Error, RunOptions, run_command};
use serde_json::json;

fn project(scripts: serde_json::Value) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    write_pkg(
        &dir,
        json!({"name": "app", "version": "2.0.0", "config": {"c": "v"}, "scripts": scripts}),
    );
    (tmp, dir)
}

fn opts(p: &std::path::Path) -> RunOptions {
    let mut o = RunOptions::new(p);
    o.silent = true;
    o
}

async fn run(
    p: &std::path::Path,
    cwd: &std::path::Path,
    name: &str,
    args: &[String],
) -> fnpm_scripts::Result<RunOutcome> {
    run_script(
        &RunRequest {
            cwd,
            name,
            args,
            npm_command: "run-script",
        },
        &opts(p),
    )
    .await
}

#[tokio::test]
async fn pre_and_post_hooks_run_around_the_script() {
    let (_t, p) = project(json!({
        "prebuild": "echo pre >> log.txt",
        "build": "echo main \"$@\" >> log.txt",
        "postbuild": "echo post >> log.txt"
    }));
    let out = run(&p, &p, "build", &["a b".into(), "$X".into()])
        .await
        .unwrap();
    assert_eq!(out, RunOutcome::Script { code: 0 });
    assert_eq!(read(&p.join("log.txt")), "pre\nmain a b $X\npost\n");
}

#[tokio::test]
async fn a_failing_pre_script_stops_the_chain() {
    let (_t, p) = project(json!({"pretest": "exit 4", "test": "echo main > main.txt"}));
    let out = run(&p, &p, "test", &[]).await.unwrap();
    assert_eq!(out, RunOutcome::Script { code: 4 });
    assert_eq!(out.exit_code(false), 4);
    assert!(!p.join("main.txt").exists());
}

#[tokio::test]
async fn scripts_run_in_the_package_json_directory_with_npm_env() {
    let (_t, p) = project(json!({"where": "pwd > where.txt; env > env.txt"}));
    let sub = p.join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    let out = run(&p, &sub, "where", &[]).await.unwrap();
    assert_eq!(out, RunOutcome::Script { code: 0 });
    assert_eq!(read(&p.join("where.txt")).trim(), p.display().to_string());
    let env = read(&p.join("env.txt"));
    for line in [
        "npm_command=run-script",
        "npm_lifecycle_event=where",
        "npm_package_name=app",
        "npm_package_version=2.0.0",
        "npm_package_config_c=v",
    ] {
        assert!(env.lines().any(|l| l == line), "{line} in\n{env}");
    }
    let path = env.lines().find_map(|l| l.strip_prefix("PATH=")).unwrap();
    assert!(path.starts_with(&format!("{}/node_modules/.bin:", p.display())));
}

#[tokio::test]
async fn bins_run_when_there_is_no_script() {
    let (_t, p) = project(json!({}));
    let bin = p.join("node_modules/.bin/my-bin");
    write(
        &bin,
        &format!("#!/bin/sh\necho \"$@\" > {}/bin.txt\nexit 7\n", p.display()),
    );
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = run(&p, &p, "my-bin", &["x y".into()]).await.unwrap();
    assert_eq!(
        out,
        RunOutcome::Binary {
            path: bin.clone(),
            code: 7
        }
    );
    assert_eq!(
        read(&p.join("bin.txt")).trim(),
        "x y",
        "arguments are passed unescaped to binaries"
    );
}

#[tokio::test]
async fn missing_script_and_if_present() {
    let (_t, p) = project(json!({}));
    let out = run(&p, &p, "definitely-missing-fnpm-script", &[])
        .await
        .unwrap();
    assert_eq!(out, RunOutcome::NotFound);
    assert_eq!(out.exit_code(false), 1);
    assert_eq!(out.exit_code(true), 0);
}

#[tokio::test]
async fn signals_are_reported_to_the_caller() {
    let (_t, p) = project(json!({"die": "kill -KILL $$"}));
    let err = run(&p, &p, "die", &[]).await.unwrap_err();
    assert!(matches!(err, Error::Signaled { signal: 9, .. }), "{err:?}");
}

#[tokio::test]
async fn run_command_returns_the_exit_code() {
    let (_t, p) = project(json!({}));
    let code = run_command(
        &p,
        "custom",
        "echo \"$npm_lifecycle_event\" > e.txt; exit 5",
        &[],
        &opts(&p),
    )
    .await
    .unwrap();
    assert_eq!(code, 5);
    assert_eq!(read(&p.join("e.txt")).trim(), "custom");
    let code = fnpm_scripts::run::run_lifecycle_script_foreground(&p, "prepack", "true", &opts(&p))
        .await
        .unwrap();
    assert_eq!(code, 0);
}

#[test]
fn messages() {
    assert_eq!(
        exit_message("build", 1),
        "error: script \"build\" exited with code 1"
    );
    assert_eq!(
        binary_exit_message(std::path::Path::new("/usr/bin/test"), "test", 1),
        "error: \"/usr/bin/test\" exited with code 1\nnote: a package.json script \"test\" was not found"
    );
    assert_eq!(
        binary_exit_message(std::path::Path::new("/x/tsc"), "tsc", 2),
        "error: \"tsc\" exited with code 2"
    );
    assert_eq!(
        binary_exit_message(std::path::Path::new("/x/tsc"), "tsc", 130),
        "error: Failed to run \"tsc\" due to exit code 130"
    );
    assert_eq!(
        fnpm_scripts::run::signal_message("x", 15),
        "error: script \"x\" was terminated by signal SIGTERM"
    );
}

#[test]
fn scripts_keep_file_order() {
    let json = json!({"scripts": {"b": "1", "a": "2", "n": 3}});
    assert_eq!(
        scripts_of(&json),
        vec![
            ("b".to_string(), "1".to_string()),
            ("a".to_string(), "2".to_string())
        ]
    );
}
