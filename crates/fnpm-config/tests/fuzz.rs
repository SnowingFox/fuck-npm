//! Malformed input never panics: random text through every parser and a full load.

use fnpm_config::*;
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}
const ALPHA: &[&str] = &[
    "a",
    "Z",
    "0",
    "/",
    "//",
    ":",
    "@",
    "[",
    "]",
    "{",
    "}",
    "$",
    "${",
    "?",
    "#",
    ";",
    "=",
    "\\",
    "\"",
    "'",
    "é",
    "ü",
    "€",
    "𝄞",
    ".",
    "-",
    "_",
    " ",
    "\t",
    "\n",
    "\r",
    "%",
    "%2f",
    "https://",
    "http://",
    "_authToken=",
    "_auth=",
    "username=",
    "_password=",
    ":443",
    "~",
    "*",
    "!",
    ",",
    "\u{feff}",
    "\u{a0}",
    "`",
    "aaaaaaa",
    "[]",
];
fn rgen(r: &mut Rng, n: usize) -> String {
    let len = (r.next() % n as u64) as usize;
    (0..len)
        .map(|_| ALPHA[(r.next() % ALPHA.len() as u64) as usize])
        .collect()
}
#[test]
fn fuzz_no_panics() {
    let env = Env::from_pairs([("A", "x"), ("HOME", "/h")]);
    let mut r = Rng(0xdeadbeefcafe1234);
    let mut fails = Vec::new();
    for i in 0..20000 {
        let s = rgen(&mut r, 60);
        let res = std::panic::catch_unwind(|| {
            let _ = ini::parse(&s, &env);
            let _ = Npmrc::parse(&s, &env);
            let _ = RegistrySource::from_url(&s);
            let k = RegistryKey::from_url(&s);
            let _: Vec<_> = k.walk().collect();
            let _ = redact_url(&s);
            let _ = auth::normalize_key(&s);
            let _ = files::parse_seconds(&s);
            let _ = env.expand(&s);
            let mut e = Env::default();
            dotenv::load_into(&mut e, &s);
            let _ = files::parse_config_file(
                std::path::Path::new("/p/fnpm.toml"),
                &s,
                files::FileKind::Project,
                None,
            );
            let _ = files::parse_config_file(
                std::path::Path::new("/p/fnpm.json"),
                &s,
                files::FileKind::Project,
                None,
            );
            let mut c = Config::defaults(std::path::Path::new("/p"));
            c.url_auth
                .push(("h/".into(), Auth::Token { token: "t".into() }));
            let _ = c.auth_for_url(&s);
            let _ = c.authorization_header(&s);
        });
        if res.is_err() {
            fails.push((i, s));
            if fails.len() > 10 {
                break;
            }
        }
    }
    assert!(fails.is_empty(), "{fails:#?}");
}
#[test]
fn fuzz_load_npmrc() {
    let mut r = Rng(777);
    let mut fails = Vec::new();
    for i in 0..150 {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("package.json"), "{}").unwrap();
        let mut text = String::new();
        for _ in 0..4 {
            let pick = r.next() % 4;
            text.push_str(["//", "registry=", "@s:registry=", ""][pick as usize]);
            text.push_str(&rgen(&mut r, 16));
            text.push_str(["=:_authToken=x\n", ":_auth=\n", "\n", "=y\n"][(r.next() % 4) as usize]);
        }
        std::fs::write(proj.join(".npmrc"), &text).unwrap();
        let env = Env::from_pairs([("HOME", home.to_str().unwrap())]);
        let opts = LoadOptions {
            cwd: proj.clone(),
            env,
            ..Default::default()
        };
        let res = std::panic::catch_unwind(|| {
            if let Ok(c) = Config::load(&opts) {
                let _ = c.authorization_header("https://registry.npmjs.org/x");
                let _ = c.redacted();
            }
        });
        if res.is_err() {
            fails.push((i, text));
            if fails.len() > 5 {
                break;
            }
        }
    }
    assert!(fails.is_empty(), "{fails:#?}");
}
