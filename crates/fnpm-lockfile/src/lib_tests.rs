use std::path::Path;

use fnpm_types::{
    Behavior, DepEdge, HashAlgorithm, Integrity, LockGraph, Package, PackageId, Resolution,
};
use fnpm_workspace::{PackageJson, Workspace, WorkspaceSettings};

use super::*;

fn ws(dir: &Path) -> Workspace {
    Workspace {
        root_dir: dir.to_owned(),
        root: PackageJson {
            raw: serde_json::json!({}),
            ..Default::default()
        },
        members: Vec::new(),
        settings: WorkspaceSettings::default(),
    }
}

fn graph() -> LockGraph {
    let mut g = LockGraph::new(Package::new("app", "", Resolution::Root));
    let i = Integrity::compute(HashAlgorithm::Sha512, b"a");
    let a = g.push(Package::new(
        "a",
        "1.0.0",
        Resolution::npm("1.0.0", Some(i)),
    ));
    let mut e = DepEdge::new("a", "^1.0.0", Behavior::PROD);
    e.target = Some(a);
    g.root_mut().dependencies.push(e);
    g
}

#[test]
fn save_then_load_uses_the_sidecar_and_skips_identical_writes() {
    let dir = tempfile::tempdir().unwrap();
    let g = graph();
    let h = save(dir.path(), &g).unwrap();
    let path = dir.path().join(FNPM_LOCK);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content_hash(text.as_bytes()), h);
    assert!(dir.path().join(sidecar::SIDECAR).exists());
    let loaded = load(dir.path(), &ws(dir.path())).unwrap().unwrap();
    assert_eq!(loaded.format, Format::Fnpm);
    assert!(!loaded.migrated);
    assert_eq!(loaded.hash, h);
    // The sidecar holds what parsing the file gives.
    let (parsed, _) = text_parse(text.as_bytes(), FNPM_LOCK, &ParseOptions::default()).unwrap();
    assert_eq!(loaded.graph, parsed);
    assert_eq!(loaded.graph.root().dependencies, g.root().dependencies);
    // Unchanged bytes: the file is not rewritten.
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(save(dir.path(), &g).unwrap(), h);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    // A stale sidecar is ignored: edit the file by hand.
    std::fs::write(&path, text.replace("^1.0.0", "^1.0.1")).unwrap();
    let loaded = load(dir.path(), &ws(dir.path())).unwrap().unwrap();
    assert_eq!(loaded.graph.root().dependencies[0].literal, "^1.0.1");
    // And so is a corrupt one.
    std::fs::write(dir.path().join(sidecar::SIDECAR), b"garbage").unwrap();
    assert!(load(dir.path(), &ws(dir.path())).unwrap().is_some());
}

#[test]
fn load_priority_and_formats() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load(dir.path(), &ws(dir.path())).unwrap().is_none());
    let bun = formats::bun_text::serialize(&graph());
    std::fs::write(dir.path().join("bun.lock"), &bun).unwrap();
    let l = load(dir.path(), &ws(dir.path())).unwrap().unwrap();
    assert_eq!((l.format, l.migrated), (Format::BunText, true));
    assert_eq!(l.path, dir.path().join("bun.lock"));
    // fnpm.lock wins over bun.lock.
    save(dir.path(), &graph()).unwrap();
    assert_eq!(
        load(dir.path(), &ws(dir.path())).unwrap().unwrap().format,
        Format::Fnpm
    );
    // A broken fnpm.lock is an error (the install ignores it), not a fallback.
    std::fs::write(dir.path().join(FNPM_LOCK), "{ nope").unwrap();
    let e = load(dir.path(), &ws(dir.path())).unwrap_err();
    assert!(matches!(e, Error::Parse { .. }), "{e}");
    // install.lockfile.path
    let opts = LoadOptions {
        lockfile_path: Some("bun.lock".into()),
        ..Default::default()
    };
    assert_eq!(
        load_with(dir.path(), &ws(dir.path()), &opts)
            .unwrap()
            .unwrap()
            .format,
        Format::BunText
    );
    assert_eq!(
        Format::detect("yarn.lock", b"# yarn lockfile v1\n"),
        Some(Format::YarnV1)
    );
    assert_eq!(
        Format::detect("yarn.lock", b"# x\n\n__metadata:\n  version: 6\n"),
        Some(Format::YarnBerry)
    );
    assert_eq!(Format::detect("pnpm-lock.yaml", b""), Some(Format::Pnpm));
    assert_eq!(Format::detect("x.lock", b""), None);
}

#[test]
fn serialize_formats() {
    let g = graph();
    assert_eq!(
        serialize(Format::Fnpm, &g).unwrap(),
        formats::fnpm::serialize(&g).into_bytes()
    );
    assert!(matches!(
        serialize(Format::BunBinary, &g),
        Err(Error::Unsupported { .. })
    ));
    assert!(matches!(
        serialize(Format::YarnBerry, &g),
        Err(Error::Unsupported { .. })
    ));
}

#[test]
fn cycles_fail_the_save() {
    let mut g = LockGraph::new(Package::new("app", "", Resolution::Root));
    let ids: Vec<PackageId> = ["a@1.0.0", "a@2.0.0", "b@1.0.0", "b@2.0.0"]
        .iter()
        .map(|k| {
            let (n, v) = k.split_once('@').unwrap();
            g.push(Package::new(n, v, Resolution::npm(v, None)))
        })
        .collect();
    let edge = |g: &mut LockGraph, from: PackageId, name: &str, to: PackageId| {
        let mut e = DepEdge::new(name, "*", Behavior::PROD);
        e.target = Some(to);
        g.packages[from.index()].dependencies.push(e);
    };
    edge(&mut g, PackageId::ROOT, "a", ids[0]);
    edge(&mut g, ids[0], "b", ids[2]);
    edge(&mut g, ids[2], "a", ids[1]);
    edge(&mut g, ids[1], "b", ids[3]);
    edge(&mut g, ids[3], "a", ids[0]);
    let dir = tempfile::tempdir().unwrap();
    let e = save(dir.path(), &g).unwrap_err();
    assert!(matches!(e, Error::DependencyCycle(_)), "{e}");
    assert!(
        e.to_string()
            .starts_with("cannot install dependency cycle into node_modules: ")
    );
}

#[test]
fn save_yarn_writes_v1() {
    let dir = tempfile::tempdir().unwrap();
    save_yarn(dir.path(), &graph(), &print::YarnOptions::default()).unwrap();
    let t = std::fs::read_to_string(dir.path().join("yarn.lock")).unwrap();
    assert!(t.contains("\na@^1.0.0:\n  version \"1.0.0\"\n"), "{t}");
}

#[test]
fn parse_options_pick_scope_registries() {
    let o = ParseOptions {
        default_registry: Some("https://r.example/".into()),
        scope_registries: [("s".to_owned(), "https://s.example/".to_owned())].into(),
    };
    assert_eq!(o.registry_for("@s/x"), "https://s.example/");
    assert_eq!(o.registry_for("@t/x"), "https://r.example/");
    assert_eq!(o.registry_for("x"), "https://r.example/");
    assert_eq!(
        ParseOptions::default().registry_for("x"),
        "https://registry.npmjs.org"
    );
}
