# e2e corpus

Real open-source projects installed with their own package manager and lockfile, then verified
with a fast build/test command. The recorded baselines are what fnpm installs get compared against.

```sh
node bench/mirror.ts start                         # local verdaccio mirror (optional, much faster)
FNPM_E2E_DIR=/somewhere/outside/repo node tests/e2e/run.ts [--only a,b] [--skip-verify] [--verify-only] [--no-mirror]
```

- `corpus.json`: project list (repo, pinned commit, PM, lockfile format, features, install/verify commands).
- `baselines/<name>.json`: one per project. Written by `run.ts`.
- `script-logger.cjs`: preloaded via `NODE_OPTIONS=--require` during install; logs every node-based
  dependency lifecycle script (`scriptsRan`). Shell-only scripts are not seen; `installScripts`
  is the static list of packages in the tree that declare install scripts (or a `binding.gyp`).

Checkouts go to `$FNPM_E2E_DIR` (default `~/.cache/fnpm-e2e`), never into the repo. Logs land in
`$FNPM_E2E_DIR/_logs/<name>.{install,verify,scripts}`.

## Baseline fields

| field | meaning |
|---|---|
| `install` | command, exit code, wall time (ms) with the PM's own lockfile |
| `scriptsRan` | `name@version event` of lifecycle scripts observed during install |
| `installScripts` | packages in the tree declaring `preinstall`/`install`/`postinstall` |
| `packages` | unique `name@version` across every installed package dir (incl. `.pnpm`/`.bun` stores) |
| `instances` | physical package dirs (after realpath dedupe), i.e. duplication level |
| `importers` | per workspace dir: direct `node_modules` entries → version, `alias@version`, or `link:<path>` |
| `bins` | per workspace dir: `node_modules/.bin` entries |
| `verify` | verify command result |

`packages` compares unique installed versions across layouts. Bin placement and importer entries
reflect the PM's linker (hoisted vs isolated); bins and top-level placement are compared only
where fnpm uses the same layout.

## fnpm runs

```sh
cargo build --release -p fnpm-cli                  # the runner uses target/release/fnpm (or $FNPM_BIN)
FNPM_E2E_DIR=/somewhere/outside/repo node tests/e2e/fnpm.ts [--only a,b] [--skip a,b] [--skip-verify] [--no-bun] [--out results-stem]
```

The runner copies the native executable into a private temporary directory for the run and records
its SHA-256 as `binarySha256` in the JSON result. Rebuilding the original executable cannot change
the binary used by later projects or lifecycle children in that run.

Per project, one at a time:

1. bun (the parity target) installs the pristine checkout with `--ignore-scripts`, for reference;
2. pristine checkout again, empty `FNPM_STORE_DIR` / `FNPM_CACHE_DIR`, cold `fnpm install` from the
   project's own lockfile (fnpm migrates it and writes `fnpm.lock`);
3. the snapshot is compared with the baseline: `packages` (missing and extra name@version),
   the version every importer resolves for each dependency it declares (node resolution through
   the importer's and its ancestors' `node_modules`), the top-level `node_modules` and `.bin`
   when the layouts match (hoisted vs isolated), and workspace lifecycle scripts;
4. the verify command (`fnpm.verify` when the project's PM cannot run on a foreign tree);
5. a no-op re-run, a warm re-install (`node_modules` removed) and `--frozen-lockfile`, each of
   which must reproduce the same snapshot and leave `fnpm.lock` unchanged.

A difference from the baseline that bun shows too is reported as "same as bun" (parity) instead
of a failure; fnpm's package set is also diffed against bun's. The JSON `vsBun.ok` is true only
when those package sets match; a baseline-compatible PASS can still list additional versions
relative to Bun. Dependency lifecycle scripts are only listed: fnpm, like bun, runs them for
trusted packages only. `fnpm.allow` (regexes) accepts
other documented differences. `fnpm.known` describes a genuine incompatibility; status KNOWN
requires every failure to match a `fnpm.knownFailures` regex. An unexpected failure remains FAIL.
The runner exits 1 if any project is FAIL. Results redact local checkout paths.
Results: `tests/e2e/results/<date>.md` (and `.json`); logs in `$FNPM_E2E_DIR/_logs/<name>.fnpm.*`.

## Integration result (2026-10-25)

[13 PASS, 5 KNOWN, 0 unexpected failures](results/2026-10-26-land.md),
from `23cc221` with native SHA-256 `cd278a4690db221dca51b63a76b9ce929a651bdec9b49b10905b7191f1436aac`.
All 18 projects were rerun with install, verification, no-op, warm reinstall and frozen checks where install succeeds.
The result filename identifies the integration checkpoint; the JSON date preserves the runner host UTC date.
Known failures remain: webpack peer hoisting, Babel Yarn catalog migration, Vue's pnpm-only preinstall guard,
date-fns @jsr registry routing, and Vitest duplicate workspace names. Matching a documented failure
keeps the runner successful but does not mean that project is compatible.
PASS accepts the documented baseline differences. Passing projects can still differ from Bun in
optional native packages, additional locked peer versions and foreign-lockfile fallback; per-project
`vsBun`, hoisting and bin comparison notes remain in the result artifact.
