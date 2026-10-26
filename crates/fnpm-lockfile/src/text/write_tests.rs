//! Writer details: the fnpm dialect, INFO shapes and graphs built in memory.

use fnpm_types::{
    Behavior, Bin, CpuSet, DepEdge, HashAlgorithm, Integrity, LockGraph, OsSet, OverrideRule,
    OverrideSelector, Package, PackageId, PatchedDependency, Resolution,
};

use super::write::{cpu_json, os_json, write};

fn os(list: &[&str]) -> String {
    let mut s = String::new();
    os_json(&mut s, OsSet::from_list(list));
    s
}

fn cpu(list: &[&str]) -> String {
    let mut s = String::new();
    cpu_json(&mut s, CpuSet::from_list(list));
    s
}

#[test]
fn negatable_lists_like_bun() {
    assert_eq!(os(&["darwin"]), "\"darwin\"");
    assert_eq!(os(&["linux", "darwin"]), "[ \"linux\", \"darwin\", ]");
    assert_eq!(os(&["!win32"]), "\"!win32\"");
    assert_eq!(os(&["none"]), "\"none\"");
    assert_eq!(cpu(&["x64"]), "\"x64\"");
    assert_eq!(cpu(&["!arm", "!ia32"]), "[ \"!arm\", \"!ia32\", ]");
}

fn npm(g: &mut LockGraph, name: &str, version: &str) -> PackageId {
    let i = Integrity::compute(
        HashAlgorithm::Sha512,
        format!("{name}@{version}").as_bytes(),
    );
    g.push(Package::new(
        name,
        version,
        Resolution::npm(version, Some(i)),
    ))
}

fn link(
    g: &mut LockGraph,
    from: PackageId,
    name: &str,
    lit: &str,
    b: Behavior,
    to: Option<PackageId>,
) {
    let mut e = DepEdge::new(name, lit, b);
    e.target = to;
    g.packages[from.index()].dependencies.push(e);
}

fn sample() -> LockGraph {
    let mut g = LockGraph::new(Package::new("app", "", Resolution::Root));
    g.lockfile_version = crate::TEXT_LOCKFILE_VERSION_CURRENT;
    let lib = g.add_workspace(Package::new(
        "@app/lib",
        "0.1.0",
        Resolution::Workspace {
            path: "packages/lib".into(),
        },
    ));
    let esbuild = npm(&mut g, "esbuild", "0.21.5");
    let native = npm(&mut g, "@esbuild/linux-x64", "0.21.5");
    let lp = npm(&mut g, "left-pad", "1.3.0");
    let le = npm(&mut g, "loose-envify", "1.4.0");
    let le_old = npm(&mut g, "loose-envify", "1.0.0");
    let jt = npm(&mut g, "js-tokens", "4.0.0");
    g.packages[native.index()].platform.os = OsSet::from_list(&["linux"]);
    g.packages[native.index()].platform.cpu = CpuSet::from_list(&["x64"]);
    g.packages[esbuild.index()].bin = Bin::NamedFile {
        name: "esbuild".into(),
        path: "bin/esbuild".into(),
    };
    g.packages[esbuild.index()].meta.has_install_script = true;
    g.packages[lp.index()].meta.deprecated = Some("use String.prototype.padStart()".into());
    g.packages[le.index()].bin = Bin::NamedFile {
        name: "loose-envify".into(),
        path: "cli.js".into(),
    };
    link(
        &mut g,
        PackageId::ROOT,
        "@app/lib",
        "workspace:*",
        Behavior::PROD,
        Some(lib),
    );
    link(
        &mut g,
        PackageId::ROOT,
        "esbuild",
        "^0.21.5",
        Behavior::DEV,
        Some(esbuild),
    );
    link(
        &mut g,
        esbuild,
        "@esbuild/linux-x64",
        "0.21.5",
        Behavior::OPTIONAL,
        Some(native),
    );
    link(&mut g, lib, "left-pad", "1.3.0", Behavior::PROD, Some(lp));
    link(
        &mut g,
        lib,
        "loose-envify",
        "^1.0.0",
        Behavior::PROD,
        Some(le_old),
    );
    link(
        &mut g,
        PackageId::ROOT,
        "loose-envify",
        "^1.4.0",
        Behavior::PROD,
        Some(le),
    );
    link(&mut g, le, "js-tokens", "^4.0.0", Behavior::PROD, Some(jt));
    link(
        &mut g,
        le_old,
        "js-tokens",
        "^4.0.0",
        Behavior::PROD,
        Some(jt),
    );
    g.trusted_dependencies = Some(
        [
            "zeta".to_owned(),
            "esbuild".to_owned(),
            "left-pad".to_owned(),
        ]
        .into(),
    );
    g.patched_dependencies.insert(
        "left-pad@1.3.0".into(),
        PatchedDependency {
            path: "patches/left-pad@1.3.0.patch".into(),
            hash: None,
        },
    );
    g.overrides.push(OverrideRule {
        selector: OverrideSelector::parse("js-tokens").unwrap().unwrap(),
        value: "4.0.0".into(),
    });
    g.catalogs.default.insert("react".into(), "^18.3.1".into());
    g
}

#[test]
fn writes_the_documented_layout() {
    let text = write(&sample()).text;
    let want = r#"{
  "lockfileVersion": 2,
  "configVersion": 1,
  "workspaces": {
    "": {
      "name": "app",
      "dependencies": {
        "@app/lib": "workspace:*",
        "loose-envify": "^1.4.0",
      },
      "devDependencies": {
        "esbuild": "^0.21.5",
      },
    },
    "packages/lib": {
      "name": "@app/lib",
      "version": "0.1.0",
      "dependencies": {
        "left-pad": "1.3.0",
        "loose-envify": "^1.0.0",
      },
    },
  },
  "trustedDependencies": [
    "esbuild",
    "left-pad",
  ],
  "patchedDependencies": {
    "left-pad@1.3.0": "patches/left-pad@1.3.0.patch",
  },
  "overrides": {
    "js-tokens": "4.0.0",
  },
  "catalog": {
    "react": "^18.3.1",
  },
  "packages": {
"#;
    assert!(text.starts_with(want), "{text}");
    for line in [
        "    \"@app/lib\": [\"@app/lib@workspace:packages/lib\"],\n",
        "    \"@esbuild/linux-x64\": [\"@esbuild/linux-x64@0.21.5\", \"\", { \"os\": \"linux\", \"cpu\": \"x64\" }, \"sha512-",
        "    \"esbuild\": [\"esbuild@0.21.5\", \"\", { \"optionalDependencies\": { \"@esbuild/linux-x64\": \"0.21.5\" }, \"hasInstallScript\": true, \"bin\": { \"esbuild\": \"bin/esbuild\" } }, \"sha512-",
        "    \"left-pad\": [\"left-pad@1.3.0\", \"\", {}, \"sha512-",
        "    \"@app/lib/loose-envify\": [\"loose-envify@1.0.0\", \"\", { \"dependencies\": { \"js-tokens\": \"^4.0.0\" } }, \"sha512-",
    ] {
        assert!(text.contains(line), "missing {line:?} in\n{text}");
    }
    // Like bun.lock, no `deprecated` (that comes from the manifest), and it reads back to the
    // same file.
    assert!(!text.contains("deprecated"));
    let (g, _) = crate::text_parse(
        text.as_bytes(),
        "fnpm.lock",
        &crate::ParseOptions::default(),
    )
    .unwrap();
    assert_eq!(write(&g).text, text);
}

#[test]
fn explicit_empty_trusted_list_is_kept() {
    let mut g = sample();
    g.trusted_dependencies = Some(Default::default());
    assert!(
        write(&g)
            .text
            .contains("\n  \"trustedDependencies\": [],\n")
    );
    g.trusted_dependencies = None;
    assert!(!write(&g).text.contains("trustedDependencies"));
}

#[test]
fn version_choice() {
    let mut g = sample();
    // A loaded v1 lockfile stays v1, v0 becomes v1.
    g.lockfile_version = 1;
    assert!(write(&g).text.contains("\"lockfileVersion\": 1,"));
    g.lockfile_version = 0;
    assert!(write(&g).text.contains("\"lockfileVersion\": 1,"));
    // Scoped overrides need v3.
    g.lockfile_version = 3;
    g.overrides.push(OverrideRule {
        selector: OverrideSelector::parse("left-pad>js-tokens")
            .unwrap()
            .unwrap(),
        value: "3.0.0".into(),
    });
    let t = write(&g).text;
    assert!(t.contains("\"lockfileVersion\": 3,"));
    assert!(t.contains("  \"overrides\": {\n    \"js-tokens\": \"4.0.0\",\n    \"left-pad\": {\n      \"js-tokens\": \"3.0.0\",\n    },\n  },\n"), "{t}");
    // An off-registry tarball without integrity keeps a new lockfile at v1.
    let mut g = sample();
    let id = g
        .packages
        .iter()
        .position(|p| p.name == "js-tokens")
        .unwrap();
    g.packages[id].resolution = Resolution::Npm {
        version: "4.0.0".into(),
        tarball_url: Some("https://mirror.example.com/js-tokens-4.0.0.tgz".into()),
        integrity: None,
    };
    assert!(write(&g).text.contains("\"lockfileVersion\": 1,"));
}

#[test]
fn other_resolutions() {
    let mut g = LockGraph::new(Package::new("", "", Resolution::Root));
    let i = Integrity::compute(HashAlgorithm::Sha512, b"t");
    let gh = g.push(Package::new(
        "gh",
        "",
        Resolution::Github {
            owner: "o".into(),
            repo: "r".into(),
            committish: "abc1234".into(),
            resolved_commit: "o-r-abc1234".into(),
            integrity: Some(i),
        },
    ));
    let git = g.push(Package::new(
        "g",
        "",
        Resolution::Git {
            url: "git@example.com:org/g.git".into(),
            committish: "".into(),
            resolved_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            integrity: None,
        },
    ));
    let tgz = g.push(Package::new(
        "t",
        "",
        Resolution::LocalTarball {
            path: "./t.tgz".into(),
            integrity: Some(i),
        },
    ));
    let dir = g.push(Package::new(
        "f",
        "",
        Resolution::Folder {
            path: "vendor/f".into(),
        },
    ));
    let ln = g.push(Package::new(
        "l",
        "",
        Resolution::Symlink {
            path: "../l".into(),
        },
    ));
    link(
        &mut g,
        PackageId::ROOT,
        "gh",
        "github:o/r",
        Behavior::PROD,
        Some(gh),
    );
    link(
        &mut g,
        PackageId::ROOT,
        "g",
        "git@example.com:org/g.git",
        Behavior::PROD,
        Some(git),
    );
    link(
        &mut g,
        PackageId::ROOT,
        "t",
        "./t.tgz",
        Behavior::PROD,
        Some(tgz),
    );
    link(
        &mut g,
        PackageId::ROOT,
        "f",
        "file:vendor/f",
        Behavior::PROD,
        Some(dir),
    );
    link(
        &mut g,
        PackageId::ROOT,
        "l",
        "link:../l",
        Behavior::PROD,
        Some(ln),
    );
    let text = write(&g).text;
    let sri = i.to_sri();
    for line in [
        "    \"\": {\n      \"dependencies\": {\n".to_owned(),
        format!("    \"gh\": [\"gh@github:o/r#abc1234\", {{}}, \"o-r-abc1234\", \"{sri}\"],"),
        "    \"g\": [\"g@git+ssh://git@example.com:org/g.git#0123456789abcdef0123456789abcdef01234567\", {}, \"0123456789abcdef0123456789abcdef01234567\"],".to_owned(),
        format!("    \"t\": [\"t@./t.tgz\", {{}}, \"{sri}\"],"),
        "    \"f\": [\"f@file:vendor/f\", {}],".to_owned(),
        "    \"l\": [\"l@link:../l\", {}],".to_owned(),
    ] {
        assert!(text.contains(&line), "missing {line:?} in\n{text}");
    }
    let (back, _) =
        crate::text_parse(text.as_bytes(), "bun.lock", &crate::ParseOptions::default()).unwrap();
    assert_eq!(write(&back).text, text);
}
