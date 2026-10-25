//! The no-op install, end to end through the built executable: the launcher answers it
//! before the tokio runtime exists, and the answer must be the one the core's own fast path
//! gives.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use fnpm_core::InstallOptions;
use fnpm_types::NoopSink;
use serde_json::Value;

const FNPM: &str = env!("CARGO_BIN_EXE_fnpm");

/// Store, cache and home inside `dir`, so nothing outside the test is read or written.
fn isolated_env(dir: &Path) -> Vec<(String, String)> {
    [
        "FNPM_STORE_DIR",
        "FNPM_CACHE_DIR",
        "HOME",
        "XDG_CONFIG_HOME",
    ]
    .iter()
    .map(|key| {
        (
            key.to_string(),
            dir.join(key.to_lowercase()).to_string_lossy().into_owned(),
        )
    })
    .collect()
}

fn install_json(project: &Path, env: &[(String, String)], args: &[&str]) -> Value {
    let output = Command::new(FNPM)
        .args(args)
        .arg("--json")
        .current_dir(project)
        .env_remove("NPM_CONFIG_REGISTRY")
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn project() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("app");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    (dir, project)
}

#[test]
fn a_repeat_install_is_a_noop() {
    let (dir, project) = project();
    let env = isolated_env(dir.path());
    let first = install_json(&project, &env, &["install"]);
    assert_eq!(first["summary"]["noop"], false);
    assert!(project.join("node_modules/.fnpm/state").is_file());

    for args in [
        &["install"][..],
        &["i"],
        &["ci"],
        &["install", "--frozen-lockfile"],
    ] {
        let again = install_json(&project, &env, args);
        assert_eq!(again["ok"], true, "{args:?}");
        assert_eq!(again["command"], "install", "{args:?}");
        assert_eq!(again["summary"]["noop"], true, "{args:?}");
        assert_eq!(
            again["summary"]["checked"],
            serde_json::json!({ "installs": 1, "packages": 1 })
        );
    }

    // Anything that may change node_modules is not a no-op.
    let forced = install_json(&project, &env, &["install", "--force"]);
    assert_eq!(forced["summary"]["noop"], false);
    let lockfile_only = install_json(&project, &env, &["install", "--lockfile-only"]);
    assert_eq!(lockfile_only["summary"]["noop"], false);
}

#[test]
fn the_launcher_and_the_core_agree_on_a_noop() {
    let (dir, project) = project();
    let env = isolated_env(dir.path());
    install_json(&project, &env, &["install"]);
    let native = install_json(&project, &env, &["install"]);

    let options: InstallOptions =
        serde_json::from_value(serde_json::json!({ "cwd": project, "command": "install" }))
            .unwrap();
    let mut core_env = fnpm_config::Env::from_process();
    for (key, value) in &env {
        core_env.set(key.clone(), value.clone());
    }
    core_env.remove("NPM_CONFIG_REGISTRY");
    let summary = fnpm_core::runtime()
        .block_on(fnpm_core::install_with_env(
            options,
            Arc::new(NoopSink),
            core_env,
        ))
        .unwrap();
    let mut core = serde_json::to_value(summary).unwrap();
    let mut native = native["summary"].clone();
    core["elapsedMs"] = Value::Null;
    native["elapsedMs"] = Value::Null;
    assert_eq!(native, core);
}

#[test]
fn a_noop_still_reports_configuration_warnings() {
    let (dir, project) = project();
    std::fs::write(
        project.join(".npmrc"),
        "//registry.npmjs.org/:certfile=/nonexistent/cert.pem\n",
    )
    .unwrap();
    let env = isolated_env(dir.path());
    install_json(&project, &env, &["install"]);
    let output = Command::new(FNPM)
        .arg("install")
        .current_dir(&project)
        .env_remove("NPM_CONFIG_REGISTRY")
        .env("NO_COLOR", "1")
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.contains("Checked 1 install across 1 package (no changes)"),
        "{stdout}"
    );
    assert!(
        stderr.contains("warn: ") && stderr.contains("certfile is not supported"),
        "{stderr}"
    );
}
