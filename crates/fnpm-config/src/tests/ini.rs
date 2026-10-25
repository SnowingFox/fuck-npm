//! Ported from bun `test/js/bun/ini/ini.test.ts` (and npm's `env-replace.js` cases).
use super::*;

fn s(v: &str) -> IniValue {
    IniValue::String(v.to_owned())
}

fn obj(entries: Vec<(&str, IniValue)>) -> IniValue {
    let mut o = IniObject::default();
    for (k, v) in entries {
        o.put(k.to_owned(), v);
    }
    IniValue::Object(o)
}

fn parse_env(text: &str, env: &[(&str, &str)]) -> IniValue {
    IniValue::Object(parse(text, &Env::from_pairs(env.iter().copied())))
}

fn p(text: &str) -> IniValue {
    parse_env(text, &[])
}

#[test]
fn weird_section() {
    assert_eq!(
        p("\n[foo\\]]\nlol = true\n"),
        obj(vec![
            ("[foo\\]]", IniValue::Bool(true)),
            ("lol", IniValue::Bool(true))
        ])
    );
}

#[test]
fn really_long_input() {
    let a = "a".repeat(1024);
    let got = p(&format!("\n[{a}.lol.this.be.long]\nwow = 'hi'\n"));
    let expected = obj(vec![(
        &a,
        obj(vec![(
            "lol",
            obj(vec![(
                "this",
                obj(vec![(
                    "be",
                    obj(vec![("long", obj(vec![("wow", s("hi"))]))]),
                )]),
            )]),
        )]),
    )]);
    assert_eq!(got, expected);
}

#[test]
fn section_rope_is_capped() {
    let header = vec!["x"; MAX_SECTION_ROPE_SEGMENTS + 10].join(".");
    let IniValue::Object(root) = p(&format!("[{header}]\nk=v\n")) else {
        panic!()
    };
    let mut depth = 0;
    let mut cur = &root;
    while let Some((_, IniValue::Object(next))) = cur.iter().next() {
        depth += 1;
        cur = next;
    }
    assert_eq!(depth, MAX_SECTION_ROPE_SEGMENTS + 1);
}

fn env_case(ini: &str, env: &[(&str, &str)], key: &str, expected: &str) {
    let IniValue::Object(o) = parse_env(ini, env) else {
        panic!()
    };
    assert_eq!(o.get(key), Some(&s(expected)), "{ini:?}");
}

#[test]
fn env_vars_npm_env_replace() {
    env_case(
        "hi = ${FOO}${BAR}",
        &[("FOO", "bar"), ("BAR", "baz")],
        "hi",
        "barbaz",
    );
    env_case("hi = ${FOO?}${BAZ?}", &[("FOO", "bar")], "hi", "bar");
    env_case("hi = \\${FOO}", &[("FOO", "bar")], "hi", "${FOO}");
    env_case("hi = \\\\${FOO}", &[("FOO", "bar")], "hi", "\\bar");
    env_case("hi = \\\\\\${FOO}", &[("FOO", "bar")], "hi", "\\${FOO}");
    env_case("hi = ${BAZ}", &[("FOO", "bar")], "hi", "${BAZ}");
    env_case("hi = \\${BAZ}", &[("FOO", "bar")], "hi", "${BAZ}");
    env_case("hi = \\\\${BAZ}", &[("FOO", "bar")], "hi", "\\${BAZ}");
    env_case("hi = \\${FOO?}", &[("FOO", "bar")], "hi", "${FOO?}");
    env_case("hi = \\\\${FOO?}", &[("FOO", "bar")], "hi", "\\bar");
    env_case("hi = ${BAZ?}", &[("FOO", "bar")], "hi", "");
    env_case("hi = \\${BAZ?}", &[("FOO", "bar")], "hi", "${BAZ?}");
    env_case("hi = \\\\${BAZ?}", &[("FOO", "bar")], "hi", "\\");
}

#[test]
fn env_vars_bun_cases() {
    let ne = [("NODE_ENV", "production")];
    env_case("hi = \\${NODE_ENV}", &ne, "hi", "${NODE_ENV}");
    env_case("hi = \\\\${NODE_ENV}", &ne, "hi", "\\production");
    let IniValue::Object(o) =
        p("filepath=C:\\Home\\someuser\\My Documents\nfilepath2=\\\\\\\\TwoBackslashes")
    else {
        panic!()
    };
    assert_eq!(
        o.get("filepath"),
        Some(&s("C:\\Home\\someuser\\My Documents"))
    );
    assert_eq!(o.get("filepath2"), Some(&s("\\\\TwoBackslashes")));
    let lol = [("LOL", "hi")];
    env_case("\nhello = ${LOL}\n      ", &lol, "hello", "hi");
    env_case(
        "hello = ${oooooooooooooooogaboga}",
        &[],
        "hello",
        "${oooooooooooooooogaboga}",
    );
    env_case("hello = greeting: ${LOL}", &lol, "hello", "greeting: hi");
    env_case(
        "hello = greeting: ${what${LOL}lol}",
        &lol,
        "hello",
        "greeting: ${whathilol}",
    );
    env_case(
        "hello = greeting: ${what${omg${LOL}why}lol}",
        &lol,
        "hello",
        "greeting: ${what${omghiwhy}lol}",
    );
    env_case("hello = greeting: ${LOL", &lol, "hello", "greeting: ${LOL");
    env_case("hello = \"${LOL}\"", &lol, "hello", "hi");
    env_case("hello = '${LOL}'", &lol, "hello", "hi");
    env_case(
        "hello = \"Bearer ${TOKEN}\"",
        &[("TOKEN", "secret123")],
        "hello",
        "Bearer secret123",
    );
    env_case("hello = \"${NOTFOUND}\"", &[], "hello", "${NOTFOUND}");
    env_case("hello = ${NOTFOUND?}", &[], "hello", "");
    env_case(
        "hello = ${TOKEN?}",
        &[("TOKEN", "secret")],
        "hello",
        "secret",
    );
    env_case("hello = \"${NOTFOUND?}\"", &[], "hello", "");
    env_case(
        "hello = \"${TOKEN?}\"",
        &[("TOKEN", "secret")],
        "hello",
        "secret",
    );
    env_case("hello = '${NOTFOUND?}'", &[], "hello", "");
    env_case("hello = Bearer ${TOKEN?}", &[], "hello", "Bearer ");
    env_case("hello = \"Bearer ${TOKEN?}\"", &[], "hello", "Bearer ");
    // In JSON strings `\\` is one backslash, so `"\\${LOL}"` expands to `\hi`.
    env_case("hello = \"\\\\${LOL}\"", &lol, "hello", "\\hi");
    // An undefined inner variable leaves the whole nest as written (npm's regex).
    env_case("hello = ${a${NOPE}b}", &[], "hello", "${a${NOPE}b}");
    // Keys and sections never expand.
    let IniValue::Object(o) = parse_env("${LOL} = v\n", &lol) else {
        panic!()
    };
    assert_eq!(o.get("${LOL}"), Some(&s("v")));
}

#[test]
fn inline_comment_cut_before_env_expansion() {
    let env = [
        ("CACHE", "/tmp/cache "),
        ("PADDED", " padded  "),
        ("LOL", "hi"),
    ];
    let ini = "cache = ${CACHE}#comment\npadded = ${PADDED} ; comment\nplain = ${LOL} # comment\ninside = ${LOL;c?}\nescaped = ${LOL}\\;x ; comment\nhash = ${LOL} \\# x # comment\ntrailing = ${LOL}\\\n";
    let IniValue::Object(o) = parse_env(ini, &env) else {
        panic!()
    };
    assert_eq!(o.get("cache"), Some(&s("/tmp/cache ")));
    assert_eq!(o.get("padded"), Some(&s(" padded  ")));
    assert_eq!(o.get("plain"), Some(&s("hi")));
    assert_eq!(o.get("inside"), Some(&s("${LOL")));
    assert_eq!(o.get("escaped"), Some(&s("hi;x")));
    assert_eq!(o.get("hash"), Some(&s("hi # x")));
    assert_eq!(o.get("trailing"), Some(&s("hi\\")));
}

#[test]
fn unicode() {
    assert_eq!(
        p("\nhi👋lol = 'lol hi 👋'\n"),
        obj(vec![("hi👋lol", s("lol hi 👋"))])
    );
    assert_eq!(
        p("\n[😎.🫒.🤦‍♀️]\nlol = 'wtf'\n    "),
        obj(vec![(
            "😎",
            obj(vec![(
                "🫒",
                obj(vec![("🤦‍♀️", obj(vec![("lol", s("wtf"))]))])
            )])
        )])
    );
}

#[test]
fn npm_ini_quirks() {
    assert_eq!(
        p("\n'{ \"what\": \"is this\" }' = seriously?\n"),
        obj(vec![("[Object object]", s("seriously?"))])
    );
    assert_eq!(
        p("\n'[1, 2, 3]' = cmon man\n"),
        obj(vec![("1,2,3", s("cmon man"))])
    );
}

#[test]
fn basics() {
    assert_eq!(
        p("\n    hello = 'friends'\n    "),
        obj(vec![("hello", s("friends"))])
    );
    assert_eq!(
        p("\nhello = 'friends'\n\n[foo]\nbar = 'baz'\n    "),
        obj(vec![
            ("hello", s("friends")),
            ("foo", obj(vec![("bar", s("baz"))]))
        ])
    );
}

#[test]
fn inline_comments() {
    let ini = "\na = hello ; comment\nb = world # comment\nc = x\\;y ; z\nd = foo#bar\ne ; comment = v\nh = http://r/#frag\ni = true ;c\nj = \"q\" ;c\nk = \"a;b#c\"\n[sec ; comment]\nf = 1\n[x.y # comment]\ng = 2\n";
    assert_eq!(
        p(ini),
        obj(vec![
            ("a", s("hello")),
            ("b", s("world")),
            ("c", s("x;y")),
            ("d", s("foo")),
            ("e", s("v")),
            ("h", s("http://r/")),
            ("i", IniValue::Bool(true)),
            ("j", s("\"q\"")),
            ("k", s("a;b#c")),
            ("sec", obj(vec![("f", s("1"))])),
            ("x", obj(vec![("y", obj(vec![("g", s("2"))]))])),
        ])
    );
}

#[test]
fn unicode_whitespace_trimmed() {
    assert_eq!(
        p("a\u{a0}=\u{a0}1\u{a0}\n\u{3000}\n[s]\u{a0}\nb\u{c}=\u{b}2\n\u{feff}; comment\n"),
        obj(vec![("a", s("1")), ("s", obj(vec![("b", s("2"))]))])
    );
}

#[test]
fn key_then_section() {
    assert_eq!(
        p("\nfoo = 'hihihi'\n\n[foo]\nisbar = 'lol'\n    "),
        obj(vec![("foo", s("hihihi"))])
    );
}

#[test]
fn empty_single_quoted() {
    assert_eq!(p("a='"), obj(vec![("a", s(""))]));
    assert_eq!(p("a=''"), obj(vec![("a", s(""))]));
    assert_eq!(p("'=x"), obj(vec![("", s("x"))]));
    assert_eq!(p("''=x"), obj(vec![("", s("x"))]));
    assert_eq!(p("[']\nx=1"), obj(vec![("", obj(vec![("x", s("1"))]))]));
    assert_eq!(p("['']\nx=1"), obj(vec![("", obj(vec![("x", s("1"))]))]));
    assert_eq!(p("a=''\n[a]\nhello=world"), obj(vec![("a", s(""))]));
    assert_eq!(p("x=''"), obj(vec![("x", s(""))]));
    assert_eq!(p("='\n[]\n='"), obj(vec![("", s(""))]));
    assert_eq!(p("a='\n[a]\nb='"), obj(vec![("a", s(""))]));
    assert_eq!(p("a=''\n[a]\nb=''"), obj(vec![("a", s(""))]));
}

#[test]
fn duplicate_properties() {
    let ini = "\nzr[] = deedee\nzr=123\nar[] = one\nar[] = three\nstr = 3\nbrr = 1\nbrr = 2\nbrr = 3\nbrr = 3\n";
    assert_eq!(
        p(ini),
        obj(vec![
            ("zr", IniValue::Array(vec![s("deedee"), s("123")])),
            ("ar", IniValue::Array(vec![s("one"), s("three")])),
            ("str", s("3")),
            ("brr", s("3")),
        ])
    );
}

#[test]
fn proto_is_skipped_and_json_values_kept() {
    let IniValue::Object(o) =
        p("__proto__ = x\nn = '1'\narr = '[\"a\",\"b\"]'\nnil = null\nf = false\nbare\nempty =\n")
    else {
        panic!()
    };
    assert!(o.get("__proto__").is_none());
    assert_eq!(o.get("n"), Some(&IniValue::Number(1.0)));
    assert_eq!(o.get("arr"), Some(&IniValue::Array(vec![s("a"), s("b")])));
    assert_eq!(o.get("nil"), Some(&IniValue::Null));
    assert_eq!(o.get("f"), Some(&IniValue::Bool(false)));
    assert_eq!(o.get("bare"), Some(&IniValue::Bool(true)));
    assert_eq!(o.get("empty"), Some(&s("")));
}

#[test]
fn crlf_lines_and_unclosed_section() {
    assert_eq!(
        p("a = 1\r\nb = 2\r\n"),
        obj(vec![("a", s("1")), ("b", s("2"))])
    );
    assert_eq!(p("[unclosed\nk = v\n"), obj(vec![("k", s("v"))]));
    assert_eq!(
        p("  [indented]\n"),
        obj(vec![("[indented]", IniValue::Bool(true))])
    );
}

const FOO_INI: &str = include_str!("foo.ini");

#[test]
fn bigboi() {
    let IniValue::Object(o) = p(FOO_INI) else {
        panic!()
    };
    let get = |k: &str| o.get(k).cloned();
    assert_eq!(get(" xa  n          p "), Some(s("\"\r\nyoyoyo\r\r\n")));
    assert_eq!(get("[disturbing]"), Some(s("hey you never know")));
    assert_eq!(get("a with spaces"), Some(s("b  c")));
    assert_eq!(
        get("ar"),
        Some(IniValue::Array(vec![
            s("one"),
            s("three"),
            s("this is included")
        ]))
    );
    assert_eq!(get("b"), Some(obj(vec![])));
    assert_eq!(get("br"), Some(s("warm")));
    assert_eq!(get("eq"), Some(s("eq=eq")));
    assert_eq!(get("false"), Some(IniValue::Bool(false)));
    assert_eq!(get("null"), Some(IniValue::Null));
    assert_eq!(get("o"), Some(s("p")));
    assert_eq!(get("s"), Some(s("something")));
    assert_eq!(get("s1"), Some(s("\"something'")));
    assert_eq!(get("s2"), Some(s("something else")));
    assert_eq!(get("s3"), Some(s("")));
    assert_eq!(get("s4"), Some(s("")));
    assert_eq!(get("s5"), Some(s("   ")));
    assert_eq!(get("s6"), Some(s(" a ")));
    assert_eq!(get("s7"), Some(IniValue::Bool(true)));
    assert_eq!(get("true"), Some(IniValue::Bool(true)));
    assert_eq!(get("undefined"), Some(s("undefined")));
    assert_eq!(get("zr"), Some(IniValue::Array(vec![s("deedee")])));
    assert_eq!(
        get("a"),
        Some(obj(vec![
            ("av", s("a val")),
            (
                "e",
                s("{ o: p, a: { av: a val, b: { c: { e: \"this [value]\" } } } }")
            ),
            (
                "j",
                s("\"{ o: \"p\", a: { av: \"a val\", b: { c: { e: \"this [value]\" } } } }\"")
            ),
            ("[]", s("a square?")),
            ("cr", IniValue::Array(vec![s("four"), s("eight")])),
            (
                "b",
                obj(vec![("c", obj(vec![("e", s("1")), ("j", s("2"))]))])
            ),
        ]))
    );
    assert_eq!(
        get("x.y.z"),
        Some(obj(vec![
            ("x.y.z", s("xyz")),
            (
                "a.b.c",
                obj(vec![
                    ("a.b.c", s("abc")),
                    ("nocomment", s("this; this is not a comment")),
                    ("noHashComment", s("this# this is not a comment")),
                ])
            ),
        ]))
    );
    assert_eq!(o.len(), 23);
}

#[test]
fn invalid_utf8_is_lossy_not_a_crash() {
    for tail in [
        &[0x80u8][..],
        &[0xc0],
        &[0xe0],
        &[0xe0, 0x80],
        &[0xf0],
        &[0xf0, 0x80],
        &[0xf0, 0x80, 0x80],
    ] {
        for prefix in ["key = ", "key = \\"] {
            let mut bytes = prefix.as_bytes().to_vec();
            bytes.extend_from_slice(tail);
            let text = String::from_utf8_lossy(&bytes);
            let IniValue::Object(o) = p(&text) else {
                panic!()
            };
            assert!(o.get("key").is_some());
        }
    }
}
