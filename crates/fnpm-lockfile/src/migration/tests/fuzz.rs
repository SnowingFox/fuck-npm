//! Mutation tests: bun's fixtures with random byte edits and swapped string values must be
//! rejected or migrated, never panic, and a migrated graph must pass `LockGraph::validate`.
//! `FNPM_FUZZ_ITERS` raises the number of mutations per fixture (default 24).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use super::support::{TestSemver, empty_workspace, install_tests, snapshots, tree_from_bun_paths};
use crate::formats::{bun_binary, bun_text, npm};
use crate::migration::MigrateOptions;

/// Values that hit the edge cases of names, specs, paths, versions and integrity.
const INTERESTING: &[&str] = &[
    "",
    ".",
    "..",
    "../x",
    "../../etc",
    "/abs",
    "C:\\\\x",
    "@",
    "@/",
    "@s/",
    "a@",
    "@a@b",
    "npm:",
    "npm:x@",
    "npm:@s/x@^1",
    "workspace:",
    "workspace:../..",
    "workspace:*",
    "file:",
    "file:../..",
    "file:/",
    "link:",
    "link:..",
    "github:",
    "github:a/b#",
    "github:a/b#-x",
    "git+https://h/x.git#",
    "git+ssh://git@github.com:a/b.git#-rf",
    "git+https://h/x#../..",
    "a/b#c",
    "1.0.0",
    "0.0.0-",
    "v1",
    "=1.0.0",
    "1.0.0-beta.1+b",
    "*",
    "^",
    ">=1 <",
    "||",
    "catalog:",
    "catalog:x",
    "https://registry.npmjs.org/a/-/a-1.0.0.tgz",
    "http://localhost:1/a/-/a-1.0.0.tgz",
    "https://evil.example/a.tgz",
    "node_modules/",
    "node_modules/a/node_modules/",
    "node_modules/../x",
    "node_modules/@x",
    "sha512-",
    "sha512-AAAA",
    "sha1-xyz",
    "sha512-!!!",
    "\\u0000",
    "\\ud800",
    "é",
    "a/b/c",
    "a\\\\b",
    "-",
    "0",
    "1e400",
    "true",
];

struct Rng(u64);

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

/// The `[start, end)` ranges of the contents of every `"..."` string.
fn string_spans(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            let end = j.min(bytes.len());
            spans.push((start, end));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    spans
}

fn mutate(input: &[u8], rng: &mut Rng, text: bool) -> Vec<u8> {
    let mut out = input.to_vec();
    let rounds = 1 + rng.below(3);
    for _ in 0..rounds {
        if out.is_empty() {
            break;
        }
        match rng.below(if text { 6 } else { 4 }) {
            0 => {
                let at = rng.below(out.len());
                out[at] = rng.next() as u8;
            }
            1 => {
                let at = rng.below(out.len());
                let len = rng.below(16).min(out.len() - at);
                out.drain(at..at + len);
            }
            2 => {
                let at = rng.below(out.len());
                let len = rng.below(64).min(out.len() - at);
                let copy = out[at..at + len].to_vec();
                let to = rng.below(out.len());
                out.splice(to..to, copy);
            }
            3 => {
                // A little-endian word set to an edge value (binary offsets and lengths).
                let at = rng.below(out.len().saturating_sub(8).max(1));
                let value: u64 = match rng.below(5) {
                    0 => 0,
                    1 => u64::MAX,
                    2 => u32::MAX as u64,
                    3 => out.len() as u64,
                    _ => rng.next() % 4096,
                };
                let width = if rng.below(2) == 0 { 4 } else { 8 };
                let end = (at + width).min(out.len());
                out[at..end].copy_from_slice(&value.to_le_bytes()[..end - at]);
            }
            _ => {
                let spans = string_spans(&out);
                if spans.is_empty() {
                    continue;
                }
                let (start, end) = spans[rng.below(spans.len())];
                let value = if rng.below(2) == 0 {
                    INTERESTING[rng.below(INTERESTING.len())]
                        .as_bytes()
                        .to_vec()
                } else {
                    // Another string of the same file, so keys meet each other.
                    let (s, e) = spans[rng.below(spans.len())];
                    out[s..e].to_vec()
                };
                out.splice(start..end, value);
            }
        }
    }
    out
}

fn iterations() -> usize {
    std::env::var("FNPM_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24)
}

fn files_named(dir: &Path, pred: &dyn Fn(&str) -> bool, out: &mut Vec<Vec<u8>>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if depth > 0 && name != "node_modules" {
                files_named(&path, pred, out, depth - 1);
            }
        } else if pred(&name)
            && let Ok(bytes) = std::fs::read(&path)
            && bytes.len() < 512 * 1024
        {
            out.push(bytes);
        }
    }
}

fn check(kind: &str, seed: u64, run: impl FnOnce() -> Option<String>) -> Option<String> {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(None) => None,
        Ok(Some(problem)) => Some(format!("{kind} seed {seed}: {problem}")),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            Some(format!("{kind} seed {seed}: panic: {msg}"))
        }
    }
}

fn report(failures: Vec<String>) {
    if !failures.is_empty() {
        let shown: Vec<_> = failures.iter().take(20).cloned().collect();
        panic!("{} failures:\n{}", failures.len(), shown.join("\n"));
    }
}

#[test]
fn mutated_package_locks_never_panic() {
    let Some(install) = install_tests() else {
        return;
    };
    let mut corpus = Vec::new();
    files_named(
        &install.join("migration"),
        &|n| n == "package-lock.json" || n == "npm-shrinkwrap.json",
        &mut corpus,
        4,
    );
    assert!(corpus.len() > 20, "{} package-locks", corpus.len());
    let ws = empty_workspace();
    let options = MigrateOptions::default();
    let mut failures = Vec::new();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for (f, file) in corpus.iter().enumerate() {
        for k in 0..iterations() {
            let seed = (f as u64) << 32 | k as u64 | 1;
            let input = mutate(file, &mut Rng(seed), true);
            failures.extend(check("npm", seed, || {
                let (graph, _) =
                    npm::migrate(&input, &ws, &options, "package-lock.json", None).ok()?;
                graph
                    .validate()
                    .err()
                    .map(|e| format!("invalid graph: {e}"))
            }));
        }
    }
    std::panic::set_hook(previous);
    report(failures);
}

#[test]
fn mutated_bun_locks_never_panic() {
    let Some(install) = install_tests() else {
        return;
    };
    let mut corpus: Vec<Vec<u8>> = Vec::new();
    for snap in [
        "migration/__snapshots__/yarn-lock-migration.test.ts.snap",
        "migration/__snapshots__/pnpm-migration.test.ts.snap",
        "__snapshots__/bun-lock.test.ts.snap",
        "__snapshots__/bun-workspaces.test.ts.snap",
    ] {
        for body in snapshots(&install.join(snap)).into_values() {
            let Some(start) = body.find("{\n  \"lockfileVersion\"") else {
                continue;
            };
            let text = &body[start..];
            if let Some(end) = text.find("\n}\n") {
                corpus.push(text.as_bytes()[..end + 3].to_vec());
            }
        }
    }
    assert!(corpus.len() > 10, "{} bun.locks", corpus.len());
    let options = MigrateOptions::default();
    let mut failures = Vec::new();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for (f, file) in corpus.iter().enumerate() {
        for k in 0..iterations() {
            let seed = (f as u64) << 32 | k as u64 | 1;
            let input = mutate(file, &mut Rng(seed), true);
            failures.extend(check("bun.lock", seed, || {
                let parsed = bun_text::parse_with_semver(&input, &options, &TestSemver).ok()?;
                if let Err(e) = parsed.graph.validate() {
                    return Some(format!("invalid graph: {e}"));
                }
                let tree = tree_from_bun_paths(&parsed.graph, &parsed.paths);
                let _ = bun_text::serialize_loaded(&parsed.graph, &tree, parsed.version);
                None
            }));
        }
    }
    std::panic::set_hook(previous);
    report(failures);
}

#[test]
fn mutated_bun_lockbs_never_panic() {
    let Some(install) = install_tests() else {
        return;
    };
    let mut corpus = Vec::new();
    files_named(&install, &|n| n.contains(".lockb"), &mut corpus, 3);
    assert!(!corpus.is_empty());
    let mut failures = Vec::new();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for (f, file) in corpus.iter().enumerate() {
        for k in 0..iterations() * 8 {
            let seed = (f as u64) << 32 | k as u64 | 1;
            let input = mutate(file, &mut Rng(seed), false);
            failures.extend(check("bun.lockb", seed, || {
                let graph = bun_binary::parse(&input).ok()?;
                graph
                    .validate()
                    .err()
                    .map(|e| format!("invalid graph: {e}"))
            }));
        }
    }
    std::panic::set_hook(previous);
    report(failures);
}
