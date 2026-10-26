//! `fnpm_core::invoke` commands against the in-process registry.

mod common;

use common::{Pkg, Project, Registry, exists};
use fnpm_core::options::InstallCommand;
use serde_json::json;

fn pkgs() -> Vec<Pkg> {
    vec![
        Pkg::new("a", "1.0.0").dep("b", "^1.0.0"),
        Pkg::new("b", "1.0.0"),
        Pkg::new("tool", "1.0.0").bin("tool", "bin/tool.js"),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outdated_reports_update_and_latest() {
    let reg = Registry::start(&pkgs()).await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "a": "^1.0.0" }, "devDependencies": { "tool": "~1.0.0" } }),
    );
    p.install().await;
    let r = p.invoke(&["outdated"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.data.as_ref().unwrap()["packages"], json!([]));

    reg.publish(&Pkg::new("a", "1.5.0").dep("b", "^1.0.0"));
    reg.publish(&Pkg::new("a", "2.0.0"));
    reg.publish(&Pkg::new("tool", "1.2.0"));
    let r = p.invoke(&["outdated"], &[], json!({})).await.unwrap();
    let rows = r.data.as_ref().unwrap()["packages"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["name"], "a");
    assert_eq!(rows[0]["current"], "1.0.0");
    assert_eq!(rows[0]["update"], "1.5.0");
    assert_eq!(rows[0]["latest"], "2.0.0");
    assert_eq!(rows[0]["dependencyType"], "dependencies");
    assert_eq!(rows[1]["name"], "tool");
    assert_eq!(rows[1]["update"], "1.0.0", "~1.0.0 allows no 1.2.0");
    assert_eq!(rows[1]["latest"], "1.2.0");
    let lines = r.lines.unwrap();
    assert_eq!(lines[1], "| Package    | Current | Update | Latest |");
    assert!(lines.iter().any(|l| l.contains("tool (dev)")), "{lines:#?}");

    // Patterns.
    let r = p.invoke(&["outdated"], &["t*"], json!({})).await.unwrap();
    assert_eq!(r.data.unwrap()["packages"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn why_ls_hash_and_licenses() {
    let reg = Registry::start(&pkgs()).await;
    let p = Project::new(
        &reg,
        json!({ "name": "app", "dependencies": { "a": "^1.0.0" } }),
    );
    p.install().await;
    let r = p.invoke(&["why"], &["b"], json!({})).await.unwrap();
    let lines = r.lines.unwrap();
    assert_eq!(lines[0], "b@1.0.0");
    assert_eq!(lines[1], "  └─ a@1.0.0 (requires ^1.0.0)");
    assert_eq!(lines[2], "     └─ app (requires ^1.0.0)");
    let err = p.invoke(&["why"], &["nope"], json!({})).await.unwrap_err();
    assert_eq!(err.code(), "REPORTED");

    let r = p.invoke(&["ls"], &[], json!({})).await.unwrap();
    let lines = r.lines.unwrap();
    assert!(
        lines[0].ends_with("node_modules (2 installed)"),
        "{lines:?}"
    );
    assert_eq!(lines[1], "└── a@1.0.0");
    let r = p
        .invoke(&["ls"], &[], json!({ "all": true }))
        .await
        .unwrap();
    assert_eq!(r.lines.unwrap()[2], "    └── b@1.0.0");

    let h1 = p
        .invoke(&["pm", "hash"], &[], json!({}))
        .await
        .unwrap()
        .lines
        .unwrap();
    let h2 = p
        .invoke(&["pm", "hash"], &[], json!({}))
        .await
        .unwrap()
        .lines
        .unwrap();
    assert_eq!(h1, h2);
    assert_eq!(h1[0].len(), 67);
    let s = p
        .invoke(&["pm", "hash-string"], &[], json!({}))
        .await
        .unwrap()
        .lines
        .unwrap();
    assert!(s.iter().any(|l| l.contains("b@1.0.0")), "{s:?}");

    let r = p.invoke(&["pm", "licenses"], &[], json!({})).await.unwrap();
    assert!(r.lines.unwrap().iter().any(|l| l.starts_with("Unknown (")));

    let r = p.invoke(&["pm", "bin"], &[], json!({})).await.unwrap();
    assert!(r.lines.unwrap()[0].ends_with("node_modules/.bin"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_reads_the_packument() {
    let reg = Registry::start(&[
        Pkg::new("a", "1.0.0"),
        Pkg::new("a", "1.2.0").dep("b", "^1"),
    ])
    .await;
    let p = Project::new(&reg, json!({}));
    let r = p.invoke(&["info"], &["a"], json!({})).await.unwrap();
    let lines = r.lines.unwrap();
    assert!(
        lines[0].starts_with("a@1.2.0 | Proprietary | deps: 1 | versions: 2"),
        "{lines:?}"
    );
    let r = p
        .invoke(&["info"], &["a@1.0.0", "version"], json!({}))
        .await
        .unwrap();
    assert_eq!(r.lines.unwrap(), vec!["1.0.0"]);
    let r = p
        .invoke(&["info"], &["a", "dist-tags.latest"], json!({}))
        .await
        .unwrap();
    assert_eq!(r.data.unwrap(), json!("1.2.0"));
    let err = p
        .invoke(&["info"], &["missing-pkg"], json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pkg_get_set_delete_keep_formatting() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(&reg, json!({}));
    p.write(
        "package.json",
        "{\n\t\"name\": \"x\",\n\t\"scripts\": {\n\t\t\"a\": \"1\"\n\t}\n}\n",
    );
    let r = p
        .invoke(&["pm", "pkg", "get"], &["name"], json!({}))
        .await
        .unwrap();
    assert_eq!(r.lines.unwrap(), vec!["\"x\""]);
    p.invoke(
        &["pm", "pkg", "set"],
        &["scripts.b=2", "version=1.0.0"],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(
        p.read("package.json"),
        "{\n\t\"name\": \"x\",\n\t\"scripts\": {\n\t\t\"a\": \"1\",\n\t\t\"b\": \"2\"\n\t},\n\t\"version\": \"1.0.0\"\n}\n"
    );
    p.invoke(&["pm", "pkg", "delete"], &["scripts.a"], json!({}))
        .await
        .unwrap();
    assert!(!p.read("package.json").contains("\"a\""));
    let mut req_json = json!({});
    req_json["json"] = json!(true);
    p.invoke(&["pm", "pkg", "set"], &["files=[\"dist\"]"], req_json)
        .await
        .unwrap();
    assert_eq!(p.json("package.json")["files"], json!(["dist"]));
    let err = p
        .invoke(&["pm", "pkg", "set"], &["novalue"], json!({}))
        .await
        .unwrap_err();
    // Said on stderr, like bun.
    assert_eq!(err.code(), "REPORTED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_bump_and_init() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(
        &reg,
        json!({ "name": "x", "version": "1.2.3", "scripts": { "version": "echo $npm_new_version > v.txt" } }),
    );
    let r = p
        .invoke(
            &["pm", "version"],
            &["minor"],
            json!({ "noGitTagVersion": true }),
        )
        .await
        .unwrap();
    assert_eq!(r.lines.unwrap(), vec!["v1.3.0"]);
    assert_eq!(p.json("package.json")["version"], "1.3.0");
    assert_eq!(p.read("v.txt").trim(), "1.3.0");
    let r = p
        .invoke(
            &["pm", "version"],
            &["prerelease"],
            json!({ "preid": "beta", "noGitTagVersion": true }),
        )
        .await
        .unwrap();
    assert_eq!(r.lines.unwrap(), vec!["v1.3.1-beta.0"]);
    let err = p
        .invoke(&["pm", "version"], &["1.3.1-beta.0"], json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "REPORTED");

    let r = p
        .invoke(&["init"], &["sub/My Pkg"], json!({}))
        .await
        .unwrap();
    assert_eq!(r.data.unwrap()["created"], true);
    assert_eq!(p.json("sub/My Pkg/package.json")["name"], "my-pkg");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrate_export_verify_diff_print() {
    let reg = Registry::start(&pkgs()).await;
    let p = Project::new(
        &reg,
        json!({ "name": "app", "dependencies": { "a": "^1.0.0" } }),
    );
    p.install().await;
    let original = p.read("fnpm.lock");

    // Export to npm, delete fnpm.lock, migrate back.
    p.invoke(&["lock", "export"], &[], json!({ "to": "npm" }))
        .await
        .unwrap();
    assert!(exists(&p.path("package-lock.json")));
    std::fs::remove_file(p.path("fnpm.lock")).unwrap();
    let r = p.invoke(&["migrate"], &[], json!({})).await.unwrap();
    assert!(
        r.lines
            .unwrap()
            .last()
            .unwrap()
            .contains("from package-lock.json")
    );
    assert!(p.read("fnpm.lock").contains("\"a@1.0.0\""));
    let err = p
        .invoke(&["lock", "migrate"], &[], json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "USAGE", "refuses to overwrite");
    p.invoke(
        &["lock", "migrate"],
        &[],
        json!({ "force": true, "from": "npm" }),
    )
    .await
    .unwrap();

    let r = p
        .invoke(&["lock", "verify"], &[], json!({ "integrity": true }))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0, "{:?}", r.lines);
    p.write(
        "package.json",
        "{ \"name\": \"app\", \"dependencies\": { \"a\": \"^1.0.0\", \"b\": \"1\" } }\n",
    );
    let r = p.invoke(&["lock", "verify"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 1);

    p.write("old.lock", &original);
    p.write(
        "package.json",
        "{ \"name\": \"app\", \"dependencies\": { \"tool\": \"1.0.0\" } }\n",
    );
    p.install().await;
    p.invoke(
        &["lock", "export"],
        &[],
        json!({ "to": "fnpm", "output": "new.lock" }),
    )
    .await
    .unwrap();
    let r = p
        .invoke(&["lock", "diff"], &["old.lock", "new.lock"], json!({}))
        .await
        .unwrap();
    let lines = r.lines.unwrap();
    assert!(lines.contains(&"+ tool@1.0.0".to_owned()), "{lines:?}");
    assert!(lines.contains(&"- a@1.0.0".to_owned()), "{lines:?}");
    let r = p
        .invoke(&["lock", "print"], &[], json!({ "yarn": true }))
        .await
        .unwrap();
    assert!(r.lines.unwrap()[0].starts_with('#'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dedupe_collapses_duplicates() {
    let reg = Registry::start(&[
        Pkg::new("c", "1.0.0"),
        Pkg::new("d", "1.0.0").dep("c", "^1.0.0"),
        Pkg::new("e", "1.0.0").dep("c", "^1.1.0"),
    ])
    .await;
    let p = Project::new(
        &reg,
        json!({ "dependencies": { "c": "^1.0.0", "d": "1.0.0" } }),
    );
    p.install().await;
    reg.publish(&Pkg::new("c", "1.1.0"));
    p.run(p.opts(InstallCommand::Add, &["e@1.0.0"]))
        .await
        .unwrap();
    let lock = p.read("fnpm.lock");
    assert!(
        lock.contains("c@1.0.0") && lock.contains("c@1.1.0"),
        "{lock}"
    );
    let r = p
        .invoke(&["dedupe"], &[], json!({ "check": true }))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 1);
    let r = p.invoke(&["dedupe"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 0, "{:?}", r.lines);
    let lock = p.read("fnpm.lock");
    assert!(!lock.contains("c@1.0.0"), "{lock}");
    assert_eq!(p.installed("c").as_deref(), Some("1.1.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prune_removes_extraneous() {
    let reg = Registry::start(&pkgs()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "b": "1.0.0" } }));
    p.install().await;
    p.write(
        "node_modules/stray/package.json",
        "{\"name\":\"stray\",\"version\":\"1.0.0\"}",
    );
    let r = p
        .invoke(&["prune"], &[], json!({ "check": true }))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 1, "{:?}", r.lines);
    assert!(exists(&p.path("node_modules/stray")));
    let r = p.invoke(&["prune"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 0);
    assert!(!exists(&p.path("node_modules/stray")));
    assert_eq!(p.installed("b").as_deref(), Some("1.0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_and_exec() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(
        &reg,
        json!({ "scripts": { "hello": "echo hi > out.txt", "fail": "exit 3", "prehello": "echo pre > pre.txt" } }),
    );
    let r = p.invoke(&["run"], &["hello"], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 0);
    assert_eq!(p.read("out.txt").trim(), "hi");
    assert!(exists(&p.path("pre.txt")), "pre hook");
    let r = p
        .invoke(&["run"], &["fail"], json!({ "silent": true }))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 3);
    let r = p.invoke(&["run"], &["missing"], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 1);
    let r = p
        .invoke(&["run"], &["missing"], json!({ "ifPresent": true }))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0);
    let r = p.invoke(&["run"], &[], json!({})).await.unwrap();
    assert!(
        r.lines
            .unwrap()
            .iter()
            .any(|l| l.contains("fnpm run hello"))
    );
    let r = p
        .invoke(&["exec"], &["echo $npm_command > exec.txt"], json!({}))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0);
    assert!(exists(&p.path("exec.txt")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_filter_runs_in_dependency_order() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(&reg, json!({ "name": "root", "workspaces": ["pkgs/*"] }));
    p.write(
        "pkgs/lib/package.json",
        &json!({ "name": "lib", "scripts": { "build": "sleep 0.2 && echo lib >> ../../order.txt" } }).to_string(),
    );
    p.write(
        "pkgs/app/package.json",
        &json!({ "name": "app", "dependencies": { "lib": "workspace:*" }, "scripts": { "build": "echo app >> ../../order.txt" } }).to_string(),
    );
    let r = p
        .invoke(&["run"], &["build"], json!({ "filter": ["*"] }))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0);
    assert_eq!(p.read("order.txt"), "lib\napp\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_and_trust() {
    let reg = Registry::start(&[
        Pkg::new("native", "1.0.0").script("postinstall", "echo built > built.txt")
    ])
    .await;
    let p = Project::new(&reg, json!({ "dependencies": { "native": "1.0.0" } }));
    let s = p.install().await;
    assert_eq!(s.blocked_scripts, 1);
    let r = p
        .invoke(&["pm", "untrusted"], &[], json!({}))
        .await
        .unwrap();
    let lines = r.lines.unwrap();
    assert!(lines.iter().any(|l| l.contains("postinstall")), "{lines:?}");
    let r = p
        .invoke(&["pm", "trust"], &["native"], json!({ "dryRun": true }))
        .await
        .unwrap();
    assert!(
        r.lines
            .unwrap()
            .iter()
            .any(|l| l.contains("would add to trustedDependencies"))
    );
    assert!(!exists(&p.path("node_modules/native/built.txt")));
    p.invoke(&["pm", "trust"], &["native"], json!({}))
        .await
        .unwrap();
    assert!(exists(&p.path("node_modules/native/built.txt")));
    assert_eq!(
        p.json("package.json")["trustedDependencies"],
        json!(["native"])
    );
    assert!(p.read("fnpm.lock").contains("\"trustedDependencies\""));
    let r = p
        .invoke(&["pm", "untrusted"], &[], json!({}))
        .await
        .unwrap();
    assert!(r.lines.unwrap()[0].starts_with("Found 0 untrusted"));
    let r = p
        .invoke(&["pm", "default-trusted"], &[], json!({}))
        .await
        .unwrap();
    assert!(r.lines.unwrap().len() > 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_registers_and_installs() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(&reg, json!({ "name": "app" }));
    let lib = p.tmp.path().join("my-lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        lib.join("package.json"),
        json!({ "name": "my-lib", "version": "0.1.0" }).to_string(),
    )
    .unwrap();
    let mut lib_req = fnpm_core::CoreRequest {
        command: vec!["link".into()],
        cwd: lib.clone(),
        ..Default::default()
    };
    lib_req.options.insert("registry".into(), json!(reg.url));
    let r = fnpm_core::invoke_with_env(lib_req, std::sync::Arc::new(fnpm_types::NoopSink), p.env())
        .await
        .unwrap();
    assert!(r.lines.unwrap()[0].contains("Registered \"my-lib\""));
    p.invoke(&["link"], &["my-lib"], json!({})).await.unwrap();
    let resolved = std::fs::canonicalize(p.path("node_modules/my-lib")).unwrap();
    assert_eq!(resolved, std::fs::canonicalize(&lib).unwrap());
    let err = p
        .invoke(&["link"], &["not-registered"], json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "USAGE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_store_cache_doctor_and_scanner_configuration() {
    let reg = Registry::start(&pkgs()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "b": "1.0.0" } }));
    p.install().await;
    let r = p.invoke(&["audit"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 0);
    let lines = r.lines.unwrap();
    let summary = lines.last().unwrap();
    assert!(
        summary.starts_with("No vulnerabilities found (checked 1 package) ["),
        "{summary}"
    );
    assert!(summary.ends_with("ms]"), "{summary}");
    reg.set_advisories(json!({ "b": [{ "id": 1, "title": "Bad b", "severity": "high", "vulnerable_versions": "<2.0.0", "url": "https://github.com/advisories/GHSA-x" }] }));
    let r = p.invoke(&["audit"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 1);
    assert!(
        r.lines
            .unwrap()
            .iter()
            .any(|l| l.contains("1 vulnerability (1 high)"))
    );
    let r = p
        .invoke(&["audit"], &[], json!({ "auditLevel": "critical" }))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0);
    let r = p
        .invoke(&["audit"], &[], json!({ "ignore": ["GHSA-x"] }))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0);

    let r = p
        .invoke(&["store", "status"], &[], json!({}))
        .await
        .unwrap();
    assert!(r.lines.unwrap().iter().any(|l| l == "packages: 1"));
    let r = p.invoke(&["store", "path"], &[], json!({})).await.unwrap();
    assert!(r.lines.unwrap()[0].contains("fnpm-home"));
    let r = p.invoke(&["pm", "cache"], &[], json!({})).await.unwrap();
    assert!(r.lines.unwrap()[0].contains("cache"));
    let r = p
        .invoke(&["pm", "cache", "pack"], &["cache.pack"], json!({}))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0);
    assert!(exists(&p.path("cache.pack")));
    let r = p.invoke(&["pm", "fetch"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 0);
    let r = p.invoke(&["doctor"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 0, "{:?}", r.lines);

    let scan = p.invoke(&["pm", "scan"], &[], json!({})).await.unwrap();
    assert_eq!(scan.exit_code, 1);
    assert!(
        scan.lines
            .unwrap()
            .iter()
            .any(|line| line.contains("configure a security scanner"))
    );
    let err = p.invoke(&["frobnicate"], &[], json!({})).await.unwrap_err();
    assert_eq!(err.code(), "USAGE");
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_like_bun_dry_run_scripts_and_git() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(
        &reg,
        json!({
            "name": "x",
            "version": "1.0.0",
            "scripts": {
                "preversion": "mybin pre",
                "postversion": "echo $npm_command $npm_old_version $npm_new_version > post.txt"
            }
        }),
    );
    // A bin of the package's own node_modules is on PATH, like `fnpm run`.
    p.write(
        "node_modules/.bin/mybin",
        "#!/bin/sh\necho \"$1\" > mybin.txt\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            p.path("node_modules/.bin/mybin"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    // --dry-run prints the version and changes nothing.
    let r = p
        .invoke(&["pm", "version"], &["patch"], json!({ "dryRun": true }))
        .await
        .unwrap();
    assert_eq!(r.lines.unwrap(), vec!["v1.0.1"]);
    assert_eq!(p.json("package.json")["version"], "1.0.0");
    assert!(!p.path("mybin.txt").exists());

    // --ignore-scripts bumps without running anything.
    p.invoke(
        &["pm", "version"],
        &["patch"],
        json!({ "ignoreScripts": true }),
    )
    .await
    .unwrap();
    assert_eq!(p.json("package.json")["version"], "1.0.1");
    assert!(!p.path("post.txt").exists());

    // A git repository: commit and tag, scripts see bun's variables.
    git(&p.dir, &["init", "-q"]);
    git(&p.dir, &["config", "user.email", "t@example.com"]);
    git(&p.dir, &["config", "user.name", "t"]);
    git(&p.dir, &["config", "commit.gpgsign", "false"]);
    git(&p.dir, &["config", "tag.gpgsign", "false"]);
    p.write(".gitignore", "node_modules\n*.txt\n");
    git(&p.dir, &["add", "-A"]);
    git(&p.dir, &["commit", "-q", "-m", "init"]);
    let r = p
        .invoke(
            &["pm", "version"],
            &["minor"],
            json!({ "message": "release %s" }),
        )
        .await
        .unwrap();
    assert_eq!(r.lines.unwrap(), vec!["v1.1.0"]);
    assert_eq!(p.read("mybin.txt").trim(), "pre");
    assert_eq!(p.read("post.txt").trim(), "version 1.0.1 1.1.0");
    assert_eq!(git(&p.dir, &["log", "-1", "--format=%s"]), "release 1.1.0");
    assert_eq!(git(&p.dir, &["tag", "--list"]), "v1.1.0");

    // A dirty tree is an error unless --force.
    p.write("dirty.js", "1\n");
    let err = p
        .invoke(&["pm", "version"], &["patch"], json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "REPORTED");
    assert_eq!(p.json("package.json")["version"], "1.1.0");
    std::fs::remove_file(p.path("dirty.js")).unwrap();

    // from-git takes the newest version tag, whatever its prefix.
    git(&p.dir, &["commit", "-q", "--allow-empty", "-m", "next"]);
    git(&p.dir, &["tag", "-a", "x@2.0.0", "-m", "x@2.0.0"]);
    p.invoke(
        &["pm", "version"],
        &["from-git"],
        json!({ "noGitTagVersion": true, "ignoreScripts": true }),
    )
    .await
    .unwrap();
    assert_eq!(p.json("package.json")["version"], "2.0.0");
}

fn request_in(
    dir: &std::path::Path,
    reg: &Registry,
    command: &[&str],
    positionals: &[&str],
) -> fnpm_core::CoreRequest {
    let mut req = fnpm_core::CoreRequest {
        command: command.iter().map(|s| (*s).to_owned()).collect(),
        cwd: dir.to_path_buf(),
        positionals: positionals.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    };
    req.options.insert("registry".into(), json!(reg.url));
    req
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registry_and_cache_commands_work_outside_a_project() {
    let reg = Registry::start(&pkgs()).await;
    let p = Project::new(&reg, json!({}));
    let outside = p.tmp.path().join("no-project");
    std::fs::create_dir_all(&outside).unwrap();
    let sink = std::sync::Arc::new(fnpm_types::NoopSink);
    for (command, positionals) in [
        (vec!["info"], vec!["a"]),
        (vec!["pm", "cache"], vec![]),
        (vec!["store", "path"], vec![]),
        (vec!["pm", "bin"], vec![]),
    ] {
        let mut req = request_in(&outside, &reg, &command, &positionals);
        if command == ["pm", "bin"] {
            req.options.insert("global".into(), json!(true));
        }
        let r = fnpm_core::invoke_with_env(req, sink.clone(), p.env()).await;
        assert!(r.is_ok(), "{command:?}: {r:?}");
    }
    // Commands that need a project still say so.
    let err =
        fnpm_core::invoke_with_env(request_in(&outside, &reg, &["why"], &["a"]), sink, p.env())
            .await
            .unwrap_err();
    assert_eq!(err.code(), "CORE_ERROR", "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_rejects_names_that_leave_the_registry() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(&reg, json!({ "name": "app" }));
    // Something the bad name would point at from `<global>/links`.
    let victim = p.tmp.path().join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("keep.txt"), "x").unwrap();
    let evil = p.tmp.path().join("evil");
    std::fs::create_dir_all(&evil).unwrap();
    std::fs::write(
        evil.join("package.json"),
        json!({ "name": "../../victim" }).to_string(),
    )
    .unwrap();
    let sink = std::sync::Arc::new(fnpm_types::NoopSink);
    for command in [["link"], ["unlink"]] {
        let err = fnpm_core::invoke_with_env(
            request_in(&evil, &reg, &command, &[]),
            sink.clone(),
            p.env(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "USAGE", "{command:?}: {err}");
    }
    let err = p
        .invoke(&["link"], &["../../victim"], json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "USAGE");
    assert!(
        victim.join("keep.txt").exists(),
        "nothing outside the registry is touched"
    );

    let err = p
        .invoke(&["pm", "pkg", "set"], &["files[99999999]=x"], json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "REPORTED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_commit_waits_for_resolution_and_preserves_existing_patch() {
    let reg = Registry::start(&[Pkg::new("a", "1.0.0")]).await;
    let p = Project::new(&reg, json!({"dependencies": {"a": "1.0.0"}}));
    p.install().await;
    let options = json!({"logLevel": "silent", "ignoreScripts": true});
    assert_eq!(
        p.invoke(&["patch"], &["a"], options.clone())
            .await
            .unwrap()
            .exit_code,
        0
    );
    let edited = "module.exports = 'first edit';\n";
    p.write("node_modules/a/index.js", edited);
    assert_eq!(
        p.invoke(&["patch-commit"], &["node_modules/a"], options.clone())
            .await
            .unwrap()
            .exit_code,
        0
    );
    assert_eq!(p.read("node_modules/a/index.js"), edited);
    let patch = p.read("patches/a@1.0.0.patch");
    assert!(patch.contains("first edit"));
    assert_eq!(
        p.json("package.json")["patchedDependencies"]["a@1.0.0"],
        "patches/a@1.0.0.patch"
    );

    assert_eq!(
        p.invoke(&["patch"], &["a"], options.clone())
            .await
            .unwrap()
            .exit_code,
        0
    );
    p.write(
        "node_modules/a/index.js",
        "module.exports = 'second edit';\n",
    );
    let mut manifest = p.json("package.json");
    manifest["dependencies"]["missing"] = json!("1.0.0");
    p.write(
        "package.json",
        &serde_json::to_string_pretty(&manifest).unwrap(),
    );
    let manifest_before = p.read("package.json");
    let lock_before = p.read("fnpm.lock");
    let result = p
        .invoke(&["patch-commit"], &["node_modules/a"], options.clone())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 1);
    assert_eq!(p.read("package.json"), manifest_before);
    assert_eq!(p.read("fnpm.lock"), lock_before);
    assert_eq!(p.read("patches/a@1.0.0.patch"), patch);
    assert_eq!(
        p.read("node_modules/a/index.js"),
        "module.exports = 'second edit';\n"
    );

    manifest["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("missing");
    p.write(
        "package.json",
        &serde_json::to_string_pretty(&manifest).unwrap(),
    );
    assert_eq!(
        p.invoke(
            &["patch-commit"],
            &["node_modules/a"],
            json!({
                "ignoreScripts": true, "logLevel": "silent", "lockfileOnly": true
            })
        )
        .await
        .unwrap()
        .exit_code,
        0
    );
    assert_eq!(
        p.read("node_modules/a/index.js"),
        "module.exports = 'second edit';\n"
    );
    assert!(p.read("patches/a@1.0.0.patch").contains("second edit"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_local_tarball_survives_clean_install() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(&reg, json!({"dependencies": {"a": "./a.tgz"}}));
    std::fs::write(p.path("a.tgz"), Pkg::new("a", "1.0.0").tgz()).unwrap();
    p.install().await;
    assert_eq!(
        p.invoke(&["patch"], &["a"], json!({"ignoreScripts": true}))
            .await
            .unwrap()
            .exit_code,
        0
    );
    p.write(
        "node_modules/a/index.js",
        "module.exports = 'patched tarball';\n",
    );
    assert_eq!(
        p.invoke(
            &["patch-commit"],
            &["node_modules/a"],
            json!({"ignoreScripts": true})
        )
        .await
        .unwrap()
        .exit_code,
        0
    );
    let patched = p.json("package.json")["patchedDependencies"]
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(patched.len(), 1);
    assert!(patched.contains_key("a@./a.tgz"));
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    p.install().await;
    assert_eq!(
        p.read("node_modules/a/index.js"),
        "module.exports = 'patched tarball';\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_accepts_long_tarball_label_and_failed_commit_keeps_manifest() {
    let reg = Registry::start(&[]).await;
    let spec = format!("./{}a.tgz", "x/../".repeat(260));
    let p = Project::new(&reg, json!({"dependencies": {"a": spec}}));
    std::fs::write(p.path("a.tgz"), Pkg::new("a", "1.0.0").tgz()).unwrap();
    p.install().await;
    let manifest_before = p.read("package.json");
    let lock_before = p.read("fnpm.lock");
    assert_eq!(
        p.invoke(
            &["patch"],
            &[&format!("a@{spec}")],
            json!({"ignoreScripts": true})
        )
        .await
        .unwrap()
        .exit_code,
        0
    );
    p.write(
        "node_modules/a/index.js",
        "module.exports = 'long label';\n",
    );
    assert_eq!(
        p.invoke(
            &["patch-commit"],
            &["node_modules/a"],
            json!({"ignoreScripts": true})
        )
        .await
        .unwrap()
        .exit_code,
        1
    );
    assert_eq!(p.read("package.json"), manifest_before);
    assert_eq!(p.read("fnpm.lock"), lock_before);
    assert_eq!(
        p.read("node_modules/a/index.js"),
        "module.exports = 'long label';\n"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_copy_backend_keeps_bins_executable_without_mode_diff() {
    use std::os::unix::fs::PermissionsExt;
    let reg = Registry::start(&[Pkg::new("a", "1.0.0").bin("tool", "cli.js")]).await;
    let p = Project::new(&reg, json!({"dependencies": {"a": "1.0.0"}}));
    let options = json!({"backend": "copyfile", "ignoreScripts": true});
    assert_eq!(
        p.invoke(&["patch"], &["a"], options.clone())
            .await
            .unwrap()
            .exit_code,
        0
    );
    assert_ne!(
        std::fs::metadata(p.path("node_modules/a/cli.js"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    p.write("node_modules/a/added.js", "module.exports = 'edit';\n");
    assert_eq!(
        p.invoke(&["patch-commit"], &["a"], options)
            .await
            .unwrap()
            .exit_code,
        0
    );
    let patch = p.read("patches/a@1.0.0.patch");
    assert!(patch.contains("diff --git a/added.js b/added.js"));
    assert!(
        !patch.contains("cli.js"),
        "bin mode is not an edit: {patch}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_keeps_absolute_patches_dir_next_to_project() {
    let reg = Registry::start(&[Pkg::new("a", "1.0.0")]).await;
    let p = Project::new(&reg, json!({"dependencies": {"a": "1.0.0"}}));
    let outside = p.dir.with_file_name("app-patches");
    assert_eq!(
        p.invoke(&["patch"], &["a"], json!({"ignoreScripts": true}))
            .await
            .unwrap()
            .exit_code,
        0
    );
    p.write(
        "node_modules/a/index.js",
        "module.exports = 'outside patch';\n",
    );
    assert_eq!(
        p.invoke(
            &["patch-commit"],
            &["a"],
            json!({"ignoreScripts": true, "patchesDir": outside})
        )
        .await
        .unwrap()
        .exit_code,
        0
    );
    let expected = outside.join("a@1.0.0.patch");
    assert!(expected.exists());
    assert_eq!(
        p.json("package.json")["patchedDependencies"]["a@1.0.0"],
        expected.to_string_lossy().replace('\\', "/")
    );
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    p.install().await;
    assert_eq!(
        p.read("node_modules/a/index.js"),
        "module.exports = 'outside patch';\n"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_keeps_isolated_project_link_for_name_and_path_arguments() {
    let reg = Registry::start(&[
        Pkg::new("a", "1.0.0").dep("b", "1.0.0"),
        Pkg::new("b", "1.0.0"),
    ])
    .await;
    for arg in ["a", "node_modules/a"] {
        let p = Project::new(&reg, json!({"dependencies": {"a": "1.0.0"}}));
        p.write("bunfig.toml", "[install]\nglobalStore = false\n");
        let options = json!({"linker": "isolated", "ignoreScripts": true});
        assert_eq!(
            p.invoke(&["patch"], &[arg], options.clone())
                .await
                .unwrap()
                .exit_code,
            0
        );
        let link = p.path("node_modules/a");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        let destination = std::fs::canonicalize(&link).unwrap();
        assert!(destination.starts_with(p.path("node_modules/.fnpm")));
        let sibling = destination.parent().unwrap().join("b/package.json");
        assert!(
            sibling.exists(),
            "the package still reaches its isolated dependency"
        );
        p.write(
            "node_modules/a/index.js",
            "module.exports = 'isolated edit';\n",
        );
        assert_eq!(
            p.invoke(&["patch-commit"], &[arg], options)
                .await
                .unwrap()
                .exit_code,
            0
        );
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(
            p.read("node_modules/a/index.js"),
            "module.exports = 'isolated edit';\n"
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_global_copy_keeps_sibling_dependencies_and_store_untouched() {
    let reg = Registry::start(&[
        Pkg::new("a", "1.0.0")
            .dep("b", "1.0.0")
            .dep("@deps/c", "1.0.0"),
        Pkg::new("@scope/a", "1.0.0")
            .dep("b", "1.0.0")
            .dep("@deps/c", "1.0.0"),
        Pkg::new("b", "1.0.0"),
        Pkg::new("@deps/c", "1.0.0"),
    ])
    .await;
    for name in ["a", "@scope/a"] {
        let p = Project::new(&reg, json!({"dependencies": {name: "1.0.0"}}));
        p.write("bunfig.toml", "[install]\nglobalStore = true\n");
        let options = json!({"linker": "isolated", "ignoreScripts": true});
        let mut install = p.opts(InstallCommand::Install, &[]);
        install.pm.linker = Some(fnpm_config::LinkerSetting::Isolated);
        p.run(install).await.unwrap();
        let folder = p.path(&format!("node_modules/{name}"));
        assert!(std::fs::symlink_metadata(&folder).unwrap().is_symlink());
        let original = std::fs::canonicalize(&folder).unwrap();
        assert!(!original.starts_with(&p.dir));
        let original_text = std::fs::read_to_string(original.join("index.js")).unwrap();
        let mut siblings = original.parent().unwrap();
        if name.starts_with('@') {
            siblings = siblings.parent().unwrap();
        }
        let expected_b = std::fs::canonicalize(siblings.join("b")).unwrap();
        let expected_c = std::fs::canonicalize(siblings.join("@deps/c")).unwrap();
        assert_eq!(
            p.invoke(&["patch"], &[name], options.clone())
                .await
                .unwrap()
                .exit_code,
            0
        );
        assert!(std::fs::symlink_metadata(&folder).unwrap().is_dir());
        assert_eq!(
            std::fs::canonicalize(folder.join("node_modules/b")).unwrap(),
            expected_b
        );
        assert_eq!(
            std::fs::canonicalize(folder.join("node_modules/@deps/c")).unwrap(),
            expected_c
        );
        assert!(!folder.join("node_modules").join(name).exists());
        std::fs::write(folder.join("index.js"), "module.exports = 'global edit';\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(original.join("index.js")).unwrap(),
            original_text
        );
        assert_eq!(
            p.invoke(&["patch-commit"], &[name], options)
                .await
                .unwrap()
                .exit_code,
            0
        );
        let manifest = p.json("package.json");
        let patch_path = manifest["patchedDependencies"][format!("{name}@1.0.0")]
            .as_str()
            .unwrap();
        let patch = p.read(patch_path);
        assert!(patch.contains("diff --git a/index.js b/index.js"));
        assert!(
            !patch.contains("node_modules/"),
            "dependency links are not edits: {patch}"
        );
        assert_eq!(
            std::fs::read_to_string(original.join("index.js")).unwrap(),
            original_text
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_refuses_an_absolute_package_path_outside_the_project() {
    let reg = Registry::start(&[Pkg::new("a", "1.0.0")]).await;
    let p = Project::new(&reg, json!({"dependencies": {"a": "1.0.0"}}));
    p.install().await;
    let outside = tempfile::tempdir().unwrap();
    let package = outside.path().join("node_modules/a");
    std::fs::create_dir_all(&package).unwrap();
    let manifest = r#"{"name":"a","version":"1.0.0"}"#;
    std::fs::write(package.join("package.json"), manifest).unwrap();
    std::fs::write(package.join("index.js"), "outside package").unwrap();
    assert_eq!(
        p.invoke(
            &["patch"],
            &[package.to_str().unwrap()],
            json!({"ignoreScripts": true})
        )
        .await
        .unwrap()
        .exit_code,
        1
    );
    assert_eq!(
        std::fs::read_to_string(package.join("index.js")).unwrap(),
        "outside package"
    );
    assert_eq!(
        std::fs::read_to_string(package.join("package.json")).unwrap(),
        manifest
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_replaces_external_link_without_editing_checkout() {
    let reg = Registry::start(&[Pkg::new("a", "1.0.0")]).await;
    let p = Project::new(&reg, json!({"dependencies": {"a": "1.0.0"}}));
    p.install().await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("package.json"),
        r#"{"name":"a","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(outside.path().join("index.js"), "outside checkout").unwrap();
    std::fs::remove_dir_all(p.path("node_modules/a")).unwrap();
    std::os::unix::fs::symlink(outside.path(), p.path("node_modules/a")).unwrap();
    assert_eq!(
        p.invoke(&["patch"], &["a"], json!({"ignoreScripts": true}))
            .await
            .unwrap()
            .exit_code,
        0
    );
    assert!(
        !std::fs::symlink_metadata(p.path("node_modules/a"))
            .unwrap()
            .is_symlink()
    );
    p.write("node_modules/a/index.js", "prepared edit");
    assert_eq!(
        std::fs::read_to_string(outside.path().join("index.js")).unwrap(),
        "outside checkout"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_commit_failed_save_preserves_patch_manifest_and_lock() {
    let reg = Registry::start(&[Pkg::new("a", "1.0.0")]).await;
    let p = Project::new(&reg, json!({"dependencies": {"a": "1.0.0"}}));
    let options = json!({"ignoreScripts": true, "logLevel": "silent"});
    assert_eq!(
        p.invoke(&["patch"], &["a"], options.clone())
            .await
            .unwrap()
            .exit_code,
        0
    );
    p.write(
        "node_modules/a/index.js",
        "module.exports = 'first edit';\n",
    );
    assert_eq!(
        p.invoke(&["patch-commit"], &["a"], options.clone())
            .await
            .unwrap()
            .exit_code,
        0
    );
    assert_eq!(
        p.invoke(&["patch"], &["a"], options.clone())
            .await
            .unwrap()
            .exit_code,
        0
    );
    p.write(
        "node_modules/a/index.js",
        "module.exports = 'second edit';\n",
    );
    let manifest = p.read("package.json");
    let lock = p.read("fnpm.lock");
    let patch = p.read("patches/a@1.0.0.patch");
    p.write("yarn.lock/keep", "unrelated directory");
    let result = p
        .invoke(
            &["patch-commit"],
            &["a"],
            json!({
                "ignoreScripts": true, "logLevel": "silent", "yarn": true
            }),
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 1);
    assert_eq!(p.read("package.json"), manifest);
    assert_eq!(p.read("fnpm.lock"), lock);
    assert_eq!(p.read("patches/a@1.0.0.patch"), patch);
    assert_eq!(p.read("yarn.lock/keep"), "unrelated directory");
    assert_eq!(
        p.read("node_modules/a/index.js"),
        "module.exports = 'second edit';\n"
    );
    std::fs::remove_dir_all(p.path("yarn.lock")).unwrap();
    assert_eq!(
        p.invoke(&["patch-commit"], &["a"], options)
            .await
            .unwrap()
            .exit_code,
        0
    );
    let current = p.read("patches/a@1.0.0.patch");
    assert!(current.contains("second edit"));
    let saved = fnpm_lockfile::formats::fnpm::parse(p.read("fnpm.lock").as_bytes()).unwrap();
    assert_eq!(
        saved.patched_dependencies["a@1.0.0"].path,
        "patches/a@1.0.0.patch"
    );
    std::fs::remove_dir_all(p.path("node_modules")).unwrap();
    p.install().await;
    assert_eq!(
        p.read("node_modules/a/index.js"),
        "module.exports = 'second edit';\n"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_registry_versions_filters_and_installed_side() {
    let reg = Registry::start(&[Pkg::new("tool", "1.0.0"), Pkg::new("tool", "2.0.0")]).await;
    let p = Project::new(&reg, json!({"name":"app","dependencies":{"tool":"1.0.0"}}));
    p.install().await;
    let result = p
        .invoke(&["pm", "diff"], &["tool"], json!({}))
        .await
        .unwrap();
    let data = result.data.unwrap();
    assert_eq!(data["from"], "tool@1.0.0");
    assert_eq!(data["to"], "tool@2.0.0");
    assert_eq!(data["totals"]["files"], 2);
    assert!(
        result
            .lines
            .unwrap()
            .join("\n")
            .contains("+module.exports = \"tool@2.0.0\";")
    );
    let result = p
        .invoke(
            &["pm", "diff"],
            &["tool@1.0.0:index.js", "2.0.0"],
            json!({"nameOnly":true}),
        )
        .await
        .unwrap();
    let files = result.data.unwrap()["files"].as_array().unwrap().clone();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "index.js");
    assert!(files[0].get("patch").is_none());
    assert!(
        p.invoke(&["pm", "diff"], &["tool@1.0.0:nope", "2.0.0"], json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("no file in either side matches nope")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_local_trees_never_runs_scripts_and_reports_binary_hashes() {
    let reg = Registry::start(&[]).await;
    let p = Project::new(&reg, json!({"name":"app"}));
    p.write("a/package.json", r#"{"name":"p","version":"1.0.0"}"#);
    p.write(
        "b/package.json",
        r#"{"name":"p","version":"2.0.0","scripts":{"postinstall":"exit 99"}}"#,
    );
    p.write("a/index.js", "module.exports = 1;\n");
    p.write(
        "b/index.js",
        "const cp = require('child_process');\nmodule.exports = 2;\n",
    );
    std::fs::write(p.path("b/data.bin"), [0, 255, 128]).unwrap();
    let result = p
        .invoke(&["pm", "diff"], &["./a", "./b"], json!({}))
        .await
        .unwrap();
    let data = result.data.unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(data["from"], "./a");
    let binary = data["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "data.bin")
        .unwrap();
    assert_eq!(binary["binary"], true);
    assert_eq!(binary["sha256After"].as_str().unwrap().len(), 64);
    assert!(
        data["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "postinstall script added: exit 99")
    );
    assert!(
        data["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "now imports child_process (index.js)")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_fix_rejects_manifest_changes_before_auditing_or_writing() {
    let reg = Registry::start(&[Pkg::new("a", "1.0.0"), Pkg::new("a", "1.1.0")]).await;
    let p = Project::new(
        &reg,
        json!({ "name": "app", "dependencies": { "a": "1.0.0" } }),
    );
    p.install().await;
    let lock = p.read("fnpm.lock");
    let result = p
        .invoke(&["audit"], &["fix"], json!({ "dryRun": true }))
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    p.write(
        "package.json",
        &json!({ "name": "app", "dependencies": { "a": "1.1.0" } }).to_string(),
    );
    let manifest = p.read("package.json");
    let requests = reg.seen().len();
    for dry_run in [false, true] {
        let error = p
            .invoke(&["audit"], &["fix"], json!({ "dryRun": dry_run }))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("does not match package.json"),
            "{error}"
        );
        assert_eq!(
            reg.seen().len(),
            requests,
            "must refuse before a registry request"
        );
        assert_eq!(p.read("fnpm.lock"), lock);
        assert_eq!(p.read("package.json"), manifest);
        assert_eq!(p.installed("a").as_deref(), Some("1.0.0"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_fix_replaces_installed_files_when_cached_layout_is_intact() {
    let reg = Registry::start(&[Pkg::new("a", "1.0.0"), Pkg::new("a", "1.0.1")]).await;
    let p = Project::new(&reg, json!({ "dependencies": { "a": "^1.0.0" } }));
    p.run(p.opts(fnpm_core::options::InstallCommand::Add, &["a@1.0.0"]))
        .await
        .unwrap();
    reg.set_advisories(json!({ "a": [{
        "id": 1, "severity": "high", "title": "unsafe", "vulnerable_versions": "<1.0.1",
        "url": "https://github.com/advisories/GHSA-test"
    }] }));
    p.invoke(&["audit"], &["fix"], json!({})).await.unwrap();
    assert_eq!(p.installed("a").as_deref(), Some("1.0.1"));
    assert!(p.read("node_modules/a/index.js").contains("a@1.0.1"));
    assert!(p.read("fnpm.lock").contains("a@1.0.1"));
}
