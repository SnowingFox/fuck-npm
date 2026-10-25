//! End-to-end install pipeline tests against the in-process registry.

mod common;

use common::{Pkg, Project, Registry, exists};
use fnpm_core::options::InstallCommand;
use fnpm_types::DependencyGroup;
use serde_json::json;

fn basic() -> Vec<Pkg> {
    vec![
        Pkg::new("a", "1.0.0").dep("b", "^1.0.0"),
        Pkg::new("a", "1.1.0").dep("b", "^1.0.0"),
        Pkg::new("a", "2.0.0"),
        Pkg::new("b", "1.0.0"),
        Pkg::new("b", "1.2.0"),
        Pkg::new("tool", "1.0.0").bin("tool", "bin/tool.js"),
        Pkg::new("@scope/pkg", "3.0.0").dep("b", "1.0.0"),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_install_resolves_fetches_links_and_saves() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "name": "app", "dependencies": { "a": "^1.0.0", "tool": "1" } }),
    );
    let s = p.install().await;
    assert!(!s.noop);
    assert_eq!(p.installed("a").as_deref(), Some("1.1.0"));
    assert_eq!(p.installed("b").as_deref(), Some("1.2.0"));
    assert_eq!(p.installed("tool").as_deref(), Some("1.0.0"));
    assert!(exists(&p.path("node_modules/.bin/tool")), "bin linked");
    assert_eq!(s.installed_count, 3);
    let names: Vec<_> = s
        .added
        .iter()
        .map(|a| (a.name.as_str(), a.version.as_str()))
        .collect();
    assert_eq!(names, vec![("a", "1.1.0"), ("tool", "1.0.0")]);
    let lock = p.read("fnpm.lock");
    assert!(lock.contains("\"a\": [\"a@1.1.0\""), "{lock}");
    assert!(lock.contains("sha512-"), "integrity recorded: {lock}");
    assert!(
        exists(&p.path(fnpm_core::fast_path::STATE_FILE)),
        "state written"
    );
    assert!(
        exists(&p.path("node_modules/.fnpm/lock.bin")),
        "sidecar written"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_install_is_a_noop_and_fast() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^1.0.0" } }));
    p.install().await;
    reg.clear_seen();
    let started = std::time::Instant::now();
    let s = p.install().await;
    let took = started.elapsed();
    assert!(s.noop, "{s:?}");
    assert_eq!(s.checked.unwrap().packages, 3);
    assert!(reg.seen().is_empty(), "no requests: {:?}", reg.seen());
    eprintln!("no-op install took {took:?}");

    // Touching package.json without changing it is still a no-op (hash compare).
    let text = p.read("package.json");
    p.write("package.json", &text);
    assert!(p.install().await.noop);

    // Removing a package from node_modules is not.
    std::fs::remove_dir_all(p.path("node_modules/b")).unwrap();
    let s = p.install().await;
    assert!(!s.noop);
    assert_eq!(p.installed("b").as_deref(), Some("1.2.0"));
    assert_eq!(
        reg.manifest_requests(),
        0,
        "lockfile reused: {:?}",
        reg.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_store_install_downloads_nothing() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^1.0.0" } }));
    p.install().await;
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    reg.clear_seen();
    let s = p.install().await;
    assert_eq!(s.installed_count, 2);
    assert_eq!(reg.seen().len(), 0, "{:?}", reg.seen());
    assert_eq!(p.installed("a").as_deref(), Some("1.1.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frozen_lockfile_and_ci() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^1.0.0" } }));
    // No lockfile: frozen fails.
    let mut o = p.opts(InstallCommand::Install, &[]);
    o.pm.frozen_lockfile = true;
    let err = p.run(o.clone()).await.unwrap_err();
    assert_eq!(err.code(), "FROZEN_LOCKFILE", "{err}");

    p.install().await;
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    reg.clear_seen();
    let lock_before = p.read("fnpm.lock");
    let (s, _) = p.run(o.clone()).await.unwrap();
    assert_eq!(s.installed_count, 2);
    assert_eq!(
        reg.manifest_requests(),
        0,
        "frozen installs read no manifests"
    );
    assert_eq!(p.read("fnpm.lock"), lock_before);

    // A changed package.json is a frozen error and changes nothing.
    p.write(
        "package.json",
        "{ \"dependencies\": { \"a\": \"^1.0.0\", \"b\": \"1.0.0\" } }\n",
    );
    let err = p.run(o).await.unwrap_err();
    assert_eq!(err.code(), "FROZEN_LOCKFILE");
    assert_eq!(p.read("fnpm.lock"), lock_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_writes_package_json_like_bun() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "name": "app" }));
    // Four-space indentation and no trailing newline must survive.
    p.write("package.json", "{\n    \"name\": \"app\"\n}");
    let (s, _) = p.run(p.opts(InstallCommand::Add, &["a"])).await.unwrap();
    assert_eq!(s.requested.len(), 1);
    assert_eq!(s.requested[0].version, "2.0.0");
    let text = p.read("package.json");
    assert_eq!(
        text,
        "{\n    \"name\": \"app\",\n    \"dependencies\": {\n        \"a\": \"^2.0.0\"\n    }\n}"
    );
    assert_eq!(p.installed("a").as_deref(), Some("2.0.0"));

    let mut o = p.opts(InstallCommand::Add, &["b@1.0.0", "tool"]);
    o.group = Some(DependencyGroup::Dev);
    o.exact = true;
    let (s, _) = p.run(o).await.unwrap();
    let pj = p.json("package.json");
    assert_eq!(pj["devDependencies"]["b"], "1.0.0");
    assert_eq!(pj["devDependencies"]["tool"], "1.0.0");
    let tool = s.requested.iter().find(|r| r.name == "tool").unwrap();
    assert_eq!(tool.bins, vec!["tool".to_owned()]);

    // The lockfile matches package.json: a frozen install afterwards succeeds.
    let mut o = p.opts(InstallCommand::Install, &[]);
    o.pm.frozen_lockfile = true;
    p.run(o).await.expect("lockfile in sync after add");

    // A scoped package with a range keeps the range as typed.
    p.run(p.opts(InstallCommand::Add, &["@scope/pkg@^3"]))
        .await
        .unwrap();
    assert_eq!(p.json("package.json")["dependencies"]["@scope/pkg"], "^3");
    assert_eq!(p.installed("@scope/pkg").as_deref(), Some("3.0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_creates_package_json_and_only_missing() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({}));
    std::fs::remove_file(p.path("package.json")).unwrap();
    p.run(p.opts(InstallCommand::Add, &["b@1.0.0"]))
        .await
        .unwrap();
    assert_eq!(p.json("package.json")["dependencies"]["b"], "1.0.0");
    let mut o = p.opts(InstallCommand::Add, &["b"]);
    o.only_missing = true;
    p.run(o).await.unwrap();
    assert_eq!(p.json("package.json")["dependencies"]["b"], "1.0.0", "kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_drops_dependency_and_files() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "a": "^1.0.0", "tool": "1.0.0" } }),
    );
    p.install().await;
    let (s, _) = p.run(p.opts(InstallCommand::Remove, &["a"])).await.unwrap();
    assert_eq!(s.removed, vec!["a".to_owned()]);
    assert!(p.json("package.json")["dependencies"].get("a").is_none());
    assert!(!exists(&p.path("node_modules/a")));
    assert!(!exists(&p.path("node_modules/b")), "transitive removed too");
    assert!(!p.read("fnpm.lock").contains("\"a@"));
    assert_eq!(p.installed("tool").as_deref(), Some("1.0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_moves_within_range_and_latest() {
    let reg = Registry::start(&[Pkg::new("a", "1.0.0"), Pkg::new("b", "1.0.0")]).await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "a": "^1.0.0", "b": "~1.0.0" } }),
    );
    p.install().await;
    assert_eq!(p.installed("a").as_deref(), Some("1.0.0"));
    reg.publish(&Pkg::new("a", "1.5.0"));
    reg.publish(&Pkg::new("a", "2.0.0"));
    reg.publish(&Pkg::new("b", "1.0.3"));

    // A plain install keeps the lockfile.
    p.install().await;
    assert_eq!(p.installed("a").as_deref(), Some("1.0.0"));

    let (s, _) = p.run(p.opts(InstallCommand::Update, &[])).await.unwrap();
    assert_eq!(p.installed("a").as_deref(), Some("1.5.0"));
    assert_eq!(p.installed("b").as_deref(), Some("1.0.3"));
    let pj = p.json("package.json");
    assert_eq!(pj["dependencies"]["a"], "^1.5.0");
    assert_eq!(pj["dependencies"]["b"], "~1.0.3");
    assert!(
        s.updated
            .iter()
            .any(|u| u.name == "a" && u.from == "1.0.0" && u.to == "1.5.0"),
        "{s:?}"
    );

    let mut o = p.opts(InstallCommand::Update, &["a"]);
    o.latest = true;
    p.run(o).await.unwrap();
    assert_eq!(p.installed("a").as_deref(), Some("2.0.0"));
    assert_eq!(p.json("package.json")["dependencies"]["a"], "^2.0.0");
    assert_eq!(p.installed("b").as_deref(), Some("1.0.3"), "untouched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspaces_link_members_and_install_their_deps() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "name": "root", "workspaces": ["packages/*"], "dependencies": { "lib": "workspace:*" } }),
    );
    p.write(
        "packages/lib/package.json",
        &json!({ "name": "lib", "version": "0.1.0", "dependencies": { "a": "1.0.0" } }).to_string(),
    );
    p.write("packages/lib/index.js", "module.exports = 1;\n");
    let s = p.install().await;
    assert!(s.installed_count >= 2, "{s:?}");
    let lib = std::fs::canonicalize(p.path("node_modules/lib")).unwrap();
    assert_eq!(lib, std::fs::canonicalize(p.path("packages/lib")).unwrap());
    // `a` is reachable from the member (isolated: through its own node_modules).
    let a = p.path("packages/lib/node_modules/a/package.json");
    let hoisted = p.path("node_modules/a/package.json");
    assert!(a.exists() || hoisted.exists(), "a installed for lib");
    let lock = p.read("fnpm.lock");
    assert!(lock.contains("\"packages/lib\""), "{lock}");
    assert!(p.install().await.noop);

    // `add` inside the member edits the member's package.json.
    let mut o = p.opts(InstallCommand::Add, &["b@1.0.0"]);
    o.pm.cwd = p.path("packages/lib");
    p.run(o).await.unwrap();
    assert_eq!(
        p.json("packages/lib/package.json")["dependencies"]["b"],
        "1.0.0"
    );
    assert!(p.json("package.json")["dependencies"].get("b").is_none());

    // --filter on add.
    let mut o = p.opts(InstallCommand::Add, &["tool"]);
    o.filter = vec!["lib".into()];
    p.run(o).await.unwrap();
    assert_eq!(
        p.json("packages/lib/package.json")["dependencies"]["tool"],
        "^1.0.0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrates_package_lock_keeping_versions() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "name": "app", "dependencies": { "a": "^1.0.0" } }),
    );
    let lock = json!({
        "name": "app",
        "lockfileVersion": 3,
        "requires": true,
        "packages": {
            "": { "name": "app", "dependencies": { "a": "^1.0.0" } },
            "node_modules/a": {
                "version": "1.0.0",
                "resolved": reg.tarball_url("a", "1.0.0"),
                "integrity": reg.integrity("a", "1.0.0"),
                "dependencies": { "b": "^1.0.0" }
            },
            "node_modules/b": {
                "version": "1.0.0",
                "resolved": reg.tarball_url("b", "1.0.0"),
                "integrity": reg.integrity("b", "1.0.0")
            }
        }
    });
    p.write(
        "package-lock.json",
        &serde_json::to_string_pretty(&lock).unwrap(),
    );
    p.install().await;
    assert_eq!(
        p.installed("a").as_deref(),
        Some("1.0.0"),
        "migrated versions kept"
    );
    assert_eq!(p.installed("b").as_deref(), Some("1.0.0"));
    assert!(p.read("fnpm.lock").contains("a@1.0.0"));
    assert!(
        exists(&p.path("package-lock.json")),
        "foreign lockfile left in place"
    );
    assert_eq!(reg.manifest_requests(), 0, "{:?}", reg.seen());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broken_foreign_lockfile_falls_through_to_the_next() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "name": "app", "dependencies": { "b": "1.0.0" } }),
    );
    p.write("package-lock.json", "{ not json");
    p.write(
        "yarn.lock",
        &format!(
            "# yarn lockfile v1\n\n\nb@1.0.0:\n  version \"1.0.0\"\n  resolved \"{}\"\n  integrity {}\n",
            reg.tarball_url("b", "1.0.0"),
            reg.integrity("b", "1.0.0")
        ),
    );
    let (_, events) = p.run(p.opts(InstallCommand::Install, &[])).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(e, fnpm_types::Event::Warn { message, .. } if message.contains("package-lock.json") && message.contains("yarn.lock"))),
        "{events:?}"
    );
    assert_eq!(p.installed("b").as_deref(), Some("1.0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lockfile_only_and_dry_run() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^1.0.0" } }));
    let mut o = p.opts(InstallCommand::Install, &[]);
    o.pm.dry_run = true;
    p.run(o).await.unwrap();
    assert!(!exists(&p.path("fnpm.lock")));
    assert!(!exists(&p.path("node_modules")));

    let mut o = p.opts(InstallCommand::Install, &[]);
    o.pm.lockfile_only = true;
    p.run(o).await.unwrap();
    assert!(exists(&p.path("fnpm.lock")));
    assert!(!exists(&p.path("node_modules/a")));
    assert_eq!(reg.tarball_requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_omits_dev_dependencies() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "b": "1.0.0" }, "devDependencies": { "tool": "1.0.0" } }),
    );
    p.install().await;
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    let mut o = p.opts(InstallCommand::Install, &[]);
    o.pm.production = true;
    p.run(o).await.unwrap();
    assert_eq!(p.installed("b").as_deref(), Some("1.0.0"));
    assert!(!exists(&p.path("node_modules/tool")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scripts_root_runs_untrusted_is_blocked() {
    let reg = Registry::start(&[
        Pkg::new("native", "1.0.0").script("postinstall", "echo built > built.txt")
    ])
    .await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "native": "1.0.0" }, "scripts": { "postinstall": "echo ok > root-postinstall.txt" } }),
    );
    let s = p.install().await;
    assert_eq!(s.blocked_scripts, 1, "{s:?}");
    assert!(exists(&p.path("root-postinstall.txt")), "root scripts run");
    assert!(!exists(&p.path("node_modules/native/built.txt")));

    // Trusted by package.json: runs.
    p.write(
        "package.json",
        &json!({ "dependencies": { "native": "1.0.0" }, "trustedDependencies": ["native"] })
            .to_string(),
    );
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    let s = p.install().await;
    assert_eq!(s.blocked_scripts, 0);
    assert!(exists(&p.path("node_modules/native/built.txt")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_required_script_is_script_failed() {
    let reg = Registry::start(&[Pkg::new("bad", "1.0.0").script("postinstall", "exit 3")]).await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "bad": "1.0.0" }, "trustedDependencies": ["bad"] }),
    );
    let err = p
        .run(p.opts(InstallCommand::Install, &[]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "SCRIPT_FAILED", "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_package_is_not_found() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "nope": "1.0.0" } }));
    let err = p
        .run(p.opts(InstallCommand::Install, &[]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND", "{err}");
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^9" } }));
    let err = p
        .run(p.opts(InstallCommand::Install, &[]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "NO_MATCHING_VERSION", "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_add_links_bins_into_the_global_bin_dir() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({}));
    let mut o = p.opts(InstallCommand::Add, &["tool"]);
    o.pm.global = true;
    p.run(o).await.unwrap();
    let global = p.tmp.path().join("global");
    let pj: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(global.join("package.json")).unwrap())
            .unwrap();
    assert_eq!(pj["dependencies"]["tool"], "^1.0.0");
    assert!(exists(&p.tmp.path().join("global-bin/tool")), "global bin");
    assert!(!exists(&p.path("node_modules")), "project untouched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn optional_platform_mismatch_is_skipped() {
    let reg = Registry::start(&[
        Pkg::new("host", "1.0.0").optional("only-aix", "1.0.0"),
        Pkg::new("only-aix", "1.0.0").os(&["aix"]),
    ])
    .await;
    let p = Project::new(&reg, json!({ "dependencies": { "host": "1.0.0" } }));
    p.install().await;
    assert_eq!(p.installed("host").as_deref(), Some("1.0.0"));
    assert!(!exists(&p.path("node_modules/only-aix")));
    assert!(
        p.read("fnpm.lock").contains("only-aix"),
        "kept in the lockfile"
    );
    let downloaded = |reg: &Registry| {
        reg.seen()
            .iter()
            .any(|r| r.contains("only-aix") && r.ends_with(".tgz"))
    };
    assert!(
        !downloaded(&reg),
        "never download another platform's package"
    );
    // Not on a reinstall from the lockfile either.
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    p.install().await;
    assert!(!downloaded(&reg));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_folder_dependency() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "local": "file:./vendor/local" } }),
    );
    p.write(
        "vendor/local/package.json",
        &json!({ "name": "local", "version": "0.0.1", "dependencies": { "b": "1.0.0" } })
            .to_string(),
    );
    p.write("vendor/local/index.js", "module.exports = 2;\n");
    p.install().await;
    assert_eq!(p.installed("local").as_deref(), Some("0.0.1"));
    assert_eq!(p.installed("b").as_deref(), Some("1.0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_answers_a_noop_without_a_runtime() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "b": "1.0.0" } }));
    p.install().await;
    let (opts, env) = (p.opts(InstallCommand::Install, &[]), p.env());
    // A plain thread: no tokio context at all.
    let prepared = std::thread::spawn(move || fnpm_core::prepare_install(opts, env).unwrap())
        .join()
        .unwrap();
    let noop = prepared.noop().expect("no-op");
    assert_eq!(noop.checked.unwrap().packages, 2);
    // Not a no-op: `run` continues the pipeline.
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    let prepared =
        fnpm_core::prepare_install(p.opts(InstallCommand::Install, &[]), p.env()).unwrap();
    assert!(prepared.noop().is_none());
    let s = prepared
        .run(std::sync::Arc::new(fnpm_types::NoopSink))
        .await
        .unwrap();
    assert_eq!(s.installed_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parsed_lockfile_survives_node_modules_removal() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^1.0.0" } }));
    p.install().await;
    let sidecar = p.path("node_modules/.fnpm/lock.bin");
    assert!(exists(&sidecar));
    let cached: Vec<_> = std::fs::read_dir(p.tmp.path().join("fnpm-home/cache/lockfile-sidecars"))
        .expect("cached sidecar dir")
        .flatten()
        .collect();
    assert_eq!(cached.len(), 1);
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    let s = p.install().await;
    assert_eq!(s.installed_count, 2);
    assert!(exists(&sidecar), "restored from the cached copy");
    assert!(p.install().await.noop);
    // A changed lockfile never uses a stale copy.
    let lock = p.read("fnpm.lock").replace("\"^1.0.0\"", "\"^1.0.0\" ");
    p.write("fnpm.lock", &lock);
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    p.install().await;
    assert_eq!(p.installed("a").as_deref(), Some("1.1.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tampered_tarball_is_an_integrity_error() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "b": "1.0.0" } }));
    p.install().await;
    // Same version, different bytes; the lockfile keeps the original integrity.
    let mut evil = Pkg::new("b", "1.0.0");
    evil.files.push(("evil.js".into(), "pwned".into()));
    reg.publish(&evil);
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    std::fs::remove_dir_all(p.tmp.path().join("fnpm-home/store")).unwrap();
    let mut o = p.opts(InstallCommand::Install, &[]);
    o.pm.frozen_lockfile = true;
    let err = p.run(o).await.unwrap_err();
    assert_eq!(err.code(), "INTEGRITY", "{err}");
    assert!(!exists(&p.path("node_modules/b/evil.js")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hoisted_workspaces() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "name": "root", "workspaces": ["packages/*"], "dependencies": { "lib": "workspace:*", "b": "1.0.0" } }),
    );
    p.write(
        "packages/lib/package.json",
        &json!({ "name": "lib", "version": "0.1.0", "dependencies": { "b": "1.2.0" } }).to_string(),
    );
    let mut o = p.opts(InstallCommand::Install, &[]);
    o.pm.linker = Some(fnpm_config::LinkerSetting::Hoisted);
    p.run(o.clone()).await.unwrap();
    assert_eq!(p.installed("b").as_deref(), Some("1.0.0"));
    let lib = std::fs::canonicalize(p.path("node_modules/lib")).unwrap();
    assert_eq!(lib, std::fs::canonicalize(p.path("packages/lib")).unwrap());
    let nested = p.path("packages/lib/node_modules/b/package.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(nested).unwrap()).unwrap();
    assert_eq!(
        v["version"], "1.2.0",
        "conflicting version under the workspace"
    );
    assert!(
        !exists(&p.path("node_modules/.fnpm/node_modules")),
        "hoisted layout"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_remove_unlinks_bins() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({}));
    let mut o = p.opts(InstallCommand::Add, &["tool"]);
    o.pm.global = true;
    p.run(o).await.unwrap();
    let bin = p.tmp.path().join("global-bin/tool");
    assert!(exists(&bin));
    let mut o = p.opts(InstallCommand::Remove, &["tool"]);
    o.pm.global = true;
    p.run(o).await.unwrap();
    assert!(!exists(&bin), "bin unlinked");
    assert!(!exists(&p.tmp.path().join("global/node_modules/tool")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_workspace_member_is_not_a_noop() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "name": "root", "workspaces": ["packages/*"] }),
    );
    p.write(
        "packages/one/package.json",
        &json!({ "name": "one", "version": "1.0.0" }).to_string(),
    );
    p.install().await;
    assert!(p.install().await.noop);

    // A new member under the glob changes no manifest the last install saw.
    p.write(
        "packages/two/package.json",
        &json!({ "name": "two", "version": "1.0.0", "dependencies": { "b": "1.0.0" } }).to_string(),
    );
    let s = p.install().await;
    assert!(!s.noop, "a new member must be installed");
    assert!(p.read("fnpm.lock").contains("\"packages/two\""));
    assert!(p.install().await.noop);

    // A matched directory that gains a package.json is a new member too.
    std::fs::create_dir_all(p.path("packages/three")).unwrap();
    let s = p.install().await;
    assert!(!s.noop, "a new directory under the glob is a change");
    assert!(p.install().await.noop);
    p.write(
        "packages/three/package.json",
        &json!({ "name": "three", "version": "1.0.0" }).to_string(),
    );
    assert!(!p.install().await.noop);
    assert!(p.read("fnpm.lock").contains("\"packages/three\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edited_patch_file_is_not_a_noop() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "b": "1.0.0" } }));
    p.install().await;
    p.write("patches/b.patch", "");
    let mut pj = p.json("package.json");
    pj["patchedDependencies"] = json!({ "b@1.0.0": "patches/b.patch" });
    p.write("package.json", &serde_json::to_string_pretty(&pj).unwrap());
    p.install().await;
    assert!(p.read("fnpm.lock").contains("patches/b.patch"));
    assert!(p.install().await.noop);
    p.write("patches/b.patch", "\n");
    assert!(
        !p.install().await.noop,
        "an edited patch file must reinstall"
    );
}

fn patch_for(text: &str) -> String {
    format!(
        "diff --git a/index.js b/index.js\n--- a/index.js\n+++ b/index.js\n@@ -1 +1 @@\n-module.exports = \"b@1.0.0\";\n+module.exports = \"{text}\";\n"
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patched_dependency_follows_the_patch_file() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({
            "dependencies": { "b": "1.0.0" },
            "patchedDependencies": { "b@1.0.0": "patches/b@1.0.0.patch" }
        }),
    );
    p.write("patches/b@1.0.0.patch", &patch_for("one"));
    p.install().await;
    assert_eq!(
        p.read("node_modules/b/index.js"),
        "module.exports = \"one\";\n"
    );
    assert!(p.install().await.noop);

    p.write("patches/b@1.0.0.patch", &patch_for("two"));
    let s = p.install().await;
    assert!(!s.noop);
    assert_eq!(
        p.read("node_modules/b/index.js"),
        "module.exports = \"two\";\n",
        "the edited patch is applied"
    );

    // Dropping the patch reinstalls the package unpatched.
    p.write(
        "package.json",
        &json!({ "dependencies": { "b": "1.0.0" } }).to_string(),
    );
    p.install().await;
    assert_eq!(
        p.read("node_modules/b/index.js"),
        "module.exports = \"b@1.0.0\";\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patched_dependency_follows_the_patch_file_isolated() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({
            "dependencies": { "b": "1.0.0" },
            "patchedDependencies": { "b@1.0.0": "patches/b@1.0.0.patch" }
        }),
    );
    let install = || {
        let mut o = p.opts(InstallCommand::Install, &[]);
        o.pm.linker = Some(fnpm_config::LinkerSetting::Isolated);
        p.run(o)
    };
    p.write("patches/b@1.0.0.patch", &patch_for("one"));
    install().await.unwrap();
    assert_eq!(
        p.read("node_modules/b/index.js"),
        "module.exports = \"one\";\n"
    );
    p.write("patches/b@1.0.0.patch", &patch_for("two"));
    install().await.unwrap();
    assert_eq!(
        p.read("node_modules/b/index.js"),
        "module.exports = \"two\";\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependencies_of_another_platforms_package_are_not_downloaded() {
    // sharp's shape: an optional per-platform package whose own dependency has no `os`.
    let reg = Registry::start(&[
        Pkg::new("host", "1.0.0").optional("only-aix", "1.0.0"),
        Pkg::new("only-aix", "1.0.0")
            .os(&["aix"])
            .dep("aix-runtime", "1.0.0"),
        Pkg::new("aix-runtime", "1.0.0"),
    ])
    .await;
    let p = Project::new(&reg, json!({ "dependencies": { "host": "1.0.0" } }));
    let downloaded = |reg: &Registry| {
        reg.seen()
            .iter()
            .filter(|r| {
                r.ends_with(".tgz") && (r.contains("aix-runtime") || r.contains("only-aix"))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    p.install().await;
    assert!(!exists(&p.path("node_modules/aix-runtime")));
    assert_eq!(downloaded(&reg), Vec::<String>::new(), "fresh install");
    // A cold install from the lockfile (empty store) neither.
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    std::fs::remove_dir_all(p.tmp.path().join("fnpm-home")).unwrap();
    reg.clear_seen();
    let s = p.install().await;
    assert_eq!(s.installed_count, 1, "{s:?}");
    assert_eq!(downloaded(&reg), Vec::<String>::new(), "from the lockfile");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_after_an_install_downloads_and_places_only_the_new_package() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^1.0.0" } }));
    p.install().await;
    let before = std::fs::metadata(p.path("node_modules/a/package.json")).unwrap();
    reg.clear_seen();
    let (s, _) = p.run(p.opts(InstallCommand::Add, &["tool"])).await.unwrap();
    assert_eq!(s.installed_count, 1, "{s:?}");
    assert_eq!(p.installed("tool").as_deref(), Some("1.0.0"));
    assert!(exists(&p.path("node_modules/.bin/tool")));
    assert_eq!(reg.tarball_requests(), 1, "{:?}", reg.seen());
    // The packages already in place were not rebuilt.
    let after = std::fs::metadata(p.path("node_modules/a/package.json")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.ino(), after.ino());
    }
    let _ = (before, after);
    // The state is consistent: the next install is a no-op.
    assert!(p.install().await.noop);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_repairs_what_changed_in_node_modules_since_the_install() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^1.0.0" } }));
    p.install().await;
    // A removed package changes node_modules, so nothing is trusted and `b` comes back.
    std::fs::remove_dir_all(p.path("node_modules/b")).unwrap();
    p.run(p.opts(InstallCommand::Add, &["tool"])).await.unwrap();
    assert_eq!(p.installed("b").as_deref(), Some("1.2.0"));
    // A package whose version changes in the graph is rebuilt even with node_modules intact.
    p.run(p.opts(InstallCommand::Add, &["b@1.0.0"]))
        .await
        .unwrap();
    assert_eq!(p.installed("b").as_deref(), Some("1.0.0"));
    p.run(p.opts(InstallCommand::Remove, &["b"])).await.unwrap();
    assert_eq!(
        p.installed("b").as_deref(),
        Some("1.2.0"),
        "a's b is hoisted again"
    );
    assert!(p.install().await.noop);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_repairs_missing_scoped_and_nested_placements() {
    let reg = Registry::start(&basic()).await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "a": "1.0.0", "b": "1.2.0", "@scope/pkg": "3.0.0" } }),
    );
    p.install().await;
    let before = fnpm_core::fast_path::dir_identity(&p.path("node_modules")).unwrap();
    let nested = p.path("node_modules/@scope/pkg/node_modules/b");
    assert!(nested.is_dir(), "conflicting b must be nested");
    std::fs::remove_dir_all(&nested).unwrap();
    assert_eq!(
        before,
        fnpm_core::fast_path::dir_identity(&p.path("node_modules")).unwrap()
    );
    p.run(p.opts(InstallCommand::Add, &["tool"])).await.unwrap();
    assert!(
        nested.join("package.json").is_file(),
        "add repairs missing nested package"
    );

    std::fs::remove_dir_all(p.path("node_modules/@scope/pkg")).unwrap();
    p.run(p.opts(InstallCommand::Remove, &["tool"]))
        .await
        .unwrap();
    assert_eq!(p.installed("@scope/pkg").as_deref(), Some("3.0.0"));
    assert!(
        nested.join("package.json").is_file(),
        "remove repairs missing scoped subtree"
    );
}
