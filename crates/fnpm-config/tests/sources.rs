//! Config file discovery, directories, environment tuning and derived option rules
//! (config-precedence.test.ts "directories named by config files", npmrc.test.ts "user
//! .npmrc lookup", bun `PackageManagerOptions::load`).

mod common;
use common::*;

use std::path::PathBuf;

use fnpm_config::{
    Backend, CliOverrides, Config, LinkerSetting, LogLevel, ManifestCacheMode, OfflineMode,
};

const NO_DIRS: &[&str] = &[];

#[test]
fn defaults() {
    let fx = Fx::new(&[]);
    let cfg = fx.load();
    let home = fx.path("home");
    assert_eq!(cfg.project_dir, fx.path("project"));
    assert_eq!(cfg.home_dir, home.join(".fnpm"));
    assert_eq!(cfg.store_dir, home.join(".fnpm/store/v1"));
    assert_eq!(cfg.cache_dir, home.join(".fnpm/cache"));
    assert_eq!(cfg.global_dir, home.join(".fnpm/global"));
    assert_eq!(cfg.global_bin_dir, home.join(".fnpm/bin"));
    assert_eq!(cfg.registry.url, fnpm_config::DEFAULT_REGISTRY);
    assert_eq!(cfg.network.concurrency, 64);
    assert_eq!(cfg.network.retries, 5);
    assert_eq!(cfg.network.retry_backoff_ms, 50);
    assert_eq!(cfg.network.idle_timeout_secs, 300);
    assert_eq!(cfg.network.manifest_cache, ManifestCacheMode::CacheControl);
    assert_eq!(cfg.network.offline, OfflineMode::Online);
    assert!(cfg.network.tls.reject_unauthorized);
    assert!(cfg.network.user_agent.starts_with("fnpm/"));
    assert_eq!(cfg.linker, LinkerSetting::Auto);
    assert_eq!(cfg.backend, None);
    assert!(
        !cfg.global_store
            && cfg.hoist
            && cfg.link_workspace_packages
            && cfg.auto_install_peers
            && cfg.verify_integrity
    );
    assert!(cfg.lockfile.save && !cfg.lockfile.print_yarn && !cfg.lockfile.force_save);
    assert!(cfg.load_lockfile && cfg.install_packages && cfg.save_package_json && cfg.progress);
    assert_eq!(
        cfg.concurrent_scripts,
        std::thread::available_parallelism().map_or(1, usize::from) * 2
    );
    assert_eq!(cfg.platform, fnpm_types::Platform::current());
    assert_eq!(cfg.log_level, LogLevel::Default);
    assert!(cfg.run.use_system_shell);
    assert_eq!(cfg.github_api_url, "https://api.github.com");
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    let d = Config::defaults(&fx.path("project"));
    assert_eq!(d.project_dir, fx.path("project"));
    assert_eq!(d.registry.url, fnpm_config::DEFAULT_REGISTRY);
    assert_eq!(d.cache_dir, fx.path("project/node_modules/.fnpm-cache"));
}

#[test]
fn fnpm_home_and_store_env() {
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(
        &[("FNPM_HOME", "~/fh"), ("FNPM_STORE_DIR", "/s")],
        CliOverrides::default(),
    );
    assert_eq!(cfg.home_dir, fx.path("home/fh"));
    assert_eq!(cfg.store_dir, PathBuf::from("/s/v1"));
    assert_eq!(cfg.cache_dir, fx.path("home/fh/cache"));
    assert_eq!(cfg.global_dir, fx.path("home/fh/global"));
}

#[test]
fn cache_dir_precedence() {
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("cache = \"from-bunfig\""))]);
    let cli = CliOverrides {
        cache_dir: Some("./cli".into()),
        ..Default::default()
    };
    assert_eq!(
        fx.load_with(&[("FNPM_CACHE_DIR", "/env")], cli).cache_dir,
        fx.path("project/cli")
    );
    assert_eq!(
        fx.load_with(
            &[("FNPM_CACHE_DIR", "/env"), ("BUN_INSTALL_CACHE_DIR", "/b")],
            CliOverrides::default()
        )
        .cache_dir,
        PathBuf::from("/env")
    );
    assert_eq!(
        fx.load_with(&[("BUN_INSTALL_CACHE_DIR", ".c")], CliOverrides::default())
            .cache_dir,
        fx.path("project/.c")
    );
    assert_eq!(
        fx.load_with(&[("BUN_INSTALL_CACHE_DIR", "~/c")], CliOverrides::default())
            .cache_dir,
        fx.path("home/c")
    );
    assert_eq!(fx.load().cache_dir, fx.path("project/from-bunfig"));
}

#[test]
fn tilde_is_home() {
    for (files, expected) in [
        (
            vec![("project/bunfig.toml", bunfig("cache = { dir = \"~/c\" }"))],
            "home/c",
        ),
        (
            vec![("project/bunfig.toml", bunfig("cache = { dir = \"~\" }"))],
            "home",
        ),
        (
            vec![("project/bunfig.toml", bunfig("cache = \"~/c\""))],
            "home/c",
        ),
        (vec![("project/.npmrc", "cache=~/c\n".to_owned())], "home/c"),
        (
            vec![(
                "project/bunfig.toml",
                bunfig("cache = { dir = \"~user/c\" }"),
            )],
            "project/~user/c",
        ),
    ] {
        let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
        let fx = Fx::new(&files);
        assert_eq!(fx.load().cache_dir, fx.path(expected), "{files:?}");
    }
    let fx = Fx::new(&[("project/.npmrc", "cafile=~/missing.pem\n")]);
    assert_eq!(
        fx.load().network.tls.ca_file,
        Some(fx.path("home/missing.pem"))
    );
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("cafile = \"~/missing.pem\""))]);
    assert_eq!(
        fx.load().network.tls.ca_file,
        Some(fx.path("home/missing.pem"))
    );
}

fn workspace() -> Fx {
    Fx::new(&[
        (
            "project/package.json",
            r#"{"name":"project","version":"1.0.0","workspaces":["packages/*"]}"#,
        ),
        (
            "project/packages/app/package.json",
            r#"{"name":"app","version":"1.0.0"}"#,
        ),
        ("project/src/.keep", ""),
    ])
}

#[test]
fn project_root_is_found_from_subdirectories() {
    let fx = workspace();
    for cwd in ["project/packages/app", "project/src", "project"] {
        assert_eq!(
            fx.load_in(cwd, &[], NO_DIRS, CliOverrides::default())
                .project_dir,
            fx.path("project"),
            "{cwd}"
        );
    }
    // A package that is not a workspace member is its own project.
    fx.write("project/tools/x/package.json", r#"{"name":"x"}"#);
    assert_eq!(
        fx.load_in("project/tools/x", &[], NO_DIRS, CliOverrides::default())
            .project_dir,
        fx.path("project/tools/x")
    );
    // `workspaces.packages` and negations.
    fx.write(
        "project/package.json",
        r#"{"workspaces":{"packages":["packages/**","!packages/app"]}}"#,
    );
    assert_eq!(
        fx.load_in(
            "project/packages/app",
            &[],
            NO_DIRS,
            CliOverrides::default()
        )
        .project_dir,
        fx.path("project/packages/app")
    );
}

#[test]
fn relative_paths_start_at_the_file() {
    for (files, flags, expected) in [
        (
            vec![("project/bunfig.toml", bunfig("cache = { dir = \"./c\" }"))],
            None,
            "project/c",
        ),
        (
            vec![("project/.npmrc", "cache=./c\n".to_owned())],
            None,
            "project/c",
        ),
        (
            vec![("home/.bunfig.toml", bunfig("cache = { dir = \"c\" }"))],
            None,
            "home/c",
        ),
        (
            vec![("home/.bunfig.toml", bunfig("cache = \"c\""))],
            None,
            "home/c",
        ),
        (
            vec![("home/.npmrc", "cache=c\n".to_owned())],
            None,
            "home/c",
        ),
        (
            vec![("ci/ci.toml", bunfig("cache = { dir = \"c\" }"))],
            Some("/ci/ci.toml"),
            "ci/c",
        ),
    ] {
        for cwd in ["project/packages/app", "project/src"] {
            let fx = workspace();
            for (p, t) in &files {
                fx.write(p, t);
            }
            let cli = CliOverrides {
                config: flags.map(|f| fx.path(&f[1..])),
                ..Default::default()
            };
            assert_eq!(
                fx.load_in(cwd, &[], NO_DIRS, cli).cache_dir,
                fx.path(expected),
                "{files:?} {cwd}"
            );
        }
    }
}

#[test]
fn typed_paths_start_where_fnpm_runs() {
    for cwd in ["project/packages/app", "project/src"] {
        let fx = workspace();
        fx.write(
            "project/ci.toml",
            &bunfig("cache = { dir = \"the-file-at-the-root\" }"),
        );
        fx.write(
            &format!("{cwd}/ci.toml"),
            &bunfig("cache = { dir = \"c\" }"),
        );
        let cli = CliOverrides {
            cache_dir: Some("./c".into()),
            ..Default::default()
        };
        assert_eq!(
            fx.load_in(cwd, &[], NO_DIRS, cli).cache_dir,
            fx.path(&format!("{cwd}/c"))
        );
        let cli = CliOverrides {
            config: Some("./ci.toml".into()),
            ..Default::default()
        };
        assert_eq!(
            fx.load_in(cwd, &[], NO_DIRS, cli).cache_dir,
            fx.path(&format!("{cwd}/c"))
        );
        let cli = CliOverrides {
            cafile: Some("ca/x.pem".into()),
            ..Default::default()
        };
        assert_eq!(
            fx.load_in(cwd, &[], NO_DIRS, cli).network.tls.ca_file,
            Some(fx.path(&format!("{cwd}/ca/x.pem")))
        );
        // `$BUN_INSTALL_CACHE_DIR` starts at the project's root.
        let cfg = fx.load_in(
            cwd,
            &[("BUN_INSTALL_CACHE_DIR", ".c")],
            NO_DIRS,
            CliOverrides::default(),
        );
        assert_eq!(cfg.cache_dir, fx.path("project/.c"));
    }
    for (file, base) in [
        ("project/bunfig.toml", "project"),
        ("project/.npmrc", "project"),
        ("home/.npmrc", "home"),
    ] {
        let fx = workspace();
        let text = if file.ends_with(".toml") {
            bunfig("cafile = \"ca/x.pem\"")
        } else {
            "cafile=ca/x.pem\n".to_owned()
        };
        fx.write(file, &text);
        let cfg = fx.load_in("project/src", &[], NO_DIRS, CliOverrides::default());
        assert_eq!(
            cfg.network.tls.ca_file,
            Some(fx.path(&format!("{base}/ca/x.pem"))),
            "{file}"
        );
    }
}

#[test]
fn global_dirs_from_user_files_and_config_flag() {
    for (path, config, unset) in [
        ("home/.bunfig.toml", None, vec![]),
        ("home/.bunfig.toml", None, vec!["XDG_CONFIG_HOME"]),
        ("home/.fnpm.toml", None, vec![]),
        ("home/.fnpm/fnpm.toml", None, vec![]),
        ("elsewhere/named.toml", Some("ABS"), vec![]),
        ("project/named.toml", Some("named.toml"), vec![]),
    ] {
        for body in ["globalDir = \"@G@\"\nglobalBinDir = \"@B@\""] {
            for (g, b, eg, eb) in [
                ("ROOT/home/g", "ROOT/home/b", "home/g", "home/b"),
                ("~/g", "~/b", "home/g", "home/b"),
            ] {
                let fx = Fx::new(&[]);
                let root = fx.root().display().to_string();
                let text = bunfig(
                    &body
                        .replace("@G@", &g.replace("ROOT", &root))
                        .replace("@B@", &b.replace("ROOT", &root)),
                );
                fx.write(path, &text);
                let config = config.map(|c| {
                    if c == "ABS" {
                        fx.path(path)
                    } else {
                        PathBuf::from(c)
                    }
                });
                for global in [false, true] {
                    let cli = CliOverrides {
                        config: config.clone(),
                        global,
                        ..Default::default()
                    };
                    let cfg = fx.load_in("project", &[], &unset, cli);
                    assert_eq!(cfg.global_dir, fx.path(eg), "{path} {global}");
                    assert_eq!(cfg.global_bin_dir, fx.path(eb), "{path} {global}");
                    if global {
                        assert_eq!(cfg.project_dir, fx.path(eg));
                        assert!(cfg.global);
                    }
                }
            }
        }
    }
    // Relative to the file.
    let fx = Fx::new(&[(
        "home/.bunfig.toml",
        &bunfig("globalDir = \"../home/g\"\nglobalBinDir = \"../home/b\""),
    )]);
    let cfg = fx.load();
    assert_eq!(
        (cfg.global_dir, cfg.global_bin_dir),
        (fx.path("home/g"), fx.path("home/b"))
    );
}

#[test]
fn project_global_dirs_are_ignored_with_a_warning() {
    let fx = Fx::new(&[(
        "project/bunfig.toml",
        &bunfig("globalDir = \"/victim/g\"\nglobalBinDir = \"/victim/b\""),
    )]);
    let cfg = fx.load();
    assert_eq!(cfg.global_dir, fx.path("home/.fnpm/global"));
    assert_eq!(
        cfg.warnings
            .iter()
            .filter(|w| w.contains("in project bunfig.toml"))
            .count(),
        2
    );
    // `-g` does not read the project's file at all.
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            global: true,
            ..Default::default()
        },
    );
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    assert_eq!(cfg.global_dir, fx.path("home/.fnpm/global"));
    // Honoured when passed with --config.
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            config: Some("bunfig.toml".into()),
            ..Default::default()
        },
    );
    assert_eq!(cfg.global_dir, PathBuf::from("/victim/g"));
    assert!(cfg.warnings.is_empty());
}

#[test]
fn global_dir_env_precedence() {
    let fx = Fx::new(&[(
        "home/.bunfig.toml",
        &bunfig("globalDir = \"/cfg/g\"\nglobalBinDir = \"/cfg/b\""),
    )]);
    let cfg = fx.load_with(
        &[
            ("FNPM_GLOBAL_DIR", "/e/g"),
            ("BUN_INSTALL_GLOBAL_DIR", "/x"),
            ("BUN_INSTALL_BIN", "/e/b"),
        ],
        CliOverrides::default(),
    );
    assert_eq!(
        (cfg.global_dir, cfg.global_bin_dir),
        (PathBuf::from("/e/g"), PathBuf::from("/e/b"))
    );
    let cfg = fx.load();
    assert_eq!(
        (cfg.global_dir, cfg.global_bin_dir),
        (PathBuf::from("/cfg/g"), PathBuf::from("/cfg/b"))
    );
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(&[("BUN_INSTALL", "~/c")], CliOverrides::default());
    assert_eq!(
        (cfg.global_dir, cfg.global_bin_dir),
        (fx.path("home/c/install/global"), fx.path("home/c/bin"))
    );
    let cfg = fx.load_with(
        &[("BUN_INSTALL", "/b"), ("FNPM_HOME", "/f")],
        CliOverrides::default(),
    );
    assert_eq!(
        (cfg.global_dir, cfg.global_bin_dir),
        (PathBuf::from("/f/global"), PathBuf::from("/f/bin"))
    );
}

#[test]
fn config_file_order() {
    let fx = Fx::new(&[
        (
            "home/.bunfig.toml",
            &bunfig(
                "exact = true\nconcurrentScripts = 1\nnetworkConcurrency = 1\nbackend = \"copyfile\"",
            ),
        ),
        (
            "project/bunfig.toml",
            &bunfig("concurrentScripts = 2\nnetworkConcurrency = 2"),
        ),
        (
            "home/.fnpm/fnpm.toml",
            &bunfig("networkConcurrency = 3\nbackend = \"hardlink\""),
        ),
        ("project/fnpm.toml", &bunfig("networkConcurrency = 4")),
    ]);
    let cfg = fx.load();
    assert!(cfg.exact);
    assert_eq!(cfg.concurrent_scripts, 2);
    assert_eq!(cfg.network.concurrency, 4);
    assert_eq!(cfg.backend, Some(Backend::Hardlink));
    fx.write(
        "project/package.json",
        r#"{"name":"p","fnpm":{"networkConcurrency":5,"exact":false}}"#,
    );
    let cfg = fx.load();
    assert_eq!(cfg.network.concurrency, 5);
    assert!(!cfg.exact);
    // `--config` replaces both project files.
    fx.write("project/named.toml", &bunfig("concurrentScripts = 9"));
    fx.write("project/package.json", r#"{"name":"p"}"#);
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            config: Some("named.toml".into()),
            ..Default::default()
        },
    );
    assert_eq!((cfg.concurrent_scripts, cfg.network.concurrency), (9, 3));
    // A missing `--config` file is an error, a malformed auto-loaded file too.
    assert!(
        fx.try_load_in(
            "project",
            &[],
            &[],
            CliOverrides {
                config: Some("nope.toml".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
    fx.write("project/bunfig.toml", "[install\n");
    let err = fx
        .try_load_in("project", &[], &[], CliOverrides::default())
        .unwrap_err();
    assert!(err.to_string().contains("bunfig.toml"), "{err}");
}

#[test]
fn run_section_is_loaded() {
    let fx = Fx::new(&[(
        "project/bunfig.toml",
        "[run]\nsilent = true\n\"elide-lines\" = 3\nnoOrphans = true\n",
    )]);
    let cfg = fx.load();
    assert!(cfg.run.silent && cfg.run.no_orphans);
    assert_eq!(cfg.run.elide_lines, Some(3));
}

#[test]
fn user_npmrc_lookup() {
    let reg = |cfg: &Config| cfg.registry.url.clone();
    // $XDG_CONFIG_HOME/.npmrc when it exists.
    let fx = Fx::new(&[
        ("xdg/.npmrc", "registry=http://xdg/\n"),
        ("home/.npmrc", "registry=http://home/\n"),
    ]);
    let xdg = fx.path("xdg").display().to_string();
    assert_eq!(
        reg(&fx.load_with(&[("XDG_CONFIG_HOME", &xdg)], CliOverrides::default())),
        "http://xdg/"
    );
    // Falls back to $HOME/.npmrc when $XDG_CONFIG_HOME has none, is unset, or is empty.
    let empty = fx.path("empty").display().to_string();
    assert_eq!(
        reg(&fx.load_with(&[("XDG_CONFIG_HOME", &empty)], CliOverrides::default())),
        "http://home/"
    );
    assert_eq!(
        reg(&fx.load_in(
            "project",
            &[],
            &["XDG_CONFIG_HOME"],
            CliOverrides::default()
        )),
        "http://home/"
    );
    assert_eq!(
        reg(&fx.load_with(&[("XDG_CONFIG_HOME", "")], CliOverrides::default())),
        "http://home/"
    );
    // $NPM_CONFIG_USERCONFIG replaces both; lowercase too; empty is ignored; missing is not a fallback.
    fx.write("u.npmrc", "registry=http://userconfig/\n");
    let u = fx.path("u.npmrc").display().to_string();
    for key in ["NPM_CONFIG_USERCONFIG", "npm_config_userconfig"] {
        assert_eq!(
            reg(&fx.load_with(
                &[(key, &u), ("XDG_CONFIG_HOME", &xdg)],
                CliOverrides::default()
            )),
            "http://userconfig/"
        );
    }
    assert_eq!(
        reg(&fx.load_with(&[("NPM_CONFIG_USERCONFIG", "")], CliOverrides::default())),
        "http://home/"
    );
    assert_eq!(
        reg(&fx.load_with(
            &[("NPM_CONFIG_USERCONFIG", "/does/not/exist")],
            CliOverrides::default()
        )),
        fnpm_config::DEFAULT_REGISTRY
    );
    // Relative to where fnpm started.
    fx.write("project/rel.npmrc", "registry=http://relative/\n");
    assert_eq!(
        reg(&fx.load_with(
            &[("NPM_CONFIG_USERCONFIG", "rel.npmrc")],
            CliOverrides::default()
        )),
        "http://relative/"
    );
    // The project .npmrc still overrides it.
    fx.write("project/.npmrc", "registry=http://project/\n");
    assert_eq!(
        reg(&fx.load_with(&[("NPM_CONFIG_USERCONFIG", &u)], CliOverrides::default())),
        "http://project/"
    );
}

#[test]
fn setup_node_npmrc_expands_node_auth_token() {
    let fx = Fx::new(&[(
        "runner/.npmrc",
        "//npm.pkg.github.com/:_authToken=${NODE_AUTH_TOKEN}\n@octo:registry=https://npm.pkg.github.com/\n",
    )]);
    let u = fx.path("runner/.npmrc").display().to_string();
    let cfg = fx.load_with(
        &[("NPM_CONFIG_USERCONFIG", &u), ("NODE_AUTH_TOKEN", "ghp_x")],
        CliOverrides::default(),
    );
    assert_eq!(auth_of(&cfg, "@octo/pkg"), Some("Bearer ghp_x".to_owned()));
}

#[test]
fn env_tuning() {
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(
        &[
            ("BUN_CONFIG_MAX_HTTP_REQUESTS", "16"),
            ("BUN_CONFIG_HTTP_RETRY_COUNT", "2"),
            ("FNPM_CONFIG_HTTP_RETRY_BACKOFF", "10"),
            ("BUN_CONFIG_HTTP_IDLE_TIMEOUT", "30"),
            ("BUN_CONFIG_YARN_LOCKFILE", ""),
            ("BUN_CONFIG_SKIP_SAVE_LOCKFILE", "1"),
            ("BUN_CONFIG_SKIP_LOAD_LOCKFILE", "yes"),
            ("BUN_CONFIG_SKIP_INSTALL_PACKAGES", "0"),
            ("BUN_CONFIG_NO_VERIFY", "true"),
            ("FNPM_TMPDIR", "/t"),
            ("GITHUB_API_URL", "https://ghe/api/v3"),
            ("NODE_TLS_REJECT_UNAUTHORIZED", "0"),
            ("NODE_USE_SYSTEM_CA", "1"),
            ("NODE_EXTRA_CA_CERTS", "/extra.pem"),
            ("BUN_FEATURE_FLAG_DISABLE_INSTALL_INDEX", "1"),
        ],
        CliOverrides::default(),
    );
    assert_eq!(
        (
            cfg.network.concurrency,
            cfg.network.retries,
            cfg.network.retry_backoff_ms,
            cfg.network.idle_timeout_secs
        ),
        (16, 2, 10, 30)
    );
    assert!(cfg.lockfile.print_yarn);
    assert!(!cfg.lockfile.save && !cfg.load_lockfile && cfg.install_packages);
    assert!(!cfg.verify_integrity);
    assert_eq!(cfg.tmp_dir, Some(PathBuf::from("/t")));
    assert_eq!(cfg.github_api_url, "https://ghe/api/v3");
    assert!(!cfg.network.tls.reject_unauthorized);
    assert_eq!(cfg.network.tls.ca_store.as_deref(), Some("system"));
    assert_eq!(
        cfg.network.tls.extra_ca_certs,
        Some(PathBuf::from("/extra.pem"))
    );
    assert!(cfg.feature_flags.disable_install_index);
    // FNPM_ spelling wins over BUN_.
    let cfg = fx.load_with(
        &[
            ("FNPM_CONFIG_MAX_HTTP_REQUESTS", "8"),
            ("BUN_CONFIG_MAX_HTTP_REQUESTS", "16"),
        ],
        CliOverrides::default(),
    );
    assert_eq!(cfg.network.concurrency, 8);
    // 0 warns and keeps the default, junk is an error.
    let cfg = fx.load_with(
        &[("BUN_CONFIG_MAX_HTTP_REQUESTS", "0")],
        CliOverrides::default(),
    );
    assert_eq!(cfg.network.concurrency, 64);
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("BUN_CONFIG_MAX_HTTP_REQUESTS"))
    );
    assert!(
        fx.try_load_in(
            "project",
            &[("BUN_CONFIG_MAX_HTTP_REQUESTS", "70000")],
            &[],
            CliOverrides::default()
        )
        .is_err()
    );
    // --network-concurrency beats the environment and is clamped to 1.
    let cli = CliOverrides {
        network_concurrency: Some(0),
        ..Default::default()
    };
    assert_eq!(
        fx.load_with(&[("BUN_CONFIG_MAX_HTTP_REQUESTS", "16")], cli)
            .network
            .concurrency,
        1
    );
    // NO_VERIFY needs 1 or true.
    assert!(
        fx.load_with(&[("BUN_CONFIG_NO_VERIFY", "yes")], CliOverrides::default())
            .verify_integrity
    );
}

#[test]
fn manifest_cache_env_and_flags() {
    let fx = Fx::new(&[]);
    let mode =
        |env: &[(&str, &str)], cli: CliOverrides| fx.load_with(env, cli).network.manifest_cache;
    assert_eq!(
        mode(&[("BUN_MANIFEST_CACHE", "1")], CliOverrides::default()),
        ManifestCacheMode::NoCacheControl
    );
    assert_eq!(
        mode(&[("FNPM_MANIFEST_CACHE", "2")], CliOverrides::default()),
        ManifestCacheMode::CacheControl
    );
    assert_eq!(
        mode(&[("BUN_MANIFEST_CACHE", "0")], CliOverrides::default()),
        ManifestCacheMode::Disabled
    );
    assert_eq!(
        mode(
            &[],
            CliOverrides {
                no_cache: true,
                ..Default::default()
            }
        ),
        ManifestCacheMode::Disabled
    );
    assert_eq!(
        mode(
            &[],
            CliOverrides {
                update: true,
                ..Default::default()
            }
        ),
        ManifestCacheMode::Disabled
    );
    assert_eq!(
        mode(
            &[],
            CliOverrides {
                force: Some(true),
                ..Default::default()
            }
        ),
        ManifestCacheMode::NoCacheControl
    );
    // Offline modes keep the cache on, since it is their only source.
    let cli = CliOverrides {
        no_cache: true,
        offline: Some(true),
        ..Default::default()
    };
    let cfg = fx.load_with(&[], cli);
    assert_eq!(
        (cfg.network.offline, cfg.network.manifest_cache),
        (OfflineMode::Offline, ManifestCacheMode::NoCacheControl)
    );
    let cli = CliOverrides {
        offline: Some(true),
        prefer_offline: Some(true),
        ..Default::default()
    };
    assert_eq!(fx.load_with(&[], cli).network.offline, OfflineMode::Offline);
    let cli = CliOverrides {
        prefer_offline: Some(true),
        ..Default::default()
    };
    assert_eq!(
        fx.load_with(&[], cli).network.offline,
        OfflineMode::PreferOffline
    );
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("prefer = \"offline\""))]);
    assert_eq!(fx.load().network.offline, OfflineMode::PreferOffline);
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("offline = true"))]);
    assert_eq!(fx.load().network.offline, OfflineMode::Offline);
}

#[test]
fn derived_rules() {
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            production: Some(true),
            ..Default::default()
        },
    );
    assert!(
        cfg.production
            && cfg.frozen_lockfile
            && cfg.omit.dev
            && cfg.fail_early
            && !cfg.lockfile.save
    );
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            dry_run: Some(true),
            yarn: Some(true),
            ..Default::default()
        },
    );
    assert!(
        cfg.dry_run
            && !cfg.install_packages
            && !cfg.save_package_json
            && !cfg.lockfile.save
            && !cfg.lockfile.print_yarn
    );
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            save: Some(false),
            ..Default::default()
        },
    );
    assert!(!cfg.lockfile.save && !cfg.save_package_json && cfg.install_packages);
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            force: Some(true),
            ..Default::default()
        },
    );
    assert!(cfg.force && cfg.lockfile.force_save);
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            frozen_lockfile: Some(true),
            force: Some(true),
            ..Default::default()
        },
    );
    assert!(!cfg.lockfile.save && !cfg.lockfile.force_save);
    let fx = Fx::new(&[(
        "project/bunfig.toml",
        "[install]\nproduction = true\n[install.lockfile]\nsave = true\n",
    )]);
    let cfg = fx.load();
    assert!(cfg.frozen_lockfile && !cfg.lockfile.save && !cfg.lockfile.force_save);
    let fx = Fx::new(&[(
        "project/bunfig.toml",
        "[install.lockfile]\nsave = true\npath = \"x.lock\"\n",
    )]);
    let cfg = fx.load();
    assert!(cfg.lockfile.save && cfg.lockfile.force_save);
    assert_eq!(cfg.lockfile.path, Some(PathBuf::from("x.lock")));
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("minimumReleaseAge = 0"))]);
    assert_eq!(fx.load().minimum_release_age_ms, None);
}

#[test]
fn cli_flags() {
    let fx = Fx::new(&[(
        "project/bunfig.toml",
        &bunfig(
            "ca = \"A\"\ncafile = \"/a.pem\"\nminimumReleaseAge = 60\nminimumReleaseAgeExcludes = [\"x\"]",
        ),
    )]);
    let cli = CliOverrides {
        omit: vec!["optional".into(), "peer".into()],
        ignore_scripts: Some(true),
        exact: Some(true),
        concurrent_scripts: Some(3),
        ca: vec!["B".into()],
        ca_store: Some("bundled".into()),
        minimum_release_age_ms: Some(5),
        minimum_release_age_excludes: vec!["a, b@1.0.0 || 1.0.1".into(), "bad@^1".into()],
        backend: Some(Backend::Symlink),
        log_level: Some(LogLevel::Silent),
        cpu: vec!["arm64".into()],
        os: vec!["linux".into()],
        save_text_lockfile: Some(false),
        verify: Some(false),
        ..Default::default()
    };
    let cfg = fx.load_with(&[], cli);
    assert!(cfg.omit.optional && cfg.omit.peer && !cfg.omit.dev && !cfg.auto_install_peers);
    assert!(cfg.ignore_scripts && cfg.exact && !cfg.save_text_lockfile && !cfg.verify_integrity);
    assert_eq!(cfg.concurrent_scripts, 3);
    assert_eq!(
        (cfg.network.tls.ca.clone(), cfg.network.tls.ca_file.clone()),
        (vec!["B".to_owned()], None)
    );
    assert_eq!(cfg.network.tls.ca_store.as_deref(), Some("bundled"));
    assert_eq!(cfg.minimum_release_age_ms, Some(5));
    assert_eq!(cfg.minimum_release_age_excludes, ["a", "b@1.0.0 || 1.0.1"]);
    assert!(cfg.warnings.iter().any(|w| w.contains("bad@^1")));
    assert_eq!(cfg.backend, Some(Backend::Symlink));
    assert_eq!(cfg.log_level, LogLevel::Silent);
    assert_eq!(cfg.platform.cpu.names().collect::<Vec<_>>(), ["arm64"]);
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            concurrent_scripts: Some(0),
            ..Default::default()
        },
    );
    assert_eq!(
        cfg.concurrent_scripts,
        std::thread::available_parallelism().map_or(1, usize::from) * 2
    );
    assert_eq!(cfg.minimum_release_age_ms, Some(60_000));
    assert_eq!(cfg.minimum_release_age_excludes, ["x"]);
    let err = fx
        .try_load_in(
            "project",
            &[],
            &[],
            CliOverrides {
                omit: vec!["devv".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("invalid `omit` value: 'devv'"));
    let err = fx
        .try_load_in(
            "project",
            &[],
            &[],
            CliOverrides {
                cpu: vec!["z80".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("invalid cpu: 'z80'"), "{err}");
}

#[test]
fn npmrc_omit_and_scripts_env() {
    let fx = Fx::new(&[(
        "project/.npmrc",
        "omit=dev\ninclude[]=peer\nignore-scripts=false\n",
    )]);
    let cfg = fx.load();
    assert!(cfg.omit.dev && !cfg.omit.peer && !cfg.ignore_scripts);
    for key in [
        "NPM_CONFIG_IGNORE_SCRIPTS",
        "npm_config_ignore_scripts",
        "FNPM_CONFIG_IGNORE_SCRIPTS",
    ] {
        assert!(
            fx.load_with(&[(key, "true")], CliOverrides::default())
                .ignore_scripts,
            "{key}"
        );
    }
    // It can only turn scripts off.
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("ignoreScripts = true"))]);
    assert!(
        fx.load_with(
            &[("npm_config_ignore_scripts", "false")],
            CliOverrides::default()
        )
        .ignore_scripts
    );
    assert!(
        !Fx::new(&[])
            .load_with(
                &[("npm_config_ignore_scripts", "")],
                CliOverrides::default()
            )
            .ignore_scripts
    );
}

#[test]
fn npm_config_env_layer() {
    let fx = Fx::new(&[("project/.npmrc", "save-exact=false\nnode-linker=hoisted\n")]);
    let cfg = fx.load_with(
        &[
            ("npm_config_save_exact", "true"),
            ("BUN_CONFIG_NODE_LINKER", "isolated"),
            ("NPM_CONFIG_OMIT", "dev"),
            ("npm_config_strict_ssl", "false"),
        ],
        CliOverrides::default(),
    );
    assert!(cfg.exact);
    assert_eq!(cfg.linker, LinkerSetting::Isolated);
    assert!(cfg.omit.dev);
    assert!(!cfg.network.tls.reject_unauthorized);
}

#[test]
fn proxies() {
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(
        &[
            ("HTTPS_PROXY", "http://s:1"),
            ("http_proxy", "http://p:2"),
            ("NO_PROXY", "a.com, b.com"),
        ],
        CliOverrides::default(),
    );
    assert_eq!(cfg.network.proxy.https.as_deref(), Some("http://s:1"));
    assert_eq!(cfg.network.proxy.http.as_deref(), Some("http://p:2"));
    assert_eq!(cfg.network.proxy.no_proxy, ["a.com", "b.com"]);
    // Like bun, the http variables never serve https requests.
    let cfg = fx.load_with(&[("HTTP_PROXY", "http://only:3")], CliOverrides::default());
    assert_eq!(cfg.network.proxy.https, None);
    let fx = Fx::new(&[(
        "project/.npmrc",
        "proxy=http://rc:1/\nhttps-proxy=false\nnoproxy=x.com\n",
    )]);
    let cfg = fx.load_with(
        &[("HTTPS_PROXY", "http://s:1"), ("NO_PROXY", "y.com")],
        CliOverrides::default(),
    );
    assert_eq!(cfg.network.proxy.http.as_deref(), Some("http://rc:1/"));
    assert_eq!(cfg.network.proxy.https, None);
    assert_eq!(cfg.network.proxy.no_proxy, ["x.com"]);
}

#[test]
fn progress_ci_verbose_global_store() {
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(
        &[("CI", "true"), ("GITHUB_ACTIONS", "true")],
        CliOverrides::default(),
    );
    assert!(cfg.ci && !cfg.progress);
    assert!(
        cfg.network.user_agent.ends_with(" ci/github-actions"),
        "{}",
        cfg.network.user_agent
    );
    assert!(
        fx.load_with(
            &[("CI", "true"), ("FNPM_PROGRESS", "1")],
            CliOverrides::default()
        )
        .progress
    );
    assert!(
        !fx.load_with(&[("BUN_INSTALL_PROGRESS", "0")], CliOverrides::default())
            .progress
    );
    assert_eq!(
        fx.load_with(&[("FNPM_VERBOSE", "1")], CliOverrides::default())
            .log_level,
        LogLevel::Verbose
    );
    let cli = CliOverrides {
        log_level: Some(LogLevel::Quiet),
        ..Default::default()
    };
    assert_eq!(
        fx.load_with(&[("BUN_INSTALL_VERBOSE", "1")], cli).log_level,
        LogLevel::Quiet
    );
    assert!(
        !fx.load_with(&[("FNPM_GLOBAL_STORE", "0")], CliOverrides::default())
            .global_store
    );
    let fx = Fx::new(&[("project/bunfig.toml", &bunfig("globalStore = false"))]);
    assert!(!fx.load().global_store);
    assert!(
        fx.load_with(
            &[("BUN_INSTALL_GLOBAL_STORE", "yes")],
            CliOverrides::default()
        )
        .global_store
    );
}

#[test]
fn provenance_sources() {
    let fx = Fx::new(&[("project/.npmrc", "provenance=true\n")]);
    assert_eq!(fx.load().provenance, Some(true));
    assert_eq!(
        fx.load_with(
            &[("NPM_CONFIG_PROVENANCE", "false")],
            CliOverrides::default()
        )
        .provenance,
        Some(false)
    );
    assert_eq!(Fx::new(&[]).load().provenance, None);
}

#[test]
fn user_agent_has_node_version() {
    let fx = Fx::new(&[]);
    let opts = fnpm_config::LoadOptions {
        cwd: fx.path("project"),
        env: fx.env(&[], &[]),
        node_version: Some("v22.11.0".into()),
        ..Default::default()
    };
    let ua = Config::load(&opts).unwrap().network.user_agent;
    assert!(ua.contains(" npm/? node/v22.11.0 "), "{ua}");
}

#[test]
fn redaction() {
    let fx = Fx::new(&[(
        "project/.npmrc",
        &format!(
            "registry=https://r.example/\n//r.example/:_authToken={TOKEN}\n//r.example/:email=me@example.com\nproxy=http://u:pw@proxy:1/\n"
        ),
    )]);
    let cfg = fx.load();
    assert_eq!(cfg.registry.email.as_deref(), Some("me@example.com"));
    let red = cfg.redacted();
    let json = serde_json::to_string(&red).unwrap();
    assert!(
        !json.contains(TOKEN) && !json.contains("me@example.com") && !json.contains(":pw@"),
        "{json}"
    );
    assert!(!format!("{cfg:?}").contains(TOKEN));
    assert_eq!(
        red.network.proxy.http.as_deref(),
        Some("http://***@proxy:1/")
    );
}

#[test]
fn config_round_trips_through_serde() {
    let cfg = Fx::new(&[]).load();
    let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        back,
        Config {
            warnings: Vec::new(),
            ..cfg
        }
    );
}

/// `cargo test --release -p fnpm-config --test sources -- --ignored --nocapture load_perf`
#[test]
#[ignore = "timing, run by hand"]
fn load_perf() {
    let fx = Fx::new(&[
        ("home/.npmrc", "//registry.npmjs.org/:_authToken=abc\n"),
        (
            "project/.npmrc",
            "@myorg:registry=https://npm.myorg.dev/\n//npm.myorg.dev/:_authToken=${MYORG}\n",
        ),
        (
            "project/bunfig.toml",
            &bunfig(
                "exact = true\nlinker = \"isolated\"\n[install.scopes]\nother = { url = \"https://o/\", token = \"t\" }",
            ),
        ),
    ]);
    let opts = fnpm_config::LoadOptions {
        cwd: fx.path("project"),
        env: fx.env(&[("MYORG", "x")], &[]),
        ..Default::default()
    };
    Config::load(&opts).unwrap();
    let n = 2000;
    let start = std::time::Instant::now();
    for _ in 0..n {
        std::hint::black_box(Config::load(&opts).unwrap());
    }
    let per = start.elapsed() / n;
    println!("Config::load: {per:?} per call");
    assert!(per < std::time::Duration::from_millis(1));
}

#[test]
fn dot_env_files_feed_the_config() {
    let fx = Fx::new(&[
        (
            "project/.env",
            &format!("BUN_CONFIG_REGISTRY={REG}\nMYORG_TOKEN=from-dotenv\n"),
        ),
        (
            "project/.npmrc",
            &format!(
                "registry={DEAD}\n@types:registry=${{EMPTY_URL}}\n//npm.myorg.dev/:_authToken=${{MYORG_TOKEN}}\n@myorg:registry=https://npm.myorg.dev/\n"
            ),
        ),
    ]);
    let cfg = fx.load_with(&[("EMPTY_URL", "")], CliOverrides::default());
    assert_eq!(cfg.registry.url, REG);
    assert_eq!(cfg.registry_for("@types/no-deps").url, REG);
    assert_eq!(
        auth_of(&cfg, "@myorg/x"),
        Some("Bearer from-dotenv".to_owned())
    );
    // The process environment wins over `.env`.
    let cfg = fx.load_with(
        &[("BUN_CONFIG_REGISTRY", "http://proc/")],
        CliOverrides::default(),
    );
    assert_eq!(cfg.registry.url, "http://proc/");
    // `--no-env-file` reads none, `--env-file` only the named ones.
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            no_env_file: true,
            ..Default::default()
        },
    );
    assert_eq!(cfg.registry.url, DEAD);
    fx.write("project/ci.env", "BUN_CONFIG_REGISTRY=http://ci/\n");
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            env_file: vec!["ci.env".into()],
            ..Default::default()
        },
    );
    assert_eq!(cfg.registry.url, "http://ci/");
    assert_eq!(
        auth_of(&cfg, "@myorg/x"),
        Some("Bearer ${MYORG_TOKEN}".to_owned())
    );
    let opts = fnpm_config::LoadOptions {
        cwd: fx.path("project"),
        env: fx.env(&[], &[]),
        ..Default::default()
    };
    assert_eq!(
        opts.effective_env().unwrap().get("MYORG_TOKEN"),
        Some("from-dotenv")
    );
}
