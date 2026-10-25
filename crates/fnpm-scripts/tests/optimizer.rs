//! Postinstall optimizer (bun `postinstall_optimizer.rs`, bun-install-native-binlink.test.ts,
//! "scripts that bun skips by itself").

mod common;

use common::*;
use fnpm_config::Env;
use fnpm_scripts::optimizer::{Decision, Optimizer};
use fnpm_types::{
    Behavior, CpuSet, LibcSet, OsSet, Package, PackageId, Platform, PlatformConstraints, Resolution,
};
use serde_json::json;

fn linux_x64() -> Platform {
    Platform {
        os: OsSet::from_list(&["linux"]),
        cpu: CpuSet::from_list(&["x64"]),
        libc: LibcSet::from_package_list(&["glibc"]),
    }
}

fn platform_pkg(name: &str, os: &str, cpu: &str) -> Package {
    let mut p = npm(name, "0.21.5");
    p.platform = PlatformConstraints::from_package_json(&[os], &[cpu], &[] as &[&str]);
    p
}

/// esbuild with optional platform packages for linux-x64 and darwin-arm64.
fn esbuild_graph(name: &str) -> (fnpm_types::LockGraph, PackageId, PackageId) {
    let mut g = root("app");
    let esbuild = add(&mut g, with_scripts(npm(name, "0.21.5")));
    let darwin = add(
        &mut g,
        platform_pkg("@esbuild/darwin-arm64", "darwin", "arm64"),
    );
    let linux = add(&mut g, platform_pkg("@esbuild/linux-x64", "linux", "x64"));
    let any = add(&mut g, npm("plain-dep", "1.0.0"));
    edge(&mut g, PackageId::ROOT, name, esbuild);
    edge(&mut g, esbuild, "plain-dep", any);
    edge_with(
        &mut g,
        esbuild,
        "@esbuild/darwin-arm64",
        darwin,
        Behavior::OPTIONAL,
    );
    edge_with(
        &mut g,
        esbuild,
        "@esbuild/linux-x64",
        linux,
        Behavior::OPTIONAL,
    );
    (g, esbuild, linux)
}

#[test]
fn defaults() {
    let o = Optimizer::from_root(&json!({}), &Env::default());
    assert_eq!(
        o.native_dependencies,
        ["esbuild", "@anthropic-ai/claude-code"]
    );
    assert_eq!(o, Optimizer::default());
}

#[test]
fn esbuild_native_binlink() {
    let (g, esbuild, linux) = esbuild_graph("esbuild");
    let o = Optimizer::default();
    assert_eq!(
        o.decide(&g, esbuild, &linux_x64()),
        Decision::NativeBinlink {
            platform_package: linux
        }
    );
    assert_eq!(
        o.native_binlink_target(&g, esbuild, &linux_x64()),
        Some(linux)
    );
    // No platform package for the target: old esbuild versions still need their postinstall.
    let windows = Platform {
        os: OsSet::from_list(&["win32"]),
        cpu: CpuSet::from_list(&["x64"]),
        libc: LibcSet::NONE,
    };
    assert_eq!(o.decide(&g, esbuild, &windows), Decision::Run);
    assert_eq!(o.native_binlink_target(&g, esbuild, &windows), None);
}

#[test]
fn libc_is_checked_for_platform_packages() {
    let (mut g, esbuild, linux) = esbuild_graph("esbuild");
    g.packages[linux.index()].platform.libc = LibcSet::from_package_list(&["musl"]);
    assert_eq!(
        Optimizer::default().decide(&g, esbuild, &linux_x64()),
        Decision::Run
    );
}

#[test]
fn native_dependencies_list_replaces_default() {
    let (g, esbuild, _) = esbuild_graph("esbuild");
    let o = Optimizer::from_root(&json!({"nativeDependencies": []}), &Env::default());
    assert_eq!(o.decide(&g, esbuild, &linux_x64()), Decision::Run);
    let (g2, custom, linux2) = esbuild_graph("my-native");
    let o = Optimizer::from_root(
        &json!({"nativeDependencies": ["my-native", 1, ""]}),
        &Env::default(),
    );
    assert_eq!(o.native_dependencies, ["my-native"]);
    assert_eq!(
        o.decide(&g2, custom, &linux_x64()),
        Decision::NativeBinlink {
            platform_package: linux2
        }
    );
    assert_eq!(o.decide(&g, esbuild, &linux_x64()), Decision::Run);
    // A non-array keeps the default.
    let o = Optimizer::from_root(&json!({"nativeDependencies": "esbuild"}), &Env::default());
    assert_eq!(
        o.native_dependencies,
        Optimizer::default().native_dependencies
    );
}

#[test]
fn sharp_is_ignored_from_0_33() {
    let o = Optimizer::default();
    let mut g = root("app");
    let new = add(&mut g, with_scripts(npm("sharp", "0.33.5")));
    let exact = add(&mut g, with_scripts(npm("sharp", "0.33.0")));
    let old = add(&mut g, with_scripts(npm("sharp", "0.32.6")));
    let pre = add(&mut g, with_scripts(npm("sharp", "0.33.0-rc.2")));
    let fork = add(
        &mut g,
        Package::new(
            "sharp",
            "0.34.0",
            Resolution::Git {
                url: "u".into(),
                committish: "".into(),
                resolved_commit: "c".into(),
                integrity: None,
            },
        ),
    );
    assert_eq!(o.decide(&g, new, &linux_x64()), Decision::Skip);
    assert_eq!(o.decide(&g, exact, &linux_x64()), Decision::Skip);
    assert_eq!(o.decide(&g, old, &linux_x64()), Decision::Run);
    assert_eq!(o.decide(&g, pre, &linux_x64()), Decision::Run);
    assert_eq!(
        o.decide(&g, fork, &linux_x64()),
        Decision::Run,
        "a git fork runs its own scripts"
    );
}

#[test]
fn ignore_scripts_list_replaces_default() {
    let mut g = root("app");
    let sharp = add(&mut g, with_scripts(npm("sharp", "0.33.5")));
    let mine = add(&mut g, with_scripts(npm("mine", "1.0.0")));
    let o = Optimizer::from_root(&json!({"ignoreScripts": ["mine"]}), &Env::default());
    assert_eq!(o.decide(&g, sharp, &linux_x64()), Decision::Run);
    assert_eq!(o.decide(&g, mine, &linux_x64()), Decision::Skip);
    let o = Optimizer::from_root(&json!({"ignoreScripts": []}), &Env::default());
    assert_eq!(o.decide(&g, sharp, &linux_x64()), Decision::Run);
}

#[test]
fn feature_flags_disable_each_half() {
    let (g, esbuild, _) = esbuild_graph("esbuild");
    let mut g2 = root("app");
    let sharp = add(&mut g2, with_scripts(npm("sharp", "0.33.5")));
    for key in [
        "BUN_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER",
        "FNPM_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER",
    ] {
        let o = Optimizer::from_root(&json!({}), &Env::from_pairs([(key, "1")]));
        assert_eq!(o.decide(&g, esbuild, &linux_x64()), Decision::Run);
        assert_eq!(o.decide(&g2, sharp, &linux_x64()), Decision::Skip);
    }
    for key in [
        "BUN_FEATURE_FLAG_DISABLE_IGNORE_SCRIPTS",
        "FNPM_FEATURE_FLAG_DISABLE_IGNORE_SCRIPTS",
    ] {
        let o = Optimizer::from_root(&json!({}), &Env::from_pairs([(key, "true")]));
        assert_eq!(o.decide(&g, esbuild, &linux_x64()), Decision::Run);
        assert_eq!(o.decide(&g2, sharp, &linux_x64()), Decision::Run);
    }
    let o = Optimizer::from_root(
        &json!({}),
        &Env::from_pairs([("BUN_FEATURE_FLAG_DISABLE_IGNORE_SCRIPTS", "0")]),
    );
    assert_eq!(o.decide(&g2, sharp, &linux_x64()), Decision::Skip);
}
