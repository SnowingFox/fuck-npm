# Roadmap

fnpm is done when every non-excluded row of [PARITY.md](PARITY.md) is ticked, every ported bun
install test passes (or its difference is listed as intentional in PARITY.md), and fnpm beats bun
on the whole benchmark matrix while using less disk than bun and pnpm. The phases below get there
in an order where each layer is testable on its own.

Every phase runs the same loop: one implementer per crate or command group on its own branch,
an adversarial review (correctness, cross-platform behaviour, performance), then merge, the
conformance files for the phase, and the benchmark gate.

Status as of 2026-10-26. Legend: **done**, **merged** (implemented, reviewed and on `main`; crate tests green, the
command is not wired end to end yet), **in progress**, **not started**.

## Integrated validation (2026-10-26)

The perf, conformance, publishing, execution and corpus tracks are integrated at `23cc221`.
Store v2 is preserved. Integration fixes cover explicit omitted-peer adds, local-tarball patch identities,
audit repairs replacing installed files, running previously blocked scripts after trust changes,
full scanner scope after node_modules removal, and a single metadata request with a release-age gate.

Platform follow-ups are integrated. Both store and linker define the verified Darwin clone flag
missing from libc 0.2.191, Windows status deduplicates hard links by volume and 128-bit file ID,
and Unix-only helpers have accurate platform guards. Windows patch ancestor preparation preserves
file/directory links; regressions cover nested scopes, shared-store isolation and unmanaged-link refusal.
The ARM GNU addon uses Zig with an explicit glibc 2.17 baseline instead of the obsolete bundled GCC.

The [native branch run](https://github.com/SnowingFox/fuck-npm/actions/runs/40254449613) built both
macOS release addons and passed their smoke tests on Intel and Apple Silicon. Its store tests exposed
APFS leaving access times unchanged after index reads. The store now refreshes those access times at
most once a day while preserving index contents and mtime. Linux follow-up checks passed 249 combined
store/linker/binding tests (3 ignored); the separate Windows track passed 375 Linux tests and strict
Clippy. Windows scripts and focused filesystem/command probes also passed cross-target checks.
The [main CI run for `ff9c309`](https://github.com/SnowingFox/fuck-npm/actions/runs/40256325760)
passed all 21 jobs: eight release addon builds, eight native artifact smoke tests, and five test jobs.
Release artifacts load and execute the smoke install on macOS Intel/Apple Silicon, Linux x64/ARM64
with GNU/musl, and Windows x64/ARM64. Test jobs passed on Linux x64 with Node 20/24, Linux ARM64,
macOS and Windows with Node 24. They cover store/binding tests, patch ancestor regressions, strict
Clippy, binding JavaScript tests and CLI native-adapter tests; the macOS job also runs the linker clone
regression. The combined Linux follow-up passed 526 tests (3 ignored), strict Clippy and formatting;
Windows scripts passed all-target strict cross-Clippy. This validates the addon matrix and selected
platform regressions, not the full workspace/conformance suite or native launcher distribution.

The [complete native distribution run for `00ec69f`](https://github.com/SnowingFox/fuck-npm/actions/runs/40265278514)
subsequently passed all 21 jobs: eight release builds containing both executables and the addon,
eight installed npm tarball smokes, and five native test jobs. The installation smokes cover
normal and `--ignore-scripts` installs on both macOS and Windows architectures and Linux GNU/musl
on x64/ARM64. They verify versions/help, addon loading, a local tarball dependency, and
offline/no-op/frozen reinstalls. Windows smoke exposed a positional parser that replaced paired
backslashes instead of individual separators; the repair and its native regression tests pass.
The scoped `@tiktok-fe/fnpm@0.0.1` and its binding/platform packages were later
published through bnpm. The `fnpm@0.0.2` candidate targets public npm with one
package, eight GitHub native archives, embedded SHA-512 verification and
postinstall or first-use installation. Its
[CI run](https://github.com/SnowingFox/fuck-npm/actions/runs/40294225393) passed
all 21 jobs, including installed tarball smoke on all eight targets with scripts
enabled and disabled. Public npm authentication and final registry publication
remain pending; see [RELEASING.md](RELEASING.md). Other package-manager registry
installs and full conformance remain open.

- Rust workspace: 1,395 passed, 13 ignored across 74 targets; strict Clippy and formatting passed.
- TypeScript CLI: 652 passed; CLI and harness type checks, bundle build and benchmark/corpus strict type checks passed.
- Registry/adapter harness: 52 passed, 1 expected failure, 2 skipped, 1 todo.
- [Conformance](../tests/conformance/STATUS.md): 3,667 passed, 2,973 failed, 58 skipped (55.2%)
  across 42 completed files out of 45 selected; timeout and stopped-file rows are explicit. The other ported files remain unmeasured.
- [Corpus](../tests/e2e/results/2026-10-26-land.md): 13 PASS, 5 KNOWN, 0 unexpected failures across 18 pinned projects.
- [Benchmarks](../bench/results/2026-10-26-linux-x64-land.md): all 110 timing and 20 disk rows measured;
  strict speed gate fails 9/22 comparisons, combined disk below Bun on 4/4 and pnpm on 4/4 fixtures.
- PARITY remains 231 checked / 183 open; no row was completed merely by merging these tracks.

Full Bun conformance, speed in every scenario, extra cache size, complete platform suites and public npm publication remain open.
Scanner matrix TTY callbacks and subprocess resource-usage APIs are still missing in the Node test adapter.
A measured adapter deadlock on reading subprocess output after exit was fixed with eager pipe draining.
Trust transitions currently force a conservative relink of the selected layout so newly enabled scripts
receive rebuilt directories; narrowing that rebuild is a remaining optimization.

## Gates used below

- **Conformance:** `npm run test:conformance -w tests`, against the native binary once it
  exists. A file "passes" when every test in it passes or is skipped with a PARITY.md entry
  explaining the intentional difference. File names are from
  [tests/conformance/INDEX.md](../tests/conformance/INDEX.md).
- **Benchmark:** `node bench/gate.ts` with tolerance 0: fnpm's mean is at most bun's for every
  fixture/scenario in scope, on the Linux baseline machine and on macOS (APFS). Disk: fnpm's
  total physical bytes and cold df delta are below both bun and pnpm for every fixture in scope.
- **Platforms:** the phase's test suites green in CI on the listed OS/filesystem pairs.

## P0: Foundations (done)

| deliverable | status |
|---|---|
| Cargo workspace, pinned dependencies, release profile | done |
| `docs/ARCHITECTURE.md` crate contracts, `docs/PARITY.md` (414 rows), `docs/inventory/bun-pm.json` | done |
| `fnpm-types`: shared data model, dependency specifiers, `InstallSummary`, events | done |
| Skeletons for every other crate matching the contracts | done |
| TS CLI (`packages/fuck-npm`): full command tree for `fnpm` and `fnpm-lock`, flag parsing, foreign-flag translation, help, output modes, progress and summary renderers, napi loader | done |
| `tests/registry` (bun's test registry on `node:http`) and 101 ported bun test files in `tests/conformance` | done |
| `bench/` harness, fixtures, verdaccio mirror, gate; baselines for bun, pnpm, npm, yarn (`bench/results/2026-10-25-linux-x64.md`) | done |

## P1: First install (in progress)

Goal: `fnpm install`, `ci`, `add`, `remove` and `update` work natively on single-package
projects with the hoisted linker, from an empty or warm store, with or without `fnpm.lock`.

| deliverable | branch | status |
|---|---|---|
| fnpm-semver: npm versions and ranges, bun's satisfies/pick-best | `feat/semver` | merged |
| fnpm-config: `.npmrc`, bunfig/fnpm.toml, env, CLI precedence, proxies | `feat/config` | merged |
| fnpm-registry: HTTP/2 client, manifest cache, auth, throttling | `feat/registry` | merged |
| fnpm-store: package store v2, streaming extraction, file index, fs ops, prune | `feat/store` | merged |
| fnpm-workspace: discovery, `--filter`, topo order, package.json editor | `feat/workspace` | merged |
| fnpm-resolver: dependency resolver ported from bun | `feat/resolver` | merged |
| fnpm-lockfile: fnpm.lock / bun.lock reader and writer, hoisting, sidecar | `feat/lockfile` | merged |
| fnpm-lockfile: npm and bun (text and binary) migration, npm/bun export | `feat/migrate-npm-bun` | merged |
| fnpm-lockfile: yarn v1, yarn berry, pnpm migration, yarn/pnpm export | `feat/migrate-yarn-pnpm` | merged |
| fnpm-linker: hoisted and isolated layouts, global virtual store, bins | `feat/linker` | merged |
| fnpm-scripts: lifecycle runner, trust policy, run machinery | `feat/scripts` | merged |
| Merge the 11 branches into `main`, resolve contract drift, build/test/clippy/fmt green | `feat/core` | done |
| fnpm-core: install pipeline (fast path, resolve/fetch/link overlap, save), summary, error codes | `feat/core` | done (command groups integrated; conformance gaps remain) |
| fnpm-napi: `install` / `invoke` wired to the core, events through the TSFN | `feat/napi` | done; all eight release addon builds and native smoke tests pass in the recorded main CI run |
| fnpm-cli: native `fnpm` / `fnpm-lock`, `cli-spec.json` generation and build.rs, native progress and summary, exec to Node | `feat/cli` | done (JS shim and postinstall swap included; native release archives ship the executables in P6) |
| Merge `feat/napi` and `feat/cli` into `feat/core`, then `main`; native end-to-end smoke test against the mirror | `feat/core` | done |
| Bench harness runs the native binary; first fnpm numbers committed | `feat/cli` | done (`bench/results/2026-10-25-linux-x64.md`: small, all scenarios; medium warm/noop/ci/cold-lockfile) |

Known gaps in the merged crates (PARITY rows left open for these):

- semver: `Range::has_prerelease` is rebuilt from comparators (`^1.2-beta` edge case);
  `Range::parse("^1.2.3")` ~150 ns vs the 100 ns target.
- config: no mTLS; `publishConfig` registry-source edge case approximated.
- registry: `_DNS_TIME_TO_LIVE_SECONDS` ignored; one TLS handshake per origin not enforced;
  manifest cache load ~0.75 ms (owned `PackageManifest` strings); local version parsing in
  `semver_ext.rs`.
- store: bun's cache-folder name printers and Windows junctions remain; macOS compilation, Windows status accounting and strict Clippy pass in the recorded main CI run.
- resolver: "reuse cached versions" only covers locked/pinned versions; production sources
  (`RegistrySource`, git, store import) first exercised by fnpm-core.
- lockfile: two bun.lock readers (`text`, used by `load`/`parse`/`serialize`, and
  `formats::bun_text`, used by migration reports) should become one; migrated Yarn/pnpm
  bins and lifecycle metadata are backfilled in core; `out_of_sync` ignores `packageExtensions`.
- linker: isolated step order not interleaved with scripts; Windows `.exe` shims.
- scripts: `--no-orphans` is only the Linux parent-death signal; local package.json/semver
  helpers instead of fnpm-workspace/fnpm-semver.

Exit criteria:

- Conformance: `bun-install.test.ts`, `bun-add.test.ts`, `bun-remove.test.ts`,
  `bun-update.test.ts`, `bun-install-registry.test.ts`, `hoist.test.ts`, `bun-lock.test.ts`,
  `frozen-lockfile-*.test.ts`, `lockfile-only.test.ts`, `npmrc.test.ts`,
  `config-precedence.test.ts`, `bun-install-retry.test.ts`, `bun-install-offline*.test.ts`,
  `bun-install-tarball-integrity.test.ts`, `bun-install-streaming-extract.test.ts`,
  `bun-install-backends.test.ts`, `bun-install-hardlink-fallback.test.ts`,
  `bun-install-cpu-os.test.ts` pass, minus tests that need P2 features (workspaces, isolated,
  scripts), which are tracked as skips with the phase that unblocks them.
- Benchmark: `small` and `medium`, all six scenarios; `large` cold-lockfile, warm, noop.
- Platforms: Linux x64 (ext4, hardlink) and macOS arm64 (APFS, clonefile).

## P2: Workspaces and policy (in progress)

| deliverable | status |
|---|---|
| Isolated layout end to end (default for workspaces), optional global virtual store | implemented; global sharing is opt-in to preserve hidden-hoist visibility |
| Workspaces, `workspace:` protocol, `--filter`, `-r`, catalogs | implemented with conformance gaps in aliases, catalogs and isolated relinking |
| Overrides and nested overrides, `packageExtensions`, patches applied at install | implemented with open conformance cases |
| Lifecycle scripts in dependency order, trusted dependencies, postinstall optimizer, native binlink | implemented; failed optional packages are removed before dependent scripts |
| git, GitHub, remote/local tarball, `file:` and `link:` dependencies | implemented and covered by crate and corpus tests |
| `minimumReleaseAge`, security scanner, `--cpu` / `--os` filters | implemented; scanner retry/scope and policy conformance remain partial |

Exit criteria:

- Conformance: `isolated-install`, `isolated-relink`, `bun-workspaces*`, `bad-workspace`,
  `catalogs`, `bun-add-catalog`, `bun-add-filter`, `overrides`, `nested-overrides`,
  `bun-install-package-extensions`, `bun-install-patch`, `bun-install-lifecycle-scripts`,
  `bun-install-native-binlink`, `bun-install-git-deps`, `public-hoist-pattern`,
  `minimum-release-age`, `bun-install-security-provider`, `bun-security-scanner-*`,
  `test-dev-peer-dependency-priority`, `symlink-path-traversal`, plus every P1 skip that waited
  on these features.
- Benchmark: the full matrix including `monorepo` and every `large` scenario.
- Platforms: add Linux arm64 and Linux btrfs (FICLONE).

## P3: Migrations and fnpm-lock (in progress)

| deliverable | status |
|---|---|
| First-install migration from package-lock / shrinkwrap, yarn.lock (v1, berry), pnpm-lock.yaml, bun.lock, bun.lockb, carrying pnpm workspace config | implemented in core; real-project corpus exercises npm v3, Yarn v1/Berry v8, pnpm v9 and bun text; shrinkwrap/binary corpus coverage remains |
| `fnpm migrate`, `fnpm-lock migrate / export / verify / diff / why / dedupe / audit / print` | wired to core commands; full foreign-lock conformance remains open |
| Golden tests: real lockfiles from each tool for every fixture; migrate, install, compare resolved versions with the source tool's `ls --json` | in progress: 18-project corpus compares native-PM snapshots, package versions, declared deps, bins, observed scripts, verification and reinstalls; results and remaining failures in `tests/e2e/results` |

Exit criteria:

- Conformance: `migration/*`, `lockfile-version-2`, `config-version`, and the remaining
  `bun-lock` tests.
- Benchmark: `cold-lockfile` starting from each foreign lockfile is at most bun's time for the
  same lockfile.
- Platforms: as P2.

## P4: Remaining commands (in progress)

`outdated`, `why`, `ls`, `info`, `dedupe`, `prune`, `link` / `unlink`, `patch` / `patch-commit`,
`audit` (+ `fix`), `pack`, `publish`, `run`, `exec`, `x` / `dlx` / `fnpx` / `fnpmx`, `create`, `init`,
`store`, the `pm` subcommands (`cache`, `pkg`, `trust`, `version`, ...), `doctor`, `whoami`,
`completions`. Execution through `x` / `create` has a native launcher path; the other
commands use the TS CLI and `fnpm_core::invoke`.

Publishing commands from `feat/cmds-publish` are integrated: `pack` / `pm pack`, `publish`,
`patch`, `patch --commit` and `patch-commit`, including workspace/catalog rewriting and patch
application during install. They have not met the phase exit criteria. Historical Linux debug
conformance results recorded by that track:

| suite | passed | failed | skipped |
|---|---:|---:|---:|
| `bun-pack` | 349 | 32 | 1 |
| `bun-publish` | 262 | 17 | 1 |
| `bun-patch` | 80 | 152 | 5 |
| `bun-install-patch` | 0 | 53 | 0 |

The historical track's 17 publish failures stopped at a subsequent install assertion for `Saved lockfile`.
The integrated release run passes all 279 measured Linux publish cases (the Windows drive-root case is skipped);
full phase parity remains open. In that track run, all install-patch cases also failed at output or diagnostic
assertions, leaving later patch behavior in those cases unverified. Pack has workspace-install
and `pm diff` blockers, and the Node subprocess adapter lacks `resourceUsage()` for the memory
test. Patch has further command gaps in target selection and scanner checks. Patch-file,
manifest and lockfile writes are restored on ordinary link/save failures; this is not a crash
journal and does not roll back node_modules. Lifecycle failures after a successful save retain
the saved metadata, as the install pipeline does. Resolution-refusal preservation, local-tarball patch reinstallation,
long tarball labels, local isolated links, copy-backend bin permissions, external-checkout
isolation and absolute patch directories have passing Rust regressions. Scoped and unscoped
global-store copies retain sibling dependency links without modifying the shared store or
including those links in the patch; bundled entries win and nested copying does not write
through their symlinks. Paths outside the project's node_modules are refused. The review's
final workspace Rust run passed 1,308 tests with 13 ignored across 74 targets (two test threads
per target); workspace clippy with warnings denied, formatting, debug CLI/NAPI builds, CLI
build/type checking, adapter type checking, and 641 TypeScript CLI tests passed. The registry
harness passed 49 tests, with one expected failure, two skips and one todo. The repaired
body-limit fixture also passed five isolated repeat runs. A separate registry priority test
failed once under heavier contention and passed in the final workspace run; its timing-based
queue setup remains a potential source of flakes. These checks do not establish Bun parity,
a speed advantage, or disk savings. A separate Linux
`wait4` measurement of the debug CLI reported peak RSS of 69,586,944 bytes for a 1 MiB
input and 69,242,880 bytes for a 32 MiB input (`pm pack --quiet --gzip-level=0`), a delta
of -344,064 bytes within the conformance assertion's 8 MiB allowance. The ported memory
test itself remains blocked by the adapter API.

The follow-up adversarial review fixed diff preparation that moved entries through symlinked
node_modules/scope directories, separated linker-generated bins from bundled `.bin` entries,
and moved bin mode/shebang normalization to a disposable copy instead of altering the shared
store. Regression coverage also checks restoration after a failed Yarn lockfile export and a
subsequent successful commit followed by a clean reinstall. Patch commits force the save path,
including an explicitly requested Yarn export, when only an existing patch's contents changed.
The registry body-limit fixture now consumes the full request and declares `Connection: close`
before closing its one-response socket; previously a pooled second request could be reset.

A fresh Linux `wait4` remeasurement (three runs per case, fnpm debug CLI and Bun 1.4.4) found
median peak RSS of 67,977,216 bytes at 1 MiB and 68,050,944 bytes at 32 MiB: +73,728 bytes.
Bun's medians were 22,495,232 and 22,491,136 bytes. Median elapsed times were 139.61/484.22 ms
for fnpm and 81.22/315.24 ms for Bun. These shared-devbox debug measurements are a streaming
memory check, not a release benchmark or evidence of a speed advantage. No install/disk matrix
or platform matrix was rerun by this review.

Exit criteria:

- Conformance: `bun-pm*`, `bun-info`, `bun-dedupe`, `bun-prune`, `bun-link`, `bun-patch`,
  `bun-audit`, `bun-pack`, `bun-publish`, `bunx*`, `bun-create`, `bun-update-*`
  (security and transitive), and the 5 todo files from INDEX.md ported and passing
  (`bun-run`, `bun-run-bunfig`, `hosted-git-info/*` as fnpm-resolver unit tests).
- Benchmark: no regression on the full matrix.

## P5: UX parity (in progress)

| deliverable | status |
|---|---|
| Help text, usage errors and hints matching bun for every command | in progress (unknown long flags and help hints share native/TS golden fixtures; other diagnostics still differ) |
| Progress and summary output identical between the native and TS renderers (shared golden fixtures) | in progress (shared fixtures pass; plain output and rendered color styles match the small cold/no-op/add/remove fixture; cached availability hints and warning/status ordering still differ) |
| `update -i`, structured command results, redacted config logs, control-character escaping | in progress (`update -i` and supported structured results are implemented; full output parity remains) |
| Every PARITY.md "Output/UX" row | in progress; unverified rows remain open |

Exit criteria: `escape-control-characters`, `redacted-config-logs`, every rewritten snapshot
reviewed, and all PARITY.md rows outside "Excluded" ticked.

## P6: Distribution (in progress)

| deliverable | status |
|---|---|
| CI: Rust and TS tests, conformance, clippy on macOS, Linux (ext4, btrfs) and Windows | partial; selected store/binding/patch tests, strict Clippy and binding/adapter JS tests pass on native Linux/macOS/Windows; full suites and btrfs remain |
| Build matrix for 8 targets: native `fnpm` / `fnpm-lock` and `fnpm.node` | done: [00ec69f run](https://github.com/SnowingFox/fuck-npm/actions/runs/40265278514) passed all eight complete builds and eight installed npm tarball smokes |
| Single npm CLI package with native archive postinstall and first-use shim fallback | implemented for `fnpm@0.0.2`: one package, eight native archives, embedded SHA-512, no npm dependencies; staged normal/disabled-script installs pass on all eight targets; public npm publication pending |
| Windows: hardlink backend, `.cmd` shims, the `.bin` placeholder question (see ARCHITECTURE "Packaging") | native executables and addon build on x64/ARM64; installed JS shim launching native binaries passes both npm smoke jobs; selected store/patch/binding tests pass; full bin conformance and other package-manager installs remain |
| Install tests of the published packages with npm, pnpm, yarn and bun, with and without scripts | staged npm tarballs pass normal and `--ignore-scripts` smoke on all eight targets; local macOS ARM64 native preview verified; published-package and other-manager checks remain |
| Publish dry run, then the first release (manual authentication on release machine) | scoped `0.0.1` published through bnpm; public `fnpm@0.0.2` candidate uses one package and eight GitHub assets; final npm authentication/publication pending; see [RELEASING.md](RELEASING.md) |

Exit criteria:

- Conformance: every ported test passes or is listed as an intentional difference.
- Benchmark: the full matrix beats bun on Linux x64 and macOS arm64; disk below bun and pnpm.
- Platforms: all 8 targets build and pass the smoke install; full suites green on macOS, Linux
  and Windows.
