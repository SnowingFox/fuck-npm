//! bun.lock reading and writing on small lockfiles (cases from bun's `bun-lock.test.ts`,
//! `lockfile-version-2.test.ts` and `config-version.test.ts`).

use fnpm_types::{ConfigVersion, LockGraph, Package, Resolution};

use super::fixtures::first_difference;
use super::support::tree_from_bun_paths;
use crate::formats::bun_text::{self, Parsed};
use crate::migration::MigrateOptions;

const SHA: &str = "sha512-v4w12JRjUGvfHDUP8vFDwu0gUWu04j0cv9hLb1Abf9VdaXu4XcrddYFTMVBVvmldKViGWH7jrb6xPJRF0wq6gw==";

fn parse(text: &str) -> crate::Result<Parsed> {
    bun_text::parse_with(text.as_bytes(), &MigrateOptions::default())
}

fn find<'g>(g: &'g LockGraph, name: &str, version: &str) -> &'g Package {
    g.packages
        .iter()
        .find(|p| p.name == name && p.version == version)
        .unwrap_or_else(|| panic!("no {name}@{version}"))
}

fn target<'g>(g: &'g LockGraph, from: &Package, dep: &str) -> Option<&'g Package> {
    from.dependency(dep)
        .and_then(|e| e.target)
        .map(|t| &g.packages[t.index()])
}

fn round_trip(text: &str) {
    let parsed = parse(text).unwrap();
    let tree = tree_from_bun_paths(&parsed.graph, &parsed.paths);
    let written = bun_text::serialize_loaded(&parsed.graph, &tree, parsed.version);
    assert!(
        written == text,
        "{}\n---\n{written}",
        first_difference(text, &written)
    );
}

#[test]
fn version_and_shape_errors() {
    for (text, message) in [
        ("{}", "Missing lockfile version"),
        (r#"{ "lockfileVersion": "1" }"#, "Invalid lockfile version"),
        (r#"{ "lockfileVersion": 1.5 }"#, "Invalid lockfile version"),
        (
            r#"{ "lockfileVersion": 4, "workspaces": { "": {} } }"#,
            "Unsupported lockfile version 4",
        ),
        (r#"{ "lockfileVersion": 1 }"#, "Missing a workspaces object"),
        (
            r#"{ "lockfileVersion": 1, "workspaces": {} }"#,
            "Expected root package",
        ),
        (
            r#"{ "lockfileVersion": 1, "configVersion": "1", "workspaces": { "": {} } }"#,
            "configVersion",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": {}, "a": {} } }"#,
            "name property",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": {}, "a": { "name": "a", "version": "x" } } }"#,
            "Invalid semver",
        ),
        (
            r#"{ "lockfileVersion": 1, "trustedDependencies": {}, "workspaces": { "": {} } }"#,
            "Expected an array",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": {} }, "packages": { "a": ["a@1.0.0"] } }"#,
            "Missing npm registry",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": {} }, "packages": { "a": ["a@1.0.0", "", {}] } }"#,
            "Missing integrity",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": {} }, "packages": { "a": ["a@^1", "", {}, ""] } }"#,
            "Unexpected resolution",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": {} }, "packages": { "a": ["../a@1.0.0", "", {}, ""] } }"#,
            "invalid package name",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": {} }, "packages": { "a": ["a@workspace:x"] } }"#,
            "Unknown workspace",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": { "dependencies": { "a": "1" } } } }"#,
            "Failed to resolve root prod dependency 'a'",
        ),
        (
            r#"{ "lockfileVersion": 1, "workspaces": { "": {} }, "packages": { "a": ["a@1.0.0", "", { "dependencies": { "b": "1" } }, ""] } }"#,
            "Failed to resolve prod dependency 'b' for package 'a'",
        ),
    ] {
        let e = parse(text).unwrap_err().to_string();
        assert!(e.contains(message), "{text}: {e}");
    }
}

#[test]
fn version_2_strictness() {
    // An off-registry tarball without integrity is accepted at v1 and rejected from v2 on.
    let lock = |v: u32, integrity: &str| {
        format!(
            r#"{{ "lockfileVersion": {v}, "workspaces": {{ "": {{ "dependencies": {{ "a": "1" }} }} }},
              "packages": {{ "a": ["a@1.0.0", "https://evil.example/a-1.0.0.tgz", {{}}, "{integrity}"] }} }}"#
        )
    };
    assert!(parse(&lock(1, "")).is_ok());
    assert!(
        parse(&lock(2, ""))
            .unwrap_err()
            .to_string()
            .contains("outside the configured registry")
    );
    assert!(parse(&lock(2, SHA)).is_ok());
    // Malformed integrity: ignored at v1, an error at v2.
    assert!(
        parse(&lock(1, "md5-xyz")).unwrap().graph.packages[1]
            .resolution
            .integrity()
            .is_none()
    );
    assert!(
        parse(&lock(2, "md5-xyz"))
            .unwrap_err()
            .to_string()
            .contains("malformed integrity")
    );
    // A git tag is checked from v2 on, a github tag always.
    let git = |v: u32, kind: &str, tag: &str| {
        format!(
            r#"{{ "lockfileVersion": {v}, "workspaces": {{ "": {{ "dependencies": {{ "a": "x" }} }} }},
              "packages": {{ "a": ["a@{kind}", {{}}, "{tag}"] }} }}"#
        )
    };
    assert!(parse(&git(1, "git+https://h.example/r.git#abc", "../x")).is_ok());
    assert!(parse(&git(2, "git+https://h.example/r.git#abc", "../x")).is_err());
    assert!(parse(&git(1, "github:u/r#abc", "../x")).is_err());
    let ok = parse(&git(2, "github:u/r#abc", "u-r-abc")).unwrap();
    assert!(
        matches!(&ok.graph.packages[1].resolution, Resolution::Github { owner, repo, resolved_commit, .. }
        if owner == "u" && repo == "r" && resolved_commit == "u-r-abc")
    );
}

#[test]
fn config_version_and_registry_urls() {
    let base = r#"{ "lockfileVersion": 1, "workspaces": { "": {} } }"#;
    assert_eq!(parse(base).unwrap().graph.config_version, ConfigVersion::V0);
    let v1 = r#"{ "lockfileVersion": 1, "configVersion": 1, "workspaces": { "": {} } }"#;
    assert_eq!(parse(v1).unwrap().graph.config_version, ConfigVersion::V1);
    let clamped = r#"{ "lockfileVersion": 1, "configVersion": 9, "workspaces": { "": {} } }"#;
    assert_eq!(
        parse(clamped).unwrap().graph.config_version,
        ConfigVersion::V1
    );
    let text = format!(
        r#"{{ "lockfileVersion": 2, "workspaces": {{ "": {{ "dependencies": {{ "a": "1", "b": "1" }} }} }},
          "packages": {{ "a": ["a@1.0.0", "", {{}}, "{SHA}"], "b": ["b@1.0.0", "https://registry.npmjs.org/b/-/b-1.0.0.tgz", {{}}, "{SHA}"] }} }}"#
    );
    let g = parse(&text).unwrap().graph;
    for name in ["a", "b"] {
        assert!(matches!(
            &find(&g, name, "1.0.0").resolution,
            Resolution::Npm {
                tarball_url: None,
                integrity: Some(_),
                ..
            }
        ));
    }
}

#[test]
fn binds_by_path_and_peers_by_version() {
    let text = format!(
        r#"{{
  "lockfileVersion": 1,
  "workspaces": {{
    "": {{ "name": "app", "dependencies": {{ "a": "1", "lib": "workspace:*", "react": "^18" }} }},
    "packages/lib": {{ "name": "lib", "version": "0.1.0", "dependencies": {{ "@s/b": "1" }}, "peerDependencies": {{ "react": "*" }} }},
  }},
  "packages": {{
    "a": ["a@1.0.0", "", {{ "dependencies": {{ "@s/b": "^2", "c": "1" }}, "peerDependencies": {{ "react": "^17 || ^18" }} }}, "{SHA}"],
    "@s/b": ["@s/b@1.0.0", "", {{}}, "{SHA}"],
    "a/@s/b": ["@s/b@2.0.0", "", {{ "dependencies": {{ "c": "1" }} }}, "{SHA}"],
    "c": ["c@1.0.0", "", {{}}, "{SHA}"],
    "lib": ["lib@workspace:packages/lib"],
    "lib/@s/b": ["@s/b@1.5.0", "", {{}}, "{SHA}"],
    "react": ["react@18.2.0", "", {{}}, "{SHA}"],
    "a/react": ["react@17.0.2", "", {{}}, "{SHA}"],
  }}
}}"#
    );
    let parsed = parse(&text).unwrap();
    let g = &parsed.graph;
    g.validate().unwrap();
    let a = find(g, "a", "1.0.0");
    let b2 = target(g, a, "@s/b").unwrap();
    assert_eq!(b2.version, "2.0.0");
    // The nested scoped package walks up past `a/@s` to the root's `c`.
    assert_eq!(target(g, b2, "c").unwrap().version, "1.0.0");
    // A required peer takes the highest locked version in its range, not the nearest copy.
    assert_eq!(target(g, a, "react").unwrap().version, "18.2.0");
    let lib = &g.packages[g.workspaces["packages/lib"].index()];
    assert_eq!(lib.version, "0.1.0");
    assert_eq!(target(g, lib, "@s/b").unwrap().version, "1.5.0");
    assert!(g.root().dependency("lib").unwrap().behavior.is_workspace());
    assert_eq!(parsed.paths.len(), 8);
}

#[test]
fn optional_peers_unresolved_and_bundled() {
    let text = format!(
        r#"{{
  "lockfileVersion": 1,
  "workspaces": {{
    "": {{ "dependencies": {{ "a": "1" }}, "optionalDependencies": {{ "fsevents": "2" }}, "peerDependencies": {{ "p": "1" }}, "optionalPeers": ["p"], "unresolved": ["fsevents"] }},
  }},
  "packages": {{
    "a": ["a@1.0.0", "", {{ "dependencies": {{ "inner": "1" }} }}, "{SHA}"],
    "a/inner": ["inner@1.0.0", "", {{ "bundled": true }}, "{SHA}"],
    "fsevents": ["fsevents@2.3.3", "", {{ "os": "darwin" }}, "{SHA}"],
  }}
}}"#
    );
    let g = parse(&text).unwrap().graph;
    let root = g.root();
    assert!(
        root.dependency("fsevents").unwrap().target.is_none(),
        "listed as unresolved"
    );
    let p = root.dependency("p").unwrap();
    assert!(p.behavior.is_optional_peer() && p.target.is_none());
    let a = find(&g, "a", "1.0.0");
    assert!(a.dependency("inner").unwrap().behavior.is_bundled());
    assert!(find(&g, "inner", "1.0.0").meta.bundled);
    assert_eq!(
        find(&g, "fsevents", "2.3.3")
            .platform
            .os
            .names()
            .collect::<Vec<_>>(),
        ["darwin"]
    );
}

#[test]
fn round_trips_every_section() {
    round_trip(&format!(
        r#"{{
  "lockfileVersion": 1,
  "configVersion": 1,
  "workspaces": {{
    "": {{
      "name": "app",
      "dependencies": {{
        "@app/lib": "workspace:*",
        "react": "catalog:",
      }},
      "devDependencies": {{
        "tool": "^1.0.0",
      }},
      "optionalDependencies": {{
        "fsevents": "^2.3.0",
      }},
      "peerDependencies": {{
        "maybe": "*",
      }},
      "optionalPeers": [
        "maybe",
      ],
      "unresolved": [
        "fsevents",
      ],
    }},
    "packages/lib": {{
      "name": "@app/lib",
      "version": "0.1.0",
      "bin": {{
        "lib": "cli.js",
      }},
      "dependencies": {{
        "left-pad": "1.2.0",
      }},
    }},
  }},
  "trustedDependencies": [
    "tool",
  ],
  "patchedDependencies": {{
    "left-pad@1.3.0": "patches/left-pad@1.3.0.patch",
  }},
  "overrides": {{
    "js-tokens": "4.0.0",
  }},
  "catalog": {{
    "react": "^18.3.1",
  }},
  "catalogs": {{
    "old": {{
      "react": "^17.0.0",
    }},
  }},
  "packages": {{
    "@app/lib": ["@app/lib@workspace:packages/lib"],

    "fsevents": ["fsevents@2.3.3", "", {{ "os": "darwin" }}, "{SHA}"],

    "gh": ["gh@github:u/gh#1234567", {{ "bin": {{ "a": "a.js", "b": "b.js" }} }}, "u-gh-1234567", "{SHA}"],

    "git-dep": ["git-dep@git+ssh://git@github.com:u/r.git#abc123", {{}}, "abc123"],

    "left-pad": ["left-pad@1.3.0", "https://mirror.example/left-pad-1.3.0.tgz", {{ "cpu": [ "x64", "arm64", ], "libc": "glibc" }}, "{SHA}"],

    "local": ["local@file:vendor", {{ "binDir": "bin" }}],

    "react": ["react@18.3.1", "", {{ "dependencies": {{ "loose-envify": "^1.1.0" }}, "os": "!win32" }}, "{SHA}"],

    "remote": ["remote@https://example.com/r.tgz", {{}}],

    "tool": ["tool@1.0.0", "", {{ "hasInstallScript": true, "bin": "cli.js" }}, ""],

    "@app/lib/left-pad": ["left-pad@1.2.0", "", {{ "os": "none" }}, "{SHA}"],

    "react/loose-envify": ["loose-envify@1.4.0", "", {{ "bin": {{ "loose-envify": "cli.js" }} }}, "{SHA}"],
  }}
}}
"#
    ));
    // An empty trusted list means "trust nothing" and is kept.
    round_trip(
        r#"{
  "lockfileVersion": 2,
  "configVersion": 0,
  "workspaces": {
    "": {},
  },
  "trustedDependencies": [],
  "packages": {}
}
"#,
    );
}

#[test]
fn scoped_overrides_are_version_3() {
    round_trip(
        r#"{
  "lockfileVersion": 3,
  "configVersion": 1,
  "workspaces": {
    "": {
      "name": "app",
    },
  },
  "overrides": {
    "a": "1.0.0",
    "b": {
      ".": "2.0.0",
      "c": "3.0.0",
    },
    "d@^1": {
      ".": "1.5.0",
    },
    "e@^2": {
      "f@<3": "3.0.1",
    },
  },
  "packages": {}
}
"#,
    );
    // A fresh graph with scoped rules is written as version 3, without them as version 2.
    let parsed = parse(
        r#"{ "lockfileVersion": 1, "workspaces": { "": {} }, "overrides": { "b": { "c": "1" } } }"#,
    )
    .unwrap();
    let tree = tree_from_bun_paths(&parsed.graph, &parsed.paths);
    assert!(
        bun_text::serialize_with_tree(&parsed.graph, &tree).contains("\"lockfileVersion\": 3,")
    );
    let parsed = parse(r#"{ "lockfileVersion": 0, "workspaces": { "": {} } }"#).unwrap();
    assert!(
        bun_text::serialize_loaded(&parsed.graph, &tree, parsed.version)
            .contains("\"lockfileVersion\": 1,")
    );
    assert!(
        bun_text::serialize_with_tree(&parsed.graph, &tree).contains("\"lockfileVersion\": 2,")
    );
}

#[test]
fn version_0_workspace_rows() {
    // Version 0 lists a workspace's dependencies in its `packages` row. Like bun's loader, the
    // reader leaves them unbound (the install binds them again).
    let text = format!(
        r#"{{ "lockfileVersion": 0, "workspaces": {{ "": {{ "dependencies": {{ "w": "workspace:*" }} }}, "w": {{ "name": "w" }} }},
          "packages": {{ "w": ["w@workspace:w", {{ "dependencies": {{ "x": "1" }} }}], "x": ["x@1.0.0", "", {{}}, "{SHA}"] }} }}"#
    );
    let g = parse(&text).unwrap().graph;
    let w = g.packages.iter().find(|p| p.name == "w").unwrap();
    assert_eq!(w.resolution, Resolution::Workspace { path: "w".into() });
    assert!(w.dependency("x").is_some_and(|e| e.target.is_none()));
    assert!(g.workspaces.contains_key("w"));
}

#[test]
fn integral_float_versions_are_numbers() {
    // bun reads the JSON number, so `1.0` and `1e0` are 1.
    let p = parse(r#"{ "lockfileVersion": 1.0, "configVersion": 1e0, "workspaces": { "": {} } }"#)
        .unwrap();
    assert_eq!(p.version, 1);
    assert_eq!(p.graph.config_version, ConfigVersion::V1);
    let p = parse(r#"{ "lockfileVersion": 2e0, "configVersion": 3.0, "workspaces": { "": {} } }"#)
        .unwrap();
    assert_eq!(p.version, 2);
    assert_eq!(p.graph.config_version, ConfigVersion::CURRENT);
    for bad in ["0.5", "-1", "1.5"] {
        let text = format!(
            r#"{{ "lockfileVersion": 1, "configVersion": {bad}, "workspaces": {{ "": {{}} }} }}"#
        );
        assert!(parse(&text).is_err(), "configVersion {bad}");
    }
    assert!(parse(r#"{ "lockfileVersion": -1, "workspaces": { "": {} } }"#).is_err());
}
