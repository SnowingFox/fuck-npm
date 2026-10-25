//! Regression tests for issues found in review: panics on malformed `.npmrc` keys, proxy
//! variables read like bun, `ignore-scripts` from the environment winning over flags,
//! `--production` freezing the lockfile, and secrets kept out of `Debug`.

mod common;

use common::*;
use fnpm_config::{Auth, CliOverrides, Config, auth::normalize_key};

#[test]
fn normalize_key_handles_multibyte_text_where_a_scheme_would_end() {
    // Byte 7 / 8 of these keys falls inside a character.
    assert_eq!(normalize_key("aaaaaaé/"), "aaaaaaé/");
    assert_eq!(normalize_key("aaaaaaaé/x"), "aaaaaaaé/x");
    assert_eq!(normalize_key("ÉÉÉÉ/"), "ÉÉÉÉ/");
    assert_eq!(normalize_key("HTTPS://Host:443/p/"), "host/p/");
    assert_eq!(normalize_key("http://h:80/"), "h/");
}

#[test]
fn npmrc_line_with_multibyte_host_does_not_panic() {
    let fx = Fx::new(&[(
        "project/.npmrc",
        "//aaaaaaé/:_authToken=x\n//aaaaaaaü/:_authToken=y\n//€€€/:_auth=\n",
    )]);
    let cfg = fx.load();
    assert!(
        cfg.authorization_header("https://registry.npmjs.org/x")
            .is_none()
    );
}

#[test]
fn proxy_variables_follow_bun() {
    let fx = Fx::new(&[]);
    let proxy = |env: &[(&str, &str)]| fx.load_with(env, CliOverrides::default()).network.proxy;

    // The http variables never serve https requests.
    let p = proxy(&[("HTTP_PROXY", "http://only:3")]);
    assert_eq!(p.http.as_deref(), Some("http://only:3"));
    assert_eq!(p.https, None);

    // Lowercase before uppercase; an empty lowercase value falls through.
    let p = proxy(&[
        ("https_proxy", "http://lower:1"),
        ("HTTPS_PROXY", "http://upper:1"),
        ("http_proxy", ""),
        ("HTTP_PROXY", "http://upper:2"),
    ]);
    assert_eq!(p.https.as_deref(), Some("http://lower:1"));
    assert_eq!(p.http.as_deref(), Some("http://upper:2"));

    // `""` and `''` count as unset.
    let p = proxy(&[("http_proxy", "\"\""), ("HTTPS_PROXY", "''")]);
    assert_eq!((p.http, p.https), (None, None));

    // ALL_PROXY fills both, but only when it is an HTTP proxy.
    let p = proxy(&[("ALL_PROXY", "http://all:9")]);
    assert_eq!(p.http.as_deref(), Some("http://all:9"));
    assert_eq!(p.https.as_deref(), Some("http://all:9"));
    let p = proxy(&[("all_proxy", "proxy.corp:3128")]);
    assert_eq!(p.https.as_deref(), Some("proxy.corp:3128"));
    let p = proxy(&[("ALL_PROXY", "socks5://s:1080")]);
    assert_eq!((p.http, p.https), (None, None));
    let p = proxy(&[("ALL_PROXY", "http://all:9"), ("HTTPS_PROXY", "http://s:1")]);
    assert_eq!(p.https.as_deref(), Some("http://s:1"));
    assert_eq!(p.http.as_deref(), Some("http://all:9"));

    // no_proxy before NO_PROXY.
    let p = proxy(&[("no_proxy", "a.com"), ("NO_PROXY", "b.com")]);
    assert_eq!(p.no_proxy, ["a.com"]);
}

#[test]
fn npmrc_proxy_also_serves_https_like_npm() {
    let fx = Fx::new(&[("project/.npmrc", "proxy=http://rc:1/\n")]);
    let p = fx.load().network.proxy;
    assert_eq!(p.http.as_deref(), Some("http://rc:1/"));
    assert_eq!(p.https.as_deref(), Some("http://rc:1/"));
    // An https variable beats `proxy` for https, as in npm.
    let p = fx
        .load_with(&[("HTTPS_PROXY", "http://s:1")], CliOverrides::default())
        .network
        .proxy;
    assert_eq!(p.https.as_deref(), Some("http://s:1"));
    assert_eq!(p.http.as_deref(), Some("http://rc:1/"));
    // `proxy=false` turns off http only.
    let fx = Fx::new(&[("project/.npmrc", "proxy=false\n")]);
    let p = fx
        .load_with(
            &[("HTTP_PROXY", "http://e:1"), ("HTTPS_PROXY", "http://s:1")],
            CliOverrides::default(),
        )
        .network
        .proxy;
    assert_eq!(p.http, None);
    assert_eq!(p.https.as_deref(), Some("http://s:1"));
}

#[test]
fn env_ignore_scripts_beats_a_cli_false() {
    let fx = Fx::new(&[]);
    let cli = CliOverrides {
        ignore_scripts: Some(false),
        ..Default::default()
    };
    for key in [
        "NPM_CONFIG_IGNORE_SCRIPTS",
        "npm_config_ignore_scripts",
        "FNPM_CONFIG_IGNORE_SCRIPTS",
        "BUN_CONFIG_IGNORE_SCRIPTS",
    ] {
        assert!(
            fx.load_with(&[(key, "true")], cli.clone()).ignore_scripts,
            "{key}"
        );
    }
    // Without the variable the flag still decides.
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("ignoreScripts = true"))]);
    assert!(!fx.load_with(&[], cli).ignore_scripts);
}

#[test]
fn production_flag_freezes_even_with_frozen_lockfile_false() {
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            production: Some(true),
            frozen_lockfile: Some(false),
            ..Default::default()
        },
    );
    assert!(cfg.production && cfg.frozen_lockfile && cfg.omit.dev);
    assert!(!cfg.lockfile.save);
    // `--frozen-lockfile=false` still overrides a config file.
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("frozenLockfile = true"))]);
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            frozen_lockfile: Some(false),
            ..Default::default()
        },
    );
    assert!(!cfg.frozen_lockfile);
}

#[test]
fn debug_output_masks_proxy_and_registry_credentials() {
    let fx = Fx::new(&[(
        "project/.npmrc",
        "https-proxy=http://puser:psecret@proxy:8080/\n//localhost:4873/:_authToken=tsecret\nregistry=http://localhost:4873/\n",
    )]);
    let cfg = fx.load_with(
        &[("HTTP_PROXY", "http://u:hsecret@proxy:3128")],
        CliOverrides::default(),
    );
    assert_eq!(
        cfg.network.proxy.https.as_deref(),
        Some("http://puser:psecret@proxy:8080/")
    );
    let debug = format!("{cfg:?}");
    for secret in ["psecret", "hsecret", "tsecret"] {
        assert!(!debug.contains(secret), "{secret} in {debug}");
    }
    assert!(debug.contains("proxy:8080"));
}

#[test]
fn registry_for_matches_scopes_exactly() {
    let mut cfg = Config::defaults(std::path::Path::new("/p"));
    assert!(std::ptr::eq(cfg.registry_for("@a/b"), &cfg.registry));
    cfg.scopes.insert(
        "@a".to_owned(),
        fnpm_config::RegistryScope::new("https://a.example/"),
    );
    assert_eq!(cfg.registry_for("@a/b").url, "https://a.example/");
    assert_eq!(cfg.registry_for("@ab/c").url, cfg.registry.url);
    assert_eq!(cfg.registry_for("a/b").url, cfg.registry.url);
    assert_eq!(cfg.registry_for("@a").url, cfg.registry.url);
    assert_eq!(cfg.registry_for("plain").url, cfg.registry.url);
    let _ = Auth::None;
}

#[test]
fn dotenv_with_many_keys_keeps_first_file_value_and_last_in_file() {
    let mut text = String::new();
    for i in 0..20_000 {
        text.push_str(&format!("K{i}=v{i}\n"));
    }
    text.push_str("K1=again\nPRE=file\n");
    let mut env = fnpm_config::Env::from_pairs([("PRE", "proc")]);
    let start = std::time::Instant::now();
    fnpm_config::dotenv::load_into(&mut env, &text);
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(env.get("K1"), Some("again"));
    assert_eq!(env.get("K19999"), Some("v19999"));
    assert_eq!(env.get("PRE"), Some("proc"));
}

#[test]
fn global_store_is_opt_in_and_environment_overrides_the_file() {
    let fx = Fx::new(&[]);
    assert!(!fx.load().global_store);
    for key in ["FNPM_GLOBAL_STORE", "BUN_INSTALL_GLOBAL_STORE"] {
        assert!(
            fx.load_with(&[(key, "1")], CliOverrides::default())
                .global_store
        );
    }
    let fx = Fx::new(&[("project/fnpm.toml", &bunfig("globalStore = true"))]);
    assert!(fx.load().global_store);
    assert!(
        !fx.load_with(&[("FNPM_GLOBAL_STORE", "0")], CliOverrides::default())
            .global_store
    );
}
