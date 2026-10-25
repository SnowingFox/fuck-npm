//! Dispatch decisions (`docs/ARCHITECTURE.md#fnpm-cli`, "Dispatch"): what runs natively,
//! what prints the version, and what is handed to the TS CLI.

use std::ffi::OsString;

use fnpm_cli::plan::Route;
use fnpm_cli::{Bin, route_args};

fn route(bin: Bin, args: &[&str]) -> Route {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    route_args(bin, &args)
}

fn native_leaf(args: &[&str]) -> Option<&'static str> {
    match route(Bin::Fnpm, args) {
        Route::Native(invocation) => Some(invocation.leaf.name),
        _ => None,
    }
}

fn is_node(bin: Bin, args: &[&str]) -> bool {
    matches!(route(bin, args), Route::Node)
}

fn is_version(bin: Bin, args: &[&str]) -> bool {
    matches!(route(bin, args), Route::Version)
}

#[test]
fn every_spelling_of_the_hot_commands_is_native() {
    for (spelling, leaf) in [
        ("install", "install"),
        ("i", "install"),
        ("ci", "ci"),
        ("add", "add"),
        ("a", "add"),
        ("remove", "remove"),
        ("rm", "remove"),
        ("r", "remove"),
        ("uninstall", "remove"),
        ("un", "remove"),
        ("update", "update"),
        ("up", "update"),
        ("upgrade", "update"),
    ] {
        assert_eq!(native_leaf(&[spelling]), Some(leaf), "{spelling}");
        assert_eq!(
            native_leaf(&[spelling, "pkg", "--json"]),
            Some(leaf),
            "{spelling} pkg --json"
        );
    }
}

#[test]
fn flags_before_the_command_are_stepped_over() {
    assert_eq!(native_leaf(&["--cwd", "app", "install"]), Some("install"));
    assert_eq!(native_leaf(&["--cwd=app", "i"]), Some("install"));
    assert_eq!(native_leaf(&["--json", "add", "x"]), Some("add"));
    assert_eq!(native_leaf(&["-F", "pkg", "update"]), Some("update"));
    // `-c` takes its value only with `=`, so `install` is the command here.
    assert_eq!(native_leaf(&["-c", "install"]), Some("install"));
    // `--registry` takes the next token, so there is no command: root help (Node).
    assert!(is_node(Bin::Fnpm, &["--registry", "install"]));
}

#[test]
fn invalid_flags_on_native_commands_stay_native() {
    // The native side prints the TS CLI's usage error itself.
    assert_eq!(native_leaf(&["install", "--bogus"]), Some("install"));
    assert_eq!(native_leaf(&["add"]), Some("add"));
    assert_eq!(native_leaf(&["update", "--depth", "3"]), Some("update"));
}

#[test]
fn interactive_update_is_decided_after_parsing() {
    // `update -i` is planned natively, then handed to Node once the flags are known to be
    // valid (`fnpm_cli::install::Outcome::Node`).
    assert_eq!(native_leaf(&["update", "-i"]), Some("update"));
    assert_eq!(
        native_leaf(&["up", "--interactive", "--latest"]),
        Some("update")
    );
}

#[test]
fn help_tree_and_other_commands_go_to_node() {
    for args in [
        &[][..],
        &["--help"],
        &["-h"],
        &["help"],
        &["help", "install"],
        &["install", "--help"],
        &["add", "x", "-h"],
        &["ci", "-t"],
        &["--tree"],
        &["install", "--tree"],
        &["outdated"],
        &["pm", "cache", "rm"],
        &["pm", "--version"],
        &["run", "build", "--", "install"],
        &["why", "react"],
        &["whoami"],
        &["migrate"],
        &["completions", "zsh"],
        &["frobnicate"],
        &["--json"],
        &["--", "install"],
        &["--cwd", "app", "outdated"],
        &["-p", "outdated"],
    ] {
        assert!(is_node(Bin::Fnpm, args), "{args:?}");
    }
}

#[test]
fn help_beats_version_on_a_command() {
    assert!(is_node(Bin::Fnpm, &["install", "--version", "--help"]));
    assert!(is_node(Bin::Fnpm, &["install", "-v", "--tree"]));
}

#[test]
fn version_is_native() {
    for args in [
        &["--version"][..],
        &["-v"],
        &["--version", "--json"],
        &["--help", "--version"],
        &["install", "-v"],
        &["add", "x", "--version"],
        // An unknown word at the root does not stop `--version` (the root is still the node).
        &["frobnicate", "--version"],
    ] {
        assert!(is_version(Bin::Fnpm, args), "{args:?}");
    }
    // After `--`, `--version` belongs to someone else.
    assert!(is_node(Bin::Fnpm, &["--", "--version"]));
}

#[test]
fn foreign_version_flags_are_translated_first() {
    // `--loglevel` swallows its value, so `-v` is still seen; `--no-version` is not `-v`.
    assert!(is_version(
        Bin::Fnpm,
        &["install", "--loglevel", "info", "-v"]
    ));
    assert_eq!(native_leaf(&["install", "--version=1"]), Some("install"));
}

#[test]
fn fnpm_lock_only_answers_version() {
    assert!(is_version(Bin::FnpmLock, &["--version"]));
    assert!(is_version(Bin::FnpmLock, &["-v"]));
    for args in [
        &[][..],
        &["migrate"],
        &["install"],
        &["export", "--version"],
        &["--help"],
        &["i"],
    ] {
        assert!(is_node(Bin::FnpmLock, args), "{args:?}");
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_go_to_node() {
    use std::os::unix::ffi::OsStringExt;
    let args = vec![
        OsString::from("install"),
        OsString::from_vec(vec![0x66, 0xff]),
    ];
    assert!(matches!(route_args(Bin::Fnpm, &args), Route::Node));
}

#[test]
fn passthrough_after_double_dash() {
    let Route::Native(invocation) = route(Bin::Fnpm, &["add", "--", "--weird", "pkg"]) else {
        panic!("not native");
    };
    assert_eq!(invocation.forwarded, ["--weird", "pkg"]);
    assert!(invocation.own.is_empty());
}

#[test]
fn foreign_flags_are_translated_in_the_plan() {
    let Route::Native(invocation) = route(
        Bin::Fnpm,
        &["install", "--save-dev", "x", "--immutable", "--bun"],
    ) else {
        panic!("not native");
    };
    assert_eq!(invocation.own, ["--dev", "x", "--frozen-lockfile"]);
    let Route::Native(invocation) = route(Bin::Fnpm, &["update", "--save-dev", "--production"])
    else {
        panic!("not native");
    };
    // On update, --save-dev is dropped and --production keeps selecting update groups.
    assert_eq!(invocation.own, ["--production"]);
}

#[test]
fn package_execution_keeps_arguments_raw() {
    let args = ["--cwd", "app", "x", "pkg", "--", "--help"].map(str::to_owned);
    match fnpm_cli::plan::route(Bin::Fnpm.def(), &args) {
        fnpm_cli::plan::Route::Exec { create, args } => {
            assert!(!create);
            assert_eq!(args, ["--cwd", "app", "pkg", "--", "--help"]);
        }
        route => panic!("unexpected route: {route:?}"),
    }
}
