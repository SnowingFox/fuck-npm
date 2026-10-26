# fuck-npm (`fnpm`)

A Node.js package manager targeting bun's package-management feature set, built to install faster
than bun and to use less disk than bun or pnpm.

`fnpm` is the package manager, `fnpm-lock` the lockfile toolkit, and `fnpx` / `fnpmx`
run package executables.

> **Status: in development.** Native installs, migrations, packing, publishing, patching,
> execution and security commands are wired end to end. Full Bun conformance and the
> requirement to beat Bun on every benchmark remain open. See the [roadmap](#roadmap).

## Install

Install the package from the public npm registry:

```sh
npm install -g fnpm@0.0.2
fnpm --version
fnpm install
fnpx --help
```

The distribution is one npm package, `fnpm`. Its postinstall downloads
only this machine's native archive from [GitHub Releases](https://github.com/SnowingFox/fuck-npm/releases),
checks the embedded SHA-512 digest and executable versions, and installs the
launchers and Rust addon. There are no separate platform npm dependencies.
Node 20.19+ and access to GitHub release assets are required.

On Unix, npm, bnpm, Bun and Yarn classic can replace the bin shim with the native
launcher to avoid Node startup. Windows, pnpm and Yarn Berry keep a Node shim
that starts the native launcher. If lifecycle scripts are disabled or the download
fails during installation, the first `fnpm`, `fnpm-lock`, `fnpx` or `fnpmx` invocation
retries the download. That first invocation needs network access; subsequent runs
use the installed files. With npm versions that require lifecycle approval, use
`--allow-scripts=fnpm` to allow the automatic native-bin swap.

The existing unscoped `fuck-npm` package on npm belongs to another maintainer.
This project's scoped `@tiktok-fe/fnpm@0.0.1` used separate platform packages;
`fnpm@0.0.2` replaces that layout and targets public npm.
See [release preparation](docs/RELEASING.md) for verification and publication.

## Development build

From a source checkout with Node 20.19+ and Rust 1.90+:

```sh
npm ci --ignore-scripts
npm run build -w fuck-npm
npm run build:native -w @fuck-npm/binding
node packages/fuck-npm/dist/fnpm.js --help
node packages/fuck-npm/dist/fnpm.js install --cwd /path/to/project
node packages/fuck-npm/dist/fnpm-lock.js --help
```

This path runs the Node CLI with the Rust addon. Release archives also
contain the native launcher for the startup times shown in the benchmarks.

## Why

bun showed that a package manager can be an order of magnitude faster than npm. fnpm uses bun's
design (read from its source in `submodule/bun`) as its compatibility target. The goals are:

- **Faster than bun in every benchmark scenario:** cold, cold with a lockfile, warm cache,
  no-op re-install, `add`, and frozen CI install, on every fixture. A gate (`bench/gate.ts`)
  compares the measured means from the same run and fails when fnpm is slower than bun anywhere.
- **Less disk than bun and pnpm:** the v2 store shares identical files within each package.
  Projects use copy-on-write clones or hardlinks where supported. Cross-version file
  deduplication was removed to reduce import overhead.
- **Drop-in:** reads `package-lock.json`, `yarn.lock` (v1 and berry), `pnpm-lock.yaml` and
  `bun.lock`, plus `.npmrc` and `bunfig.toml`, and accepts npm/yarn/pnpm flag spellings.

## Features

The full feature list is [docs/PARITY.md](docs/PARITY.md): 414 rows, each naming the bun source it
ports and the owning crate. Checked rows have crate-level tests; command and end-to-end gaps remain open.

- **Commands:** `install`/`i`, `ci`, `add`, `remove`, `update` (including `-i`), `outdated`,
  `why`, `ls`, `info`, `dedupe`, `prune`, `link`/`unlink`, `patch`/`patch-commit`, `audit`,
  `pack`, `publish`, `run`, `exec`, `x`/`dlx`/`fnpx`/`fnpmx`, `create`, `init`, `store`, `doctor`,
  `whoami`, `migrate`, `completions`, and the `pm` subcommands (`cache`, `pkg`, `trust`,
  `version`, `hash`, `licenses`, ...).
- **Layouts:** hoisted (npm-style) and isolated (pnpm-style), with bun's default (hoisted for
  single packages, isolated for workspaces). The global virtual store is opt-in for
  isolated installs.
- **Workspaces:** globs, the `workspace:` protocol, `--filter` selectors (`...`, `^`, `!`,
  `{dir}`), catalogs, topological script order.
- **Dependency protocols:** npm ranges and tags, `npm:` aliases, git, GitHub, remote and local
  tarballs, `file:`, `link:`, `workspace:`, `catalog:`.
- **Policy:** overrides/resolutions, `packageExtensions`, patches, trusted dependencies with
  bun's default list, lifecycle scripts, `minimumReleaseAge`, security scanners, `--cpu`/`--os`.
- **Lockfiles:** `fnpm.lock` (bun.lock's text format), migration from every major lockfile on
  first install, and `fnpm-lock migrate | export | verify | diff | why | dedupe | audit | print`.
- **Output:** structured JSON for supported command results, animated installation progress on a TTY
  with package/script counts, local store reuse, registry traffic and worker settings,
  plain text when piped, bun's install summary format.

Excluded on purpose: bun-runtime features (running files, `bun build`, `bun upgrade`). fnpm reads
the binary `bun.lockb` for migration but never writes it.

## Benchmarks

The documentation site contains the same results on its
[home page](website/src/content/docs/index.mdx),
[benchmark overview](website/src/content/docs/benchmarks/overview.md) and
[real-project details](website/src/content/docs/benchmarks/real-projects.md).
[Run the documentation site locally](website/README.md); no hosted deployment is configured yet.

### Real repositories: OpenNext, OpenCode, tRPC, Excalidraw and Jest

These are installs of five pinned upstream repositories, followed by actual project
builds/type checks/tests. All 14 measured project/tool combinations passed their
listed project check and had zero missing declared dependencies after explicit
foreign-platform exclusions. The [full report](bench/results/2026-10-26-linux-x64-real-projects.md)
and [raw samples](bench/results/2026-10-26-linux-x64-real-projects.json) retain
source revisions, package versions, variation, commands and evidence.

Measured 2026-10-26 on Linux x64/ext4, 8 CPU cores, 15.4 GiB RAM and Node 24.22.1.
Each value below is the **median of three runs, in seconds**. fnpm was version
`0.0.1`, source `048c97d`; these are the measured binary's results, not a new run
of the `0.0.2` distribution. Bun was 1.4.4; project-specific pnpm/Yarn versions
appear in the table. Installs ran to completion without a time cap.

- **Cold lock:** each tool's prepared native lock, empty private package-manager
  cache/store and no `node_modules`; OS and registry mirror caches remain warm.
- **Warm relink:** retain that private cache/store, remove `node_modules`, reinstall.
- **No-op:** retain cache/store and the installed dependency tree, reinstall.

Timed installs disable lifecycle scripts. Build/test validation and the separate
script-enabled OpenNext check are outside the installation timings. npm is covered
by the synthetic matrix below; it cannot directly install these repositories'
original workspace/catalog/Bun formats.

| Project | Tool/version | Cold lock | Warm relink | No-op | Missing deps | Project check |
|---|---|---:|---:|---:|---:|---|
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | bun 1.4.4 | 4.9949 | 0.6028 | 0.0798 | 0 | pass |
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | pnpm 10.33.0 | 17.4433 | 6.8035 | 2.8804 | 0 | pass |
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | fnpm 0.0.1 | 7.2118 | 0.5771 | 0.0089 | 0 | pass |
| [tRPC](https://github.com/trpc/trpc) | bun 1.4.4 | 5.6106 | 1.4238 | 0.1905 | 0 | pass |
| [tRPC](https://github.com/trpc/trpc) | pnpm 12.4.1 | 150.1458 | 2.9970 | 0.4394 | 0 | pass |
| [tRPC](https://github.com/trpc/trpc) | fnpm 0.0.1 | 10.0058 | 0.8811 | 0.0121 | 0 | pass |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | bun 1.4.4 | 2.3536 | 0.3205 | 0.0661 | 0 | pass |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | yarn 1.22.22 | 196.8902 | 18.8943 | 1.0250 | 0 | pass |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | fnpm 0.0.1 | 2.0217 | 0.2277 | 0.0098 | 0 | pass |
| [Jest](https://github.com/jestjs/jest) | bun 1.4.4 | 2.2663 | 0.3629 | 0.0890 | 0 | pass |
| [Jest](https://github.com/jestjs/jest) | yarn 4.18.0 | 39.0481 | 24.3643 | 4.4330 | 0 | pass |
| [Jest](https://github.com/jestjs/jest) | fnpm 0.0.1 | 2.3337 | 0.3237 | 0.0103 | 0 | pass |
| [OpenCode](https://github.com/anomalyco/opencode) | bun 1.4.4 | 11.3630 | 0.7154 | 0.1249 | 0 | pass |
| [OpenCode](https://github.com/anomalyco/opencode) | fnpm 0.0.1 | 9.4839 | 0.6254 | 0.0102 | 0 | pass |

fnpm's median was lower than Bun's for warm relinking and no-op installs on all
five repositories, and cold installs on Excalidraw and OpenCode (2/5). OpenNext,
tRPC and Jest cold installs were slower than Bun. This was a shared machine;
[minimum–maximum ranges](bench/results/2026-10-26-linux-x64-real-projects.md#variation-and-package-sets)
show the variation and do not establish universal speedups.

**Combined physical disk, MiB:** `node_modules` plus each tool's private cache/store,
measured before builds, with shared `(device, inode)` pairs counted once. Shared
reflink extents cannot be identified by this measurement.

| Repository | fnpm | Bun | Project package manager |
|---|---:|---:|---:|
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | 2592.87 | 2653.39 | 2580.16 (pnpm) |
| [OpenCode](https://github.com/anomalyco/opencode) | 3138.05 | 3238.86 | — |
| [tRPC](https://github.com/trpc/trpc) | 2490.91 | 2558.46 | 3541.51 (pnpm) |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | 901.80 | 934.82 | 4294.92 (yarn) |
| [Jest](https://github.com/jestjs/jest) | 805.96 | 828.70 | 1007.64 (yarn) |

fnpm's combined footprint was lower than Bun's on these five repositories.
OpenNext still used slightly more than pnpm. Its fnpm tree contained 1,651 unique
package versions versus Bun's 1,637 (14 extra), so it is not an identical-package
comparison. The fnpm and Bun version sets matched for the other four repositories.
OpenCode's 23 declarations for other OS/CPU targets were explicitly accounted for,
not treated as missing host dependencies. Other baseline package-set differences
are preserved in the full report.

**What ran after installation:**

| Tested repository revision | Workspaces | Successful project validation |
|---|---:|---|
| [OpenNext Cloudflare `8ea9eb9`](https://github.com/opennextjs/opennextjs-cloudflare/tree/8ea9eb973b6d71743e4b37ab2fdf5d98fe4d0ddb) | 23 | Cloudflare package `tsc` + `vitest --run`; separate script-enabled install, root build, 37 test files / 364 tests and a better-sqlite3 SQL query |
| [OpenCode `7a83c69`](https://github.com/anomalyco/opencode/tree/7a83c69fe12f021ec5b8ad10a1171e6d01d99289) | 39 | `packages/util`: `bun check --no-script` |
| [tRPC `8d09833`](https://github.com/trpc/trpc/tree/8d09833c76791b2ea7270af174fdab9f1408aeb8) | 43 | `packages/server`: `tsdown` build |
| [Excalidraw `b7de45e`](https://github.com/excalidraw/excalidraw/tree/b7de45e88111f5680ae4bb3c3f3541c7d3b279fe) | 10 | `tsc` type check |
| [Jest `61050e9`](https://github.com/jestjs/jest/tree/61050e9323e742539dc2360236671110e37329e6) | 70 | `node ./scripts/build.mjs` build |

These checks validate the commands listed, not every upstream test suite.
The separate script-enabled OpenNext run took 240.16 s to install on Linux Node 24,
compiled better-sqlite3 from source, passed its root build and all 364 Cloudflare
tests, and returned `{"value":42}` from a real SQL query. The original pnpm lock
was unchanged; three untrusted scripts were blocked by the default trust policy.
That lifecycle result does not cover Node 26 on macOS. The earlier
[18-project compatibility corpus](tests/e2e/results/2026-10-26-land.md) is recorded
separately as 13 PASS / 5 KNOWN.

### Synthetic fixtures: npm, pnpm, Yarn, Bun and fnpm

Measured 2026-10-25 from `23cc221`: [raw results and variation](bench/results/2026-10-26-linux-x64-land.md).
8-core Xeon, ext4, Node 24.22.1; fnpm 0.0.1, bun 1.4.4, pnpm 12.15.0, npm 11.19.1, yarn 1.22.22.
All tools use the same pre-warmed local Verdaccio mirror. Means below use five measured runs and one warmup.

| fixture | scenario | fnpm | bun | pnpm | npm | yarn |
|---|---|---:|---:|---:|---:|---:|
| small | cold | 696 ms | **554 ms** | 800 ms | 2.95 s | 3.28 s |
| small | cold-lockfile | 141 ms | **123 ms** | 883 ms | 1.61 s | 2.76 s |
| small | warm | **21.1 ms** | 35.3 ms | 212 ms | 1.24 s | 996 ms |
| small | noop | **2.7 ms** | 7.2 ms | 135 ms | 581 ms | 399 ms |
| small | add | 10.8 ms | **9.7 ms** | 221 ms | 662 ms | 824 ms |
| small | ci | **21.1 ms** | 25.0 ms | 271 ms | 1.24 s | 1.07 s |
| medium | cold | **12.6 s** | 12.8 s | 14.4 s | 47.8 s | 47.5 s |
| medium | cold-lockfile | **1.00 s** | 1.16 s | 27.4 s | 11.7 s | 33.6 s |
| medium | warm | **71.6 ms** | 141 ms | 446 ms | 9.97 s | 5.40 s |
| medium | noop | **3.0 ms** | 10.5 ms | 148 ms | 701 ms | 420 ms |
| medium | add | 17.3 ms | **14.3 ms** | 371 ms | 900 ms | 2.02 s |
| medium | ci | **60.0 ms** | 141 ms | 442 ms | 9.85 s | 5.32 s |
| large | cold-lockfile | 1.35 s | **1.10 s** | 42.2 s | 20.3 s | 38.9 s |
| large | warm | **185 ms** | 230 ms | 1.00 s | 15.1 s | 15.4 s |
| large | noop | **2.2 ms** | 62.4 ms | 148 ms | 2.83 s | 770 ms |
| large | add | 76.9 ms | **71.3 ms** | 1.07 s | 3.22 s | 7.14 s |
| large | ci | **172 ms** | 215 ms | 1.01 s | 15.2 s | 15.5 s |
| monorepo | cold-lockfile | 960 ms | **759 ms** | 28.5 s | 12.5 s | 30.5 s |
| monorepo | warm | 176 ms | **157 ms** | 685 ms | 9.65 s | 8.03 s |
| monorepo | noop | **3.2 ms** | 27.2 ms | 143 ms | 1.45 s | 629 ms |
| monorepo | add | 49.1 ms | **30.3 ms** | 4.42 s | 1.49 s | 3.98 s |
| monorepo | ci | **147 ms** | 152 ms | 650 ms | 9.83 s | 7.56 s |

Total physical disk (node_modules plus the tool's store/cache, shared inodes counted once):

| fixture | fnpm | bun | pnpm | npm | yarn |
|---|---:|---:|---:|---:|---:|
| small | 16.7 MiB | 17.1 MiB | 21.7 MiB | 19.3 MiB | 34.7 MiB |
| medium | 502.7 MiB | 506.6 MiB | 777.7 MiB | 627.2 MiB | 3.7 GiB |
| large | 423.0 MiB | 446.4 MiB | 836.7 MiB | 510.0 MiB | 1.0 GiB |
| monorepo | 407.6 MiB | 425.6 MiB | 553.9 MiB | 493.4 MiB | 2.5 GiB |

The strict speed gate fails 9 of 22 comparisons with Bun. fnpm uses less combined physical disk
than Bun on 4/4 fixtures and pnpm on 4/4. The raw report also separates node_modules,
extra store/cache, inode counts and cold filesystem deltas. The cold filesystem delta is lower than
Bun on 4/4 fixtures and pnpm on 4/4; other writes on the shared filesystem affect it.
Fnpm's extra store/cache usage is larger than Bun's on all four fixtures.
`cold` and pnpm cold-lockfile timings include Verdaccio metadata serialization. These Linux measurements
do not establish macOS or Windows performance. See [bench/README.md](bench/README.md) for isolation and caveats.

## How it works

```text
              fnpm / fnpm-lock   (Rust executable, crates/fnpm-cli)
                      |
      install i ci add remove update x create  everything else, --help,
      (argv parsed natively, cli-spec.json)    interactive commands
                      |                                |
                      |                  exec node dist/fnpm.js <same argv>
                      |                                |
                      |                    TS CLI (packages/fuck-npm)
                      |                                | napi
                      v                                v
                  fnpm-core  <----------------  fnpm.node (crates/fnpm-napi)
                      |
  config  workspace  registry (HTTP/2)  store (v2)  resolver  lockfile  linker  scripts
```

- **Native launcher.** Node's startup alone (`node -e 0`, about 47 ms) is slower than bun's whole
  no-op install (7.2 ms). So `fnpm` is a Rust executable that runs the hot commands end to end
  with no Node. Everything else (help, prompts, the long tail of commands) execs the TypeScript
  CLI with the same argv. Commands and flags are defined once, in TypeScript; a build step emits
  `cli-spec.json`, which the Rust parser compiles in. See [docs/DECISIONS.md](docs/DECISIONS.md).
- **Store.** `~/.fnpm/store/v2` holds one directory per package (each file written once,
  identical files of a package sharing an inode) with a precomputed file index, and the global
  virtual store. Projects link from it by `clonefile` (APFS), `FICLONE` (btrfs/XFS) or
  hardlink, with copy only as a sticky last resort.
- **Pipeline.** Resolution, download, extraction and linking overlap: each package is fetched as
  soon as it is resolved and linked as soon as it is in the store. Tarballs stream through
  gunzip, untar and hashing in one pass.
- **Beyond bun.** A no-op fast path (`<cache>/projects/<project-path-hash>/state`: exit after a few `stat` calls
  when nothing changed), within-package file sharing, the file index (no `readdir` while
  linking), HTTP/2 multiplexing to the registry, a lockfile that holds everything an install
  needs (zero manifest fetches on a frozen install), an optional global virtual store,
  and the native launcher.

The full design, including every crate's API, is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Compatibility

Latest integration checks: **3,667 passed / 2,973 failed / 58 skipped**
(55.2%) across 42 completed conformance files out of 45 selected; timeouts and the stopped matrix are listed separately.
The 18-project corpus has **13 PASS / 5 KNOWN / 0 unexpected failures**.
See [conformance status](tests/conformance/STATUS.md) and
[corpus results](tests/e2e/results/2026-10-26-land.md). Full parity remains open.

[Single-package platform CI](https://github.com/SnowingFox/fuck-npm/actions/runs/40294225393)
passed all 21 jobs: eight native builds, eight normal/disabled-script npm tarball
smokes, and five test jobs across Linux, macOS and Windows. The `fnpm@0.0.2`
candidate also passed fresh macOS arm64 installs using the actual GitHub native
archives, covering all four bins, addon loading, TS command dispatch and
offline/frozen reinstalls. The scoped `@tiktok-fe/fnpm@0.0.1` was published through
bnpm; public `fnpm@0.0.2` publication is pending local npm authentication.
See [release status](docs/RELEASING.md#release-history).

## Repository layout

```text
crates/
  fnpm-types        shared data model, specifiers, crate contracts
  fnpm-semver       npm-compatible versions and ranges
  fnpm-config       .npmrc, bunfig.toml, fnpm.toml, env, CLI precedence
  fnpm-registry     HTTP/2 client, manifest cache, auth
  fnpm-store        package store, streaming extraction, file index, prune
  fnpm-workspace    discovery, --filter, topological order, package.json edits
  fnpm-resolver     dependency graph, peers, overrides, catalogs
  fnpm-lockfile     fnpm.lock, migration from and export to npm/yarn/pnpm/bun
  fnpm-linker       hoisted and isolated layouts, global virtual store, bins
  fnpm-scripts      lifecycle scripts, trust policy
  fnpm-core         install pipeline and every other command
  fnpm-napi         Node-API addon (fnpm.node) used by the TS CLI
  fnpm-cli          native fnpm / fnpm-lock executables
packages/
  fuck-npm          TS CLI: command registry, help, prompts, output
  fuck-npm-binding  loader for the platform-specific fnpm.node
tests/
  registry          npm registry test server (node:http)
  conformance       bun's test/cli/install suite ported to vitest (see INDEX.md)
bench/              hyperfine benchmark harness, fixtures, results
docs/               architecture, parity checklist, roadmap, decisions
website/            fnpm documentation site, search and benchmark pages
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
- [ ] **P1 First install (in progress):** core crates, install pipeline, napi and native
  launcher integrated; conformance and benchmark gates remain open
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
