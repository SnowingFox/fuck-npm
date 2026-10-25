//! Ported from bun `test/cli/install/npmrc.test.ts` ".npmrc auth resolves by path-segment
//! ancestor" and "credentials embedded in the registry URL": the `Authorization` header the
//! manifest request of `@myorg/pkg` (or `pkg`) would carry.

mod common;
use common::*;

use fnpm_config::CliOverrides;

const HOST: &str = "127.0.0.1:9999";
const REGISTRY_PATH: &str = "/api/v4/projects/123/packages/npm/";
const SECRET: &str = "walkup-secret-token";

#[derive(Default, Clone)]
struct Probe {
    default: bool,
    userinfo: &'static str,
    bunfig_token: Option<&'static str>,
    bunfig_basic: Option<(&'static str, &'static str)>,
    bunfig_bare: bool,
    home_npmrc: Option<String>,
    path: Option<&'static str>,
}

fn probe(lines: &str, o: Probe) -> Option<String> {
    let path = o.path.unwrap_or(REGISTRY_PATH);
    let url = format!("http://{}{HOST}{path}", o.userinfo);
    let registry_line = if o.default {
        format!("registry={url}")
    } else {
        format!("@myorg:registry={url}")
    };
    let in_bunfig = o.bunfig_token.is_some() || o.bunfig_basic.is_some() || o.bunfig_bare;
    let npmrc = format!(
        "{}{lines}\n",
        if in_bunfig {
            String::new()
        } else {
            format!("{registry_line}\n")
        }
    );
    let mut files: Vec<(&str, String)> = vec![("project/.npmrc", npmrc)];
    if let Some(home) = &o.home_npmrc {
        files.push(("home/.npmrc", format!("{home}\n")));
    }
    if in_bunfig {
        let creds = match (o.bunfig_token, o.bunfig_basic) {
            (Some(t), _) => format!(", token = \"{t}\""),
            (None, Some((u, p))) => format!(", username = \"{u}\", password = \"{p}\""),
            _ => String::new(),
        };
        let entry = format!("{{ url = \"{url}\"{creds} }}");
        let text = if o.default {
            format!("[install]\nregistry = {entry}\n")
        } else {
            format!("[install.scopes]\nmyorg = {entry}\n")
        };
        files.push(("project/bunfig.toml", text));
    }
    let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
    let cfg = Fx::new(&files).load();
    let pkg = if o.default { "pkg" } else { "@myorg/pkg" };
    let registry = cfg.registry_for(pkg);
    assert!(registry.url.ends_with(path), "{} {path}", registry.url);
    auth_of(&cfg, pkg)
}

fn line(path: &str, opt: &str, value: &str) -> String {
    format!("//{HOST}{path}:{opt}={value}")
}

fn bearer(t: &str) -> Option<String> {
    Some(format!("Bearer {t}"))
}

#[test]
fn ancestor_matrix() {
    let without_slash = &REGISTRY_PATH[..REGISTRY_PATH.len() - 1];
    let deeper = format!("{REGISTRY_PATH}deeper/");
    for (path, expected) in [
        ("/", bearer(SECRET)),
        ("/api/", bearer(SECRET)),
        ("/api/v4/projects/", bearer(SECRET)),
        (REGISTRY_PATH, bearer(SECRET)),
        (without_slash, bearer(SECRET)),
        ("/api/v4/projects/12/", None),
        ("/api/v4/projects/12", None),
        ("/api/v4/projects/123/packages/other/", None),
        (deeper.as_str(), None),
    ] {
        for default in [false, true] {
            assert_eq!(
                probe(
                    &line(path, "_authToken", SECRET),
                    Probe {
                        default,
                        ..Default::default()
                    }
                ),
                expected,
                "{path}"
            );
        }
    }
}

#[test]
fn longest_match_wins() {
    let root = line("/", "_authToken", "ROOT");
    let deep = line(REGISTRY_PATH, "_authToken", "DEEP");
    assert_eq!(
        probe(&format!("{root}\n{deep}"), Probe::default()),
        bearer("DEEP")
    );
    assert_eq!(
        probe(&format!("{deep}\n{root}"), Probe::default()),
        bearer("DEEP")
    );
    assert_eq!(
        probe(
            &format!("{}\n{root}", line("/api/v4/", "_authToken", "MID")),
            Probe::default()
        ),
        bearer("MID")
    );
    assert_eq!(
        probe(
            &format!(
                "{root}\n{}",
                line("/api/v4/projects/12/", "_authToken", "ATTACKER")
            ),
            Probe::default()
        ),
        bearer("ROOT")
    );
}

#[test]
fn walks_up_for_auth_and_basic() {
    let blob = b64("linus:verysecure");
    assert_eq!(
        probe(&line("/", "_auth", &blob), Probe::default()),
        Some(format!("Basic {blob}"))
    );
    let pw = b64("verysecure");
    assert_eq!(
        probe(
            &format!(
                "{}\n{}",
                line("/", "username", "gandalf"),
                line("/", "_password", &pw)
            ),
            Probe::default()
        ),
        Some(basic("gandalf", "verysecure"))
    );
    assert_eq!(
        probe(
            &format!("//other.example.com/:_authToken={SECRET}"),
            Probe::default()
        ),
        None
    );
}

#[test]
fn credentials_resolve_per_config_path() {
    let pw = b64("verysecure");
    let frodo = b64("frodo:onering");
    let r = REGISTRY_PATH;
    let cases: Vec<(String, Option<String>)> = vec![
        (
            format!(
                "{}\n{}",
                line("/", "username", "gandalf"),
                line(r, "_password", &pw)
            ),
            None,
        ),
        (
            format!(
                "{}\n{}",
                line(r, "username", "gandalf"),
                line("/", "_password", &pw)
            ),
            None,
        ),
        (
            format!(
                "{}\n{}\n{}",
                line("/", "_authToken", "ROOT"),
                line(r, "username", "gandalf"),
                line(r, "_password", &pw)
            ),
            Some(basic("gandalf", "verysecure")),
        ),
        (
            format!(
                "{}\n{}\n{}",
                line(r, "_authToken", "DEEP"),
                line("/", "username", "gandalf"),
                line("/", "_password", &pw)
            ),
            bearer("DEEP"),
        ),
        (
            format!(
                "{}\n{}",
                line("/", "_authToken", "ROOT"),
                line(r, "_auth", &b64("gandalf:verysecure"))
            ),
            Some(basic("gandalf", "verysecure")),
        ),
        (
            format!(
                "{}\n{}\n{}",
                line("/", "_auth", &frodo),
                line("/", "username", "gandalf"),
                line("/", "_password", &pw)
            ),
            Some(format!("Basic {frodo}")),
        ),
        (
            format!(
                "{}\n{}\n{}",
                line("/", "username", "gandalf"),
                line("/", "_password", &pw),
                line("/", "_auth", &frodo)
            ),
            Some(format!("Basic {frodo}")),
        ),
        (
            format!(
                "{}\n{}",
                line("/", "_authToken", "TOKEN"),
                line("/", "_auth", &frodo)
            ),
            bearer("TOKEN"),
        ),
        (
            format!(
                "{}\n{}",
                line("/", "_auth", &frodo),
                line("/", "_authToken", "TOKEN")
            ),
            bearer("TOKEN"),
        ),
        (
            format!(
                "{}\n{}\n{}",
                line("/", "_authToken", "TOKEN"),
                line("/", "username", "gandalf"),
                line("/", "_password", &pw)
            ),
            bearer("TOKEN"),
        ),
        (
            format!(
                "{}\n{}\n{}",
                line("/", "username", "gandalf"),
                line("/", "_password", &pw),
                line("/", "_authToken", "TOKEN")
            ),
            bearer("TOKEN"),
        ),
        (
            format!(
                "{}\n{}",
                line("/", "_authToken", "ROOT"),
                line(r, "email", "g@example.com")
            ),
            bearer("ROOT"),
        ),
        (
            format!(
                "{}\n{}",
                line("/", "_authToken", "ROOT"),
                line(r, "username", "gandalf")
            ),
            bearer("ROOT"),
        ),
        (
            format!(
                "{}\n{}\n{}",
                line("/", "username", "gandalf"),
                line("/", "_password", &pw),
                line(r, "username", "saruman")
            ),
            Some(basic("gandalf", "verysecure")),
        ),
    ];
    for (lines, expected) in cases {
        assert_eq!(probe(&lines, Probe::default()), expected, "{lines}");
    }
}

#[test]
fn trailing_slash_is_a_distinct_key() {
    let pw = b64("verysecure");
    assert_eq!(
        probe(
            &format!(
                "{}\n{}",
                line("/api/v4/", "_authToken", "SLASH"),
                line("/api/v4", "_authToken", "NOSLASH")
            ),
            Probe::default()
        ),
        bearer("SLASH")
    );
    assert_eq!(
        probe(
            &format!(
                "{}\n//{HOST}:_authToken=BARE",
                line("/", "_authToken", "SLASH")
            ),
            Probe::default()
        ),
        bearer("SLASH")
    );
    assert_eq!(
        probe(&format!("//{HOST}:_authToken=BARE"), Probe::default()),
        bearer("BARE")
    );
    let split = format!("//{HOST}:username=gandalf\n{}", line("/", "_password", &pw));
    assert_eq!(probe(&split, Probe::default()), None);
    assert_eq!(
        probe(
            &split,
            Probe {
                path: Some("/"),
                ..Default::default()
            }
        ),
        None
    );
}

#[test]
fn certfile_and_keyfile_are_ignored() {
    let r = REGISTRY_PATH;
    let pair = format!(
        "{}\n{}",
        line(r, "certfile", "a.pem"),
        line(r, "keyfile", "b.key")
    );
    assert_eq!(
        probe(
            &format!("{}\n{pair}", line("/", "_authToken", SECRET)),
            Probe::default()
        ),
        bearer(SECRET)
    );
    assert_eq!(
        probe(
            &format!(
                "{}\n{}",
                line("/", "_authToken", SECRET),
                line(r, "certfile", "a.pem")
            ),
            Probe::default()
        ),
        bearer(SECRET)
    );
    assert_eq!(
        probe(
            &format!(
                "{}\n{}\n{}",
                line("/", "certfile", "a.pem"),
                line("/", "keyfile", "b.key"),
                line(r, "_authToken", SECRET)
            ),
            Probe::default()
        ),
        bearer(SECRET)
    );
    assert_eq!(
        probe(
            &pair,
            Probe {
                bunfig_token: Some(SECRET),
                ..Default::default()
            }
        ),
        bearer(SECRET)
    );
    assert_eq!(
        probe(
            &pair,
            Probe {
                default: true,
                bunfig_token: Some(SECRET),
                ..Default::default()
            }
        ),
        bearer(SECRET)
    );
    assert_eq!(
        probe(
            &pair,
            Probe {
                default: true,
                userinfo: "user:pass@",
                ..Default::default()
            }
        ),
        Some(basic("user", "pass"))
    );
    assert_eq!(
        probe(
            &line(r, "keyfile", "b.key"),
            Probe {
                default: true,
                userinfo: "user:pass@",
                ..Default::default()
            }
        ),
        Some(basic("user", "pass"))
    );
    assert_eq!(
        probe(
            "//other.example.com/:_authToken=OTHER",
            Probe {
                bunfig_token: Some(SECRET),
                ..Default::default()
            }
        ),
        bearer(SECRET)
    );
}

#[test]
fn keys_collapse_across_files() {
    let p = "/api/v4/";
    let alice = b64("alice:s3cret");
    let home = |l: String| Probe {
        path: Some("/api/v4/"),
        home_npmrc: Some(l),
        ..Default::default()
    };
    assert_eq!(
        probe(
            &format!(
                "{}\n{}",
                line(p, "_authToken", ""),
                line(p, "_auth", &alice)
            ),
            home(line(p, "_authToken", "HOMETOKEN"))
        ),
        Some(format!("Basic {alice}"))
    );
    assert_eq!(
        probe(
            &line(p, "_authToken", "PROJECT"),
            home(line(p, "_authToken", "HOMETOKEN"))
        ),
        bearer("PROJECT")
    );
    assert_eq!(
        probe("", home(line(p, "_authToken", "HOMETOKEN"))),
        bearer("HOMETOKEN")
    );
    assert_eq!(
        probe(
            &line(p, "_authToken", "PROJECT"),
            home(line("/", "_authToken", "HOMETOKEN"))
        ),
        bearer("PROJECT")
    );
    let home_basic = format!(
        "{}\n{}",
        line(p, "username", "alice"),
        line(p, "_password", &b64("s3cret"))
    );
    assert_eq!(
        probe(&line(p, "username", ""), home(home_basic.clone())),
        None
    );
    assert_eq!(
        probe(&line(p, "_password", ""), home(home_basic.clone())),
        None
    );
    assert_eq!(
        probe(&line(p, "username", "bob"), home(home_basic)),
        Some(basic("bob", "s3cret"))
    );
}

#[test]
fn auth_is_forwarded_verbatim() {
    let r = REGISTRY_PATH;
    for blob in [
        b64("opaquetokenblob"),
        "!!not-base64!!".to_owned(),
        b64("tok:"),
        "YWI6Y2Q".to_owned(),
    ] {
        assert_eq!(
            probe(&line(r, "_auth", &blob), Probe::default()),
            Some(format!("Basic {blob}"))
        );
        let o = Probe {
            default: true,
            userinfo: "url-user:url-pass@",
            ..Default::default()
        };
        assert_eq!(
            probe(&line(r, "_auth", &blob), o),
            Some(format!("Basic {blob}"))
        );
    }
    let blob = b64("opaquetokenblob");
    let lines = format!(
        "{}\n{}\n{}",
        line(r, "_auth", &blob),
        line(r, "username", "x"),
        line(r, "_password", &b64("y"))
    );
    assert_eq!(
        probe(&lines, Probe::default()),
        Some(format!("Basic {blob}"))
    );
}

#[test]
fn lone_half_credentials_supply_nothing() {
    let stored = Probe {
        default: true,
        userinfo: "url-user:url-pass@",
        ..Default::default()
    };
    assert_eq!(
        probe(&line("/", "username", "attacker"), stored.clone()),
        Some(basic("url-user", "url-pass"))
    );
    assert_eq!(
        probe(&line("/", "_password", &b64("other")), stored),
        Some(basic("url-user", "url-pass"))
    );
    let token_url: &'static str = Box::leak(format!(":{SECRET}@").into_boxed_str());
    let o = Probe {
        default: true,
        userinfo: token_url,
        ..Default::default()
    };
    assert_eq!(
        probe(
            &format!(
                "{}\n{}",
                line("/", "username", "u"),
                line("/", "_authToken", "")
            ),
            o
        ),
        bearer(SECRET)
    );
}

#[test]
fn registries_declared_in_bunfig() {
    for default in [false, true] {
        let o = Probe {
            default,
            bunfig_bare: true,
            ..Default::default()
        };
        assert_eq!(probe(&line("/", "_authToken", SECRET), o), bearer(SECRET));
    }
    assert_eq!(
        probe(
            &line("/api/v4/projects/12/", "_authToken", SECRET),
            Probe {
                bunfig_bare: true,
                ..Default::default()
            }
        ),
        None
    );
    let blob = b64("alice:s3cret");
    assert_eq!(
        probe(
            &line("/api/", "_auth", &blob),
            Probe {
                bunfig_bare: true,
                ..Default::default()
            }
        ),
        Some(format!("Basic {blob}"))
    );
    assert_eq!(
        probe(
            &line("/", "_authToken", "FROM_NPMRC"),
            Probe {
                bunfig_token: Some(SECRET),
                ..Default::default()
            }
        ),
        bearer(SECRET)
    );
    let o = Probe {
        bunfig_basic: Some(("bunfig-user", "bunfig-pass")),
        ..Default::default()
    };
    assert_eq!(
        probe(&line(REGISTRY_PATH, "_auth", &b64("x:y")), o),
        Some(basic("bunfig-user", "bunfig-pass"))
    );
}

#[test]
fn default_registry_userinfo_and_lines() {
    let o = Probe {
        default: true,
        userinfo: "user:pass@",
        ..Default::default()
    };
    assert_eq!(
        probe(
            &format!("//other.example.com/:_authToken={SECRET}"),
            o.clone()
        ),
        Some(basic("user", "pass"))
    );
    assert_eq!(probe(&line("/", "_authToken", SECRET), o), bearer(SECRET));
}

/// `@myorg:registry=http://<host><path>` (or the default/env/bunfig/CLI spelling) with
/// `lines` in the project `.npmrc`: the request URL of `@myorg/pkg` and its header.
fn embedded(path: &str, lines: &str, userinfo: &str, source: &str) -> (String, Option<String>) {
    let url = format!("http://{userinfo}{HOST}{path}");
    let mut npmrc = String::new();
    match source {
        "scoped" => npmrc.push_str(&format!("@myorg:registry={url}\n")),
        "default" => npmrc.push_str(&format!("registry={url}\n")),
        _ => {}
    }
    npmrc.push_str(lines);
    npmrc.push('\n');
    let mut files = vec![("project/.npmrc", npmrc)];
    if source == "bunfig" {
        files.push((
            "project/bunfig.toml",
            format!("[install]\nregistry = \"{url}\"\n"),
        ));
    }
    let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
    let fx = Fx::new(&files);
    let cfg = match source {
        "env" => fx.load_with(&[("NPM_CONFIG_REGISTRY", &url)], CliOverrides::default()),
        "cli" => fx.load_with(&[], registry_cli(&url)),
        _ => fx.load(),
    };
    let request = manifest_url(&cfg, "@myorg/pkg");
    let path = request
        .splitn(4, '/')
        .nth(3)
        .map(|p| format!("/{p}"))
        .unwrap_or_default();
    (path, auth_of(&cfg, "@myorg/pkg"))
}

#[test]
fn embedded_credentials_are_stripped_from_the_path() {
    let e = |path: &str, lines: &str| embedded(path, lines, "", "scoped");
    let p = |s: &str| s.to_owned();
    assert_eq!(
        e("/api/npm/_auth=YWJj/ZGVm", ""),
        (p("/api/npm/@myorg%2fpkg"), Some("Basic YWJj/ZGVm".into()))
    );
    assert_eq!(
        e("/api/_authToken=a/b", ""),
        (p("/api/@myorg%2fpkg"), bearer("a/b"))
    );
    assert_eq!(
        e("/api/username=u/_password=p/q", ""),
        (p("/api/@myorg%2fpkg"), Some(basic("u", "p/q")))
    );
    assert_eq!(e("/a=b/c/", ""), (p("/a=b/c/@myorg%2fpkg"), None));
    assert_eq!(e("/api/npm=1/", ""), (p("/api/npm=1/@myorg%2fpkg"), None));
    assert_eq!(e("/api/:_AuthToken=T", ""), (p("/api/@myorg%2fpkg"), None));
    for path in ["/api/:_passwd=SECRET", "/api/:authtoken=SECRET"] {
        assert_eq!(e(path, ""), (p("/api/@myorg%2fpkg"), None));
    }
    for (path, expected) in [
        ("/a:b=c/npm/", "/a:b=c/npm/@myorg%2fpkg"),
        ("/api/_v=1/npm/", "/api/_v=1/npm/@myorg%2fpkg"),
        ("/api/_secret=x/", "/api/_secret=x/@myorg%2fpkg"),
    ] {
        assert_eq!(e(path, ""), (p(expected), None));
    }
    assert_eq!(
        embedded("/a:b=c/npm/", "", "", "cli"),
        (p("/a:b=c/npm/@myorg%2fpkg"), None)
    );
    for source in ["default", "env", "bunfig"] {
        assert_eq!(
            embedded("/api/:_authToken=S", "", "", source),
            (p("/api/@myorg%2fpkg"), bearer("S")),
            "{source}"
        );
    }
    assert_eq!(
        embedded("/api/:_authToken=S/", "", "", "cli"),
        (p("/api/@myorg%2fpkg"), bearer("S"))
    );
    let t_line = format!("registry=http://{HOST}/\n//{HOST}/:_authToken=T");
    assert_eq!(
        embedded("/api/:_authToken=S/", &t_line, "", "cli"),
        (p("/api/@myorg%2fpkg"), bearer("T"))
    );
    assert_eq!(
        e("/api/:_authToken=EMBEDDED", ""),
        (p("/api/@myorg%2fpkg"), bearer("EMBEDDED"))
    );
    assert_eq!(
        e(
            "/api/:_authToken=EMBEDDED",
            &format!("//{HOST}/:_authToken=FROM_NPMRC")
        ),
        (p("/api/@myorg%2fpkg"), bearer("FROM_NPMRC"))
    );
    assert_eq!(
        embedded("/api/:_authToken=EMBEDDED", "", "u:p@", "scoped"),
        (p("/api/@myorg%2fpkg"), Some(basic("u", "p")))
    );
    assert_eq!(
        e("/a:b/c/", &format!("//{HOST}/a:b/c/:_authToken=TOK")),
        (p("/a:b/c/@myorg%2fpkg"), bearer("TOK"))
    );
    assert_eq!(e("/a:b/c/", ""), (p("/a:b/c/@myorg%2fpkg"), None));
    let opaque = format!("//{HOST}/:_auth=b3BhcXVlYmxvYg");
    assert_eq!(
        e("/api/:_auth=", &opaque),
        (p("/api/@myorg%2fpkg"), Some("Basic b3BhcXVlYmxvYg".into()))
    );
    assert_eq!(
        e("/api/:_authToken=", &opaque),
        (p("/api/@myorg%2fpkg"), Some("Basic b3BhcXVlYmxvYg".into()))
    );
    assert_eq!(
        e(
            "/api/:username=embeddeduser/:_password=embeddedpass",
            &format!("//{HOST}/:_auth=T1BBUVVFQkxPQg==")
        ),
        (
            p("/api/@myorg%2fpkg"),
            Some("Basic T1BBUVVFQkxPQg==".into())
        )
    );
    assert_eq!(
        e("/api/:_authToken=aa:bb", ""),
        (p("/api/@myorg%2fpkg"), bearer("aa:bb"))
    );
    assert_eq!(
        e("/api/:username=u/:_password=p:q", ""),
        (p("/api/@myorg%2fpkg"), Some(basic("u", "p:q")))
    );
    assert_eq!(
        e("/api/:_password=cA==:_authToken=T", ""),
        (p("/api/@myorg%2fpkg"), bearer("T"))
    );
    assert_eq!(
        e("/api/:username=u:_authToken=T", ""),
        (p("/api/@myorg%2fpkg"), bearer("T"))
    );
    for value in ["opaque-blob".to_owned(), b64("u:")] {
        assert_eq!(
            e(&format!("/api/:_authToken=T:_auth={value}"), ""),
            (p("/api/@myorg%2fpkg"), Some(format!("Basic {value}")))
        );
    }
    assert_eq!(
        e("/api/:username=a:username=b:_password=p", ""),
        (p("/api/@myorg%2fpkg"), Some(basic("a", "p")))
    );
}

#[test]
fn bunfig_scope_token_beats_embedded() {
    for path in ["/api/:_auth=opaque-blob", "/api/:_authToken=EMBEDDED"] {
        let fx = Fx::new(&[(
            "project/bunfig.toml",
            &format!(
                "[install.scopes]\nmyorg = {{ url = \"http://{HOST}{path}\", token = \"T\" }}\n"
            ),
        )]);
        let cfg = fx.load();
        assert_eq!(
            manifest_url(&cfg, "@myorg/pkg"),
            format!("http://{HOST}/api/@myorg%2fpkg")
        );
        assert_eq!(auth_of(&cfg, "@myorg/pkg"), bearer("T"));
    }
}

#[test]
fn var_credentials() {
    let fx = Fx::new(&[(
        "project/bunfig.toml",
        "[install.registry]\nurl = \"https://r.example/\"\ntoken = \"$MY_TOKEN\"\n",
    )]);
    assert_eq!(
        auth_of(
            &fx.load_with(&[("MY_TOKEN", "t")], CliOverrides::default()),
            "x"
        ),
        bearer("t")
    );
    let cfg = fx.load();
    assert_eq!(auth_of(&cfg, "x"), None);
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("unset variable $MY_TOKEN"))
    );
    // An unset `$VAR` token falls back to the `.npmrc` line.
    fx.write("project/.npmrc", "//r.example/:_authToken=FROM_NPMRC\n");
    assert_eq!(auth_of(&fx.load(), "x"), bearer("FROM_NPMRC"));
    // A set one wins over it.
    assert_eq!(
        auth_of(
            &fx.load_with(&[("MY_TOKEN", "t")], CliOverrides::default()),
            "x"
        ),
        bearer("t")
    );
    // A registry written as $VAR picks up its `.npmrc` line.
    let fx = Fx::new(&[
        (
            "project/bunfig.toml",
            "[install.scopes]\nmyorg = \"$MYORG_URL\"\n",
        ),
        ("project/.npmrc", "//m.example/:_authToken=M\n"),
    ]);
    let cfg = fx.load_with(
        &[("MYORG_URL", "https://m.example/")],
        CliOverrides::default(),
    );
    assert_eq!(cfg.registry_for("@myorg/x").url, "https://m.example/");
    assert_eq!(auth_of(&cfg, "@myorg/x"), bearer("M"));
    // A `.npmrc` `_authToken` that starts with `$` and names no variable is sent verbatim.
    let fx = Fx::new(&[(
        "project/.npmrc",
        "registry=https://r.example/\n//r.example/:_authToken=$abc-def\n",
    )]);
    let cfg = fx.load();
    assert_eq!(auth_of(&cfg, "x"), bearer("$abc-def"));
    assert!(cfg.warnings.is_empty());
}

#[test]
fn header_breaking_npmrc_credentials_are_refused() {
    let fx = Fx::new(&[(
        "project/.npmrc",
        "registry=https://r.example/\n//r.example/:_authToken=\"a\\nb\"\n",
    )]);
    let err = fx
        .try_load_in("project", &[], &[], CliOverrides::default())
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("//r.example/:_authToken in .npmrc contains a newline or NUL byte"),
        "{err}"
    );
}
