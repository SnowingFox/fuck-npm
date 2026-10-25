//! Ported from bun `test/cli/install/npmrc.test.ts` (the `loadNpmrc` cases) and
//! `bun-install-registry.test.ts` scope/auth parsing.
use super::*;
use crate::auth::base64_encode;

#[derive(Debug, PartialEq, Eq, Default)]
struct Loaded {
    url: String,
    token: String,
    username: String,
    password: String,
    auth: String,
}

/// bun's `iniInternals.loadNpmrc`: the default registry with the `.npmrc` line its URL walks to.
fn load_npmrc_env(ini: &str, env: &[(&str, &str)]) -> Loaded {
    let env = Env::from_pairs(env.iter().copied());
    let mut warnings = Vec::new();
    let install = settings_from_text(ini, &env, &mut warnings);
    let mut registry = install
        .registry
        .clone()
        .unwrap_or_else(|| RegistrySource::with_url(crate::DEFAULT_REGISTRY));
    let key = RegistryKey::from_url(&registry.url);
    let found = key
        .walk()
        .find_map(|k| install.url_auth.iter().find(|(e, _)| e == k))
        .map(|(_, c)| c.clone());
    if let Some(found) = found
        && (!registry.has_credentials() || registry.declared_in_npmrc)
    {
        registry.token = found.token;
        registry.auth = found.auth;
        registry.username = found.username;
        registry.password = found.password;
    }
    Loaded {
        url: registry.url,
        token: registry.token,
        username: registry.username,
        password: registry.password,
        auth: registry.auth,
    }
}

fn load_npmrc(ini: &str) -> Loaded {
    load_npmrc_env(ini, &[])
}

fn token(ini: &str) -> String {
    load_npmrc(ini).token
}

fn b64(s: &str) -> String {
    base64_encode(s.as_bytes())
}

#[test]
fn npm_config_bool_coercion() {
    for (v, expected) in [
        ("", true),
        ("true", true),
        ("false", false),
        ("null", false),
        ("undefined", false),
        ("0", false),
        ("1", true),
        ("0.0", false),
        ("0x0", false),
        ("0x10", true),
        ("-0", false),
        ("abc", true),
        ("  false  ", false),
        ("Infinity", true),
        ("0b0", false),
    ] {
        assert_eq!(npm_config_bool(v), expected, "{v:?}");
    }
    assert_eq!(npmrc_bool(&IniValue::Number(0.0)), Some(false));
    assert_eq!(npmrc_bool(&IniValue::Null), Some(false));
    assert_eq!(
        npmrc_bool(&IniValue::Object(IniObject::default())),
        Some(true)
    );
    assert_eq!(npmrc_bool(&IniValue::Array(vec![])), None);
}

#[test]
fn flat_entries() {
    let rc = Npmrc::parse(
        "a = 1\nlist[] = x\nlist[] = y\n[sec]\nk = v\nflag\n",
        &Env::default(),
    )
    .unwrap();
    assert_eq!(rc.get("a"), Some("1"));
    assert_eq!(rc.get_all("list").collect::<Vec<_>>(), ["x", "y"]);
    assert_eq!(rc.get("list"), Some("y"));
    assert_eq!(rc.get("sec.k"), Some("v"));
    assert_eq!(rc.get("sec.flag"), Some("true"));
    let rc = Npmrc::parse("\u{feff}registry=https://r/\n", &Env::default()).unwrap();
    assert_eq!(rc.get("registry"), Some("https://r/"));
}

#[test]
fn default_and_scoped_registry() {
    let mut w = Vec::new();
    let i = settings_from_text(
        "registry = http://localhost:1234/\n  @types:registry=http://localhost:5/\n",
        &Env::default(),
        &mut w,
    );
    assert_eq!(i.registry.as_ref().unwrap().url, "http://localhost:1234/");
    assert!(i.registry.as_ref().unwrap().declared_in_npmrc);
    assert_eq!(
        i.scopes.as_ref().unwrap()["types"].url,
        "http://localhost:5/"
    );
    assert!(w.is_empty());
}

#[test]
fn default_registry_from_env_expansion() {
    let l = load_npmrc_env(
        "registry = http://localhost:${PORT}/\n",
        &[("PORT", "4873")],
    );
    assert_eq!(l.url, "http://localhost:4873/");
    let l = load_npmrc_env(
        "registry = \"http://localhost:${PORT}/\"\n",
        &[("PORT", "4873")],
    );
    assert_eq!(l.url, "http://localhost:4873/");
}

#[test]
fn credentials_for_default_registry() {
    let l = load_npmrc("\n//registry.npmjs.org/:_authToken=skibidi\n");
    assert_eq!(
        (l.url.as_str(), l.token.as_str()),
        ("https://registry.npmjs.org/", "skibidi")
    );
    let l = load_npmrc(&format!(
        "//registry.npmjs.org/:username=zorp\n//registry.npmjs.org/:_password={}\n",
        b64("skibidi")
    ));
    assert_eq!(
        (l.username.as_str(), l.password.as_str()),
        ("zorp", "skibidi")
    );
}

#[test]
fn same_host_different_paths() {
    let ini = "\nregistry=https://somehost.com/org1/npm/registry/\n//somehost.com/org1/npm/registry/:_authToken=jwt1\n//somehost.com/org2/npm/registry/:_authToken=jwt2\n//somehost.com/org3/npm/registry/:_authToken=jwt3\n";
    let l = load_npmrc(ini);
    assert_eq!(l.url, "https://somehost.com/org1/npm/registry/");
    assert_eq!(l.token, "jwt1");
    assert_eq!(
        token(
            "registry=https://somehost.com/org1/npm/registry/\n//somehost.com/org2/npm/registry/:_authToken=jwt2\n"
        ),
        ""
    );
}

#[test]
fn bracketed_ipv6_hosts() {
    for (url, key) in [
        ("http://[::1]:4873/", "//[::1]:4873/"),
        ("http://[::1]/", "//[::1]/"),
        (
            "http://[2001:db8::1]:4873/npm/registry/",
            "//[2001:db8::1]:4873/npm/registry/",
        ),
        ("http://[::1]:4873/", "//[::1]:4873"),
        ("http://[::1]:4873/npm/registry/", "//[::1]:4873/"),
        ("http://[::80]/", "//[::80]/"),
    ] {
        let l = load_npmrc(&format!("registry={url}\n{key}:_authToken=v6-token\n"));
        assert_eq!(
            l,
            Loaded {
                url: url.to_owned(),
                token: "v6-token".to_owned(),
                ..Default::default()
            },
            "{url} {key}"
        );
    }
    let l = load_npmrc(&format!(
        "registry=http://[::1]:4873/\n//[::1]:4873/:username=v6-user\n//[::1]:4873/:_password={}\n",
        b64("v6-password")
    ));
    assert_eq!(
        (l.username.as_str(), l.password.as_str()),
        ("v6-user", "v6-password")
    );
    let auth = b64("v6-user:v6-password");
    assert_eq!(
        load_npmrc(&format!(
            "registry=http://[::1]:4873/\n//[::1]:4873/:_auth={auth}\n"
        ))
        .auth,
        auth
    );
    for key in ["//[::1]:4874/", "//[::2]:4873/", "//[::1]:4873/other/"] {
        assert_eq!(
            token(&format!(
                "registry=http://[::1]:4873/\n{key}:_authToken=v6-token\n"
            )),
            "",
            "{key}"
        );
    }
}

#[test]
fn protocol_relative_registry() {
    for (url, key) in [
        ("//registry.example.com/", "//registry.example.com/"),
        (
            "//registry.example.com:8080/npm/",
            "//registry.example.com:8080/npm/",
        ),
        (
            "//registry.example.com:8080/npm/",
            "//registry.example.com:8080/npm",
        ),
        (
            "//registry.example.com:8080/npm/",
            "//registry.example.com:8080/",
        ),
    ] {
        let l = load_npmrc(&format!("registry={url}\n{key}:_authToken=rel-token\n"));
        assert_eq!(
            l,
            Loaded {
                url: url.to_owned(),
                token: "rel-token".to_owned(),
                ..Default::default()
            }
        );
    }
    for key in [
        "//registry.example.com:9090/npm/",
        "//other.example.com:8080/npm/",
        "//registry.example.com:8080/other/",
    ] {
        assert_eq!(
            token(&format!(
                "registry=//registry.example.com:8080/npm/\n{key}:_authToken=rel-token\n"
            )),
            ""
        );
    }
    for (prefix, url) in [
        ("http://", "http://registry.example.com/npm/"),
        ("//", "registry.example.com/npm/"),
    ] {
        let l = load_npmrc(&format!(
            "registry={prefix}:embedded-token@registry.example.com/npm/\n"
        ));
        assert_eq!(
            l,
            Loaded {
                url: url.to_owned(),
                token: "embedded-token".to_owned(),
                ..Default::default()
            }
        );
        let l = load_npmrc(&format!(
            "registry={prefix}embedded-user:embedded-password@registry.example.com/npm/\n"
        ));
        assert_eq!(
            l,
            Loaded {
                url: url.to_owned(),
                username: "embedded-user".to_owned(),
                password: "embedded-password".to_owned(),
                ..Default::default()
            }
        );
    }
}

#[test]
fn short_hosts() {
    for host in ["a", "ab"] {
        let l = load_npmrc(&format!(
            "registry=http://{host}/\n//{host}/:_authToken={host}-token\n"
        ));
        assert_eq!(l.token, format!("{host}-token"));
    }
}

#[test]
fn credentials_written_into_registry_url() {
    let carol = b64("carol:");
    for (url, username, password, auth) in [
        (
            "http://carol:s3cret@example.com/npm/",
            "carol",
            "s3cret",
            "",
        ),
        ("http://carol@example.com/npm/", "", "", carol.as_str()),
        ("http://carol:@example.com/npm/", "", "", carol.as_str()),
    ] {
        let l = load_npmrc(&format!("registry={url}\n"));
        assert_eq!(
            l,
            Loaded {
                url: "http://example.com/npm/".to_owned(),
                username: username.to_owned(),
                password: password.to_owned(),
                auth: auth.to_owned(),
                ..Default::default()
            }
        );
    }
    let l = load_npmrc("registry=http://:s3cret@example.com/npm/\n");
    assert_eq!(
        (l.url.as_str(), l.token.as_str()),
        ("http://example.com/npm/", "s3cret")
    );
    assert_eq!(
        load_npmrc("registry=http://example.com/npm\n").url,
        "http://example.com/npm"
    );
}

#[test]
fn path_segment_ancestors() {
    let reg = "registry=https://somehost.com/org1/npm/registry/\n";
    assert_eq!(
        token(&format!("{reg}//somehost.com/:_authToken=root\n")),
        "root"
    );
    assert_eq!(
        token(&format!("{reg}//somehost.com/org1/:_authToken=mid\n")),
        "mid"
    );
    let shallow_first = format!(
        "{reg}//somehost.com/:_authToken=root\n//somehost.com/org1/:_authToken=mid\n//somehost.com/org1/npm/registry/:_authToken=exact\n"
    );
    let deep_first = format!(
        "{reg}//somehost.com/org1/npm/registry/:_authToken=exact\n//somehost.com/org1/:_authToken=mid\n//somehost.com/:_authToken=root\n"
    );
    assert_eq!(token(&shallow_first), "exact");
    assert_eq!(token(&deep_first), "exact");
    for line in [
        "//somehost.com/api/v4/projects/12/:_authToken=attacker",
        "//somehost.com/api/v4/projects/12:_authToken=attacker",
    ] {
        assert_eq!(
            token(&format!(
                "registry=https://somehost.com/api/v4/projects/123/packages/npm/\n{line}\n"
            )),
            ""
        );
    }
    let l = load_npmrc(&format!(
        "{reg}//somehost.com/:_auth={}\n",
        b64("bilbo:verysecure")
    ));
    assert_eq!(
        (l.auth, l.username, l.password),
        (b64("bilbo:verysecure"), String::new(), String::new())
    );
    let l = load_npmrc(&format!(
        "{reg}//somehost.com/:username=bilbo\n//somehost.com/:_password={}\n",
        b64("verysecure")
    ));
    assert_eq!(
        (l.username.as_str(), l.password.as_str()),
        ("bilbo", "verysecure")
    );
}

#[test]
fn npmrc_line_replaces_url_credentials() {
    let l =
        load_npmrc("registry=https://:TOK@somehost.com/\n//somehost.com/:_auth=not-valid-base64\n");
    assert_eq!(
        (l.token.as_str(), l.auth.as_str()),
        ("", "not-valid-base64")
    );
    let l = load_npmrc(&format!(
        "registry=https://:TOK@somehost.com/\n//somehost.com/:username=gandalf\n//somehost.com/:_password={}\n",
        b64("verysecure")
    ));
    assert_eq!(
        (l.token.as_str(), l.username.as_str(), l.password.as_str()),
        ("", "gandalf", "verysecure")
    );
}

#[test]
fn key_authority_normalised_like_whatwg() {
    assert_eq!(
        token("registry=https://Registry.Example.COM/api/\n//registry.example.com/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://Registry.Example.COM/api/\n//Registry.Example.COM/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://registry.example.com/api/\n//Registry.Example.COM/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://example.com/API/\n//example.com/api/:_authToken=T\n"),
        ""
    );
    assert_eq!(
        token("registry=https://example.com:443/api/\n//example.com/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=http://example.com:80/api/\n//example.com/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://example.com:8443/api/\n//example.com:8443/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://example.com:8443/api/\n//example.com/:_authToken=T\n"),
        ""
    );
    assert_eq!(
        token("registry=HTTPS://example.com:443/api/\n//example.com/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://example.com/r/\n//example.com/r/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://example.com/r/\n//example.com/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=http://example.com:443/api/\n//example.com:443/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=http://example.com:8080/api/\n//example.com/:_authToken=T\n"),
        ""
    );
    for (registry, key, expected) in [
        (
            "https://example.com:443/api/",
            "//example.com:443/api/",
            "T",
        ),
        ("https://example.com:443/api/", "//example.com:443/", "T"),
        ("https://example.com/api/", "//example.com:443/", "T"),
        ("https://example.com/api/", "//example.com:443", "T"),
        ("http://example.com:80/api/", "//example.com:80/api/", "T"),
        ("http://example.com/api/", "//example.com:80/", "T"),
        ("https://[::1]/api/", "//[::1]:443/", "T"),
        ("http://example.com/api/", "//example.com:443/", ""),
        ("https://example.com/api/", "//example.com:80/", ""),
        ("https://example.com:8443/api/", "//example.com:443/", ""),
        ("https://example.com/api/", "//example.com:4430/", ""),
        ("https://example.com/api/", "//example.com:443/other/", ""),
    ] {
        assert_eq!(
            token(&format!("registry={registry}\n{key}:_authToken=T\n")),
            expected,
            "{registry} {key}"
        );
    }
    let r = "registry=https://example.com/a/b/\n";
    assert_eq!(
        token(&format!(
            "{r}//example.com/a/:_authToken=plain\n//example.com:443/a/b/:_authToken=port\n"
        )),
        "port"
    );
    assert_eq!(
        token(&format!(
            "{r}//example.com/a/b/:_authToken=plain\n//example.com:443/a/:_authToken=port\n"
        )),
        "plain"
    );
    assert_eq!(
        token(&format!(
            "{r}//example.com:443/a/:_authToken=port\n//example.com/a/:_authToken=plain\n"
        )),
        "plain"
    );
    assert_eq!(
        token("registry=http://localhost:4873/\n//http://localhost:4873/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://example.com/api/\n//https://example.com/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=http://example.com/api/\n//http://example.com:80/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=http://example.com/api/\n//HTTP://Example.COM/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://[::1]/\n//https://[::1]:443/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=http://[::80]/\n//[::80]/:_authToken=T\n"),
        "T"
    );
    assert_eq!(
        token("registry=https://example.com/\n//example.com/:_authtoken=T\n"),
        ""
    );
    assert_eq!(
        load_npmrc("registry=https://example.com/\n//example.com/:_authtoken=T\n").auth,
        ""
    );
}

#[test]
fn credential_precedence_per_key() {
    let host = "h.example";
    let reg = format!("registry=https://{host}/api/v4/\n");
    let pw = b64("verysecure");
    let frodo = b64("frodo:onering");
    // A split pair authenticates with neither.
    assert_eq!(
        load_npmrc(&format!(
            "{reg}//{host}/:username=gandalf\n//{host}/api/v4/:_password={pw}\n"
        )),
        Loaded {
            url: format!("https://{host}/api/v4/"),
            ..Default::default()
        }
    );
    // `_authToken` > `_auth` > `username` + `_password`, regardless of line order.
    assert_eq!(
        token(&format!(
            "{reg}//{host}/:_auth={frodo}\n//{host}/:_authToken=TOKEN\n"
        )),
        "TOKEN"
    );
    let l = load_npmrc(&format!(
        "{reg}//{host}/:username=gandalf\n//{host}/:_password={pw}\n//{host}/:_auth={frodo}\n"
    ));
    assert_eq!((l.auth.as_str(), l.username.as_str()), (frodo.as_str(), ""));
    // A deeper username without a password does not shadow a shallower token.
    assert_eq!(
        token(&format!(
            "{reg}//{host}/:_authToken=ROOT\n//{host}/api/v4/:username=gandalf\n"
        )),
        "ROOT"
    );
    // A deeper email is not a credential.
    assert_eq!(
        token(&format!(
            "{reg}//{host}/:_authToken=ROOT\n//{host}/api/v4/:email=g@example.com\n"
        )),
        "ROOT"
    );
    // certfile / keyfile are warned about and ignored.
    let mut w = Vec::new();
    let i = settings_from_text(
        &format!(
            "{reg}//{host}/:_authToken=ROOT\n//{host}/api/v4/:certfile=a.pem\n//{host}/api/v4/:keyfile=b.key\n"
        ),
        &Env::default(),
        &mut w,
    );
    assert_eq!(i.url_auth.len(), 1);
    assert!(w.iter().any(|m| m.contains("certfile is not supported")));
    assert!(w.iter().any(|m| m.contains("keyfile is not supported")));
    // The trailing slash makes a distinct key.
    assert_eq!(
        token(&format!(
            "{reg}//{host}/api/v4/:_authToken=SLASH\n//{host}/api/v4:_authToken=NOSLASH\n"
        )),
        "SLASH"
    );
    assert_eq!(
        token(&format!(
            "{reg}//{host}/:_authToken=SLASH\n//{host}:_authToken=BARE\n"
        )),
        "SLASH"
    );
    assert_eq!(token(&format!("{reg}//{host}:_authToken=BARE\n")), "BARE");
    // An empty value after the collapse clears an earlier one.
    assert_eq!(
        token(&format!(
            "{reg}//{host}/api/v4/:_authToken=A\n//{host}/api/v4/:_authToken=\n"
        )),
        ""
    );
}

#[test]
fn unknown_and_misspelt_options() {
    let mut w = Vec::new();
    settings_from_text(
        "registry=https://example.com/\n//example.com/:_authtoken=SECRETTOKEN\n",
        &Env::default(),
        &mut w,
    );
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("ignoring unknown .npmrc option _authtoken"));
    assert!(!w[0].contains("SECRETTOKEN"));
    let mut w = Vec::new();
    settings_from_text(
        "registry=https://example.com/\n//example.com/:always-auth=true\n//example.com/:tokenHelper=/x\n//example.com/:cafile=/etc/ca.pem\n//127.0.0.1:1/:_authToken=true\n",
        &Env::default(),
        &mut w,
    );
    assert!(w.is_empty(), "{w:?}");
}

#[test]
fn empty_auth_diagnostics() {
    let mut w = Vec::new();
    settings_from_text(
        "registry=http://somehost.com/\n//somehost.com/:_auth=\n",
        &Env::default(),
        &mut w,
    );
    assert!(w.iter().any(|m| m.contains("ignoring empty _auth value")));
    let mut w = Vec::new();
    settings_from_text(
        "@myorg:registry=https://somehost.com/api/v4/packages/npm/\n//somehost.com/:_auth=\n",
        &Env::default(),
        &mut w,
    );
    assert!(w.is_empty());
    let mut w = Vec::new();
    settings_from_text(
        &format!(
            "registry=http://somehost.com/\n//somehost.com/:_auth=\n//somehost.com/:_auth={}\n",
            b64("a:b")
        ),
        &Env::default(),
        &mut w,
    );
    assert!(w.is_empty());
}

#[test]
fn options_bun_reads() {
    let env = Env::from_pairs([("HOME", "/home/u")]);
    let mut w = Vec::new();
    let i = settings_from_text(
        "cache=~/c\ndry-run=true\nca=PEM\nomit[]=dev\nomit[]=peer\ninclude=optional\nignore-scripts=true\nlink-workspace-packages=false\nsave-exact=1\nprovenance=true\nhoist=false\nnode-linker=pnpm\npublic-hoist-pattern[]=*eslint*\npublic-hoist-pattern[]=!foo\nhoist-pattern=*types*\nstrict-ssl=false\nproxy=http://p:1/\nhttps-proxy=http://p:2/\nnoproxy=a.com,b.com\nminimum-release-age=1440\n",
        &env,
        &mut w,
    );
    assert_eq!(i.cache_dir, Some(PathBuf::from("/home/u/c")));
    assert_eq!(i.dry_run, Some(true));
    assert_eq!(i.ca, Some(vec!["PEM".to_owned()]));
    assert_eq!(
        (i.dev, i.peer, i.optional),
        (Some(false), Some(false), Some(true))
    );
    assert_eq!(i.ignore_scripts, Some(true));
    assert_eq!(i.link_workspace_packages, Some(false));
    assert_eq!(i.exact, Some(true));
    assert_eq!(i.provenance, Some(true));
    assert_eq!(i.hoist, Some(false));
    assert_eq!(i.linker, Some(LinkerSetting::Isolated));
    assert_eq!(
        i.public_hoist_pattern,
        Some(vec!["*eslint*".to_owned(), "!foo".to_owned()])
    );
    assert_eq!(i.hoist_pattern, Some(vec!["*types*".to_owned()]));
    assert_eq!(i.strict_ssl, Some(false));
    assert_eq!(i.proxy.as_deref(), Some("http://p:1/"));
    assert_eq!(i.https_proxy.as_deref(), Some("http://p:2/"));
    assert_eq!(
        i.noproxy,
        Some(vec!["a.com".to_owned(), "b.com".to_owned()])
    );
    assert_eq!(i.minimum_release_age_ms, Some(1440 * 60_000));
    let i = settings_from_text(
        "cache=false\ninstall-strategy=hoisted\nca[]=A\nca[]=B\n",
        &env,
        &mut w,
    );
    assert_eq!(i.disable_cache, Some(true));
    assert_eq!(i.linker, Some(LinkerSetting::Hoisted));
    assert_eq!(i.ca, Some(vec!["A".to_owned(), "B".to_owned()]));
    let i = settings_from_text(
        "install-strategy=linked\nnode-linker=hoisted\n",
        &env,
        &mut w,
    );
    assert_eq!(i.linker, Some(LinkerSetting::Hoisted));
    let i = settings_from_text(
        "install-strategy=nested\nhoist-pattern='{\"a\":1}'\n",
        &env,
        &mut w,
    );
    assert_eq!(i.linker, None);
    assert_eq!(i.hoist_pattern, None);
}

#[test]
fn load_resolves_relative_paths_against_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".npmrc");
    std::fs::write(&path, "cache=../c\ncafile=ca/x.pem\n").unwrap();
    let mut w = Vec::new();
    let i = load_npmrc_files(
        &[path, dir.path().join("missing")],
        &Env::default(),
        &InstallFile::default(),
        None,
        &mut w,
    );
    assert_eq!(i.cache_dir, Some(dir.path().parent().unwrap().join("c")));
    assert_eq!(i.cafile, Some(dir.path().join("ca/x.pem")));
}

#[test]
fn keys_collapse_across_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home.npmrc");
    let project = dir.path().join("project.npmrc");
    let basic = b64("alice:s3cret");
    std::fs::write(&home, "//h/api/v4/:_authToken=HOMETOKEN\n").unwrap();
    std::fs::write(
        &project,
        format!("//h/api/v4/:_authToken=\n//h/api/v4/:_auth={basic}\n"),
    )
    .unwrap();
    let mut w = Vec::new();
    let i = load_npmrc_files(
        &[home.clone(), project.clone()],
        &Env::default(),
        &InstallFile::default(),
        None,
        &mut w,
    );
    assert_eq!(
        i.url_auth,
        vec![(
            "h/api/v4/".to_owned(),
            RegistrySource {
                auth: basic,
                ..Default::default()
            }
        )]
    );
    std::fs::write(&project, "//h/api/v4/:_authToken=PROJECT\n").unwrap();
    let i = load_npmrc_files(
        &[home, project],
        &Env::default(),
        &InstallFile::default(),
        None,
        &mut w,
    );
    assert_eq!(i.url_auth[0].1.token, "PROJECT");
}
