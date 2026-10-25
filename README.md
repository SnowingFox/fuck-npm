# fuck-npm (`fnpm`)

A Node.js package manager with bun's feature set, built to install faster than bun and to use
less disk than bun or pnpm.

Two binaries: `fnpm` (the package manager) and `fnpm-lock` (a lockfile toolkit).

> **Status: not usable yet.** The foundations are in place (contracts, CLI surface, test
> registry, ported conformance suite, benchmark harness) and the core crates are being built.
> `fnpm install` does not install anything today. See the [roadmap](#roadmap).

## Why

bun showed that a package manager can be an order of magnitude faster than npm. fnpm takes bun's
design (read from its source in `submodule/bun`), keeps compatibility with its behaviour, and adds
the techniques bun doesn't use:

- **Faster than bun in every benchmark scenario:** cold, cold with a lockfile, warm cache,
  no-op re-install, `add`, and frozen CI install, on every fixture. On the small fixture that
  means a no-op under 7.2 ms and a warm install under 23 ms. A gate (`bench/gate.ts`) fails when
  fnpm is slower than bun anywhere.
- **Less disk than bun and pnpm:** fnpm's store deduplicates *files*, not packages, so two
  versions of a package share every unchanged file. Projects get those files as copy-on-write
  clones or hardlinks, not copies.
- **Drop-in:** reads `package-lock.json`, `yarn.lock` (v1 and berry), `pnpm-lock.yaml` and
  `bun.lock`, plus `.npmrc` and `bunfig.toml`, and accepts npm/yarn/pnpm flag spellings.

## Features

The full feature list is [docs/PARITY.md](docs/PARITY.md): 414 rows, each naming the bun source it
ports and the owning crate, ticked once its tests pass (none are yet). In short:

- **Commands:** `install`/`i`, `ci`, `add`, `remove`, `update` (including `-i`), `outdated`,
  `why`, `ls`, `info`, `dedupe`, `prune`, `link`/`unlink`, `patch`/`patch-commit`, `audit`,
  `pack`, `publish`, `run`, `exec`, `x`/`dlx`/`fnpmx`, `create`, `init`, `store`, `doctor`,
  `whoami`, `migrate`, `completions`, and the `pm` subcommands (`cache`, `pkg`, `trust`,
  `version`, `hash`, `licenses`, ...).
- **Layouts:** hoisted (npm-style) and isolated (pnpm-style), with bun's default (hoisted for
  single packages, isolated for workspaces). The global virtual store is on by default for
  isolated installs.
- **Workspaces:** globs, the `workspace:` protocol, `--filter` selectors (`...`, `^`, `!`,
  `{dir}`), catalogs, topological script order.
- **Dependency protocols:** npm ranges and tags, `npm:` aliases, git, GitHub, remote and local
  tarballs, `file:`, `link:`, `workspace:`, `catalog:`.
- **Policy:** overrides/resolutions, `packageExtensions`, patches, trusted dependencies with
  bun's default list, lifecycle scripts, `minimumReleaseAge`, security scanners, `--cpu`/`--os`.
- **Lockfiles:** `fnpm.lock` (bun.lock's text format), migration from every major lockfile on
  first install, and `fnpm-lock migrate | export | verify | diff | why | dedupe | audit | print`.
- **Output:** `--json` everywhere (failures included), a single-line progress view on a TTY,
  plain text when piped, bun's install summary format.

Excluded on purpose: bun-runtime features (running files, `bun build`, `bun upgrade`). fnpm reads
the binary `bun.lockb` for migration but never writes it.

## Benchmarks

Baseline from [bench/results/2026-10-25-linux-x64.md](bench/results/2026-10-25-linux-x64.md):
8-core Xeon, ext4, Node 24.22.1; bun 1.4.4, pnpm 12.15.0, npm 11.19.1, yarn 1.22.22; every tool
against the same pre-warmed local verdaccio mirror. Mean wall-clock time. fnpm has no numbers yet.

| fixture | scenario | fnpm | bun | pnpm | npm | yarn |
|---|---|---:|---:|---:|---:|---:|
| small | cold | pending | **796 ms** | 872 ms | 2.80 s | 3.29 s |
| small | cold-lockfile | pending | **138 ms** | 826 ms | 1.67 s | 2.71 s |
| small | warm | pending | **23.3 ms** | 218 ms | 1.37 s | 1.07 s |
| small | noop | pending | **7.2 ms** | 138 ms | 579 ms | 439 ms |
| small | add | pending | **9.5 ms** | 217 ms | 693 ms | 833 ms |
| small | ci | pending | **23.5 ms** | 212 ms | 1.18 s | 1.06 s |
| medium | cold | pending | **13.3 s** | 14.7 s | 55.6 s | 50.3 s |
| medium | cold-lockfile | pending | **1.21 s** | 30.1 s | 10.8 s | 33.7 s |
| medium | warm | pending | **143 ms** | 475 ms | 9.53 s | 5.60 s |
| medium | noop | pending | **12.5 ms** | 168 ms | 659 ms | 482 ms |
| medium | add | pending | **15.4 ms** | 417 ms | 859 ms | 2.26 s |
| medium | ci | pending | **129 ms** | 517 ms | 9.06 s | 5.81 s |
| large | cold-lockfile | pending | **1.54 s** | 44.7 s | 21.9 s | 41.3 s |
| large | warm | pending | **290 ms** | 1.08 s | 15.6 s | 16.3 s |
| large | noop | pending | **67.7 ms** | 144 ms | 3.37 s | 936 ms |

Total physical disk (node_modules plus the tool's store/cache, hardlinks counted once):

| fixture | fnpm | bun | pnpm | npm | yarn |
|---|---:|---:|---:|---:|---:|
| small | pending | 17.1 MiB | 21.8 MiB | 19.3 MiB | 34.7 MiB |
| medium | pending | 506.5 MiB | 801.6 MiB | 627.2 MiB | 3.7 GiB |
| large | pending | 446.4 MiB | 843.0 MiB | 509.9 MiB | 1.0 GiB |

`cold` (no lockfile) times are dominated by verdaccio, which spends 0.2 to 2 s re-serialising
each large packument, so they compress the gaps between tools. pnpm's `cold-lockfile` is slow
for the same reason (it re-fetches full metadata for every lockfile entry). The `monorepo`
fixture and the remaining `large` scenarios haven't been run yet. Methodology, isolation and
disk measurement are in [bench/README.md](bench/README.md).

## How it works

```text
              fnpm / fnpm-lock   (Rust executable, crates/fnpm-cli)
                      |
      install i ci add remove update         everything else, --help,
      (argv parsed natively, cli-spec.json)  interactive commands
                      |                                |
                      |                  exec node dist/fnpm.js <same argv>
                      |                                |
                      |                    TS CLI (packages/fuck-npm)
                      |                                | napi
                      v                                v
                  fnpm-core  <----------------  fnpm.node (crates/fnpm-napi)
                      |
  config  workspace  registry (HTTP/2)  store (CAS)  resolver  lockfile  linker  scripts
```

- **Native launcher.** Node's startup alone (`node -e 0`, about 47 ms) is slower than bun's whole
  no-op install (7.2 ms). So `fnpm` is a Rust executable that runs the hot commands end to end
  with no Node. Everything else (help, prompts, the long tail of commands) execs the TypeScript
  CLI with the same argv. Commands and flags are defined once, in TypeScript; a build step emits
  `cli-spec.json`, which the Rust parser compiles in. See [docs/DECISIONS.md](docs/DECISIONS.md).
- **Store.** `~/.fnpm/store/v1` holds content-addressed file blobs (SHA-512), package
  directories made of hardlinks to those blobs plus a precomputed file index, and the global
  virtual store. Projects link from it by `clonefile` (APFS), `FICLONE` (btrfs/XFS) or
  hardlink, with copy only as a sticky last resort.
- **Pipeline.** Resolution, download, extraction and linking overlap: each package is fetched as
  soon as it is resolved and linked as soon as it is in the store. Tarballs stream through
  gunzip, untar and hashing in one pass.
- **Beyond bun.** A no-op fast path (`node_modules/.fnpm/state`: exit after a few `stat` calls
  when nothing changed), the per-file CAS, the file index (no `readdir` while linking), HTTP/2
  multiplexing to the registry, a lockfile that holds everything an install needs (zero manifest
  fetches on a frozen install), the global virtual store on by default, and the native launcher.

The full design, including every crate's API, is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Compatibility

- **Lockfiles.** With no `fnpm.lock`, the first install migrates `package-lock.json` /
  `npm-shrinkwrap.json` (lockfileVersion 2 to 4; v1 isn't read), `yarn.lock` (v1 and berry), `pnpm-lock.yaml`
  (lockfileVersion 7 to 11), `bun.lock` or `bun.lockb`, keeping the resolved versions, and
  leaves the old file in place. `fnpm-lock export --to <format>` converts back, so a team can
  adopt fnpm gradually.
- **Config.** `.npmrc` with npm's semantics (scopes, per-host auth, proxies, CA), `bunfig.toml`
  `[install]`, `fnpm.toml` (the same schema plus fnpm keys) and `package.json#fnpm`. Precedence
  is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#configuration-sources-and-environment).
- **Environment.** `FNPM_*` variables, with `BUN_*` and `npm_config_*` honoured as fallbacks.
- **Flags.** npm/yarn/pnpm spellings are translated (`--save-dev`, `--save-exact`,
  `--immutable`, `--shamefully-hoist`, `--node-linker`, ...) or ignored when harmless
  (`--no-audit`, `--no-fund`, ...), following bun's table.

## Repository layout

```text
crates/
  fnpm-types        shared data model, specifiers, crate contracts
  fnpm-semver       npm-compatible versions and ranges
  fnpm-config       .npmrc, bunfig.toml, fnpm.toml, env, CLI precedence
  fnpm-registry     HTTP/2 client, manifest cache, auth
  fnpm-store        CAS, streaming extraction, file index, prune
  fnpm-workspace    discovery, --filter, topological order, package.json edits
  fnpm-resolver     dependency graph, peers, overrides, catalogs
  fnpm-lockfile     fnpm.lock, migration from and export to npm/yarn/pnpm/bun
  fnpm-linker       hoisted and isolated layouts, global virtual store, bins
  fnpm-scripts      lifecycle scripts, trust policy
  fnpm-core         install pipeline and every other command
  fnpm-napi         Node-API addon (fnpm.node) used by the TS CLI
  fnpm-cli          native fnpm / fnpm-lock executables (planned)
packages/
  fuck-npm          TS CLI: command registry, help, prompts, output
  fuck-npm-binding  loader for the platform-specific fnpm.node
tests/
  registry          npm registry test server (node:http)
  conformance       bun's test/cli/install suite ported to vitest (see INDEX.md)
bench/              hyperfine benchmark harness, fixtures, results
docs/               architecture, parity checklist, roadmap, decisions
submodule/bun       bun's source, the reference implementation
```

## Development

Prerequisites: Rust 1.90+, Node 20.19+ (the `tests` workspace needs Node 24.5+ and the bench
scripts 22.18+), git. For benchmarks: [hyperfine](https://github.com/sharkdp/hyperfine) 1.13+ and
the tools you want to compare on `PATH`.

```sh
git clone --recurse-submodules https://github.com/SnowingFox/fuck-npm.git
cd fuck-npm
npm install                                   # root workspaces: packages/*, tests
```

Rust:

```sh
cargo build --workspace
cargo test --workspace                        # or one crate: cargo test -p fnpm-semver
cargo clippy --workspace --all-targets -- -D warnings
```

TS CLI and the native addon:

```sh
npm run build:native -w @fuck-npm/binding     # napi build of crates/fnpm-napi (release)
npm run build -w fuck-npm                     # tsup -> packages/fuck-npm/dist
npm test -w fuck-npm                          # vitest; also run with FORCE_COLOR=0
npm run type-check -w fuck-npm
node packages/fuck-npm/dist/fnpm.js --help
```

Test registry and conformance suite:

```sh
node tests/registry/serve.ts --port 4873      # standalone test registry (--help for options)
npm test -w tests                             # registry harness tests
npm run test:conformance -w tests             # bun's ported install tests, run against fnpm
```

The conformance tests define fnpm's target behaviour and are expected to fail until each feature
lands. [tests/conformance/INDEX.md](tests/conformance/INDEX.md) lists every bun test file and
whether it was ported (101), left as todo (5) or excluded (7). The CLI under test is `FNPM_EXE`,
or `packages/fuck-npm/dist/fnpm.js` by default.

Benchmarks run against a local verdaccio mirror so the network doesn't dominate:

```sh
node bench/mirror.ts prewarm                  # start the mirror, fetch every fixture once
node bench/run.ts --fixtures small,medium --runs 5
node bench/gate.ts                            # exit 1 if fnpm is slower than bun anywhere
node bench/mirror.ts stop
```

Flags and caveats: [bench/README.md](bench/README.md).

To contribute, read the crate's section in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and the
bun sources it points to, implement against the contract in `crates/fnpm-types`, enable the
matching conformance tests, and tick the [PARITY](docs/PARITY.md) rows they cover. Pin every new
dependency exactly in the root `Cargo.toml`. Tests never touch the public internet.

## Roadmap

Deliverables and exit criteria: [docs/ROADMAP.md](docs/ROADMAP.md).

- [x] **P0 Foundations:** crate contracts and `fnpm-types`, the CLI with the full command tree,
  test registry plus 101 ported bun test files, benchmark harness and baselines
- [ ] **P1 First install (in progress):** 11 core crates (built and reviewed on branches,
  merge pending), then the `fnpm-core` install pipeline, napi, the `fnpm-cli` native launcher,
  conformance and benchmark gate
- [ ] **P2 Workspaces and policy:** isolated layout and global virtual store, `--filter`,
  catalogs, overrides, patches, trusted scripts, security scanner
- [ ] **P3 Migrations and `fnpm-lock`:** first-run migration from every lockfile, the
  `fnpm-lock` toolkit
- [ ] **P4 Remaining commands:** outdated, why, ls, dedupe, prune, link, patch, audit, pack,
  publish, run/exec/x, pm, store, doctor
- [ ] **P5 UX parity:** help, errors, progress and summaries matching bun line for line
- [ ] **P6 Distribution:** native + napi CI matrix for 8 targets, npm packages, first release

## License

[MIT](LICENSE)

## Acknowledgements

- [bun](https://github.com/oven-sh/bun): the reference implementation. fnpm ports its algorithms
  and its install test suite; its source is a submodule for that reason.
- [pnpm](https://github.com/pnpm/pnpm): the content-addressable store and the isolated
  `node_modules` layout.
- [npm](https://github.com/npm/cli) and [yarn](https://github.com/yarnpkg/berry): the formats and
  semantics fnpm stays compatible with.
