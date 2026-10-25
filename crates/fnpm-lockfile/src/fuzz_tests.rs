//! Mutation fuzzing: corrupted lockfiles must produce errors, never panics, in the reader and
//! in everything that consumes what it accepted (writer, hoisting, sidecar, yarn printer).

use std::path::{Path, PathBuf};

use crate::ParseOptions;

/// xorshift64, so a failing seed reproduces.
pub(crate) struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

fn fixtures() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bun-lock");
    let mut v = Vec::new();
    for dir in ["snapshots", "files"] {
        for e in std::fs::read_dir(root.join(dir)).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "lock") {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

const TOKENS: &[&str] = &[
    r#""""#,
    "[]",
    "{}",
    r#""a""#,
    "1",
    "0",
    "-1",
    "4294967296",
    "1e400",
    "true",
    "null",
    ",",
    ":",
    r#""workspace:""#,
    r#""link:""#,
    r#""file:""#,
    r#""github:a/b#c""#,
    r#""git+https://x/y#z""#,
    r#""@""#,
    r#""@a""#,
    r#""@a/""#,
    r#""a@""#,
    r#""a/b/c""#,
    r#""../x""#,
    r#""/abs""#,
    r#""*""#,
    r#""sha512-""#,
    r#""npm:""#,
    r#""catalog:""#,
    r#""a>b""#,
    r#""@x/y>@z/w""#,
    r#""\u0000""#,
    r#""\ud800""#,
    r#""node_modules""#,
    r#""a/node_modules/b""#,
    r#""packages""#,
    r#""lockfileVersion""#,
    r#""é/ü""#,
];

/// Applies one to three random line or token edits.
pub(crate) fn mutate(src: &str, rng: &mut Rng) -> String {
    let mut s = src.to_owned();
    for _ in 0..1 + rng.below(3) {
        let lines: Vec<&str> = s.lines().collect();
        let n = lines.len().max(1);
        s = match rng.below(6) {
            0 => {
                let i = rng.below(n);
                lines
                    .iter()
                    .enumerate()
                    .filter(|&(j, _)| j != i)
                    .map(|(_, l)| *l)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            1 => {
                let i = rng.below(n);
                let mut v = lines.clone();
                if i < v.len() {
                    v.insert(i, lines[i]);
                }
                v.join("\n")
            }
            2 => {
                let (i, j) = (rng.below(n), rng.below(n));
                let mut v = lines.clone();
                if i < v.len() && j < v.len() {
                    v.swap(i, j);
                }
                v.join("\n")
            }
            _ => {
                let b = s.as_bytes();
                let mut quotes: Vec<usize> = Vec::new();
                let mut i = 0;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'"' {
                        quotes.push(i);
                    }
                    i += 1;
                }
                if quotes.len() < 2 {
                    return s;
                }
                let k = rng.below(quotes.len() / 2) * 2;
                let (start, end) = (quotes[k], quotes[k + 1] + 1);
                let tok = TOKENS[rng.below(TOKENS.len())];
                let tok = if rng.below(3) == 0 {
                    // Keep the string but splice the token's text into it.
                    let inner = &s[start + 1..end - 1];
                    let count = inner.chars().count().max(1);
                    let cut = inner
                        .char_indices()
                        .map(|(i, _)| i)
                        .nth(rng.below(count))
                        .unwrap_or(0);
                    format!("\"{}{}\"", &inner[..cut], tok.trim_matches('"'))
                } else {
                    tok.to_owned()
                };
                format!("{}{}{}", &s[..start], tok, &s[end..])
            }
        };
    }
    s
}

fn exercise(src: &str) {
    let Ok((graph, _)) = crate::text_parse(src.as_bytes(), "fnpm.lock", &ParseOptions::default())
    else {
        return;
    };
    let text = crate::formats::fnpm::serialize(&graph);
    let _ = crate::formats::bun_text::serialize(&graph);
    let _ = crate::tree::try_hoist(
        &graph,
        &crate::tree::HoistOptions::all(fnpm_types::Platform::current()),
    );
    if let Ok(bytes) = crate::sidecar::encode(1, &graph) {
        assert!(
            crate::sidecar::decode(&bytes, 1).is_some(),
            "sidecar does not decode"
        );
    }
    let _ = crate::print::yarn_v1(&graph, &Default::default());
    let _ = crate::hash::meta_hash(&graph);
    let _ = crate::diff(&graph, &graph);
    // What the writer produces must load again, to the same bytes. bun's reader takes any
    // string as a workspace member's name but rejects an unsafe one in `packages`, so such a
    // lockfile cannot round-trip in bun either.
    // The same holds for dependency names that are not folder names (bun aborts the install).
    let splits_back = |name: &str| {
        fnpm_types::name::is_safe_install_folder_name(name)
            && fnpm_types::name::split_name_and_maybe_version(&format!("{name}@workspace:x")).0
                == name
    };
    let bad_member_name = graph
        .workspaces
        .values()
        .any(|id| id.0 != 0 && !splits_back(&graph.packages[id.0 as usize].name));
    let bad_dependency_name = graph.packages.iter().any(|p| {
        p.dependencies
            .iter()
            .any(|e| !fnpm_types::name::is_safe_install_folder_name(&e.name))
    });
    if bad_member_name || bad_dependency_name {
        return;
    }
    match crate::text_parse(text.as_bytes(), "fnpm.lock", &ParseOptions::default()) {
        Ok((again, _)) => {
            let text2 = crate::formats::fnpm::serialize(&again);
            assert!(
                text == text2,
                "written lockfile is not a fixpoint:\n{text}\n----\n{text2}"
            );
        }
        Err(e) => panic!("written lockfile does not read back: {e}\n{text}"),
    }
}

fn run(iterations: usize, seed: u64) {
    let files = fixtures();
    let mut rng = Rng(seed);
    let mut failures = Vec::new();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        for _ in 0..iterations {
            let m = mutate(&src, &mut rng);
            if let Err(e) = std::panic::catch_unwind(|| exercise(&m)) {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_default();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                failures.push((name, msg, m));
            }
        }
    }
    std::panic::set_hook(hook);
    if !failures.is_empty() {
        let dir = std::env::temp_dir().join("fnpm-lockfile-fuzz");
        let _ = std::fs::create_dir_all(&dir);
        let mut report = String::new();
        for (i, (f, msg, m)) in failures.iter().enumerate() {
            let _ = std::fs::write(dir.join(format!("{i}.lock")), m);
            let first = msg.lines().next().unwrap_or("");
            report.push_str(&format!("{i}: {f}: {first}\n"));
        }
        let _ = std::fs::write(dir.join("report.txt"), &report);
        panic!(
            "{} panics (inputs in {}):\n{report}",
            failures.len(),
            dir.display()
        );
    }
}

#[test]
fn mutated_lockfiles_never_panic() {
    run(6, 0x9e37_79b9_7f4a_7c15);
}

#[test]
#[ignore = "long fuzzing run; FNPM_FUZZ_SEED picks the seed"]
fn mutated_lockfiles_never_panic_long() {
    let seed = std::env::var("FNPM_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    run(1000, seed);
}
