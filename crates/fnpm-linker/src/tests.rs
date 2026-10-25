//! End-to-end tests of planning and linking with an in-memory graph, a store laid out in a
//! temp dir and a fake [`PackageSource`] (no registry). Hoisted trees are written by hand so
//! each test pins the exact layout it links.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fnpm_config::{
    Auth, Config, LinkerSetting, LockfileConfig, LogLevel, NetworkConfig, Omit, RegistryScope,
};
use fnpm_lockfile::tree::{Tree, TreeNode};
use fnpm_store::{FileIndex, IndexedFile, PackageEntry, Store};
use fnpm_types::{
    Behavior, Bin, DepEdge, HashAlgorithm, Integrity, LockGraph, NoopSink, Package, PackageId,
    PlatformConstraints, Resolution,
};

use crate::isolated::{PlanFilter, PlanOptions, plan_filtered};
use crate::{Layout, LinkOptions, Linker, PackageSource};

pub(crate) fn test_config(project: &Path, store: &Path) -> Config {
    Config {
        cwd: project.to_path_buf(),
        project_dir: project.to_path_buf(),
        home_dir: store.to_path_buf(),
        store_dir: store.to_path_buf(),
        cache_dir: store.join("cache"),
        global_dir: store.join("global"),
        global_bin_dir: store.join("bin"),
        registry: RegistryScope {
            url: "https://registry.npmjs.org/".into(),
            auth: Auth::None,
            email: None,
            cert_file: None,
            key_file: None,
        },
        scopes: BTreeMap::new(),
        url_auth: Vec::new(),
        network: NetworkConfig {
            concurrency: 64,
            retries: 5,
            retry_backoff_ms: 50,
            idle_timeout_secs: 300,
            offline: Default::default(),
            manifest_cache: Default::default(),
            proxy: Default::default(),
            tls: Default::default(),
            user_agent: "fnpm-test".into(),
        },
        linker: LinkerSetting::Auto,
        backend: None,
        global_store: true,
        hoist: true,
        hoist_pattern: Vec::new(),
        public_hoist_pattern: Vec::new(),
        link_workspace_packages: true,
        frozen_lockfile: false,
        production: false,
        dry_run: false,
        force: false,
        exact: false,
        ignore_scripts: false,
        verify_integrity: true,
        save_text_lockfile: true,
        lockfile: LockfileConfig {
            save: true,
            print_yarn: false,
            path: None,
            save_path: None,
            force_save: false,
        },
        omit: Omit::default(),
        auto_install_peers: true,
        concurrent_scripts: 4,
        minimum_release_age_ms: None,
        minimum_release_age_excludes: Vec::new(),
        platform: fnpm_types::Platform::current(),
        security_scanner: None,
        log_level: LogLevel::Default,
        ci: false,
        // Fields the tests do not care about keep the crate's defaults.
        ..Config::defaults(project)
    }
}

/// A graph under construction.
pub(crate) struct G {
    pub g: LockGraph,
}

impl G {
    pub fn new() -> Self {
        Self {
            g: LockGraph::new(Package::new("app", "1.0.0", Resolution::Root)),
        }
    }

    pub fn npm(&mut self, name: &str, version: &str) -> PackageId {
        let i = Integrity::compute(
            HashAlgorithm::Sha512,
            format!("{name}@{version}").as_bytes(),
        );
        self.g.push(Package::new(
            name,
            version,
            Resolution::npm(version, Some(i)),
        ))
    }

    pub fn with(&mut self, name: &str, version: &str, resolution: Resolution) -> PackageId {
        self.g.push(Package::new(name, version, resolution))
    }

    pub fn workspace(&mut self, name: &str, path: &str) -> PackageId {
        let id = self.g.add_workspace(Package::new(
            name,
            "1.0.0",
            Resolution::Workspace { path: path.into() },
        ));
        self.edge(
            PackageId::ROOT,
            name,
            id,
            Behavior::WORKSPACE | Behavior::PROD,
            "workspace:*",
        );
        id
    }

    pub fn dep(&mut self, from: PackageId, to: PackageId) {
        let name = self.g.packages[to.index()].name.clone();
        self.edge(from, &name, to, Behavior::PROD, "^1");
    }

    pub fn edge(
        &mut self,
        from: PackageId,
        alias: &str,
        to: PackageId,
        behavior: Behavior,
        literal: &str,
    ) {
        let mut e = DepEdge::new(alias, literal, behavior);
        e.target = Some(to);
        self.g.packages[from.index()].dependencies.push(e);
    }

    pub fn unresolved(&mut self, from: PackageId, alias: &str, behavior: Behavior) {
        self.g.packages[from.index()]
            .dependencies
            .push(DepEdge::new(alias, "^1", behavior));
    }

    pub fn bin(&mut self, id: PackageId, bin: Bin) {
        self.g.packages[id.index()].bin = bin;
    }
}

pub(crate) fn opts<'a>(trusted: &'a dyn Fn(PackageId) -> bool) -> PlanOptions<'a> {
    PlanOptions {
        hoist: true,
        hoist_pattern: &[],
        public_hoist_pattern: &[],
        global_store: false,
        trusted,
    }
}

fn no_trust(_: PackageId) -> bool {
    false
}

fn plan_default(g: &LockGraph) -> crate::IsolatedLayout {
    plan_filtered(g, &opts(&no_trust), &PlanFilter::default())
}

fn entry_of(l: &crate::IsolatedLayout, g: &LockGraph, name: &str) -> Vec<u32> {
    l.entries
        .iter()
        .enumerate()
        .filter(|(_, e)| g.packages[e.package.index()].name == name)
        .map(|(i, _)| i as u32)
        .collect()
}

fn deps_of(l: &crate::IsolatedLayout, i: u32) -> Vec<(String, String)> {
    l.entries[i as usize]
        .deps
        .iter()
        .map(|(a, d)| (a.clone(), l.entries[*d as usize].key.clone()))
        .collect()
}

#[test]
fn plan_simple_chain() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    let bb = b.npm("b", "2.0.0");
    b.dep(PackageId::ROOT, a);
    b.dep(a, bb);
    let l = plan_default(&b.g);
    assert_eq!(l.entries.len(), 3);
    assert_eq!(l.entries[0].key, "app@root");
    assert_eq!(deps_of(&l, 0), [("a".to_owned(), "a@1.0.0".to_owned())]);
    let ai = entry_of(&l, &b.g, "a")[0];
    assert_eq!(deps_of(&l, ai), [("b".to_owned(), "b@2.0.0".to_owned())]);
    assert_eq!(l.roots, vec![(PackageId::ROOT, vec![("a".to_owned(), ai)])]);
    let hidden: Vec<_> = l.hidden_hoist.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(hidden, ["a", "b"]);
    assert!(l.entries.iter().all(|e| e.global_hash.is_none()));
}

#[test]
fn plan_shares_entries() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    let c = b.npm("c", "1.0.0");
    let shared = b.npm("shared", "1.0.0");
    b.dep(PackageId::ROOT, a);
    b.dep(PackageId::ROOT, c);
    b.dep(a, shared);
    b.dep(c, shared);
    let l = plan_default(&b.g);
    assert_eq!(entry_of(&l, &b.g, "shared").len(), 1);
    assert_eq!(l.entries.len(), 4);
}

#[test]
fn plan_cycles_terminate() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    let c = b.npm("c", "1.0.0");
    b.dep(PackageId::ROOT, a);
    b.dep(a, c);
    b.dep(c, a);
    let l = plan_default(&b.g);
    assert_eq!(l.entries.len(), 3);
    let ci = entry_of(&l, &b.g, "c")[0];
    assert_eq!(deps_of(&l, ci), [("a".to_owned(), "a@1.0.0".to_owned())]);
}

#[test]
fn plan_peers_from_the_parent() {
    let mut b = G::new();
    let react = b.npm("react", "18.3.1");
    let plugin = b.npm("plugin", "1.0.0");
    b.dep(PackageId::ROOT, react);
    b.dep(PackageId::ROOT, plugin);
    b.edge(plugin, "react", react, Behavior::PEER, "*");
    let l = plan_default(&b.g);
    let pi = entry_of(&l, &b.g, "plugin");
    assert_eq!(pi.len(), 1);
    let key = &l.entries[pi[0] as usize].key;
    assert!(
        key.starts_with("plugin@1.0.0+") && key.len() == "plugin@1.0.0+".len() + 16,
        "{key}"
    );
    assert_eq!(
        deps_of(&l, pi[0]),
        [("react".to_owned(), "react@18.3.1".to_owned())]
    );
    // react itself has no peers: no suffix.
    assert_eq!(
        l.entries[entry_of(&l, &b.g, "react")[0] as usize].key,
        "react@18.3.1"
    );
}

#[test]
fn plan_peer_contexts_split_entries() {
    let mut b = G::new();
    let r17 = b.npm("react", "17.0.2");
    let r18 = b.npm("react", "18.3.1");
    let plugin = b.npm("plugin", "1.0.0");
    let w1 = b.workspace("w1", "packages/w1");
    let w2 = b.workspace("w2", "packages/w2");
    b.dep(w1, r17);
    b.dep(w1, plugin);
    b.dep(w2, r18);
    b.dep(w2, plugin);
    b.edge(plugin, "react", r18, Behavior::PEER, "*");
    let l = plan_default(&b.g);
    let pi = entry_of(&l, &b.g, "plugin");
    assert_eq!(pi.len(), 2);
    let keys: Vec<_> = pi
        .iter()
        .map(|&i| l.entries[i as usize].key.clone())
        .collect();
    assert_ne!(keys[0], keys[1]);
    let mut targets: Vec<_> = pi.iter().map(|&i| deps_of(&l, i)[0].1.clone()).collect();
    targets.sort();
    assert_eq!(targets, ["react@17.0.2", "react@18.3.1"]);
    // Both workspaces are importers.
    let roots: Vec<_> = l
        .roots
        .iter()
        .map(|(p, _)| b.g.packages[p.index()].name.clone())
        .collect();
    assert_eq!(roots, ["app", "w1", "w2"]);
}

#[test]
fn plan_unresolved_peer_installs_the_lockfile_choice() {
    let mut b = G::new();
    let react = b.npm("react", "18.3.1");
    let plugin = b.npm("plugin", "1.0.0");
    b.dep(PackageId::ROOT, plugin);
    b.edge(plugin, "react", react, Behavior::PEER, "*");
    let l = plan_default(&b.g);
    let pi = entry_of(&l, &b.g, "plugin")[0];
    assert_eq!(
        deps_of(&l, pi),
        [("react".to_owned(), "react@18.3.1".to_owned())]
    );
    assert!(!l.roots[0].1.iter().any(|(a, _)| a == "react"));
}

#[test]
fn plan_optional_peer_without_provider_is_skipped() {
    let mut b = G::new();
    let plugin = b.npm("plugin", "1.0.0");
    b.dep(PackageId::ROOT, plugin);
    b.unresolved(plugin, "typescript", Behavior::PEER | Behavior::OPTIONAL);
    let l = plan_default(&b.g);
    assert_eq!(l.entries.len(), 2);
    assert!(deps_of(&l, 1).is_empty());
}

#[test]
fn plan_package_satisfies_its_own_peer() {
    let mut b = G::new();
    let x = b.npm("x", "1.0.0");
    let p = b.npm("p", "1.0.0");
    b.dep(PackageId::ROOT, p);
    b.dep(p, x);
    b.edge(p, "x", x, Behavior::PEER, "*");
    let l = plan_default(&b.g);
    let pi = entry_of(&l, &b.g, "p")[0];
    assert_eq!(l.entries[pi as usize].key, "p@1.0.0");
    assert_eq!(deps_of(&l, pi), [("x".to_owned(), "x@1.0.0".to_owned())]);
}

#[test]
fn plan_dev_and_optional_filters() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    let devtool = b.npm("devtool", "1.0.0");
    let nested_dev = b.npm("nested-dev", "1.0.0");
    let opt = b.npm("opt", "1.0.0");
    b.dep(PackageId::ROOT, a);
    b.edge(PackageId::ROOT, "devtool", devtool, Behavior::DEV, "^1");
    b.edge(a, "nested-dev", nested_dev, Behavior::DEV, "^1");
    b.edge(a, "opt", opt, Behavior::OPTIONAL, "^1");
    let l = plan_default(&b.g);
    let names: Vec<_> = l
        .entries
        .iter()
        .map(|e| b.g.packages[e.package.index()].name.as_str())
        .collect();
    assert!(names.contains(&"devtool"));
    assert!(names.contains(&"opt"));
    assert!(
        !names.contains(&"nested-dev"),
        "dev deps of registry packages are never installed"
    );
    let f = PlanFilter {
        include: Behavior::PROD | Behavior::PEER,
        ..Default::default()
    };
    let l = plan_filtered(&b.g, &opts(&no_trust), &f);
    let names: Vec<_> = l
        .entries
        .iter()
        .map(|e| b.g.packages[e.package.index()].name.as_str())
        .collect();
    assert_eq!(names, ["app", "a"]);
}

#[test]
fn plan_platform_mismatch_is_skipped() {
    let mut b = G::new();
    let esb = b.npm("esbuild", "0.21.5");
    let other = b.npm("@esbuild/other-os", "0.21.5");
    b.g.packages[other.index()].platform =
        PlatformConstraints::from_package_json(&["aix"], &["ppc64"], &[] as &[&str]);
    b.dep(PackageId::ROOT, esb);
    b.edge(
        esb,
        "@esbuild/other-os",
        other,
        Behavior::OPTIONAL,
        "0.21.5",
    );
    let l = plan_default(&b.g);
    assert_eq!(l.entries.len(), 2);
}

#[test]
fn plan_aliases_and_bundled() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    let bundled = b.npm("inner", "1.0.0");
    b.edge(PackageId::ROOT, "my-a", a, Behavior::PROD, "npm:a@1");
    b.edge(a, "inner", bundled, Behavior::PROD | Behavior::BUNDLED, "1");
    let l = plan_default(&b.g);
    assert_eq!(deps_of(&l, 0), [("my-a".to_owned(), "a@1.0.0".to_owned())]);
    assert_eq!(l.entries.len(), 2);
}

#[test]
fn plan_hoist_patterns() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    let eslint_plugin = b.npm("eslint-plugin-x", "1.0.0");
    let other = b.npm("other", "1.0.0");
    b.dep(PackageId::ROOT, a);
    b.dep(a, eslint_plugin);
    b.dep(a, other);
    let public = vec!["*eslint*".to_owned()];
    let hoist = vec!["other".to_owned()];
    let o = PlanOptions {
        hoist: true,
        hoist_pattern: &hoist,
        public_hoist_pattern: &public,
        global_store: false,
        trusted: &no_trust,
    };
    let l = plan_filtered(&b.g, &o, &PlanFilter::default());
    let public: Vec<_> = l.public_hoist.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(public, ["eslint-plugin-x"]);
    let hidden: Vec<_> = l.hidden_hoist.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(hidden, ["other"]);
    // Direct dependencies are never replaced by public hoisting.
    let o = PlanOptions {
        hoist: false,
        hoist_pattern: &[],
        public_hoist_pattern: &["*".to_owned()],
        global_store: false,
        trusted: &no_trust,
    };
    let l = plan_filtered(&b.g, &o, &PlanFilter::default());
    assert!(l.hidden_hoist.is_empty());
    let public: Vec<_> = l.public_hoist.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(public, ["eslint-plugin-x", "other"]);
}

#[test]
fn plan_hidden_hoist_first_claimant_wins() {
    let mut b = G::new();
    let x1 = b.npm("x", "1.0.0");
    let x2 = b.npm("x", "2.0.0");
    let a = b.npm("a", "1.0.0");
    b.dep(PackageId::ROOT, x1);
    b.dep(PackageId::ROOT, a);
    b.dep(a, x2);
    let l = plan_default(&b.g);
    let (_, e) = l.hidden_hoist.iter().find(|(n, _)| n == "x").unwrap();
    assert_eq!(l.entries[*e as usize].key, "x@1.0.0");
}

#[test]
fn plan_workspaces_and_links() {
    let mut b = G::new();
    let w = b.workspace("w", "packages/w");
    let lib = b.npm("lib", "1.0.0");
    let linked = b.with(
        "linked",
        "1.0.0",
        Resolution::Symlink {
            path: "../linked".into(),
        },
    );
    b.dep(w, lib);
    b.edge(
        PackageId::ROOT,
        "linked",
        linked,
        Behavior::PROD,
        "link:../linked",
    );
    b.edge(w, "w-self", w, Behavior::PROD, "workspace:*");
    let l = plan_default(&b.g);
    let root_deps: Vec<_> = l.roots[0].1.iter().map(|(a, _)| a.clone()).collect();
    assert_eq!(
        root_deps,
        ["linked"],
        "workspace membership is not a root link"
    );
    let w_deps: Vec<_> = l.roots[1].1.iter().map(|(a, _)| a.clone()).collect();
    assert!(w_deps.contains(&"lib".to_owned()));
    assert!(w_deps.contains(&"w-self".to_owned()));
}

#[test]
fn plan_filter_selects_workspaces() {
    let mut b = G::new();
    let w1 = b.workspace("w1", "packages/w1");
    let w2 = b.workspace("w2", "packages/w2");
    let l1 = b.npm("l1", "1.0.0");
    let l2 = b.npm("l2", "1.0.0");
    b.dep(w1, l1);
    b.dep(w2, l2);
    let f = PlanFilter {
        workspaces: Some(vec![w1]),
        ..Default::default()
    };
    let l = plan_filtered(&b.g, &opts(&no_trust), &f);
    let names: Vec<_> = l
        .entries
        .iter()
        .map(|e| b.g.packages[e.package.index()].name.as_str())
        .collect();
    assert!(names.contains(&"l1"));
    assert!(!names.contains(&"l2"));
}

fn global(g: &LockGraph, trusted: &dyn Fn(PackageId) -> bool) -> crate::IsolatedLayout {
    let o = PlanOptions {
        hoist: true,
        hoist_pattern: &[],
        public_hoist_pattern: &[],
        global_store: true,
        trusted,
    };
    plan_filtered(g, &o, &PlanFilter::default())
}

#[test]
fn global_hashes_eligibility_and_propagation() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    let c = b.npm("c", "1.0.0");
    let d = b.npm("d", "1.0.0");
    let w = b.workspace("w", "packages/w");
    let uses_w = b.npm("uses-w", "1.0.0");
    let folder = b.with("f", "1.0.0", Resolution::Folder { path: "f".into() });
    b.dep(PackageId::ROOT, a);
    b.dep(a, c);
    b.dep(PackageId::ROOT, d);
    b.dep(PackageId::ROOT, uses_w);
    b.edge(uses_w, "w", w, Behavior::PROD, "*");
    b.edge(PackageId::ROOT, "f", folder, Behavior::PROD, "file:f");
    let l = global(&b.g, &no_trust);
    let h = |name: &str| l.entries[entry_of(&l, &b.g, name)[0] as usize].global_hash;
    assert!(h("a").is_some() && h("c").is_some() && h("d").is_some());
    assert_eq!(h("app"), None);
    assert_eq!(h("w"), None);
    assert_eq!(h("uses-w"), None, "depends on a workspace");
    assert_eq!(h("f"), None);
    assert_ne!(h("a"), h("c"));
    // Deterministic.
    assert_eq!(global(&b.g, &no_trust), l);
    let ai = entry_of(&l, &b.g, "a")[0];
    assert_eq!(
        l.global_dir_name(ai),
        Some(format!("a@1.0.0-{:016x}", h("a").unwrap()))
    );
    // Trusted: the entry and everything it links are local.
    let trusted = move |id: PackageId| id == a;
    let l2 = global(&b.g, &trusted);
    let h2 = |name: &str| l2.entries[entry_of(&l2, &b.g, name)[0] as usize].global_hash;
    assert_eq!(h2("a"), None);
    assert_eq!(h2("c"), None);
    assert!(h2("d").is_some());
}

#[test]
fn global_hash_depends_on_closure() {
    let mut b1 = G::new();
    let a = b1.npm("a", "1.0.0");
    let c = b1.npm("c", "1.0.0");
    b1.dep(PackageId::ROOT, a);
    b1.dep(a, c);
    let mut b2 = G::new();
    let a2 = b2.npm("a", "1.0.0");
    let c2 = b2.npm("c", "1.0.1");
    b2.dep(PackageId::ROOT, a2);
    b2.dep(a2, c2);
    let l1 = global(&b1.g, &no_trust);
    let l2 = global(&b2.g, &no_trust);
    let h = |l: &crate::IsolatedLayout, g: &LockGraph| {
        l.entries[entry_of(l, g, "a")[0] as usize]
            .global_hash
            .unwrap()
    };
    assert_ne!(h(&l1, &b1.g), h(&l2, &b2.g));
    // Same closure in another project (different root name, extra unrelated dep): same hash.
    let mut b3 = G::new();
    b3.g.packages[0].name = "other-app".into();
    let z = b3.npm("z", "9.0.0");
    let a3 = b3.npm("a", "1.0.0");
    let c3 = b3.npm("c", "1.0.0");
    b3.dep(PackageId::ROOT, z);
    b3.dep(PackageId::ROOT, a3);
    b3.dep(a3, c3);
    let l3 = global(&b3.g, &no_trust);
    assert_eq!(h(&l1, &b1.g), h(&l3, &b3.g));
}

#[test]
fn global_hash_cycles_share_one_hash() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    let c = b.npm("c", "1.0.0");
    b.dep(PackageId::ROOT, a);
    b.dep(a, c);
    b.dep(c, a);
    let l = global(&b.g, &no_trust);
    let ha = l.entries[entry_of(&l, &b.g, "a")[0] as usize].global_hash;
    let hc = l.entries[entry_of(&l, &b.g, "c")[0] as usize].global_hash;
    assert!(ha.is_some());
    assert_eq!(ha, hc);
}

#[test]
fn global_hash_patched_and_types_quirk() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    b.dep(PackageId::ROOT, a);
    b.g.patched_dependencies.insert(
        "a@1.0.0".into(),
        fnpm_types::PatchedDependency {
            path: "patches/a.patch".into(),
            hash: None,
        },
    );
    let l = global(&b.g, &no_trust);
    assert_eq!(l.entries[1].global_hash, None);

    let mut b = G::new();
    let react = b.npm("react", "18.3.1");
    let types = b.npm("@types/react", "18.3.1");
    let lib = b.npm("lib", "1.0.0");
    b.dep(PackageId::ROOT, react);
    b.dep(PackageId::ROOT, types);
    b.dep(PackageId::ROOT, lib);
    b.edge(lib, "react", react, Behavior::PEER, "*");
    let l = global(&b.g, &no_trust);
    let li = entry_of(&l, &b.g, "lib")[0];
    assert_eq!(
        l.entries[li as usize].global_hash, None,
        "TypeScript could not reach the project's @types/react"
    );
    assert!(
        l.entries[entry_of(&l, &b.g, "react")[0] as usize]
            .global_hash
            .is_some()
    );
}

#[test]
fn tarball_entries_carry_their_pin() {
    let mut b = G::new();
    let i = Integrity::compute(HashAlgorithm::Sha512, b"tarball");
    let t = b.with(
        "t",
        "1.0.0",
        Resolution::RemoteTarball {
            url: "http://h/t.tgz".into(),
            integrity: Some(i),
        },
    );
    b.dep(PackageId::ROOT, t);
    let l = plan_default(&b.g);
    let hex = i.to_hex();
    assert_eq!(l.entries[1].key, format!("t@http+++h+t.tgz+{}", &hex[..16]));
}
/// Packages laid out like the store's `pkgs/<dir>/package`.
pub(crate) struct FakeStore {
    pub root: PathBuf,
    pub entries: HashMap<u32, PackageEntry>,
}

impl FakeStore {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            entries: HashMap::new(),
        }
    }

    pub fn add(&mut self, id: PackageId, name: &str, version: &str, files: &[(&str, &str)]) {
        let dir = self
            .root
            .join("pkgs")
            .join(format!("{}@{version}-{}", name.replace('/', "+"), id.0))
            .join("package");
        let mut all: Vec<(String, String)> = vec![(
            "package.json".into(),
            format!(r#"{{"name":"{name}","version":"{version}"}}"#),
        )];
        all.extend(
            files
                .iter()
                .map(|(p, c)| ((*p).to_owned(), (*c).to_owned())),
        );
        let mut dirs = std::collections::BTreeSet::new();
        let mut idx_files = Vec::new();
        for (p, c) in &all {
            let f = dir.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, c).unwrap();
            let mut parent = Path::new(p).parent();
            while let Some(d) = parent {
                if !d.as_os_str().is_empty() {
                    dirs.insert(d.to_string_lossy().into_owned());
                }
                parent = d.parent();
            }
            idx_files.push(IndexedFile {
                path: p.clone(),
                size: c.len() as u64,
                mode: 0o644,
                digest: [0; 64],
            });
        }
        let index = FileIndex {
            dirs: dirs.into_iter().collect(),
            files: idx_files,
            package_json: None,
        };
        self.entries.insert(
            id.0,
            PackageEntry {
                root: dir,
                index: Arc::new(index),
            },
        );
    }
}

impl PackageSource for FakeStore {
    fn wait(&self, id: PackageId) -> Result<PackageEntry, String> {
        self.entries
            .get(&id.0)
            .cloned()
            .ok_or_else(|| format!("no package {}", id.0))
    }
}

struct Env {
    _t: tempfile::TempDir,
    project: PathBuf,
    store_dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let project = t.path().join("project");
        let store_dir = t.path().join("store");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&store_dir).unwrap();
        Self {
            _t: t,
            project,
            store_dir,
        }
    }

    fn linker(&self) -> Linker {
        let cfg = test_config(&self.project, &self.store_dir);
        Linker::new(Arc::new(cfg), Store::open(&self.store_dir).unwrap())
    }

    fn nm(&self, p: &str) -> PathBuf {
        self.project.join("node_modules").join(p)
    }
}

fn lopts(global: bool) -> LinkOptions {
    LinkOptions {
        backend: fnpm_config::Backend::Hardlink,
        force: false,
        fresh: false,
        threads: 4,
        global_store: global,
    }
}

/// root -> a@1 (bin `a`) -> b@1; root -> c@1 -> b@2.
fn sample(store: &mut FakeStore) -> G {
    let mut g = G::new();
    let a = g.npm("a", "1.0.0");
    let b1 = g.npm("b", "1.0.0");
    let b2 = g.npm("b", "2.0.0");
    let c = g.npm("c", "1.0.0");
    g.dep(PackageId::ROOT, a);
    g.dep(PackageId::ROOT, c);
    g.dep(a, b1);
    g.dep(c, b2);
    g.bin(
        a,
        Bin::File {
            path: "cli.js".into(),
        },
    );
    g.bin(
        b1,
        Bin::NamedFile {
            name: "bee".into(),
            path: "bee.js".into(),
        },
    );
    store.add(a, "a", "1.0.0", &[("cli.js", "#!/usr/bin/env node\r\n")]);
    store.add(b1, "b", "1.0.0", &[("bee.js", ""), ("lib/x.js", "1")]);
    store.add(b2, "b", "2.0.0", &[("lib/x.js", "2")]);
    store.add(c, "c", "1.0.0", &[]);
    g
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn isolated_local_fresh_then_warm() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let g = sample(&mut store);
    let linker = env.linker();
    let layout = Layout::Isolated(linker.plan_isolated(&g.g, None));
    let r = linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r.installed, 4);
    assert_eq!(r.backend_used, Some(fnpm_config::Backend::Hardlink));
    assert_eq!(
        read(&env.nm("a/package.json")),
        r#"{"name":"a","version":"1.0.0"}"#
    );
    assert_eq!(
        std::fs::read_link(env.nm("a")).unwrap(),
        PathBuf::from(".fnpm/a@1.0.0/node_modules/a")
    );
    assert_eq!(read(&env.nm(".fnpm/a@1.0.0/node_modules/b/lib/x.js")), "1");
    assert_eq!(read(&env.nm(".fnpm/c@1.0.0/node_modules/b/lib/x.js")), "2");
    assert_eq!(
        std::fs::read_link(env.nm(".fnpm/a@1.0.0/node_modules/b")).unwrap(),
        PathBuf::from("../../b@1.0.0/node_modules/b")
    );
    // Bins: root gets direct deps' bins; entries get their deps' bins and their own.
    assert_eq!(
        std::fs::read_link(env.nm(".bin/a")).unwrap(),
        PathBuf::from("../a/cli.js")
    );
    assert!(std::fs::symlink_metadata(env.nm(".bin/bee")).is_err());
    assert_eq!(
        std::fs::read_link(env.nm(".fnpm/a@1.0.0/node_modules/.bin/bee")).unwrap(),
        PathBuf::from("../b/bee.js")
    );
    assert_eq!(
        std::fs::read_link(env.nm(".fnpm/a@1.0.0/node_modules/.bin/a")).unwrap(),
        PathBuf::from("../a/cli.js")
    );
    assert_eq!(read(&env.nm("a/cli.js")), "#!/usr/bin/env node\n");
    // Hidden hoist: first claimant of `b` is a's b@1.
    assert_eq!(
        std::fs::read_link(env.nm(".fnpm/node_modules/b")).unwrap(),
        PathBuf::from("../b@1.0.0/node_modules/b")
    );
    assert!(crate::is_isolated_tree(&env.project));

    let r2 = linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r2.installed, 0);
    assert_eq!(r2.removed, 0);

    // A stray top-level package is removed on the next install; a damaged entry is repaired.
    std::fs::create_dir_all(env.nm("stray")).unwrap();
    std::fs::remove_file(env.nm(".fnpm/c@1.0.0/node_modules/c/package.json")).unwrap();
    let r3 = linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r3.installed, 1);
    assert_eq!(r3.removed, 1);
    assert!(!env.nm("stray").exists());
    assert!(env.nm("c/package.json").exists());

    // --force rebuilds everything.
    let mut f = lopts(false);
    f.force = true;
    assert_eq!(
        linker
            .link(&env.project, &g.g, &layout, &store, &f, &NoopSink)
            .unwrap()
            .installed,
        4
    );
    crate::flush_background_deletes();
}

#[test]
fn isolated_global_store_cold_and_warm() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let g = sample(&mut store);
    let linker = env.linker();
    let plan = linker.plan_isolated(&g.g, None);
    assert!(plan.entries.iter().skip(1).all(|e| e.global_hash.is_some()));
    let layout = Layout::Isolated(plan.clone());
    let r = linker
        .link(&env.project, &g.g, &layout, &store, &lopts(true), &NoopSink)
        .unwrap();
    assert_eq!(r.installed, 4);
    let links = env.store_dir.join("links");
    let a_dir = links.join(
        plan.global_dir_name(
            plan.entries
                .iter()
                .position(|e| e.key == "a@1.0.0")
                .unwrap() as u32,
        )
        .unwrap(),
    );
    assert_eq!(std::fs::read_link(env.nm(".fnpm/a@1.0.0")).unwrap(), a_dir);
    assert_eq!(
        read(&env.nm("a/package.json")),
        r#"{"name":"a","version":"1.0.0"}"#
    );
    assert_eq!(read(&env.nm(".fnpm/a@1.0.0/node_modules/b/lib/x.js")), "1");
    // Inside the global entry the dependency link is relative to the links dir.
    let b_link = std::fs::read_link(a_dir.join("node_modules/b")).unwrap();
    assert!(
        b_link.to_string_lossy().starts_with("../../b@1.0.0-"),
        "{}",
        b_link.display()
    );
    // Bins of a dependency inside a global entry were linked while it was still staged.
    assert_eq!(
        std::fs::read_link(a_dir.join("node_modules/.bin/bee")).unwrap(),
        PathBuf::from("../b/bee.js")
    );
    assert!(a_dir.join("node_modules/.bin/bee").exists());
    // No staging left.
    crate::flush_background_deletes();
    let names: Vec<String> = std::fs::read_dir(&links)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
    assert_eq!(names.len(), 4);

    // A second project with the same closure: warm hit, nothing materialised.
    let p2 = env.project.parent().unwrap().join("project2");
    std::fs::create_dir_all(&p2).unwrap();
    let r2 = linker
        .link(&p2, &g.g, &layout, &store, &lopts(true), &NoopSink)
        .unwrap();
    assert_eq!(r2.installed, 0);
    assert_eq!(
        read(&p2.join("node_modules/.fnpm/c@1.0.0/node_modules/b/lib/x.js")),
        "2"
    );
    assert_eq!(
        std::fs::read_link(p2.join("node_modules/.bin/a")).unwrap(),
        PathBuf::from("../a/cli.js")
    );
    // Warm relink of the first project writes nothing.
    let r3 = linker
        .link(&env.project, &g.g, &layout, &store, &lopts(true), &NoopSink)
        .unwrap();
    assert_eq!((r3.installed, r3.removed), (0, 0));
    // Switching off the global store turns the links into local entries.
    let r4 = linker
        .link(
            &env.project,
            &g.g,
            &Layout::Isolated(linker.plan_isolated(&g.g, None)),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r4.installed, 4);
    assert_eq!(
        crate::fsutil::kind(&env.nm(".fnpm/a@1.0.0")),
        crate::fsutil::Kind::Dir
    );
}

#[test]
fn isolated_force_republishes_global_entries() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let g = sample(&mut store);
    let linker = env.linker();
    let layout = Layout::Isolated(linker.plan_isolated(&g.g, None));
    linker
        .link(&env.project, &g.g, &layout, &store, &lopts(true), &NoopSink)
        .unwrap();
    let mut f = lopts(true);
    f.force = true;
    assert_eq!(
        linker
            .link(&env.project, &g.g, &layout, &store, &f, &NoopSink)
            .unwrap()
            .installed,
        4
    );
    assert_eq!(
        read(&env.nm("a/package.json")),
        r#"{"name":"a","version":"1.0.0"}"#
    );
    crate::flush_background_deletes();
}

#[test]
fn isolated_workspaces_links_and_public_hoist() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = G::new();
    let w = g.workspace("@app/w", "packages/w");
    let lib = g.npm("lib", "1.0.0");
    let eslint = g.npm("eslint-x", "1.0.0");
    let linked = g.with(
        "linked",
        "1.0.0",
        Resolution::Symlink {
            path: "../outside".into(),
        },
    );
    g.dep(w, lib);
    g.dep(lib, eslint);
    g.edge(PackageId::ROOT, "@app/w", w, Behavior::PROD, "workspace:*");
    g.edge(
        PackageId::ROOT,
        "linked",
        linked,
        Behavior::PROD,
        "link:../outside",
    );
    store.add(lib, "lib", "1.0.0", &[]);
    store.add(eslint, "eslint-x", "1.0.0", &[]);
    std::fs::create_dir_all(env.project.join("packages/w")).unwrap();
    std::fs::create_dir_all(env.project.parent().unwrap().join("outside")).unwrap();
    let mut cfg = test_config(&env.project, &env.store_dir);
    cfg.public_hoist_pattern = vec!["*eslint*".into()];
    let linker = Linker::new(Arc::new(cfg), Store::open(&env.store_dir).unwrap());
    let layout = Layout::Isolated(linker.plan_isolated(&g.g, None));
    linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(
        std::fs::read_link(env.nm("@app/w")).unwrap(),
        PathBuf::from("../../packages/w")
    );
    assert_eq!(
        std::fs::read_link(env.nm("linked")).unwrap(),
        PathBuf::from("../../outside")
    );
    assert_eq!(
        std::fs::read_link(env.project.join("packages/w/node_modules/lib")).unwrap(),
        PathBuf::from("../../../node_modules/.fnpm/lib@1.0.0/node_modules/lib")
    );
    assert_eq!(
        std::fs::read_link(env.nm("eslint-x")).unwrap(),
        PathBuf::from(".fnpm/eslint-x@1.0.0/node_modules/eslint-x")
    );
    assert!(env.nm("eslint-x/package.json").exists());
}

#[test]
fn isolated_missing_optional_is_skipped_and_required_fails() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = G::new();
    let a = g.npm("a", "1.0.0");
    let opt = g.npm("opt", "1.0.0");
    g.dep(PackageId::ROOT, a);
    g.edge(a, "opt", opt, Behavior::OPTIONAL, "^1");
    store.add(a, "a", "1.0.0", &[]);
    let linker = env.linker();
    let layout = Layout::Isolated(linker.plan_isolated(&g.g, None));
    let r = linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r.skipped, 1);
    assert!(std::fs::symlink_metadata(env.nm(".fnpm/a@1.0.0/node_modules/opt")).is_err());

    let mut g2 = G::new();
    let missing = g2.npm("missing", "1.0.0");
    g2.dep(PackageId::ROOT, missing);
    let layout = Layout::Isolated(linker.plan_isolated(&g2.g, None));
    let p2 = env.project.parent().unwrap().join("p2");
    std::fs::create_dir_all(&p2).unwrap();
    let empty = FakeStore::new(&env.store_dir);
    let err = linker
        .link(&p2, &g2.g, &layout, &empty, &lopts(false), &NoopSink)
        .unwrap_err();
    assert!(matches!(err, crate::Error::Missing { .. }), "{err}");
}

/// The hoisted tree of `sample`: a, c, b@1 at the top; c/node_modules/b@2.
fn sample_tree() -> Tree {
    let n = |package: u32, name: &str, parent: Option<u32>, path: &str| TreeNode {
        package: PackageId(package),
        name: name.into(),
        parent,
        path: path.into(),
    };
    Tree {
        nodes: vec![
            n(1, "a", None, "a"),
            n(2, "b", None, "b"),
            n(4, "c", None, "c"),
            n(3, "b", Some(2), "c/b"),
        ],
        workspace_roots: Vec::new(),
    }
}

#[test]
fn hoisted_fresh_warm_and_replace() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let g = sample(&mut store);
    let linker = env.linker();
    let layout = Layout::Hoisted(sample_tree());
    let r = linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r.installed, 4);
    assert_eq!(read(&env.nm("b/lib/x.js")), "1");
    assert_eq!(read(&env.nm("c/node_modules/b/lib/x.js")), "2");
    assert_eq!(
        std::fs::read_link(env.nm(".bin/a")).unwrap(),
        PathBuf::from("../a/cli.js")
    );
    assert_eq!(
        std::fs::read_link(env.nm(".bin/bee")).unwrap(),
        PathBuf::from("../b/bee.js")
    );
    assert_eq!(r.dirs.len(), 4);
    {
        use std::os::unix::fs::MetadataExt;
        let src = store.entries[&2].root.join("lib/x.js");
        assert_eq!(
            std::fs::metadata(src).unwrap().ino(),
            std::fs::metadata(env.nm("b/lib/x.js")).unwrap().ino()
        );
    }

    let r2 = linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!((r2.installed, r2.removed), (0, 0));

    // A wrong version is replaced; stray and nested-stale folders are removed.
    // (A new inode: the installed file is a hard link to the store's.)
    std::fs::remove_file(env.nm("b/package.json")).unwrap();
    std::fs::write(
        env.nm("b/package.json"),
        r#"{"name":"b","version":"0.0.1"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(env.nm("stray")).unwrap();
    std::fs::create_dir_all(env.nm("@scope/gone")).unwrap();
    std::fs::create_dir_all(env.nm("a/node_modules/old")).unwrap();
    std::fs::create_dir_all(env.nm(".vite")).unwrap();
    let r3 = linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r3.installed, 1);
    assert_eq!(r3.removed, 3);
    assert_eq!(
        read(&env.nm("b/package.json")),
        r#"{"name":"b","version":"1.0.0"}"#
    );
    assert!(
        !env.nm("stray").exists()
            && !env.nm("@scope").exists()
            && !env.nm("a/node_modules/old").exists()
    );
    assert!(env.nm(".vite").exists(), "other tools' dot entries stay");
    crate::flush_background_deletes();
}

#[test]
fn hoisted_links_workspaces_and_platform_skips() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = G::new();
    let w = g.workspace("w", "packages/w");
    let lib = g.npm("lib", "1.0.0");
    let other_os = g.npm("other-os", "1.0.0");
    g.g.packages[other_os.index()].platform =
        PlatformConstraints::from_package_json(&["aix"], &["ppc64"], &[] as &[&str]);
    g.dep(w, lib);
    g.edge(
        PackageId::ROOT,
        "other-os",
        other_os,
        Behavior::OPTIONAL,
        "1",
    );
    store.add(lib, "lib", "1.0.0", &[]);
    std::fs::create_dir_all(env.project.join("packages/w")).unwrap();
    let n = |package: PackageId, name: &str| TreeNode {
        package,
        name: name.into(),
        parent: None,
        path: name.into(),
    };
    let tree = Tree {
        nodes: vec![n(lib, "lib"), n(other_os, "other-os"), n(w, "w")],
        workspace_roots: Vec::new(),
    };
    let linker = env.linker();
    let r = linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(tree.clone()),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r.skipped, 1);
    assert_eq!(
        std::fs::read_link(env.nm("w")).unwrap(),
        PathBuf::from("../packages/w")
    );
    assert!(env.nm("lib/package.json").exists());
    // Warm: the link is kept.
    let r2 = linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(tree),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r2.installed, 0);
    // Workspace-local conflicts go into the workspace's node_modules.
    let tree = Tree {
        nodes: vec![n(lib, "lib"), n(w, "w")],
        workspace_roots: vec![(w, vec![2])],
    };
    let mut tree = tree;
    tree.nodes.push(TreeNode {
        package: lib,
        name: "lib2".into(),
        parent: None,
        path: "w/lib2".into(),
    });
    linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(tree),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert!(
        env.project
            .join("packages/w/node_modules/lib2/package.json")
            .exists()
    );
}

#[test]
fn switching_layouts_moves_the_old_tree_aside() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let g = sample(&mut store);
    let linker = env.linker();
    linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(sample_tree()),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    std::fs::create_dir_all(env.nm(".vite")).unwrap();
    std::fs::create_dir_all(env.nm(".fnpm")).unwrap();
    std::fs::write(env.nm(".fnpm/state"), "{}").unwrap();
    assert!(!crate::is_isolated_tree(&env.project));
    let iso = Layout::Isolated(linker.plan_isolated(&g.g, None));
    let r = linker
        .link(&env.project, &g.g, &iso, &store, &lopts(false), &NoopSink)
        .unwrap();
    assert_eq!(r.installed, 4);
    assert_eq!(
        crate::fsutil::kind(&env.nm("a")),
        crate::fsutil::Kind::Symlink
    );
    assert!(env.nm(".vite").exists());
    assert!(crate::is_isolated_tree(&env.project));
    // And back.
    let r = linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(sample_tree()),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r.installed, 4);
    assert_eq!(crate::fsutil::kind(&env.nm("a")), crate::fsutil::Kind::Dir);
    assert!(!env.nm(".fnpm").exists());
    crate::flush_background_deletes();
    let left: Vec<String> = std::fs::read_dir(env.nm(""))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        left.iter().all(|n| !n.starts_with(".old_modules")),
        "{left:?}"
    );
}

#[test]
fn prune_plans_and_executes() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let g = sample(&mut store);
    let linker = env.linker();
    let layout = Layout::Hoisted(sample_tree());
    linker
        .link(
            &env.project,
            &g.g,
            &layout,
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    std::fs::create_dir_all(env.nm("extra")).unwrap();
    std::fs::create_dir_all(env.nm("c/node_modules/zz")).unwrap();
    std::fs::create_dir_all(env.nm(".old_modules-1")).unwrap();
    let p = crate::prune::plan(&env.project, &g.g, &layout);
    let mut names: Vec<_> = p.removals.iter().map(|r| r.name.as_str()).collect();
    names.sort();
    assert_eq!(names, [".old_modules-1", "extra", "zz"]);
    assert_eq!(p.removals.len(), 3);
    let (removed, failed) = crate::prune::execute(&env.project, &p);
    assert_eq!((removed, failed.len()), (3, 0));
    assert!(!env.nm("extra").exists());
    assert_eq!(
        crate::prune::detect_layout(&env.project, &g.g, fnpm_types::NodeLinker::Isolated),
        fnpm_types::NodeLinker::Hoisted
    );

    let env2 = Env::new();
    let iso = Layout::Isolated(linker.plan_isolated(&g.g, None));
    linker
        .link(&env2.project, &g.g, &iso, &store, &lopts(false), &NoopSink)
        .unwrap();
    std::fs::create_dir_all(env2.nm(".fnpm/gone@1.0.0/node_modules/gone")).unwrap();
    std::os::unix::fs::symlink(".fnpm/gone@1.0.0/node_modules/gone", env2.nm("gone")).unwrap();
    let p = crate::prune::plan(&env2.project, &g.g, &iso);
    let names: Vec<_> = p.removals.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"gone@1.0.0") && names.contains(&"gone"),
        "{names:?}"
    );
    assert_eq!(p.removals.len(), 2);
    assert_eq!(
        crate::prune::detect_layout(&env2.project, &g.g, fnpm_types::NodeLinker::Hoisted),
        fnpm_types::NodeLinker::Isolated
    );
    crate::prune::execute(&env2.project, &p);
    assert!(std::fs::symlink_metadata(env2.nm("gone")).is_err());
}

#[test]
fn linker_selection() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    use fnpm_types::{ConfigVersion, NodeLinker};
    assert_eq!(
        crate::choose_linker(Some(NodeLinker::Hoisted), ConfigVersion::V1, true, false, p),
        NodeLinker::Hoisted
    );
    assert_eq!(
        crate::choose_linker(None, ConfigVersion::V1, true, false, p),
        NodeLinker::Isolated
    );
    assert_eq!(
        crate::choose_linker(None, ConfigVersion::V1, true, true, p),
        NodeLinker::Hoisted
    );
    assert_eq!(
        crate::choose_linker(None, ConfigVersion::V0, true, false, p),
        NodeLinker::Hoisted
    );
    assert_eq!(
        crate::choose_linker(None, ConfigVersion::V1, false, false, p),
        NodeLinker::Hoisted
    );
    std::fs::create_dir_all(p.join("node_modules/.fnpm")).unwrap();
    std::fs::write(p.join("node_modules/.fnpm/state"), "").unwrap();
    assert_eq!(
        crate::choose_linker(None, ConfigVersion::V0, false, false, p),
        NodeLinker::Hoisted
    );
    std::fs::create_dir_all(p.join("node_modules/.fnpm/a@1.0.0")).unwrap();
    assert_eq!(
        crate::choose_linker(None, ConfigVersion::V0, false, false, p),
        NodeLinker::Isolated
    );
    assert_eq!(crate::parse_linker("isolated"), Some(NodeLinker::Isolated));
    assert_eq!(crate::parse_linker("pnpm"), None);
    assert_eq!(
        crate::parse_backend("clonefile_each_dir"),
        Some(fnpm_config::Backend::Clonefile)
    );
    assert_eq!(
        crate::parse_backend("hardlink"),
        Some(fnpm_config::Backend::Hardlink)
    );
    assert_eq!(crate::parse_backend("nope"), None);
}

#[test]
fn include_follows_config() {
    let env = Env::new();
    let mut cfg = test_config(&env.project, &env.store_dir);
    cfg.production = true;
    cfg.omit.optional = true;
    let l = Linker::new(Arc::new(cfg), Store::open(&env.store_dir).unwrap());
    let b = l.include();
    assert!(!b.intersects(Behavior::DEV) && !b.intersects(Behavior::OPTIONAL));
    assert!(b.intersects(Behavior::PROD) && b.intersects(Behavior::PEER));
}

#[test]
fn native_binlink_redirect_in_hoisted_install() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = G::new();
    let esb = g.npm("esbuild", "0.21.5");
    let native = g.npm("@esbuild/native", "0.21.5");
    let p = fnpm_types::Platform::current();
    g.g.packages[native.index()].platform = PlatformConstraints {
        os: p.os,
        cpu: p.cpu,
        libc: fnpm_types::LibcSet::NONE,
    };
    g.dep(PackageId::ROOT, esb);
    g.edge(esb, "@esbuild/native", native, Behavior::OPTIONAL, "0.21.5");
    g.bin(
        esb,
        Bin::NamedFile {
            name: "esbuild".into(),
            path: "bin/esbuild".into(),
        },
    );
    store.add(esb, "esbuild", "0.21.5", &[("bin/esbuild", "js")]);
    store.add(
        native,
        "@esbuild/native",
        "0.21.5",
        &[("bin/esbuild", "ELF")],
    );
    let n = |package: PackageId, name: &str| TreeNode {
        package,
        name: name.into(),
        parent: None,
        path: name.into(),
    };
    let tree = Tree {
        nodes: vec![n(native, "@esbuild/native"), n(esb, "esbuild")],
        workspace_roots: Vec::new(),
    };
    let linker = env.linker();
    linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(tree.clone()),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(
        std::fs::read_link(env.nm(".bin/esbuild")).unwrap(),
        PathBuf::from("../@esbuild/native/bin/esbuild")
    );
    // Turned off: the package's own bin.
    let env2 = Env::new();
    let linker = env2.linker().with_native_dependencies(Some(Vec::new()));
    linker
        .link(
            &env2.project,
            &g.g,
            &Layout::Hoisted(tree),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(
        std::fs::read_link(env2.nm(".bin/esbuild")).unwrap(),
        PathBuf::from("../esbuild/bin/esbuild")
    );
}

#[test]
fn copy_backend_and_tags() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = G::new();
    let git = g.with(
        "gitpkg",
        "1.0.0",
        Resolution::Git {
            url: "https://h/r.git".into(),
            committish: String::new(),
            resolved_commit: "abc".into(),
            integrity: None,
        },
    );
    g.dep(PackageId::ROOT, git);
    store.add(git, "gitpkg", "1.0.0", &[]);
    let linker = env.linker();
    let tree = Tree {
        nodes: vec![TreeNode {
            package: git,
            name: "gitpkg".into(),
            parent: None,
            path: "gitpkg".into(),
        }],
        workspace_roots: Vec::new(),
    };
    let mut o = lopts(false);
    o.backend = fnpm_config::Backend::Copyfile;
    let r = linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(tree.clone()),
            &store,
            &o,
            &NoopSink,
        )
        .unwrap();
    assert_eq!(r.backend_used, Some(fnpm_config::Backend::Copyfile));
    assert_eq!(read(&env.nm("gitpkg/.fnpm-tag")), "abc");
    assert_eq!(
        linker
            .link(
                &env.project,
                &g.g,
                &Layout::Hoisted(tree.clone()),
                &store,
                &o,
                &NoopSink
            )
            .unwrap()
            .installed,
        0
    );
    std::fs::remove_file(env.nm("gitpkg/.fnpm-tag")).unwrap();
    std::fs::write(env.nm("gitpkg/.fnpm-tag"), "old").unwrap();
    assert_eq!(
        linker
            .link(
                &env.project,
                &g.g,
                &Layout::Hoisted(tree),
                &store,
                &o,
                &NoopSink
            )
            .unwrap()
            .installed,
        1
    );
    crate::flush_background_deletes();
}

#[test]
fn planted_links_are_not_written_through() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = G::new();
    let s1 = g.npm("@s/one", "1.0.0");
    g.dep(PackageId::ROOT, s1);
    g.bin(
        s1,
        Bin::File {
            path: "x.js".into(),
        },
    );
    store.add(s1, "@s/one", "1.0.0", &[("x.js", "")]);
    let outside = env.project.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(env.nm("")).unwrap();
    std::fs::write(env.nm("keep-me"), "").unwrap();
    std::os::unix::fs::symlink(&outside, env.nm("@s")).unwrap();
    std::os::unix::fs::symlink(&outside, env.nm(".bin")).unwrap();
    let tree = Tree {
        nodes: vec![TreeNode {
            package: s1,
            name: "@s/one".into(),
            parent: None,
            path: "@s/one".into(),
        }],
        workspace_roots: Vec::new(),
    };
    let linker = env.linker();
    linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(tree),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "nothing written outside"
    );
    assert_eq!(crate::fsutil::kind(&env.nm("@s")), crate::fsutil::Kind::Dir);
    assert!(env.nm("@s/one/package.json").exists());
    assert_eq!(
        crate::fsutil::kind(&env.nm(".bin")),
        crate::fsutil::Kind::Dir
    );
    assert!(env.nm(".bin/one").exists());
}

/// Timing smoke test: `cargo test --release -p fnpm-linker perf_smoke -- --ignored --nocapture`.
#[test]
#[ignore]
fn perf_smoke() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = G::new();
    let n = 1000;
    let mut ids = Vec::new();
    for i in 0..n {
        let id = g.npm(&format!("p{i}"), "1.0.0");
        let files: Vec<(String, String)> = (0..10)
            .map(|f| (format!("lib/f{f}.js"), "x".repeat(200)))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        store.add(id, &format!("p{i}"), "1.0.0", &refs);
        ids.push(id);
    }
    for (i, &id) in ids.iter().enumerate() {
        if i < 100 {
            g.dep(PackageId::ROOT, id);
        }
        for k in 1..4 {
            let j = (i * 7 + k * 13) % n;
            if j > i {
                g.dep(id, ids[j]);
            }
        }
    }
    let linker = env.linker();
    let t = std::time::Instant::now();
    let plan = linker.plan_isolated(&g.g, None);
    eprintln!(
        "plan isolated: {:?} ({} entries)",
        t.elapsed(),
        plan.entries.len()
    );
    let layout = Layout::Isolated(plan);
    for (label, global) in [("isolated local", false), ("isolated global", true)] {
        let p = env.project.parent().unwrap().join(label.replace(' ', "-"));
        std::fs::create_dir_all(&p).unwrap();
        let t = std::time::Instant::now();
        let r = linker
            .link(&p, &g.g, &layout, &store, &lopts(global), &NoopSink)
            .unwrap();
        eprintln!("{label} cold: {:?} installed {}", t.elapsed(), r.installed);
        let t = std::time::Instant::now();
        linker
            .link(&p, &g.g, &layout, &store, &lopts(global), &NoopSink)
            .unwrap();
        eprintln!("{label} warm: {:?}", t.elapsed());
        if global {
            let p2 = p.with_file_name("isolated-global-2");
            std::fs::create_dir_all(&p2).unwrap();
            let t = std::time::Instant::now();
            linker
                .link(&p2, &g.g, &layout, &store, &lopts(true), &NoopSink)
                .unwrap();
            eprintln!("{label} new project, warm store: {:?}", t.elapsed());
        }
    }
    let tree = Tree {
        nodes: ids
            .iter()
            .enumerate()
            .map(|(i, &id)| TreeNode {
                package: id,
                name: format!("p{i}"),
                parent: None,
                path: format!("p{i}"),
            })
            .collect(),
        workspace_roots: Vec::new(),
    };
    let layout = Layout::Hoisted(tree);
    let p = env.project.parent().unwrap().join("hoisted");
    std::fs::create_dir_all(&p).unwrap();
    let t = std::time::Instant::now();
    linker
        .link(&p, &g.g, &layout, &store, &lopts(false), &NoopSink)
        .unwrap();
    eprintln!("hoisted cold: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    linker
        .link(&p, &g.g, &layout, &store, &lopts(false), &NoopSink)
        .unwrap();
    eprintln!("hoisted warm: {:?}", t.elapsed());
    crate::flush_background_deletes();
}

// ---- Regression tests from the review pass. ----

fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn bin_target_that_is_a_symlink_is_never_chmodded_through() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let secret = t.path().join("secret");
    std::fs::write(&secret, "key").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let pkg = t.path().join("node_modules/evil");
    std::fs::create_dir_all(&pkg).unwrap();
    std::os::unix::fs::symlink(&secret, pkg.join("cli")).unwrap();
    let bin_dir = t.path().join("node_modules/.bin");
    crate::bins::link_bins("evil", &Bin::File { path: "cli".into() }, &pkg, &bin_dir).unwrap();
    assert_eq!(mode_of(&secret), 0o600);
    // A real file still gets the exec bits.
    std::fs::write(pkg.join("real"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(pkg.join("real"), std::fs::Permissions::from_mode(0o644)).unwrap();
    crate::bins::link_bins(
        "evil",
        &Bin::NamedFile {
            name: "real".into(),
            path: "real".into(),
        },
        &pkg,
        &bin_dir,
    )
    .unwrap();
    assert_eq!(mode_of(&pkg.join("real")), crate::bins::exec_mode());
}

#[cfg(target_os = "linux")]
#[test]
fn umask_is_read_without_changing_it() {
    let m = crate::bins::exec_mode();
    assert_eq!(m & !0o777, 0);
    // Owner bits are never masked by any sane umask in CI.
    assert_eq!(m & 0o700, 0o700);
}

#[test]
fn moving_aside_skips_symlinked_node_modules() {
    let t = tempfile::tempdir().unwrap();
    let shared = t.path().join("shared");
    std::fs::create_dir_all(shared.join("pkg")).unwrap();
    let project = t.path().join("p");
    std::fs::create_dir_all(&project).unwrap();
    std::os::unix::fs::symlink(&shared, project.join("node_modules")).unwrap();
    assert_eq!(
        crate::move_node_modules_aside(&project.join("node_modules"), &[]),
        0
    );
    assert!(shared.join("pkg").is_dir());
}

#[test]
fn moving_aside_moves_everything_and_keeps_other_tools() {
    let t = tempfile::tempdir().unwrap();
    let nm = t.path().join("node_modules");
    for d in ["a", "@s/b", ".bin", ".vite", ".old_modules-dead"] {
        std::fs::create_dir_all(nm.join(d)).unwrap();
    }
    assert_eq!(crate::move_node_modules_aside(&nm, &[]), 3);
    crate::flush_background_deletes();
    let mut left: Vec<String> = std::fs::read_dir(&nm)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [".vite"]);
}

#[test]
fn switching_layouts_leaves_workspaces_outside_the_project_alone() {
    let env = Env::new();
    let outside = env.project.parent().unwrap().join("outside");
    // `.bin` is a linker entry: switching layouts would move it aside in a member inside the
    // project, but a member outside of it is never touched.
    std::fs::create_dir_all(outside.join("node_modules/.bin")).unwrap();
    std::fs::write(outside.join("node_modules/.bin/keep-me"), "x").unwrap();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = sample(&mut store);
    g.workspace("outside", "../outside");
    let linker = env.linker();
    linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(sample_tree()),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    let iso = Layout::Isolated(linker.plan_isolated(&g.g, None));
    linker
        .link(&env.project, &g.g, &iso, &store, &lopts(false), &NoopSink)
        .unwrap();
    crate::flush_background_deletes();
    assert!(outside.join("node_modules/.bin/keep-me").is_file());
}

#[test]
fn npm_version_cannot_turn_a_store_key_into_a_path() {
    let key = crate::store_key::store_key("a", &Resolution::npm("1.0.0/../../../evil", None), "");
    assert!(!key.contains('/') && !key.contains('\\'), "{key}");
    assert_eq!(key, "a@1.0.0+..+..+..+evil");
}

fn index_of(files: &[&str]) -> FileIndex {
    FileIndex {
        dirs: Vec::new(),
        files: files
            .iter()
            .map(|p| IndexedFile {
                path: (*p).to_owned(),
                size: 0,
                mode: 0o644,
                digest: [0; 64],
            })
            .collect(),
        package_json: None,
    }
}

#[test]
fn package_json_is_placed_last_so_a_failed_copy_is_not_trusted() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("package.json"),
        r#"{"name":"a","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(src.join("index.js"), "1").unwrap();
    let m = crate::fsutil::Materializer::new(fnpm_config::Backend::Hardlink);
    let dest = t.path().join("dest");
    // `zz-missing.js` sorts after `package.json`: it still fails before the manifest lands.
    let err = m
        .materialize(
            &src,
            &index_of(&["index.js", "package.json", "zz-missing.js"]),
            &dest,
        )
        .unwrap_err();
    assert!(err.to_string().contains("zz-missing.js"), "{err}");
    assert!(dest.join("index.js").exists());
    assert!(!dest.join("package.json").exists());
    assert!(!crate::verify::verify_package_json(&dest, "a", "1.0.0"));
    // A complete copy has it.
    let dest2 = t.path().join("dest2");
    m.materialize(&src, &index_of(&["index.js", "package.json"]), &dest2)
        .unwrap();
    assert!(crate::verify::verify_package_json(&dest2, "a", "1.0.0"));
}

#[test]
fn store_index_paths_cannot_escape_the_package() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("a/b/src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(t.path().join("a/b/x"), "x").unwrap();
    let m = crate::fsutil::Materializer::new(fnpm_config::Backend::Copyfile);
    for bad in ["../x", "lib/../../x", "/etc/passwd", ""] {
        let dest = t.path().join("dest");
        let r = m.materialize(&src, &index_of(&[bad]), &dest);
        assert!(r.is_err(), "{bad:?} was accepted");
    }
    let mut idx = index_of(&[]);
    idx.dirs.push("../../evil".into());
    assert!(m.materialize(&src, &idx, &t.path().join("d3")).is_err());
    assert!(!t.path().join("evil").exists());
}

#[test]
fn link_name_resolves_in_the_global_link_dir() {
    let env = Env::new();
    let mut g = G::new();
    let lib = g.with(
        "mylib",
        "1.0.0",
        Resolution::Symlink {
            path: "mylib".into(),
        },
    );
    let rel = g.with(
        "rel",
        "1.0.0",
        Resolution::Symlink {
            path: "../rel".into(),
        },
    );
    g.dep(PackageId::ROOT, lib);
    g.dep(PackageId::ROOT, rel);
    let registered = env.store_dir.join("global/links/mylib");
    std::fs::create_dir_all(&registered).unwrap();
    let rel_dir = env.project.parent().unwrap().join("rel");
    std::fs::create_dir_all(&rel_dir).unwrap();
    let store = FakeStore::new(&env.store_dir);
    let linker = env.linker();
    let tree = Tree {
        nodes: vec![
            TreeNode {
                package: lib,
                name: "mylib".into(),
                parent: None,
                path: "mylib".into(),
            },
            TreeNode {
                package: rel,
                name: "rel".into(),
                parent: None,
                path: "rel".into(),
            },
        ],
        workspace_roots: Vec::new(),
    };
    linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(tree),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert!(crate::verify::same_file(&env.nm("mylib"), &registered));
    assert!(crate::verify::same_file(&env.nm("rel"), &rel_dir));
    // Isolated: the same.
    let iso = Layout::Isolated(linker.plan_isolated(&g.g, None));
    linker
        .link(&env.project, &g.g, &iso, &store, &lopts(false), &NoopSink)
        .unwrap();
    assert!(crate::verify::same_file(&env.nm("mylib"), &registered));
    assert!(crate::verify::same_file(&env.nm("rel"), &rel_dir));
}

#[test]
fn background_deletes_all_finish() {
    let t = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for i in 0..200 {
        let p = t.path().join(format!("d{i}"));
        std::fs::create_dir_all(p.join("x")).unwrap();
        paths.push(p);
    }
    std::thread::scope(|s| {
        for chunk in paths.chunks(50) {
            s.spawn(move || {
                for p in chunk {
                    crate::fsutil::delete_in_background(p.clone());
                }
            });
        }
    });
    crate::flush_background_deletes();
    assert!(paths.iter().all(|p| !p.exists()));
}

#[test]
fn out_of_range_peer_targets_do_not_panic() {
    let mut b = G::new();
    let a = b.npm("a", "1.0.0");
    b.dep(PackageId::ROOT, a);
    b.edge(a, "ghost", PackageId(999), Behavior::PEER, "*");
    b.edge(a, "ghost2", PackageId(998), Behavior::PROD, "*");
    let l = plan_default(&b.g);
    assert_eq!(entry_of(&l, &b.g, "a").len(), 1);
}

#[test]
fn shipped_tag_file_is_replaced_not_written_through() {
    let env = Env::new();
    let mut store = FakeStore::new(&env.store_dir);
    let mut g = G::new();
    let i = Integrity::compute(HashAlgorithm::Sha512, b"t");
    let t = g.with(
        "t",
        "1.0.0",
        Resolution::RemoteTarball {
            url: "https://h/t.tgz".into(),
            integrity: Some(i),
        },
    );
    g.dep(PackageId::ROOT, t);
    store.add(t, "t", "1.0.0", &[(".fnpm-tag", "shared blob")]);
    let blob = store.entries[&t.0].root.join(".fnpm-tag");
    let linker = env.linker();
    let tree = Tree {
        nodes: vec![TreeNode {
            package: t,
            name: "t".into(),
            parent: None,
            path: "t".into(),
        }],
        workspace_roots: Vec::new(),
    };
    linker
        .link(
            &env.project,
            &g.g,
            &Layout::Hoisted(tree),
            &store,
            &lopts(false),
            &NoopSink,
        )
        .unwrap();
    assert_eq!(read(&blob), "shared blob");
    assert_eq!(read(&env.nm("t/.fnpm-tag")), i.to_sri());
}

#[test]
fn concurrent_shebang_normalisation_of_one_target_is_safe() {
    let t = tempfile::tempdir().unwrap();
    let pkg = t.path().join("node_modules/a");
    std::fs::create_dir_all(&pkg).unwrap();
    for round in 0..20 {
        let body = format!("#!/usr/bin/env node\r\nconsole.log({round})\r\n");
        std::fs::write(pkg.join("cli.js"), &body).unwrap();
        std::thread::scope(|s| {
            for k in 0..8 {
                let pkg = &pkg;
                let bin_dir = t.path().join(format!("bins{k}"));
                s.spawn(move || {
                    crate::bins::link_bins(
                        "a",
                        &Bin::File {
                            path: "cli.js".into(),
                        },
                        pkg,
                        &bin_dir,
                    )
                    .unwrap();
                });
            }
        });
        assert_eq!(
            read(&pkg.join("cli.js")),
            format!("#!/usr/bin/env node\nconsole.log({round})\r\n")
        );
    }
}
