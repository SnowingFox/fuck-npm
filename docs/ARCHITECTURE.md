# fnpm architecture

fnpm (`fuck-npm`) is a package manager with every user-visible feature of bun's, built to be
faster than bun and to use less disk. `fnpm` and `fnpm-lock` are native Rust executables
([fnpm-cli](#fnpm-cli)). They run `install`, `i`, `ci`, `add`, `remove` and `update` end to end in
process. Every other command, `--help`, and interactive flows are handed to a TypeScript CLI
(`packages/fuck-npm`) that parses arguments and renders output, with the work done by the same
Rust core loaded as a napi-rs addon (`crates/fnpm-napi`). Why: see
[startup budget](#startup-budget) and `docs/DECISIONS.md`.

This document is the contract between the crates. Each crate section says what the crate owns,
its public API, what it may depend on, how it uses threads, its error type, which bun sources
to port, and its performance budget. A section plus the bun sources it names should be enough
to implement that crate. The skeletons in `crates/*/src` match the signatures here; when they
disagree, fix whichever is wrong in the same change.

Paths like `install/npm.rs` are relative to `submodule/bun/src/`. Parity rows live in
`docs/PARITY.md`; the raw feature inventory is `docs/inventory/bun-pm.json`. Implementation
status per crate is in `docs/ROADMAP.md`, and the reasoning behind the larger choices is in
`docs/DECISIONS.md`.

## Contents

1. [Ground rules](#ground-rules)
2. [Crate graph](#crate-graph)
3. [Threading model](#threading-model)
4. [On-disk layouts](#on-disk-layouts)
5. [Configuration sources and environment](#configuration-sources-and-environment)
6. [fnpm.lock](#fnpmlock)
7. [Install pipeline](#install-pipeline)
8. Crates: [fnpm-types](#fnpm-types), [fnpm-semver](#fnpm-semver), [fnpm-config](#fnpm-config),
   [fnpm-registry](#fnpm-registry), [fnpm-store](#fnpm-store), [fnpm-workspace](#fnpm-workspace),
   [fnpm-resolver](#fnpm-resolver), [fnpm-lockfile](#fnpm-lockfile), [fnpm-linker](#fnpm-linker),
   [fnpm-scripts](#fnpm-scripts), [fnpm-core](#fnpm-core), [fnpm-napi](#fnpm-napi),
   [fnpm-cli](#fnpm-cli)
9. [napi JS API and the event schema](#napi-js-api-and-the-event-schema)
10. TypeScript: [CLI](#packagesfuck-npm), [binding loader](#packagesfuck-npm-binding),
    [bench](#bench), [test registry and conformance](#testsregistry-and-testsconformance)
11. [Packaging and distribution](#packaging-and-distribution)
12. [Startup budget](#startup-budget)

## Ground rules

- **Errors.** Every crate has one `pub enum Error` (`thiserror`) and
  `pub type Result<T, E = Error>`. Library crates never panic on input from users, the network
  or the filesystem; `unwrap`/`expect` only for invariants. No `anyhow` in libraries.
  `fnpm-core` wraps all crate errors and maps them to stable string codes
  (`fnpm_core::Error::code`) that both CLIs (native and TS) turn into messages and exit codes.
- **Strings and paths.** Package data is UTF-8 `String`. Paths inside lockfiles and graphs are
  relative, `/`-separated, and never absolute unless the user wrote an absolute path. On disk,
  `std::path::PathBuf`.
- **Determinism.** Same inputs give the same lockfile bytes, the same tree and the same store
  keys, regardless of thread scheduling. Sort before you serialise.
- **Dependencies.** Only versions pinned in the root `Cargo.toml` `[workspace.dependencies]`
  (exact `=x.y.z`). Adding a crate means adding it there first, pinned.
- **Tests.** Unit tests next to the code; integration tests in `crates/<name>/tests/`. Tests
  that need a registry use the axum fixture server in-process (Rust) or `tests/registry`
  (TS). Tests never contact the public internet.
- **Compatibility first.** Where bun and npm disagree, follow bun (we port its tests), unless a
  section here says otherwise. Every intentional difference is listed in that crate's section.

## Crate graph

The dependency graph is acyclic. An arrow `a <- b` means `b` may depend on `a`.

```text
fnpm-types
   ^
fnpm-semver
   ^
fnpm-config
   ^------------------^------------------^
fnpm-registry     fnpm-store      fnpm-workspace
   ^------------------^------------------^
      fnpm-resolver          fnpm-lockfile
          ^------------------------^
      fnpm-linker            fnpm-scripts
          ^------------------------^
                  fnpm-core
          ^------------------------^
      fnpm-napi (cdylib)      fnpm-cli (bins: fnpm, fnpm-lock)
```

| crate | depends on (fnpm) | key external crates |
|---|---|---|
| fnpm-types | none | serde, thiserror, sha1, sha2, base64, hex, url, percent-encoding |
| fnpm-semver | types | thiserror |
| fnpm-config | types, semver | toml, serde_json, url |
| fnpm-registry | types, semver, config | tokio, reqwest (h2 + rustls), sonic-rs, rkyv, xxhash |
| fnpm-store | types, semver, config | tokio, flate2 (zlib-rs), libdeflater, rayon, rustix, reflink-copy, rkyv, blake3, ring |
| fnpm-workspace | types, semver, config | globset, rayon, serde_json, serde_yaml_ng |
| fnpm-resolver | types, semver, config, registry, store, workspace | tokio, rayon, parking_lot |
| fnpm-lockfile | types, semver, config, workspace | serde_json, serde_yaml_ng, sonic-rs, rkyv, xxhash |
| fnpm-linker | types, config, store, lockfile, workspace | rayon, rustix, reflink-copy, globset |
| fnpm-scripts | types, config, workspace, lockfile | tokio (process) |
| fnpm-core | all of the above | tokio, rayon, crossbeam-channel |
| fnpm-napi | core, types | napi, napi-derive, mimalloc |
| fnpm-cli | core, types | mimalloc, rustix; build script: serde_json (reads `cli-spec.json`) |

`fnpm-napi` and `fnpm-cli` are siblings: neither depends on the other, and neither contains
package-manager logic. Both call `fnpm-core`.

Rule of thumb for where code goes: data shapes shared by two or more crates go in
`fnpm-types`; anything that touches the network goes in `fnpm-registry`; anything that writes
the store goes in `fnpm-store`; the hoisting algorithm lives in `fnpm-lockfile::tree` because
both lockfile serialisation and the hoisted linker need it.

## Threading model

- **One tokio runtime per process** (`fnpm_core::runtime()`, multi-thread, `min(cpus, 8)`
  workers). Network I/O, timers, child processes and the orchestration of the pipeline run
  here. The napi `async fn`s and the native launcher's `install` run on it.
- **The global rayon pool** for CPU work: manifest JSON parsing, gunzip + untar + hashing,
  hoisting, lockfile parsing and printing, workspace globbing. Never block a tokio worker on
  CPU work longer than ~100 µs; use `rayon::spawn` + a oneshot channel, or
  `tokio::task::spawn_blocking` for blocking syscalls.
- **A dedicated link pool** of at most 8 threads (bun `PackageInstaller.rs:130`; tuned by the
  benchmark) owned by `fnpm-linker`, so directory-inode contention does not starve the
  extraction threads.
- **Overlap.** Resolution streams `Resolved::Package` events over a tokio mpsc channel; the
  core starts each tarball fetch immediately; the store publishes completed packages; the
  linker's `PackageSource::wait` blocks a link thread until its package is ready. Nothing
  waits for a whole phase unless it has to (scripts wait for linking of their dependency
  closure; the lockfile is saved after resolution).
- **Shared state** uses `parking_lot` locks or lock-free maps; never hold a lock across an
  `.await`.

## On-disk layouts

### fnpm home (`FNPM_HOME`, default `~/.fnpm`)

```text
~/.fnpm/
  store/v1/                  FNPM_STORE_DIR overrides ~/.fnpm/store (v1 is appended)
    files/<2 hex>/<128 hex>[-x]   CAS blobs: BLAKE3 (64-byte output) of the file contents; -x = executable
    pkgs/<name>@<version>-<16 hex>/
      package/...                 hardlinks to blobs, the package root
      index.bin                   rkyv FileIndex (dirs, files, sizes, modes, digests, package.json)
    links/<store key>-<16 hex>/node_modules/<name>   global virtual store entries
    tmp/                          staging (same filesystem as files/ and pkgs/)
    projects.json                 projects that use this store, for prune
  cache/                     FNPM_CACHE_DIR (also BUN_INSTALL_CACHE_DIR, install.cache.dir)
    metadata/v1/<16 hex>.rkyv     manifest cache, key = xxh3-64(registry URL + "\0" + name)
    git/<16 hex>/                 bare clones for git dependencies
    dlx/<16 hex>/                 `fnpm x` installs
    prefetch/<16 hex>.txt         look-ahead hints per project
  global/                    `-g` installs (FNPM_GLOBAL_DIR, BUN_INSTALL_GLOBAL_DIR)
  bin/                       `-g` bins (FNPM_GLOBAL_BIN_DIR, BUN_INSTALL_BIN)
  fnpm.toml                  global config
```

`<name>` in `pkgs/` replaces `/` with `+` (`@types+node@20.1.0-…`). `<16 hex>` in `pkgs/` is
the first 16 hex digits of the package's integrity digest. The store and the projects should
be on one filesystem; when they are not, linking falls back to copy (sticky, see
fnpm-store) and the doctor command warns.

### Project

```text
<project>/
  fnpm.lock                       the lockfile (text, committed)
  node_modules/
    .fnpm/
      state                       no-op fast-path state (JSON, see fnpm-core)
      lock.bin                    rkyv sidecar of the parsed lockfile, keyed by its hash
      <store key>/node_modules/<name>     isolated layout entries (or symlinks into links/)
      node_modules/<name>         hidden hoist (isolated)
    .bin/                         bin links (hoisted and isolated root)
    <name>/                       hoisted packages, or symlinks into .fnpm (isolated)
```
## Configuration sources and environment

Owned by [fnpm-config](#fnpm-config). Sources, lowest precedence first; a later source
replaces a key set by an earlier one (scopes merge per scope name, `url_auth` appends):

1. User `.npmrc`: `$NPM_CONFIG_USERCONFIG` / `$npm_config_userconfig` if set (replaces the
   next two candidates), else `$XDG_CONFIG_HOME/.npmrc` if it exists and XDG is absolute,
   else `$HOME/.npmrc`.
2. Project `.npmrc` (`<project_dir>/.npmrc`). The global `prefix/etc/npmrc` is not read (bun).
3. Global bunfig: `$XDG_CONFIG_HOME/.bunfig.toml` or `$HOME/.bunfig.toml`, `[install]` only.
4. Project `bunfig.toml`, `[install]` only (`globalDir` / `globalBinDir` ignored with bun's
   warning).
5. Global `~/.fnpm/fnpm.toml`, then project `fnpm.toml`, same `[install]` schema as bunfig.
   `--config <path>` replaces the project fnpm.toml/bunfig candidate.
6. `package.json#fnpm` in the project root (same keys as `[install]`, camelCase JSON).
7. Environment variables (table below).
8. CLI flags (`fnpm_config::CliOverrides`, built from `PmOptions`, which the TS CLI sends over
   napi and fnpm-cli builds natively from the same flag spec).
9. Finally `.npmrc` `//host/path/:_authToken` entries fill scopes that still have no
   credentials (bun `PackageManager.rs:1787-1870`).

Registry credentials carry over when the registry URL is replaced by one on the same host
with a path prefix and without an https -> http downgrade (bun
`PackageManagerOptions.rs:347-418`). `--registry` drops `.npmrc` credentials of the replaced
registry but keeps token env vars.

Every `.npmrc` key bun reads is honoured (`registry`, `@scope:registry`, per-URL
`_authToken` / `_auth` / `username` / `_password` / `email` / `certfile` / `keyfile`, `cache`,
`dry-run`, `ca`, `cafile`, `strict-ssl`, `omit`, `include`, `ignore-scripts`,
`link-workspace-packages`, `save-exact`, `node-linker`, `hoist-pattern`,
`public-hoist-pattern`, `minimum-release-age`, `proxy`, `https-proxy`, `noproxy`).

### Environment variables

`FNPM_*` names are fnpm's own; the `BUN_*` and `npm_config_*` columns are honoured so existing
setups keep working. Where several are set, the first column wins, then left to right.

| setting | fnpm | also honoured |
|---|---|---|
| home | `FNPM_HOME` | |
| store | `FNPM_STORE_DIR` | |
| cache | `FNPM_CACHE_DIR` | `BUN_INSTALL_CACHE_DIR`, `npm_config_cache` |
| global dir / bin | `FNPM_GLOBAL_DIR`, `FNPM_GLOBAL_BIN_DIR` | `BUN_INSTALL_GLOBAL_DIR`, `BUN_INSTALL_BIN`, `BUN_INSTALL` (root of both) |
| registry | `FNPM_REGISTRY`, `FNPM_CONFIG_REGISTRY` | `BUN_CONFIG_REGISTRY`, `NPM_CONFIG_REGISTRY`, `npm_config_registry` |
| token (default registry) | `FNPM_CONFIG_TOKEN` | `BUN_CONFIG_TOKEN`, `NPM_CONFIG_TOKEN`, `npm_config_token` |
| user npmrc | | `NPM_CONFIG_USERCONFIG`, `npm_config_userconfig` |
| ignore scripts (only turns scripts off) | `FNPM_CONFIG_IGNORE_SCRIPTS` | `NPM_CONFIG_IGNORE_SCRIPTS`, `npm_config_ignore_scripts` |
| any `.npmrc` key `<key>` | `FNPM_CONFIG_<KEY>` | `BUN_CONFIG_<KEY>`, `NPM_CONFIG_<KEY>`, `npm_config_<key>` |
| max HTTP requests (default 64) | `FNPM_CONFIG_MAX_HTTP_REQUESTS` | `BUN_CONFIG_MAX_HTTP_REQUESTS` |
| retries (default 5) / backoff ms (50) | `FNPM_CONFIG_HTTP_RETRY_COUNT`, `FNPM_CONFIG_HTTP_RETRY_BACKOFF` | `BUN_CONFIG_HTTP_RETRY_COUNT`, `BUN_CONFIG_HTTP_RETRY_BACKOFF` |
| idle timeout s (300) | `FNPM_CONFIG_HTTP_IDLE_TIMEOUT` | `BUN_CONFIG_HTTP_IDLE_TIMEOUT` |
| skip integrity | `FNPM_CONFIG_NO_VERIFY` | `BUN_CONFIG_NO_VERIFY` |
| skip save / load lockfile, skip install | `FNPM_CONFIG_SKIP_SAVE_LOCKFILE` etc. | `BUN_CONFIG_SKIP_SAVE_LOCKFILE`, `BUN_CONFIG_SKIP_LOAD_LOCKFILE`, `BUN_CONFIG_SKIP_INSTALL_PACKAGES` (value `0` keeps the step) |
| write yarn.lock | `FNPM_CONFIG_YARN_LOCKFILE` | `BUN_CONFIG_YARN_LOCKFILE` |
| manifest cache (0 off, 1 no cache-control, 2 on) | `FNPM_MANIFEST_CACHE` | `BUN_MANIFEST_CACHE` |
| global virtual store on/off | `FNPM_GLOBAL_STORE` | `BUN_INSTALL_GLOBAL_STORE` |
| progress off | `FNPM_PROGRESS=0` | `BUN_INSTALL_PROGRESS=0`; off in CI (`CI`, `TDDIUM`, `GITHUB_ACTIONS`, `JENKINS_URL`, `bamboo.buildKey`) and when stderr is not a TTY |
| verbose | `FNPM_VERBOSE` | `BUN_INSTALL_VERBOSE` |
| temp dir | `FNPM_TMPDIR` | `BUN_TMPDIR` |
| GitHub API | `GITHUB_API_URL`, `GITHUB_TOKEN` | |
| TLS | `NODE_TLS_REJECT_UNAUTHORIZED=0`, `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `NODE_USE_SYSTEM_CA` | |
| proxy | `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` (any case) | |
| feature flags | `FNPM_FEATURE_FLAG_*` | `BUN_FEATURE_FLAG_DISABLE_NATIVE_DEPENDENCY_LINKER`, `..._DISABLE_IGNORE_SCRIPTS`, `..._DISABLE_INSTALL_INDEX`, `BUN_CONFIG_DISABLE_COPY_FILE_RANGE`, `BUN_CONFIG_DISABLE_ioctl_ficlonerange` |

### `fnpm.toml`

Same keys as bunfig `[install]` (`docs/runtime/bunfig.mdx` in bun), plus fnpm-only keys:

```toml
[install]
registry = "https://registry.npmjs.org/"
linker = "isolated"            # auto | hoisted | isolated
backend = "clonefile"          # clonefile | hardlink | copyfile | symlink
globalStore = true             # fnpm default true (bun: false)
exact = false
frozenLockfile = false
ignoreScripts = false
concurrentScripts = 16
networkConcurrency = 64        # fnpm-only
minimumReleaseAge = 259200     # seconds
minimumReleaseAgeExcludes = ["@my-org/*"]
publicHoistPattern = ["*eslint*"]

[install.scopes]
myorg = { url = "https://npm.myorg.dev/", token = "$MYORG_TOKEN" }

[install.lockfile]
save = true
print = "yarn"                 # also write yarn.lock

[install.cache]
dir = "~/.fnpm/cache"

[install.security]
scanner = "@acme/fnpm-scanner"
```

## fnpm.lock

`fnpm.lock` uses bun.lock's text format (`install/lockfile/bun.lock.rs`), so
`fnpm-lock export --to bun` and `migrate --from bun` are near-identity and bun's lockfile
tests apply. It is JSONC: trailing commas, 2-space indent, one line per package, sorted keys,
trailing newline, file mode 0o666.

- Top-level keys in this order: `lockfileVersion`, `configVersion`, `workspaces`,
  `trustedDependencies`, `patchedDependencies`, `overrides`, `catalog`, `catalogs`,
  `packageExtensions`, `packages`. Empty sections are omitted, except that an explicitly empty
  `trustedDependencies: []` is written (it means "trust nothing").
- `workspaces` is keyed by workspace path (`""` is the root), sorted. Each entry has `name`,
  `version` (members only), `bin`/`binDir` if any, and the four dependency groups as declared,
  plus `optionalPeers` and `unresolved` arrays when non-empty.
- `packages` is keyed by **hoisted install path**: `name` for `node_modules/name`,
  `parent/name` for `node_modules/parent/node_modules/name`, and `<workspace name>/name` for a
  workspace's own `node_modules`. Keys are sorted by depth then path. The graph is recovered
  by node's lookup rule over these paths (`fnpm_lockfile::tree::resolve_paths`), and written
  by `fnpm_lockfile::tree::hoist`.
- Package tuples, by resolution:

| resolution | tuple |
|---|---|
| npm | `["name@version", "<tarball URL or '' for the default location>", INFO, "integrity"]` |
| git | `["name@git+<url>#<commit>", INFO, "<commit>", "integrity"]` |
| github | `["name@github:owner/repo#<commit>", INFO, "<commit>", "integrity"]` |
| remote tarball | `["name@<url>", INFO, "integrity"]` |
| local tarball | `["name@file:<path>", INFO, "integrity"]` |
| folder | `["name@file:<path>", INFO]` |
| symlink | `["name@link:<path>", INFO]` |
| workspace | `["name@workspace:<path>"]` |

  INFO is a single-line object with, when non-empty: `dependencies`, `devDependencies`
  (local packages only), `optionalDependencies`, `peerDependencies` (optional peers listed
  here and in `optionalPeers`), `optionalPeers`, `bundled: true`, `os`, `cpu` (negatable
  lists, `["none"]` for nothing), `libc`, `hasInstallScript: true`, `bin` or `binDir`,
  `deprecated` (fnpm addition, ignored by bun).
- Everything an install needs is in the file (integrity, tarball location, bin, platform,
  install-script flag), so a frozen install makes zero manifest requests.

Full example (a root with one workspace, a catalog, an override, a patch and a nested
package):

```jsonc
{
  "lockfileVersion": 1,
  "configVersion": 1,
  "workspaces": {
    "": {
      "name": "app",
      "dependencies": {
        "@app/lib": "workspace:*",
        "react": "catalog:",
      },
      "devDependencies": {
        "esbuild": "^0.21.5",
      },
    },
    "packages/lib": {
      "name": "@app/lib",
      "version": "0.1.0",
      "dependencies": {
        "left-pad": "1.3.0",
        "loose-envify": "^1.0.0",
      },
      "peerDependencies": {
        "react": ">=17",
      },
    },
  },
  "trustedDependencies": [
    "esbuild",
  ],
  "patchedDependencies": {
    "left-pad@1.3.0": "patches/left-pad@1.3.0.patch",
  },
  "overrides": {
    "js-tokens": "4.0.0",
  },
  "catalog": {
    "react": "^18.3.1",
  },
  "packages": {
    "@app/lib": ["@app/lib@workspace:packages/lib"],
    "@esbuild/linux-x64": ["@esbuild/linux-x64@0.21.5", "", { "os": "linux", "cpu": "x64" }, "sha512-…"],
    "esbuild": ["esbuild@0.21.5", "", { "optionalDependencies": { "@esbuild/linux-x64": "0.21.5" }, "bin": { "esbuild": "bin/esbuild" }, "hasInstallScript": true }, "sha512-…"],
    "js-tokens": ["js-tokens@4.0.0", "", {}, "sha512-…"],
    "left-pad": ["left-pad@1.3.0", "", { "deprecated": "use String.prototype.padStart()" }, "sha512-…"],
    "loose-envify": ["loose-envify@1.4.0", "", { "dependencies": { "js-tokens": "^3.0.0 || ^4.0.0" }, "bin": { "loose-envify": "cli.js" } }, "sha512-…"],
    "react": ["react@18.3.1", "", { "dependencies": { "loose-envify": "^1.1.0" } }, "sha512-…"],
    "@app/lib/loose-envify": ["loose-envify@1.0.0", "", { "dependencies": { "js-tokens": "^1.0.1" } }, "sha512-…"],
    "@app/lib/loose-envify/js-tokens": ["js-tokens@1.0.3", "", {}, "sha512-…"],
  }
}
```

(The `…` stand for full SRI strings.) The binary sidecar `node_modules/.fnpm/lock.bin` caches
the parsed graph keyed by xxh3-64 of these bytes; it is never committed and is safe to delete.

## Install pipeline

`fnpm_core::install`, in order. Steps marked (overlap) start before the previous step ends.

1. **Fast path.** Locate the project root (walk up for package.json), `stat`
   `node_modules/.fnpm/state`, compare the lockfile hash, each package.json's size/mtime
   (hash on mismatch) and the config hash. Unchanged: emit the summary with `noop: true` and
   return. Budget: < 5 ms of Rust for a 100-workspace monorepo.
2. **Config + workspace.** `Config::load`, `Workspace::load` (parallel package.json reads).
3. **Lockfile.** `fnpm_lockfile::load` (sidecar if fresh; else parse; else migrate from the
   highest-priority foreign lockfile).
4. **Resolve** (streaming). `Resolver::resolve` with the previous graph; unchanged entries are
   reused without manifest fetches. Frozen mode errors on any difference.
5. **Fetch + extract** (overlap with 4). For each `Resolved::Package` not in the store:
   `RegistryClient::tarball` -> `Store::import_tarball` (gunzip, untar, hash, CAS, index).
   Manifests always take priority over tarballs in the request queue.
6. **Link** (overlap with 5). `Linker::plan`, then `Linker::link` with a `PackageSource` that
   waits on the in-flight imports.
7. **Scripts.** `TrustPolicy` + optimizer select jobs; `fnpm_scripts::run_jobs` in
   dependency order, `2 * cpus` concurrent; root and workspace scripts last, in the
   foreground. Bins that a script produced are relinked.
8. **Save.** `fnpm_lockfile::save` (atomic, skipped when bytes are unchanged), package.json
   edits (`PackageJsonEditor`), the sidecar, `Store::register_project`, then the state file
   last. Manifest cache writes are flushed in the background before the process exits.
## fnpm-types

**Status: implemented** (`cargo test -p fnpm-types`). The shared data model. No I/O, no async.

Modules and main items:

```rust
// name.rs
pub struct PackageName(Box<str>);           // serde as string, validated
impl PackageName {
    pub fn parse(name: &str) -> Result<Self, NameError>;   // npm's rule for existing names
    pub fn new_unchecked(name: impl Into<Box<str>>) -> Self;
    pub fn as_str(&self) -> &str; pub fn is_scoped(&self) -> bool;
    pub fn scope(&self) -> Option<&str>; pub fn unscoped(&self) -> &str;
}
pub fn validate_new_package_name(name: &str) -> Result<(), Vec<NameProblem>>; // init/pack/publish
pub fn is_npm_package_name(s: &str) -> bool;
pub fn is_scoped_package_name(s: &str) -> Result<bool, NameError>;
pub fn split_name_and_maybe_version(s: &str) -> (&str, Option<&str>);
pub fn is_safe_install_folder_name(s: &str) -> bool;   // aliases from untrusted manifests
pub fn contains_control_character(s: &str) -> bool;
pub fn fallback_package_name(location: &str) -> &str;

// spec.rs: every protocol bun supports, plus #semver: and ::path:
pub enum SpecTag { Npm, DistTag, Tarball, Folder, Symlink, Workspace, Git, Github, Catalog }
pub fn infer_tag(literal: &str) -> SpecTag;            // bun Tag::infer, bug for bug
pub enum DependencySpec {
    Npm { name: String, range: String, is_alias: bool },
    DistTag { name: String, tag: String, is_alias: bool },
    Tarball { uri: TarballUri },                       // Remote(url) | Local(path)
    Folder { path: String }, Symlink { path: String },
    Workspace { spec: WorkspaceSpec },                 // Any | Caret | Tilde | Range | Path
    Git(GitSpec),                                      // url, committish, hosted: Option<HostedGitInfo>
    Github(GithubSpec),                                // owner, repo, committish
    Catalog { catalog: String },
}
impl DependencySpec {
    pub fn parse(alias: &str, literal: &str) -> Result<Self, SpecError>;
    pub fn parse_with_tag(alias: &str, literal: &str, tag: SpecTag) -> Result<Self, SpecError>;
    pub fn tag(&self) -> SpecTag; pub fn registry_name<'a>(&'a self, alias: &'a str) -> &'a str;
}
pub struct Committish { pub reference: Option<String>, pub semver: Option<String>, pub path: Option<String> }
pub struct Dependency { pub name: String, pub literal: String, pub spec: DependencySpec, pub behavior: Behavior }
pub fn without_npm_protocol(lit: &str) -> &str; pub fn is_tarball(s: &str) -> bool; /* … */

// hosted_git.rs: hosted-git-info 9.0.0 (all 652 vectors from bun's cases.ts pass)
pub struct HostedGitInfo { pub provider: HostProvider, pub user: Option<String>, pub project: String,
                           pub committish: Option<String>, pub default_representation: Representation }
impl HostedGitInfo {
    pub fn from_url(s: &str) -> Result<Option<Self>, HostedGitError>;
    pub fn shortcut(&self) -> String; pub fn ssh_url(&self) -> String; pub fn https_url(&self) -> String;
    pub fn clone_url(&self, ssh: bool) -> String; pub fn tarball_url(&self) -> String;
}
pub fn is_github_shorthand(s: &str) -> bool;

// behavior.rs
pub struct Behavior(pub u8);   // PROD, OPTIONAL, DEV, PEER, WORKSPACE, BUNDLED (bun's bit layout)
pub enum DependencyGroup { Prod, Dev, Optional, Peer }   // serde: "dependencies", …
pub struct PeerMeta { pub optional: bool }

// resolution.rs
pub enum Resolution {
    Root,
    Npm { version: String, tarball_url: Option<String>, integrity: Option<Integrity> },
    Git { url: String, committish: String, resolved_commit: String, integrity: Option<Integrity> },
    Github { owner: String, repo: String, committish: String, resolved_commit: String, integrity: Option<Integrity> },
    RemoteTarball { url: String, integrity: Option<Integrity> },
    LocalTarball { path: String, integrity: Option<Integrity> },
    Folder { path: String }, Symlink { path: String }, Workspace { path: String },
}   // Display = bun.lock's "name@<this>" part; is_remote / is_storable / is_local_tree

// integrity.rs
pub enum HashAlgorithm { Sha1, Sha256, Sha384, Sha512 }   // Ord = strength
pub struct Integrity { /* algorithm + [u8; 64] */ }        // Copy; serde as SRI string
impl Integrity {
    pub fn parse(sri: &str) -> Result<Self, IntegrityError>;  // strongest entry wins, ?opts ignored
    pub fn parse_sha1_hex(hex: &str) -> Result<Self, IntegrityError>;
    pub fn from_dist(integrity: Option<&str>, shasum: Option<&str>) -> Option<Self>;
    pub fn compute(alg: HashAlgorithm, data: &[u8]) -> Self; pub fn verify(&self, data: &[u8]) -> bool;
    pub fn hasher(&self) -> IntegrityHasher; pub fn to_sri(&self) -> String; pub fn to_hex(&self) -> String;
}
pub enum IntegrityHasher { /* streaming: update(&[u8]), finish() -> Integrity */ }

// platform.rs: bun's Negatable semantics
pub struct OsSet(pub u16); pub struct CpuSet(pub u16); pub struct LibcSet(pub u8);
//   from_list (package.json), parse_cli (--os/--cpu/--libc, errors on unknown), to_list, current()
pub struct Platform { pub os: OsSet, pub cpu: CpuSet, pub libc: LibcSet }   // the install target
pub struct PlatformConstraints { pub os: OsSet, pub cpu: CpuSet, pub libc: LibcSet }
impl PlatformConstraints { pub fn matches(&self, t: &Platform) -> bool;
                           pub fn matches_for_edge(&self, t: &Platform, optional: bool) -> bool; }

// bin.rs
pub enum Bin { None, File { path }, NamedFile { name, path }, Map { entries: Vec<(String, String)> }, Dir { path } }
pub fn normalize_bin_name(s: &str) -> Option<&str>; pub fn normalize_bin_target(s: &str) -> Option<String>;

// meta.rs
pub struct ScriptFlags(pub u8);  // preinstall … postprepare, binding.gyp; has_install_script()
pub enum BundledDependencies { None, All(bool), List(Vec<String>) }
pub struct PackageMeta { scripts, has_install_script, deprecated, engines, bundled_dependencies, bundled, license, man }

// overrides.rs: key parsing only (matching is fnpm-resolver)
pub struct OverrideSelector { pub parent: Option<PackageSelector>, pub target: PackageSelector }
impl OverrideSelector { pub fn parse(key: &str) -> Result<Option<Self>, SelectorError>; pub fn to_key(&self) -> String; }
pub struct OverrideRule { pub selector: OverrideSelector, pub value: String }

// graph.rs
pub struct PackageId(pub u32);   // PackageId::ROOT == 0
pub struct DepEdge { pub name: String, pub literal: String, pub behavior: Behavior, pub target: Option<PackageId> }
pub struct Package { pub name, pub version, pub resolution: Resolution, pub dependencies: Vec<DepEdge>,
                     pub peer_meta, pub bin: Bin, pub platform: PlatformConstraints, pub meta: PackageMeta }
pub struct LockGraph {
    pub lockfile_version: u32, pub config_version: ConfigVersion, pub node_linker: Option<NodeLinker>,
    pub packages: Vec<Package>,                         // arena; [0] is the root
    pub workspaces: BTreeMap<String, PackageId>,        // "" -> ROOT, "packages/a" -> id
    pub catalogs: Catalogs, pub overrides: Vec<OverrideRule>,
    pub patched_dependencies: BTreeMap<String, PatchedDependency>,   // "name@version"
    pub trusted_dependencies: Option<BTreeSet<String>>,              // None = default list
    pub package_extensions: BTreeMap<String, PackageExtension>,
}
impl LockGraph {
    pub fn new(root: Package) -> Self; pub fn push(&mut self, p: Package) -> PackageId;
    pub fn add_workspace(&mut self, p: Package) -> PackageId;
    pub fn find(&self, name: &str, r: &Resolution) -> Option<PackageId>; pub fn index(&self) -> GraphIndex;
    pub fn reachable(&self, include: Behavior, roots: Option<&[PackageId]>) -> Vec<bool>;
    pub fn sort_dependencies(&mut self); pub fn validate(&self) -> Result<(), GraphError>;
}

// event.rs: the CLI contract, see "napi JS API and the event schema"
pub enum Event { Resolve{..}, Download{..}, Extract{..}, Link{..}, Script{..}, Warn{..}, Error{..}, Summary{..} }
pub struct InstallSummary { /* camelCase, = types.ts InstallSummary */ }
pub trait EventSink: Send + Sync { fn emit(&self, e: Event); fn enabled(&self) -> bool { true } }
pub struct NoopSink; pub struct VecSink(..);
```

Key decisions:

- **The graph is the model, the tree is derived.** A package appears once per
  `(name, resolution)`; edges point at ids. Peer contexts (isolated) and hoisting (hoisted) are
  computed from it by `fnpm-linker` / `fnpm-lockfile::tree`. `validate()` enforces: root at 0,
  no dangling edges, workspace map consistent, no duplicate `(name, resolution)`.
- **Unresolved edges** have `target: None` (failed optional, platform mismatch, missing peer,
  omitted group). Writers emit them in `unresolved` / `optionalPeers`.
- **Integrity lives on the resolution** (npm/git/github/tarballs), so "can we skip the
  fetch" is one lookup.
- `trusted_dependencies: None` means "no field, use the default list"; `Some(empty)` means
  "trust nothing" (bun's three modes).
- `infer_tag` reproduces bun quirks deliberately: `https://github.com/u/r/tree/x` is `Git`,
  `user@host/path` is scp-like, `vx` is a dist tag. Tests in `crates/fnpm-types/tests/spec.rs`.
- Beyond bun: `#semver:<range>` and `::path:<dir>` git committish parts are parsed
  (`Committish`); resolving them is `fnpm-registry::git`.

Bun sources ported: `install/dependency.rs`, `install/hosted_git_info.rs`,
`install/integrity.rs`, `install/resolution.rs`, `install/bin.rs` (Tag),
`install_types/resolver_hooks.rs` (Behavior, Negatable, OperatingSystem, Architecture,
Libc), `install/lockfile/override_selector.rs`, `install/lockfile/Package/Meta.rs`,
`bun_core/string/immutable.rs` (`is_npm_package_name`), `semver/Version.rs` (scan only).

Perf: `Integrity` is `Copy` (65 bytes); `infer_tag` is allocation-free except for URL-shaped
inputs that go through `HostedGitInfo`. Hot paths in other crates should cache parsed specs
per `(name, literal)` rather than re-parse.

## fnpm-semver

**Responsibility.** npm-compatible versions and ranges. Used by the resolver (picking
versions), config (`minimumReleaseAge` is not semver, but `engines` checks are), lockfile
converters (yarn/pnpm keys contain ranges), overrides (`intersects`), `pm version`
(`inc`), `outdated`.

**API** (`crates/fnpm-semver/src/lib.rs`):

```rust
pub enum Error { InvalidVersion(String), InvalidRange(String) }
pub enum Identifier { Numeric(u64), AlphaNumeric(Box<str>) }
pub struct Version { pub major: u64, pub minor: u64, pub patch: u64, pub pre: Vec<Identifier>, pub build: Box<str> }
impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self;
    pub fn parse(s: &str) -> Result<Self>;        // "v1.2.3", "=1.2.3", " 1.2.3 "
    pub fn parse_loose(s: &str) -> Result<Self>;  // also "1.0.0rc.1", "1.2.3beta"
    pub fn is_prerelease(&self) -> bool;
    pub fn inc(&self, release: ReleaseType, preid: Option<&str>) -> Self;
}
impl Ord for Version; impl Display for Version; impl FromStr for Version;
pub enum ReleaseType { Major, Minor, Patch, Premajor, Preminor, Prepatch, Prerelease }
pub enum Op { Eq, Lt, Lte, Gt, Gte }
pub struct Comparator { pub op: Op, pub version: Version }
pub struct Range { pub sets: Vec<Vec<Comparator>> }    // OR of ANDs
pub struct SatisfyOptions { pub include_prerelease: bool }
impl Range {
    pub fn parse(s: &str) -> Result<Self>;   // ^ ~ x-ranges, hyphen, ||, *, "", >=a <b
    pub fn any() -> Self; pub fn is_any(&self) -> bool;
    pub fn as_exact(&self) -> Option<&Version>;
    pub fn satisfies(&self, v: &Version) -> bool;
    pub fn satisfies_with(&self, v: &Version, o: SatisfyOptions) -> bool;
    pub fn intersects(&self, other: &Range) -> bool;
    pub fn min_version(&self) -> Option<Version>;
}
pub fn max_satisfying<'a, I: IntoIterator<Item = &'a Version>>(v: I, r: &Range, o: SatisfyOptions) -> Option<&'a Version>;
pub fn satisfies(version: &str, range: &str) -> bool;
```

**Semantics.** node-semver: `^0.0.x` locks patch, `~1` = `>=1.0.0 <2.0.0-0`, `1.2` =
`>=1.2.0 <1.3.0-0`, hyphen ranges with partials, a prerelease satisfies only if some
comparator in the same set has a prerelease with the same `[major, minor, patch]` (unless
`include_prerelease`). Leading `v`/`=` and whitespace between operator and version are
accepted. A single leading `v` was already stripped by `fnpm-types` for `Npm` specs.

**Threading.** Pure, `Send + Sync` values.

**Bun sources.** `semver/Version.rs`, `semver/SemverRange.rs`, `semver/SemverQuery.rs`,
`semver/intersects.rs`, `semver/lib.rs`. Test vectors: node-semver's
`test/fixtures/{range-include,range-exclude,comparisons,equality,increments}.js` (port as
tables) and bun's `test/js/bun/semver/*`.

**Perf.** `Range::parse` of `^1.2.3` < 100 ns; `satisfies` < 20 ns for a single-set range;
no allocation in `satisfies`. Keep `Version` comparisons allocation-free (`pre` compared in
place). The resolver caches parsed ranges per literal.
## fnpm-config

**Responsibility.** Read every configuration source and produce one immutable `Config` with
every value resolved (see [Configuration sources](#configuration-sources-and-environment)).
Nothing downstream reads env vars or config files except through `Config` (exceptions:
script env construction and `GITHUB_TOKEN`).

**API** (`crates/fnpm-config/src/{lib,env,npmrc,files}.rs`):

```rust
pub enum Error { Parse { path, message }, InvalidValue { key, message }, Io { path, source } }
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/";

pub enum Auth { None, Token { token }, Basic { username, password }, Legacy { auth } }
pub struct RegistryScope { pub url: String /* ends with '/' */, pub auth: Auth, pub email: Option<String>,
                           pub cert_file: Option<PathBuf>, pub key_file: Option<PathBuf> }
pub enum LinkerSetting { Auto, Hoisted, Isolated }  // fixed() -> Option<NodeLinker>
pub enum Backend { Clonefile, Hardlink, Copyfile, Symlink }
pub enum OfflineMode { Online, PreferOffline, Offline }
pub enum ManifestCacheMode { Disabled, NoCacheControl, CacheControl }
pub enum LogLevel { Silent, Quiet, Default, Verbose }
pub struct Omit { pub dev: bool, pub optional: bool, pub peer: bool }
pub struct NetworkConfig { pub concurrency: u16, pub retries: u16, pub retry_backoff_ms: u64,
    pub idle_timeout_secs: u64, pub offline: OfflineMode, pub manifest_cache: ManifestCacheMode,
    pub proxy: ProxyConfig, pub tls: TlsConfig, pub user_agent: String }
pub struct LockfileConfig { pub save: bool, pub print_yarn: bool, pub path: Option<PathBuf>, pub save_path: Option<PathBuf> }

pub struct Config {
    pub cwd: PathBuf, pub project_dir: PathBuf, pub home_dir: PathBuf,
    pub store_dir: PathBuf, pub cache_dir: PathBuf, pub global_dir: PathBuf, pub global_bin_dir: PathBuf,
    pub registry: RegistryScope, pub scopes: BTreeMap<String, RegistryScope>, pub url_auth: Vec<(String, Auth)>,
    pub network: NetworkConfig, pub linker: LinkerSetting, pub backend: Option<Backend>,
    pub global_store: bool, pub hoist: bool, pub hoist_pattern: Vec<String>, pub public_hoist_pattern: Vec<String>,
    pub link_workspace_packages: bool, pub frozen_lockfile: bool, pub production: bool, pub dry_run: bool,
    pub force: bool, pub exact: bool, pub ignore_scripts: bool, pub verify_integrity: bool,
    pub save_text_lockfile: bool, pub lockfile: LockfileConfig, pub omit: Omit, pub auto_install_peers: bool,
    pub concurrent_scripts: usize, pub minimum_release_age_ms: Option<u64>,
    pub minimum_release_age_excludes: Vec<String>, pub platform: Platform,
    pub security_scanner: Option<String>, pub log_level: LogLevel, pub ci: bool,
}
impl Config {
    pub fn load(opts: &LoadOptions) -> Result<Self>;
    pub fn defaults(cwd: &Path) -> Self;
    pub fn registry_for(&self, package_name: &str) -> &RegistryScope;
    pub fn auth_for_url(&self, url: &str) -> Option<&Auth>;
    pub fn authorization_header(&self, url: &str) -> Option<String>;
}
pub struct CliOverrides { /* Option<…> per flag, see source */ }
pub struct LoadOptions { pub cwd: PathBuf, pub cli: CliOverrides, pub env: Env }

pub struct Env { /* snapshot */ }
impl Env {
    pub fn from_process() -> Self; pub fn from_pairs(..) -> Self; pub fn get(&self, k: &str) -> Option<&str>;
    pub fn first(&self, keys: &[&str]) -> Option<&str>;      // first non-empty, trimmed
    pub fn config_var(&self, npm_key: &str) -> Option<&str>; // FNPM_CONFIG_, BUN_CONFIG_, NPM_CONFIG_, npm_config_
    pub fn truthy(&self, k: &str) -> Option<bool>; pub fn is_ci(&self) -> bool; pub fn expand(&self, v: &str) -> String;
}
pub struct Npmrc { pub entries: Vec<(String, String)> }
impl Npmrc { pub fn parse(text: &str, env: &Env) -> Result<Self>; pub fn load(p: &Path, env: &Env) -> Result<Option<Self>>;
             pub fn get(&self, k: &str) -> Option<&str>; pub fn get_all(..) -> impl Iterator<Item = &str>; }
pub struct InstallFile { /* bunfig [install] schema, all Option */ }
pub fn load_install_table(path: &Path) -> Result<Option<InstallFile>>;
pub fn config_file_candidates(cwd: &Path, home: &Path, xdg: Option<&Path>) -> Vec<PathBuf>;
```

**Defaults** (bun `PackageManagerOptions.rs:128-197, 1294-1364` unless noted):
`network.concurrency = 64`, `retries = 5`, `retry_backoff_ms = 50`, `idle_timeout_secs = 300`,
`manifest_cache = CacheControl`, `linker = Auto`, `backend = None` (platform default),
`global_store = true` (**fnpm differs**: bun defaults false), `hoist = true`,
`link_workspace_packages = true`, `auto_install_peers = true`, `verify_integrity = true`,
`concurrent_scripts = 2 * available_parallelism`, `platform = Platform::current()`,
`lockfile.save = true`, user agent `fnpm/<version> npm/? node/<process.version> <os> <arch>`.
`--production` implies `omit.dev`, `frozen_lockfile` (bun). `--dry-run` implies no lockfile
save and no package.json write. `NPM_CONFIG_IGNORE_SCRIPTS` can only turn scripts off.

**Validation errors** use bun's messages (`invalid cpu: 'x'. valid values: …`,
`BUN_CONFIG_MAX_HTTP_REQUESTS` out of range is an error, `0` warns and uses the default).

**Threading.** Called once per command on the calling thread; reading the four or five
files is sequential and cheap. `Config` is shared as `Arc<Config>`.

**Bun sources.** `ini/lib.rs` (parser, env expansion, auth precedence at lines 1268, 1289,
1375), `bunfig/bunfig.rs` (`[install]` keys, lines 752-828 and 1380-1700),
`bunfig/arguments.rs` (file discovery), `install/PackageManager.rs:1787-1952`
(overlay, npmrc files, env ignore-scripts), `install/PackageManager/PackageManagerOptions.rs`
(`Options::load`, `scope_for_registry_url`, `inherits_credentials`), `bun_core/env_var.rs`.
Tests: `test/cli/install/config-precedence.test.ts`, `npmrc.test.ts`,
`bun-install-registry.test.ts` (scopes, auth).

**Perf.** `Config::load` under 1 ms for typical setups; the no-op fast path computes the
config hash from the same loaded `Config`.

## fnpm-registry

**Responsibility.** Everything network: manifests (abbreviated by default, full on request),
tarball bodies, GitHub commit resolution, `git ls-remote`/clone, and arbitrary registry JSON
endpoints (audit, publish, whoami, view, dist-tag). Owns the HTTP client, auth headers,
retries, the in-flight dedupe and the on-disk manifest cache.

**API** (`crates/fnpm-registry/src/{lib,manifest,cache,git}.rs`):

```rust
pub enum Error { Status { url, status }, NotFound(String), Network { url, message }, Offline(String),
                 InvalidManifest { name, message }, Integrity { url, expected, actual }, Git(String), Io(io::Error) }
pub struct ManifestRequest { pub full: bool, pub revalidate: bool }
pub struct TarballBody { pub url: String, pub content_length: Option<u64>,
                         pub stream: BoxStream<'static, Result<Bytes>> }
#[derive(Clone)] pub struct RegistryClient { /* Arc<Inner> */ }
impl RegistryClient {
    pub fn new(config: Arc<Config>) -> Result<Self>;
    pub async fn manifest(&self, name: &str, req: ManifestRequest) -> Result<Arc<PackageManifest>>;
    pub fn cached_manifest(&self, name: &str) -> Option<Arc<PackageManifest>>;
    pub fn prefetch<I: IntoIterator<Item = String>>(&self, names: I);
    pub async fn tarball(&self, url: &str, expected: Option<&Integrity>) -> Result<TarballBody>;
    pub async fn tarball_bytes(&self, url: &str) -> Result<Bytes>;
    pub fn default_tarball_url(&self, name: &str, version: &str) -> String;
    pub async fn resolve_github(&self, owner: &str, repo: &str, committish: &str) -> Result<String>;
    pub fn github_tarball_url(&self, owner: &str, repo: &str, sha: &str) -> String;
    pub async fn request_json(&self, method: &str, url: &str, body: Option<Value>, otp: Option<&str>) -> Result<Value>;
    pub fn stats(&self) -> RegistryStats;
}
pub struct PackageManifest { pub name, pub dist_tags: BTreeMap<String, String>, pub versions: Vec<VersionManifest>,
    pub time: BTreeMap<String, u64>, pub modified, pub etag, pub last_modified, pub fetched_at_ms: u64,
    pub max_age_secs: u32, pub is_full: bool }
impl PackageManifest {
    pub fn parse(name: &str, json: &[u8]) -> Result<Self>;
    pub fn find(&self, v: &Version) -> Option<&VersionManifest>;
    pub fn dist_tag(&self, tag: &str) -> Option<&VersionManifest>;
    pub fn pick(&self, range: &Range, opts: PickOptions) -> Option<&VersionManifest>;
}
pub struct VersionManifest { pub version: Version, pub dependencies: Vec<(String, String)>,
    pub optional_dependencies, pub peer_dependencies, pub peer_dependencies_meta, pub bundle_dependencies,
    pub bin: Bin, pub platform: PlatformConstraints, pub dist: Dist, pub has_install_script: bool,
    pub deprecated: Option<String>, pub engines, pub scripts /* full only */, pub license }
pub struct Dist { pub tarball: String, pub integrity: Option<Integrity>, pub file_count: Option<u32>, pub unpacked_size: Option<u64> }
pub struct PickOptions { pub include_prerelease: bool, pub minimum_release_age_ms: Option<u64>, pub now_ms: u64 }
pub struct ManifestCache; // open(dir), path_for, load, store, clear
pub mod git { pub async fn resolve_commit(url: &str, c: &Committish) -> Result<String>;
              pub async fn checkout(cache_dir: &Path, url: &str, sha: &str, subdir: Option<&str>, dest: &Path) -> Result<()>; }
```

**Behaviour.**

- `Accept: application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*` for
  abbreviated manifests; full when `req.full`. `If-None-Match` / `If-Modified-Since` from the
  cache; a 304 refreshes `fetched_at_ms`. A cached entry younger than `max_age_secs` (from
  `Cache-Control`, default 300 s) is used without a request (bun `npm.rs:727`,
  `NetworkTask.rs:394`). `--prefer-offline` uses any cached entry; `--offline` never
  requests and fails with `Error::Offline`.
- HTTP/2 (reqwest + rustls, ALPN h2, falling back to h1) with one connection per origin and up
  to `network.concurrency` concurrent streams through a semaphore. Manifests and tarballs use
  two priority classes: tarball requests only take permits when no manifest is queued (bun
  "manifests before tarballs").
- Retries: `network.retries` attempts on connect errors, 5xx and 429, exponential backoff from
  `retry_backoff_ms` with jitter; on errors the concurrency limit drops by 25% (bun
  `runTasks.rs:2069`) and recovers slowly.
- Dedupe: one in-flight future per `(registry, name, full)`; concurrent callers share it.
  Parsed manifests are memoised for the client's lifetime.
- Auth: `Config::authorization_header(url)` for every request; tarball credentials only go to
  the host of the registry that owns them (bun "tarball credential scoping"). Secrets never
  appear in errors (`url` fields are redacted of userinfo and `_authToken` queries).
- Parsing: sonic-rs over the bytes, materialising only the fields in `VersionManifest`;
  versions that fail to parse are skipped (npm). Runs on rayon for documents > 64 KiB.
- Disk cache: `cache_dir/metadata/v1/<xxh3-64 hex>.rkyv`, rkyv archive of a flat record;
  written on a blocking task after a 200, never on the critical path; a corrupt file is
  deleted and refetched.
- `pick`: like npm: if the `latest` dist-tag satisfies, use it; else the highest satisfying
  non-deprecated version; else the highest satisfying. With `minimum_release_age_ms`,
  versions newer than the cutoff are skipped unless the name matches an exclude (needs
  `time`: request `full`).
- GitHub: `GET {GITHUB_API_URL}/repos/{o}/{r}/commits/{ref}` with
  `Accept: application/vnd.github.sha`; tarball `{api}/repos/{o}/{r}/tarball/{sha}`.
- git: shells out to `git` (`GIT_TERMINAL_PROMPT=0`), bare cache per URL under
  `cache_dir/git/`; `#semver:` picks the highest matching tag from `ls-remote --tags`.

**Threading.** Async on tokio; JSON parsing on rayon; disk cache I/O on `spawn_blocking`.
`RegistryClient: Clone + Send + Sync`.

**Bun sources.** `install/npm.rs` (manifest parsing, cache serializer at 1143, version
picking), `install/NetworkTask.rs`, `install/PackageManager/runTasks.rs` (retry/backoff),
`install/PackageManagerTask.rs` (dedupe ids), `install/retry_timer.rs`,
`install/PackageManager/PopulateManifestCache.rs`, `install/git_runner.rs`,
`install/repository.rs`, `runtime/cli/publish_command.rs` + `audit_command.rs` +
`pm_view_command.rs` (endpoint shapes). Tests: `test/cli/install/bun-install-registry.test.ts`,
`bun-install-retry.test.ts`, `bun-install-offline*.test.ts`, `bun-install-proxy.test.ts`,
`bun-install-git-deps.test.ts`.

**Perf.** Cold resolve of the `medium` fixture against the local mirror must issue every
manifest request within the first 20% of wall time (prefetch). Cached manifest load (rkyv)
< 50 µs for a 1 MB packument. No more than one TLS handshake per origin.

## fnpm-store

**Responsibility.** The global content-addressed store (layout in
[On-disk layouts](#on-disk-layouts)): import tarballs and directories, maintain `index.bin`,
prune, report status; plus the low-level file materialisation primitives (`fsops`) the linker
uses.

**API** (`crates/fnpm-store/src/{lib,index,tar,fsops}.rs`):

```rust
pub enum Error { Integrity { package, expected, actual }, InvalidTarball { package, message },
                 PathTraversal { package, path }, Io { path, source } }
pub struct PackageKey { pub name: String, pub version: String, pub integrity: Integrity }
impl PackageKey { pub fn dir_name(&self) -> String; }   // "@scope+name@1.2.3-<16 hex>"
pub struct PackageEntry { pub root: PathBuf /* pkgs/<dir>/package */, pub index: Arc<FileIndex> }
pub struct ImportOptions { pub skip_verify: bool }
#[derive(Clone)] pub struct Store { /* root */ }
impl Store {
    pub fn open(root: &Path) -> Result<Self>;
    pub fn root(&self) -> &Path; pub fn links_dir(&self) -> PathBuf; pub fn package_dir(&self, k: &PackageKey) -> PathBuf;
    pub fn get(&self, k: &PackageKey) -> Result<Option<PackageEntry>>;
    pub async fn import_tarball<S>(&self, k: &PackageKey, body: S, o: ImportOptions) -> Result<PackageEntry>
        where S: Stream<Item = io::Result<Bytes>> + Send + Unpin;
    pub fn import_tarball_bytes(&self, k: &PackageKey, tgz: &[u8], o: ImportOptions) -> Result<PackageEntry>;
    pub fn import_directory(&self, k: &PackageKey, src: &Path) -> Result<PackageEntry>;
    pub fn pack_integrity(dir: &Path) -> Result<Integrity>;
    pub fn register_project(&self, project_dir: &Path) -> Result<()>;
    pub fn prune(&self, o: PruneOptions) -> Result<PruneReport>;
    pub fn status(&self) -> Result<StoreStatus>;
}
pub struct FileIndex { pub dirs: Vec<String>, pub files: Vec<IndexedFile>, pub package_json: Option<String> }
pub struct IndexedFile { pub path: String, pub size: u64, pub mode: u32, pub digest: [u8; 64] }
pub mod tar { pub struct TarParser; pub enum TarEvent<'a> { Entry(EntryHeader), Data(&'a [u8]), EntryEnd };
              pub fn sanitize_entry_path(raw: &str) -> Option<String>; }
pub mod fsops {
    pub use fnpm_config::Backend;
    pub struct StickyBackend; // new(preferred), current(), degrade(to)
    pub fn default_backend() -> Backend;
    pub fn place_file(src: &Path, dst: &Path, sticky: &StickyBackend) -> Result<Backend, MaterializeError>;
    pub fn clone_dir(src: &Path, dst: &Path) -> Result<bool, MaterializeError>;
    pub fn symlink_dir(target: &Path, link: &Path) -> Result<(), MaterializeError>;
    pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()>;
}
```

**Import algorithm.** (1) Stream bytes through the integrity hasher and a zlib-rs inflater
into `TarParser` in one pass (bun `TarballStream.rs`, `extract_tarball.rs`); tarballs whose
`Content-Length` is under 2 MiB are buffered and inflated in one shot with libdeflate.
(2) For each file: BLAKE3 the contents (64-byte extended output, 128 hex digits; the
tarball's own SHA-512/SHA-1 integrity is checked separately with ring), write the blob to `files/<2>/<hex>[-x]` with
`O_EXCL` (exists = already stored, skip), and record it in the index. Hashing and blob writes
fan out to rayon for files > 64 KiB. (3) Build `tmp/<random>/package/` with hardlinks to the
blobs (directories first, from the index), write `index.bin`, then `rename` into
`pkgs/<dir>`; `EEXIST`/`ENOTEMPTY` means another process won, so discard ours
(bun `cache_publish.rs`). (4) Verify the integrity before the rename; mismatch deletes the
staging dir and returns `Error::Integrity` unless `skip_verify`.

Tar rules (bun "tarball extraction path traversal protections"): strip the first path
component; reject absolute, `..`, drive-letter and NUL paths; skip symlink/hardlink/device
entries; modes normalised to 0o644/0o755 (executable if any x bit); pax `path` and GNU
long-name headers supported. `package.json` content is kept in the index.

**Prune.** Read `projects.json`, drop projects whose directory is gone, collect every
`node_modules/.fnpm/state` package list and `.fnpm/<key>` symlink target, delete unreferenced
`links/` then `pkgs/`, then blobs with `st_nlink == 1`.

**fsops.** Backend order per platform: macOS `clonefileat` (whole directory first via
`clone_dir`, per-file fallback), Linux `ioctl(FICLONE)` -> `linkat` -> `copy_file_range`,
Windows `CreateHardLinkW` -> `CopyFileW`. `StickyBackend` degrades process-wide on
`EXDEV`/`ENOTSUP`/`EPERM` so a failing backend is tried once. Directory symlinks are
relative on Unix and junctions on Windows.

**Threading.** `import_tarball` awaits network chunks on tokio and does decompression,
hashing and writes on rayon (or one `spawn_blocking` per package for the one-shot path).
`Store` is `Clone + Send + Sync`; concurrent imports of the same key are safe.

**Bun sources.** `install/TarballStream.rs`, `install/extract_tarball.rs`,
`install/cache_publish.rs`, `install/cache_pack.rs`, `install/PackageInstall.rs:92,305,1544,1840`
(backends), `sys/copy_file.rs`, `install/isolated_install/{FileCloner,FileCopier,Hardlinker,Symlinker}.rs`,
`install/prune.rs`, `runtime/cli/pm_cache_prune_command.rs`. Tests:
`bun-install-tarball-integrity.test.ts`, `bun-install-streaming-extract.test.ts`,
`bun-install-hardlink-fallback.test.ts`, `bun-install-tmpdir-exdev.test.ts`,
`bun-install-cache-publish.test.ts`, `bun-prune.test.ts`.

**Perf.** Extraction of a 1 MB tarball with 100 files < 8 ms on one core (zlib-rs inflate
~1 GB/s, BLAKE3 several GB/s per core). A store hit (`get`) is one `stat` plus an index read (< 50 µs).
Disk: identical files across versions are stored once; `fnpm store status` reports physical
bytes.
## fnpm-workspace

**Responsibility.** The package.json model (typed view + format-preserving editor), finding
the project root, workspace member discovery (package.json `workspaces`, `pnpm-workspace.yaml`
`packages`), root settings from `pnpm-workspace.yaml` (catalogs, overrides,
onlyBuiltDependencies), `--filter` selection and topological order.

**API** (`crates/fnpm-workspace/src/{lib,manifest,filter}.rs`):

```rust
pub enum Error { InvalidPackageJson { path, message }, NoProject(PathBuf), BadPattern { pattern, message },
                 DuplicateName { name, first, second }, BadFilter { filter, message }, NoMatch, Io { path, source } }
pub struct PackageJson { pub name: Option<String>, pub version: Option<String>,
    pub dependencies: Vec<(String, String)>, pub dev_dependencies, pub optional_dependencies, pub peer_dependencies,
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>, pub bundle_dependencies: Vec<String>,
    pub workspaces: Option<Vec<String>>, pub bin: Bin, pub scripts: BTreeMap<String, String>,
    pub platform: PlatformConstraints, pub engines: BTreeMap<String, String>, pub raw: serde_json::Value }
impl PackageJson {
    pub fn parse(text: &str) -> Result<Self>; pub fn read(path: &Path) -> Result<Self>;
    pub fn group(&self, g: DependencyGroup) -> &[(String, String)];
    pub fn dependency_list(&self, include_dev: bool) -> Result<Vec<Dependency>, SpecError>;
    pub fn field(&self, key: &str) -> Option<&Value>;
}
pub struct PackageJsonEditor { pub text: String }
impl PackageJsonEditor { pub fn new(text: String) -> Self;
    pub fn set_dependency(&mut self, g: DependencyGroup, name: &str, spec: &str);
    pub fn remove_dependency(&mut self, name: &str) -> bool;
    pub fn set_path(&mut self, path: &str, v: Value); pub fn delete_path(&mut self, path: &str) -> bool;
    pub fn finish(self) -> String; }
pub struct Member { pub name: String, pub version: String, pub path: String, pub dir: PathBuf, pub manifest: PackageJson }
pub struct WorkspaceSettings { pub catalogs: Catalogs, pub overrides: Vec<(String, String)>,
                               pub only_built_dependencies: Option<Vec<String>> }
pub struct Workspace { pub root_dir: PathBuf, pub root: PackageJson, pub members: Vec<Member>, pub settings: WorkspaceSettings }
impl Workspace {
    pub fn find_root(cwd: &Path) -> Result<PathBuf>;
    pub fn load(root_dir: &Path) -> Result<Self>;
    pub fn member(&self, name: &str) -> Option<&Member>; pub fn member_by_path(&self, p: &str) -> Option<&Member>;
    pub fn member_for_dir(&self, dir: &Path) -> Option<&Member>;
    pub fn topo_levels(&self, members: &[usize]) -> (Vec<Vec<usize>>, Vec<Vec<usize>>);
}
pub struct Selector { pub target: Target, pub exclude: bool, pub include_dependencies: bool,
                      pub include_dependents: bool, pub exclude_self: bool }
pub enum Target { Name(String), Path(String) }
impl Selector { pub fn parse(s: &str) -> Result<Self>; }
pub fn select(ws: &Workspace, selectors: &[Selector], cwd: &Path) -> Result<Vec<usize>>;
```

**Rules.** JSONC-tolerant parsing like bun's package.json parser (comments, trailing
commas); a non-string dependency value is an `InvalidPackageJson` error naming the key.
`workspaces` accepts an array or `{ packages, catalog, catalogs }` (bun reads catalogs from
there too). Globs use globset with `literal_separator`; `!pat` excludes; `node_modules` and
dot-directories are never searched; a member without package.json is skipped with a warning
(bun `lockfile/Package/WorkspaceMap.rs`). Duplicate member names are an error. The editor
preserves indentation (detected from the file), key order, `\r\n`, and the final newline
(bun `PackageJSONEditor.rs`); `set_dependency` keeps a sorted group sorted.

**Filter grammar** (bun `PackageManager/workspace_selection.rs`, `runtime/cli/filter_arg.rs`):
`name`, `@scope/*`, `*` (includes root), `./dir`, `{dir}`, `!sel`, `sel...`, `...sel`,
`^...` (dependencies but not self), `...^`. No match is `Error::NoMatch` with bun's message.

**Threading.** `load` walks directories with rayon and parses member package.json files in
parallel.

**Bun sources.** `install/lockfile/Package/WorkspaceMap.rs`,
`install/PackageManager/{workspace_selection,workspace_manifests,WorkspacePackageJSONCache,PackageJSONEditor,package_json_write_back}.rs`,
`install/pnpm.rs` (pnpm-workspace.yaml fields, lines near 1763),
`runtime/cli/{filter_arg,filter_run}.rs`. Tests: `bun-workspaces.test.ts`,
`bun-add-filter.test.ts`, `bad-workspace.test.ts`, `catalogs.test.ts`,
`test/cli/run/filter-workspace.test.ts`.

**Perf.** `Workspace::load` for 100 members < 5 ms warm cache.

## fnpm-resolver

**Responsibility.** Produce the new `LockGraph` from declared dependencies and the previous
graph. Applies catalogs, overrides (incl. `parent>child`), packageExtensions, patches (record
only), `npm:` aliases, every protocol, `workspace:` and `link-workspace-packages`, peer
resolution (auto-install peers, optional peers, warnings), optional/platform filtering,
`minimumReleaseAge`, `--latest` updates, frozen checks. Streams finished packages for the
fetcher.

**API** (`crates/fnpm-resolver/src/{lib,overrides,prefetch}.rs`):

```rust
pub enum Error { NoMatchingVersion { name, spec }, NotFound(String), WorkspaceNotFound { name },
                 CatalogEntryMissing { catalog, name }, FrozenLockfile { changes: Vec<String> },
                 InvalidSpec { name, spec, message }, Registry(fnpm_registry::Error) }
pub enum Mode { Normal, Frozen, Update { names: Vec<String>, latest: bool } }
pub struct Request { pub workspace: String, pub name: String, pub spec: String, pub group: DependencyGroup }
pub struct ResolveInput<'a> { pub workspace: &'a Workspace, pub previous: Option<&'a LockGraph>,
    pub mode: Mode, pub requests: Vec<Request>, pub removals: Vec<(String, String)> }
pub enum Resolved { Package { id: PackageId } }
pub struct ResolveOutput { pub graph: LockGraph, pub changed: bool,
    pub requests: Vec<(Request, PackageId)>, pub warnings: Vec<String> }
pub struct Resolver { /* config, client */ }
impl Resolver {
    pub fn new(config: Arc<Config>, client: RegistryClient) -> Self;
    pub async fn resolve(&self, input: ResolveInput<'_>, tx: Option<mpsc::UnboundedSender<Resolved>>,
                         events: &dyn EventSink) -> Result<ResolveOutput>;
}
pub mod overrides {
    pub fn parse_overrides(root: &Value) -> (Vec<OverrideRule>, Vec<String>);
    pub fn find<'a>(rules: &'a [OverrideRule], parent: Option<&Package>, name: &str, declared: &str) -> Option<&'a OverrideRule>;
}
pub mod prefetch { pub fn load_hints(cache: &Path, project: &Path) -> Vec<String>;
                   pub fn save_hints(cache: &Path, project: &Path, names: &[String]); }
```

**Algorithm.**

1. Build the root and workspace packages from `Workspace` (edges from
   `PackageJson::dependency_list`, dev included for local packages unless `omit.dev`).
   Root-level fields (`overrides`/`resolutions`, catalogs, patchedDependencies,
   trustedDependencies, packageExtensions) go into the graph.
2. Breadth-first over edges with a work queue. For each edge: apply catalog
   (`catalog:` -> catalog spec; missing entry is an error), then overrides
   (`overrides::find`), then parse the spec.
   - `workspace:` -> the member by name (or path); `link-workspace-packages` also links a
     plain range that a member's version satisfies (bun `Package.rs:2335`).
   - npm / dist-tag: reuse the previous graph's package when it satisfies (Normal mode),
     else `RegistryClient::manifest` + `PackageManifest::pick`. Exact versions skip the range
     parse (fast path). An `npm:` alias edge keeps its alias as `DepEdge::name`.
   - git: `git::resolve_commit`; github: `RegistryClient::resolve_github`; the store import
     later fills the integrity.
   - tarball (remote): resolution is the URL; integrity from the lockfile or computed on
     import. Local tarball / folder / link: paths relative to the declaring package, stored
     relative to the project root; only the root, a workspace or a top-level override may
     declare a folder or link (bun `LINK_PATH_RULE`).
   - Dedupe: a package already in the graph with the same `(name, resolution)` is reused.
     When several versions satisfy, prefer one already in the graph (bun's dedupe), else
     the highest.
3. Peers: after the queue drains, for every package with peer deps, resolve each peer from
   the nearest ancestor's providing edge in the dependents' context; if none and
   `auto_install_peers`, add it as a dependency of the root-most dependent (bun installs
   peers); optional peers that nobody provides stay unresolved; incompatible peers produce
   a warning, not an error.
4. Optional/platform: a package whose `platform` does not match `config.platform` is kept in
   the graph (for the lockfile) but its subtree is not fetched; a failed optional fetch or
   resolve unresolves the edge with a warning.
5. `packageExtensions`: applied to a package's edges when the package is added; the first
   matching extension per name wins; any extension can make a peer optional.
6. Frozen: compare with `previous` (packages, edges, root fields) and return
   `FrozenLockfile` listing the differences.
7. Emit `Resolved::Package` for every package whose resolution is final and that is storable
   and not disabled; emit `Event::Resolve` progress.

**Prefetch.** Before step 2, `prefetch::load_hints` names are sent to
`RegistryClient::prefetch`. While resolving, when a manifest arrives, the dependencies of its
likely-picked version are prefetched before the queue reaches them (bun
`PackageManagerEnqueue.rs:1246,1503`). On success, the manifest request order is saved.

**Threading.** One tokio task drives the queue and keeps up to `network.concurrency`
manifest futures in flight (`FuturesUnordered`); the graph is owned by that task (no locks).
CPU-heavy steps (peer pass, dedupe over large graphs) run on rayon.

**Bun sources.** `install/PackageManager/{PackageManagerEnqueue,PackageManagerResolution,processDependencyList,runTasks,UpdateRequest,add_catalog}.rs`,
`install/lockfile/{OverrideMap,CatalogMap,package_extensions}.rs`, `install/lockfile/Package.rs`
(dependency parsing, workspace linking), `install/npm.rs` (version picking),
`install/resolvers/folder_resolver.rs`, `install/update_transitive.rs`, `install/dedupe.rs`.
Tests: `bun-install.test.ts`, `bun-add.test.ts`, `bun-update.test.ts`,
`bun-update-transitive.test.ts`, `overrides.test.ts`, `nested-overrides.test.ts`,
`catalogs.test.ts`, `bun-install-package-extensions.test.ts`, `bun-install-cpu-os.test.ts`,
`bun-install-link-path.test.ts`, `bun-workspaces.test.ts`.

**Perf.** Warm (all manifests cached and fresh) resolve of 1.5k packages < 40 ms. With a
fresh lockfile and unchanged package.json files, `resolve` does no manifest work at all:
diff the declared edges against `previous` and return it (`changed: false`) in < 2 ms.

## fnpm-lockfile

**Responsibility.** `fnpm.lock` (see [fnpm.lock](#fnpmlock)), the rkyv sidecar, the hoisted
tree algorithm (shared with the linker), and converters for every foreign lockfile, both
directions where writable. Migration keeps resolved versions exactly.

**API** (`crates/fnpm-lockfile/src/{lib,formats,tree,sidecar}.rs`):

```rust
pub enum Error { Parse { path, message }, Unsupported { format: Format }, Io { path, source } }
pub const FNPM_LOCK: &str = "fnpm.lock";
pub enum Format { Fnpm, BunText, BunBinary, Npm, YarnV1, YarnBerry, Pnpm }
impl Format { pub const PRIORITY: [(Format, &str); 8]; }
pub struct Loaded { pub graph: LockGraph, pub format: Format, pub path: PathBuf, pub migrated: bool, pub hash: u64 }
pub fn load(dir: &Path, ws: &Workspace) -> Result<Option<Loaded>>;
pub fn parse(format: Format, bytes: &[u8], ws: &Workspace) -> Result<LockGraph>;
pub fn serialize(format: Format, graph: &LockGraph) -> Result<Vec<u8>>;
pub fn save(dir: &Path, graph: &LockGraph) -> Result<u64>;
pub fn out_of_sync(graph: &LockGraph, ws: &Workspace) -> Vec<String>;
pub struct LockDiff { pub added: Vec<String>, pub removed: Vec<String>, pub changed: Vec<(String, String, String)> }
pub fn diff(a: &LockGraph, b: &LockGraph) -> LockDiff;
pub mod formats { pub mod fnpm; pub mod bun_text; pub mod bun_binary; pub mod npm; pub mod yarn_v1; pub mod yarn_berry; pub mod pnpm; }
//   each: pub fn parse(bytes[, ws]) -> Result<LockGraph>; writable ones: pub fn serialize(&LockGraph) -> String | Result<String>
pub mod tree {
    pub struct TreeNode { pub package: PackageId, pub name: String, pub parent: Option<u32>, pub path: String }
    pub struct Tree { pub nodes: Vec<TreeNode>, pub workspace_roots: Vec<(PackageId, Vec<u32>)> }
    pub struct HoistOptions { pub include: Behavior, pub platform: Platform, pub workspaces: Option<Vec<PackageId>> }
    pub fn hoist(graph: &LockGraph, o: &HoistOptions) -> Tree;
    pub fn resolve_paths(paths: &[(String, PackageId)], from: &str, name: &str) -> Option<PackageId>;
}
pub mod sidecar { pub const SIDECAR: &str; pub fn load(project: &Path, hash: u64) -> Option<LockGraph>;
                  pub fn save(project: &Path, hash: u64, g: &LockGraph) -> Result<()>; }
```

**Load priority** (bun `migration.rs`): `fnpm.lock`, `bun.lock`, `bun.lockb`,
`npm-shrinkwrap.json`, `package-lock.json`, `yarn.lock` (v1 or berry by `__metadata`),
`pnpm-lock.yaml`. `install.lockfile.path` overrides. Migrated graphs get
`config_version = V0` except pnpm (`V1`), matching bun's `choose_config_version`; the
foreign file is left in place and `fnpm.lock` is written on the first successful install
(unless frozen / dry-run).

**Converters.**

| format | read | write | notes |
|---|---|---|---|
| fnpm.lock | yes | yes | bun.lock schema; `deprecated` in INFO |
| bun.lock (v0-v2) | yes | yes | same tuples; configVersion, nested overrides (v3 groups) |
| bun.lockb | yes | no | binary buffers (`lockfile/bun.lockb.rs`, `Buffers.rs`) |
| package-lock v1-v3 | yes | yes (v3) | tree paths -> graph via `resolve_paths`; `link: true` = workspace |
| yarn.lock v1 | yes | yes | needs workspace for `workspace:`/file entries; integrity from `integrity` field |
| yarn berry | yes | no | `resolution`/`checksum` (`yarn_berry.rs`, builtin extensions) |
| pnpm-lock lockfileVersion 7-11 (older rejected, as bun) | yes | yes (v9) | `importers`, `packages`, `snapshots`, catalogs, overrides, patchedDependencies, onlyBuiltDependencies |

Data a format lacks (bin, os/cpu, hasInstallScript for yarn v1) stays default and is filled
from the store's `index.bin` package.json on install; the next save writes it, so after one
install the lockfile is complete and no manifest is ever needed.

**Tree.** `hoist` ports `lockfile/Tree.rs:1412,1782`: breadth-first from the root and each
workspace, place each dependency at the highest ancestor where the name is free and no peer
of a dependent would resolve differently; workspaces get their own `node_modules` for
conflicting deps; disabled packages are not placed. Output order: depth, then path.

**Threading.** Parsing large lockfiles (> 1 MB) splits the `packages` section across rayon;
serialisation formats per package in parallel then concatenates in key order.

**Bun sources.** `install/lockfile/bun.lock.rs`, `install/lockfile/bun.lockb.rs`,
`install/lockfile/Buffers.rs`, `install/lockfile/Tree.rs`, `install/lockfile.rs`
(load/save, `choose_config_version`, trusted dependencies), `install/ConfigVersion.rs`,
`install/migration.rs`, `install/migration/npm_lock.rs`, `install/yarn.rs`,
`install/yarn_berry.rs`, `install/yarn_berry/*`, `install/pnpm.rs`,
`install/lockfile/printer/{tree_printer,Yarn}.rs`, `install/lockfile/OverrideMap.rs`
(serialisation), `runtime/cli/pm_diff_*.rs`. Tests: `bun-lock.test.ts`, `bun-lockb.test.ts`,
`lockfile-version-2.test.ts`, `config-version.test.ts`, `test/cli/install/migration/*`,
the `__snapshots__` of `bun-install.test.ts`.

**Perf.** Parse + graph build of a 1.5k-package fnpm.lock < 8 ms; sidecar load < 1 ms;
serialise < 5 ms; the writer skips the rename when the bytes are identical.
## fnpm-linker

**Responsibility.** Turn a `LockGraph` into `node_modules`: choose the linker, plan the
layout (hoisted tree from `fnpm_lockfile::tree::hoist`, or the isolated layout), materialise
packages from the store with the best backend, maintain the global virtual store, link bins,
remove stale entries, and switch between layouts.

**API** (`crates/fnpm-linker/src/{lib,isolated,bins}.rs`):

```rust
pub enum Error { NodeModules(io::Error), Materialize { package, source: MaterializeError },
                 UnsafeBin { package, bin }, Missing { package, message }, Io { path, source } }
pub fn choose_linker(configured: Option<NodeLinker>, cv: ConfigVersion, has_workspaces: bool,
                     migrated_from_npm: bool, project_dir: &Path) -> NodeLinker;
pub enum Layout { Hoisted(Tree), Isolated(IsolatedLayout) }
pub trait PackageSource: Sync { fn wait(&self, id: PackageId) -> Result<PackageEntry, String>; }
pub struct LinkOptions { pub backend: Backend, pub force: bool, pub fresh: bool, pub threads: usize, pub global_store: bool }
pub struct LinkReport { pub installed: u32, pub removed: u32, pub skipped: u32,
                        pub dirs: Vec<(PackageId, PathBuf)>, pub backend_used: Option<Backend> }
pub struct Linker { /* config, store */ }
impl Linker {
    pub fn new(config: Arc<Config>, store: Store) -> Self;
    pub fn plan(&self, graph: &LockGraph, linker: NodeLinker) -> Layout;
    pub fn link(&self, project_dir: &Path, graph: &LockGraph, layout: &Layout, source: &dyn PackageSource,
                opts: &LinkOptions, events: &dyn EventSink) -> Result<LinkReport>;
}
pub mod isolated {
    pub struct Entry { pub package: PackageId, pub key: String, pub deps: Vec<(String, u32)>, pub global_hash: Option<u64> }
    pub struct IsolatedLayout { pub entries: Vec<Entry>, pub roots: Vec<(PackageId, Vec<(String, u32)>)>,
                                pub hidden_hoist: Vec<(String, u32)>, pub public_hoist: Vec<(String, u32)> }
    pub struct PlanOptions<'a> { pub hoist: bool, pub hoist_pattern: &'a [String], pub public_hoist_pattern: &'a [String],
                                 pub global_store: bool, pub trusted: &'a dyn Fn(PackageId) -> bool }
    pub fn plan(graph: &LockGraph, o: &PlanOptions<'_>) -> IsolatedLayout;
}
pub mod bins {
    pub struct LinkedBin { pub name: String, pub target: PathBuf }
    pub fn link_bins(package_name: &str, bin: &Bin, package_dir: &Path, bin_dir: &Path) -> Result<Vec<LinkedBin>>;
    pub fn unlink_bins(package_dir: &Path, bin_dir: &Path) -> Result<u32>;
}
```

**Linker selection.** `choose_linker` (bun `lockfile.rs:565-600`): configured value wins;
otherwise isolated when configVersion is 1 and there are workspaces (not migrated from npm) or
`node_modules/.fnpm` exists; otherwise hoisted. Switching layouts moves the old
`node_modules` aside to `node_modules/.old_modules-<rand>` and deletes it in the background.

**Hoisted.** Materialise `Tree` nodes in order; a node waits until its parent directory
exists. New `node_modules` (`fresh`): no verification or deletion. Existing: compare each
directory's `package.json` name/version (cheap check, bun `PackageInstall.rs:1232,1352`)
and replace mismatches; delete directories not in the tree. Self-contained workspaces copy.
Bins: `node_modules/.bin` for top-level packages, nested `.bin` for nested ones.

**Isolated.** Entry keys exactly as bun's store paths (`isolated_install/Store.rs:340-508`):
`name@resolution` with `/ \ : # ?` replaced by `+`, resolution capped at 80 bytes (63 bytes
+ `+` + 16 hex hash), `+<16 hex>` peer suffix when the entry's peer set differs. Entries with
the same package and the same resolved peer set are shared. Root/workspaces/links are not
entries; dependents link to their folders. Hidden hoist into `.fnpm/node_modules` when
`hoist`, public hoist into the root by `publicHoistPattern` (pnpm matcher semantics).

**Global virtual store** (on by default; bun's is opt-in): an eligible entry lives at
`<store>/links/<key>-<16 hex closure hash>/node_modules/<name>` and the project's
`.fnpm/<key>` is a symlink to it. Eligibility and hashing follow bun
`isolated_install.rs:1770-2080`: npm, git, github and tarball resolutions only; not trusted
(scripts would write into a shared dir), not patched, no `@types` peer quirk; ineligibility
propagates to dependents; SCCs share one hash (Tarjan); hash = xxh3 over
`[layout version, no_verify] + store path + integrity + sorted (dep name, dep hash)`. Entries
are built in `<entry>.tmp-<rand>` and renamed; EEXIST means another project built it.

**Materialisation.** Per package: if the backend is clonefile and `clone_dir` succeeds, one
syscall; else create every directory in `index.dirs` (batch), then `fsops::place_file` for
each file in `index.files` (no readdir; bun walks, we read the index). Files keep the store's
modes. At most `threads` (default `min(8, cpus)`) packages at a time.

**Bins** (bun `bin.rs`): names normalised (`fnpm_types::bin::normalize_bin_name`), targets
must stay inside the package (`UnsafeBin`), `chmod +x` the target, rewrite a `\r\n` shebang
line to `\n`. First claimant wins (direct dependencies first). Windows: `.cmd`, `.ps1` and
extensionless sh shims instead of symlinks. `Bin::Dir` is the only readdir.

**Threading.** `link` runs on its own rayon pool (`threads` workers), blocking in
`PackageSource::wait` is expected and fine there.

**Bun sources.** `install/hoisted_install.rs`, `install/PackageInstaller.rs`,
`install/PackageInstall.rs`, `install/isolated_install.rs`,
`install/isolated_install/{Installer,Store,FileCloner,FileCopier,Hardlinker,Symlinker}.rs`,
`install/bin.rs`, `install/windows-shim/*`, `install_types/NodeLinker.rs`,
`install/PackageManager/remove_stale_workspace_links.rs`. Tests: `hoist.test.ts`,
`isolated-install.test.ts`, `isolated-relink.test.ts`, `public-hoist-pattern.test.ts`,
`bun-install-backends.test.ts`, `bun-install-hardlink-fallback.test.ts`,
`bun.lockb.bin-linking`, `bunx-directories-bin.test.ts`, `symlink-path-traversal.test.ts`,
`bun-workspaces-self-contained.test.ts`.

**Perf.** Warm isolated install with the global store: one symlink per top-level entry plus
one per project entry; target < 150 ms for the `medium` fixture on Linux (bun's own global
store benchmark: 125 ms). Hoisted warm (store hit) install of `medium` must beat bun's warm
install. Zero `readdir` on the store side.

## fnpm-scripts

**Responsibility.** Decide which lifecycle scripts run (trust policy, default trusted list,
postinstall optimizer), build their environment, and run them concurrently in dependency
order; run single commands for `fnpm run` / `exec` / `x`.

**API** (`crates/fnpm-scripts/src/{lib,trust,optimizer,env}.rs`):

```rust
pub enum Error { Failed { package, script, code }, Signaled { package, script, signal }, Spawn { shell, source } }
pub const LIFECYCLE: [&str; 6] = ["preinstall", "install", "postinstall", "preprepare", "prepare", "postprepare"];
pub struct Job { pub package: PackageId, pub name: String, pub version: String, pub dir: PathBuf,
                 pub package_json: PathBuf, pub scripts: Vec<(String, String)>, pub optional: bool, pub foreground: bool }
pub struct RunOptions { pub concurrency: usize, pub shell: Option<String>, pub env: Vec<(String, String)>,
                        pub silent: bool, pub project_dir: PathBuf }
pub struct Report { pub ran: u32, pub failed_optional: Vec<String> }
pub async fn run_jobs(levels: Vec<Vec<Job>>, o: &RunOptions, events: &dyn EventSink) -> Result<Report>;
pub async fn run_command(cwd: &Path, event: &str, command: &str, args: &[String], o: &RunOptions) -> Result<i32>;
pub mod trust {
    pub fn default_trusted() -> &'static [&'static str];
    pub struct TrustPolicy { pub explicit: Option<BTreeSet<String>>, pub extra: BTreeSet<String> }
    impl TrustPolicy { pub fn from_graph(g: &LockGraph, extra: impl IntoIterator<Item = String>) -> Self;
                       pub fn is_trusted(&self, g: &LockGraph, id: PackageId) -> bool; }
    pub struct Blocked { pub package: PackageId, pub name: String, pub version: String, pub scripts: Vec<String> }
}
pub mod optimizer { pub enum Decision { Run, Skip, NativeBinlink { platform_package: PackageId } }
    pub struct Optimizer { pub native_dependencies: Vec<String>, pub ignore_scripts: Vec<String> }
    impl Optimizer { pub fn from_root(root: &Value, env: &Env) -> Self;
                     pub fn decide(&self, g: &LockGraph, id: PackageId, p: &Platform) -> Decision; } }
pub mod env { pub struct ScriptContext<'a> { .. }
              pub fn build(ctx: &ScriptContext<'_>, inherited: &[(OsString, OsString)]) -> Vec<(OsString, OsString)>; }
```

**Rules** (inventory "Lifecycle script names and order", "trustedDependencies semantics",
"Script environment variables", "Postinstall optimizer"):

- Dependencies run `preinstall`, `install`, `postinstall` (or `node-gyp rebuild` when there
  is a `binding.gyp` and no install/preinstall); git/github deps and the root also run
  `preprepare`, `prepare`, `postprepare`; workspaces run preinstall/install/postinstall/prepare.
- Trust: root and workspaces always; others only if trusted. No `trustedDependencies` field
  -> default list (`install/default-trusted-dependencies.txt`, copied into
  `crates/fnpm-scripts/data/`), npm resolutions from the default registry only. An explicit
  list replaces the default; `[]` trusts nothing. `--trust` adds names and writes them to
  package.json. Untrusted packages with scripts are reported (`blockedScripts` in the
  summary, `fnpm pm untrusted`).
- Order: a package's scripts run after all its dependencies' scripts finished. Up to
  `concurrent_scripts` children; output captured and printed on failure; a failing optional
  dependency is reported and the install continues; a failing required one fails the install
  with `SCRIPT_FAILED`. Root scripts run last in the foreground.
- `--ignore-scripts` skips all dependency and root lifecycle scripts.
- Shell: `sh -c` on POSIX (bun's shell is not available to us), `cmd.exe /d /s /c` on
  Windows; `--shell` / `run.shell` override.

**Threading.** Async on tokio (`tokio::process`), a semaphore for concurrency.

**Bun sources.** `install/lifecycle_script_runner.rs`,
`install/PackageManager/PackageManagerLifecycle.rs`, `install/lockfile/Package/Scripts.rs`,
`install/postinstall_optimizer.rs`, `install/default-trusted-dependencies.txt`,
`install/lockfile.rs:1063-1093,3620-3860`, `runtime/cli/{run_command,exec_command,bunx_command,pm_trusted_command}.rs`.
Tests: `bun-install-lifecycle-scripts.test.ts`, `bun-install-native-binlink.test.ts`,
`bun-run.test.ts`, `bunx.test.ts`, `bun-pm.test.ts` (trust/untrusted).

**Perf.** Script scheduling overhead < 1 ms per job; scripts never block linking of
unrelated packages.
## fnpm-core

**Responsibility.** The orchestrator and the only crate `fnpm-napi` and `fnpm-cli` call. Owns the
runtime, wires crates into the [install pipeline](#install-pipeline), implements every other
command, batches events, writes summaries, and owns the no-op fast path state file.

**API** (`crates/fnpm-core/src/{lib,options,commands,events,fast_path}.rs`):

```rust
pub enum Error { Config(..), Workspace(..), Registry(..), Store(..), Resolve(..), Lockfile(..), Link(..),
                 Scripts(..), Usage { message }, UnknownCommand(Vec<String>) }
impl Error { pub fn code(&self) -> &'static str; }
pub fn version() -> &'static str;
pub fn runtime() -> &'static tokio::runtime::Runtime;
pub async fn install(opts: InstallOptions, events: Arc<dyn EventSink>) -> Result<InstallSummary>;
pub async fn invoke(request: CoreRequest, events: Arc<dyn EventSink>) -> Result<CoreResult>;

pub mod options {   // serde mirrors of packages/fuck-npm/src/core/types.ts (camelCase)
    pub struct PmOptions { pub cwd, pub config, pub yarn, pub production, pub save, pub ca, pub cafile, pub dry_run,
        pub frozen_lockfile, pub force, pub cache_dir, pub no_cache, pub log_level, pub progress, pub summary,
        pub verify, pub offline, pub prefer_offline, pub ignore_scripts, pub trust, pub global, pub env_file,
        pub no_env_file, pub backend, pub registry, pub concurrent_scripts, pub network_concurrency,
        pub save_text_lockfile, pub omit, pub lockfile_only, pub linker, pub minimum_release_age_ms,
        pub minimum_release_age_excludes, pub cpu, pub os, pub libc, pub ca_store }
    impl PmOptions { pub fn to_cli_overrides(&self) -> fnpm_config::CliOverrides; }
    pub enum InstallCommand { Install, Add, Remove, Update }
    pub struct InstallOptions { #[serde(flatten)] pub pm: PmOptions, pub command, pub packages: Vec<String>,
        pub group: Option<DependencyGroup>, pub save_prod, pub exact, pub filter: Vec<String>, pub analyze,
        pub only_missing, pub catalog: Option<String>, pub latest, pub recursive, pub update_groups, pub depth }
    pub struct CoreRequest { pub command: Vec<String>, pub cwd: PathBuf, pub positionals: Vec<String>,
        pub passthrough: Vec<String>, pub options: Map<String, Value>, pub json: bool }
    pub struct CoreResult { pub exit_code: i32, pub data: Option<Value>, pub lines: Option<Vec<String>> }
}
pub mod events { pub const FLUSH_INTERVAL: Duration;   // 50 ms
    pub struct BatchingSink; // new(deliver), flush(), start() -> FlushGuard; impl EventSink }
pub mod fast_path { pub const STATE_FILE: &str; pub struct State { .. }; pub enum Check { Noop { packages }, Changed(Option<State>) };
    pub fn check(project: &Path, config_hash: u64) -> Check; pub fn write(project: &Path, s: &State) -> io::Result<()>; }
pub mod commands { pub async fn dispatch(req: CoreRequest, events: Arc<dyn EventSink>) -> Result<CoreResult>; }
```

**Commands** (`request.command` path -> module), each returning `CoreResult` with `data`
(the `--json` payload) and pre-rendered `lines` for human output:

| path | does | bun source (`runtime/cli/`) |
|---|---|---|
| `outdated` | table of current/update/latest per workspace | `outdated_command.rs` |
| `why <pat>` | dependency chains to matching packages | `why_command.rs` |
| `ls` | tree (`--all`, `--depth`) | `package_manager_command.rs` (ls) |
| `info <pkg>` / `pm view` | registry metadata | `pm_view_command.rs` |
| `dedupe` | re-resolve preferring shared versions | `dedupe_command.rs`, `install/dedupe.rs` |
| `prune` | remove extraneous packages | `prune_command.rs` |
| `link` / `unlink` | register / consume global links (`~/.fnpm/global/links`) | `link_command.rs`, `unlink_command.rs` |
| `patch` / `patch-commit` | prepare an editable copy, write `patches/*.patch` | `patch_command.rs`, `patch_commit_command.rs`, `install/patch_install.rs` |
| `audit` (+ `fix`) | bulk advisory endpoint | `audit_command.rs`, `install/audit_fix.rs` |
| `pack` / `publish` | deterministic tarball; publish with OTP/provenance rules | `pack_command.rs`, `publish_command.rs` |
| `run` / `exec` / `x` / `create` | scripts and bins; `x` installs into `cache/dlx` | `run_command.rs`, `exec_command.rs`, `bunx_command.rs`, `create_command.rs` |
| `init` | new package.json | `init_command.rs` |
| `cache` (`rm`, `prune`, `pack`, `unpack`) | manifest cache and store maintenance | `pm_cache_prune_command.rs`, `install/cache_pack.rs` |
| `store` (`path`, `status`, `prune`) | fnpm store | (fnpm) |
| `pkg` (`get`, `set`, `delete`, `fix`) | package.json edits | `pm_pkg_command.rs` |
| `pm …` | bin, ls, hash, cache, trust, untrusted, default-trusted, version, licenses, whoami, scan, diff, fetch, migrate | `package_manager_command.rs`, `pm_*_command.rs`, `scan_command.rs` |
| `doctor` | store filesystem, backend support, registry reachability, config dump | (fnpm) |
| `whoami` | registry user | `package_manager_command.rs` |
| `migrate` / `lock migrate` | write fnpm.lock from a foreign lockfile | `install/migration.rs` |
| `lock export|verify|diff|why|dedupe|audit|print` | `fnpm-lock` toolkit | (fnpm) |

**Fast path state** (`node_modules/.fnpm/state`, JSON): `version`, `fnpmVersion`,
`lockfileHash` (xxh3-64 of fnpm.lock), `manifests` (`[path, size, mtimeNs, xxh3]` per
package.json), `configHash` (xxh3 of the install-relevant config: linker, backend, omit,
platform, registry URLs, ignoreScripts, production, globalStore), `nodeModules`
(`[dev, ino, mtimeNs]`), `linker`, `packages`. `check` returns `Noop` only if every field
matches; a package.json whose size and mtime match is not read. Written last, after a fully
successful install; deleted at the start of any install that will modify `node_modules`.

**Summary.** `InstallSummary` (fnpm-types) is filled exactly as bun prints: `added` for new
direct dependencies, `updated` for changed direct ones, `requested` for add/update targets
with their bins, `installedCount`, `removedCount`, `checked` for no-op, `failedCount`,
`blockedScripts`, `elapsedMs`. The CLI renders it.

**Threading.** `install`/`invoke` run on `runtime()`. The pipeline is one async function that
spawns: the resolver task, a fetch task per package (bounded by the registry semaphore), the
link job on the linker pool, and the script runner. A `crossbeam-channel` or tokio
`watch` per package implements `PackageSource::wait`.

**Errors.** `Error::code` values are part of the CLI contract: `FROZEN_LOCKFILE`,
`NO_MATCHING_VERSION`, `NOT_FOUND`, `INTEGRITY`, `NETWORK`, `SCRIPT_FAILED`,
`INVALID_PACKAGE_JSON`, `USAGE`, `CORE_ERROR`. Add codes, do not rename them.

**Bun sources.** `install/PackageManager/install_with_manager.rs` (pipeline, summary at
1264-1530), `install/PackageManager/updatePackageJSONAndInstall.rs`,
`install/PackageManager/add_remove_with_filter.rs`, `install/PackageManager/security_scanner.rs`,
`runtime/cli/install_command.rs`, and the command files in the table.

**Perf.** No-op install end to end through the native launcher (process start, argv, config,
fast path, summary) must beat bun on every fixture; on the small fixture that is < 7.2 ms on the
Linux baseline machine, and the target is < 3 ms. Event batching costs < 1% CPU on a cold
install. See [startup budget](#startup-budget).

## fnpm-napi

**Responsibility.** The cdylib loaded by `@fuck-npm/binding`. Converts JS values to
`fnpm_core` options with serde, runs the core on its runtime, delivers batched events through
a `ThreadsafeFunction`, and rejects promises with `error.code = fnpm_core::Error::code()`.
Sets mimalloc as the global allocator. No package-manager logic of its own.

**API** (`crates/fnpm-napi/src/lib.rs`; the JS view is `packages/fuck-npm-binding/index.d.ts`):

```rust
#[napi] pub fn version() -> String;
#[napi] pub fn install<'env>(env: &'env Env, options: serde_json::Value,
                             on_events: Option<EventsFn>) -> napi::Result<Object<'env>>;   // Promise<InstallSummary>
#[napi] pub fn invoke<'env>(env: &'env Env, request: serde_json::Value,
                            on_events: Option<EventsFn>) -> napi::Result<Object<'env>>;    // Promise<CoreResult>
type EventsFn = ThreadsafeFunction<Value, Unknown<'static>, Value, Status, /* callee handled */ true>;
```

**Call flow.**

1. On the JS thread: create a promise (`Env::create_deferred`), convert the payload to JSON
   and deserialise it (`InstallOptions` / `CoreRequest`, unknown fields ignored). Integral
   doubles become integers first (napi-rs only does so up to `u32::MAX`, which would break
   `minimumReleaseAgeMs` beyond 49 days). Neither function throws: a payload that does not
   convert or deserialise (`undefined`, functions, symbols, `NaN`, wrong types), a relative
   or missing `cwd` (the core would otherwise use the Node process's directory), and an
   `onEvents` that is not a function all reject right away with `USAGE`
   (`invalid install options: <reason>`), without touching the runtime.
2. Spawn the work on `fnpm_core::runtime()`. The functions are not napi `async fn`s on
   purpose: those run on napi-rs's own tokio runtime, which would mean a second thread pool
   and a hop between runtimes. For the same reason the `napi` crate is built without
   `tokio_rt`, which would start that runtime (a thread per CPU) when the addon loads.
3. With `onEvents`, the sink is a `BatchingSink` whose deliver closure serialises the batch
   to a JSON array and calls the TSFN non-blocking (unbounded queue) as `(null, events)`.
   fnpm-napi runs the 50 ms flush timer itself rather than `BatchingSink::start`: that
   guard aborts the timer task and flushes at once, so a timer flush still running on
   another worker could deliver after the final batch, or after the listener closed. Here
   the final flush waits until the timer task has stopped. Without `onEvents` the sink is
   `NoopSink` and no event is built.
4. Every batch is sent with a return-value callback that releases an ack once the JS
   callback has returned. The promise settles through a different Node queue, so before
   settling the task waits until every ack is back (`listener::Listener::close`), then drops
   the TSFN, so it no longer keeps the event loop alive. The last progress batch therefore
   always arrives before the summary.
5. Settle on the JS thread: resolve with the result as JSON, or reject with an `Error`
   created there with `code` and `message` set (the rejection is that exact object).

**Errors and panics.** Codes are `fnpm_core::Error::code()` (`NOT_IMPLEMENTED` for commands of
a later parity phase). A panic in the core (in
`runtime()` or the spawned work) is caught and rejects with `CORE_ERROR` and
`internal error in the fnpm core: <panic message>`; the runtime keeps running. This needs
unwinding, so the addon is built with the `release-napi` profile (`release` with
`panic = "unwind"`); the native executables keep `panic = "abort"`. If `onEvents` throws,
the exception is raised as an uncaught exception (like an `EventEmitter` listener's), later
batches are still delivered, and the promise still settles.

**Build.** `npm run build:native -w @fuck-npm/binding` (`napi build --platform --profile
release-napi`, `@napi-rs/cli` pinned) writes `fnpm.<triple>.node` and the generated
`native.d.ts` (not committed) into `packages/fuck-npm-binding`. `build:native:test` is a
debug build with the cargo feature `testing`, which adds `__testDeliver` and
`__testRejectNow`: they drive the delivery, ack and rejection plumbing on a private runtime,
independent of what the core does, and `__testEmit`, which emits events through the same
`run` path and batching sink as `install`. `.github/workflows/napi.yml` builds all eight targets
(glibc with `--use-napi-cross`, linked against glibc 2.17; musl with cargo-zigbuild and
`-mevex512` for libdeflate's AVX-512 code under zig; on musl mimalloc uses local-dynamic TLS,
since an initial-exec TLS model crashes Node while it dlopens the addon), runs `cargo test`, clippy and
the vitest suite on Linux x64/arm64, macOS arm64 and Windows x64, and loads every release
build on its own platform (musl ones in Alpine) to run `tests/smoke.cjs`: a `USAGE`
rejection, an empty-project install with events, and the no-op fast path.

**Tests.** `cargo test -p fnpm-napi`: payload parsing (camelCase, unknown fields, wrong
types, enum values, integral doubles, absolute `cwd`), error-code mapping, panic payloads,
and ack accounting with concurrent producers and a slow consumer. `npm test -w
@fuck-npm/binding` (vitest, loads the built addon): loader triples, the not-found report and
that an installed copy never loads development builds, no threads started by loading,
version, `USAGE` rejections instead of exceptions for every malformed payload, `cwd` and
callback, event batches in order and all before settling (also before a rejection, with 2000
batches, and through the batching sink while its timer flushes), progress coalescing, a
throwing listener, the process exiting once the promise settles, panics as `CORE_ERROR`, and
real installs against an in-memory registry (`tests/helpers/registry.mjs`): `add` with events
then the no-op fast path and `pm ls`, `NOT_FOUND`, `NO_MATCHING_VERSION`, `FROZEN_LOCKFILE`,
`INVALID_PACKAGE_JSON`, and concurrent installs in one process.
`packages/fuck-npm/tests/unit/core/native.test.ts` runs the CLI's `Core` (`loadCore`) over the
same build: options the CLI builds for every install flag deserialize in Rust, events reach
the sink, and rejections become `CoreError` with the core's code.

**Perf.** `npm run bench -w @fuck-npm/binding` measures load time, a sync call, a rejected
payload, an empty async round trip, 1/100/1000 event batches, and a no-op install of an empty
project (the core's fast path end to end). With the `release-napi` build on the Linux baseline
machine: load about 2 ms (`dlopen`) / 4 ms (`require` of `index.js`), no-op install of the
small fixture 0.23 ms in process, about 10 µs per event batch.

## fnpm-cli

**Status: planned** (decided 2026-10-25, `docs/DECISIONS.md`). The crate does not exist yet.

**Responsibility.** The `fnpm` and `fnpm-lock` executables. For the hot commands it parses argv,
calls `fnpm_core::install` in process, and renders progress and the summary itself. Everything
else is handed to the TypeScript CLI by exec. It has no package-manager logic, and wherever it
does something the TS CLI also does (parsing, validation, messages, output), the result must be
byte-identical. The conformance suite runs against the native binary, so any divergence shows
up as a failing test.

**Dispatch.** Decided from argv alone, before any file is read:

| invocation | handled by |
|---|---|
| `fnpm install` / `i`, `ci`, `add` / `a`, `remove` / `rm` / `r` / `uninstall`, `update` / `up` (including `-g`, `--filter`, foreign npm/yarn/pnpm flags) | native |
| `fnpm --version` / `-v` with no other arguments | native (version compiled in; equals the npm package version) |
| any of the above with `-h` / `--help`, or `update -i` / `--interactive` | node |
| every other command (`outdated`, `why`, `pm …`, `run`, `x`, `publish`, …), `help`, `completions`, `--tree`, unknown commands | node |
| every `fnpm-lock` invocation except `--version` | node |
| a native command with an invalid flag or value | native: the error the TS CLI prints, from the same message templates |

"Node" means: exec `node <pkg>/dist/fnpm.js` (or `fnpm-lock.js`) with the original argv after the
program name, unchanged. On Unix this is `execvp` (`std::os::unix::process::CommandExt::exec`),
so the node process takes over the pid, the terminal, signals and the exit status. On Windows the
launcher spawns node with inherited stdio, ignores console Ctrl-C itself
(`SetConsoleCtrlHandler`) so the child handles it, and exits with the child's code.

- **Finding node:** `FNPM_NODE` if set, else `node` on `PATH`. If none is found the launcher
  prints `fnpm: '<cmd>' needs Node.js 20.19 or newer, and no 'node' was found on PATH` and exits 1.
  The native commands never need Node.
- **Finding the JS entry:** `FNPM_JS_ENTRY` if set (development), else
  `<dir of realpath(current_exe)>/../dist/<bin>.js`, which is where the postinstall swap leaves the
  binary relative to the bundle (see [packaging](#packaging-and-distribution)).
- **Environment:** passed through untouched (including `NODE_OPTIONS`), plus
  `FNPM_LAUNCHER=<path of the native binary>` so the TS CLI can tell it was launched natively.

**cli-spec.json.** The command tree and flags stay defined once, in
`packages/fuck-npm/src/cli/trees/*.ts`. A build step in that package (`npm run build:spec`,
planned) serialises the registry to `crates/fnpm-cli/cli-spec.json`: for both bins, every command
node (names, aliases, hidden), its flags (long and short names, whether a value is taken, value
enums, repeatable, negatable, defaults), flag conflicts, the foreign-flag translation and ignore
tables (`foreign-flags.ts`), and the usage-error message templates. The file is committed, so
`cargo build` never needs Node; a vitest test regenerates it and fails when the committed copy is
stale. `crates/fnpm-cli/build.rs` reads it and generates static lookup tables into `OUT_DIR`
(no JSON parsing at run time). The native parser only interprets the subtrees it dispatches
natively; for anything else, "is it native" is all it has to answer.

**Exit codes and errors.** The same as the TS CLI (`src/cli/runtime.ts`,
`src/commands/install.ts`): 0 on success, 1 on any failure, including usage errors, core errors
and an install with `failedCount > 0`. Usage errors print `error: <message>` and the
`Run 'fnpm <cmd> --help' for detailed usage.` hint on stderr. Under `--json`, success prints
`{ok: true, command, summary}` and failure prints `{ok: false, error: {code, message}}` on stdout,
with codes from `src/errors.ts` and `fnpm_core::Error::code`. SIGINT during a native install
cancels the pipeline, removes staging directories, leaves the previous `fnpm.lock` and state
file untouched, and exits 130 (the TS CLI doesn't handle SIGINT yet and must be brought in
line). A node child's status is passed through as is.

**Output.** A Rust port of `output/output.ts` (mode selection: json, rich on a TTY, plain when
piped; colour from `NO_COLOR` / `FORCE_COLOR` / TTY via the same `colorEnabled` rules),
`output/progress.ts` (one stderr line, redrawn at most every 50 ms, off for `--silent` / `--quiet` / `--no-progress`, `FNPM_PROGRESS=0` / `BUN_INSTALL_PROGRESS=0`,
CI markers and non-TTY stderr; warnings printed above the line) and `output/summary.ts` (bun's
summary, line for line, including `[12.34ms]` / `[1.23s]` elapsed formatting and the ASCII
arrows without colour), plus the `fnpm <cmd> v<version>` header line on stderr. Events arrive through an
in-process `EventSink`, so nothing is serialised to JSON. Shared golden fixtures (summary and
event sequences with expected plain and coloured output) are checked by both vitest and
`cargo test -p fnpm-cli`, so the two renderers can't drift.

**Threading.** `main` builds `fnpm_core::runtime()` only on the native path; the exec path does
no allocation-heavy work and touches no runtime. mimalloc is the global allocator.

**Perf.** See [startup budget](#startup-budget). Process start to dispatch decision < 0.5 ms;
`--version` < 1 ms.

## napi JS API and the event schema

The JS surface is `packages/fuck-npm-binding/index.d.ts`; the CLI-side types are
`packages/fuck-npm/src/core/types.ts`. Both sides must change together.

```ts
export function version(): string;
export function install(options: InstallOptions, onEvents?: (err: Error | null, events: CoreEvent[]) => void): Promise<InstallSummary>;
export function invoke(request: CoreRequest, onEvents?: (err: Error | null, events: CoreEvent[]) => void): Promise<CoreResult>;
// Never throws. Rejections: Error with `code` in FROZEN_LOCKFILE | NO_MATCHING_VERSION | NOT_FOUND |
//             INTEGRITY | NETWORK | SCRIPT_FAILED | INVALID_PACKAGE_JSON | USAGE | NOT_IMPLEMENTED | CORE_ERROR
// `cwd` (in options and request) must be absolute.
```

Events (`fnpm_types::event::Event`, serde `tag = "type"`, camelCase fields), delivered as
arrays at most every 50 ms; progress events of one type are coalesced to the latest, others
arrive in order:

```ts
type CoreEvent =
  | { type: "resolve"; resolved: number; total: number; name?: string }
  | { type: "download"; done: number; total: number; bytes?: number; name?: string }
  | { type: "extract"; done: number; total: number; name?: string }
  | { type: "link"; done: number; total: number }
  | { type: "script"; package: string; script: string; phase: "start" | "end"; exitCode?: number }
  | { type: "warn"; message: string; code?: string }
  | { type: "error"; message: string; code?: string }
  | { type: "summary"; summary: InstallSummary };

type InstallSummary = {
  added: { name: string; version: string; alias?: string; availableVersion?: string }[];
  updated: { name: string; from: string; to: string; availableVersion?: string }[];
  removed: string[];
  requested: { name: string; version: string; realName?: string; bins: string[]; skipped?: boolean }[];
  installedCount: number; removedCount: number;
  checked?: { installs: number; packages: number };
  failedCount: number; blockedScripts: number; elapsedMs: number; noop: boolean;
};
```

Example batch during a cold install:

```json
[{"type":"resolve","resolved":312,"total":540,"name":"react-dom"},
 {"type":"download","done":120,"total":540,"bytes":8388608},
 {"type":"warn","message":"incorrect peer dependency \"react@17.0.2\"","code":"PEER"}]
```

## packages/fuck-npm

The TS CLI (`fnpm` and `fnpm-lock` bins, one tsup bundle). Responsibilities: argument
parsing from a data-driven command registry (`src/cli/registry.ts`, `trees/fnpm.ts`,
`trees/fnpm-lock.ts`), npm/yarn/pnpm flag translation (`foreign-flags.ts`, bun's
`FOREIGN_FLAGS` table), help text, `--json` / rich (Clack, TTY) / plain output modes, the
progress renderer (`output/progress.ts`, one line, redraw at most every 50 ms), summary printing (`output/summary.ts`, bun's exact format), and
interactive flows (`update -i`). It talks to the core only through the `Core` interface in
`src/core/types.ts` (`load.ts` adapts the napi module, `fake.ts` implements it for tests).

With the native launcher, the TS CLI is reached by exec for every command that
[fnpm-cli](#fnpm-cli) does not handle natively. It still implements every command itself,
including install, because it is what runs in development (`node dist/fnpm.js`), under the JS
shim when postinstall scripts were skipped, and on platforms without a native binary. Its
command registry is the single source of the CLI surface: the build emits
`crates/fnpm-cli/cli-spec.json` from it, and the native output renderers are ports of
`src/output/*`, checked against shared golden fixtures.

Startup rules: `module.enableCompileCache()`, the hot path imports nothing heavy (Clack and
help are lazy), the addon is loaded with a direct `require` of the resolved triple, and when
the command is `install` the core's fast path runs before any UI is built. No callback is
passed under `--json` / `--silent`. Tests: vitest with injected fakes.

## packages/fuck-npm-binding

`index.js` resolves the `<platform>-<arch>[-gnu|-musl|-msvc]` triple (musl detected from
`/usr/bin/ldd`, then `process.report`) and loads, in order: `NAPI_RS_NATIVE_LIBRARY_PATH`,
`fnpm.<triple>.node` next to it, `fnpm.<triple>.node` in `crates/fnpm-napi`, the
`@fuck-npm/binding-<triple>` platform package, then raw cargo output in
`$CARGO_TARGET_DIR` or `target/{release-napi,release,debug}` (development). The
`crates/` and `target/` candidates only apply in the repository checkout: in an installed
copy `../../` is the user's `node_modules`, where a dependency named `target` or `crates`
could plant a library that every fnpm process would load, scripts allowed or not.
`index.d.ts` is the contract above. Platform packages are napi-rs standard prebuilt packages
built in CI (`napi build --platform --profile release-napi`); they also carry the native
executables (see [packaging](#packaging-and-distribution)).

## bench

`bench/run.ts` drives hyperfine over fixtures (`small`, `medium`, `large`, `monorepo`) ×
tools (fnpm, bun, pnpm, npm, yarn) × scenarios (`cold`, `cold-lockfile`, `warm`, `noop`,
`add`, `ci`) against a pre-warmed local verdaccio mirror (`bench/mirror.ts`). Disk
(`bench/lib/disk.ts`) is measured two ways: the filesystem's used-space delta (`statfs`) across
a cold install, which is the only number that sees clones, and an `lstat` walk of node_modules
and the tool's store/cache counting physical blocks with hardlinked inodes once. Results go to
`bench/results/<date>-<os>-<arch>.{json,md}`. `bench/gate.ts` exits 1 when fnpm is slower than
bun (beyond `--tolerance`, default 0) in any fixture/scenario bun has a result for, or when fnpm
failed or has no result. fnpm currently runs as `node packages/fuck-npm/dist/fnpm.js` with
`FNPM_STORE_DIR`/`FNPM_CACHE_DIR` pointed at bench-owned directories; once `fnpm-cli` exists
the harness runs the native binary (`target/release/fnpm`), which is what users get after the
postinstall swap. Full methodology: `bench/README.md`.

## tests/registry and tests/conformance

`tests/registry` is bun's test registry ported to `node:http`: packuments (abbreviated and
full), tarballs, dist-tags, scoped names, ETag/304, auth, publish, plus fault injection and
an uplink cache. Rust integration tests may start it as a subprocess or use the in-process
axum fixture server; TS tests import it.

`tests/conformance` runs bun's `test/cli/install/*` tests (ported to vitest) against fnpm:
`harness.ts` mirrors bun's harness (`fnpmExe`, `fnpmEnv` with per-worker `FNPM_STORE_DIR`,
`FNPM_CACHE_DIR` and `FNPM_REGISTRY`). Snapshots are rewritten only where fnpm's output
intentionally differs, and each such difference is listed in `docs/PARITY.md`. A crate is
done when the conformance tests for its PARITY rows pass.

## Packaging and distribution

Status: planned (P6 in `docs/ROADMAP.md`). The shape follows bun's npm distribution
(`submodule/bun/packages/bun-release`), with napi-rs's prebuilt-package conventions.

| package | contents |
|---|---|
| `fuck-npm` | `dist/` (the TS bundle), `bin/fnpm` and `bin/fnpm-lock` (JS shims as published), `install.js` (postinstall), `optionalDependencies` on every `@fuck-npm/binding-<triple>` at the same version |
| `@fuck-npm/binding` | the loader (`index.js`, `index.d.ts`) that finds `fnpm.<triple>.node` |
| `@fuck-npm/binding-<triple>` | `fnpm.<triple>.node`, plus the native `fnpm` and `fnpm-lock` executables (`.exe` on Windows); `os` / `cpu` / `libc` set so package managers install only the matching one |

Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`.

- **Postinstall swap.** `install.js` resolves the platform package, runs its `fnpm --version`
  and checks it matches, then replaces `bin/fnpm` and `bin/fnpm-lock` in the `fuck-npm` package
  with hardlinks to (or copies of) the native executables. Package managers point
  `node_modules/.bin/fnpm` (and global bins) at `bin/fnpm`, so after the swap invoking `fnpm`
  starts the native binary directly, with no Node and no wrapper. The native binary finds the
  bundle at `../dist/` relative to itself.
- **Shim fallback.** If postinstall didn't run (`--ignore-scripts`, or pnpm/bun not trusting
  the package), `bin/fnpm` is still the published JS shim. It resolves the platform package and
  runs the native binary as a child with inherited stdio, forwarding the exit code and signals;
  if there is no binary for the platform, it imports `dist/fnpm.js` and runs in process through
  napi. Either way the CLI behaves the same, only with Node's startup added. `fnpm doctor`
  reports when the shim is in use and how to fix it.
- **Windows.** npm's `cmd-shim` writes `.bin/fnpm.cmd` before postinstall runs and bakes in the
  interpreter from the shebang of whatever `bin/fnpm` is at that moment. bun publishes a
  shebang-less placeholder for this reason. The Windows layout (placeholder vs JS shim, and
  `.exe` naming) is settled in P6 with tests on real npm, pnpm, yarn and bun installs.
- **Size.** The native executable and `fnpm.node` both link `fnpm-core`, so each platform package
  carries the core twice. Accepted for now; the alternative (the TS CLI calling the native
  binary instead of napi) can be revisited after measuring.
- **Versioning and publishing.** Every package shares one version. CI builds
  `cargo build --release -p fnpm-cli` and `napi build --platform --release` per target,
  publishes the platform packages first and `fuck-npm` last. Publishing is manual and needs an
  `NPM_TOKEN` secret.

## Startup budget

Measured on the Linux baseline machine (`bench/results/2026-10-25-linux-x64.md`): bun's no-op
install of the small fixture takes 7.2 ms and its warm install 23.3 ms, while `node -e 0` alone
takes about 47 ms. A Node-hosted CLI can't win the no-op, warm, `add` and `ci` scenarios, which is
why the hot commands are native.

| step (native path, small fixture no-op) | budget |
|---|---:|
| exec to `main` (static binary, mimalloc) | < 1 ms |
| argv parse and dispatch from the compiled-in spec | < 0.1 ms |
| project root, config load (`.npmrc`, bunfig, fnpm.toml, env) | < 0.5 ms |
| fast path: `stat` the state file, lockfile, every package.json, node_modules root | < 0.5 ms |
| summary render and exit | < 0.2 ms |
| **total** | **< 3 ms** (must stay under bun's 7.2 ms) |

Rules that keep it there:

- The tokio runtime and rayon pool are created lazily, only after the fast path says work is
  needed.
- No JSON parsing on the no-op path beyond the state file; package.json files whose size and
  mtime match the state are not read.
- No network, no store access, no lockfile parse on a no-op.
- The exec path to Node does no work beyond locating `node` and the bundle.

Node-hosted commands keep the TS CLI's rules (compile cache, lazy imports); their cost is Node's
startup plus the bundle, which is fine for commands that are not in the benchmark.
