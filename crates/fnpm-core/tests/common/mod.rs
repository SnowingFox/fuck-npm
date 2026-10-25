//! An in-process registry (axum) serving packuments and tarballs built from fixture
//! definitions, plus helpers to run the core against a temporary project.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use fnpm_config::Env;
use fnpm_core::options::{InstallCommand, InstallOptions, PmOptions};
use fnpm_core::{CoreRequest, CoreResult};
use fnpm_types::event::VecSink;
use fnpm_types::{Event, HashAlgorithm, InstallSummary, Integrity};
use parking_lot::Mutex;
use serde_json::{Value, json};

/// One published version.
#[derive(Debug, Clone, Default)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub deps: Vec<(String, String)>,
    pub optional: Vec<(String, String)>,
    pub peers: Vec<(String, String)>,
    pub bin: Option<(String, String)>,
    pub scripts: Vec<(String, String)>,
    /// Extra files besides package.json: `(path, contents)`.
    pub files: Vec<(String, String)>,
    pub os: Option<Vec<String>>,
}

impl Pkg {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            files: vec![(
                "index.js".into(),
                format!("module.exports = \"{name}@{version}\";\n"),
            )],
            ..Self::default()
        }
    }
    pub fn dep(mut self, name: &str, range: &str) -> Self {
        self.deps.push((name.into(), range.into()));
        self
    }
    pub fn optional(mut self, name: &str, range: &str) -> Self {
        self.optional.push((name.into(), range.into()));
        self
    }
    pub fn peer(mut self, name: &str, range: &str) -> Self {
        self.peers.push((name.into(), range.into()));
        self
    }
    pub fn bin(mut self, name: &str, path: &str) -> Self {
        self.bin = Some((name.into(), path.into()));
        self.files.push((
            path.into(),
            "#!/usr/bin/env node\nconsole.log(\"hi\");\n".into(),
        ));
        self
    }
    pub fn script(mut self, event: &str, cmd: &str) -> Self {
        self.scripts.push((event.into(), cmd.into()));
        self
    }
    pub fn os(mut self, os: &[&str]) -> Self {
        self.os = Some(os.iter().map(|s| (*s).to_owned()).collect());
        self
    }

    fn manifest(&self) -> Value {
        let map = |v: &[(String, String)]| {
            Value::Object(
                v.iter()
                    .map(|(k, r)| (k.clone(), Value::String(r.clone())))
                    .collect(),
            )
        };
        let mut m = json!({ "name": self.name, "version": self.version });
        if !self.deps.is_empty() {
            m["dependencies"] = map(&self.deps);
        }
        if !self.optional.is_empty() {
            m["optionalDependencies"] = map(&self.optional);
        }
        if !self.peers.is_empty() {
            m["peerDependencies"] = map(&self.peers);
        }
        if let Some((n, p)) = &self.bin {
            m["bin"] = json!({ n: p });
        }
        if !self.scripts.is_empty() {
            m["scripts"] = map(&self.scripts);
        }
        if let Some(os) = &self.os {
            m["os"] = json!(os);
        }
        m
    }

    /// The gzipped tarball (`package/` prefix, like npm pack).
    pub fn tgz(&self) -> Vec<u8> {
        let mut tar = Vec::new();
        let mut files = vec![(
            "package.json".to_owned(),
            serde_json::to_string_pretty(&self.manifest()).unwrap(),
        )];
        files.extend(self.files.iter().cloned());
        for (path, body) in &files {
            let name = format!("package/{path}");
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            let octal = |n: u64, w: usize| {
                let mut s = format!("{:0w$o}", n, w = w - 1).into_bytes();
                s.push(0);
                s
            };
            let mode = if self.bin.as_ref().is_some_and(|(_, p)| p == path) {
                0o755
            } else {
                0o644
            };
            h[100..108].copy_from_slice(&octal(mode, 8));
            h[108..116].copy_from_slice(&octal(0, 8));
            h[116..124].copy_from_slice(&octal(0, 8));
            h[124..136].copy_from_slice(&octal(body.len() as u64, 12));
            h[136..148].copy_from_slice(&octal(0, 12));
            h[156] = b'0';
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            h[148..156].fill(b' ');
            let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
            h[148..156].copy_from_slice(&octal(sum, 8));
            tar.extend_from_slice(&h);
            tar.extend_from_slice(body.as_bytes());
            tar.extend(std::iter::repeat_n(0u8, (512 - body.len() % 512) % 512));
        }
        tar.extend_from_slice(&[0u8; 1024]);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar).unwrap();
        enc.finish().unwrap()
    }
}

/// version -> (manifest, tgz).
type Versions = BTreeMap<String, (Value, Vec<u8>)>;
/// dist-tag -> version.
type Tags = BTreeMap<String, String>;

/// What the registry serves.
#[derive(Default)]
struct State {
    packages: BTreeMap<String, (Versions, Tags)>,
    /// The bulk advisory endpoint's answer.
    advisories: Value,
    /// Paths requested, in order.
    seen: Vec<String>,
}

/// The fake registry.
pub struct Registry {
    pub url: String,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn unscoped(name: &str) -> &str {
    name.rsplit('/').next().unwrap()
}

impl Registry {
    pub async fn start(pkgs: &[Pkg]) -> Self {
        let state: Arc<Mutex<State>> = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let reg_url = url.clone();
        let s = state.clone();
        let app = axum::Router::new().fallback(move |req: Request| {
            let s = s.clone();
            let base = reg_url.clone();
            async move { serve(&s, &base, req.uri().path()) }
        });
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let r = Self { url, state, task };
        for p in pkgs {
            r.publish(p);
        }
        r
    }

    /// Adds a version (and moves `latest` to it unless a higher one exists).
    pub fn publish(&self, p: &Pkg) {
        let tgz = p.tgz();
        let mut st = self.state.lock();
        let entry = st.packages.entry(p.name.clone()).or_default();
        entry.0.insert(p.version.clone(), (p.manifest(), tgz));
        let latest = entry
            .0
            .keys()
            .filter(|v| !v.contains('-'))
            .max_by(|a, b| fnpm_semver::order(a, b).unwrap())
            .cloned();
        if let Some(l) = latest {
            entry.1.insert("latest".into(), l);
        }
    }

    /// What `POST /-/npm/v1/security/advisories/bulk` answers.
    pub fn set_advisories(&self, v: Value) {
        self.state.lock().advisories = v;
    }

    pub fn tag(&self, name: &str, tag: &str, version: &str) {
        let mut st = self.state.lock();
        st.packages
            .get_mut(name)
            .unwrap()
            .1
            .insert(tag.into(), version.into());
    }

    pub fn integrity(&self, name: &str, version: &str) -> String {
        let st = self.state.lock();
        let tgz = &st.packages[name].0[version].1;
        Integrity::compute(HashAlgorithm::Sha512, tgz).to_sri()
    }

    pub fn tarball_url(&self, name: &str, version: &str) -> String {
        format!("{}{name}/-/{}-{version}.tgz", self.url, unscoped(name))
    }

    /// Paths requested so far.
    pub fn seen(&self) -> Vec<String> {
        self.state.lock().seen.clone()
    }

    pub fn clear_seen(&self) {
        self.state.lock().seen.clear();
    }

    /// Manifest requests (not tarballs) so far.
    pub fn manifest_requests(&self) -> usize {
        self.seen().iter().filter(|p| !p.ends_with(".tgz")).count()
    }

    pub fn tarball_requests(&self) -> usize {
        self.seen().iter().filter(|p| p.ends_with(".tgz")).count()
    }
}

fn serve(state: &Mutex<State>, base: &str, path: &str) -> Response {
    let mut st = state.lock();
    st.seen.push(path.to_owned());
    let decoded = path
        .trim_start_matches('/')
        .replace("%2f", "/")
        .replace("%2F", "/");
    if decoded.ends_with("security/advisories/bulk") {
        let body = if st.advisories.is_null() {
            json!({})
        } else {
            st.advisories.clone()
        };
        return Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
    }
    if decoded == "-/ping" {
        return Response::builder()
            .status(200)
            .body(Body::from("{}"))
            .unwrap();
    }
    let not_found = || {
        Response::builder()
            .status(404)
            .header("content-type", "application/json")
            .body(Body::from("{\"error\":\"Not found\"}"))
            .unwrap()
    };
    if let Some((name, file)) = decoded.split_once("/-/") {
        let Some((versions, _)) = st.packages.get(name) else {
            return not_found();
        };
        let prefix = format!("{}-", unscoped(name));
        let Some(version) = file
            .strip_prefix(&prefix)
            .and_then(|f| f.strip_suffix(".tgz"))
        else {
            return not_found();
        };
        let Some((_, tgz)) = versions.get(version) else {
            return not_found();
        };
        return Response::builder()
            .status(200)
            .header("content-type", "application/octet-stream")
            .body(Body::from(tgz.clone()))
            .unwrap();
    }
    let Some((versions, tags)) = st.packages.get(&decoded) else {
        return not_found();
    };
    let mut vs = serde_json::Map::new();
    let mut time = serde_json::Map::new();
    for (v, (manifest, tgz)) in versions {
        let mut m = manifest.clone();
        m["dist"] = json!({
            "tarball": format!("{base}{decoded}/-/{}-{v}.tgz", unscoped(&decoded)),
            "integrity": Integrity::compute(HashAlgorithm::Sha512, tgz).to_sri(),
        });
        vs.insert(v.clone(), m);
        time.insert(v.clone(), json!("2020-01-01T00:00:00.000Z"));
    }
    let doc = json!({
        "name": decoded,
        "dist-tags": tags,
        "versions": vs,
        "time": time,
    });
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("cache-control", "max-age=0")
        .body(Body::from(doc.to_string()))
        .unwrap()
}

/// A temporary project directory plus an isolated fnpm home.
pub struct Project {
    pub tmp: tempfile::TempDir,
    pub dir: PathBuf,
    pub registry_url: String,
}

impl Project {
    pub fn new(registry: &Registry, package_json: Value) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("app");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&package_json).unwrap() + "\n",
        )
        .unwrap();
        Self {
            tmp,
            dir,
            registry_url: registry.url.clone(),
        }
    }

    pub fn env(&self) -> Env {
        let home = self.tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        Env::from_pairs([
            ("HOME", home.to_string_lossy().into_owned()),
            (
                "FNPM_HOME",
                self.tmp
                    .path()
                    .join("fnpm-home")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("PATH", std::env::var("PATH").unwrap_or_default()),
            (
                "FNPM_GLOBAL_DIR",
                self.tmp
                    .path()
                    .join("global")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "FNPM_GLOBAL_BIN_DIR",
                self.tmp
                    .path()
                    .join("global-bin")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ])
    }

    pub fn opts(&self, command: InstallCommand, packages: &[&str]) -> InstallOptions {
        InstallOptions {
            pm: PmOptions {
                cwd: self.dir.clone(),
                registry: Some(self.registry_url.clone()),
                ..PmOptions::default()
            },
            command,
            packages: packages.iter().map(|s| (*s).to_owned()).collect(),
            ..InstallOptions::default()
        }
    }

    pub async fn run(
        &self,
        opts: InstallOptions,
    ) -> fnpm_core::Result<(InstallSummary, Vec<Event>)> {
        let sink = Arc::new(VecSink::default());
        let r = fnpm_core::install_with_env(opts, sink.clone(), self.env()).await?;
        let events = sink.0.lock().unwrap().clone();
        Ok((r, events))
    }

    pub async fn install(&self) -> InstallSummary {
        self.run(self.opts(InstallCommand::Install, &[]))
            .await
            .unwrap()
            .0
    }

    pub async fn invoke(
        &self,
        command: &[&str],
        positionals: &[&str],
        options: Value,
    ) -> fnpm_core::Result<CoreResult> {
        let mut options = options.as_object().cloned().unwrap_or_default();
        options
            .entry("registry")
            .or_insert(Value::String(self.registry_url.clone()));
        let req = CoreRequest {
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            cwd: self.dir.clone(),
            positionals: positionals.iter().map(|s| (*s).to_owned()).collect(),
            passthrough: Vec::new(),
            options,
            json: false,
        };
        fnpm_core::invoke_with_env(req, Arc::new(fnpm_types::NoopSink), self.env()).await
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    pub fn json(&self, rel: &str) -> Value {
        serde_json::from_str(&self.read(rel)).unwrap()
    }

    pub fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// The version in `node_modules/<rel>/package.json`.
    pub fn installed(&self, rel: &str) -> Option<String> {
        let p = self.path(&format!("node_modules/{rel}/package.json"));
        let text = std::fs::read_to_string(p).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        v["version"].as_str().map(str::to_owned)
    }
}

pub fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}
