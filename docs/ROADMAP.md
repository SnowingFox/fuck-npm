# Roadmap

fnpm is done when every non-excluded row of [PARITY.md](PARITY.md) is ticked, every ported bun
install test passes (or its difference is listed as intentional in PARITY.md), and fnpm beats bun
on the whole benchmark matrix while using less disk than bun and pnpm. The phases below get there
in an order where each layer is testable on its own.

Every phase runs the same loop: one implementer per crate or command group on its own branch,
an adversarial review (correctness, cross-platform behaviour, performance), then merge, the
conformance files for the phase, and the benchmark gate.

Status as of 2026-10-25. Legend: **done**, **reviewed** (implemented and reviewed on its branch,
not merged), **in progress**, **not started**.

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
| fnpm-semver: npm versions and ranges, bun's satisfies/pick-best | `feat/semver` | reviewed |
| fnpm-config: `.npmrc`, bunfig/fnpm.toml, env, CLI precedence, proxies | `feat/config` | reviewed |
| fnpm-registry: HTTP/2 client, manifest cache, auth, throttling | `feat/registry` | reviewed |
| fnpm-store: CAS, streaming extraction, file index, fs ops, prune | `feat/store` | reviewed |
| fnpm-workspace: discovery, `--filter`, topo order, package.json editor | `feat/workspace` | reviewed |
| fnpm-resolver: dependency resolver ported from bun | `feat/resolver` | reviewed |
| fnpm-lockfile: fnpm.lock / bun.lock reader and writer, hoisting, sidecar | `feat/lockfile` | reviewed |
| fnpm-lockfile: npm and bun (text and binary) migration, npm/bun export | `feat/migrate-npm-bun` | reviewed |
| fnpm-lockfile: yarn v1, yarn berry, pnpm migration, yarn/pnpm export | `feat/migrate-yarn-pnpm` | reviewed |
| fnpm-linker: hoisted and isolated layouts, global virtual store, bins | `feat/linker` | reviewed |
| fnpm-scripts: lifecycle runner, trust policy, run machinery | `feat/scripts` | reviewed |
| Merge the 11 branches into `main`, resolve contract drift, `cargo test --workspace` green | `integrate` | in progress (branch created, no commits yet) |
| fnpm-core: install pipeline (fast path, resolve/fetch/link overlap, save), summary, error codes | `feat/core` | in progress (no commits yet; skeleton on `main`) |
| fnpm-napi: `install` / `invoke` wired to the core, events through the TSFN | `feat/napi` | in progress (no commits yet; skeleton on `main`) |
| fnpm-cli: native `fnpm` / `fnpm-lock`, `cli-spec.json` generation and build.rs, native progress and summary, exec to Node | `feat/cli` | in progress (no commits yet; crate not created) |
| Bench harness runs the native binary; first fnpm numbers committed | | not started |

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
| Isolated layout and global virtual store end to end (default for workspaces) | not started (linker side reviewed in P1) |
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

## P3: Migrations and fnpm-lock (not started)

| deliverable | status |
|---|---|
| First-install migration from package-lock / shrinkwrap, yarn.lock (v1, berry), pnpm-lock.yaml, bun.lock, bun.lockb, carrying pnpm workspace config | not started (format code reviewed in P1) |
| `fnpm migrate`, `fnpm-lock migrate / export / verify / diff / why / dedupe / audit / print` | not started |
| Golden tests: real lockfiles from each tool for every fixture; migrate, install, compare resolved versions with the source tool's `ls --json` | not started |

Exit criteria:

- Conformance: `migration/*`, `lockfile-version-2`, `config-version`, and the remaining
  `bun-lock` tests.
- Benchmark: `cold-lockfile` starting from each foreign lockfile is at most bun's time for the
  same lockfile.
- Platforms: as P2.

## P4: Remaining commands (not started)

`outdated`, `why`, `ls`, `info`, `dedupe`, `prune`, `link` / `unlink`, `patch` / `patch-commit`,
`audit` (+ `fix`), `pack`, `publish`, `run`, `exec`, `x` / `dlx` / `fnpmx`, `create`, `init`,
`store`, the `pm` subcommands (`cache`, `pkg`, `trust`, `version`, ...), `doctor`, `whoami`,
`completions`. All run through the TS CLI
and `fnpm_core::invoke`.

Exit criteria:

- Conformance: `bun-pm*`, `bun-info`, `bun-dedupe`, `bun-prune`, `bun-link`, `bun-patch`,
  `bun-audit`, `bun-pack`, `bun-publish`, `bunx*`, `bun-create`, `bun-update-*`
  (security and transitive), and the 5 todo files from INDEX.md ported and passing
  (`bun-run`, `bun-run-bunfig`, `hosted-git-info/*` as fnpm-resolver unit tests).
- Benchmark: no regression on the full matrix.

## P5: UX parity (not started)

| deliverable | status |
|---|---|
| Help text, usage errors and hints matching bun for every command | not started |
| Progress and summary output identical between the native and TS renderers (shared golden fixtures) | not started |
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
