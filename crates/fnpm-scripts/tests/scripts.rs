//! Script names and order, and the default node-gyp script (bun `Scripts.rs`;
//! bun-install-lifecycle-scripts.test.ts "default node-gyp script", "auto node-gyp ...",
//! "git dependencies also run `preprepare`, `prepare`, and `postprepare` scripts",
//! "workspace lifecycle scripts").

mod common;

use common::*;
use fnpm_scripts::scripts::{NODE_GYP_REBUILD, PackageScripts, ScriptSource, job_scripts};
use serde_json::json;

fn all_six() -> serde_json::Value {
    json!({"scripts": {
        "preinstall": "a", "install": "b", "postinstall": "c",
        "preprepare": "d", "prepare": "e", "postprepare": "f", "test": "g"
    }})
}

fn events(v: &[(String, String)]) -> Vec<&str> {
    v.iter().map(|(e, _)| e.as_str()).collect()
}

#[test]
fn root_and_git_run_all_six() {
    let s = PackageScripts::from_json(&all_six());
    let all = [
        "preinstall",
        "install",
        "postinstall",
        "preprepare",
        "prepare",
        "postprepare",
    ];
    assert_eq!(events(&s.entries(ScriptSource::Root, false)), all);
    assert_eq!(events(&s.entries(ScriptSource::Git, false)), all);
    assert_eq!(
        s.entries(ScriptSource::Root, false)[4],
        ("prepare".to_string(), "e".to_string())
    );
}

#[test]
fn dependencies_run_install_scripts_only() {
    let s = PackageScripts::from_json(&all_six());
    assert_eq!(
        events(&s.entries(ScriptSource::Dependency, false)),
        ["preinstall", "install", "postinstall"]
    );
}

#[test]
fn workspaces_add_prepare_only() {
    let s = PackageScripts::from_json(&all_six());
    assert_eq!(
        events(&s.entries(ScriptSource::Workspace, false)),
        ["preinstall", "install", "postinstall", "prepare"]
    );
}

#[test]
fn empty_and_non_string_scripts_are_absent() {
    let s = PackageScripts::from_json(
        &json!({"scripts": {"preinstall": "", "postinstall": 1, "install": "x"}}),
    );
    assert_eq!(
        events(&s.entries(ScriptSource::Dependency, false)),
        ["install"]
    );
    assert!(!PackageScripts::from_json(&json!({"scripts": "nope"})).has_any());
    assert!(!PackageScripts::from_json(&json!({})).has_any());
}

#[test]
fn node_gyp_replaces_install_and_keeps_postinstall() {
    let s = PackageScripts::from_json(
        &json!({"scripts": {"postinstall": "echo post", "prepare": "p"}}),
    );
    let e = s.entries(ScriptSource::Dependency, true);
    assert_eq!(
        e,
        vec![
            ("install".into(), NODE_GYP_REBUILD.into()),
            ("postinstall".into(), "echo post".into())
        ]
    );
    let e = s.entries(ScriptSource::Root, true);
    assert_eq!(events(&e), ["install", "postinstall", "prepare"]);
}

#[test]
fn wants_default_node_gyp_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let s = PackageScripts::from_json(&json!({"scripts": {"postinstall": "x"}}));
    assert!(!s.wants_default_node_gyp(dir), "no binding.gyp");
    write(&dir.join("binding.gyp"), "{}");
    assert!(s.wants_default_node_gyp(dir));
    for script in ["install", "preinstall"] {
        let s = PackageScripts::from_json(&json!({"scripts": {script: "x"}}));
        assert!(
            !s.wants_default_node_gyp(dir),
            "does not add auto node-gyp script when {script} exists"
        );
    }
    let s = PackageScripts::from_json(&json!({"gypfile": false}));
    assert!(!s.wants_default_node_gyp(dir), "gypfile: false opts out");
    let s = PackageScripts::from_json(&json!({"gypfile": "false"}));
    assert!(
        s.wants_default_node_gyp(dir),
        "only a literal false opts out"
    );
}

#[test]
fn job_scripts_reads_disk_and_only_trusted_get_node_gyp() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_pkg(
        dir,
        json!({"name": "native", "scripts": {"postinstall": "echo hi"}}),
    );
    write(&dir.join("binding.gyp"), "{}");
    let trusted = job_scripts(dir, ScriptSource::Dependency, true).unwrap();
    assert_eq!(events(&trusted), ["install", "postinstall"]);
    let untrusted = job_scripts(dir, ScriptSource::Dependency, false).unwrap();
    assert_eq!(events(&untrusted), ["postinstall"]);
    assert!(job_scripts(&dir.join("missing"), ScriptSource::Dependency, true).is_err());
}

#[test]
fn blocked_events_count_like_bun() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let s = PackageScripts::from_json(&all_six());
    assert_eq!(
        s.blocked_events(ScriptSource::Dependency, dir),
        ["preinstall", "install", "postinstall"]
    );
    assert_eq!(s.blocked_events(ScriptSource::Git, dir).len(), 6);
    write(&dir.join("binding.gyp"), "{}");
    let s = PackageScripts::from_json(&json!({"scripts": {"postinstall": "x"}}));
    assert_eq!(
        s.blocked_events(ScriptSource::Dependency, dir),
        ["install", "postinstall"]
    );
    let none = PackageScripts::from_json(&json!({"gypfile": false}));
    assert!(
        none.blocked_events(ScriptSource::Dependency, dir)
            .is_empty()
    );
}

#[test]
fn jsonc_package_json() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join("package.json"),
        "{\n  // comment\n  \"scripts\": { \"postinstall\": \"x\", },\n}\n",
    );
    let s = PackageScripts::read(tmp.path()).unwrap();
    assert_eq!(s.get("postinstall"), Some("x"));
    assert_eq!(s.get("install"), None);
    assert_eq!(s.get("nonsense"), None);
}

#[test]
fn source_from_resolution() {
    use fnpm_types::Resolution;
    assert_eq!(
        ScriptSource::from_resolution(&Resolution::Root),
        ScriptSource::Root
    );
    assert_eq!(
        ScriptSource::from_resolution(&Resolution::Workspace { path: "a".into() }),
        ScriptSource::Workspace
    );
    assert_eq!(
        ScriptSource::from_resolution(&Resolution::Git {
            url: "u".into(),
            committish: "".into(),
            resolved_commit: "c".into(),
            integrity: None
        }),
        ScriptSource::Git
    );
    assert_eq!(
        ScriptSource::from_resolution(&Resolution::Folder { path: "a".into() }),
        ScriptSource::Dependency
    );
    assert!(ScriptSource::Git.is_dependency() && !ScriptSource::Workspace.is_dependency());
}

#[test]
fn only_the_root_job_is_foreground() {
    use fnpm_scripts::scripts::job_for;
    use fnpm_types::{Package, PackageId, Resolution};
    let tmp = tempfile::tempdir().unwrap();
    let ws_dir = tmp.path().join("packages/ws");
    write_pkg(tmp.path(), json!({"scripts": {"postinstall": "r"}}));
    write_pkg(&ws_dir, json!({"scripts": {"postinstall": "w"}}));
    let mut g = root("app");
    let ws = add(
        &mut g,
        Package::new(
            "ws",
            "1.0.0",
            Resolution::Workspace {
                path: "packages/ws".into(),
            },
        ),
    );
    let root_job = job_for(&g, PackageId::ROOT, tmp.path(), false).unwrap();
    let ws_job = job_for(&g, ws, &ws_dir, false).unwrap();
    assert!(root_job.foreground);
    assert!(
        !ws_job.foreground,
        "bun buffers workspace scripts like dependencies"
    );
    assert_eq!(
        ws_job.scripts,
        [("postinstall".to_string(), "w".to_string())]
    );
}
