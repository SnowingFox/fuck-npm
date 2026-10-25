//! Reader behaviour ported from bun's `test/cli/install/bun-lock.test.ts`,
//! `lockfile-version-2.test.ts` and `config-version.test.ts`.

use fnpm_types::{Behavior, ConfigVersion, PackageId, Resolution};

use crate::{Error, ParseOptions};

fn parse(src: &str) -> Result<(fnpm_types::LockGraph, Vec<String>), String> {
    crate::text_parse(src.as_bytes(), "bun.lock", &ParseOptions::default()).map_err(|e| match e {
        Error::Parse { message, .. } => message,
        other => other.to_string(),
    })
}

fn err(src: &str) -> String {
    parse(src).map(|_| ()).unwrap_err()
}

fn with_overrides(overrides: &str) -> String {
    format!(
        "{{\n  \"lockfileVersion\": 1,\n  \"configVersion\": 1,\n  \"workspaces\": {{\n    \"\": {{\n      \"name\": \"invalid-overrides\"\n    }}\n  }},\n  \"overrides\": {overrides},\n  \"packages\": {{}}\n}}"
    )
}

#[test]
fn hand_edited_overrides_are_rejected_at_the_right_place() {
    // "rejects a top-level row whose value is a number"
    assert_eq!(
        err(&with_overrides("{\n    \"no-deps\": 1\n  }")),
        "10:16: Expected a string or an object"
    );
    // "rejects a group whose key is a bare scope"
    assert_eq!(
        err(&with_overrides(
            "{\n    \"@scope\": {\n      \".\": \"1.0.0\"\n    }\n  }"
        )),
        "10:5: Invalid override key"
    );
    // "rejects a group child whose key is a bare scope"
    assert_eq!(
        err(&with_overrides(
            "{\n    \"no-deps\": {\n      \"@scope\": \"1.0.0\"\n    }\n  }"
        )),
        "11:7: Invalid override key"
    );
    // "rejects a group child whose value is a number"
    assert_eq!(
        err(&with_overrides(
            "{\n    \"no-deps\": {\n      \"a-dep\": 1\n    }\n  }"
        )),
        "11:16: Expected a string"
    );
    // "rejects a group whose key carries a non-npm range"
    assert_eq!(
        err(&with_overrides(
            "{\n    \"no-deps@file:./vendored\": {\n      \".\": \"1.0.0\"\n    }\n  }"
        )),
        "11:12: Invalid override version"
    );
    // "rejects a group child whose value does not parse as a dependency"
    assert_eq!(
        err(&with_overrides(
            "{\n    \"no-deps\": {\n      \"a-dep\": \"./a:dep\"\n    }\n  }"
        )),
        "11:16: Invalid override version"
    );
}

#[test]
fn scoped_overrides_read_into_rules() {
    let (g, _) = parse(&with_overrides(
        "{\n    \"a\": \"1.0.0\",\n    \"b@^2\": {\n      \".\": \"2.1.0\",\n      \"c\": \"3.0.0\"\n    }\n  }",
    ))
    .unwrap();
    let keys: Vec<(String, String)> = g
        .overrides
        .iter()
        .map(|r| (r.selector.to_key(), r.value.clone()))
        .collect();
    assert_eq!(
        keys,
        [
            ("a".into(), "1.0.0".into()),
            ("b@^2".into(), "2.1.0".into()),
            ("b@^2>c".into(), "3.0.0".into())
        ]
    );
}

#[test]
fn unsupported_versions_name_the_supported_range() {
    let e = err(
        "{\"lockfileVersion\": 99, \"workspaces\": {\"\": {\"name\": \"future-lockfile\"}}, \"packages\": {}}",
    );
    assert!(e.contains("Unsupported lockfile version 99"), "{e}");
    assert!(e.contains("newer version of fnpm"), "{e}");
    assert!(e.contains("supports lockfile versions up to 3"), "{e}");
    assert_eq!(err("{\"workspaces\": {}}"), "1:1: Missing lockfile version");
    assert_eq!(
        err("{\"lockfileVersion\": \"1\", \"workspaces\": {}}"),
        "1:21: Invalid lockfile version"
    );
    assert_eq!(
        err("{\"lockfileVersion\": 1.5, \"workspaces\": {}}"),
        "1:21: Invalid lockfile version"
    );
    assert_eq!(
        err("{\"lockfileVersion\": 1}"),
        "1:1: Missing a workspaces object property"
    );
    assert_eq!(
        err("{\"lockfileVersion\": 1, \"workspaces\": {\"a\": {\"name\": \"a\"}}}"),
        "1:38: Expected root package"
    );
    assert_eq!(err("{ this is not json"), "1:3: Expected a string key");
}

fn no_deps_lock(version: u32, url: &str, integrity: &str) -> String {
    format!(
        r#"{{"lockfileVersion": {version}, "configVersion": 1, "workspaces": {{"": {{"name": "redirected-tarball-url", "dependencies": {{"no-deps": "1.0.0"}}}}}}, "packages": {{"no-deps": ["no-deps@1.0.0", "{url}", {{}}, "{integrity}"]}}}}"#
    )
}

#[test]
fn off_registry_tarballs_need_an_integrity_at_v2() {
    let off = "http://127.0.0.1:9/no-deps/-/no-deps-1.0.0.tgz";
    let e = err(&no_deps_lock(2, off, ""));
    assert!(e.ends_with("Missing integrity hash for npm package resolved to a tarball URL outside the configured registry"), "{e}");
    // lockfile-version-2.test.ts: v1 keeps loading.
    assert!(parse(&no_deps_lock(1, off, "")).is_ok());
    // Under the configured registry no integrity is needed.
    let opts = ParseOptions {
        default_registry: Some("http://127.0.0.1:9/".into()),
        ..Default::default()
    };
    assert!(crate::text_parse(no_deps_lock(2, off, "").as_bytes(), "bun.lock", &opts).is_ok());
    // Under a scope's registry too.
    let scoped = r#"{"lockfileVersion": 2, "workspaces": {"": {"dependencies": {"@s/x": "1.0.0"}}}, "packages": {"@s/x": ["@s/x@1.0.0", "https://npm.example.com/@s/x/-/x-1.0.0.tgz", {}, ""]}}"#;
    assert!(parse(scoped).is_err());
    let opts = ParseOptions {
        scope_registries: [("s".to_owned(), "https://npm.example.com/".to_owned())].into(),
        ..Default::default()
    };
    assert!(crate::text_parse(scoped.as_bytes(), "bun.lock", &opts).is_ok());
    // The default registry's own URLs need nothing.
    assert!(
        parse(&no_deps_lock(
            2,
            "https://registry.npmjs.org/no-deps/-/no-deps-1.0.0.tgz",
            ""
        ))
        .is_ok()
    );
}

#[test]
fn malformed_integrity_is_an_error_at_v2_and_a_warning_before() {
    let e = err(&no_deps_lock(2, "", "sha512-!!!"));
    assert!(
        e.ends_with("Unsupported or malformed integrity hash"),
        "{e}"
    );
    let (g, warnings) = parse(&no_deps_lock(1, "", "sha512-!!!")).unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].ends_with("Unsupported or malformed integrity hash; ignoring"));
    assert_eq!(g.packages[1].resolution.integrity(), None);
}

#[test]
fn config_version_rules() {
    let lock = |cv: &str| format!(r#"{{"lockfileVersion": 1, {cv} "workspaces": {{"": {{}}}}}}"#);
    assert_eq!(
        parse(&lock("")).unwrap().0.config_version,
        ConfigVersion::V0
    );
    assert_eq!(
        parse(&lock(r#""configVersion": 0,"#))
            .unwrap()
            .0
            .config_version,
        ConfigVersion::V0
    );
    assert_eq!(
        parse(&lock(r#""configVersion": 1,"#))
            .unwrap()
            .0
            .config_version,
        ConfigVersion::V1
    );
    assert_eq!(
        parse(&lock(r#""configVersion": 7,"#))
            .unwrap()
            .0
            .config_version,
        ConfigVersion::V1
    );
    assert!(
        err(&lock(r#""configVersion": "1","#))
            .ends_with("Invalid \"configVersion\". Expected a number")
    );
    assert!(
        err(&lock(r#""configVersion": 1.5,"#))
            .ends_with("Invalid \"configVersion\". Expected a number")
    );
}

#[test]
fn a_missing_packages_object_is_an_empty_one() {
    let src = "{\n  \"lockfileVersion\": 2,\n  \"workspaces\": {\n    \"\": {\n      \"name\": \"no-packages-object\"\n    },\n    \"packages/member\": {\n      \"name\": \"member\",\n      \"version\": \"1.0.0\"\n    }\n  }\n}";
    let (g, _) = parse(src).unwrap();
    assert_eq!(g.workspaces.len(), 2);
    let text = super::write::write(&g, super::Dialect::Bun).text;
    assert!(
        text.contains(
            "\"packages\": {\n    \"member\": [\"member@workspace:packages/member\"],\n  }"
        ),
        "{text}"
    );
}

#[test]
fn package_errors() {
    let pkg = |row: &str| {
        format!(
            r#"{{"lockfileVersion": 2, "workspaces": {{"": {{"dependencies": {{"a": "1.0.0"}}}}}}, "packages": {{"a": {row}}}}}"#
        )
    };
    assert!(err(&pkg("1")).ends_with("Expected an array"));
    assert!(err(&pkg("[]")).ends_with("Missing package info"));
    assert!(err(&pkg("[1]")).ends_with("Expected a string"));
    assert!(err(&pkg(r#"["a"]"#)).ends_with("Invalid package resolution"));
    assert!(err(&pkg(r#"["a@1.0"]"#)).ends_with("Unexpected resolution: 1.0"));
    assert!(err(&pkg(r#"["a@1.0.0"]"#)).ends_with("Missing npm registry"));
    assert!(err(&pkg(r#"["a@1.0.0", ""]"#)).ends_with("Missing dependencies object"));
    assert!(err(&pkg(r#"["a@1.0.0", "", []]"#)).ends_with("Expected an object"));
    assert!(err(&pkg(r#"["a@1.0.0", "", {}]"#)).ends_with("Missing integrity"));
    assert!(err(&pkg(r#"["../a@1.0.0", "", {}, ""]"#)).ends_with("invalid package name"));
    assert!(err(&pkg(r#"["a@workspace:nope"]"#)).ends_with("Unknown workspace: 'nope'"));
    assert!(
        err(&pkg(r#"["a@github:o/r#abc", {}, "../evil"]"#)).ends_with("Invalid git dependency tag")
    );
    assert!(
        err(&pkg(r#"["a@git+https://x/r.git#abc", {}]"#)).ends_with("Missing git dependency tag")
    );
    assert!(
        err(&pkg(
            r#"["a@1.0.0", "", {"dependencies": {"b": "./x:y"}}, ""]"#
        ))
        .ends_with("Invalid dependency version")
    );
    // The root's dependency must resolve.
    let missing = r#"{"lockfileVersion": 2, "workspaces": {"": {"dependencies": {"a": "1.0.0"}}}, "packages": {}}"#;
    assert!(err(missing).ends_with("Failed to resolve root prod dependency 'a'"));
    // An optional one may stay unresolved.
    let optional = r#"{"lockfileVersion": 2, "workspaces": {"": {"optionalDependencies": {"a": "1.0.0"}}}, "packages": {}}"#;
    let (g, _) = parse(optional).unwrap();
    assert_eq!(g.root().dependencies[0].target, None);
    // Duplicate paths.
    let dup = r#"{"lockfileVersion": 2, "workspaces": {"": {"dependencies": {"a": "1.0.0"}}}, "packages": {"a": ["a@1.0.0", "", {}, ""], "a": ["a@2.0.0", "", {}, ""]}}"#;
    assert!(err(dup).ends_with("Duplicate package path"));
    // Git tags are only checked from v2 on (github always).
    let v1_git = r#"{"lockfileVersion": 1, "workspaces": {"": {"dependencies": {"a": "git+https://x/r.git"}}}, "packages": {"a": ["a@git+https://x/r.git#abc", {}, "../x"]}}"#;
    assert!(parse(v1_git).is_ok());
}

#[test]
fn binds_like_bun() {
    let src = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "app", "dependencies": { "a": "^1", "lib": "workspace:*" }, "devDependencies": { "d": "1" } },
    "packages/lib": { "name": "lib", "version": "2.0.0", "dependencies": { "a": "^2" }, "peerDependencies": { "p": "^1" }, "optionalPeers": ["p"] },
  },
  "packages": {
    "a": ["a@1.0.0", "", { "dependencies": { "b": "1" }, "os": "darwin", "cpu": ["x64", "arm64"], "bin": { "a": "cli.js" }, "hasInstallScript": true }, ""],
    "b": ["b@1.0.0", "", {}, ""],
    "d": ["d@1.0.0", "", { "optionalDependencies": { "gone": "1" }, "unresolved": ["gone"] }, ""],
    "gone": ["gone@1.0.0", "", {}, ""],
    "lib": ["lib@workspace:packages/lib"],
    "lib/a": ["a@2.0.0", "", {}, ""],
  }
}"#;
    let (g, w) = parse(src).unwrap();
    assert!(w.is_empty(), "{w:?}");
    g.validate().unwrap();
    let by = |name: &str, version: &str| {
        g.packages
            .iter()
            .position(|p| p.name == name && p.version == version)
            .map(|i| PackageId(i as u32))
    };
    let root = g.root();
    assert_eq!(root.name, "app");
    assert_eq!(root.dependency("a").unwrap().target, by("a", "1.0.0"));
    let lib = &g.packages[g.workspaces["packages/lib"].index()];
    assert_eq!(lib.version, "2.0.0");
    assert_eq!(lib.dependency("a").unwrap().target, by("a", "2.0.0"));
    let p = lib.dependency("p").unwrap();
    assert_eq!(p.behavior, Behavior::PEER | Behavior::OPTIONAL);
    assert_eq!(p.target, None);
    assert!(lib.peer_meta["p"].optional);
    let a = &g.packages[by("a", "1.0.0").unwrap().index()];
    assert_eq!(a.dependency("b").unwrap().target, by("b", "1.0.0"));
    assert!(a.meta.has_install_script);
    assert_eq!(a.platform.os.names().collect::<Vec<_>>(), ["darwin"]);
    assert_eq!(a.platform.cpu.names().collect::<Vec<_>>(), ["arm64", "x64"]);
    assert!(
        matches!(&a.bin, fnpm_types::Bin::NamedFile { name, path } if name == "a" && path == "cli.js")
    );
    assert!(matches!(
        &a.resolution,
        Resolution::Npm {
            tarball_url: None,
            integrity: None,
            ..
        }
    ));
    // Listed under `unresolved`: stays unresolved although a `gone` package exists.
    let d = &g.packages[by("d", "1.0.0").unwrap().index()];
    assert_eq!(d.dependency("gone").unwrap().target, None);
}

#[test]
fn a_workspace_listed_twice_is_one_workspace() {
    let member =
        "    \"packages/a\": {\n      \"name\": \"a\",\n      \"version\": \"1.0.0\",\n    },\n";
    let src = format!(
        "{{\n  \"lockfileVersion\": 1,\n  \"workspaces\": {{\n    \"\": {{\n      \"name\": \"root\",\n    }},\n{member}{member}  }},\n  \"packages\": {{\n    \"a\": [\"a@workspace:packages/a\"],\n  }}\n}}\n"
    );
    let (g, w) = parse(&src).unwrap();
    assert_eq!(w, ["11:5: Duplicate key \"packages/a\" in object literal"]);
    assert_eq!(g.workspaces.len(), 2);
}
