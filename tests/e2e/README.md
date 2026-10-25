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

`packages` and `bins` are linker independent and are the primary comparison. `importers` reflects
the PM's linker (hoisted vs isolated) and is compared only where fnpm uses the same layout.
