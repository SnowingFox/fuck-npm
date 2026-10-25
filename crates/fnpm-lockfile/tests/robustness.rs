//! Robustness: migrating and exporting malformed yarn.lock / pnpm-lock.yaml never panics.

mod common;
use common::*;
use fnpm_lockfile::formats::{pnpm, yarn_v1};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn lockfiles() -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = vec![];
    let mut stack = vec![fixtures()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let n = p.file_name().unwrap().to_string_lossy().to_string();
            if n == "yarn.lock" || n == "pnpm-lock.yaml" {
                let b = std::fs::read(&p).unwrap();
                if b.len() < 60000 {
                    out.push((p, b));
                }
            }
        }
    }
    out.sort();
    out
}

fn run(path: &std::path::Path, bytes: &[u8]) -> Option<String> {
    let dir = path.parent().unwrap();
    let ws = workspace(dir);
    let files = ScopedFiles(dir.to_path_buf());
    let o = options(&files);
    let is_pnpm = path.ends_with("pnpm-lock.yaml");
    let r = catch_unwind(AssertUnwindSafe(|| {
        if is_pnpm {
            let _ = pnpm::migrate(bytes, &ws, &o).map(|m| pnpm::serialize(&m.graph));
        } else {
            let _ = yarn_v1::migrate(bytes, &ws, &o).map(|m| yarn_v1::serialize(&m.graph));
        }
    }));
    r.err().map(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    })
}

/// Mutated copies of every fixture lockfile (truncation, deletion, inserted YAML and protocol
/// tokens, non-ASCII text) must fail with an error, never panic. `FUZZ_ITERS` raises the count.
#[test]
fn mutated_lockfiles_never_panic() {
    let iters: usize = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let locs = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let l2 = locs.clone();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(l) = info.location() {
            *l2.lock().unwrap() = format!("{}:{}", l.file(), l.line());
        }
    }));
    let mut seed: u64 = 0x1234567;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let toks: &[&[u8]] = &[
        "\u{e9}".as_bytes(),
        b"#",
        b"@",
        b":",
        b"(",
        b")",
        b"/",
        b"\n",
        b" ",
        b"'",
        b"\"",
        b"-",
        b"[",
        b"]",
        b"{",
        b"}",
        b"npm:",
        b"link:",
        b"file:",
        b"workspace:",
        b"patch:",
        b"git+",
        b"github:",
        b"_",
        b"..",
        b"~",
        b"*",
        b"0",
        b"\n  ",
        b"\n    ",
        b"&a",
        b"*a",
        b"!",
        b"?",
        b"%",
        b"#~",
    ];
    let mut found = std::collections::BTreeMap::new();
    for (p, b) in lockfiles() {
        for i in 0..iters {
            let mut m = b.clone();
            let n = 1 + rnd() % 4;
            for _ in 0..n {
                if m.is_empty() {
                    break;
                }
                let at = (rnd() as usize) % m.len();
                match rnd() % 4 {
                    0 => {
                        if rnd() % 4 == 0 {
                            m.truncate(at);
                        }
                    }
                    1 => {
                        let t = toks[(rnd() as usize) % toks.len()];
                        m.splice(at..at, t.iter().copied());
                    }
                    2 => {
                        let end = (at + 1 + (rnd() as usize) % 20).min(m.len());
                        m.drain(at..end);
                    }
                    _ => {
                        let t = toks[(rnd() as usize) % toks.len()];
                        let end = (at + t.len()).min(m.len());
                        m.splice(at..end, t.iter().copied());
                    }
                }
            }
            if let Some(msg) = run(&p, &m) {
                let key = format!(
                    "{} {}",
                    locs.lock().unwrap(),
                    msg.chars().take(120).collect::<String>()
                );
                found
                    .entry(key)
                    .or_insert_with(|| (p.clone(), i, m.clone()));
            }
        }
    }
    for (k, (p, i, m)) in &found {
        eprintln!(
            "PANIC {k}\n  in {} (mutation {i}):\n{}",
            p.display(),
            String::from_utf8_lossy(m)
        );
    }
    assert!(found.is_empty(), "{} distinct panics", found.len());
}
