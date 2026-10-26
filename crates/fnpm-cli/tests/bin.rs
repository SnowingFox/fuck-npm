//! The built executables: `--version`, usage errors, and the exec hand-off to Node with the
//! argv unchanged and `FNPM_LAUNCHER` set.

#![cfg(unix)]

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

/// A fake JS entry run by `/bin/sh` standing in for node: prints its own path, its argv
/// (one per line) and `FNPM_LAUNCHER`, then exits 7. The script is only read, never exec'd,
/// so a parallel test forking while it is written cannot make the exec fail with ETXTBSY.
fn fake_entry(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        "printf '%s\\n' \"$0\"\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done\nprintf 'launcher=%s\\n' \"$FNPM_LAUNCHER\"\nexit 7\n",
    )
    .unwrap();
    path.to_path_buf()
}

const SH: &str = "/bin/sh";

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
    let node = PathBuf::from(SH);
    let entry = fake_entry(&dir.path().join("fnpm.js"));
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
    let node = PathBuf::from(SH);
    let entry = dir.path().join("dist").join("fnpm.js");
    fake_entry(&dir.path().join("dist").join("fnpm-lock.js"));
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
    let node = PathBuf::from(SH);
    let entry = fake_entry(&dir.path().join("fnpm.js"));
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
        format!(
            "fnpm: 'outdated' needs Node.js 20.19 or newer, and FNPM_NODE ({}) was not found\n",
            missing.display()
        )
    );

    // Without FNPM_NODE: `node` on PATH, and an empty PATH has none.
    let output = Command::new(FNPM)
        .args(["--cwd", "app", "why", "react"])
        .env_remove("FNPM_NODE")
        .env("FNPM_JS_ENTRY", &entry)
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "fnpm: 'why' needs Node.js 20.19 or newer, and no 'node' was found on PATH\n"
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
        "error: fnpm install has no --bogus flag.\nnote: Run 'fnpm install --help' for more information\n"
    );

    let output = run(FNPM, &["add", "--json"], &[("FNPM_NODE", &missing)]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        "{\"ok\":false,\"error\":{\"code\":\"MISSING_ARGUMENT\",\"message\":\"`fnpm add` needs <packages>: \\\"name\\\" or \\\"name@version\\\" of package(s) to install.\"}}\n"
    );
}

#[test]
fn script_version_flag_is_forwarded_to_node() {
    let dir = tempfile::tempdir().unwrap();
    let node = PathBuf::from(SH);
    let entry = fake_entry(&dir.path().join("fnpm.js"));
    let output = run(
        FNPM,
        &["script.cjs", "--version"],
        &[("FNPM_NODE", &node), ("FNPM_JS_ENTRY", &entry)],
    );
    assert_eq!(output.status.code(), Some(7));
    assert!(stdout(&output).contains("script.cjs\n--version\n"));
}

#[test]
fn node_gyp_shim_reaches_package_once_and_preserves_arguments() {
    assert_node_gyp_shim_runs_once(std::ffi::OsStr::new("shim directory with spaces"));
}

#[test]
fn node_gyp_shim_works_with_non_utf8_tempdir() {
    use std::os::unix::ffi::OsStrExt;

    assert_node_gyp_shim_runs_once(std::ffi::OsStr::from_bytes(b"caf\xe9"));
}

fn assert_node_gyp_shim_runs_once(shim_dir_name: &std::ffi::OsStr) {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project with spaces");
    let package = project.join("node_modules/node-gyp");
    let helpers = tmp.path().join("helpers");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&helpers).unwrap();
    std::fs::write(project.join("binding.gyp"), "{}").unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"native-project","private":true}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"node-gyp","version":"1.0.0","bin":{"node-gyp":"gyp"}}"#,
    )
    .unwrap();
    let gyp = package.join("gyp");
    std::fs::write(&gyp, "#!/bin/sh\nprintf '<%s>\\n' \"$@\"\nexit 7\n").unwrap();
    std::fs::set_permissions(&gyp, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Exercise the real `fnpm x` resolver. Bound recursion so a broken shim fails promptly
    // instead of growing PATH until the OS rejects exec with E2BIG.
    let launcher = helpers.join("fnpm");
    std::fs::write(
        &launcher,
        r#"#!/bin/sh
printf '%s\n' "${#PATH}" >> "$FNPM_TEST_TRACE"
depth=${FNPM_TEST_DEPTH:-0}
if [ "$depth" -ge 3 ]; then
  echo 'node-gyp shim recursed through fnpm x' >&2
  exit 97
fi
export FNPM_TEST_DEPTH=$((depth + 1))
exec "$FNPM_TEST_BINARY" "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let shim_root = tmp.path().join(shim_dir_name);
    std::fs::create_dir(&shim_root).unwrap();
    let shim = fnpm_scripts::node_gyp::NodeGypShim::create(&shim_root).unwrap();
    let trace = tmp.path().join("invocations");
    let path = std::env::join_paths([
        helpers.as_path(),
        shim.dir(),
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ])
    .unwrap();
    let output = Command::new(shim.dir().join("node-gyp"))
        .args(["rebuild", "--release", "argument with spaces", "", "*.gyp"])
        .current_dir(&project)
        .env_clear()
        .env("HOME", tmp.path())
        .env("PATH", path)
        .env("FNPM_TEST_BINARY", FNPM)
        .env("FNPM_TEST_TRACE", &trace)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "<rebuild>\n<--release>\n<argument with spaces>\n<>\n<*.gyp>\n"
    );
    let invocations = std::fs::read_to_string(trace).unwrap();
    assert_eq!(
        invocations.lines().count(),
        1,
        "recursive PATH lengths: {invocations}"
    );
}
