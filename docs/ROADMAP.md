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
| fnpm-store: CAS, streaming extraction, file index, fs ops, prune | `feat/store` | merged |
| fnpm-workspace: discovery, `--filter`, topo order, package.json editor | `feat/workspace` | merged |
| fnpm-resolver: dependency resolver ported from bun | `feat/resolver` | merged |
| fnpm-lockfile: fnpm.lock / bun.lock reader and writer, hoisting, sidecar | `feat/lockfile` | merged |
| fnpm-lockfile: npm and bun (text and binary) migration, npm/bun export | `feat/migrate-npm-bun` | merged |
| fnpm-lockfile: yarn v1, yarn berry, pnpm migration, yarn/pnpm export | `feat/migrate-yarn-pnpm` | merged |
| fnpm-linker: hoisted and isolated layouts, global virtual store, bins | `feat/linker` | merged |
| fnpm-scripts: lifecycle runner, trust policy, run machinery | `feat/scripts` | merged |
| Merge the 11 branches into `main`, resolve contract drift, build/test/clippy/fmt green | `feat/core` | done |
| fnpm-core: install pipeline (fast path, resolve/fetch/link overlap, save), summary, error codes | `feat/core` | done (some P4 commands still return `NOT_IMPLEMENTED`) |
| fnpm-napi: `install` / `invoke` wired to the core, events through the TSFN | `feat/napi` | done (the 8-target `napi.yml` matrix has not run on GitHub yet) |
| fnpm-cli: native `fnpm` / `fnpm-lock`, `cli-spec.json` generation and build.rs, native progress and summary, exec to Node | `feat/cli` | done (JS shim and postinstall swap included; the platform packages ship the executables in P6) |
| Merge `feat/napi` and `feat/cli` into `feat/core`, then `main`; native end-to-end smoke test against the mirror | `feat/core` | done |
| Bench harness runs the native binary; first fnpm numbers committed | `feat/cli` | done (`bench/results/2026-10-25-linux-x64.md`: small, all scenarios; medium warm/noop/ci/cold-lockfile) |

Known gaps in the merged crates (PARITY rows left open for these):

- semver: `Range::has_prerelease` is rebuilt from comparators (`^1.2-beta` edge case);
  `Range::parse("^1.2.3")` ~150 ns vs the 100 ns target.
- config: no mTLS; `publishConfig` registry-source edge case approximated.
- registry: `_DNS_TIME_TO_LIVE_SECONDS` ignored; one TLS handshake per origin not enforced;
  manifest cache load ~0.75 ms (owned `PackageManifest` strings); local version parsing in
  `semver_ext.rs`.
- store: bun's cache-folder name printers; Windows junctions; macOS/Windows paths never built.
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

## P2: Workspaces and policy (not started)

| deliverable | status |
|---|---|
| Isolated layout and global virtual store end to end (default for workspaces) | not started (linker side merged in P1) |
| Workspaces, `workspace:` protocol, `--filter`, `-r`, catalogs | not started |
| Overrides and nested overrides, `packageExtensions`, patches applied at install | not started |
| Lifecycle scripts in dependency order, trusted dependencies, postinstall optimizer, native binlink | not started |
| git, GitHub, remote/local tarball, `file:` and `link:` dependencies | not started |
| `minimumReleaseAge`, security scanner, `--cpu` / `--os` filters | not started |

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
| `fnpm migrate`, `fnpm-lock migrate / export / verify / diff / why / dedupe / audit / print` | not started |
| Golden tests: real lockfiles from each tool for every fixture; migrate, install, compare resolved versions with the source tool's `ls --json` | in progress: 18-project corpus compares native-PM snapshots, package versions, declared deps, bins, observed scripts, verification and reinstalls; results and remaining failures in `tests/e2e/results` |

Exit criteria:

- Conformance: `migration/*`, `lockfile-version-2`, `config-version`, and the remaining
  `bun-lock` tests.
- Benchmark: `cold-lockfile` starting from each foreign lockfile is at most bun's time for the
  same lockfile.
- Platforms: as P2.

## P4: Remaining commands (in progress)

`outdated`, `why`, `ls`, `info`, `dedupe`, `prune`, `link` / `unlink`, `patch` / `patch-commit`,
`audit` (+ `fix`), `pack`, `publish`, `run`, `exec`, `x` / `dlx` / `fnpmx`, `create`, `init`,
`store`, the `pm` subcommands (`cache`, `pkg`, `trust`, `version`, ...), `doctor`, `whoami`,
`completions`. All run through the TS CLI
and `fnpm_core::invoke`.

Publishing commands are implemented on `feat/cmds-publish`: `pack` / `pm pack`, `publish`,
`patch`, `patch --commit` and `patch-commit`, including workspace/catalog rewriting and patch
application during install. They have not met the phase exit criteria. Latest Linux debug
conformance results for this track:

| suite | passed | failed | skipped |
|---|---:|---:|---:|
| `bun-pack` | 349 | 32 | 1 |
| `bun-publish` | 262 | 17 | 1 |
| `bun-patch` | 80 | 152 | 5 |
| `bun-install-patch` | 0 | 53 | 0 |

The 17 publish failures stop at a subsequent install assertion for `Saved lockfile`; these
whole tests remain failing. All install-patch cases also fail at output or diagnostic
assertions, so later patch behavior in those cases remains unverified. Pack has workspace-install
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
| `update -i`, `--json` payloads for every command, redacted config logs, control-character escaping | not started |
| Every PARITY.md "Output/UX" row | not started |

Exit criteria: `escape-control-characters`, `redacted-config-logs`, every rewritten snapshot
reviewed, and all PARITY.md rows outside "Excluded" ticked.

## P6: Distribution (not started)

| deliverable | status |
|---|---|
| CI: Rust and TS tests, conformance, clippy on macOS, Linux (ext4, btrfs) and Windows | not started |
| Build matrix for 8 targets: native `fnpm` / `fnpm-lock` and `fnpm.node` | not started |
| npm packages: `fuck-npm`, `@fuck-npm/binding`, `@fuck-npm/binding-<triple>`; postinstall swap and JS shim fallback | not started |
| Windows: hardlink backend, `.cmd` shims, the `.bin` placeholder question (see ARCHITECTURE "Packaging") | not started |
| Install tests of the published packages with npm, pnpm, yarn and bun, with and without scripts | not started |
| Publish dry run, then the first release (manual, behind `NPM_TOKEN`) | not started |

Exit criteria:

- Conformance: every ported test passes or is listed as an intentional difference.
- Benchmark: the full matrix beats bun on Linux x64 and macOS arm64; disk below bun and pnpm.
- Platforms: all 8 targets build and pass the smoke install; full suites green on macOS, Linux
  and Windows.
