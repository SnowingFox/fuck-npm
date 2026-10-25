//! Fixtures shared by the integration tests: a temp root with `home/` and `project/`, the
//! environment bun's tests use (`HOME` = `XDG_CONFIG_HOME` = `<root>/home`), and helpers that
//! read the `Authorization` header a request would carry.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use base64::Engine as _;
use fnpm_config::{Auth, CliOverrides, Config, Env, LoadOptions};

pub const REG: &str = "http://localhost:4873/";
pub const DEAD: &str = "http://localhost:1/";
pub const TOKEN: &str = "npm_test_token";

pub fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

pub fn basic(u: &str, p: &str) -> String {
    format!("Basic {}", b64(&format!("{u}:{p}")))
}

pub fn bunfig(body: &str) -> String {
    format!("[install]\n{body}\n")
}

pub fn auth_line(host: &str) -> String {
    format!("//{host}/:_authToken={TOKEN}\n")
}

pub struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    pub fn new(files: &[(&str, &str)]) -> Self {
        let fx = Fx {
            dir: tempfile::tempdir().unwrap(),
        };
        fx.write("home/.keep", "");
        fx.write(
            "project/package.json",
            r#"{"name":"config-precedence","version":"1.0.0"}"#,
        );
        for (path, text) in files {
            fx.write(path, text);
        }
        fx
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    pub fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    pub fn env(&self, extra: &[(&str, &str)], unset: &[&str]) -> Env {
        let home = self.path("home").display().to_string();
        let mut pairs: Vec<(String, String)> = vec![
            ("HOME".into(), home.clone()),
            ("USERPROFILE".into(), home.clone()),
            ("XDG_CONFIG_HOME".into(), home),
        ];
        pairs.retain(|(k, _)| !unset.contains(&k.as_str()));
        pairs.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        Env::from_pairs(pairs)
    }

    pub fn try_load_in(
        &self,
        cwd: &str,
        env: &[(&str, &str)],
        unset: &[&str],
        cli: CliOverrides,
    ) -> fnpm_config::Result<Config> {
        Config::load(&LoadOptions {
            cwd: self.path(cwd),
            cli,
            env: self.env(env, unset),
            ..Default::default()
        })
    }

    pub fn load_in(
        &self,
        cwd: &str,
        env: &[(&str, &str)],
        unset: &[&str],
        cli: CliOverrides,
    ) -> Config {
        self.try_load_in(cwd, env, unset, cli).unwrap()
    }

    pub fn load_with(&self, env: &[(&str, &str)], cli: CliOverrides) -> Config {
        self.load_in("project", env, &[], cli)
    }

    pub fn load(&self) -> Config {
        self.load_with(&[], CliOverrides::default())
    }
}

pub fn manifest_url(cfg: &Config, pkg: &str) -> String {
    format!("{}{}", cfg.registry_for(pkg).url, pkg.replace('/', "%2f"))
}

/// The `Authorization` header a manifest request for `pkg` carries.
pub fn auth_of(cfg: &Config, pkg: &str) -> Option<String> {
    cfg.auth_for_package_url(pkg, &manifest_url(cfg, pkg))
        .and_then(Auth::header_value)
}

pub fn registry_cli(url: &str) -> CliOverrides {
    CliOverrides {
        registry: Some(url.to_owned()),
        ..Default::default()
    }
}
