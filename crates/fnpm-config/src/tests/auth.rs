//! Scope building: embedded `:_authToken=` markers, `$VAR` credentials, URL userinfo
//! (bun `npm.rs` `Scope::build` and the npmrc.test.ts "credentials embedded in the registry
//! URL" cases).
use super::*;

fn build(url: &str, env: &[(&str, &str)]) -> Scope {
    let mut w = Vec::new();
    build_scope(
        CredentialSite::Registry(""),
        RegistrySource::from_url(url),
        &Env::from_pairs(env.iter().copied()),
        true,
        &mut w,
    )
    .unwrap()
}

fn basic(u: &str, p: &str) -> Auth {
    Auth::Basic {
        username: u.to_owned(),
        password: p.to_owned(),
    }
}

fn tok(t: &str) -> Auth {
    Auth::Token {
        token: t.to_owned(),
    }
}

const H: &str = "http://127.0.0.1:9";

#[test]
fn lenient_parse() {
    let u = LenientUrl::parse("https://user:pa:ss@Host.com:8080/a/b?q=1#h");
    assert_eq!(
        (
            u.protocol, u.username, u.password, u.host, u.hostname, u.port
        ),
        (
            "https",
            "user",
            "pa:ss",
            "Host.com:8080",
            "Host.com",
            "8080"
        )
    );
    assert_eq!(u.pathname, "/a/b?q=1");
    assert_eq!(u.query_free_path(), "/a/b");
    let u = LenientUrl::parse("//:tok@h/x/");
    assert_eq!(
        (u.protocol, u.password, u.host, u.pathname),
        ("", "tok", "h", "/x/")
    );
    let u = LenientUrl::parse("[::1]:4873/");
    assert_eq!(
        (u.host, u.hostname, u.port),
        ("[::1]:4873", "[::1]", "4873")
    );
    let u = LenientUrl::parse("http://h");
    assert_eq!(u.pathname, "/");
    assert_eq!(LenientUrl::parse("").pathname, "/");
}

#[test]
fn embedded_markers() {
    for (path, expected_url, expected_auth) in [
        (
            "/api/npm/_auth=YWJj/ZGVm",
            "/api/npm/",
            Auth::Legacy {
                auth: "YWJj/ZGVm".to_owned(),
            },
        ),
        ("/api/_authToken=a/b", "/api/", tok("a/b")),
        ("/api/username=u/_password=p/q", "/api/", basic("u", "p/q")),
        ("/a=b/c/", "/a=b/c/", Auth::None),
        ("/api/npm=1/", "/api/npm=1/", Auth::None),
        ("/api/:_AuthToken=T", "/api/", Auth::None),
        ("/api/:_passwd=SECRET", "/api/", Auth::None),
        ("/api/:authtoken=SECRET", "/api/", Auth::None),
        ("/a:b=c/npm/", "/a:b=c/npm/", Auth::None),
        ("/api/_v=1/npm/", "/api/_v=1/npm/", Auth::None),
        ("/api/_secret=x/", "/api/_secret=x/", Auth::None),
        ("/api/:_authToken=S", "/api/", tok("S")),
        ("/api/:_authToken=S/", "/api/", tok("S")),
        ("/a:b/c/", "/a:b/c/", Auth::None),
        ("/api/:_auth=", "/api/", Auth::None),
        ("/api/:_authToken=aa:bb", "/api/", tok("aa:bb")),
        (
            "/api/:username=u/:_password=p:q",
            "/api/",
            basic("u", "p:q"),
        ),
        ("/api/:_password=cA==:_authToken=T", "/api/", tok("T")),
        ("/api/:username=u:_authToken=T", "/api/", tok("T")),
        (
            "/api/:_authToken=T:_auth=opaque-blob",
            "/api/",
            Auth::Legacy {
                auth: "opaque-blob".to_owned(),
            },
        ),
        (
            "/api/:username=a:username=b:_password=p",
            "/api/",
            basic("a", "p"),
        ),
    ] {
        let s = build(&format!("{H}{path}"), &[]);
        assert_eq!(s.url, format!("{H}{expected_url}"), "{path}");
        assert_eq!(s.auth, expected_auth, "{path}");
    }
}

#[test]
fn userinfo_beats_embedded_but_path_is_stripped() {
    let s = build("http://u:p@127.0.0.1:9/api/:_authToken=EMBEDDED", &[]);
    assert_eq!(s.url, "http://127.0.0.1:9/api/");
    assert_eq!(s.auth, basic("u", "p"));
}

#[test]
fn explicit_token_beats_embedded_and_path_is_stripped() {
    let mut w = Vec::new();
    let mut src = RegistrySource::from_url(&format!("{H}/api/:_auth=opaque-blob"));
    src.token = "T".to_owned();
    let s = build_scope(
        CredentialSite::Registry("myorg"),
        src,
        &Env::default(),
        true,
        &mut w,
    )
    .unwrap();
    assert_eq!(
        (s.url.as_str(), s.auth.clone()),
        ("http://127.0.0.1:9/api/", tok("T"))
    );
}

#[test]
fn percent_encoded_userinfo() {
    let s = build("http://percent%2Duser:p%40ss%3Aw%25rd@localhost:1/", &[]);
    assert_eq!(s.auth, basic("percent-user", "p@ss:w%rd"));
    let s = build("http://:%61%62@localhost:1/", &[]);
    assert_eq!(s.auth, tok("ab"));
    assert_eq!(s.url, "http://localhost:1/");
    let s = build("http://carol@localhost:1/", &[]);
    assert_eq!(
        s.auth.header_value().unwrap(),
        format!("Basic {}", base64_encode(b"carol:"))
    );
}

#[test]
fn var_credentials() {
    let mut w = Vec::new();
    let env = Env::from_pairs([("TOK", "secret"), ("REG", "https://r.example/")]);
    let src = RegistrySource {
        url: "$REG".to_owned(),
        token: "$TOK".to_owned(),
        ..Default::default()
    };
    let s = build_scope(CredentialSite::Registry(""), src, &env, true, &mut w).unwrap();
    assert_eq!(
        (s.url.as_str(), s.auth.clone()),
        ("https://r.example/", tok("secret"))
    );
    let src = RegistrySource {
        url: "https://r/".to_owned(),
        token: "$UNSET".to_owned(),
        ..Default::default()
    };
    let s = build_scope(CredentialSite::Registry(""), src, &env, true, &mut w).unwrap();
    assert_eq!(s.auth, Auth::None);
    assert!(w.iter().any(|m| m.contains("unset variable $UNSET")));
    // `.npmrc` lines keep an unset `$VAR` literally.
    let src = RegistrySource {
        token: "$UNSET".to_owned(),
        ..Default::default()
    };
    let s = build_scope(CredentialSite::NpmrcLine("h/"), src, &env, false, &mut w).unwrap();
    assert_eq!(s.auth, tok("$UNSET"));
    // A `$` before something that is no variable name is a literal password.
    let src = RegistrySource {
        url: "https://r/".to_owned(),
        username: "u".to_owned(),
        password: "$ecret!".to_owned(),
        ..Default::default()
    };
    let s = build_scope(CredentialSite::Registry(""), src, &env, true, &mut w).unwrap();
    assert_eq!(s.auth, basic("u", "$ecret!"));
}

#[test]
fn refuses_header_breaking_credentials() {
    let mut w = Vec::new();
    for src in [
        RegistrySource {
            url: "https://r/".to_owned(),
            token: "a\nb".to_owned(),
            ..Default::default()
        },
        RegistrySource {
            url: "https://r/".to_owned(),
            username: "a\rb".to_owned(),
            password: "p".to_owned(),
            ..Default::default()
        },
        RegistrySource {
            url: "https://r/".to_owned(),
            auth: "a\0".to_owned(),
            ..Default::default()
        },
    ] {
        let err = build_scope(
            CredentialSite::Registry("myorg"),
            src,
            &Env::default(),
            true,
            &mut w,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("@myorg registry contains a newline or NUL byte"),
            "{err}"
        );
    }
    let src = RegistrySource {
        token: "x\n".to_owned(),
        ..Default::default()
    };
    let err = build_scope(
        CredentialSite::NpmrcLine("h/"),
        src,
        &Env::default(),
        false,
        &mut w,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("//h/:_authToken in .npmrc"),
        "{err}"
    );
}

#[test]
fn registry_key_walk() {
    let k = RegistryKey::from_url("https://h/a/");
    assert_eq!(
        k.walk().collect::<Vec<_>>(),
        [
            "h/a/", "h:443/a/", "h/a", "h:443/a", "h/", "h:443/", "h", "h:443"
        ]
    );
    let k = RegistryKey::from_url("http://H:8080/x?y=1");
    assert_eq!(
        k.walk().collect::<Vec<_>>(),
        ["h:8080/x/", "h:8080/x", "h:8080/", "h:8080"]
    );
    assert_eq!(
        RegistryKey::from_url("//r.example:8080/npm/").key(),
        "r.example:8080/npm/"
    );
}

#[test]
fn normalize_keys() {
    assert_eq!(
        normalize_key("Registry.Example.COM/Api/"),
        "registry.example.com/Api/"
    );
    assert_eq!(normalize_key("https://h:443/x/"), "h/x/");
    assert_eq!(normalize_key("http://h:80/"), "h/");
    assert_eq!(normalize_key("h:443/"), "h:443/");
    assert_eq!(normalize_key("https://[::1]:443/"), "[::1]/");
    assert_eq!(normalize_key("http://[::80]/"), "[::80]/");
}

#[test]
fn origin_and_auth_helpers() {
    assert!(same_origin("https://h/a/b.tgz", "https://h:443/r/"));
    assert!(!same_origin("http://h/a", "https://h/"));
    assert!(!same_origin("https://other/a", "https://h/"));
    assert_eq!(
        Auth::Legacy {
            auth: base64_encode(b"alice:pw")
        }
        .username()
        .as_deref(),
        Some("alice")
    );
    assert_eq!(
        Auth::Legacy {
            auth: "!!".to_owned()
        }
        .username(),
        None
    );
    assert_eq!(base64_decode_lenient("dXNl cg=="), b"user");
    assert_eq!(base64_decode_lenient("dmVyeXNlY3VyZQ"), b"verysecure");
    assert_eq!(base64_decode_lenient("!!"), b"");
    assert_eq!(
        base64_decode_lenient("_-8"),
        base64::engine::general_purpose::STANDARD
            .decode("/+8=")
            .unwrap()
    );
}
