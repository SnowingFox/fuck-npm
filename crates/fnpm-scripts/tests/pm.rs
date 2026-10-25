//! `fnpm pm untrusted` / `trust` / `default-trusted` (bun pm_trusted_command.rs and
//! bun-install-lifecycle-scripts.test.ts "bun pm untrusted and trust ...").

mod common;

use std::path::PathBuf;

use common::*;
use fnpm_scripts::TrustPolicy;
use fnpm_scripts::pm::*;
use fnpm_types::{LockGraph, Package, PackageId, Resolution};
use serde_json::json;

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    graph: LockGraph,
    a: PackageId,
    b: PackageId,
    aliased: PackageId,
    git_below: PackageId,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let mut g = root("app");
    g.trusted_dependencies = Some(Default::default());
    let a = add(&mut g, with_scripts(npm("a", "1.0.0")));
    let b = add(&mut g, with_scripts(npm("b", "2.0.0")));
    let aliased = add(&mut g, with_scripts(npm("real-name", "1.0.0")));
    let git_below = add(
        &mut g,
        Package::new(
            "g",
            "1.0.0",
            Resolution::Git {
                url: "u".into(),
                committish: "".into(),
                resolved_commit: "c".into(),
                integrity: None,
            },
        ),
    );
    edge(&mut g, PackageId::ROOT, "a", a);
    edge(&mut g, PackageId::ROOT, "my-alias", aliased);
    edge(&mut g, a, "b", b);
    edge(&mut g, a, "g", git_below);
    for (name, scripts) in [
        ("node_modules/a", json!({"postinstall": "echo a"})),
        (
            "node_modules/a/node_modules/b",
            json!({"preinstall": "echo b1", "postinstall": "echo b2"}),
        ),
        ("node_modules/my-alias", json!({"install": "echo r"})),
        (
            "node_modules/a/node_modules/g",
            json!({"prepare": "echo g"}),
        ),
    ] {
        write_pkg(&dir.join(name), json!({"scripts": scripts}));
    }
    Fixture {
        _tmp: tmp,
        dir,
        graph: g,
        a,
        b,
        aliased,
        git_below,
    }
}

fn candidates(f: &Fixture) -> Vec<Candidate> {
    let c = |p, alias: &str, rel: &str, depth| Candidate {
        package: p,
        alias: alias.into(),
        dir: f.dir.join(rel),
        depth,
        in_global_store: false,
    };
    vec![
        c(f.a, "a", "node_modules/a", 0),
        c(f.b, "b", "node_modules/a/node_modules/b", 1),
        c(f.aliased, "my-alias", "node_modules/my-alias", 0),
        c(f.git_below, "g", "node_modules/a/node_modules/g", 1),
        c(PackageId::ROOT, "app", "", 0),
    ]
}

fn untrusted(f: &Fixture) -> Vec<Untrusted> {
    let policy = TrustPolicy::from_graph(&f.graph, []);
    find_untrusted(&f.graph, &policy, candidates(f), &|_| {
        fnpm_config::DEFAULT_REGISTRY
    })
    .unwrap()
}

#[test]
fn finds_untrusted_packages_with_scripts() {
    let f = fixture();
    let list = untrusted(&f);
    let names: Vec<&str> = list.iter().map(|u| u.alias.as_str()).collect();
    assert_eq!(names, ["a", "b", "my-alias", "g"]);
    assert_eq!(list[1].scripts.len(), 2);
    assert_eq!(list[2].trusted_name, "real-name");
    assert!(list[2].can_trust);
    assert!(
        !list[3].can_trust,
        "a git package only a registry package declares"
    );
    assert_eq!(
        list[3].scripts,
        [("prepare".to_string(), "echo g".to_string())]
    );
}

#[test]
fn trusted_packages_are_not_listed() {
    let mut f = fixture();
    f.graph.trusted_dependencies = Some(
        ["a".to_string(), "real-name".to_string()]
            .into_iter()
            .collect(),
    );
    let names: Vec<String> = untrusted(&f).into_iter().map(|u| u.alias).collect();
    assert_eq!(names, ["b", "g"]);
}

#[test]
fn renders_untrusted_listing() {
    let f = fixture();
    let lines = render_untrusted(&untrusted(&f), false);
    assert_eq!(lines[0], "./node_modules/a @1.0.0");
    assert_eq!(lines[1], " » [postinstall]: echo a");
    assert_eq!(lines[2], "");
    assert!(lines.contains(&" alias of \"real-name\"".to_string()));
    assert!(lines.contains(&format!(" {CANNOT_TRUST}")));
    assert_eq!(
        lines.last().unwrap(),
        "If you trust them and wish to run their scripts, use `fnpm pm trust`."
    );
    assert!(
        render_untrusted(&untrusted(&f), true)
            .last()
            .unwrap()
            .contains("`fnpm pm -g trust`")
    );
    let empty = render_untrusted(&[], false);
    assert_eq!(empty[0], "Found 0 untrusted dependencies with scripts.");
}

#[test]
fn trust_by_name_runs_deepest_first() {
    let f = fixture();
    let list = untrusted(&f);
    let plan = plan_trust(&list, &["a".into(), "b".into()], false).unwrap();
    assert_eq!(plan.names_to_add, ["b", "a"]);
    assert_eq!(plan.scripts_count, 3);
    let levels = trust_jobs(&f.graph, &list, &plan);
    let order: Vec<Vec<&str>> = levels
        .iter()
        .map(|l| l.iter().map(|j| j.name.as_str()).collect())
        .collect();
    assert_eq!(order, [vec!["b"], vec!["a"]]);
    assert_eq!(levels[0][0].version, "2.0.0");
}

#[test]
fn trust_all_skips_what_names_cannot_trust() {
    let f = fixture();
    let list = untrusted(&f);
    let plan = plan_trust(&list, &[], true).unwrap();
    assert_eq!(plan.undeclared, ["g"]);
    assert!(!plan.names_to_add.contains(&"g".to_string()));
    assert!(plan.names_to_add.contains(&"real-name".to_string()));
    let result = render_trust_result(&list, &plan, std::time::Duration::from_millis(12));
    assert!(
        result
            .iter()
            .any(|l| l.starts_with(" 4 scripts ran across 3 packages [")),
        "{result:?}"
    );
    assert!(
        result
            .iter()
            .any(|l| l == " 1 package with blocked scripts")
    );
    assert!(result.iter().any(|l| l == " ✓ [postinstall]: echo a"));
}

#[test]
fn trust_by_alias_notes_the_real_name() {
    let f = fixture();
    let list = untrusted(&f);
    let err = plan_trust(&list, &["my-alias".into()], false).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("0 scripts ran. The following packages are already trusted"),
        "{msg}"
    );
    assert!(msg.contains(" - my-alias"));
    assert!(msg.contains(
        "note: \"my-alias\" is an alias of \"real-name\", run 'fnpm pm trust real-name' to trust it"
    ));
    let err = plan_trust(&list, &["g".into()], false)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!("note: \"g\" {CANNOT_TRUST}")),
        "{err}"
    );
    let err = plan_trust(&[], &[], true).unwrap_err().to_string();
    assert_eq!(
        err,
        "0 scripts ran. This means all dependencies are already trusted or none have scripts."
    );
}

#[test]
fn global_store_packages_are_deferred() {
    let f = fixture();
    let mut list = untrusted(&f);
    list[0].in_global_store = true;
    let plan = plan_trust(&list, &["a".into()], false).unwrap();
    assert_eq!(plan.actions[0], Action::Defer);
    assert_eq!(plan.names_to_add, ["a"]);
    assert!(trust_jobs(&f.graph, &list, &plan).is_empty());
    let result = render_trust_result(&list, &plan, std::time::Duration::ZERO);
    assert!(
        result
            .iter()
            .any(|l| l.contains("1 trusted package linked from the global store"))
    );
}

#[test]
fn dry_run_output() {
    let f = fixture();
    let list = untrusted(&f);
    let plan = plan_trust(&list, &["b".into()], false).unwrap();
    let lines = render_dry_run(&list, &plan);
    assert!(lines.contains(&"dry run: would run 2 scripts across 1 package".to_string()));
    assert_eq!(lines.last().unwrap(), " - b");
}

#[test]
fn trust_command_flag_checks() {
    assert_eq!(
        check_trust_command(&[], false, false, false, false)
            .unwrap_err()
            .to_string(),
        "expected package names(s) or --all"
    );
    assert!(
        check_trust_command(&[], true, true, false, false)
            .unwrap_err()
            .to_string()
            .contains("ignore-scripts")
    );
    assert!(
        check_trust_command(&["a".into()], false, false, true, false)
            .unwrap_err()
            .to_string()
            .contains("frozen")
    );
    assert!(check_trust_command(&["a".into()], false, false, true, true).is_ok());
}

#[test]
fn edits_trusted_dependencies() {
    let mut pj = json!({"name": "app", "dependencies": {}});
    assert!(add_trusted_dependencies(
        &mut pj,
        &["zeta".into(), "alpha".into()]
    ));
    assert_eq!(pj["trustedDependencies"], json!(["alpha", "zeta"]));
    assert_eq!(
        pj.as_object().unwrap().keys().next_back().unwrap(),
        "trustedDependencies"
    );
    assert!(!add_trusted_dependencies(&mut pj, &["alpha".into()]));
    assert!(add_trusted_dependencies(&mut pj, &["beta".into()]));
    assert_eq!(pj["trustedDependencies"], json!(["alpha", "beta", "zeta"]));
    let mut not_object = json!([]);
    assert!(!add_trusted_dependencies(&mut not_object, &["a".into()]));
}

#[test]
fn default_trusted_listing() {
    let lines = render_default_trusted();
    let n = fnpm_scripts::trust::default_trusted().len();
    assert_eq!(lines[0], format!("Default trusted dependencies ({n}):"));
    assert_eq!(lines.len(), n + 1);
    assert!(lines.contains(&" - esbuild".to_string()));
}
