# bench

Install benchmarks for fnpm against bun, pnpm, npm and yarn v1. Every run is timed with
[hyperfine](https://github.com/sharkdp/hyperfine) against a local, pre-warmed registry mirror, so
the numbers measure the package manager, not the network.

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
raw hyperfine times for every run, machine info, the tool versions, and the disk numbers. The
Markdown has one table per fixture with mean ± σ and the ratio to the fastest tool in that row,
a min/max/runs breakdown, and the disk table. Absolute paths are stripped so the files can be
committed.

The review rerun in `results/2026-10-25-linux-x64.json` replaces only the small/medium
fnpm and Bun rows (six scenarios, five measured runs and one warmup, plus disk). Other rows
are from earlier invocations. Its fnpm binary SHA-256 was
`c31a62a0fbba38adf6e18b8701c48a46db4dff9453c640499b40638895b0b3c9`.
These hoisted fixtures were measured before the later isolated-binding normalization and
no-op state-version bump; the corpus records its own binary hash. The strict gate fails
7 of 12 small/medium comparisons. No-op wins do not establish a general speed or disk win.

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
