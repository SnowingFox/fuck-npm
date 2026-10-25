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
    let r = p.invoke(&["why"], &["nope"], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 1);

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
    assert!(r.lines.unwrap().iter().any(|l| l.contains("UNKNOWN")));

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
    assert_eq!(err.code(), "USAGE");
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
    assert_eq!(err.code(), "USAGE");

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
async fn audit_store_cache_doctor_and_not_implemented() {
    let reg = Registry::start(&pkgs()).await;
    let p = Project::new(&reg, json!({ "dependencies": { "b": "1.0.0" } }));
    p.install().await;
    let r = p.invoke(&["audit"], &[], json!({})).await.unwrap();
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.lines.unwrap().last().unwrap(), "No vulnerabilities found");
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

    let err = p.invoke(&["publish"], &[], json!({})).await.unwrap_err();
    assert_eq!(err.code(), "NOT_IMPLEMENTED");
    let err = p.invoke(&["frobnicate"], &[], json!({})).await.unwrap_err();
    assert_eq!(err.code(), "USAGE");
}
