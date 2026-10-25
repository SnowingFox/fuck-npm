//! `fnpm install --analyze <files>` / `fnpm add --analyze <files>` (bun
//! `runtime/cli/pm_update_package_json.rs`): the packages that the files import, which the
//! install then adds like positionals (with `--only-missing`).
//!
//! bun runs its bundler to find the imports. fnpm scans the source text instead: `import` and
//! `export ... from` declarations, `import()`, `require()` and `import.meta.resolve()` with a
//! string literal, skipping comments, strings and template literals. Relative imports are
//! followed into the files they name (imports inside `${}` of a template literal are not
//! seen) (with the extensions and `index` files the bundler tries),
//! so the whole local import graph is analyzed. Builtins (`node:`, `bun:`, Node's module
//! names, with or without a subpath), paths, URLs, `#imports`, and the `@/` and `~/` aliases
//! are not packages.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Node's builtin modules (`module.builtinModules`, without the `node:` prefix).
const BUILTINS: &[&str] = &[
    "_http_agent",
    "_http_client",
    "_http_common",
    "_http_incoming",
    "_http_outgoing",
    "_http_server",
    "_stream_duplex",
    "_stream_passthrough",
    "_stream_readable",
    "_stream_transform",
    "_stream_wrap",
    "_stream_writable",
    "_tls_common",
    "_tls_wrap",
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
    "bun",
];

/// The extensions the bundler tries for an import without one, in its order.
const EXTENSIONS: &[&str] = &[
    ".tsx", ".ts", ".jsx", ".js", ".mjs", ".cjs", ".mts", ".cts", ".json",
];

/// The package that `specifier` names, or `None` for a builtin, a path, a URL or an alias.
pub fn package_of(specifier: &str) -> Option<String> {
    let s = specifier.trim();
    if s.is_empty()
        || s.starts_with('.')
        || s.starts_with('/')
        || s.starts_with('\\')
        || s.starts_with('#')
        || s.starts_with("@/")
        || s.starts_with("~/")
        || s.starts_with('~')
        || s.contains(':')
    {
        return None;
    }
    let mut parts = s.split('/');
    let first = parts.next()?;
    let name = if first.starts_with('@') {
        let second = parts.next().filter(|p| !p.is_empty())?;
        format!("{first}/{second}")
    } else {
        first.to_owned()
    };
    if BUILTINS.contains(&first) {
        return None;
    }
    fnpm_types::name::is_npm_package_name(&name).then_some(name)
}

/// The string literal specifiers that `source` imports.
pub fn specifiers(source: &str) -> Vec<String> {
    let b = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // The last two significant tokens: identifiers, or a punctuation byte.
    let mut prev: Vec<String> = Vec::new();
    let push = |prev: &mut Vec<String>, t: String| {
        prev.push(t);
        if prev.len() > 4 {
            prev.remove(0);
        }
    };
    // Skip a shebang.
    if b.starts_with(b"#!") {
        while i < b.len() && b[i] != b'\n' {
            i += 1;
        }
    }
    while i < b.len() {
        let c = b[i];
        match c {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'\'' | b'"' => {
                let start = i + 1;
                i += 1;
                while i < b.len() && b[i] != c && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let text = source
                    .get(start..i.min(b.len()))
                    .unwrap_or_default()
                    .to_owned();
                i += 1;
                let last = |n: usize| {
                    prev.len()
                        .checked_sub(n)
                        .and_then(|k| prev.get(k))
                        .map(String::as_str)
                };
                let is_import = match (last(1), last(2), last(3)) {
                    (Some("from"), _, _) => true,
                    (Some("import"), _, _) => true,
                    (Some("export"), _, _) => false,
                    (Some("("), Some("import" | "require"), _) => true,
                    (Some("("), Some("resolve"), Some(".")) => true,
                    _ => false,
                };
                if is_import {
                    out.push(text);
                }
                push(&mut prev, "\"\"".into());
            }
            b'`' => {
                i += 1;
                let mut depth = 0usize;
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 1,
                        b'`' if depth == 0 => break,
                        b'$' if b.get(i + 1) == Some(&b'{') => {
                            depth += 1;
                            i += 1;
                        }
                        b'}' if depth > 0 => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
                push(&mut prev, "``".into());
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                {
                    i += 1;
                }
                // `x.import` / `obj.require` are properties, not the keyword.
                let word = &source[start..i];
                let after_dot = prev.last().is_some_and(|p| p == ".");
                if after_dot && matches!(word, "import" | "require") {
                    push(&mut prev, format!(".{word}"));
                } else {
                    push(&mut prev, word.to_owned());
                }
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                push(&mut prev, (c as char).to_string());
                i += 1;
            }
        }
    }
    out
}

/// The file that relative `specifier` from `dir` names, as the bundler resolves it.
fn resolve_relative(dir: &Path, specifier: &str) -> Option<PathBuf> {
    let base = dir.join(specifier);
    if base.is_file() {
        return Some(base);
    }
    let s = base.to_string_lossy().into_owned();
    // `./x.js` may be `./x.ts` (TypeScript's rule).
    for (from, to) in [
        (".js", ".ts"),
        (".js", ".tsx"),
        (".mjs", ".mts"),
        (".cjs", ".cts"),
    ] {
        if let Some(stem) = s.strip_suffix(from) {
            let candidate = PathBuf::from(format!("{stem}{to}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for ext in EXTENSIONS {
        let candidate = PathBuf::from(format!("{s}{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if base.is_dir() {
        for ext in EXTENSIONS {
            let candidate = base.join(format!("index{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_source(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e,
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts"
        )
    })
}

/// The `name` of the nearest package.json above `dir` that has one.
fn own_package_name(dir: &Path) -> Option<String> {
    for d in dir.ancestors() {
        let Ok(text) = std::fs::read(d.join("package.json")) else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(name) = json
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|n| !n.is_empty())
        {
            return Some(name.to_owned());
        }
    }
    None
}

/// The packages that `entries` (relative to `cwd`) and the local files they import import, in
/// name order.
pub fn packages(cwd: &Path, entries: &[String]) -> Result<Vec<String>> {
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for e in entries {
        let path = cwd.join(e);
        let Some(file) =
            resolve_relative(cwd, e).or_else(|| path.is_file().then_some(path.clone()))
        else {
            return Err(Error::usage(format!(
                "ModuleNotFound resolving \"{e}\" (entry point)"
            )));
        };
        queue.push_back(file);
    }
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut names: BTreeSet<String> = BTreeSet::new();
    while let Some(file) = queue.pop_front() {
        let file = std::fs::canonicalize(&file).unwrap_or(file);
        if !seen.insert(file.clone()) || !is_source(&file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let dir = file.parent().unwrap_or(cwd).to_path_buf();
        // A package importing itself by name (`exports` self-reference) is not a dependency.
        let own_name = own_package_name(&dir);
        for spec in specifiers(&text) {
            if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
                if let Some(next) = resolve_relative(&dir, &spec) {
                    queue.push_back(next);
                }
                continue;
            }
            if let Some(name) = package_of(&spec)
                && own_name.as_deref() != Some(name.as_str())
            {
                names.insert(name);
            }
        }
    }
    Ok(names.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_import_form() {
        let src = r#"
      import "bar";
      import debounce from "lodash/debounce";
      import { a, b as c } from 'deep/a/b.js';
      import * as ns from "@scope/pkg/button";
      export * from "re-exported/sub";
      export { x } from "re-x";
      const { createRoot } = require("react-dom/client");
      import("dynamic/sub");
      const r = import.meta.resolve("resolved");
      // import "commented";
      /* require("blocked") */
      const s = "import 'not-an-import'";
      const t = `require("templated")`;
      obj.require("method-call");
      export const y = 1;
    "#;
        let got = specifiers(src);
        assert_eq!(
            got,
            [
                "bar",
                "lodash/debounce",
                "deep/a/b.js",
                "@scope/pkg/button",
                "re-exported/sub",
                "re-x",
                "react-dom/client",
                "dynamic/sub",
                "resolved"
            ]
        );
    }

    #[test]
    fn package_names() {
        assert_eq!(package_of("lodash/debounce").as_deref(), Some("lodash"));
        assert_eq!(
            package_of("@scope/pkg/button").as_deref(),
            Some("@scope/pkg")
        );
        for not in [
            "fs",
            "fs/not-a-builtin",
            "node:path/posix",
            "bun:sqlite",
            "bun/sub",
            "@/alias",
            "@/components/ui/button",
            "~/alias",
            "#internal/thing",
            "./x",
            "../x",
            "/abs",
            "https://example.com/mod/x.js",
            "@scope",
        ] {
            assert_eq!(package_of(not), None, "{not}");
        }
    }

    #[test]
    fn follows_relative_imports() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("entry.ts"), "import './lib'; import 'a';").unwrap();
        std::fs::create_dir(t.path().join("lib")).unwrap();
        std::fs::write(t.path().join("lib/index.ts"), "export * from '../util.js';").unwrap();
        std::fs::write(t.path().join("util.ts"), "require('b/sub');").unwrap();
        assert_eq!(
            packages(t.path(), &["./entry.ts".into()]).unwrap(),
            ["a", "b"]
        );
        assert!(packages(t.path(), &["./missing.ts".into()]).is_err());
    }
}
