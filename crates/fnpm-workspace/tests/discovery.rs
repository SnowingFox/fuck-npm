//! Workspace discovery, ported from bun's `test/cli/install/bun-workspaces.test.ts`,
//! `bad-workspace.test.ts`, `catalogs.test.ts` and the `"workspaces"` negation cases of
//! `test/cli/run/filter-workspace.test.ts`.

mod common;

use common::{pkg, tree, write};
use fnpm_workspace::{Error, LoadOptions, MissingWorkspace, Workspace};
use serde_json::json;

fn paths(ws: &Workspace) -> Vec<&str> {
    ws.members.iter().map(|m| m.path.as_str()).collect()
}

fn root(workspaces: serde_json::Value) -> String {
    json!({ "name": "root", "workspaces": workspaces }).to_string()
}

#[test]
fn array_and_object_forms() {
    for ws in [json!(["packages/*"]), json!({ "packages": ["packages/*"] })] {
        let dir = tree(&[
            ("package.json", &root(ws)),
            (
                "packages/b/package.json",
                &pkg("b", json!({ "version": "2.0.0" })),
            ),
            (
                "packages/a/package.json",
                &pkg("a", json!({ "version": "1.0.0" })),
            ),
        ]);
        let w = Workspace::load(dir.path()).unwrap();
        assert_eq!(paths(&w), ["packages/a", "packages/b"]);
        assert_eq!(w.members[0].name, "a");
        assert_eq!(w.members[0].version, "1.0.0");
        assert_eq!(w.members[1].dir, dir.path().join("packages/b"));
        assert_eq!(w.root.name.as_deref(), Some("root"));
        assert_eq!(w.member("b").unwrap().path, "packages/b");
        assert_eq!(w.member_by_path("./packages/a/").unwrap().name, "a");
        assert!(w.member("zzz").is_none());
    }
}

#[test]
fn no_workspaces() {
    let dir = tree(&[
        ("package.json", r#"{"name": "solo"}"#),
        ("packages/a/package.json", &pkg("a", json!({}))),
    ]);
    assert!(Workspace::load(dir.path()).unwrap().members.is_empty());
    // `workspaces` of another shape means no workspaces, like bun
    let dir = tree(&[("package.json", r#"{"workspaces": "packages/*"}"#)]);
    assert!(Workspace::load(dir.path()).unwrap().members.is_empty());
}

#[test]
fn missing_root_is_no_project() {
    let dir = tree(&[]);
    assert!(matches!(
        Workspace::load(dir.path()),
        Err(Error::NoProject(_))
    ));
}

#[test]
fn nameless_members_are_skipped_with_a_warning_when_they_declare_dependencies() {
    // bun: describe("workspace member package.json without a name")
    let dir = tree(&[
        (
            "package.json",
            &root(json!(["packages/*", "packages/nodeps"])),
        ),
        ("packages/good/package.json", &pkg("good", json!({}))),
        (
            "packages/nameless/package.json",
            r#"{"dependencies": {"doesnt-exist-oops": "1.2.3"}}"#,
        ),
        (
            "packages/empty-name/package.json",
            r#"{"name": "", "devDependencies": {"x": "1"}}"#,
        ),
        ("packages/nodeps/package.json", r#"{"version": "1.0.0"}"#),
    ]);
    let (w, warnings) = Workspace::load_with(dir.path(), &LoadOptions::default()).unwrap();
    assert_eq!(paths(&w), ["packages/good"]);
    assert_eq!(
        warnings,
        [
            "skipping workspace \"packages/empty-name\": package.json has no \"name\"",
            "skipping workspace \"packages/nameless\": package.json has no \"name\"",
        ]
    );
}

#[test]
fn skips_node_modules_git_and_hidden_directories() {
    let dir = tree(&[
        ("package.json", &root(json!(["packages/**"]))),
        ("packages/a/package.json", &pkg("a", json!({}))),
        (
            "packages/a/nested/package.json",
            &pkg("a-nested", json!({})),
        ),
        (
            "packages/a/node_modules/dep/package.json",
            &pkg("dep", json!({})),
        ),
        ("packages/.hidden/package.json", &pkg("hidden", json!({}))),
        ("packages/.git/package.json", &pkg("git", json!({}))),
        (
            "packages/CMakeFiles/x/package.json",
            &pkg("cmake", json!({})),
        ),
        ("packages/no-manifest/readme.md", "x"),
    ]);
    assert_eq!(
        paths(&Workspace::load(dir.path()).unwrap()),
        ["packages/a", "packages/a/nested"]
    );
    // an explicit dot segment does match
    let dir = tree(&[
        ("package.json", &root(json!(["packages/.*"]))),
        ("packages/.hidden/package.json", &pkg("hidden", json!({}))),
    ]);
    assert_eq!(
        paths(&Workspace::load(dir.path()).unwrap()),
        ["packages/.hidden"]
    );
}

#[test]
fn root_matched_by_a_glob_is_not_a_member() {
    let dir = tree(&[
        ("package.json", &root(json!(["**", ".", "./"]))),
        ("a/package.json", &pkg("a", json!({}))),
    ]);
    assert_eq!(paths(&Workspace::load(dir.path()).unwrap()), ["a"]);
}

/// filter-workspace.test.ts: `bun run and bun install find the same members for ...`
fn negation_fixture(workspaces: serde_json::Value) -> tempfile::TempDir {
    tree(&[
        (
            "package.json",
            &json!({ "name": "ws", "workspaces": workspaces }).to_string(),
        ),
        (
            "packages/app/package.json",
            &pkg("app", json!({ "dependencies": { "lib": "workspace:*" } })),
        ),
        ("packages/lib/package.json", &pkg("lib", json!({}))),
        (
            "packages/legacy/package.json",
            &pkg(
                "legacy",
                json!({ "dependencies": { "lib": "workspace:*" } }),
            ),
        ),
        (
            "packages/legacy/nested/package.json",
            &pkg("legacy-nested", json!({})),
        ),
        ("other/legacy/package.json", &pkg("other-legacy", json!({}))),
        ("other/lib/package.json", &pkg("other-lib", json!({}))),
    ])
}

fn negation_configs() -> Vec<(serde_json::Value, Vec<&'static str>)> {
    let app_and_lib = vec!["packages/app", "packages/lib"];
    vec![
        (
            json!({ "packages": ["packages/*", "!packages/legacy"] }),
            app_and_lib.clone(),
        ),
        (json!(["packages/*", "!**/legacy"]), app_and_lib.clone()),
        (
            json!(["packages/*", "!packages/{app,legacy}"]),
            vec!["packages/lib"],
        ),
        (
            json!(["packages/**", "!packages/legacy"]),
            vec!["packages/app", "packages/legacy/nested", "packages/lib"],
        ),
        (
            json!(["./packages/*", "!./packages/legacy"]),
            app_and_lib.clone(),
        ),
        (
            json!(["packages/*", "!./packages/legacy"]),
            app_and_lib.clone(),
        ),
        (
            json!(["packages/*", "!packages/legacy/"]),
            app_and_lib.clone(),
        ),
        (
            json!(["packages/*", "!././packages/legacy//"]),
            app_and_lib.clone(),
        ),
        (
            json!(["!packages/legacy", "packages/*"]),
            app_and_lib.clone(),
        ),
        (
            json!(["!./packages/legacy/", "packages/*"]),
            app_and_lib.clone(),
        ),
        (
            json!(["packages/*", "!packages/legacy", "packages/leg*"]),
            app_and_lib.clone(),
        ),
        (
            json!(["packages/**", "!packages/legacy/**"]),
            app_and_lib.clone(),
        ),
        (
            json!(["packages/*", "!packages/leg*/**"]),
            app_and_lib.clone(),
        ),
        (
            json!(["packages/lib", "!packages/lib"]),
            vec!["packages/lib"],
        ),
    ]
}

#[test]
fn negated_entries() {
    for (workspaces, members) in negation_configs() {
        let dir = negation_fixture(workspaces.clone());
        let w = Workspace::load(dir.path()).unwrap();
        assert_eq!(paths(&w), members, "{workspaces}");
    }
}

#[test]
fn missing_literal_workspace() {
    // bad-workspace.test.ts: "bad workspace path"
    let dir = tree(&[("package.json", &root(json!(["i-dont-exist"])))]);
    let err = Workspace::load(dir.path()).unwrap_err();
    assert_eq!(err.to_string(), "Workspace not found \"i-dont-exist\"");
    let opts = LoadOptions {
        missing: MissingWorkspace::Skip,
    };
    assert!(
        Workspace::load_with(dir.path(), &opts)
            .unwrap()
            .0
            .members
            .is_empty()
    );
    // a directory without package.json is just as missing
    let dir = tree(&[("package.json", &root(json!(["pkg"]))), ("pkg/readme", "")]);
    assert!(matches!(Workspace::load(dir.path()), Err(Error::WorkspaceNotFound(e)) if e == "pkg"));
}

#[test]
fn glob_under_a_missing_directory() {
    // relative: nothing matches
    let dir = tree(&[("package.json", &root(json!(["missing/*"])))]);
    assert!(Workspace::load(dir.path()).unwrap().members.is_empty());
    // absolute: bun opens the literal prefix and reports it
    let dir = tree(&[]);
    let entry = format!("{}/missing/*", dir.path().display());
    write(dir.path(), "package.json", &root(json!([entry])));
    let err = Workspace::load(dir.path()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Failed to run workspace pattern {entry} due to error ENOENT (open \"{}\")",
            dir.path().join("missing").display()
        )
    );
}

#[cfg(unix)]
#[test]
fn glob_into_unreadable_directories() {
    use std::os::unix::fs::PermissionsExt;
    for (mode, syscall, failed_on) in [
        (0o000, "open", "secret"),
        (0o444, "fstatat", "secret/package.json"),
    ] {
        let dir = tree(&[
            ("package.json", &root(json!(["pkgs/*"]))),
            ("pkgs/pkg1/package.json", &pkg("pkg1", json!({}))),
            ("pkgs/secret/package.json", &pkg("secret", json!({}))),
        ]);
        let secret = dir.path().join("pkgs/secret");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(mode)).unwrap();
        // Root ignores the mode bits.
        let as_root = std::fs::read_to_string(secret.join("package.json")).is_ok();
        let result = Workspace::load(dir.path());
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();
        if as_root {
            continue;
        }
        assert_eq!(
            result.unwrap_err().to_string(),
            format!(
                "Failed to run workspace pattern pkgs/* due to error EACCES ({syscall} \"{}\")",
                dir.path().join("pkgs").join(failed_on).display()
            )
        );
    }
}

#[test]
fn non_string_entries_and_bad_shapes() {
    // bad-workspace.test.ts: "non-string workspaces entry"
    let dir = tree(&[("package.json", r#"{"name": "hey", "workspaces": [123]}"#)]);
    let err = Workspace::load(dir.path()).unwrap_err().to_string();
    assert!(
        err.ends_with("Workspaces expects an array of strings, like:\n  \"workspaces\": [\n    \"path/to/package\"\n  ]"),
        "{err}"
    );
    assert!(
        err.starts_with(&dir.path().join("package.json").display().to_string()),
        "{err}"
    );
    let dir = tree(&[("package.json", r#"{"workspaces": {"packages": "x"}}"#)]);
    let err = Workspace::load(dir.path()).unwrap_err().to_string();
    assert!(
        err.contains("\"workspaces.packages\" expects an array of strings"),
        "{err}"
    );
}

#[test]
fn duplicate_names() {
    let dir = tree(&[
        ("package.json", &root(json!(["packages/*"]))),
        ("packages/a/package.json", &pkg("same", json!({}))),
        ("packages/b/package.json", &pkg("same", json!({}))),
    ]);
    let e = Workspace::load(dir.path()).unwrap_err();
    assert_eq!(e.to_string(), "Workspace name \"same\" already exists");
    let Error::DuplicateName { first, second, .. } = e else {
        panic!("{e}")
    };
    assert_eq!(
        (first.as_str(), second.as_str()),
        ("packages/a", "packages/b")
    );
}

#[test]
fn invalid_and_empty_member_manifests() {
    let dir = tree(&[
        ("package.json", &root(json!(["packages/*"]))),
        ("packages/a/package.json", "asdfsadfas"),
    ]);
    assert!(
        matches!(Workspace::load(dir.path()), Err(Error::InvalidPackageJson { path, .. }) if path.ends_with("packages/a/package.json"))
    );
    let dir = tree(&[
        ("package.json", &root(json!(["packages/*"]))),
        ("packages/a/package.json", ""),
    ]);
    let err = Workspace::load(dir.path()).unwrap_err().to_string();
    assert!(err.ends_with("file is empty"), "{err}");
    let opts = LoadOptions {
        missing: MissingWorkspace::SkipEmptyToo,
    };
    assert!(
        Workspace::load_with(dir.path(), &opts)
            .unwrap()
            .0
            .members
            .is_empty()
    );
}

#[test]
fn jsonc_manifests() {
    let dir = tree(&[
        (
            "package.json",
            "{\n  // comment\n  \"workspaces\": [\"packages/*\",],\n}\n",
        ),
        ("packages/a/package.json", "{ /* c */ \"name\": \"a\", }"),
    ]);
    assert_eq!(paths(&Workspace::load(dir.path()).unwrap()), ["packages/a"]);
}

#[test]
fn workspaces_outside_the_project() {
    // bun-workspaces.test.ts: describe("a workspace outside the project")
    let dir = tree(&[
        (
            "project/package.json",
            &root(json!(["../ext/*", "../lib", "packages/*"])),
        ),
        ("project/packages/a/package.json", &pkg("a", json!({}))),
        ("ext/x/package.json", &pkg("x", json!({}))),
        ("lib/package.json", &pkg("lib", json!({}))),
    ]);
    let w = Workspace::load(&dir.path().join("project")).unwrap();
    assert_eq!(paths(&w), ["../ext/x", "../lib", "packages/a"]);
    assert_eq!(w.member("lib").unwrap().dir, dir.path().join("lib"));
}

#[test]
fn absolute_glob_entries() {
    let dir = tree(&[("packages/a/package.json", &pkg("a", json!({})))]);
    let entry = format!("{}/packages/*", dir.path().display());
    write(dir.path(), "package.json", &root(json!([entry])));
    assert_eq!(paths(&Workspace::load(dir.path()).unwrap()), ["packages/a"]);
}

#[cfg(unix)]
#[test]
fn symlinked_members_and_loops() {
    // filter-workspace.test.ts: "self-referential directory symlink in a workspace does not loop"
    let dir = tree(&[
        ("package.json", &root(json!(["packages/**"]))),
        ("packages/pkga/package.json", &pkg("pkga", json!({}))),
        ("packages/cyc/package.json", &pkg("cyc", json!({}))),
        ("real/linked/package.json", &pkg("linked", json!({}))),
    ]);
    std::os::unix::fs::symlink(
        dir.path().join("packages/cyc"),
        dir.path().join("packages/cyc/loop"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        dir.path().join("real/linked"),
        dir.path().join("packages/linked"),
    )
    .unwrap();
    // `cyc` is matched once more through `loop` (and the walk stops there), which makes its
    // name a duplicate.
    let err = Workspace::load(dir.path()).unwrap_err();
    assert!(
        matches!(err, Error::DuplicateName { ref name, .. } if name == "cyc"),
        "{err}"
    );
    std::fs::remove_file(dir.path().join("packages/cyc/loop")).unwrap();
    let w = Workspace::load(dir.path()).unwrap();
    assert_eq!(
        paths(&w),
        ["packages/cyc", "packages/linked", "packages/pkga"]
    );
}

#[test]
fn many_members() {
    let mut files: Vec<(String, String)> = vec![(
        "package.json".into(),
        root(json!(["packages/*", "apps/*/*"])),
    )];
    for i in 0..150 {
        files.push((
            format!("packages/p{i:03}/package.json"),
            pkg(&format!("p{i:03}"), json!({ "version": "1.0.0" })),
        ));
    }
    for i in 0..20 {
        files.push((
            format!("apps/g{}/a{i}/package.json", i % 3),
            pkg(&format!("a{i}"), json!({})),
        ));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let dir = tree(&refs);
    let w = Workspace::load(dir.path()).unwrap();
    assert_eq!(w.members.len(), 170);
    let mut sorted = paths(&w);
    sorted.sort();
    assert_eq!(paths(&w), sorted);
}

#[test]
fn pnpm_workspace_yaml() {
    let dir = tree(&[
        (
            "package.json",
            r#"{"name": "root", "pnpm": {"onlyBuiltDependencies": ["esbuild"]}}"#,
        ),
        (
            "pnpm-workspace.yaml",
            "packages:\n  - 'packages/*'\n  - '!packages/skip'\ncatalog:\n  react: ^18.0.0\n  bad: [1]\ncatalogs:\n  legacy:\n    react: ^16.0.0\n  default:\n    lodash: ^4\noverrides:\n  foo: 1.0.0\n  bar: npm:baz@2\nonlyBuiltDependencies:\n  - sharp\n  - esbuild\n",
        ),
        ("packages/a/package.json", &pkg("a", json!({}))),
        ("packages/skip/package.json", &pkg("skip", json!({}))),
    ]);
    let (w, warnings) = Workspace::load_with(dir.path(), &LoadOptions::default()).unwrap();
    assert_eq!(paths(&w), ["packages/a"]);
    assert_eq!(w.settings.catalogs.get("", "react"), Some("^18.0.0"));
    assert_eq!(w.settings.catalogs.get("default", "lodash"), Some("^4"));
    assert_eq!(w.settings.catalogs.get("legacy", "react"), Some("^16.0.0"));
    assert_eq!(w.settings.catalogs.get("", "bad"), None);
    assert_eq!(
        w.settings.overrides,
        [
            ("foo".to_owned(), "1.0.0".to_owned()),
            ("bar".to_owned(), "npm:baz@2".to_owned())
        ]
    );
    assert_eq!(
        w.settings.only_built_dependencies,
        Some(vec!["esbuild".to_owned(), "sharp".to_owned()])
    );
    assert_eq!(
        warnings,
        ["skipping non-string entries of \"catalog\" in pnpm-workspace.yaml (1)"]
    );
}

#[test]
fn package_json_workspaces_win_over_pnpm_yaml_packages() {
    let dir = tree(&[
        (
            "package.json",
            &json!({ "workspaces": { "packages": ["apps/*"], "catalog": { "react": "^19" } } })
                .to_string(),
        ),
        (
            "pnpm-workspace.yaml",
            "packages: ['packages/*']\ncatalog:\n  react: ^18\n  vue: ^3\n",
        ),
        ("packages/a/package.json", &pkg("a", json!({}))),
        ("apps/b/package.json", &pkg("b", json!({}))),
    ]);
    let w = Workspace::load(dir.path()).unwrap();
    assert_eq!(paths(&w), ["apps/b"]);
    assert_eq!(w.settings.catalogs.get("", "react"), Some("^19"));
    assert_eq!(w.settings.catalogs.get("", "vue"), Some("^3"));
    assert_eq!(w.settings.only_built_dependencies, None);
}

#[test]
fn package_json_catalogs() {
    // catalogs.test.ts: catalogs in `workspaces`, or top-level when `workspaces` is an array
    let dir = tree(&[(
        "package.json",
        &json!({
            "workspaces": { "packages": [], "catalog": { "a": "1" }, "catalogs": { "x": { "b": "2", "n": 3 } } },
            "catalog": { "ignored": "1" }
        })
        .to_string(),
    )]);
    let w = Workspace::load(dir.path()).unwrap();
    assert_eq!(w.settings.catalogs.get("", "a"), Some("1"));
    assert_eq!(w.settings.catalogs.get("x", "b"), Some("2"));
    assert_eq!(w.settings.catalogs.get("x", "n"), None);
    assert_eq!(w.settings.catalogs.get("", "ignored"), None);

    let dir = tree(&[(
        "package.json",
        &json!({ "workspaces": [], "catalog": { "top": "1" }, "catalogs": { "default": { "d": "2" } } }).to_string(),
    )]);
    let w = Workspace::load(dir.path()).unwrap();
    assert_eq!(w.settings.catalogs.get("", "top"), Some("1"));
    assert_eq!(w.settings.catalogs.get("default", "d"), Some("2"));

    // without `workspaces`, top-level catalogs are not read
    let dir = tree(&[(
        "package.json",
        &json!({ "catalog": { "top": "1" } }).to_string(),
    )]);
    assert!(
        Workspace::load(dir.path())
            .unwrap()
            .settings
            .catalogs
            .is_empty()
    );

    let dir = tree(&[(
        "package.json",
        &json!({ "workspaces": { "catalog": { "a": "1" }, "catalogs": { "default": { "a": "2" } } } }).to_string(),
    )]);
    let err = Workspace::load(dir.path()).unwrap_err().to_string();
    assert!(
        err.ends_with(
            "\"a\" is defined in both \"catalog\" and \"catalogs.default\"; keep one of them"
        ),
        "{err}"
    );
}

#[test]
fn find_root() {
    let dir = tree(&[
        (
            "package.json",
            &root(json!(["packages/*", "!packages/excluded"])),
        ),
        ("packages/a/package.json", &pkg("a", json!({}))),
        ("packages/a/src/deep/file.txt", ""),
        (
            "packages/excluded/package.json",
            &pkg("excluded", json!({})),
        ),
        ("other/package.json", &pkg("other", json!({}))),
        ("no-manifest/x/file.txt", ""),
    ]);
    let r = dir.path();
    assert_eq!(Workspace::find_root(r).unwrap(), r);
    assert_eq!(Workspace::find_root(&r.join("packages/a")).unwrap(), r);
    assert_eq!(
        Workspace::find_root(&r.join("packages/a/src/deep")).unwrap(),
        r
    );
    assert_eq!(
        Workspace::find_root(&r.join("packages/excluded")).unwrap(),
        r.join("packages/excluded")
    );
    assert_eq!(
        Workspace::find_root(&r.join("other")).unwrap(),
        r.join("other")
    );
    assert_eq!(Workspace::find_root(&r.join("no-manifest/x")).unwrap(), r);
}

#[test]
fn find_root_skips_ancestors_without_workspaces_and_stops_at_the_first_with_them() {
    let dir = tree(&[
        ("package.json", &root(json!(["mid/inner/*"]))),
        ("mid/package.json", &root(json!(["elsewhere/*"]))),
        ("mid/inner/pkg/package.json", &pkg("pkg", json!({}))),
        ("top/package.json", &root(json!(["sub/plain/pkg"]))),
        ("top/sub/package.json", r#"{"name": "no-workspaces"}"#),
        ("top/sub/plain/pkg/package.json", &pkg("p", json!({}))),
    ]);
    let r = dir.path();
    // `mid` declares workspaces that do not list the package, so the package is its own root.
    assert_eq!(
        Workspace::find_root(&r.join("mid/inner/pkg")).unwrap(),
        r.join("mid/inner/pkg")
    );
    assert_eq!(
        Workspace::find_root(&r.join("top/sub/plain/pkg")).unwrap(),
        r.join("top")
    );
}

#[test]
fn find_root_with_pnpm_workspace_yaml() {
    let dir = tree(&[
        ("package.json", r#"{"name": "root"}"#),
        ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
        ("packages/a/package.json", &pkg("a", json!({}))),
    ]);
    assert_eq!(
        Workspace::find_root(&dir.path().join("packages/a")).unwrap(),
        dir.path()
    );
}

#[test]
fn find_root_without_package_json() {
    let dir = tree(&[("x/file", "")]);
    match Workspace::find_root(&dir.path().join("x")) {
        Err(Error::NoProject(p)) => assert_eq!(p, dir.path().join("x")),
        // Some ancestor of the temp dir has a package.json.
        Ok(p) => assert!(!p.starts_with(dir.path())),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn member_for_dir() {
    let dir = tree(&[
        ("package.json", &root(json!(["packages/**"]))),
        ("packages/a/package.json", &pkg("a", json!({}))),
        ("packages/a/nested/package.json", &pkg("nested", json!({}))),
    ]);
    let w = Workspace::load(dir.path()).unwrap();
    assert_eq!(
        w.member_for_dir(&dir.path().join("packages/a/src"))
            .unwrap()
            .name,
        "a"
    );
    assert_eq!(
        w.member_for_dir(&dir.path().join("packages/a/nested/x"))
            .unwrap()
            .name,
        "nested"
    );
    assert!(w.member_for_dir(dir.path()).is_none());
    assert!(w.member_for_dir(&dir.path().join("packages")).is_none());
}

#[test]
fn topo_levels() {
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "root", "workspaces": ["packages/*"], "dependencies": { "app": "workspace:*" } }).to_string(),
        ),
        ("packages/app/package.json", &pkg("app", json!({ "dependencies": { "lib": "workspace:*" } }))),
        ("packages/lib/package.json", &pkg("lib", json!({ "devDependencies": { "util": "*" } }))),
        ("packages/util/package.json", &pkg("util", json!({ "peerDependencies": { "app": "*" } }))),
        ("packages/lone/package.json", &pkg("lone", json!({}))),
    ]);
    let w = Workspace::load(dir.path()).unwrap();
    let idx = |n: &str| w.members.iter().position(|m| m.name == n).unwrap();
    let all: Vec<usize> = std::iter::once(usize::MAX)
        .chain(0..w.members.len())
        .collect();
    let (levels, cycles) = w.topo_levels(&all);
    assert!(cycles.is_empty());
    // peers are not ordering edges for `run --filter`
    assert_eq!(
        levels,
        vec![
            vec![idx("lone"), idx("util")],
            vec![idx("lib")],
            vec![idx("app")],
            vec![usize::MAX]
        ]
    );
    // only edges inside the set count
    let (levels, _) = w.topo_levels(&[idx("app"), idx("util")]);
    assert_eq!(levels, vec![vec![idx("app"), idx("util")]]);
    // publish order: the relation graph also follows peers (util -> app), a cycle broken somewhere
    let order = w.dependency_order(&[idx("lib"), idx("lone"), idx("app")], &Default::default());
    assert_eq!(order, vec![idx("lib"), idx("lone"), idx("app")]);
}

#[test]
fn topo_levels_with_a_cycle_drop_the_order() {
    // filter-workspace.test.ts: "ignore dependency order on cycle"
    let dir = tree(&[
        ("package.json", &root(json!(["dep0", "dep1", "dep2"]))),
        (
            "dep0/package.json",
            &pkg("dep0", json!({ "dependencies": { "dep1": "*" } })),
        ),
        (
            "dep1/package.json",
            &pkg("dep1", json!({ "dependencies": { "dep0": "*" } })),
        ),
        (
            "dep2/package.json",
            &pkg("dep2", json!({ "dependencies": { "dep0": "*" } })),
        ),
    ]);
    let w = Workspace::load(dir.path()).unwrap();
    let (levels, cycles) = w.topo_levels(&[0, 1, 2]);
    assert_eq!(levels, vec![vec![0, 1, 2]]);
    assert_eq!(cycles, vec![vec![0, 1]]);
    // publish order still places every package and puts dep2 after dep0
    let order = w.dependency_order(&[0, 1, 2], &Default::default());
    assert_eq!(order.len(), 3);
    assert!(order.iter().position(|&x| x == 0) < order.iter().position(|&x| x == 2));
}

/// `Workspace::load` for 100 members, warm cache. Budget (ARCHITECTURE.md): < 5 ms.
/// Run with `cargo test --release -p fnpm-workspace --test discovery perf -- --ignored --nocapture`.
#[test]
#[ignore]
fn perf_load_100_members() {
    let mut files: Vec<(String, String)> =
        vec![("package.json".into(), root(json!(["packages/*"])))];
    for i in 0..100 {
        let deps = json!({ "version": "1.0.0", "dependencies": { "react": "^18", "lodash": "^4", format!("p{:03}", (i + 1) % 100): "workspace:*" }, "scripts": { "build": "tsc" } });
        files.push((
            format!("packages/p{i:03}/package.json"),
            pkg(&format!("p{i:03}"), deps),
        ));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let dir = tree(&refs);
    Workspace::load(dir.path()).unwrap();
    let runs = 50;
    let start = std::time::Instant::now();
    for _ in 0..runs {
        assert_eq!(Workspace::load(dir.path()).unwrap().members.len(), 100);
    }
    let per = start.elapsed() / runs;
    let w = Workspace::load(dir.path()).unwrap();
    let selectors = fnpm_workspace::Selector::parse_all(&["...p050"]).unwrap();
    let start = std::time::Instant::now();
    let selected = fnpm_workspace::select(&w, &selectors, dir.path()).unwrap();
    let select_time = start.elapsed();
    println!(
        "load(100 members): {per:?} per run; select(...p050): {select_time:?} ({} selected)",
        selected.len()
    );
}

#[test]
fn root_path_with_glob_characters_matches_literally() {
    let outer = tempfile::tempdir().unwrap();
    for name in ["app[1]", "a{b,c}", "x*y", "q?", "a+(b)", "!neg"] {
        let r = outer.path().join(name);
        common::write(
            &r,
            "package.json",
            &root(json!(["packages/*", "!packages/skip"])),
        );
        common::write(&r, "packages/a/package.json", &pkg("a", json!({})));
        common::write(&r, "packages/skip/package.json", &pkg("skip", json!({})));
        let ws = Workspace::load(&r).unwrap();
        assert_eq!(paths(&ws), ["packages/a"], "{name}");
        assert_eq!(
            Workspace::find_root(&r.join("packages/a")).unwrap(),
            r,
            "{name}"
        );
        assert_eq!(
            Workspace::find_root(&r.join("packages/skip")).unwrap(),
            r.join("packages/skip"),
            "{name}"
        );
    }
}

#[test]
fn find_root_keeps_a_nameless_or_unreadable_package_as_its_own_root() {
    let dir = tree(&[
        ("package.json", &root(json!(["packages/*"]))),
        (
            "packages/nameless/package.json",
            r#"{"dependencies": {"x": "1"}}"#,
        ),
        ("packages/empty-name/package.json", r#"{"name": ""}"#),
        ("packages/broken/package.json", "{not json"),
        ("packages/ok/package.json", &pkg("ok", json!({}))),
    ]);
    let r = dir.path();
    for d in [
        "packages/nameless",
        "packages/empty-name",
        "packages/broken",
    ] {
        assert_eq!(Workspace::find_root(&r.join(d)).unwrap(), r.join(d), "{d}");
    }
    assert_eq!(Workspace::find_root(&r.join("packages/ok")).unwrap(), r);
}

/// bun `is_shared_directory`: a package.json in a sticky, world-writable directory above the
/// project (like `/tmp`) could have been planted by another user and is ignored.
#[cfg(unix)]
#[test]
fn find_root_ignores_package_json_in_shared_directories() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tree(&[
        ("shared/package.json", &root(json!(["**"]))),
        ("shared/proj/package.json", &pkg("proj", json!({}))),
        ("shared/loose/file.txt", ""),
    ]);
    let shared = dir.path().join("shared");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    assert_eq!(
        Workspace::find_root(&shared.join("proj")).unwrap(),
        shared.join("proj")
    );
    // Not as the nearest package.json either.
    assert!(matches!(
        Workspace::find_root(&shared.join("loose")),
        Err(Error::NoProject(_))
    ));
    // The cwd itself is trusted.
    assert_eq!(Workspace::find_root(&shared).unwrap(), shared);
    // Sticky but not group/other-writable is fine.
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1755)).unwrap();
    assert_eq!(Workspace::find_root(&shared.join("proj")).unwrap(), shared);
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
}
