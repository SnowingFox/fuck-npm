//! `.env` files (bun `src/dotenv/env_loader.rs`): the parser, `$VAR` / `${VAR:-default}`
//! expansion, the default file set chosen by `NODE_ENV`, and `--env-file`.

use std::path::{Path, PathBuf};

use crate::{Env, Error, Result};

const WS: &[u8] = b"\t\x0B\x0C \n\r";

struct Parser<'a> {
    pos: usize,
    src: &'a [u8],
}

impl<'a> Parser<'a> {
    fn skip_line(&mut self) {
        match self.src[self.pos..]
            .iter()
            .position(|b| matches!(b, b'\n' | b'\r'))
        {
            Some(i) => self.pos += i + 1,
            None => self.pos = self.src.len(),
        }
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.src.len() && WS.contains(&self.src[self.pos]) {
            self.pos += 1;
        }
    }

    fn parse_key(&mut self, check_export: bool) -> Option<&'a [u8]> {
        if check_export {
            self.skip_whitespace();
        }
        let start = self.pos;
        let mut end = start;
        while end < self.src.len()
            && matches!(self.src[end], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.')
        {
            end += 1;
        }
        if end < self.src.len() && start < end {
            self.pos = end;
            self.skip_whitespace();
            if self.pos < self.src.len() {
                if check_export
                    && end < self.pos
                    && &self.src[start..end] == b"export"
                    && let Some(key) = self.parse_key(false)
                {
                    return Some(key);
                }
                match self.src[self.pos] {
                    b'=' => {
                        self.pos += 1;
                        return Some(&self.src[start..end]);
                    }
                    b':' if self.src.get(self.pos + 1).is_some_and(|b| WS.contains(b)) => {
                        self.pos += 2;
                        return Some(&self.src[start..end]);
                    }
                    _ => {}
                }
            }
        }
        self.pos = start;
        None
    }

    /// The quoted value starting at `self.pos`, without its quotes. `None` if it never closes.
    fn parse_quoted(&mut self, quote: u8) -> Option<Vec<u8>> {
        let start = self.pos;
        let mut end = start + 1;
        while end < self.src.len() {
            match self.src[end] {
                b'\\' => end += 1,
                q if q == quote => {
                    end += 1;
                    // The first unescaped closing quote ends the value; the rest of the line
                    // is discarded (node's dotenv).
                    self.pos = end;
                    self.skip_line();
                    let mut out = Vec::with_capacity(end - start);
                    let mut i = start;
                    while i < end {
                        match self.src[i] {
                            b'\\' if quote == b'"' => {
                                match self.src.get(i + 1) {
                                    Some(b'n') => out.push(b'\n'),
                                    Some(b'r') => out.push(b'\r'),
                                    Some(&c) => out.extend_from_slice(&[b'\\', c]),
                                    None => out.push(b'\\'),
                                }
                                i += 2;
                            }
                            b'\r' => {
                                i += 1;
                                if i >= end || self.src[i] != b'\n' {
                                    out.push(b'\n');
                                }
                            }
                            c => {
                                out.push(c);
                                i += 1;
                            }
                        }
                    }
                    if out.len() >= 2 {
                        out.remove(0);
                        out.pop();
                    }
                    return Some(out);
                }
                _ => {}
            }
            end += 1;
        }
        None
    }

    fn parse_value(&mut self) -> Vec<u8> {
        let start = self.pos;
        self.skip_whitespace();
        if self.pos >= self.src.len() {
            return Vec::new();
        }
        let quote = self.src[self.pos];
        if matches!(quote, b'`' | b'"' | b'\'')
            && let Some(v) = self.parse_quoted(quote)
        {
            return v;
        }
        let mut end = start;
        while end < self.src.len() && !matches!(self.src[end], b'#' | b'\r' | b'\n') {
            end += 1;
        }
        self.pos = end;
        let v = &self.src[start..end];
        let a = v.iter().position(|b| !WS.contains(b)).unwrap_or(v.len());
        let b = v.iter().rposition(|b| !WS.contains(b)).map_or(a, |i| i + 1);
        v[a..b].to_vec()
    }
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Left-to-right expansion of `$NAME`, `${NAME}` and `${NAME:-default}`; `\$` is a literal
/// `$`. Returns `false` when nothing was expanded.
fn expand_into(
    lookup: &dyn Fn(&str) -> Option<String>,
    value: &[u8],
    out: &mut Vec<u8>,
    depth: u8,
) -> bool {
    let mut pos = 0;
    let mut changed = false;
    while pos < value.len() {
        let b = value[pos];
        if b == b'\\' && value.get(pos + 1) == Some(&b'$') {
            out.push(b'$');
            pos += 2;
            changed = true;
            continue;
        }
        if b != b'$' || pos + 1 >= value.len() {
            out.push(b);
            pos += 1;
            continue;
        }
        let next = value[pos + 1];
        if next == b'{' {
            let inner_start = pos + 2;
            let mut i = inner_start;
            let mut nest = 1usize;
            let close = loop {
                if i >= value.len() {
                    break None;
                }
                match value[i] {
                    b'\\' if i + 1 < value.len() => i += 2,
                    b'$' if value.get(i + 1) == Some(&b'{') => {
                        nest += 1;
                        i += 2;
                    }
                    b'}' => {
                        nest -= 1;
                        if nest == 0 {
                            break Some(i);
                        }
                        i += 1;
                    }
                    _ => i += 1,
                }
            };
            let Some(close) = close else {
                out.extend_from_slice(&value[pos..]);
                break;
            };
            changed = true;
            let inner = &value[inner_start..close];
            let key_end = inner
                .iter()
                .position(|&c| !is_ident(c))
                .unwrap_or(inner.len());
            let key = String::from_utf8_lossy(&inner[..key_end]);
            let rest = &inner[key_end..];
            if rest.is_empty() {
                if let Some(v) = lookup(&key) {
                    out.extend_from_slice(v.as_bytes());
                }
            } else if let Some(default) = rest.strip_prefix(b":-") {
                match lookup(&key) {
                    Some(v) => out.extend_from_slice(v.as_bytes()),
                    None if depth < 200 => {
                        expand_into(lookup, default, out, depth + 1);
                    }
                    None => out.extend_from_slice(default),
                }
            } else {
                out.extend_from_slice(&value[pos..=close]);
            }
            pos = close + 1;
            continue;
        }
        if is_ident(next) {
            changed = true;
            let key_start = pos + 1;
            let mut k = key_start;
            while k < value.len() && is_ident(value[k]) {
                k += 1;
            }
            if let Some(v) = lookup(&String::from_utf8_lossy(&value[key_start..k])) {
                out.extend_from_slice(v.as_bytes());
            }
            pos = k;
            continue;
        }
        out.push(b'$');
        pos += 1;
    }
    changed
}

/// Parses one `.env` document into `env`. Variables already set (by the process or an
/// earlier file) are kept; a key repeated within this file takes its last value. Values
/// are expanded once the whole file is read.
pub fn load_into(env: &mut Env, text: &str) {
    let src = text.strip_prefix('\u{feff}').unwrap_or(text).as_bytes();
    let mut parser = Parser { pos: 0, src };
    let mut added: Vec<String> = Vec::new();
    while parser.pos < src.len() {
        let Some(key) = parser.parse_key(true) else {
            parser.skip_line();
            continue;
        };
        let key = String::from_utf8_lossy(key).into_owned();
        let value = String::from_utf8_lossy(&parser.parse_value()).into_owned();
        if env.get(&key).is_some() && !added.contains(&key) {
            continue;
        }
        if !added.contains(&key) {
            added.push(key.clone());
        }
        env.set(key, value);
    }
    for key in added {
        let Some(current) = env.get(&key).map(str::to_owned) else {
            continue;
        };
        if current.len() < 2 {
            continue;
        }
        let lookup = |name: &str| env.get(name).map(str::to_owned);
        let mut out = Vec::new();
        if expand_into(&lookup, current.as_bytes(), &mut out, 0) {
            env.set(key, String::from_utf8_lossy(&out).into_owned());
        }
    }
}

/// Which default set of `.env` files is read: `BUN_ENV`, then `NODE_ENV`; anything but
/// `development` and `test` is production.
fn suffix(env: &Env) -> &'static str {
    match env
        .get_not_empty("BUN_ENV")
        .or_else(|| env.get_not_empty("NODE_ENV"))
    {
        Some("development") => "development",
        Some("test") => "test",
        _ => "production",
    }
}

/// The environment the config (and lifecycle scripts) see: `env` plus the `.env` files.
/// With `files` (`--env-file`, relative to `cwd`, comma-separated, the last one wins) only
/// those are read; a missing one is an error. Otherwise, unless `disabled`
/// (`--no-env-file`), `<dir>/.env.<mode>.local`, `.env.local` (not for `test`),
/// `.env.<mode>`, `.env`, where an earlier file wins over a later one.
pub fn with_env_files(
    env: &Env,
    dir: Option<&Path>,
    cwd: &Path,
    files: &[PathBuf],
    disabled: bool,
) -> Result<Env> {
    let mut out = env.clone();
    if !files.is_empty() {
        for arg in files.iter().rev() {
            let arg = arg.to_string_lossy();
            for part in arg.trim_matches(' ').rsplit(',').filter(|p| !p.is_empty()) {
                let path = cwd.join(part);
                let text = std::fs::read(&path).map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })?;
                load_into(&mut out, &String::from_utf8_lossy(&text));
            }
        }
        return Ok(out);
    }
    let Some(dir) = dir.filter(|_| !disabled) else {
        return Ok(out);
    };
    let mode = suffix(env);
    let mut names = vec![format!(".env.{mode}.local")];
    if mode != "test" {
        names.push(".env.local".to_owned());
    }
    names.push(format!(".env.{mode}"));
    names.push(".env".to_owned());
    for name in names {
        // Like bun, a file that is missing, a directory or unreadable is skipped.
        if let Ok(text) = std::fs::read(dir.join(&name)) {
            load_into(&mut out, &String::from_utf8_lossy(&text));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str, base: &[(&str, &str)]) -> Env {
        let mut env = Env::from_pairs(base.iter().copied());
        load_into(&mut env, text);
        env
    }

    #[test]
    fn values() {
        let e = parse(
            "A=1\nexport B = two \nC: three\nD=\"q\\nline\"\nE='single $A'\nF=`tick`\nG=x # comment\nH=\"a\" trailing\n# c\nI\nJ=\n",
            &[],
        );
        assert_eq!(e.get("A"), Some("1"));
        assert_eq!(e.get("B"), Some("two"));
        assert_eq!(e.get("C"), Some("three"));
        assert_eq!(e.get("D"), Some("q\nline"));
        assert_eq!(e.get("E"), Some("single 1"));
        assert_eq!(e.get("F"), Some("tick"));
        assert_eq!(e.get("G"), Some("x"));
        assert_eq!(e.get("H"), Some("a"));
        assert_eq!(e.get("I"), None);
        assert_eq!(e.get("J"), Some(""));
    }

    #[test]
    fn expansion_and_precedence() {
        let e = parse(
            "A=$HOME/x\nB=${MISSING:-def}\nC=\\$HOME\nD=${HOME}${HOME}\nPRE=file\nR=1\nR=2\n",
            &[("HOME", "/h"), ("PRE", "proc")],
        );
        assert_eq!(e.get("A"), Some("/h/x"));
        assert_eq!(e.get("B"), Some("def"));
        assert_eq!(e.get("C"), Some("$HOME"));
        assert_eq!(e.get("D"), Some("/h/h"));
        assert_eq!(e.get("PRE"), Some("proc"));
        assert_eq!(e.get("R"), Some("2"));
    }

    #[test]
    fn files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "A=env\nB=env\nC=env\n").unwrap();
        std::fs::write(dir.path().join(".env.local"), "A=local\n").unwrap();
        std::fs::write(dir.path().join(".env.production"), "B=prod\n").unwrap();
        std::fs::write(dir.path().join(".env.test"), "B=test\n").unwrap();
        let e = with_env_files(&Env::default(), Some(dir.path()), dir.path(), &[], false).unwrap();
        assert_eq!(
            (e.get("A"), e.get("B"), e.get("C")),
            (Some("local"), Some("prod"), Some("env"))
        );
        let e = with_env_files(
            &Env::from_pairs([("NODE_ENV", "test")]),
            Some(dir.path()),
            dir.path(),
            &[],
            false,
        )
        .unwrap();
        assert_eq!((e.get("A"), e.get("B")), (Some("env"), Some("test")));
        let e = with_env_files(&Env::default(), Some(dir.path()), dir.path(), &[], true).unwrap();
        assert_eq!(e.get("A"), None);
        std::fs::write(dir.path().join("x.env"), "A=x\n").unwrap();
        std::fs::write(dir.path().join("y.env"), "A=y\n").unwrap();
        let e = with_env_files(
            &Env::default(),
            Some(dir.path()),
            dir.path(),
            &["x.env,y.env".into()],
            false,
        )
        .unwrap();
        assert_eq!((e.get("A"), e.get("C")), (Some("y"), None));
        assert!(
            with_env_files(
                &Env::default(),
                None,
                dir.path(),
                &["nope.env".into()],
                false
            )
            .is_err()
        );
    }
}
