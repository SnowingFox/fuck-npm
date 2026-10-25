//! Ported from bun `test/cli/install/config-precedence.test.ts`: which source wins for each
//! setting, checked on the resolved `Config` instead of a live install.

mod common;
use common::*;

use fnpm_config::{CliOverrides, LinkerSetting, ManifestCacheMode};

#[test]
fn project_bunfig_linker_beats_home_npmrc_install_strategy() {
    let fx = Fx::new(&[
        ("home/.npmrc", "install-strategy=hoisted\n"),
        (
            "project/bunfig.toml",
            &bunfig(&format!("registry = \"{REG}\"\nlinker = \"isolated\"")),
        ),
    ]);
    assert_eq!(fx.load().linker, LinkerSetting::Isolated);
}

#[test]
fn project_bunfig_registry_beats_project_npmrc_registry() {
    let fx = Fx::new(&[
        ("project/.npmrc", &format!("registry={DEAD}\n")),
        (
            "project/bunfig.toml",
            &bunfig(&format!("registry = \"{REG}\"")),
        ),
    ]);
    assert_eq!(fx.load().registry.url, REG);
}

#[test]
fn project_bunfig_registry_beats_home_npmrc_registry() {
    let fx = Fx::new(&[
        ("home/.npmrc", &format!("registry={DEAD}\n")),
        (
            "project/bunfig.toml",
            &bunfig(&format!("registry = \"{REG}\"")),
        ),
    ]);
    assert_eq!(fx.load().registry.url, REG);
}

#[test]
fn project_npmrc_registry_beats_home_npmrc_registry() {
    let fx = Fx::new(&[
        ("home/.npmrc", &format!("registry={DEAD}\n")),
        ("project/.npmrc", &format!("registry={REG}\n")),
    ]);
    assert_eq!(fx.load().registry.url, REG);
}

#[test]
fn home_npmrc_token_applies_to_registry_set_in_project_bunfig() {
    let fx = Fx::new(&[
        ("home/.npmrc", &auth_line("localhost:4873")),
        (
            "project/bunfig.toml",
            &bunfig(&format!("registry = \"{REG}\"")),
        ),
    ]);
    assert_eq!(
        auth_of(&fx.load(), "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn project_npmrc_scope_and_token_alongside_bunfig_registry() {
    let fx = Fx::new(&[
        (
            "project/.npmrc",
            &format!(
                "@needs-auth:registry={REG}\n{}",
                auth_line("localhost:4873")
            ),
        ),
        (
            "project/bunfig.toml",
            &bunfig(&format!("registry = \"{DEAD}\"")),
        ),
    ]);
    let cfg = fx.load();
    assert_eq!(cfg.registry.url, DEAD);
    assert_eq!(cfg.registry_for("@needs-auth/test-pkg").url, REG);
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    assert_eq!(auth_of(&cfg, "no-deps"), None);
}

#[test]
fn bunfig_scoped_registry_keeps_credentials_from_home_npmrc() {
    let fx = Fx::new(&[
        ("home/.npmrc", &auth_line("localhost:4873")),
        (
            "project/bunfig.toml",
            &bunfig(&format!(
                "registry = \"{DEAD}\"\nscopes = {{ \"needs-auth\" = \"{REG}\" }}"
            )),
        ),
    ]);
    assert_eq!(
        auth_of(&fx.load(), "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn home_registry_line_and_token_still_authenticate_bunfig_registry() {
    let fx = Fx::new(&[
        (
            "home/.npmrc",
            &format!("registry={DEAD}\n{}", auth_line("localhost:4873")),
        ),
        (
            "project/bunfig.toml",
            &bunfig(&format!("registry = \"{REG}\"")),
        ),
    ]);
    let cfg = fx.load();
    assert_eq!(cfg.registry.url, REG);
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn npmrc_token_applies_when_bunfig_spells_registry_with_trailing_slash() {
    let fx = Fx::new(&[
        (
            "project/.npmrc",
            &format!(
                "registry=http://localhost:4873\n{}",
                auth_line("localhost:4873")
            ),
        ),
        (
            "project/bunfig.toml",
            &bunfig(&format!("registry = \"{REG}\"")),
        ),
    ]);
    assert_eq!(
        auth_of(&fx.load(), "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    let fx = Fx::new(&[
        (
            "project/.npmrc",
            &format!(
                "@needs-auth:registry=http://localhost:4873\n{}",
                auth_line("localhost:4873")
            ),
        ),
        (
            "project/bunfig.toml",
            &bunfig(&format!(
                "registry = \"{DEAD}\"\nscopes = {{ \"needs-auth\" = \"{REG}\" }}"
            )),
        ),
    ]);
    assert_eq!(
        auth_of(&fx.load(), "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn bunfig_registry_token_beats_npmrc_token_for_same_registry() {
    let fx = Fx::new(&[
        (
            "home/.npmrc",
            "//localhost:4873/:_authToken=token-from-npmrc\n",
        ),
        (
            "project/bunfig.toml",
            &format!("[install.registry]\nurl = \"{REG}\"\ntoken = \"{TOKEN}\"\n"),
        ),
    ]);
    let cfg = fx.load();
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    // Tarballs on the registry's origin carry the same credential.
    assert_eq!(
        cfg.authorization_header("http://localhost:4873/@needs-auth/test-pkg/-/test-pkg-1.0.0.tgz"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn bunfig_scoped_registry_beats_same_scope_in_npmrc() {
    let fx = Fx::new(&[
        ("project/.npmrc", &format!("@types:registry={DEAD}\n")),
        (
            "project/bunfig.toml",
            &bunfig(&format!(
                "registry = \"{DEAD}\"\nscopes = {{ types = \"{REG}\" }}"
            )),
        ),
    ]);
    assert_eq!(fx.load().registry_for("@types/no-deps").url, REG);
}

#[test]
fn bunfig_credentials_survive_same_url_registry_line() {
    let fx = Fx::new(&[
        ("project/.npmrc", &format!("registry={REG}\n")),
        (
            "project/bunfig.toml",
            &format!("[install.registry]\nurl = \"{REG}\"\ntoken = \"{TOKEN}\"\n"),
        ),
    ]);
    assert_eq!(
        auth_of(&fx.load(), "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    let fx = Fx::new(&[
        ("project/.npmrc", &format!("@needs-auth:registry={REG}\n")),
        (
            "project/bunfig.toml",
            &format!(
                "[install]\nregistry = \"{DEAD}\"\n[install.scopes]\nneeds-auth = {{ url = \"{REG}\", token = \"{TOKEN}\" }}\n"
            ),
        ),
    ]);
    assert_eq!(
        auth_of(&fx.load(), "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    let fx = Fx::new(&[
        ("project/.npmrc", &format!("registry={REG}\n")),
        (
            "project/bunfig.toml",
            &format!(
                "[install.registry]\nurl = \"{REG}\"\nusername = \"config-precedence\"\npassword = \"verysecure\"\n"
            ),
        ),
    ]);
    assert_eq!(
        auth_of(&fx.load(), "@needs-auth/test-pkg"),
        Some(basic("config-precedence", "verysecure"))
    );
}

#[test]
fn bunfig_registry_userinfo_forms() {
    for form in ["registry = \"URL\"", "[install.registry]\nurl = \"URL\""] {
        let body = form.replace("URL", "http://config-precedence:verysecure@localhost:4873/");
        let fx = Fx::new(&[("project/bunfig.toml", &format!("[install]\n{body}\n"))]);
        let cfg = fx.load();
        assert_eq!(cfg.registry.url, REG);
        assert_eq!(
            auth_of(&cfg, "@needs-auth/test-pkg"),
            Some(basic("config-precedence", "verysecure"))
        );
        let body = form.replace("URL", "http://:token-from-url@localhost:4873/");
        let fx = Fx::new(&[("project/bunfig.toml", &format!("[install]\n{body}\n"))]);
        assert_eq!(
            auth_of(&fx.load(), "no-deps"),
            Some("Bearer token-from-url".to_owned())
        );
    }
    for scope in [
        "needs-auth = \"http://config-precedence:verysecure@localhost:4873/\"",
        "needs-auth = { url = \"http://config-precedence:verysecure@localhost:4873/\" }",
    ] {
        let fx = Fx::new(&[(
            "project/bunfig.toml",
            &format!("[install]\nregistry = \"{DEAD}\"\n[install.scopes]\n{scope}\n"),
        )]);
        let cfg = fx.load();
        assert_eq!(cfg.registry_for("@needs-auth/x").url, REG);
        assert_eq!(
            auth_of(&cfg, "@needs-auth/test-pkg"),
            Some(basic("config-precedence", "verysecure"))
        );
    }
}

#[test]
fn bunfig_registry_object_keys_replace_url_credentials() {
    let token_case = format!(
        "url = \"http://user-from-url:password-from-url@localhost:4873/\"\ntoken = \"{TOKEN}\""
    );
    for (table, expected) in [
        (
            "url = \"http://user-from-url:password-from-url@localhost:4873/\"\nusername = \"config-precedence\"\npassword = \"verysecure\"",
            basic("config-precedence", "verysecure"),
        ),
        (
            "url = \"http://:token-from-url@localhost:4873/\"\nusername = \"config-precedence\"\npassword = \"verysecure\"",
            basic("config-precedence", "verysecure"),
        ),
        (token_case.as_str(), format!("Bearer {TOKEN}")),
    ] {
        let fx = Fx::new(&[(
            "project/bunfig.toml",
            &format!("[install.registry]\n{table}\n"),
        )]);
        assert_eq!(auth_of(&fx.load(), "@needs-auth/test-pkg"), Some(expected));
    }
}

#[test]
fn url_credentials_in_bunfig_object_beat_npmrc_token() {
    let fx = Fx::new(&[
        (
            "home/.npmrc",
            "//localhost:4873/:_authToken=token-from-npmrc\n",
        ),
        (
            "project/bunfig.toml",
            "[install.registry]\nurl = \"http://config-precedence:verysecure@localhost:4873/\"\n",
        ),
    ]);
    assert_eq!(
        auth_of(&fx.load(), "@needs-auth/test-pkg"),
        Some(basic("config-precedence", "verysecure"))
    );
}

#[test]
fn cli_linker_beats_every_file() {
    let fx = Fx::new(&[
        ("home/.npmrc", "install-strategy=hoisted\n"),
        ("project/.npmrc", "install-strategy=hoisted\n"),
        ("project/bunfig.toml", &bunfig("linker = \"hoisted\"")),
    ]);
    let cfg = fx.load_with(
        &[],
        CliOverrides {
            linker: Some(LinkerSetting::Isolated),
            ..Default::default()
        },
    );
    assert_eq!(cfg.linker, LinkerSetting::Isolated);
}

#[test]
fn home_npmrc_install_strategy_applies_without_bunfig_linker() {
    let fx = Fx::new(&[
        ("home/.npmrc", "install-strategy=linked\n"),
        (
            "project/bunfig.toml",
            &bunfig(&format!("registry = \"{REG}\"")),
        ),
    ]);
    assert_eq!(fx.load().linker, LinkerSetting::Isolated);
}

#[test]
fn bunfig_exact_beats_npmrc_save_exact() {
    let fx = Fx::new(&[
        ("project/.npmrc", "save-exact=false\n"),
        ("project/bunfig.toml", &bunfig("exact = true")),
    ]);
    assert!(fx.load().exact);
    let fx = Fx::new(&[
        ("home/.npmrc", "save-exact=true\n"),
        ("project/bunfig.toml", &bunfig("exact = false")),
    ]);
    assert!(!fx.load().exact);
}

#[test]
fn bunfig_ignore_scripts_beats_npmrc() {
    let fx = Fx::new(&[
        ("project/.npmrc", "ignore-scripts=false\n"),
        ("project/bunfig.toml", &bunfig("ignoreScripts = true")),
    ]);
    assert!(fx.load().ignore_scripts);
    let fx = Fx::new(&[
        ("home/.npmrc", "ignore-scripts=true\n"),
        ("project/bunfig.toml", &bunfig("ignoreScripts = false")),
    ]);
    assert!(!fx.load().ignore_scripts);
}

#[test]
fn bunfig_hoist_settings_beat_npmrc() {
    let fx = Fx::new(&[
        ("project/.npmrc", "public-hoist-pattern=no-deps\n"),
        (
            "project/bunfig.toml",
            &bunfig("publicHoistPattern = \"*types*\""),
        ),
    ]);
    assert_eq!(fx.load().public_hoist_pattern, ["*types*"]);
    let fx = Fx::new(&[
        ("home/.npmrc", "hoist-pattern=@types/*\n"),
        ("project/bunfig.toml", &bunfig("hoistPattern = \"no-deps\"")),
    ]);
    assert_eq!(fx.load().hoist_pattern, ["no-deps"]);
    let fx = Fx::new(&[
        ("project/.npmrc", "hoist=false\n"),
        ("project/bunfig.toml", &bunfig("hoist = true")),
    ]);
    assert!(fx.load().hoist);
    let fx = Fx::new(&[
        ("home/.npmrc", "hoist=true\n"),
        ("project/bunfig.toml", &bunfig("hoist = false")),
    ]);
    assert!(!fx.load().hoist);
    let cfg = Fx::new(&[]).load();
    assert_eq!(cfg.hoist_pattern, ["*"]);
    assert!(cfg.public_hoist_pattern.is_empty());
}

#[test]
fn scopes_from_npmrc_and_bunfig_merge() {
    let fx = Fx::new(&[
        ("project/.npmrc", &format!("@types:registry={REG}\n")),
        (
            "project/bunfig.toml",
            &format!(
                "[install]\nregistry = \"{DEAD}\"\n[install.scopes]\nneeds-auth = {{ url = \"{REG}\", token = \"{TOKEN}\" }}\n"
            ),
        ),
    ]);
    let cfg = fx.load();
    assert_eq!(cfg.registry_for("@types/no-deps").url, REG);
    assert_eq!(cfg.registry_for("@needs-auth/test-pkg").url, REG);
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    assert_eq!(cfg.scopes.len(), 2);
}

#[test]
fn bunfig_cache_dir_beats_npmrc_cache() {
    let fx = Fx::new(&[
        ("project/.npmrc", "cache=../npmrc-cache\n"),
        (
            "project/bunfig.toml",
            &bunfig("cache = { dir = \"../bunfig-cache\" }"),
        ),
    ]);
    assert_eq!(fx.load().cache_dir, fx.path("bunfig-cache"));
}

#[test]
fn bunfig_cache_false_disables_cache_and_metadata() {
    for body in ["cache = false", "cache = { disable = true }"] {
        let cfg = Fx::new(&[("project/bunfig.toml", &bunfig(body))]).load();
        assert!(cfg.cache_disabled, "{body}");
        assert_eq!(cfg.network.manifest_cache, ManifestCacheMode::Disabled);
    }
}

// ── registry from the command line and the environment ──

/// Owned `(path or key, text)` pairs.
type Owned = Vec<(&'static str, String)>;

const BASIC_CP: (&str, &str) = ("config-precedence", "verysecure");

fn with_userinfo(userinfo: &str) -> String {
    format!("http://{userinfo}@localhost:4873/")
}

#[test]
fn cli_registry_userinfo_is_basic_auth() {
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(
        &[],
        registry_cli(&with_userinfo("config-precedence:verysecure")),
    );
    assert_eq!(cfg.registry.url, REG);
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(basic(BASIC_CP.0, BASIC_CP.1))
    );
    assert!(!format!("{:?}", cfg).contains("verysecure"));
    let cfg = fx.load_with(&[], registry_cli(&with_userinfo(&format!(":{TOKEN}"))));
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn token_env_beats_cli_registry_userinfo() {
    let cfg = Fx::new(&[]).load_with(
        &[("BUN_CONFIG_TOKEN", TOKEN)],
        registry_cli(&with_userinfo(":wrong-token")),
    );
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn registry_env_userinfo_is_basic_auth() {
    for key in [
        "FNPM_REGISTRY",
        "FNPM_CONFIG_REGISTRY",
        "BUN_CONFIG_REGISTRY",
        "NPM_CONFIG_REGISTRY",
        "npm_config_registry",
    ] {
        let url = with_userinfo("config-precedence:verysecure");
        let cfg = Fx::new(&[]).load_with(&[(key, &url)], CliOverrides::default());
        assert_eq!(cfg.registry.url, REG, "{key}");
        assert_eq!(
            auth_of(&cfg, "@needs-auth/test-pkg"),
            Some(basic(BASIC_CP.0, BASIC_CP.1)),
            "{key}"
        );
    }
    let url = with_userinfo(&format!(":{TOKEN}"));
    let cfg = Fx::new(&[]).load_with(&[("BUN_CONFIG_REGISTRY", &url)], CliOverrides::default());
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn registry_env_order_first_non_empty_wins() {
    let fx = Fx::new(&[]);
    let cfg = fx.load_with(
        &[
            ("FNPM_REGISTRY", " "),
            ("BUN_CONFIG_REGISTRY", "http://b/"),
            ("NPM_CONFIG_REGISTRY", "http://n/"),
        ],
        CliOverrides::default(),
    );
    assert_eq!(cfg.registry.url, "http://b/");
    let cfg = fx.load_with(
        &[
            ("FNPM_REGISTRY", "http://f/"),
            ("BUN_CONFIG_REGISTRY", "http://b/"),
        ],
        CliOverrides::default(),
    );
    assert_eq!(cfg.registry.url, "http://f/");
    let cfg = fx.load_with(
        &[("NPM_CONFIG_REGISTRY", "http://n/")],
        registry_cli("http://cli/"),
    );
    assert_eq!(cfg.registry.url, "http://cli/");
}

#[test]
fn user_only_userinfo_is_basic_with_empty_password() {
    let url = with_userinfo("config-precedence:");
    let expected = basic("config-precedence", "");
    let sources: Vec<(Owned, Owned, CliOverrides, &str)> = vec![
        (
            vec![(
                "project/bunfig.toml",
                bunfig(&format!("registry = \"{url}\"")),
            )],
            vec![],
            CliOverrides::default(),
            "no-deps",
        ),
        (
            vec![(
                "project/bunfig.toml",
                format!("[install.registry]\nurl = \"{url}\"\n"),
            )],
            vec![],
            CliOverrides::default(),
            "no-deps",
        ),
        (
            vec![(
                "project/bunfig.toml",
                bunfig(&format!("scopes = {{ types = \"{url}\" }}")),
            )],
            vec![],
            CliOverrides::default(),
            "@types/no-deps",
        ),
        (
            vec![("project/.npmrc", format!("registry={url}\n"))],
            vec![],
            CliOverrides::default(),
            "no-deps",
        ),
        (
            vec![("project/.npmrc", format!("@types:registry={url}\n"))],
            vec![],
            CliOverrides::default(),
            "@types/no-deps",
        ),
        (vec![], vec![], registry_cli(&url), "no-deps"),
        (
            vec![],
            vec![("BUN_CONFIG_REGISTRY", url.clone())],
            CliOverrides::default(),
            "no-deps",
        ),
    ];
    for (files, env, cli, dep) in sources {
        let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let cfg = Fx::new(&files).load_with(&env, cli);
        assert_eq!(
            auth_of(&cfg, dep),
            Some(expected.clone()),
            "{files:?} {env:?}"
        );
    }
}

#[test]
fn lone_username_is_no_credential() {
    let cfg = Fx::new(&[(
        "project/bunfig.toml",
        &format!("[install.registry]\nurl = \"{REG}\"\nusername = \"config-precedence\"\n"),
    )])
    .load();
    assert_eq!(auth_of(&cfg, "no-deps"), None);
    let cfg = Fx::new(&[(
        "project/.npmrc",
        &format!("registry={REG}\n//localhost:4873/:username=config-precedence\n"),
    )])
    .load();
    assert_eq!(auth_of(&cfg, "no-deps"), None);
}

#[test]
fn registry_userinfo_replaces_npmrc_token_for_same_host() {
    let npmrc = format!("registry={REG}\n//localhost:4873/:_authToken=token-from-npmrc\n");
    let url = with_userinfo("config-precedence:verysecure");
    let fx = Fx::new(&[("project/.npmrc", &npmrc)]);
    assert_eq!(
        auth_of(
            &fx.load_with(&[], registry_cli(&url)),
            "@needs-auth/test-pkg"
        ),
        Some(basic(BASIC_CP.0, BASIC_CP.1))
    );
    assert_eq!(
        auth_of(
            &fx.load_with(&[("BUN_CONFIG_REGISTRY", &url)], CliOverrides::default()),
            "@needs-auth/test-pkg"
        ),
        Some(basic(BASIC_CP.0, BASIC_CP.1))
    );
}

#[test]
fn percent_encoded_userinfo_is_decoded_everywhere() {
    let url = with_userinfo("percent%2Duser:p%40ss%3Aw%25rd");
    let expected = basic("percent-user", "p@ss:w%rd");
    let cases: Vec<(Owned, Owned, CliOverrides)> = vec![
        (
            vec![(
                "project/bunfig.toml",
                bunfig(&format!("registry = \"{url}\"")),
            )],
            vec![],
            CliOverrides::default(),
        ),
        (
            vec![(
                "project/bunfig.toml",
                format!(
                    "[install]\nregistry = \"{DEAD}\"\n[install.scopes]\nneeds-auth = {{ url = \"{url}\" }}\n"
                ),
            )],
            vec![],
            CliOverrides::default(),
        ),
        (
            vec![("project/.npmrc", format!("registry={url}\n"))],
            vec![],
            CliOverrides::default(),
        ),
        (vec![], vec![], registry_cli(&url)),
        (
            vec![],
            vec![("BUN_CONFIG_REGISTRY", url.clone())],
            CliOverrides::default(),
        ),
    ];
    for (files, env, cli) in cases {
        let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let cfg = Fx::new(&files).load_with(&env, cli);
        assert_eq!(
            auth_of(&cfg, "@needs-auth/test-pkg"),
            Some(expected.clone())
        );
    }
    let encoded: String = TOKEN.bytes().map(|b| format!("%{b:02x}")).collect();
    let cfg = Fx::new(&[(
        "project/bunfig.toml",
        &bunfig(&format!(
            "registry = \"{}\"",
            with_userinfo(&format!(":{encoded}"))
        )),
    )])
    .load();
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn token_env_beats_home_npmrc_token() {
    let fx = Fx::new(&[(
        "home/.npmrc",
        &format!("registry={REG}\n//localhost:4873/:_authToken=token-from-npmrc\n"),
    )]);
    let cfg = fx.load_with(&[("BUN_CONFIG_TOKEN", TOKEN)], CliOverrides::default());
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    for key in [
        "FNPM_CONFIG_TOKEN",
        "BUN_CONFIG_TOKEN",
        "NPM_CONFIG_TOKEN",
        "npm_config_token",
    ] {
        let cfg = Fx::new(&[]).load_with(&[(key, TOKEN)], registry_cli(REG));
        assert_eq!(
            auth_of(&cfg, "@needs-auth/test-pkg"),
            Some(format!("Bearer {TOKEN}")),
            "{key}"
        );
    }
    let err = Fx::new(&[])
        .try_load_in(
            "project",
            &[("BUN_CONFIG_TOKEN", "a\nb")],
            &[],
            CliOverrides::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("$BUN_CONFIG_TOKEN contains a newline")
    );
}

#[test]
fn cli_registry_drops_npmrc_token_of_replaced_registry_but_keeps_token_env() {
    let fx = Fx::new(&[(
        "project/.npmrc",
        &format!("registry={DEAD}\n//localhost:1/:_authToken=token-for-dead\n"),
    )]);
    let cfg = fx.load_with(&[("BUN_CONFIG_TOKEN", TOKEN)], registry_cli(REG));
    assert_eq!(cfg.registry.url, REG);
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    let cfg = fx.load_with(&[], registry_cli(REG));
    assert_eq!(auth_of(&cfg, "@needs-auth/test-pkg"), None);
}

#[test]
fn npm_config_registry_beats_npmrc_registry() {
    let cfg = Fx::new(&[("project/.npmrc", &format!("registry={DEAD}\n"))])
        .load_with(&[("NPM_CONFIG_REGISTRY", REG)], CliOverrides::default());
    assert_eq!(cfg.registry.url, REG);
}

fn last_layers(url: &str) -> Vec<(Vec<(&'static str, String)>, CliOverrides)> {
    vec![
        (
            vec![("BUN_CONFIG_REGISTRY", url.to_owned())],
            CliOverrides::default(),
        ),
        (
            vec![("NPM_CONFIG_REGISTRY", url.to_owned())],
            CliOverrides::default(),
        ),
        (vec![], registry_cli(url)),
    ]
}

#[test]
fn scope_with_empty_url_is_at_the_final_default_registry() {
    for (env, cli) in last_layers(REG) {
        let fx = Fx::new(&[(
            "project/.npmrc",
            &format!("registry={DEAD}\n@types:registry=${{EMPTY_URL}}\n"),
        )]);
        let mut env: Vec<(&str, String)> = env;
        env.push(("EMPTY_URL", String::new()));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let cfg = fx.load_with(&env, cli);
        assert_eq!(cfg.registry_for("@types/no-deps").url, REG);
        assert!(!cfg.scopes.contains_key("@types"));
    }
}

#[test]
fn scope_with_token_and_no_url_stays_at_configured_registry_when_default_moves() {
    let configured = "http://localhost:5001/";
    let other = "http://localhost:5002/";
    for (env, cli) in last_layers(other) {
        let fx = Fx::new(&[(
            "project/bunfig.toml",
            &format!(
                "[install]\nregistry = \"{configured}\"\n[install.scopes]\nneeds-auth = {{ token = \"{TOKEN}\" }}\n"
            ),
        )]);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let cfg = fx.load_with(&env, cli);
        assert_eq!(cfg.registry.url, other);
        assert_eq!(cfg.registry_for("@needs-auth/test-pkg").url, configured);
        assert_eq!(
            auth_of(&cfg, "@needs-auth/test-pkg"),
            Some(format!("Bearer {TOKEN}"))
        );
        assert_eq!(auth_of(&cfg, "no-deps"), None);
        assert_eq!(cfg.authorization_header(&format!("{other}no-deps")), None);
    }
}

#[test]
fn scope_with_token_and_no_url_keeps_token_when_default_names_same_registry() {
    for (env, cli) in last_layers(REG) {
        let fx = Fx::new(&[(
            "project/bunfig.toml",
            &format!(
                "[install]\nregistry = \"{REG}\"\n[install.scopes]\nneeds-auth = {{ token = \"{TOKEN}\" }}\n"
            ),
        )]);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let cfg = fx.load_with(&env, cli);
        assert_eq!(
            auth_of(&cfg, "@needs-auth/test-pkg"),
            Some(format!("Bearer {TOKEN}"))
        );
        assert_eq!(auth_of(&cfg, "no-deps"), None);
    }
}

#[test]
fn scope_with_empty_url_gets_default_registry_credentials() {
    let fx = Fx::new(&[
        ("project/.npmrc", "@needs-auth:registry=\n"),
        (
            "project/bunfig.toml",
            &format!("[install.registry]\nurl = \"{REG}\"\ntoken = \"{TOKEN}\"\n"),
        ),
    ]);
    let cfg = fx.load();
    assert_eq!(cfg.registry_for("@needs-auth/test-pkg").url, REG);
    assert_eq!(
        auth_of(&cfg, "@needs-auth/test-pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
}

#[test]
fn cli_registry_must_be_http() {
    let err = Fx::new(&[])
        .try_load_in("project", &[], &[], registry_cli("ftp://u:secret@h/"))
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Registry URL must start with 'https://' or 'http://'"),
        "{msg}"
    );
    assert!(!msg.contains("secret"), "{msg}");
}

#[test]
fn credentials_carry_over_rules() {
    // Same host, deeper path: the token follows.
    let fx = Fx::new(&[(
        "project/bunfig.toml",
        &format!("[install.registry]\nurl = \"https://h.example/npm/\"\ntoken = \"{TOKEN}\"\n"),
    )]);
    let cfg = fx.load_with(
        &[("BUN_CONFIG_REGISTRY", "https://h.example/npm/sub/")],
        CliOverrides::default(),
    );
    assert_eq!(auth_of(&cfg, "x"), Some(format!("Bearer {TOKEN}")));
    // A sibling path does not get it.
    let cfg = fx.load_with(
        &[("BUN_CONFIG_REGISTRY", "https://h.example/other/")],
        CliOverrides::default(),
    );
    assert_eq!(auth_of(&cfg, "x"), None);
    // Nor an https -> http downgrade.
    let cfg = fx.load_with(
        &[("BUN_CONFIG_REGISTRY", "http://h.example/npm/")],
        CliOverrides::default(),
    );
    assert_eq!(auth_of(&cfg, "x"), None);
    // Nor another host.
    let cfg = fx.load_with(&[], registry_cli("https://evil.example/npm/"));
    assert_eq!(auth_of(&cfg, "x"), None);
}

#[test]
fn credentials_never_cross_origins() {
    let fx = Fx::new(&[(
        "project/.npmrc",
        &format!(
            "registry=https://r.example/\n//r.example/:_authToken={TOKEN}\n//cdn.example/:_authToken=CDN\n"
        ),
    )]);
    let cfg = fx.load();
    assert_eq!(
        cfg.authorization_header("https://r.example/pkg"),
        Some(format!("Bearer {TOKEN}"))
    );
    // A tarball on another host gets that host's own line (https only).
    assert_eq!(
        cfg.authorization_header("https://cdn.example/pkg.tgz"),
        Some("Bearer CDN".to_owned())
    );
    assert_eq!(cfg.authorization_header("http://cdn.example/pkg.tgz"), None);
    assert_eq!(
        cfg.authorization_header("https://other.example/pkg.tgz"),
        None
    );
    assert_eq!(cfg.authorization_header("ftp://r.example/pkg.tgz"), None);
    assert_eq!(cfg.authorization_header("not a url"), None);
}
