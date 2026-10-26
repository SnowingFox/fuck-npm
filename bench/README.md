# bench

Install benchmarks for fnpm against bun, pnpm, npm and yarn v1. The synthetic matrix uses
[hyperfine](https://github.com/sharkdp/hyperfine) against a local, pre-warmed registry mirror.
The real-project runner below records wall time directly, including process startup. Upstream
tarball URLs pinned in original project locks can still bypass the mirror.

## Quick start

```sh
node bench/mirror.ts prewarm                      # start verdaccio, install every fixture once with npm
node bench/run.ts --fixtures small,medium --runs 5
node bench/run.ts --fixtures large --scenarios warm,noop,cold-lockfile --runs 3
node bench/gate.ts                                # exit 1 if fnpm is slower than bun anywhere
node bench/mirror.ts stop
```

Requirements: Node >= 22.18 (the scripts are TypeScript run through Node's type stripping),
hyperfine >= 1.13, and whichever tools you want to compare on `PATH`. fnpm is run as the native
launcher (`crates/fnpm-cli`, what users run after the postinstall swap): `FNPM_BIN` if set, else
`$CARGO_TARGET_DIR/release/fnpm` (default `target/release/fnpm`, from
`cargo build --release -p fnpm-cli`). Without one it falls back to
`node packages/fuck-npm/dist/fnpm.js`, and is skipped if that has not been built either.

### Flags (`run.ts`)

| flag | default | meaning |
|---|---|---|
| `--fixtures` | all | comma list of `bench/fixtures/*` |
| `--tools` | all | `fnpm,bun,pnpm,npm,yarn` |
| `--scenarios` | all | `cold,cold-lockfile,warm,noop,add,ci` |
| `--runs` | 5 | hyperfine `--runs` |
| `--out` | dated results stem | separate output stem for an integration or review run |
| `--warmup` | 1 | hyperfine `--warmup` |
| `--no-disk` | | skip the disk measurement |
| `--regen-lockfiles` | | regenerate each tool's lockfile even if the fixture is unchanged |
| `--add` | `nanoid@5.1.5` | package spec used by the `add` scenario |

`BENCH_REGISTRY` overrides the registry URL (default `http://127.0.0.1:4873/`). If it is left at
the default and nothing answers there, `run.ts` starts verdaccio itself. A non-default URL must
already be up.

## Fixtures

| fixture | contents | resolved packages (npm) |
|---|---|---:|
| `small` | express plus six popular libraries | ~90 |
| `medium` | the Next.js/T3 app from bun's own `bench/install` | ~95 |
| `large` | webpack, babel, jest, eslint 8, typescript, MUI/react app stack | ~1,500 |
| `monorepo` | 10 workspaces (`packages/*`, `apps/*`) with shared and cross-workspace deps | |

Fixtures contain only manifests. Each tool's lockfile is generated once per
(fixture content, tool version, registry) into `bench/.tmp/locked/<tool>/<fixture>` and reused.
For pnpm, the harness writes `pnpm-workspace.yaml` (with `linkWorkspacePackages: true`) into its
copy of workspace fixtures. Cross-workspace dependencies use `"*"` because npm does not support
the `workspace:` protocol.

## Scenarios

The project is copied to `bench/.tmp/work/<tool>/<fixture>`. hyperfine's `--setup` runs once
before the measured runs and `--prepare` runs before every run; neither is timed.

| scenario | before each run | timed command |
|---|---|---|
| `cold` | no lockfile, no cache, no node_modules | `install` |
| `cold-lockfile` | lockfile; no cache, no node_modules | `install` |
| `warm` | lockfile and warm cache; node_modules deleted | `install` |
| `noop` | everything already installed | `install` |
| `add` | warm cache; node_modules deleted, project files restored, then a full `install` | `add nanoid@5.1.5` |
| `ci` | lockfile and warm cache; node_modules deleted | frozen install (`npm ci`, `--frozen-lockfile`) |

Commands per tool. All tools get `--ignore-scripts` and the registry flag; npm also gets
`--no-audit --no-fund`.

| tool | install | frozen | add |
|---|---|---|---|
| fnpm | `fnpm install` | `fnpm install --frozen-lockfile` | `fnpm add <spec>` |
| bun | `bun install` | `bun install --frozen-lockfile` | `bun add <spec>` |
| pnpm | `pnpm install` | `pnpm install --frozen-lockfile` | `pnpm add <spec>` (`-w` in workspaces) |
| npm | `npm install` | `npm ci` | `npm install <spec>` |
| yarn | `yarn install` | `yarn install --frozen-lockfile` | `yarn add <spec>` (`-W` in workspaces) |

Every tool uses its default linker (bun: hoisted unless the project has workspaces; pnpm:
isolated; npm/yarn: hoisted).

## Isolation

Each (tool, fixture) pair has its own state under `bench/.tmp/state/<tool>/<fixture>`, on the same
filesystem as the project so hardlinks and clones are possible:

- npm: `npm_config_cache`
- pnpm: `--store-dir`, `--config.cache-dir` (also `npm_config_store_dir`/`pnpm_config_*`)
- yarn: `--cache-folder`, `YARN_CACHE_FOLDER`
- bun: `BUN_INSTALL_CACHE_DIR`
- fnpm: `FNPM_STORE_DIR`, `FNPM_CACHE_DIR`

`HOME` and the `XDG_*` dirs point into that state too, so user config (`~/.npmrc`,
`~/.bunfig.toml`, `~/.yarnrc`, pnpm's global rc) doesn't apply. `COREPACK_HOME` is kept
pointing at the real one so corepack doesn't re-download pnpm or yarn. CI-detection variables
(`CI`, `GITHUB_ACTIONS`, ...) are removed because pnpm and others change behaviour under CI.
The registry is forced on every tool with both the CLI flag and env (`npm_config_registry`,
`BUN_CONFIG_REGISTRY`, `YARN_REGISTRY`, `FNPM_REGISTRY`).

hyperfine runs with `--shell=none` and its working directory set to the project, so no shell
startup is included in the timings. fnpm uses `FNPM_BIN` when set, otherwise the native release
launcher when available. The fallback through `node` includes Node startup in its timings.

## Registry mirror

`bench/mirror.ts` writes `bench/.tmp/verdaccio/config.yaml` and starts verdaccio 6.11.1 on
`127.0.0.1:4873` with `npx`. The uplink is registry.npmjs.org through `HTTPS_PROXY`/`HTTP_PROXY`
from the environment; storage is `bench/.tmp/verdaccio/storage`. Metadata `maxage` is 30 days, so
once a package has been fetched the uplink is never contacted again during timed runs.
`prewarm` installs each fixture with npm, and lockfile generation plus hyperfine warmup fetch
anything else a particular tool asks for (e.g. bun's abbreviated metadata, the `add` package).

verdaccio serves every tool, so its own latency (JSON serialization of large packuments, single
Node process) is part of all cold numbers. It's the same for every tool but compresses the
ratios between them compared to the real registry, where request latency dominates.

## Disk measurement

After the timed scenarios, each (tool, fixture) gets two extra installs:

1. Cold install from the lockfile into an empty cache. `sync`, then the filesystem's used space
   (`statfs` total - free) is read before and after. This is the "cold df delta" column. It's the
   only measurement that sees reflinks/clones (APFS `clonefile`, btrfs/XFS `FICLONE`), but
   anything else writing to the same filesystem at the same time shows up as noise.
2. Warm install (cache present, fresh node_modules). Then every `node_modules` under the project
   and every cache/store dir is walked with `lstat`, never following symlinks. Physical size is
   `st_blocks * 512`. Inodes are deduplicated by `(dev, ino)`, so files hardlinked between the
   store and node_modules count once. node_modules is walked first, so "store/cache (extra)" is
   what the cache costs on top of node_modules. "node_modules entries" counts directory entries
   (files, dirs, symlinks) including hardlink duplicates, which is roughly the number of
   filesystem operations a link step has to do. "unique inodes" is across both.

## Output

`bench/results/<date>-<os>-<arch>.{json,md}`. Multiple invocations on the same day merge into
the same file (a re-run replaces the matching fixture/tool/scenario entries). The JSON has the
raw hyperfine times for every run, machine info, the tool versions, the source revision,
the native fnpm SHA-256, and the disk numbers. The
Markdown has one table per fixture with mean ± σ and the ratio to the fastest tool in that row,
a min/max/runs breakdown, and the disk table. Absolute paths are stripped so the files can be
committed.

The integration run in [results/2026-10-26-linux-x64-land.md](results/2026-10-26-linux-x64-land.md)
measures `23cc2210361a8f4b5b53221aa48b76a4dff55163` with native SHA-256 `cd278a4690db221dca51b63a76b9ce929a651bdec9b49b10905b7191f1436aac`.
It contains all 110 requested tool/scenario results (five runs and one warmup each) and 20 disk measurements.
Small and medium include all six scenarios; large and monorepo include cold-lockfile, warm, noop, ci and add.
Fixture lockfiles were generated for all tools after prewarming the mirror; matching tool/fixture stamps
reuse those lockfiles after a runtime-only rebuild. fnpm/Bun package-version sets match on all four fixtures.
The strict gate fails 9/22 Bun comparisons; combined physical usage is below Bun on 4/4
fixtures and pnpm on 4/4. Testing, corpus checks and builds completed before the serial timings.
Historical results retain their original provenance and are not merged into this integration artifact.

## Gate

`node bench/gate.ts [results.json] [--fixtures ...] [--scenarios ...] [--tolerance 0.05]` compares
fnpm with bun for every fixture/scenario that bun has a result for. It exits 1 if fnpm's mean is
slower (beyond the tolerance, default 0), if fnpm failed, or if fnpm has no result. It defaults
to the newest results file.

## Caveats

- verdaccio is slow at serving packuments: it re-parses and filters the stored JSON on every
  request, which takes 0.2 to 2 s for big packages (`next` is 43 MB full, 35 MB abbreviated). Any
  scenario that fetches metadata (`cold` for every tool, and `cold-lockfile` for pnpm 12, which
  re-fetches full metadata for every lockfile entry to verify it) is dominated by that. Tarball
  serving is fast (a 43 MB tarball takes about 0.1 to 0.25 s). A static pre-serialized registry
  (`BENCH_REGISTRY`) gives cold numbers closer to the real CDN-backed registry.
- The OS page cache is not dropped between runs (that needs root). Cold means cold *package
  manager* cache, not cold disk.
- On a shared machine, other processes add variance. Look at σ and min next to the mean.
- yarn v1 and npm resolve differently from bun/pnpm, so the installed trees differ a little in
  size. The entry and inode counts make that visible.

## Real projects

The measured repositories are [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare),
[OpenCode](https://github.com/anomalyco/opencode), [tRPC](https://github.com/trpc/trpc),
[Excalidraw](https://github.com/excalidraw/excalidraw) and [Jest](https://github.com/jestjs/jest).
The [README results tables](../README.md#real-repositories-opennext-opencode-trpc-excalidraw-and-jest)
show every project/tool timing, combined disk and actual project validation up front.
The [complete real-project report](results/2026-10-26-linux-x64-real-projects.md)
contains pinned revisions, raw-sample ranges and package-set differences. It includes
a separate OpenNext script-enabled install with source-built better-sqlite3,
37 test files / 364 tests and a real SQL query; timed installs have scripts disabled.

`real.ts` measures OpenNext Cloudflare, tRPC, Excalidraw, Jest and OpenCode from
pinned git revisions. Sources must be git checkouts named `opennextjs-cloudflare`,
`trpc`, `excalidraw`, `jest` and `opencode` under `--sources` (default:
`FNPM_E2E_DIR` or `~/.cache/fnpm-e2e`). The four corpus revisions are in
`tests/e2e/corpus.json`; OpenNext is pinned in `real.ts`. It creates immutable
`git archive` copies and never installs into the source checkouts.

```sh
node bench/real.ts --root /absolute/empty-scratch --sources /absolute/checkouts \
  --tools fnpm,bun,pnpm,yarn --fnpm /absolute/release/fnpm \
  --source-revision <binary-source-commit> --runs 3 --timeout 0
```

Run on Linux with Node 24.5+ and GNU `timeout`, with the selected package managers
on PATH and a warmed mirror at `BENCH_REGISTRY` (default `http://127.0.0.1:4873/`).
Each project compares fnpm, Bun, and its own package manager. Install, preparation
and verification have no time limit by default; `--timeout <seconds>` and
`--prepare-timeout <seconds>` opt into caps (`0` disables them). npm is covered by
the synthetic matrix; it cannot directly install these projects' original
workspace/catalog/Bun formats.

Preparation installs from the original committed lock without timing it, then
retains the generated native lock and any migrated root manifest/workspace
configuration. Timed scenarios are cold package-manager state with that lock,
warm state with node_modules removed, and no-op. Each uses private HOME/cache/store
and runs three times; medians, ranges, failed attempts, migration warnings and
binary SHA-256 are recorded in `<scratch>/results.json`; command output is in
`<scratch>/runs/<project>/<tool>/logs/`. OS and mirror caches remain warm. Timed installs
disable lifecycle scripts. This runner resumes successful results; changing the
fnpm binary hash invalidates its previous preparation and timings. Use a new
scratch directory when changing other tools or sources. If an opted-in time cap
interrupts a scenario, retain its samples/logs and rerun that failed scenario with
`--timeout 0` to obtain completed measurements; successful results are reused.

Verification restores the source and native lock, installs again, checks declared
dependencies and package-version stability, records foreign OS/CPU exclusions only
when supported by explicit text-lock metadata, and executes a direct project
build/typecheck/test command. The command avoids invoking a package manager that
could silently reinstall another tool's dependency tree. Disk measurement counts
unique inodes across all node_modules and private package-manager state before
project builds; it cannot identify shared reflink extents. Inspect package-set
differences before comparing disk or speed. Project checks cover only the listed
commands, and script-enabled installs require separate validation.

The [2026-10-26 real-project report](results/2026-10-26-linux-x64-real-projects.md)
contains the measurements and limits; the companion JSON preserves raw samples.
Regenerate the portable JSON and Markdown from the scratch results with:

```sh
node bench/real-report.ts /absolute/scratch bench/results/my-real-projects
```

The exporter includes optional `clean-bun-trpc-disk.json` (only if its package set
matches the baseline) and `opennext-lifecycle.json` evidence. It replaces machine
home, scratch and executable paths with portable placeholders. Check the exported
files for private registry/proxy values before publishing them.
