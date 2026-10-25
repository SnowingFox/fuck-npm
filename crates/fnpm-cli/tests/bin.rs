//! The built executables: `--version`, usage errors, and the exec hand-off to Node with the
//! argv unchanged and `FNPM_LAUNCHER` set.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FNPM: &str = env!("CARGO_BIN_EXE_fnpm");
const FNPM_LOCK: &str = env!("CARGO_BIN_EXE_fnpm-lock");

fn run(exe: &str, args: &[&str], envs: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(exe);
    command
        .args(args)
        .env_remove("FNPM_NODE")
        .env_remove("FNPM_JS_ENTRY")
        .env_remove("FNPM_LOCK_JS_ENTRY");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// A fake `node` that prints its argv (one per line) and `FNPM_LAUNCHER`, then exits 7.
fn fake_node(dir: &Path) -> PathBuf {
    let path = dir.join("node");
    std::fs::write(
        &path,
        "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done\nprintf 'launcher=%s\\n' \"$FNPM_LAUNCHER\"\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_matches_the_npm_package() {
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/fuck-npm/package.json");
    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap();
    let expected = format!("{}\n", pkg["version"].as_str().unwrap());
    for exe in [FNPM, FNPM_LOCK] {
        for flag in ["--version", "-v"] {
            let output = run(exe, &[flag], &[]);
            assert!(output.status.success());
            assert_eq!(stdout(&output), expected);
            assert_eq!(stderr(&output), "");
        }
    }
}

#[test]
fn exec_hands_node_the_original_argv() {
    let dir = tempfile::tempdir().unwrap();
    let node = fake_node(dir.path());
    let entry = dir.path().join("fnpm.js");
    let args = ["--cwd", "a b", "outdated", "--json", "", "ünï", "--", "-x"];
    let output = run(
        FNPM,
        &args,
        &[("FNPM_NODE", &node), ("FNPM_JS_ENTRY", &entry)],
    );
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    let lines: Vec<String> = stdout(&output).lines().map(str::to_owned).collect();
    assert_eq!(lines[0], entry.to_str().unwrap());
    assert_eq!(&lines[1..=args.len()], &args);
    let launcher = lines.last().unwrap().strip_prefix("launcher=").unwrap();
    assert_eq!(
        std::fs::canonicalize(launcher).unwrap(),
        std::fs::canonicalize(FNPM).unwrap()
    );
}

#[test]
fn fnpm_lock_execs_its_own_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let node = fake_node(dir.path());
    let entry = dir.path().join("dist").join("fnpm.js");
    let output = run(
        FNPM_LOCK,
        &["migrate", "--from", "npm"],
        &[("FNPM_NODE", &node), ("FNPM_JS_ENTRY", &entry)],
    );
    assert_eq!(output.status.code(), Some(7));
    let lines: Vec<String> = stdout(&output).lines().map(str::to_owned).collect();
    assert_eq!(
        lines[0],
        dir.path()
            .join("dist")
            .join("fnpm-lock.js")
            .to_str()
            .unwrap()
    );
    assert_eq!(&lines[1..4], ["migrate", "--from", "npm"]);
}

#[test]
fn help_and_interactive_update_go_to_node() {
    let dir = tempfile::tempdir().unwrap();
    let node = fake_node(dir.path());
    let entry = dir.path().join("fnpm.js");
    for args in [&["install", "--help"][..], &["update", "-i"], &[]] {
        let output = run(
            FNPM,
            args,
            &[("FNPM_NODE", &node), ("FNPM_JS_ENTRY", &entry)],
        );
        assert_eq!(output.status.code(), Some(7), "{args:?}");
        let lines: Vec<String> = stdout(&output).lines().map(str::to_owned).collect();
        assert_eq!(&lines[1..=args.len()], args);
    }
}

#[test]
fn missing_node_is_explained() {
    let dir = tempfile::tempdir().unwrap();
    let entry = dir.path().join("fnpm.js");
    let missing = dir.path().join("no-such-node");
    let output = run(
        FNPM,
        &["outdated"],
        &[("FNPM_NODE", &missing), ("FNPM_JS_ENTRY", &entry)],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "fnpm: 'outdated' needs Node.js 20.19 or newer, and no 'node' was found on PATH\n"
    );
}

#[test]
fn usage_errors_never_need_node() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-node");
    let output = run(FNPM, &["install", "--bogus"], &[("FNPM_NODE", &missing)]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "error: `fnpm install` has no `--bogus` flag.\nRun 'fnpm install --help' for detailed usage.\n"
    );

    let output = run(FNPM, &["add", "--json"], &[("FNPM_NODE", &missing)]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        "{\"ok\":false,\"error\":{\"code\":\"MISSING_ARGUMENT\",\"message\":\"`fnpm add` needs <packages>: \\\"name\\\" or \\\"name@version\\\" of package(s) to install.\"}}\n"
    );
}
