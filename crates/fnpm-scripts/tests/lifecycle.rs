//! The lifecycle script runner against real shells (ports of
//! bun-install-lifecycle-scripts.test.ts cases that exercise script running).
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::*;
use fnpm_scripts::{Error, Job, RunOptions, run_dag, run_jobs};
use fnpm_types::event::ScriptPhase;
use fnpm_types::{Event, NoopSink, PackageId, event::VecSink};
use serde_json::json;

fn job(name: &str, dir: &Path, scripts: &[(&str, &str)]) -> Job {
    Job {
        package: PackageId(1),
        name: name.into(),
        version: "1.0.0".into(),
        dir: dir.to_path_buf(),
        package_json: dir.join("package.json"),
        scripts: scripts
            .iter()
            .map(|(e, c)| (e.to_string(), c.to_string()))
            .collect(),
        optional: false,
        foreground: false,
    }
}

fn project() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    write_pkg(&dir, json!({"name": "foo", "version": "1.0.0"}));
    (tmp, dir)
}

fn dep(project: &Path, name: &str) -> PathBuf {
    let dir = project.join("node_modules").join(name);
    write_pkg(
        &dir,
        json!({"name": name, "version": "1.0.0", "config": {"port": "8080"}}),
    );
    dir
}

fn opts(project: &Path) -> RunOptions {
    RunOptions::new(project)
}

fn env_file(path: &Path) -> BTreeMap<String, String> {
    read(path)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_package_with_all_lifecycle_scripts() {
    let (_t, p) = project();
    let names = [
        "preinstall",
        "install",
        "postinstall",
        "preprepare",
        "prepare",
        "postprepare",
    ];
    let scripts: Vec<(String, String)> = names
        .iter()
        .map(|n| (n.to_string(), format!("echo {n} >> order.txt")))
        .collect();
    let mut root = job("foo", &p, &[]);
    root.scripts = scripts;
    root.foreground = true;
    root.package = PackageId::ROOT;
    let sink = VecSink::default();
    let report = run_jobs(vec![vec![root]], &opts(&p), &sink).await.unwrap();
    assert_eq!(report.ran, 6);
    assert_eq!(
        read(&p.join("order.txt")).lines().collect::<Vec<_>>(),
        names
    );
    let events = sink.0.into_inner().unwrap();
    let starts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Script {
                script,
                phase: ScriptPhase::Start,
                ..
            } => Some(script.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(starts, names);
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Script {
            phase: ScriptPhase::End,
            exit_code: Some(0),
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_lifecycle_scripts_run_before_root_lifecycle_scripts() {
    let (_t, p) = project();
    let d = dep(&p, "slow-dep");
    let dep_job = job(
        "slow-dep",
        &d,
        &[("postinstall", "sleep 0.2; echo done > done.txt")],
    );
    let mut root = job(
        "foo",
        &p,
        &[("install", "cat node_modules/slow-dep/done.txt > root.txt")],
    );
    root.foreground = true;
    let report = run_dag(
        vec![root, dep_job],
        vec![vec![1], vec![]],
        &opts(&p),
        &NoopSink,
    )
    .await
    .unwrap();
    assert_eq!(report.ran, 2);
    assert_eq!(read(&p.join("root.txt")).trim(), "done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn levels_wait_for_the_previous_level() {
    let (_t, p) = project();
    let a = dep(&p, "a");
    let b = dep(&p, "b");
    let levels = vec![
        vec![job(
            "a",
            &a,
            &[("postinstall", "sleep 0.2; echo a > a.txt")],
        )],
        vec![job("b", &b, &[("postinstall", "cat ../a/a.txt > b.txt")])],
    ];
    run_jobs(levels, &opts(&p), &NoopSink).await.unwrap();
    assert_eq!(read(&b.join("b.txt")).trim(), "a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_environment() {
    let (_t, p) = project();
    let d = dep(&p, "env-dep");
    let j = job("alias-name", &d, &[("postinstall", "env > env.txt")]);
    let mut o = opts(&p);
    o.env.push(("FNPM_TEST_EXTRA".into(), "yes".into()));
    run_jobs(vec![vec![j]], &o, &NoopSink).await.unwrap();
    let env = env_file(&d.join("env.txt"));
    assert_eq!(env["npm_lifecycle_event"], "postinstall");
    assert_eq!(env["npm_lifecycle_script"], "env > env.txt");
    assert_eq!(
        env["npm_package_name"], "env-dep",
        "package.json name wins over the folder name"
    );
    assert_eq!(env["npm_package_version"], "1.0.0");
    assert_eq!(env["npm_package_config_port"], "8080");
    assert_eq!(
        env["npm_package_json"],
        d.join("package.json").display().to_string()
    );
    assert_eq!(env["npm_config_local_prefix"], p.display().to_string());
    assert!(
        env["npm_config_user_agent"].starts_with("fnpm/")
            || std::env::var("npm_config_user_agent").is_ok()
    );
    assert!(env.contains_key("npm_execpath"));
    assert_eq!(env["FNPM_TEST_EXTRA"], "yes");
    let expected_init = std::env::var("INIT_CWD").unwrap_or_else(|_| p.display().to_string());
    assert_eq!(env["INIT_CWD"], expected_init);
    if std::thread::available_parallelism().map_or(1, |n| n.get()) > 2
        && std::env::var("JOBS").is_err()
    {
        assert!(env["JOBS"].parse::<usize>().unwrap() > 2);
    }
    let path: Vec<&str> = env["PATH"].split(':').collect();
    assert_eq!(path[0], d.join("node_modules/.bin").display().to_string());
    assert!(
        path.iter().any(|e| e.contains("fnpm-node-gyp-")),
        "node-gyp shim on PATH"
    );
    let own = path
        .iter()
        .position(|e| *e == p.join("node_modules/.bin").display().to_string())
        .unwrap();
    assert!(
        own > 1,
        "the project's .bin comes after the inherited PATH for a dependency"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_scripts_find_the_project_bins_first() {
    let (_t, p) = project();
    let bin = p.join("node_modules/.bin/hello-bin");
    write(
        &bin,
        &format!("#!/bin/sh\necho from-bin > {}/bin.txt\n", p.display()),
    );
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut root = job("foo", &p, &[("postinstall", "hello-bin")]);
    root.foreground = true;
    run_jobs(vec![vec![root]], &opts(&p), &NoopSink)
        .await
        .unwrap();
    assert_eq!(read(&p.join("bin.txt")).trim(), "from-bin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn npm_run_in_scripts_is_rewritten_to_fnpm() {
    let (_t, p) = project();
    let fake = p.join("node_modules/.bin/fnpm");
    write(
        &fake,
        &format!("#!/bin/sh\necho \"$@\" > {}/fnpm-args.txt\n", p.display()),
    );
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut root = job("foo", &p, &[("prepare", "npm run build -w pkg")]);
    root.foreground = true;
    run_jobs(vec![vec![root]], &opts(&p), &NoopSink)
        .await
        .unwrap();
    assert_eq!(
        read(&p.join("fnpm-args.txt")).trim(),
        "run --filter=pkg build"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_gyp_is_always_available() {
    let (_t, p) = project();
    let d = dep(&p, "native");
    let fake_gyp = p.join("fake gyp.sh");
    write(
        &fake_gyp,
        "#!/bin/sh\nprintf '<%s>\\n' \"$@\" > \"$PWD/gyp.txt\"\n",
    );
    std::fs::set_permissions(
        &fake_gyp,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let mut o = opts(&p);
    o.env
        .push(("npm_config_node_gyp".into(), fake_gyp.display().to_string()));
    let j = job(
        "native",
        &d,
        &[(
            "install",
            "node-gyp rebuild --release 'argument with spaces' '' '*.gyp'",
        )],
    );
    run_jobs(vec![vec![j]], &o, &NoopSink).await.unwrap();
    assert_eq!(
        read(&d.join("gyp.txt")),
        "<rebuild>\n<--release>\n<argument with spaces>\n<>\n<*.gyp>\n"
    );
    // The shim directory is removed afterwards.
    let leftover = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("fnpm-node-gyp-")
        })
        .filter(|e| {
            std::fs::read_to_string(e.path().join("node-gyp"))
                .is_ok_and(|s| s.contains(&*fake_gyp.display().to_string()))
        })
        .count();
    assert_eq!(leftover, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_0_in_lifecycle_scripts_works() {
    let (_t, p) = project();
    let d = dep(&p, "exits");
    let j = job(
        "exits",
        &d,
        &[
            ("preinstall", "exit 0"),
            ("postinstall", "echo ran > post.txt"),
        ],
    );
    run_jobs(vec![vec![j]], &opts(&p), &NoopSink).await.unwrap();
    assert_eq!(read(&d.join("post.txt")).trim(), "ran");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_lifecycle_script_fails_the_install() {
    let (_t, p) = project();
    let d = dep(&p, "fails");
    let j = job(
        "fails",
        &d,
        &[
            ("preinstall", "echo out; echo err 1>&2; exit 3"),
            ("postinstall", "echo no > post.txt"),
        ],
    );
    let after = dep(&p, "after");
    let k = job("after", &after, &[("postinstall", "echo no > ran.txt")]);
    let sink = VecSink::default();
    let mut o = opts(&p);
    o.silent = true;
    let err = run_dag(vec![j, k], vec![vec![], vec![0]], &o, &sink)
        .await
        .unwrap_err();
    match &err {
        Error::Failed {
            package,
            script,
            code,
        } => {
            assert_eq!(
                (package.as_str(), script.as_str(), *code),
                ("fails", "preinstall", 3)
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        err.to_string(),
        "preinstall script from \"fails\" exited with 3"
    );
    assert!(
        !d.join("post.txt").exists(),
        "the next script of the chain does not start"
    );
    assert!(!after.join("ran.txt").exists(), "dependents do not start");
    let events = sink.0.into_inner().unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Script {
            phase: ScriptPhase::End,
            exit_code: Some(3),
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_optional_dependency_is_tolerated() {
    let (_t, p) = project();
    let d = dep(&p, "opt");
    let mut j = job("opt", &d, &[("postinstall", "exit 1")]);
    j.optional = true;
    let other = dep(&p, "other");
    let k = job("other", &other, &[("postinstall", "echo ok > ok.txt")]);
    let report = run_dag(vec![j, k], vec![vec![], vec![0]], &opts(&p), &NoopSink)
        .await
        .unwrap();
    assert_eq!(report.failed_optional, ["opt"]);
    assert_eq!(read(&other.join("ok.txt")).trim(), "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signal_fails_even_an_optional_dependency() {
    let (_t, p) = project();
    let d = dep(&p, "killed");
    let mut j = job("killed", &d, &[("postinstall", "kill -TERM $$")]);
    j.optional = true;
    let mut o = opts(&p);
    o.silent = true;
    let err = run_jobs(vec![vec![j]], &o, &NoopSink).await.unwrap_err();
    assert!(matches!(err, Error::Signaled { signal: 15, .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "postinstall script from \"killed\" terminated by SIGTERM"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reach_max_concurrent_scripts() {
    let (_t, p) = project();
    let running = p.join("running");
    std::fs::create_dir_all(&running).unwrap();
    let script = format!(
        "touch {r}/r.$$; sleep 0.25; ls {r} | wc -l | tr -d \" \" > count.txt; sleep 0.25; rm {r}/r.$$",
        r = running.display()
    );
    let dirs: Vec<PathBuf> = (0..4).map(|i| dep(&p, &format!("dep{i}"))).collect();
    let jobs: Vec<Job> = dirs
        .iter()
        .map(|d| job("d", d, &[("preinstall", &script)]))
        .collect();
    let mut o = opts(&p);
    o.concurrency = 2;
    let report = run_jobs(vec![jobs], &o, &NoopSink).await.unwrap();
    assert_eq!(report.ran, 4);
    for d in &dirs {
        let n: usize = read(&d.join("count.txt")).trim().parse().unwrap();
        assert!((1..=2).contains(&n), "ran alongside {} others", n - 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dependency_cycle_still_runs() {
    let (_t, p) = project();
    let a = dep(&p, "a");
    let b = dep(&p, "b");
    let jobs = vec![
        job("a", &a, &[("postinstall", "touch x")]),
        job("b", &b, &[("postinstall", "touch x")]),
    ];
    let report = run_dag(jobs, vec![vec![1], vec![0]], &opts(&p), &NoopSink)
        .await
        .unwrap();
    assert_eq!(report.ran, 2);
    assert!(a.join("x").exists() && b.join("x").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_to_run() {
    let (_t, p) = project();
    let report = run_jobs(vec![vec![job("x", &p, &[])], vec![]], &opts(&p), &NoopSink)
        .await
        .unwrap();
    assert_eq!(report, fnpm_scripts::Report::default());
    let report = run_dag(vec![], vec![], &opts(&p), &NoopSink).await.unwrap();
    assert_eq!(report.ran, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_shell_and_missing_shell() {
    let (_t, p) = project();
    let d = dep(&p, "s");
    let mut o = opts(&p);
    o.shell = Some("sh".into());
    run_jobs(
        vec![vec![job("s", &d, &[("postinstall", "echo $0 > sh.txt")])]],
        &o,
        &NoopSink,
    )
    .await
    .unwrap();
    assert!(read(&d.join("sh.txt")).trim().ends_with("sh"));
    o.shell = Some("no-such-shell-fnpm".into());
    let err = run_jobs(
        vec![vec![job("s", &d, &[("postinstall", "true")])]],
        &o,
        &NoopSink,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Spawn { .. }));
}

#[test]
fn concurrency_parsing() {
    let default = fnpm_scripts::default_concurrency();
    assert!(default >= 2 && default.is_multiple_of(2));
    assert_eq!(fnpm_scripts::parse_concurrency("3"), 3);
    assert_eq!(fnpm_scripts::parse_concurrency("0"), default);
    assert_eq!(fnpm_scripts::parse_concurrency("abc"), default);
    assert_eq!(fnpm_scripts::parse_concurrency("-1"), default);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_scripts_run_in_the_background_with_the_project_path() {
    // bun runs only the root in the foreground; a workspace is not a dependency, so its PATH
    // keeps the inherited PATH last.
    let (_t, p) = project();
    let ws = p.join("packages/ws");
    write_pkg(&ws, json!({"name": "ws", "version": "1.0.0"}));
    let j = job("ws", &ws, &[("postinstall", "echo \"$PATH\" > path.txt")]);
    assert!(!j.foreground);
    run_jobs(vec![vec![j]], &opts(&p), &NoopSink).await.unwrap();
    let path = read(&ws.join("path.txt"));
    let parts: Vec<&str> = path.trim().split(':').collect();
    let own = parts
        .iter()
        .position(|e| *e == ws.join("node_modules/.bin").display().to_string())
        .unwrap();
    let project_bin = parts
        .iter()
        .position(|e| *e == p.join("node_modules/.bin").display().to_string())
        .unwrap();
    let inherited = parts.iter().position(|e| *e == "/bin" || *e == "/usr/bin");
    assert_eq!(own, 0);
    assert!(project_bin < 3, "{path}");
    if let Some(inherited) = inherited {
        assert!(project_bin < inherited, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nul_bytes_in_package_json_strings_do_not_break_the_spawn() {
    let (_t, p) = project();
    let d = p.join("node_modules/nul-dep");
    write(
        &d.join("package.json"),
        r#"{"name": "nul-dep", "version": "1.0.0", "config": {"x": "a\u0000b"}}"#,
    );
    let j = job("nul-dep", &d, &[("postinstall", "env > env.txt")]);
    run_jobs(vec![vec![j]], &opts(&p), &NoopSink).await.unwrap();
    let env = env_file(&d.join("env.txt"));
    assert_eq!(env["npm_package_config_x"], "a");
}
