//! `--filter` selection over a loaded workspace, ported from bun's
//! `test/cli/install/bun-workspaces.test.ts` (`describe("install --filter")`) and
//! `bun-add-filter.test.ts` (explicit-only root).

mod common;

use std::path::Path;

use common::{pkg, tree};
use fnpm_workspace::{
    Error, RootSelection, SelectOptions, Selector, Target, Workspace, select, select_with,
    unmatched_message,
};
use serde_json::json;

const ROOT: &str = "<root>";

fn names(ws: &Workspace, selected: &[usize]) -> Vec<String> {
    selected
        .iter()
        .map(|&i| {
            if i == usize::MAX {
                ROOT.to_owned()
            } else {
                ws.members[i].name.clone()
            }
        })
        .collect()
}

fn sel(ws: &Workspace, patterns: &[&str], cwd: &Path) -> Vec<String> {
    let selectors = Selector::parse_all(patterns).unwrap();
    names(ws, &select(ws, &selectors, cwd).unwrap())
}

/// A fake `satisfies` for exact versions and `^1`-style majors, so these tests do not depend
/// on fnpm-semver.
fn fake_satisfies(version: &str, range: &str) -> bool {
    match range.strip_prefix('^') {
        Some(major) => version.split('.').next() == major.split('.').next(),
        None => version == range,
    }
}

fn sel_with(
    ws: &Workspace,
    patterns: &[&str],
    cwd: &Path,
    root: RootSelection,
) -> (Vec<String>, Vec<usize>) {
    let selectors = Selector::parse_all(patterns).unwrap();
    let opts = SelectOptions {
        root,
        satisfies: fake_satisfies,
        ..Default::default()
    };
    let s = select_with(ws, &selectors, cwd, &opts);
    (names(ws, &s.selected), s.unmatched_patterns)
}

#[test]
fn parse() {
    let p = |s: &str| Selector::parse(s).unwrap();
    assert_eq!(
        p("pkg"),
        Selector {
            target: Target::Name("pkg".into()),
            exclude: false,
            include_dependencies: false,
            include_dependents: false,
            exclude_self: false
        }
    );
    assert_eq!(p("*").target, Target::All);
    assert_eq!(p("**").target, Target::All);
    assert_eq!(p("@scope/*").target, Target::Name("@scope/*".into()));
    assert_eq!(
        p("./packages/*").target,
        Target::Path("./packages/*".into())
    );
    assert_eq!(p("../x").target, Target::Path("../x".into()));
    assert_eq!(p("{./apps}").target, Target::Subtree("./apps".into()));
    let s = p("!...^app^...");
    assert!(s.exclude && s.include_dependencies && s.include_dependents && s.exclude_self);
    assert_eq!(s.target, Target::Name("app".into()));
    assert!(!p("!!pkg").exclude);
    let s = p("...{./packages/pkg-a}");
    assert!(
        s.include_dependents
            && !s.include_dependencies
            && s.target == Target::Subtree("./packages/pkg-a".into())
    );
    assert!(p("app...").is_relational());
    assert!(!p("app").is_relational());
    // filter-workspace.test.ts: "a bare '...' is rejected"
    for bare in ["...", "!...", "...^", "^..."] {
        let e = Selector::parse(bare).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("--filter \"{bare}\" is missing a workspace name or path")
        );
    }
}

fn basic() -> tempfile::TempDir {
    tree(&[
        (
            "package.json",
            &json!({ "name": "root", "workspaces": ["packages/*"], "dependencies": { "a-dep": "1.0.1" } }).to_string(),
        ),
        ("packages/pkg1/package.json", &pkg("pkg1", json!({ "version": "1.0.0", "dependencies": { "no-deps": "2.0.0" } }))),
        ("packages/pkg2/package.json", &pkg("pkg2", json!({ "dependencies": { "pkg1": "1.0.0" } }))),
    ])
}

#[test]
fn names_negation_and_root() {
    let dir = basic();
    let ws = Workspace::load(dir.path()).unwrap();
    let r = dir.path();
    assert_eq!(sel(&ws, &[], r), [ROOT, "pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["pkg1"], r), ["pkg1"]);
    assert_eq!(sel(&ws, &["root"], r), [ROOT]);
    // "all but one or two"
    assert_eq!(sel(&ws, &["!pkg2"], r), [ROOT, "pkg1"]);
    assert_eq!(sel(&ws, &["!root"], r), ["pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["!pkg1"], r), [ROOT, "pkg2"]);
    assert_eq!(sel(&ws, &["*"], r), [ROOT, "pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["pkg*"], r), ["pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["*", "!pkg*"], r), [ROOT]);
}

#[test]
fn paths() {
    // "filter with a path"
    let dir = basic();
    let ws = Workspace::load(dir.path()).unwrap();
    let r = dir.path();
    assert_eq!(sel(&ws, &["./packages/pkg1"], r), ["pkg1"]);
    assert_eq!(sel(&ws, &["./packages/*"], r), ["pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["!./packages/pkg1"], r), [ROOT, "pkg2"]);
    assert_eq!(sel(&ws, &["!./packages/*"], r), [ROOT]);
    assert_eq!(sel(&ws, &["!./"], r), ["pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["./"], r), [ROOT]);
    // bun's matcher: `a/**` does not match `a`, so `./**` leaves out the root.
    assert_eq!(sel(&ws, &["./**"], r), ["pkg1", "pkg2"]);
    // relative to the cwd
    assert_eq!(sel(&ws, &["."], &r.join("packages/pkg2")), ["pkg2"]);
    assert_eq!(sel(&ws, &["../pkg1"], &r.join("packages/pkg2")), ["pkg1"]);
    assert_eq!(sel(&ws, &["./packages/pkg1/"], r), ["pkg1"]);
}

fn apps_and_packages() -> tempfile::TempDir {
    tree(&[
        (
            "package.json",
            &json!({ "name": "root", "workspaces": ["packages/*", "apps/*"] }).to_string(),
        ),
        ("packages/pkg1/package.json", &pkg("pkg1", json!({}))),
        ("packages/pkg2/package.json", &pkg("pkg2", json!({}))),
        ("apps/app1/package.json", &pkg("app1", json!({}))),
    ])
}

#[test]
fn subtree() {
    // "{dir} selects every workspace under a directory"
    let dir = apps_and_packages();
    let ws = Workspace::load(dir.path()).unwrap();
    let r = dir.path();
    assert_eq!(sel(&ws, &["{./packages}"], r), ["pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["{packages}"], r), ["pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["{./packages/pkg1}"], r), ["pkg1"]);
    assert_eq!(sel(&ws, &["{.}"], &r.join("apps")), ["app1"]);
    assert_eq!(sel(&ws, &["{.}"], r), [ROOT, "app1", "pkg1", "pkg2"]);
    assert_eq!(sel(&ws, &["!{./packages}"], r), [ROOT, "app1"]);
    assert_eq!(sel(&ws, &["{./pack*}"], r), ["pkg1", "pkg2"]);
}

fn graph() -> tempfile::TempDir {
    let p = |name: &str, deps: serde_json::Value| {
        let mut v = json!({ "name": name, "version": "1.0.0" });
        v.as_object_mut()
            .unwrap()
            .extend(deps.as_object().unwrap().clone());
        v.to_string()
    };
    tree(&[
        (
            "package.json",
            &json!({
                "name": "root", "workspaces": ["packages/*"],
                "dependencies": { "app": "workspace:*", "left-pad": "1.0.0" }
            })
            .to_string(),
        ),
        (
            "packages/app/package.json",
            &p(
                "app",
                json!({ "dependencies": { "lib": "workspace:*", "is-number": "1.0.0" } }),
            ),
        ),
        (
            "packages/lib/package.json",
            &p(
                "lib",
                json!({ "dependencies": { "util": "workspace:*", "no-deps": "1.0.0" } }),
            ),
        ),
        (
            "packages/util/package.json",
            &p("util", json!({ "dependencies": { "a-dep": "1.0.1" } })),
        ),
        (
            "packages/tool/package.json",
            &p(
                "tool",
                json!({ "devDependencies": { "lib": "1.0.0" }, "dependencies": { "no-deps-bins": "1.0.0" } }),
            ),
        ),
        (
            "packages/lone/package.json",
            &p(
                "lone",
                json!({ "dependencies": { "peer-no-deps": "1.0.0" } }),
            ),
        ),
    ])
}

#[test]
fn relations_walk_the_workspace_graph() {
    // "relation selectors walk the workspace graph"
    let dir = graph();
    let ws = Workspace::load(dir.path()).unwrap();
    let r = dir.path();
    let s = |p: &[&str]| sel_with(&ws, p, r, RootSelection::Implicit).0;
    // util and its dependents: lib, app, root (via app), tool (via its devDependency on lib)
    assert_eq!(s(&["...util"]), [ROOT, "app", "lib", "tool", "util"]);
    assert_eq!(s(&["app^..."]), ["lib", "util"]);
    assert_eq!(s(&["app..."]), ["app", "lib", "util"]);
    assert_eq!(s(&["...^util"]), [ROOT, "app", "lib", "tool"]);
    assert_eq!(s(&["...lib"]), [ROOT, "app", "lib", "tool"]);
    assert_eq!(s(&["*", "!...lib"]), ["lone", "util"]);
    assert_eq!(s(&["root..."]), [ROOT, "app", "lib", "util"]);
    assert_eq!(
        s(&["...{./packages/util}"]),
        [ROOT, "app", "lib", "tool", "util"]
    );
    // the npm range "1.0.0" links only while lib's version satisfies it
    let mut ws2 = ws.clone();
    let lib = ws2.members.iter().position(|m| m.name == "lib").unwrap();
    ws2.members[lib].version = "2.0.0".into();
    assert_eq!(
        sel_with(&ws2, &["...lib"], r, RootSelection::Implicit).0,
        [ROOT, "app", "lib"]
    );
    // linkWorkspacePackages = false: only workspace: specs are edges
    let opts = SelectOptions {
        link_workspace_packages: false,
        satisfies: fake_satisfies,
        ..Default::default()
    };
    let s2 = select_with(&ws, &Selector::parse_all(&["...lib"]).unwrap(), r, &opts);
    assert_eq!(names(&ws, &s2.selected), [ROOT, "app", "lib"]);
}

#[test]
fn relations_with_catalogs_paths_and_aliases() {
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "root", "workspaces": { "packages": ["packages/*"], "catalog": { "b": "^1.0.0" } } }).to_string(),
        ),
        ("packages/a/package.json", &pkg("a", json!({ "dependencies": { "b": "catalog:", "x": "workspace:packages/c" } }))),
        ("packages/b/package.json", &pkg("b", json!({ "version": "1.2.0" }))),
        ("packages/c/package.json", &pkg("c", json!({}))),
        ("packages/d/package.json", &pkg("d", json!({ "dependencies": { "alias": "npm:b@^1" } }))),
        ("packages/e/package.json", &pkg("e", json!({ "dependencies": { "b": "github:user/b", "c": "file:../c" } }))),
    ]);
    let ws = Workspace::load(dir.path()).unwrap();
    let s = |p: &[&str]| sel_with(&ws, p, dir.path(), RootSelection::Implicit).0;
    assert_eq!(s(&["a^..."]), ["b", "c"]);
    assert_eq!(s(&["d^..."]), ["b"]);
    assert_eq!(s(&["e^..."]), Vec::<String>::new());
    assert_eq!(s(&["...^b"]), ["a", "d"]);
}

#[test]
fn explicit_only_root() {
    // bun-add-filter.test.ts: the root only when named; `*` excludes it
    let dir = graph();
    let ws = Workspace::load(dir.path()).unwrap();
    let r = dir.path();
    let s = |p: &[&str]| sel_with(&ws, p, r, RootSelection::ExplicitOnly).0;
    assert_eq!(s(&["*"]), ["app", "lib", "lone", "tool", "util"]);
    assert_eq!(s(&[]), ["app", "lib", "lone", "tool", "util"]);
    assert_eq!(s(&["root"]), [ROOT]);
    assert_eq!(s(&["./"]), [ROOT]);
    assert_eq!(s(&["!app"]), ["lib", "lone", "tool", "util"]);
    // relations never reach the root, but the named root seeds one
    assert_eq!(s(&["...util"]), ["app", "lib", "tool", "util"]);
    assert_eq!(s(&["root..."]), [ROOT, "app", "lib", "util"]);
    assert_eq!(s(&["root^..."]), ["app", "lib", "util"]);
    // with only the root, `*` is the root
    let dir = tree(&[("package.json", r#"{"name": "solo"}"#)]);
    let solo = Workspace::load(dir.path()).unwrap();
    assert_eq!(
        sel_with(&solo, &["*"], dir.path(), RootSelection::ExplicitOnly).0,
        [ROOT]
    );
}

#[test]
fn unmatched_patterns() {
    let dir = basic();
    let ws = Workspace::load(dir.path()).unwrap();
    let (selected, unmatched) = sel_with(
        &ws,
        &["pkg1", "typo", "!pkg2", "nope*"],
        dir.path(),
        RootSelection::Implicit,
    );
    assert_eq!(selected, ["pkg1"]);
    assert_eq!(unmatched, [1, 3]);
    let selectors = Selector::parse_all(&["nosuchpkg"]).unwrap();
    assert!(matches!(
        select(&ws, &selectors, dir.path()),
        Err(Error::NoMatch)
    ));
    assert_eq!(
        unmatched_message(&["nosuchpkg"]),
        "No workspace packages matched the filter \"nosuchpkg\""
    );
    assert_eq!(
        unmatched_message(&["a", "b"]),
        "No workspace packages matched the filters \"a\", \"b\""
    );
    // an only-negative set that removes everything
    let selectors = Selector::parse_all(&["!*"]).unwrap();
    assert!(matches!(
        select(&ws, &selectors, dir.path()),
        Err(Error::NoMatch)
    ));
}

#[test]
fn scoped_names() {
    let dir = tree(&[
        (
            "package.json",
            &json!({ "workspaces": ["packages/*"] }).to_string(),
        ),
        ("packages/a/package.json", &pkg("@scope/a", json!({}))),
        ("packages/b/package.json", &pkg("@scope/b", json!({}))),
        ("packages/c/package.json", &pkg("@other/c", json!({}))),
    ]);
    let ws = Workspace::load(dir.path()).unwrap();
    assert_eq!(
        sel(&ws, &["@scope/*"], dir.path()),
        ["@scope/a", "@scope/b"]
    );
    assert_eq!(sel(&ws, &["@scope/a"], dir.path()), ["@scope/a"]);
    assert_eq!(sel(&ws, &["@*/c"], dir.path()), ["@other/c"]);
    // a nameless root is selectable by path but not by `*`'s name rule
    assert_eq!(
        sel(&ws, &["*"], dir.path()),
        [ROOT, "@scope/a", "@scope/b", "@other/c"]
    );
}

#[test]
fn workspace_protocol_resolution() {
    // bun Package.rs: `workspace:` ranges must match, `name@range` aliases, paths, root
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "root", "version": "3.0.0", "workspaces": ["packages/*"] })
                .to_string(),
        ),
        (
            "packages/a/package.json",
            &pkg("a", json!({ "version": "1.2.0" })),
        ),
        ("packages/b/package.json", &pkg("b", json!({}))),
        (
            "packages/c/package.json",
            &pkg("c", json!({ "version": "2.0.0" })),
        ),
    ]);
    let ws = Workspace::load(dir.path()).unwrap();
    let r = dir.path();
    let a_dir = r.join("packages/a");
    let res = |from: &Path, alias: &str, lit: &str| {
        ws.resolve_workspace_spec(from, alias, lit, fake_satisfies)
    };
    let a = Some(0);
    let b = Some(1);
    let c = Some(2);
    assert_eq!(res(r, "a", "workspace:*").unwrap(), a);
    assert_eq!(res(r, "a", "workspace:^").unwrap(), a);
    assert_eq!(res(r, "a", "workspace:~").unwrap(), a);
    assert_eq!(res(r, "a", "workspace:").unwrap(), a);
    assert_eq!(res(r, "a", "workspace:^1.0.0").unwrap(), a);
    assert_eq!(res(r, "a", "workspace:1.2.0").unwrap(), a);
    assert_eq!(res(r, "a", "workspace:beta").unwrap(), a);
    let err = res(r, "a", "workspace:^2.0.0").unwrap_err();
    assert_eq!(
        err.to_string(),
        "No matching version for workspace dependency \"a\". Version: \"workspace:^2.0.0\""
    );
    // a member without a version satisfies no real range
    assert!(matches!(
        res(r, "b", "workspace:^1.0.0"),
        Err(Error::NoMatchingWorkspaceVersion { .. })
    ));
    assert_eq!(res(r, "b", "workspace:*").unwrap(), b);
    // aliases
    assert_eq!(res(r, "x", "workspace:c@*").unwrap(), c);
    assert_eq!(res(r, "x", "workspace:c@^2").unwrap(), c);
    assert_eq!(res(r, "x", "workspace:c@").unwrap(), c);
    assert_eq!(res(r, "x", "workspace:*").unwrap(), None);
    // paths from the declaring package
    assert_eq!(res(&a_dir, "x", "workspace:../c").unwrap(), c);
    assert_eq!(res(r, "x", "workspace:./packages/b").unwrap(), b);
    assert_eq!(res(&a_dir, "x", "workspace:../missing").unwrap(), None);
    // the root, by name or by path
    assert_eq!(
        res(&a_dir, "root", "workspace:*").unwrap(),
        Some(usize::MAX)
    );
    assert_eq!(
        res(&a_dir, "root", "workspace:^3").unwrap(),
        Some(usize::MAX)
    );
    assert_eq!(res(r, "self", "workspace:.").unwrap(), Some(usize::MAX));
    // the relation graph uses the same rules
    let dir2 = tree(&[
        (
            "package.json",
            &json!({ "name": "root", "workspaces": ["packages/*"] }).to_string(),
        ),
        (
            "packages/a/package.json",
            &pkg(
                "a",
                json!({ "dependencies": { "x": "workspace:../c", "y": "workspace:b@*" } }),
            ),
        ),
        ("packages/b/package.json", &pkg("b", json!({}))),
        ("packages/c/package.json", &pkg("c", json!({}))),
    ]);
    let ws2 = Workspace::load(dir2.path()).unwrap();
    assert_eq!(
        sel_with(&ws2, &["a^..."], dir2.path(), RootSelection::Implicit).0,
        ["b", "c"]
    );
}

#[test]
fn trailing_globstar_path_needs_a_segment() {
    // bun's matcher: `a/**` matches `a/b` but not `a`.
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "root", "workspaces": ["packages", "packages/*"] }).to_string(),
        ),
        ("packages/package.json", &pkg("top", json!({}))),
        ("packages/a/package.json", &pkg("a", json!({}))),
    ]);
    let ws = Workspace::load(dir.path()).unwrap();
    let r = dir.path();
    assert_eq!(sel(&ws, &["./packages/**"], r), ["a"]);
    assert_eq!(sel(&ws, &["./packages"], r), ["top"]);
    let mut subtree = sel(&ws, &["{packages}"], r);
    subtree.sort();
    assert_eq!(subtree, ["a", "top"]);
}

#[test]
fn nothing_depends_on_the_root() {
    // bun `relation_graph`: the root is no workspace by name or path, so an edge never leads
    // to it, while its own dependencies are followed.
    let dir = tree(&[
        (
            "package.json",
            &json!({ "name": "root", "version": "1.0.0", "workspaces": ["packages/*"], "dependencies": { "b": "workspace:*" } }).to_string(),
        ),
        (
            "packages/a/package.json",
            &pkg("a", json!({ "dependencies": { "root": "workspace:*", "b": "workspace:*" } })),
        ),
        ("packages/b/package.json", &pkg("b", json!({}))),
        (
            "packages/c/package.json",
            &pkg("c", json!({ "dependencies": { "x": "workspace:../.." } })),
        ),
    ]);
    let ws = Workspace::load(dir.path()).unwrap();
    let r = dir.path();
    let rel = |p: &[&str]| sel_with(&ws, p, r, RootSelection::Implicit).0;
    assert_eq!(rel(&["a..."]), ["a", "b"]);
    assert_eq!(rel(&["c..."]), ["c"]);
    assert_eq!(rel(&["./..."]), [ROOT, "b"]);
    assert_eq!(rel(&["...b"]), [ROOT, "a", "b"]);
    let opts = SelectOptions {
        satisfies: fake_satisfies,
        ..Default::default()
    };
    let order = ws.dependency_order(&[usize::MAX, 0, 1, 2], &opts);
    let pos = |i: usize| order.iter().position(|&x| x == i).unwrap();
    assert!(pos(1) < pos(usize::MAX) && pos(1) < pos(0));
}

#[test]
fn cwd_with_glob_characters_selects_by_path() {
    let outer = tempfile::tempdir().unwrap();
    let r = outer.path().join("w[1]");
    common::write(
        &r,
        "package.json",
        &json!({ "name": "root", "workspaces": ["packages/*"] }).to_string(),
    );
    common::write(&r, "packages/a/package.json", &pkg("a", json!({})));
    common::write(&r, "packages/b/package.json", &pkg("b", json!({})));
    let ws = Workspace::load(&r).unwrap();
    assert_eq!(sel(&ws, &["./packages/a"], &r), ["a"]);
    assert_eq!(sel(&ws, &["{packages}"], &r), ["a", "b"]);
    assert_eq!(sel(&ws, &["../a"], &r.join("packages/b")), ["a"]);
}
